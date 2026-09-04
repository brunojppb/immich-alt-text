use std::collections::VecDeque;
use std::time::{Duration, Instant};

use immich_alt_text::app::{App, InFlight, LogRow, RunState, Screen};
use immich_alt_text::config::Config;
use immich_alt_text::events::Stage;
use immich_alt_text::theme::Theme;
use immich_alt_text::ui;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn config() -> Config {
    let mut c = Config::default();
    c.immich.url = "https://photos.home.lan".into();
    c.immich.api_key = "secret-key".into();
    c.llm.model = "gemma-3-12b-it".into();
    c
}

/// A mid-run app with fixed numbers so the picture never changes between runs.
fn running_app(now: Instant) -> App {
    let mut app = App::new(config(), false);
    app.run_state = RunState::Running;
    app.scanned = 14_920;
    app.queued = 3_102;
    app.done = 1_284;
    app.failed = 3;
    app.run_started = Some(now - Duration::from_secs(6_137));
    app.llm_time_total = Duration::from_millis(4_100) * app.done as u32;
    app.asset_time_total = Duration::from_millis(4_700) * app.done as u32;
    // 20 completions 4.76 s apart give 12.6 per minute.
    app.recent = (0..20u32)
        .rev()
        .map(|i| now - Duration::from_millis(4_760) * i)
        .collect::<VecDeque<_>>();
    app.in_flight.push(InFlight {
        id: "a1".into(),
        name: "IMG_4471.HEIC".into(),
        stage: Stage::CallingLlm,
        started_at: now - Duration::from_millis(3_200),
    });
    app.log.push_back(LogRow::Done {
        at: "18:42:11".into(),
        name: "IMG_4470.HEIC".into(),
        took: Duration::from_millis(4_300),
        description: "A golden retriever sits on a wooden dock at sunset, looking toward the water while boats pass by.".into(),
    });
    app.log.push_back(LogRow::Done {
        at: "18:42:07".into(),
        name: "IMG_4469.HEIC".into(),
        took: Duration::from_millis(3_900),
        description: "Two children build a sandcastle on a crowded beach under a blue sky.".into(),
    });
    app.log.push_back(LogRow::Failed {
        at: "18:42:02".into(),
        name: "IMG_4468.HEIC".into(),
        error: "llm: timeout after 120 s (4 tries)".into(),
    });
    app.log.push_back(LogRow::Done {
        at: "18:41:58".into(),
        name: "IMG_4467.HEIC".into(),
        took: Duration::from_millis(4_000),
        description: "A plate of pasta with tomato sauce and basil on a white tablecloth.".into(),
    });
    app
}

fn snapshot(name: &str, width: u16, height: u16, app: &App, now: Instant) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::btop();
    terminal.draw(|f| ui::render(f, app, now, &theme)).unwrap();
    insta::assert_snapshot!(name, terminal.backend());
}

#[test]
fn run_screen_120x40() {
    let now = Instant::now();
    snapshot("run_120x40", 120, 40, &running_app(now), now);
}

#[test]
fn run_screen_80x24_keeps_side_by_side_and_shows_in_flight() {
    let now = Instant::now();
    let rendered = render_to_string(80, 24, &running_app(now), now);
    let layout_line = rendered.lines().nth(1).unwrap();
    assert!(layout_line.contains("╭ progress"));
    assert!(layout_line.contains("╭ counters"));
    assert!(rendered.contains("╭ in flight"));
    assert!(rendered.contains("done        1 284"));
    assert!(rendered.contains("failed          3"));
    assert!(rendered.contains("avg total   4.7 s"));
    assert!(rendered.contains("eta "));
    snapshot("run_80x24", 80, 24, &running_app(now), now);
}

#[test]
fn run_screen_40x10_is_tiny() {
    let now = Instant::now();
    let rendered = render_to_string(40, 10, &running_app(now), now);
    assert!(!rendered.contains("elapsed "));
    assert!(!rendered.contains("failed "));
    assert!(!rendered.contains("╭ progress"));
    assert!(!rendered.contains("╭ counters"));
    assert!(!rendered.contains("╭ log"));
    snapshot("run_40x10", 40, 10, &running_app(now), now);
}

#[test]
fn run_screen_79x23_stacks_and_hides_in_flight() {
    let now = Instant::now();
    let rendered = render_to_string(79, 23, &running_app(now), now);
    let layout_line = rendered.lines().nth(1).unwrap();
    assert!(layout_line.contains("╭ progress"));
    assert!(!layout_line.contains("╭ counters"));
    assert!(!rendered.contains("╭ in flight"));
}

#[test]
fn run_screen_idle() {
    let now = Instant::now();
    let app = App::new(config(), false);
    snapshot("run_idle_100x30", 100, 30, &app, now);
}

#[test]
fn run_screen_error_state() {
    let now = Instant::now();
    let mut app = running_app(now);
    app.run_state = RunState::Error("immich: HTTP 401 Unauthorized: check the API key".into());
    app.run_elapsed = Some(Duration::from_secs(61));
    app.in_flight.clear();
    snapshot("run_error_120x40", 120, 40, &app, now);
}

#[test]
fn log_popup() {
    let now = Instant::now();
    let mut app = running_app(now);
    app.log_expanded = true;
    snapshot("run_popup_120x40", 120, 40, &app, now);
}

#[test]
fn settings_screen() {
    let now = Instant::now();
    let mut app = App::new(config(), true);
    assert_eq!(app.screen, Screen::Settings);
    app.settings.focused = 2;
    app.settings.test_result = Some((Ok("v3.1.0".into()), Err("HTTP 401 Unauthorized".into())));
    snapshot("settings_80x24", 80, 24, &app, now);
}

#[test]
fn settings_screen_with_error_message() {
    let now = Instant::now();
    let mut app = App::new(config(), true);
    app.settings.show_secrets = true;
    app.settings.message = Some("invalid config: run.workers must be at least 1".into());
    snapshot("settings_error_100x30", 100, 30, &app, now);
}

fn render_to_string(width: u16, height: u16, app: &App, now: Instant) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::btop();
    terminal.draw(|f| ui::render(f, app, now, &theme)).unwrap();
    terminal.backend().to_string()
}
