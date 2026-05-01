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
    sd_total_bytes: Option<u64>,
    flash_result: Arc<Mutex<Option<Result<(), String>>>>,
    success_banner: bool,
    reset_after_flash: bool,
}

pub fn run() -> Result<()> {
    let explorer = FileExplorer::new()?;
    let (devices, device_err) = match device::list() {
        Ok(d) => (d, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    // Best-effort capacity probe; failures are surfaced in the log but don't
    // block opening the TUI (the user can still see device info etc).
    let (sd_total_bytes, probe_err) = match device::probe_sd_size() {
        Ok(s) => (Some(s), None),
        Err(e) => (None, Some(e.to_string())),
    };

    let mut log: Vec<String> = Vec::new();
    if let Some(s) = sd_total_bytes {
        log.push(format!("SD capacity: {} MiB", s / (1024 * 1024)));
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
        sd_total_bytes,
        flash_result: Arc::new(Mutex::new(None)),
        success_banner: false,
        reset_after_flash: true,
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

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;

        if event::poll(std::time::Duration::from_millis(100))? {
            let evt = event::read()?;
            if let Event::Key(key) = &evt {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                // Ctrl-C always exits, even mid-flash (flash thread will abort
                // when its USB calls fail; user accepted the risk).
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c'))
                {
                    return Ok(());
                }

                // Dismiss the success overlay on any key once it's shown.
                if app.success_banner {
                    app.success_banner = false;
                    continue;
                }

                let busy = *app.flashing.lock().unwrap();
                if busy {
                    // Don't quit mid-flash via plain q/Esc — would corrupt the SD card.
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Tab => {
                        app.focus = match app.focus {
                            Focus::Picker => Focus::Size,
                            Focus::Size => Focus::FlashButton,
                            Focus::FlashButton => Focus::Picker,
                        };
                        continue;
                    }
                    KeyCode::Char('r') if app.focus != Focus::Size => {
                        // Toggle reset-after-flash. Disabled while typing in the size field.
                        app.reset_after_flash = !app.reset_after_flash;
                        continue;
                    }
                    _ => {}
                }

                match app.focus {
                    Focus::Picker => {
                        if matches!(key.code, KeyCode::Enter) {
                            let f = app.explorer.current();
                            if f.is_file() {
                                on_image_selected(app, f.path().clone());
                                continue;
                            }
                        }
                        app.explorer.handle(&evt)?;
                    }
                    Focus::Size => match key.code {
                        KeyCode::Char(c) => app.size_input.push(c),
                        KeyCode::Backspace => { app.size_input.pop(); }
                        KeyCode::Enter => app.focus = Focus::FlashButton,
                        _ => {}
                    },
                    Focus::FlashButton => {
                        if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
                            start_flash(app)?;
                        }
                    }
                }
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
                Err(e) => app.log.lock().unwrap().push(format!("Flash failed: {e}")),
            }
            *app.flashing.lock().unwrap() = false;
            *app.progress.lock().unwrap() = None;
        }
    }
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
            app.log.lock().unwrap().push(format!(
                "Image is {} MiB but SD only fits a {} MiB rootfs (half of SD - GPT). \
                 Pick a smaller image or larger card.",
                img_len / (1024 * 1024),
                max / (1024 * 1024),
            ));
            // leave size_input untouched so user notices the problem
            return;
        }
        if rounded > max {
            // round DOWN to a whole MiB so it fits, prefer 128 MiB granularity
            // when it doesn't drop us below the image size
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

fn start_flash(app: &mut App) -> Result<()> {
    let image = app
        .selected_image
        .clone()
        .ok_or_else(|| anyhow!("Select an image first (focus the file picker, press Enter)"))?;
    let size_bytes = flash::parse_size(&app.size_input)
        .map_err(|e| anyhow!("Invalid size: {e}"))?;
    if size_bytes % SECTOR_SIZE != 0 {
        app.log.lock().unwrap().push(format!(
            "Size {} bytes is not a multiple of sector size {}", size_bytes, SECTOR_SIZE
        ));
        return Ok(());
    }
    if let Some(total) = app.sd_total_bytes {
        let max = flash::max_rootfs_bytes(total);
        if size_bytes > max {
            app.log.lock().unwrap().push(format!(
                "Rootfs size {} MiB > {} MiB (half of SD - GPT). Won't fit.",
                size_bytes / (1024 * 1024),
                max / (1024 * 1024),
            ));
            return Ok(());
        }
    }

    let cfg = Config {
        image: image.clone(),
        rootfs_size_bytes: size_bytes,
        reset_after_flash: app.reset_after_flash,
    };

    *app.flashing.lock().unwrap() = true;
    let log = app.log.clone();
    let progress = app.progress.clone();
    let result_slot = app.flash_result.clone();

    log.lock().unwrap().push(format!(
        "Starting flash: image={} rootfs_size={} bytes reset={}",
        image.display(), size_bytes, cfg.reset_after_flash,
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
    Ok(())
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

    if app.success_banner {
        draw_success_overlay(f, app, f.area());
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

    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Devices  (Tab cycles · q/Esc/Ctrl-C quits · r toggles reset-after-flash)"),
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
        "Flash (Enter)  ·  reset-after-flash: {}",
        if app.reset_after_flash { "ON" } else { "off" }
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
    let w = (msg.len() as u16 + 6).min(area.width);
    let h = 5u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect { x, y, width: w, height: h };

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

fn border_style(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    }
}
