use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui_explorer::FileExplorer;

use crate::device;
use crate::flash::{self, Report, ProgressHandle};

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
    flash_result: Arc<Mutex<Option<Result<(), String>>>>,
}

pub fn run() -> Result<()> {
    let explorer = FileExplorer::new()?;
    let (devices, device_err) = match device::list() {
        Ok(d) => (d, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };

    let mut app = App {
        explorer,
        selected_image: None,
        size_input: "256MiB".to_string(),
        focus: Focus::Picker,
        log: Arc::new(Mutex::new(Vec::new())),
        progress: Arc::new(Mutex::new(None)),
        flashing: Arc::new(Mutex::new(false)),
        devices,
        device_err,
        flash_result: Arc::new(Mutex::new(None)),
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

                let busy = *app.flashing.lock().unwrap();
                if busy {
                    if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                        // ignore quit while flashing — would corrupt the SD card
                    }
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
                    _ => {}
                }

                match app.focus {
                    Focus::Picker => {
                        if matches!(key.code, KeyCode::Enter) {
                            let f = app.explorer.current();
                            if f.is_file() {
                                app.selected_image = Some(f.path().clone());
                                if let Ok(meta) = std::fs::metadata(f.path()) {
                                    let rounded = flash::round_up_mib(meta.len(), flash::DEFAULT_ROUND_MIB);
                                    let mib = rounded / (1024 * 1024);
                                    app.size_input = format!("{mib}MiB");
                                }
                                app.focus = Focus::Size;
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
                Ok(()) => app.log.lock().unwrap().push("Flash completed.".to_string()),
                Err(e) => app.log.lock().unwrap().push(format!("Flash failed: {e}")),
            }
            *app.flashing.lock().unwrap() = false;
            *app.progress.lock().unwrap() = None;
        }
    }
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

    *app.flashing.lock().unwrap() = true;
    let log = app.log.clone();
    let progress = app.progress.clone();
    let result_slot = app.flash_result.clone();

    log.lock().unwrap().push(format!(
        "Starting flash: image={} rootfs_size={} bytes", image.display(), size_bytes
    ));

    thread::spawn(move || {
        let report = TuiReport { log: log.clone(), progress: progress.clone() };
        let res: Result<()> = (|| {
            let dev = device::open_single()?;
            flash::flash_image(dev, &image, size_bytes, &report)?;
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if app.devices.is_empty() && app.device_err.is_none() {
                3
            } else {
                3 + app.devices.len() as u16 + app.device_err.is_some() as u16
            }),
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

    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Devices (Tab to cycle focus, q to quit)"),
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
    let p = Paragraph::new(Span::styled(label, style)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style(app.focus == Focus::FlashButton))
            .title("Flash (Enter)"),
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

fn border_style(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    }
}
