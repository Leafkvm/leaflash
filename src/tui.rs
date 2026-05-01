use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui_explorer::FileExplorer;

use crate::device;
use crate::flash::{self, Config, ProgressHandle, Report};

const SECTOR_SIZE: u64 = flash::SECTOR_SIZE;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Picker,
    Size,
    FlashButton,
}

struct App {
    explorer: FileExplorer,
    selected_image: Option<PathBuf>,
    size_input: String,
    focus: Focus,
    log: Arc<Mutex<Vec<String>>>,
    progress: Arc<Mutex<Option<(u64, u64, String)>>>,
    flashing: Arc<Mutex<bool>>,
    devices: Vec<device::DeviceSummary>,
    device_err: Option<String>,
    sd_total_sectors: Option<u64>,
    sd_total_bytes: Option<u64>,
    sd_existing: Option<flash::Layout>,
    flash_result: Arc<Mutex<Option<Result<(), String>>>>,
    success_banner: bool,
    reset_after_flash: bool,
    userdata_magic: bool,
    /// When Some, render the centered confirm dialog.
    pending_confirm: Option<PendingConfirm>,
    /// When Some, render a red error overlay. Any key dismisses it.
    error_msg: Option<String>,
}

/// What's queued behind the confirm dialog: the validated Config, plus
/// whether the existing on-disk GPT already matches — used to drop the
/// SD-erase warning when nothing destructive is going to happen to
/// rootfs_b / userdata.
struct PendingConfirm {
    cfg: Config,
    layout_matches: bool,
}

pub fn run() -> Result<()> {
    let explorer = FileExplorer::new()?;
    let (devices, device_err) = match device::list() {
        Ok(d) => (d, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    let (sd_total_bytes, sd_total_sectors, sd_existing, probe_err) =
        match device::probe_sd_full() {
            Ok(p) => (Some(p.total_bytes), Some(p.total_sectors), p.existing, None),
            Err(e) => (None, None, None, Some(e.to_string())),
        };

    let mut log: Vec<String> = Vec::new();
    if let Some(s) = sd_total_bytes {
        log.push(format!("SD capacity: {} MiB", s / (1024 * 1024)));
        if sd_existing.is_some() {
            log.push(
                "Existing leaflash partition table detected on SD.".to_string(),
            );
        }
    } else if let Some(e) = probe_err {
        log.push(format!("Could not probe SD capacity: {e}"));
    }

    let mut app = App {
        explorer,
        selected_image: None,
        size_input: "256MiB".to_string(),
        focus: Focus::Picker,
        log: Arc::new(Mutex::new(log)),
        progress: Arc::new(Mutex::new(None)),
        flashing: Arc::new(Mutex::new(false)),
        devices,
        device_err,
        sd_total_sectors,
        sd_total_bytes,
        sd_existing,
        flash_result: Arc::new(Mutex::new(None)),
        success_banner: false,
        reset_after_flash: true,
        userdata_magic: false,
        pending_confirm: None,
        error_msg: None,
    };

    let mut terminal = setup_terminal()?;
    let res = event_loop(&mut terminal, &mut app);
    restore_terminal(&mut terminal)?;
    res
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// What the event handler decided. Errors from operations that the user can
/// recover from (parse failures, picker errors, etc.) are turned into
/// `error_msg` overlays instead of crashing the TUI.
enum Tick {
    Continue,
    Quit,
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;

        if event::poll(std::time::Duration::from_millis(100))? {
            let evt = event::read()?;
            match handle_event(app, &evt) {
                Ok(Tick::Quit) => return Ok(()),
                Ok(Tick::Continue) => {}
                Err(e) => app.error_msg = Some(e.to_string()),
            }
        }

        // surface flash thread completion
        let mut result_slot = app.flash_result.lock().unwrap();
        if let Some(res) = result_slot.take() {
            match res {
                Ok(()) => {
                    app.log.lock().unwrap().push("Flash completed.".to_string());
                    app.success_banner = true;
                }
                Err(e) => {
                    app.log.lock().unwrap().push(format!("Flash failed: {e}"));
                    app.error_msg = Some(format!("Flash failed:\n{e}"));
                }
            }
            *app.flashing.lock().unwrap() = false;
            *app.progress.lock().unwrap() = None;
        }
    }
}

fn handle_event(app: &mut App, evt: &Event) -> Result<Tick> {
    let Event::Key(key) = evt else { return Ok(Tick::Continue) };
    if key.kind != KeyEventKind::Press {
        return Ok(Tick::Continue);
    }

    // Ctrl-C always exits, even mid-flash.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c'))
    {
        return Ok(Tick::Quit);
    }

    // Modal overlays consume input first.
    if app.error_msg.is_some() {
        app.error_msg = None;
        return Ok(Tick::Continue);
    }
    if app.success_banner {
        app.success_banner = false;
        return Ok(Tick::Continue);
    }
    if app.pending_confirm.is_some() {
        return handle_confirm_key(app, key);
    }

    // Don't quit mid-flash via plain q/Esc — would corrupt the SD card.
    if *app.flashing.lock().unwrap() {
        return Ok(Tick::Continue);
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Ok(Tick::Quit),
        KeyCode::Tab => {
            app.focus = match app.focus {
                Focus::Picker => Focus::Size,
                Focus::Size => Focus::FlashButton,
                Focus::FlashButton => Focus::Picker,
            };
            return Ok(Tick::Continue);
        }
        KeyCode::Char('r') if app.focus != Focus::Size => {
            app.reset_after_flash = !app.reset_after_flash;
            return Ok(Tick::Continue);
        }
        KeyCode::Char('m') if app.focus != Focus::Size => {
            app.userdata_magic = !app.userdata_magic;
            return Ok(Tick::Continue);
        }
        _ => {}
    }

    match app.focus {
        Focus::Picker => {
            if matches!(key.code, KeyCode::Enter) {
                let f = app.explorer.current();
                if f.is_file() {
                    on_image_selected(app, f.path().clone());
                    return Ok(Tick::Continue);
                }
            }
            app.explorer.handle(evt)?;
        }
        Focus::Size => match key.code {
            KeyCode::Char(c) => app.size_input.push(c),
            KeyCode::Backspace => { app.size_input.pop(); }
            KeyCode::Enter => app.focus = Focus::FlashButton,
            _ => {}
        },
        Focus::FlashButton => {
            if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
                open_confirmation(app)?;
            }
        }
    }
    Ok(Tick::Continue)
}

fn handle_confirm_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Result<Tick> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            let pc = app.pending_confirm.take().expect("checked above");
            kickoff_flash(app, pc.cfg);
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
            app.pending_confirm = None;
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            // Toggle reset preference and update the held config so the
            // dialog re-renders with the new value.
            app.reset_after_flash = !app.reset_after_flash;
            if let Some(pc) = app.pending_confirm.as_mut() {
                pc.cfg.reset_after_flash = app.reset_after_flash;
            }
        }
        KeyCode::Char('m') | KeyCode::Char('M') => {
            app.userdata_magic = !app.userdata_magic;
            if let Some(pc) = app.pending_confirm.as_mut() {
                pc.cfg.userdata_magic = app.userdata_magic;
            }
        }
        _ => {}
    }
    Ok(Tick::Continue)
}

fn on_image_selected(app: &mut App, path: PathBuf) {
    app.selected_image = Some(path.clone());
    app.focus = Focus::Size;

    let Ok(meta) = std::fs::metadata(&path) else {
        return;
    };
    let img_len = meta.len();
    let mut rounded = flash::round_up_mib(img_len, flash::DEFAULT_ROUND_MIB);

    if let Some(total) = app.sd_total_bytes {
        let max = flash::max_rootfs_bytes(total);
        if img_len > max {
            app.error_msg = Some(format!(
                "Image is {} MiB but SD only fits a {} MiB rootfs (half of SD - GPT).\n\
                 Pick a smaller image or use a larger card.",
                img_len / (1024 * 1024),
                max / (1024 * 1024),
            ));
            return;
        }
        if rounded > max {
            let mib = 1024 * 1024;
            let max_mib_floor = max / mib * mib;
            let granular = max / (flash::DEFAULT_ROUND_MIB * mib)
                * (flash::DEFAULT_ROUND_MIB * mib);
            rounded = if granular >= img_len { granular } else { max_mib_floor };
            app.log.lock().unwrap().push(format!(
                "Default 128-MiB round-up wouldn't fit; capped rootfs at {} MiB.",
                rounded / mib,
            ));
        }
    }

    let mib = rounded / (1024 * 1024);
    app.size_input = format!("{mib}MiB");
}

/// Validate inputs and stage a Config in `pending_confirm`. Errors here go
/// into the red overlay, not the log, since they block the user's intent.
fn open_confirmation(app: &mut App) -> Result<()> {
    let image = app
        .selected_image
        .clone()
        .ok_or_else(|| anyhow!("Select an image first (focus the file picker, press Enter)"))?;
    let size_bytes = flash::parse_size(&app.size_input)
        .map_err(|e| anyhow!("Invalid size: {e}"))?;
    if size_bytes == 0 {
        return Err(anyhow!("Rootfs size must be > 0"));
    }
    if size_bytes % SECTOR_SIZE != 0 {
        return Err(anyhow!(
            "Size {} bytes is not a multiple of sector size {}",
            size_bytes, SECTOR_SIZE
        ));
    }
    if let Some(total) = app.sd_total_bytes {
        let max = flash::max_rootfs_bytes(total);
        if size_bytes > max {
            return Err(anyhow!(
                "Rootfs size {} MiB > {} MiB (half of SD - GPT). Won't fit.",
                size_bytes / (1024 * 1024),
                max / (1024 * 1024),
            ));
        }
    }
    if let Ok(meta) = std::fs::metadata(&image) {
        if meta.len() > size_bytes {
            return Err(anyhow!(
                "Image is {} MiB but rootfs partition is only {} MiB; pick rootfs >= {} MiB.",
                meta.len() / (1024 * 1024),
                size_bytes / (1024 * 1024),
                meta.len().div_ceil(1024 * 1024),
            ));
        }
    }

    let cfg = Config {
        image,
        rootfs_size_bytes: size_bytes,
        reset_after_flash: app.reset_after_flash,
        userdata_magic: app.userdata_magic,
    };
    let layout_matches = match (app.sd_total_sectors, app.sd_existing) {
        (Some(total_sectors), Some(existing)) => {
            flash::expected_layout(total_sectors, size_bytes)
                .ok()
                .map(|exp| exp == existing)
                .unwrap_or(false)
        }
        _ => false,
    };
    app.pending_confirm = Some(PendingConfirm { cfg, layout_matches });
    Ok(())
}

fn kickoff_flash(app: &mut App, cfg: Config) {
    *app.flashing.lock().unwrap() = true;
    let log = app.log.clone();
    let progress = app.progress.clone();
    let result_slot = app.flash_result.clone();

    log.lock().unwrap().push(format!(
        "Starting flash: image={} rootfs_size={} bytes reset={} userdata_magic={}",
        cfg.image.display(), cfg.rootfs_size_bytes, cfg.reset_after_flash, cfg.userdata_magic,
    ));

    thread::spawn(move || {
        let report = TuiReport { log: log.clone(), progress: progress.clone() };
        let res: Result<()> = (|| {
            let dev = device::open_single()?;
            flash::flash_image(dev, &cfg, &report)?;
            Ok(())
        })();
        *result_slot.lock().unwrap() = Some(res.map_err(|e| e.to_string()));
    });
}

struct TuiReport {
    log: Arc<Mutex<Vec<String>>>,
    progress: Arc<Mutex<Option<(u64, u64, String)>>>,
}

impl Report for TuiReport {
    fn stage(&self, msg: &str) {
        self.log.lock().unwrap().push(msg.to_string());
    }
    fn progress_begin(&self, total: u64, msg: &str) -> Box<dyn ProgressHandle> {
        *self.progress.lock().unwrap() = Some((0, total, msg.to_string()));
        Box::new(TuiProgress {
            slot: self.progress.clone(),
            log: self.log.clone(),
            msg: msg.to_string(),
        })
    }
}

struct TuiProgress {
    slot: Arc<Mutex<Option<(u64, u64, String)>>>,
    log: Arc<Mutex<Vec<String>>>,
    msg: String,
}

impl ProgressHandle for TuiProgress {
    fn inc(&mut self, delta: u64) {
        let mut g = self.slot.lock().unwrap();
        if let Some((cur, total, _)) = g.as_mut() {
            *cur = (*cur + delta).min(*total);
        }
    }
    fn finish(self: Box<Self>) {
        let mut g = self.slot.lock().unwrap();
        if let Some((cur, total, _)) = g.as_mut() {
            *cur = *total;
        }
        *g = None;
        self.log.lock().unwrap().push(format!("{}: done", self.msg));
    }
}

fn draw(f: &mut ratatui::Frame<'_>, app: &App) {
    let dev_lines = devices_line_count(app);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2 + dev_lines),
            Constraint::Min(10),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(7),
        ])
        .split(f.area());

    draw_devices(f, app, chunks[0]);
    draw_picker(f, app, chunks[1]);
    draw_size(f, app, chunks[2]);
    draw_button(f, app, chunks[3]);
    draw_log(f, app, chunks[4]);

    // Overlays in priority order (last drawn = on top)
    if let Some(pc) = &app.pending_confirm {
        draw_confirm_overlay(f, app, pc, f.area());
    }
    if app.success_banner {
        draw_success_overlay(f, app, f.area());
    }
    if let Some(msg) = &app.error_msg {
        draw_error_overlay(f, msg, f.area());
    }
}

fn devices_line_count(app: &App) -> u16 {
    let mut n = 0u16;
    if app.device_err.is_some() {
        n += 1;
    } else if app.devices.is_empty() {
        n += 1;
    } else {
        n += app.devices.len() as u16;
    }
    if app.sd_total_bytes.is_some() {
        n += 1;
    }
    if app.userdata_magic {
        n += 1;
    }
    n.max(1)
}

fn draw_devices(f: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(err) = &app.device_err {
        lines.push(Line::from(Span::styled(
            format!("Device discovery error: {err}"),
            Style::default().fg(Color::Red),
        )));
    } else if app.devices.is_empty() {
        lines.push(Line::from("No RockUSB devices found"));
    } else {
        for d in &app.devices {
            let status = if d.available { "ok" } else { "unavailable" };
            lines.push(Line::from(format!(
                "RockUSB bus {} addr {} ({status})",
                d.bus, d.address
            )));
        }
    }
    if let Some(s) = app.sd_total_bytes {
        lines.push(Line::from(format!("SD: {} MiB", s / (1024 * 1024))));
    }

    if app.userdata_magic {
        lines.push(Line::from(Span::styled(
            "WARNING: userdata-magic ON — userdata will be auto-wiped on next boot.",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Devices  (Tab cycles · q/Esc/Ctrl-C quits · r=reset · m=userdata-magic)"),
    );
    f.render_widget(p, area);
}

fn draw_picker(f: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let title = if let Some(p) = &app.selected_image {
        format!("Image (selected: {})", p.display())
    } else {
        "Image (Enter to pick)".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style(app.focus == Focus::Picker))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget_ref(app.explorer.widget(), inner);
}

fn draw_size(f: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let parsed = flash::parse_size(&app.size_input)
        .map(|b| format!(" = {} bytes ({} sectors)", b, b / SECTOR_SIZE))
        .unwrap_or_else(|e| format!(" (invalid: {e})"));
    let p = Paragraph::new(format!("{}{}", app.size_input, parsed)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style(app.focus == Focus::Size))
            .title(format!(
                "rootfs size (each of A and B), default rounded to {} MiB",
                flash::DEFAULT_ROUND_MIB
            )),
    );
    f.render_widget(p, area);
}

fn draw_button(f: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let busy = *app.flashing.lock().unwrap();
    let label = if busy { "[ Flashing... ]" } else { "[ Flash ]" };
    let style = if app.focus == Focus::FlashButton {
        Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let title = format!(
        "Flash (Enter)  ·  reset(r): {}  ·  userdata-magic(m): {}",
        if app.reset_after_flash { "ON" } else { "off" },
        if app.userdata_magic { "ON" } else { "off" },
    );
    let p = Paragraph::new(Span::styled(label, style)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style(app.focus == Focus::FlashButton))
            .title(title),
    );
    f.render_widget(p, area);
}

fn draw_log(f: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some((cur, total, msg)) = app.progress.lock().unwrap().clone() {
        let pct = if total == 0 { 0 } else { (cur * 100 / total).min(100) };
        lines.push(Line::from(format!("[{pct:3}%] {msg}: {cur}/{total}")));
    }
    let log = app.log.lock().unwrap();
    let take = log.len().saturating_sub(20);
    for s in &log[take..] {
        lines.push(Line::from(s.as_str()));
    }
    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title("Log"));
    f.render_widget(p, area);
}

fn draw_success_overlay(f: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let msg = if app.reset_after_flash {
        "Flash completed and device rebooted."
    } else {
        "Flash completed."
    };
    let popup = centered_rect(area, (msg.len() as u16 + 6).max(40), 5);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
        .title(" Success ");
    let p = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(msg, Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled("press any key to dismiss", Style::default().fg(Color::DarkGray))),
    ])
    .block(block)
    .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(p, popup);
}

fn draw_error_overlay(f: &mut ratatui::Frame<'_>, msg: &str, area: Rect) {
    let line_count = msg.lines().count() as u16;
    let h = (line_count + 4).clamp(5, area.height.saturating_sub(2));
    let w = msg
        .lines()
        .map(|l| l.len() as u16)
        .max()
        .unwrap_or(40)
        .saturating_add(6)
        .clamp(30, area.width.saturating_sub(4));
    let popup = centered_rect(area, w, h);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
        .title(" Error ");
    let lines: Vec<Line> = msg
        .lines()
        .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(Color::Red))))
        .chain(std::iter::once(Line::from("")))
        .chain(std::iter::once(Line::from(Span::styled(
            "press any key to dismiss",
            Style::default().fg(Color::DarkGray),
        ))))
        .collect();
    let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(p, popup);
}

fn draw_confirm_overlay(f: &mut ratatui::Frame<'_>, app: &App, pc: &PendingConfirm, area: Rect) {
    let cfg = &pc.cfg;
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Confirm flash",
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    let mib = 1024 * 1024;
    let push = |lines: &mut Vec<Line>, k: &str, v: String| {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<22}"), Style::default().fg(Color::DarkGray)),
            Span::raw(v),
        ]));
    };

    push(&mut lines, "image", cfg.image.display().to_string());
    if let Ok(meta) = std::fs::metadata(&cfg.image) {
        push(&mut lines, "image size", format!("{} MiB ({} bytes)", meta.len() / mib, meta.len()));
    }
    push(&mut lines, "rootfs A size", format!("{} MiB", cfg.rootfs_size_bytes / mib));
    push(&mut lines, "rootfs B size", format!("{} MiB", cfg.rootfs_size_bytes / mib));
    let userdata_estimate = app.sd_total_bytes.map(|t| {
        t.saturating_sub(2 * cfg.rootfs_size_bytes)
            .saturating_sub(flash::GPT_OVERHEAD_BYTES)
    });
    if let Some(u) = userdata_estimate {
        push(&mut lines, "userdata (approx)", format!("{} MiB", u / mib));
    }
    if let Some(s) = app.sd_total_bytes {
        push(&mut lines, "SD total", format!("{} MiB", s / mib));
    }
    if !app.devices.is_empty() {
        let d = &app.devices[0];
        push(&mut lines, "device", format!("RockUSB bus {} addr {}", d.bus, d.address));
    }
    push(
        &mut lines,
        "reset-after-flash",
        if cfg.reset_after_flash { "ON".to_string() } else { "off".to_string() },
    );
    push(
        &mut lines,
        "userdata-magic",
        if cfg.userdata_magic { "ON".to_string() } else { "off".to_string() },
    );
    // Warnings come BEFORE the key-hint line so they survive any height
    // clamping the terminal forces on us.
    //
    // - SD-erase warning: shown unless the device already has the exact
    //   GPT we'd write. In that case flash_image will only refresh
    //   rootfs_a, leaving rootfs_b and userdata intact.
    // - userdata-magic warning: shown whenever the user enabled magic,
    //   regardless of layout-match — userdata gets wiped at next boot
    //   either way.
    lines.push(Line::from(""));
    if pc.layout_matches {
        lines.push(Line::from(Span::styled(
            "Existing GPT matches: rootfs_b and userdata will be preserved; rootfs_a refreshed.",
            Style::default().fg(Color::Green),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "WARNING: this erases the entire SD card.",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    if cfg.userdata_magic {
        lines.push(Line::from(Span::styled(
            "WARNING: userdata-magic is ON — userdata will be auto-wiped on next boot.",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[y/Enter] confirm  [n/Esc] cancel  [r] reset  [m] magic",
        Style::default().fg(Color::DarkGray),
    )));

    // Width: prefer 80 so the long warning lines don't wrap. Fall back if
    // the terminal is narrower. Then size the popup to actually fit the
    // wrapped content — Paragraph clips overflow, and we don't want the
    // key-hint or warning rows to fall off the bottom (which is what
    // happened when userdata-magic was on and the userdata warning
    // wrapped onto a second row).
    let w = 80u16.min(area.width.saturating_sub(4)).max(40);
    let inner_w = w.saturating_sub(2) as usize;
    let mut needed = 0u16;
    for line in &lines {
        let lw = line.width().max(1);
        let rows = lw.div_ceil(inner_w.max(1));
        needed = needed.saturating_add(rows.max(1) as u16);
    }
    let h = (needed + 2).min(area.height.saturating_sub(2));
    let popup = centered_rect(area, w, h);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .title(" Confirm ");
    let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(p, popup);
}

fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn border_style(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    }
}
