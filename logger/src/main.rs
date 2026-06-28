mod config;

use anyhow::Context;
use axum::{Router, body::Bytes, extract::State, http::{HeaderMap, StatusCode}, routing::post};
use chrono::{NaiveTime, Timelike, Utc};
use clap::Parser;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs},
};
use rumqttc::{AsyncClient, Event as MqttEvent, MqttOptions, Packet, QoS};
use serde_json::json;
use std::{
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::time::MissedTickBehavior;

// ── shared state ─────────────────────────────────────────────────────────────

struct AppState {
    // schedule (logger-side, local time)
    interval_secs:      u64,
    window_start_local: NaiveTime,
    window_stop_local:  NaiveTime,
    utc_offset_hours:   i32,

    // per-capture settings (sent in senddata payload)
    exposure:   i32,
    gain:       i32,
    stack_secs: u32,  // 0 = single frame, 1-10 = stacking duration in seconds

    // ESP32 camera settings (sent via cmd topic on change)
    quality:      u8,
    gain_ceiling: u8,
    framesize:    u8,

    // status (written by background tasks)
    mqtt_connected:  bool,
    last_capture_at: Option<chrono::DateTime<Utc>>,
    last_capture_sz: usize,
    files_today:     u32,
    next_capture_at: Option<Instant>,
    log_lines:       Vec<String>,

    log_dir: PathBuf,
}

impl AppState {
    fn push_log(&mut self, line: String) {
        self.log_lines.push(line);
        if self.log_lines.len() > 30 {
            self.log_lines.remove(0);
        }
    }
}

type Shared = Arc<Mutex<AppState>>;

// ── TUI app ──────────────────────────────────────────────────────────────────

struct TuiApp {
    tab:          usize,
    selected:     [usize; 2],  // per tab (schedule, camera)
    camera_dirty: bool,        // camera cmd fields changed but not yet sent
}

impl TuiApp {
    fn new() -> Self { Self { tab: 0, selected: [0, 0], camera_dirty: false } }

    fn tab_field_count(&self) -> usize {
        match self.tab { 0 => 4, 1 => 6, _ => 0 }
    }

    fn sel(&self) -> usize { self.selected[self.tab.min(1)] }

    fn next_tab(&mut self) { self.tab = (self.tab + 1) % 3; }
    fn prev_tab(&mut self) { self.tab = (self.tab + 2) % 3; }

    fn move_up(&mut self) {
        let s = &mut self.selected[self.tab.min(1)];
        if *s > 0 { *s -= 1; }
    }
    fn move_down(&mut self) {
        let max = self.tab_field_count();
        let s = &mut self.selected[self.tab.min(1)];
        if *s + 1 < max { *s += 1; }
    }
}

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(about = "moon_shadow_photo — periodic photo logger")]
struct Args {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = config::load(&args.config)
        .with_context(|| format!("load {}", args.config.display()))?;

    let (start_h, start_m) = config::parse_hhmm(&cfg.start_time)?;
    let (stop_h,  stop_m)  = config::parse_hhmm(&cfg.stop_time)?;
    let window_start_local = NaiveTime::from_hms_opt(start_h, start_m, 0).unwrap();
    let window_stop_local  = NaiveTime::from_hms_opt(stop_h,  stop_m,  0).unwrap();

    let log_dir = PathBuf::from(&cfg.log_dir);
    check_log_dir(&log_dir)?;

    let today = Utc::now().date_naive();
    let files_today = count_jpgs(&log_dir.join(today.format("%Y-%m-%d").to_string()));

    let state: Shared = Arc::new(Mutex::new(AppState {
        interval_secs: cfg.interval,
        window_start_local,
        window_stop_local,
        utc_offset_hours: cfg.utc_offset_hours,
        exposure:     cfg.exposure,
        gain:         cfg.gain,
        stack_secs:   cfg.stack_secs,
        quality:      cfg.quality,
        gain_ceiling: cfg.gain_ceiling,
        framesize:    cfg.framesize,
        mqtt_connected:  false,
        last_capture_at: None,
        last_capture_sz: 0,
        files_today,
        next_capture_at: None,
        log_lines:       Vec::new(),
        log_dir:         log_dir.clone(),
    }));

    // MQTT
    let mut mqtt_opts = MqttOptions::new(&cfg.client_id, &cfg.broker, cfg.port);
    mqtt_opts.set_keep_alive(Duration::from_secs(30));
    if !cfg.mqtt_user.is_empty() {
        mqtt_opts.set_credentials(&cfg.mqtt_user, &cfg.mqtt_pass);
    }
    let (client, mut eventloop) = AsyncClient::new(mqtt_opts, 32);

    // Spawn: MQTT event loop (tracks connectivity)
    let state_mqtt = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(MqttEvent::Incoming(Packet::ConnAck(_))) => {
                    state_mqtt.lock().unwrap().mqtt_connected = true;
                }
                Err(_) => {
                    state_mqtt.lock().unwrap().mqtt_connected = false;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Ok(MqttEvent::Incoming(Packet::Disconnect)) => {
                    state_mqtt.lock().unwrap().mqtt_connected = false;
                }
                _ => {}
            }
        }
    });

    // Spawn: HTTP server (receives JPEG uploads)
    let state_http = Arc::clone(&state);
    let http_port = cfg.http_port;
    tokio::spawn(async move { run_http_server(state_http, http_port).await });

    // Spawn: capture publisher
    let state_pub = Arc::clone(&state);
    let client_pub = client.clone();
    let sensor_id = cfg.sensor_id.clone();
    tokio::spawn(async move {
        publisher_task(state_pub, client_pub, sensor_id).await;
    });

    // Send initial camera settings to ESP32 when MQTT connects (best-effort)
    {
        let s = state.lock().unwrap();
        let cmd_topic = format!("{}/cmd", cfg.sensor_id);
        let q  = s.quality;
        let gc = s.gain_ceiling;
        let fs = s.framesize;
        drop(s);
        let c = client.clone();
        let t = cmd_topic.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let _ = c.publish(&t, QoS::AtMostOnce, false,
                json!({"cmd":"SetQuality","value":q}).to_string()).await;
            let _ = c.publish(&t, QoS::AtMostOnce, false,
                json!({"cmd":"SetGainCeiling","value":gc}).to_string()).await;
            let _ = c.publish(&t, QoS::AtMostOnce, false,
                json!({"cmd":"SetFramesize","value":fs}).to_string()).await;
        });
    }

    // Run TUI (blocks until 'q')
    run_tui(state, client, cfg.sensor_id).await?;

    Ok(())
}

// ── publisher task ────────────────────────────────────────────────────────────

async fn publisher_task(state: Shared, client: AsyncClient, sensor_id: String) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_sent = Instant::now()
        .checked_sub(Duration::from_secs(9999))
        .unwrap_or(Instant::now());

    loop {
        ticker.tick().await;

        let (interval, in_win, exposure, gain, stack_secs) = {
            let mut s = state.lock().unwrap();
            let interval = s.interval_secs;
            let in_win = in_window_local(s.window_start_local, s.window_stop_local, s.utc_offset_hours);
            let elapsed = last_sent.elapsed();

            s.next_capture_at = if in_win && elapsed < Duration::from_secs(interval) {
                Some(Instant::now() + (Duration::from_secs(interval) - elapsed))
            } else if in_win {
                Some(Instant::now())
            } else {
                None
            };

            (interval, in_win, s.exposure, s.gain, s.stack_secs)
        };

        if in_win && last_sent.elapsed() >= Duration::from_secs(interval) {
            let payload = json!({"exposure": exposure, "gain": gain, "stack_secs": stack_secs}).to_string();
            if client.publish(
                format!("{}/senddata", sensor_id),
                QoS::AtMostOnce, false, payload.as_bytes(),
            ).await.is_ok() {
                let ts = Utc::now().format("%H:%M:%S UTC").to_string();
                let line = format!("{} → senddata exp={} gain={} stack={}s", ts, exposure, gain, stack_secs);
                state.lock().unwrap().push_log(line);
            }
            last_sent = Instant::now();
        }
    }
}

// ── TUI ───────────────────────────────────────────────────────────────────────

async fn run_tui(state: Shared, client: AsyncClient, sensor_id: String) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = TuiApp::new();
    let mut events = EventStream::new();
    let mut redraw = tokio::time::interval(Duration::from_millis(200));

    let cmd_topic      = format!("{}/cmd",      sensor_id);
    let senddata_topic = format!("{}/senddata", sensor_id);
    let result: anyhow::Result<()> = loop {
        tokio::select! {
            _ = redraw.tick() => {
                let s = state.lock().unwrap();
                terminal.draw(|f| draw(f, &app, &s))?;
            }
            Some(Ok(ev)) = events.next() => {
                if let Event::Key(key) = ev {
                    if key.kind != KeyEventKind::Press { continue; }
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                        KeyCode::Tab       => app.next_tab(),
                        KeyCode::BackTab   => app.prev_tab(),
                        KeyCode::Up        => app.move_up(),
                        KeyCode::Down      => app.move_down(),
                        KeyCode::Left | KeyCode::Char('-') => {
                            let cmd = adjust(&mut state.lock().unwrap(), &app, -1);
                            if app.tab == 1 {
                                if cmd.is_some() { app.camera_dirty = true; }
                            } else if let Some(cmd) = cmd {
                                let c = client.clone(); let t = cmd_topic.clone();
                                tokio::spawn(async move { let _ = c.publish(t, QoS::AtMostOnce, false, cmd).await; });
                            }
                        }
                        KeyCode::Right | KeyCode::Char('+') => {
                            let cmd = adjust(&mut state.lock().unwrap(), &app, 1);
                            if app.tab == 1 {
                                if cmd.is_some() { app.camera_dirty = true; }
                            } else if let Some(cmd) = cmd {
                                let c = client.clone(); let t = cmd_topic.clone();
                                tokio::spawn(async move { let _ = c.publish(t, QoS::AtMostOnce, false, cmd).await; });
                            }
                        }
                        KeyCode::Char('s') | KeyCode::Char('S') if app.tab == 1 => {
                            let (q, gc, fs) = {
                                let s = state.lock().unwrap();
                                (s.quality, s.gain_ceiling, s.framesize)
                            };
                            let c = client.clone(); let t = cmd_topic.clone();
                            let state2 = Arc::clone(&state);
                            tokio::spawn(async move {
                                let _ = c.publish(&t, QoS::AtMostOnce, false,
                                    json!({"cmd":"SetQuality","value":q}).to_string()).await;
                                let _ = c.publish(&t, QoS::AtMostOnce, false,
                                    json!({"cmd":"SetGainCeiling","value":gc}).to_string()).await;
                                let _ = c.publish(&t, QoS::AtMostOnce, false,
                                    json!({"cmd":"SetFramesize","value":fs}).to_string()).await;
                                let ts = chrono::Utc::now().format("%H:%M:%S UTC").to_string();
                                state2.lock().unwrap().push_log(
                                    format!("{} → camera cmd sent: Q={} GC={} FS={}", ts, q, gc, fs));
                            });
                            app.camera_dirty = false;
                        }
                        KeyCode::Char('c') | KeyCode::Char('C') => {
                            let (exposure, gain, stack_secs) = {
                                let s = state.lock().unwrap();
                                (s.exposure, s.gain, s.stack_secs)
                            };
                            let payload = json!({"exposure": exposure, "gain": gain, "stack_secs": stack_secs}).to_string();
                            let c = client.clone();
                            let t = senddata_topic.clone();
                            let state2 = Arc::clone(&state);
                            tokio::spawn(async move {
                                if c.publish(t, QoS::AtMostOnce, false, payload.as_bytes()).await.is_ok() {
                                    let ts = Utc::now().format("%H:%M:%S UTC").to_string();
                                    let line = format!("{} → MANUAL exp={} gain={} stack={}s", ts, exposure, gain, stack_secs);
                                    state2.lock().unwrap().push_log(line);
                                }
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    result
}

// ── key handlers ─────────────────────────────────────────────────────────────

/// Adjust selected field by `delta` (+1 or -1). Returns Some(mqtt_cmd_payload) if ESP32 needs updating.
fn adjust(s: &mut AppState, app: &TuiApp, delta: i64) -> Option<String> {
    match (app.tab, app.sel()) {
        // ── Schedule ─────────────────────────────────────────────────────────
        (0, 0) => {  // interval
            s.interval_secs = (s.interval_secs as i64 + delta * 5).max(5).min(3600) as u64;
            None
        }
        (0, 1) => {  // window start (local) ± 5 min
            s.window_start_local = shift_time(s.window_start_local, delta * 5);
            None
        }
        (0, 2) => {  // window stop (local) ± 5 min
            s.window_stop_local = shift_time(s.window_stop_local, delta * 5);
            None
        }
        (0, 3) => {  // UTC offset ± 1h
            s.utc_offset_hours = (s.utc_offset_hours + delta as i32).max(-12).min(14);
            None
        }
        // ── Camera ───────────────────────────────────────────────────────────
        (1, 0) => {  // exposure: -1 or 0-1200 step 50
            s.exposure = if delta > 0 {
                if s.exposure < 0 { 0 } else { (s.exposure + 50).min(1200) }
            } else {
                if s.exposure <= 0 { -1 } else { (s.exposure - 50).max(0) }
            };
            None  // exposure goes in senddata, no cmd needed
        }
        (1, 1) => {  // gain: -1 or 0-30
            s.gain = if delta > 0 {
                if s.gain < 0 { 0 } else { (s.gain + 1).min(30) }
            } else {
                if s.gain <= 0 { -1 } else { s.gain - 1 }
            };
            None  // gain goes in senddata
        }
        (1, 2) => {  // quality 1-63
            s.quality = (s.quality as i64 + delta).max(1).min(63) as u8;
            Some(json!({"cmd":"SetQuality","value": s.quality}).to_string())
        }
        (1, 3) => {  // gain ceiling 0-6
            s.gain_ceiling = (s.gain_ceiling as i64 + delta).max(0).min(6) as u8;
            Some(json!({"cmd":"SetGainCeiling","value": s.gain_ceiling}).to_string())
        }
        (1, 4) => {  // stack_secs 0-10 (goes in senddata, no cmd)
            s.stack_secs = (s.stack_secs as i64 + delta).max(0).min(10) as u32;
            None
        }
        (1, 5) => {  // framesize 0-13
            s.framesize = (s.framesize as i64 + delta).max(0).min(13) as u8;
            Some(json!({"cmd":"SetFramesize","value": s.framesize}).to_string())
        }
        _ => None,
    }
}


// ── draw ──────────────────────────────────────────────────────────────────────

fn draw(f: &mut Frame, app: &TuiApp, s: &AppState) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Length(3),  // tab bar
            Constraint::Min(0),     // content
            Constraint::Length(1),  // footer
        ])
        .split(f.area());

    // Header
    let mqtt_icon = if s.mqtt_connected { "● MQTT" } else { "○ MQTT" };
    let mqtt_style = if s.mqtt_connected { Style::default().fg(Color::Green) } else { Style::default().fg(Color::Red) };
    let local_str = local_time_str(s.utc_offset_hours);
    let offset_str = if s.utc_offset_hours >= 0 {
        format!("UTC+{}", s.utc_offset_hours)
    } else {
        format!("UTC{}", s.utc_offset_hours)
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled("  moon_shadow_photo  ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!("{}  {}  ", local_str, offset_str)),
        Span::styled(mqtt_icon, mqtt_style),
    ])).block(Block::default().borders(Borders::ALL));
    f.render_widget(header, outer[0]);

    // Tabs
    let tab_titles = vec![
        Line::from("  Schedule  "),
        Line::from("  Camera  "),
        Line::from("  Status  "),
    ];
    let tabs = Tabs::new(tab_titles)
        .select(app.tab)
        .block(Block::default().borders(Borders::ALL))
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
    f.render_widget(tabs, outer[1]);

    // Content
    match app.tab {
        0 => draw_schedule(f, outer[2], app, s),
        1 => draw_camera(f, outer[2], app, s),
        _ => draw_status(f, outer[2], s),
    }

    // Footer
    let footer_text = if app.tab == 1 {
        "  ↑↓ nav  ←→ adjust  s send to device  c capture now  Tab next  q quit"
    } else {
        "  ↑↓ nav  ←→ adjust  c capture now  Tab next  q quit"
    };
    let footer = Paragraph::new(footer_text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, outer[3]);
}

fn hl(selected: bool) -> Style {
    if selected {
        Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn val_style(selected: bool) -> Style {
    if selected {
        Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Cyan)
    }
}

fn draw_schedule(f: &mut Frame, area: ratatui::layout::Rect, app: &TuiApp, s: &AppState) {
    let in_win = in_window_local(s.window_start_local, s.window_stop_local, s.utc_offset_hours);
    let status_line = if in_win {
        let countdown = s.next_capture_at
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or_default();
        format!("● ACTIVE — next capture in {}s", countdown.as_secs())
    } else {
        "○ outside window".to_string()
    };
    let status_color = if in_win { Color::Green } else { Color::DarkGray };

    let interval_too_short = s.stack_secs > 0 && s.interval_secs <= s.stack_secs as u64;
    let int_val_style = if interval_too_short {
        if app.sel() == 0 && app.tab == 0 {
            Style::default().fg(Color::Black).bg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
    } else {
        val_style(app.sel() == 0 && app.tab == 0)
    };

    let fields: Vec<Row> = vec![
        Row::new(vec![
            Cell::from("  Interval").style(hl(app.sel() == 0 && app.tab == 0)),
            Cell::from(format!("{:>6} s", s.interval_secs)).style(int_val_style),
            Cell::from(if interval_too_short { "⚠ must be > stack_secs" } else { "" }),
        ]),
        Row::new(vec![
            Cell::from("  Window start").style(hl(app.sel() == 1 && app.tab == 0)),
            Cell::from(format!("{}", s.window_start_local.format("%H:%M"))).style(val_style(app.sel() == 1 && app.tab == 0)),
            Cell::from("local  (±5 min)"),
        ]),
        Row::new(vec![
            Cell::from("  Window stop").style(hl(app.sel() == 2 && app.tab == 0)),
            Cell::from(format!("{}", s.window_stop_local.format("%H:%M"))).style(val_style(app.sel() == 2 && app.tab == 0)),
            Cell::from("local  (±5 min)"),
        ]),
        Row::new(vec![
            Cell::from("  UTC offset").style(hl(app.sel() == 3 && app.tab == 0)),
            Cell::from(format!("{:>+5}h", s.utc_offset_hours)).style(val_style(app.sel() == 3 && app.tab == 0)),
            Cell::from("hours"),
        ]),
        Row::new(vec![Cell::from(""), Cell::from(""), Cell::from("")]),
        Row::new(vec![
            Cell::from("  Status"),
            Cell::from(status_line).style(Style::default().fg(status_color)),
            Cell::from(""),
        ]),
    ];

    let table = Table::new(fields, [Constraint::Length(18), Constraint::Length(12), Constraint::Min(0)])
        .block(Block::default().borders(Borders::ALL).title(" Schedule "));
    f.render_widget(table, area);
}

const FRAMESIZE_LABELS: &[&str] = &[
    "96×96", "160×120 QQVGA", "176×144 QCIF", "240×176 HQVGA",
    "240×240", "320×240 QVGA", "400×296 CIF", "480×320 HVGA",
    "640×480 VGA", "800×600 SVGA", "1024×768 XGA", "1280×720 HD",
    "1280×1024 SXGA", "1600×1200 UXGA",
];

fn draw_camera(f: &mut Frame, area: ratatui::layout::Rect, app: &TuiApp, s: &AppState) {
    let exp_label = if s.exposure < 0 { "auto AEC".to_string() } else { format!("{} lines", s.exposure) };
    let gain_label = if s.gain < 0 { "auto AGC".to_string() } else { format!("{}", s.gain) };
    let gc_labels = ["2×","4×","8×","16×","32×","64×","128×"];
    let gc_label = gc_labels.get(s.gain_ceiling as usize).unwrap_or(&"?");

    let fields: Vec<Row> = vec![
        Row::new(vec![
            Cell::from("  Exposure").style(hl(app.sel() == 0 && app.tab == 1)),
            Cell::from(format!("{:>5}", s.exposure)).style(val_style(app.sel() == 0 && app.tab == 1)),
            Cell::from(exp_label),
        ]),
        Row::new(vec![
            Cell::from("  Gain").style(hl(app.sel() == 1 && app.tab == 1)),
            Cell::from(format!("{:>5}", s.gain)).style(val_style(app.sel() == 1 && app.tab == 1)),
            Cell::from(gain_label),
        ]),
        Row::new(vec![
            Cell::from("  Quality").style(hl(app.sel() == 2 && app.tab == 1)),
            Cell::from(format!("{:>5}", s.quality)).style(val_style(app.sel() == 2 && app.tab == 1)),
            Cell::from("1-63, lower = better"),
        ]),
        Row::new(vec![
            Cell::from("  Gain ceiling").style(hl(app.sel() == 3 && app.tab == 1)),
            Cell::from(format!("{:>5}", s.gain_ceiling)).style(val_style(app.sel() == 3 && app.tab == 1)),
            Cell::from(format!("{}  (0-6)", gc_label)),
        ]),
        {
            let stack_warn = s.stack_secs > 0 && s.interval_secs <= s.stack_secs as u64;
            let stack_style = if stack_warn {
                if app.sel() == 4 && app.tab == 1 {
                    Style::default().fg(Color::Black).bg(Color::Red).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                }
            } else {
                val_style(app.sel() == 4 && app.tab == 1)
            };
            let hint = if s.stack_secs == 0 {
                "single frame  (0-10s)".to_string()
            } else {
                format!("{}s effective exposure  — interval must be >{}s", s.stack_secs, s.stack_secs)
            };
            Row::new(vec![
                Cell::from("  Stack secs").style(hl(app.sel() == 4 && app.tab == 1)),
                Cell::from(format!("{:>5}", s.stack_secs)).style(stack_style),
                Cell::from(hint),
            ])
        },
        {
            let fs = s.framesize as usize;
            let fs_label = FRAMESIZE_LABELS.get(fs).copied().unwrap_or("?");
            let psram_warn = s.stack_secs > 0 && s.framesize >= 13;
            let fs_style = if psram_warn {
                if app.sel() == 5 && app.tab == 1 {
                    Style::default().fg(Color::Black).bg(Color::Red).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                }
            } else {
                val_style(app.sel() == 5 && app.tab == 1)
            };
            let hint = if psram_warn {
                format!("{}  ⚠ UXGA+stack may exhaust PSRAM", fs_label)
            } else {
                format!("{}  (0-13)", fs_label)
            };
            Row::new(vec![
                Cell::from("  Framesize").style(hl(app.sel() == 5 && app.tab == 1)),
                Cell::from(format!("{:>5}", s.framesize)).style(fs_style),
                Cell::from(hint),
            ])
        },
    ];

    let cam_title = if app.camera_dirty { " Camera  ● unsaved " } else { " Camera " };
    let table = Table::new(fields, [Constraint::Length(18), Constraint::Length(12), Constraint::Min(0)])
        .block(Block::default().borders(Borders::ALL).title(cam_title));
    f.render_widget(table, area);
}

fn draw_status(f: &mut Frame, area: ratatui::layout::Rect, s: &AppState) {
    let content = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(0)])
        .split(area);

    let mqtt_str = if s.mqtt_connected { "● Connected" } else { "○ Disconnected" };
    let mqtt_color = if s.mqtt_connected { Color::Green } else { Color::Red };
    let last_cap = s.last_capture_at
        .map(|t| format!("{}  ({} bytes)", t.format("%Y-%m-%d %H:%M:%S UTC"), s.last_capture_sz))
        .unwrap_or_else(|| "—".to_string());

    let info = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  MQTT          "),
            Span::styled(mqtt_str, Style::default().fg(mqtt_color)),
        ]),
        Line::from(format!("  Last capture  {}", last_cap)),
        Line::from(format!("  Files today   {}", s.files_today)),
        Line::from(format!("  Log dir       {}", s.log_dir.display())),
    ];
    let info_widget = Paragraph::new(info)
        .block(Block::default().borders(Borders::ALL).title(" Status "));
    f.render_widget(info_widget, content[0]);

    // Recent log
    let log_lines: Vec<Line> = s.log_lines.iter().rev().take(20)
        .map(|l| Line::from(format!("  {}", l)))
        .collect();
    let log_widget = Paragraph::new(log_lines)
        .block(Block::default().borders(Borders::ALL).title(" Recent activity "));
    f.render_widget(log_widget, content[1]);
}

// ── HTTP server ───────────────────────────────────────────────────────────────

async fn run_http_server(state: Shared, port: u16) {
    let addr = format!("0.0.0.0:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => {
            state.lock().unwrap().push_log(format!("HTTP listening on {addr}"));
            l
        }
        Err(e) => {
            state.lock().unwrap().push_log(
                format!("⚠ HTTP bind {addr} failed: {e}  (old process still running?)"));
            return;
        }
    };

    let app = Router::new()
        .route("/upload", post(handle_upload))
        .with_state(state.clone());

    if let Err(e) = axum::serve(listener, app).await {
        state.lock().unwrap().push_log(format!("⚠ HTTP server stopped: {e}"));
    }
}

async fn handle_upload(
    State(state): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if body.len() < 2 || body[0] != 0xFF || body[1] != 0xD8 {
        let msg = format!("⚠ upload rejected: bad JPEG magic ({} B, first bytes: {:02X} {:02X})",
            body.len(),
            body.first().copied().unwrap_or(0),
            body.get(1).copied().unwrap_or(0));
        state.lock().unwrap().push_log(msg);
        return StatusCode::BAD_REQUEST;
    }

    let timestamp = headers
        .get("x-timestamp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");

    let (log_dir, files_today, ts_clean) = {
        let s = state.lock().unwrap();
        let ts_clean: String = timestamp.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(15)
            .collect();
        (s.log_dir.clone(), s.files_today, ts_clean)
    };

    let today = Utc::now().date_naive();
    let dir = log_dir.join(today.format("%Y-%m-%d").to_string());

    if let Err(e) = std::fs::create_dir_all(&dir) {
        let msg = format!("⚠ upload: mkdir {} failed: {}", dir.display(), e);
        state.lock().unwrap().push_log(msg);
        return StatusCode::INTERNAL_SERVER_ERROR;
    }

    let filename = format!("{}_{:03}.jpg", ts_clean, files_today);
    let path = dir.join(&filename);

    if let Err(e) = std::fs::write(&path, &body) {
        let msg = format!("⚠ upload: write {} failed: {}", path.display(), e);
        state.lock().unwrap().push_log(msg);
        return StatusCode::INTERNAL_SERVER_ERROR;
    }

    let truncated = body.len() < 4
        || body[body.len() - 2] != 0xFF
        || body[body.len() - 1] != 0xD9;

    let mut s = state.lock().unwrap();
    s.last_capture_at = Some(Utc::now());
    s.last_capture_sz = body.len();
    s.files_today += 1;
    if truncated {
        s.push_log(format!("{} ⚠ {} ({} B) — truncated JPEG (FB-OVF on device, reduce gain or raise quality#)",
            timestamp, filename, body.len()));
    } else {
        s.push_log(format!("{} saved {}  ({} B)", timestamp, filename, body.len()));
    }

    StatusCode::OK
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn in_window_local(start_local: NaiveTime, stop_local: NaiveTime, utc_offset_hours: i32) -> bool {
    let start_utc = local_to_utc(start_local, utc_offset_hours);
    let stop_utc  = local_to_utc(stop_local,  utc_offset_hours);
    let now = Utc::now().time();
    if start_utc <= stop_utc {
        now >= start_utc && now < stop_utc
    } else {
        now >= start_utc || now < stop_utc
    }
}

fn local_to_utc(local: NaiveTime, utc_offset_hours: i32) -> NaiveTime {
    let secs = local.num_seconds_from_midnight() as i64 - utc_offset_hours as i64 * 3600;
    let secs = ((secs % 86400) + 86400) as u32 % 86400;
    NaiveTime::from_num_seconds_from_midnight_opt(secs, 0).unwrap()
}

fn shift_time(t: NaiveTime, delta_mins: i64) -> NaiveTime {
    let secs = t.num_seconds_from_midnight() as i64 + delta_mins * 60;
    let secs = ((secs % 86400) + 86400) as u32 % 86400;
    NaiveTime::from_num_seconds_from_midnight_opt(secs, 0).unwrap()
}

fn local_time_str(utc_offset_hours: i32) -> String {
    let local = Utc::now().naive_utc() + chrono::Duration::hours(utc_offset_hours as i64);
    local.format("%Y-%m-%d %H:%M:%S").to_string()
}


fn count_jpgs(dir: &PathBuf) -> u32 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map(|x| x == "jpg").unwrap_or(false))
                .count() as u32
        })
        .unwrap_or(0)
}

fn check_log_dir(log_dir: &PathBuf) -> anyhow::Result<()> {
    if !log_dir.exists() {
        std::fs::create_dir_all(log_dir)
            .with_context(|| format!("Cannot create log_dir: {}", log_dir.display()))?;
    }
    anyhow::ensure!(log_dir.is_dir(), "log_dir is not a directory: {}", log_dir.display());
    let sentinel = log_dir.join(".write_check");
    std::fs::write(&sentinel, b"ok")
        .with_context(|| format!("log_dir not writable: {}", log_dir.display()))?;
    std::fs::remove_file(&sentinel).ok();
    Ok(())
}
