//! Pure UI state. No I/O. `on_event` and `on_key` are the only ways it changes.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::events::{Action, Command, Event, Key, Stage};
use crate::settings::SettingsForm;

pub const LOG_CAP: usize = 500;
pub const RATE_WINDOW: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Run,
    Settings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Running,
    Paused,
    Finished,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    pub id: String,
    pub name: String,
    pub stage: Stage,
    pub started_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogRow {
    Done {
        at: String,
        name: String,
        took: Duration,
        description: String,
    },
    Failed {
        at: String,
        name: String,
        error: String,
    },
}

pub struct App {
    pub config: Config,
    pub screen: Screen,
    pub run_state: RunState,
    pub scanned: u64,
    pub queued: u64,
    pub done: u64,
    pub failed: u64,
    pub in_flight: Vec<InFlight>,
    /// Newest first.
    pub log: VecDeque<LogRow>,
    pub log_selected: usize,
    pub log_expanded: bool,
    pub run_started: Option<Instant>,
    /// Set when a run ends so the clock stops.
    pub run_elapsed: Option<Duration>,
    /// Completion instants for the rate estimate. Oldest first.
    pub recent: VecDeque<Instant>,
    pub llm_time_total: Duration,
    pub asset_time_total: Duration,
    pub settings: SettingsForm,
    pub footer_message: Option<String>,
    pub should_quit: bool,
    connection_test_id: u64,
}

impl App {
    pub fn new(config: Config, first_run: bool) -> Self {
        let settings = SettingsForm::from_config(&config);
        Self {
            config,
            screen: if first_run {
                Screen::Settings
            } else {
                Screen::Run
            },
            run_state: RunState::Idle,
            scanned: 0,
            queued: 0,
            done: 0,
            failed: 0,
            in_flight: Vec::new(),
            log: VecDeque::new(),
            log_selected: 0,
            log_expanded: false,
            run_started: None,
            run_elapsed: None,
            recent: VecDeque::new(),
            llm_time_total: Duration::ZERO,
            asset_time_total: Duration::ZERO,
            settings,
            footer_message: None,
            should_quit: false,
            connection_test_id: 0,
        }
    }

    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::PageLoaded { scanned, queued } => {
                self.scanned = scanned;
                self.queued = queued;
            }
            Event::DiscoveryDone { total_queued } => {
                self.queued = total_queued;
            }
            Event::AssetStarted { id, name } => self.in_flight.push(InFlight {
                id,
                name,
                stage: Stage::Fetching,
                started_at: Instant::now(),
            }),
            Event::AssetStage { id, stage } => {
                if let Some(in_flight) = self.in_flight.iter_mut().find(|entry| entry.id == id) {
                    in_flight.stage = stage;
                }
            }
            Event::AssetDone {
                id,
                name,
                description,
                took,
                llm_took,
            } => {
                self.in_flight.retain(|entry| entry.id != id);
                self.done += 1;
                self.llm_time_total += llm_took;
                self.asset_time_total += took;
                self.recent.push_back(Instant::now());
                while self.recent.len() > RATE_WINDOW {
                    self.recent.pop_front();
                }
                self.push_log(LogRow::Done {
                    at: timestamp(),
                    name,
                    took,
                    description,
                });
            }
            Event::AssetFailed { id, name, error } => {
                self.in_flight.retain(|entry| entry.id != id);
                self.failed += 1;
                self.push_log(LogRow::Failed {
                    at: timestamp(),
                    name,
                    error,
                });
            }
            Event::RunFinished {
                done,
                failed,
                elapsed,
            } => {
                self.done = done;
                self.failed = failed;
                self.run_elapsed = Some(elapsed);
                self.in_flight.clear();
                self.run_state = RunState::Finished;
            }
            Event::Fatal { error } => {
                self.run_elapsed = Some(self.elapsed(Instant::now()));
                self.in_flight.clear();
                self.run_state = RunState::Error(error);
            }
            Event::ConnectionTest { id, immich, llm } if id == self.connection_test_id => {
                self.settings.testing = false;
                self.settings.test_result = Some((immich, llm));
            }
            Event::ConnectionTest { .. } => {}
        }
    }

    pub fn on_key(&mut self, key: Key) -> Option<Action> {
        if matches!(key, Key::CtrlC) {
            self.should_quit = true;
            return Some(Action::Quit);
        }
        match self.screen {
            Screen::Run => self.on_run_key(key),
            Screen::Settings => self.on_settings_key(key),
        }
    }

    /// Commits a candidate after persistence and engine replacement both succeed.
    pub fn config_save_succeeded(&mut self, config: Config) {
        self.invalidate_connection_test();
        self.config = config;
        self.settings = SettingsForm::from_config(&self.config);
        self.reset_to_idle();
        self.screen = Screen::Run;
        self.footer_message = Some("settings saved".into());
    }

    /// Keeps the candidate form and current runtime state after a save failure.
    pub fn config_save_failed(&mut self, message: impl Into<String>) {
        self.settings.message = Some(message.into());
    }

    /// Share of the queue finished so far, 0.0 to 1.0.
    pub fn progress_ratio(&self) -> f64 {
        if self.queued == 0 {
            return 0.0;
        }
        ((self.done + self.failed) as f64 / self.queued as f64).min(1.0)
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        match (self.run_elapsed, self.run_started) {
            (Some(elapsed), _) => elapsed,
            (None, Some(started)) => now.saturating_duration_since(started),
            (None, None) => Duration::ZERO,
        }
    }

    /// Completions per minute over the last `RATE_WINDOW` results.
    pub fn rate_per_min(&self, _now: Instant) -> Option<f64> {
        if self.recent.len() < 2 {
            return None;
        }
        let first = self.recent.front()?;
        let last = self.recent.back()?;
        let span = last.saturating_duration_since(*first).as_secs_f64();
        if span <= 0.0 {
            return None;
        }
        Some((self.recent.len() - 1) as f64 / span * 60.0)
    }

    pub fn eta(&self, now: Instant) -> Option<Duration> {
        let rate = self.rate_per_min(now)?;
        let remaining = self.queued.saturating_sub(self.done + self.failed) as f64;
        Some(Duration::from_secs_f64((remaining / rate * 60.0).round()))
    }

    pub fn avg_llm(&self) -> Option<Duration> {
        (self.done > 0).then(|| self.llm_time_total / self.done as u32)
    }

    pub fn avg_total(&self) -> Option<Duration> {
        (self.done > 0).then(|| self.asset_time_total / self.done as u32)
    }

    pub fn state_label(&self) -> &'static str {
        match self.run_state {
            RunState::Idle => "IDLE",
            RunState::Running => "RUNNING",
            RunState::Paused => "PAUSED",
            RunState::Finished => "FINISHED",
            RunState::Error(_) => "ERROR",
        }
    }

    pub fn immich_host(&self) -> String {
        host_of(&self.config.immich.url)
    }

    pub fn llm_host(&self) -> String {
        host_of(&self.config.llm.base_url)
    }

    fn on_run_key(&mut self, key: Key) -> Option<Action> {
        if self.log_expanded {
            if matches!(key, Key::Esc | Key::Enter) {
                self.log_expanded = false;
            }
            return None;
        }

        match key {
            Key::Char('q') => {
                self.should_quit = true;
                Some(Action::Quit)
            }
            Key::Char('s') if self.can_start() => {
                self.reset_run();
                self.run_state = RunState::Running;
                Some(Action::Send(Command::Start))
            }
            Key::Char('p') if self.run_state == RunState::Running => {
                self.run_state = RunState::Paused;
                Some(Action::Send(Command::Pause))
            }
            Key::Char('p') if self.run_state == RunState::Paused => {
                self.run_state = RunState::Running;
                Some(Action::Send(Command::Resume))
            }
            Key::Char('c') => {
                self.invalidate_connection_test();
                self.settings = SettingsForm::from_config(&self.config);
                self.footer_message = None;
                self.screen = Screen::Settings;
                None
            }
            Key::Up => {
                self.log_selected = self.log_selected.saturating_sub(1);
                None
            }
            Key::Down => {
                if self.log_selected + 1 < self.log.len() {
                    self.log_selected += 1;
                }
                None
            }
            Key::Enter => {
                if !self.log.is_empty() {
                    self.log_expanded = true;
                }
                None
            }
            _ => None,
        }
    }

    fn on_settings_key(&mut self, key: Key) -> Option<Action> {
        match key {
            Key::Esc => {
                self.invalidate_connection_test();
                self.screen = Screen::Run;
                None
            }
            Key::Tab => {
                self.settings.focus_next();
                None
            }
            Key::BackTab => {
                self.settings.focus_prev();
                None
            }
            Key::Enter if self.settings.is_last_focused() => self.save_settings(),
            Key::Enter => {
                self.settings.focus_next();
                None
            }
            Key::Backspace => {
                self.settings.backspace();
                None
            }
            Key::Char(c) if !c.is_control() => {
                self.settings.insert(c);
                None
            }
            Key::CtrlR => {
                self.settings.toggle_secrets();
                None
            }
            Key::CtrlT => match self.settings.to_config(&self.config) {
                Ok(config) => {
                    let id = self.next_connection_test_id();
                    self.settings.testing = true;
                    self.settings.test_result = None;
                    self.settings.message = None;
                    Some(Action::TestConnections { id, config })
                }
                Err(message) => {
                    self.settings.message = Some(message);
                    None
                }
            },
            Key::CtrlS => self.save_settings(),
            _ => None,
        }
    }

    fn save_settings(&mut self) -> Option<Action> {
        if self.run_state == RunState::Running {
            self.settings.message = Some("pause the run before saving".into());
            return None;
        }

        match self.settings.to_config(&self.config) {
            Ok(config) => Some(Action::SaveConfig(config)),
            Err(message) => {
                self.settings.message = Some(message);
                None
            }
        }
    }

    fn next_connection_test_id(&mut self) -> u64 {
        self.connection_test_id = self.connection_test_id.wrapping_add(1);
        self.connection_test_id
    }

    fn invalidate_connection_test(&mut self) {
        self.next_connection_test_id();
        self.settings.testing = false;
    }

    fn can_start(&self) -> bool {
        matches!(
            self.run_state,
            RunState::Idle | RunState::Finished | RunState::Error(_)
        )
    }

    fn reset_run(&mut self) {
        self.scanned = 0;
        self.queued = 0;
        self.done = 0;
        self.failed = 0;
        self.in_flight.clear();
        self.recent.clear();
        self.llm_time_total = Duration::ZERO;
        self.asset_time_total = Duration::ZERO;
        self.run_started = Some(Instant::now());
        self.run_elapsed = None;
        self.footer_message = None;
    }

    fn reset_to_idle(&mut self) {
        self.run_state = RunState::Idle;
        self.scanned = 0;
        self.queued = 0;
        self.done = 0;
        self.failed = 0;
        self.in_flight.clear();
        self.recent.clear();
        self.llm_time_total = Duration::ZERO;
        self.asset_time_total = Duration::ZERO;
        self.run_started = None;
        self.run_elapsed = None;
    }

    fn push_log(&mut self, row: LogRow) {
        self.log.push_front(row);
        if self.log.len() > LOG_CAP {
            self.log.pop_back();
        }
        if self.log_selected > 0 && self.log_selected + 1 < self.log.len() {
            self.log_selected += 1;
        }
    }
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            let host = parsed.host_str()?.to_string();
            Some(match parsed.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .unwrap_or_else(|| url.to_string())
}

fn timestamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::events::{Action, Command, Event, Key, Stage};

    fn config() -> Config {
        let mut c = Config::default();
        c.immich.url = "https://photos.home.lan".into();
        c.immich.api_key = "k".into();
        c.llm.model = "gemma".into();
        c
    }

    fn app() -> App {
        App::new(config(), false)
    }

    fn done(name: &str) -> Event {
        Event::AssetDone {
            id: name.to_string(),
            name: name.to_string(),
            description: format!("desc {name}"),
            took: Duration::from_secs(4),
            llm_took: Duration::from_secs(3),
        }
    }

    #[test]
    fn first_run_opens_settings() {
        assert_eq!(App::new(config(), true).screen, Screen::Settings);
        assert_eq!(app().screen, Screen::Run);
    }

    #[test]
    fn counters_follow_events() {
        let mut a = app();
        a.on_event(Event::PageLoaded {
            scanned: 10,
            queued: 4,
        });
        a.on_event(Event::AssetStarted {
            id: "1".into(),
            name: "1".into(),
        });
        assert_eq!(a.in_flight.len(), 1);
        a.on_event(Event::AssetStage {
            id: "1".into(),
            stage: Stage::Writing,
        });
        assert_eq!(a.in_flight[0].stage, Stage::Writing);
        a.on_event(done("1"));
        a.on_event(Event::AssetStarted {
            id: "2".into(),
            name: "2".into(),
        });
        a.on_event(Event::AssetFailed {
            id: "2".into(),
            name: "2".into(),
            error: "boom".into(),
        });
        assert_eq!((a.scanned, a.queued, a.done, a.failed), (10, 4, 1, 1));
        assert!(a.in_flight.is_empty());
        assert_eq!(a.log.len(), 2);
        assert!(matches!(a.log[0], LogRow::Failed { .. }), "newest first");
        assert_eq!(a.avg_llm(), Some(Duration::from_secs(3)));
        assert_eq!(a.avg_total(), Some(Duration::from_secs(4)));
        assert!((a.progress_ratio() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn discovery_done_sets_final_queue_size() {
        let mut a = app();
        a.on_event(Event::PageLoaded {
            scanned: 10,
            queued: 4,
        });
        a.on_event(Event::DiscoveryDone { total_queued: 4 });
        assert_eq!(a.queued, 4);
    }

    #[test]
    fn log_is_capped() {
        let mut a = app();
        for i in 0..(LOG_CAP + 25) {
            a.on_event(done(&i.to_string()));
        }
        assert_eq!(a.log.len(), LOG_CAP);
        assert!(
            matches!(&a.log[0], LogRow::Done { name, .. } if name == &(LOG_CAP + 24).to_string())
        );
    }

    #[test]
    fn rate_and_eta_use_recent_completions() {
        let mut a = app();
        let t0 = Instant::now();
        a.queued = 100;
        a.done = 20;
        for i in 0..20u64 {
            a.recent.push_back(t0 + Duration::from_secs(i * 3));
        }
        let now = t0 + Duration::from_secs(57);
        assert!((a.rate_per_min(now).unwrap() - 20.0).abs() < 1e-6);
        assert_eq!(a.eta(now), Some(Duration::from_secs(240)));
        assert_eq!(app().rate_per_min(now), None);
    }

    #[test]
    fn start_and_pause_keys() {
        let mut a = app();
        assert_eq!(
            a.on_key(Key::Char('p')),
            None,
            "pause does nothing when idle"
        );
        assert_eq!(a.on_key(Key::Char('s')), Some(Action::Send(Command::Start)));
        assert_eq!(a.run_state, RunState::Running);
        assert_eq!(
            a.on_key(Key::Char('s')),
            None,
            "start does nothing while running"
        );
        assert_eq!(a.on_key(Key::Char('p')), Some(Action::Send(Command::Pause)));
        assert_eq!(a.run_state, RunState::Paused);
        assert_eq!(
            a.on_key(Key::Char('p')),
            Some(Action::Send(Command::Resume))
        );
        assert_eq!(a.run_state, RunState::Running);
    }

    #[test]
    fn start_resets_counters_but_keeps_log() {
        let mut a = app();
        a.on_event(Event::PageLoaded {
            scanned: 5,
            queued: 5,
        });
        a.on_event(done("1"));
        a.on_event(Event::RunFinished {
            done: 1,
            failed: 0,
            elapsed: Duration::from_secs(9),
        });
        assert_eq!(a.run_state, RunState::Finished);
        a.on_key(Key::Char('s'));
        assert_eq!((a.scanned, a.queued, a.done), (0, 0, 0));
        assert_eq!(a.log.len(), 1);
    }

    #[test]
    fn fatal_sets_error_state() {
        let mut a = app();
        a.on_key(Key::Char('s'));
        a.on_event(Event::Fatal {
            error: "HTTP 401".into(),
        });
        assert_eq!(a.run_state, RunState::Error("HTTP 401".into()));
        assert_eq!(a.state_label(), "ERROR");
        assert_eq!(
            a.on_key(Key::Char('s')),
            Some(Action::Send(Command::Start)),
            "start again after an error"
        );
    }

    #[test]
    fn log_scroll_and_expand() {
        let mut a = app();
        a.on_event(done("1"));
        a.on_event(done("2"));
        assert_eq!(a.on_key(Key::Down), None);
        assert_eq!(a.log_selected, 1);
        a.on_key(Key::Down);
        assert_eq!(a.log_selected, 1, "stops at the last row");
        a.on_event(done("3"));
        assert_eq!(
            a.log_selected, 2,
            "highlight follows its row when a new row arrives"
        );
        a.on_key(Key::Enter);
        assert!(a.log_expanded);
        a.on_key(Key::Esc);
        assert!(!a.log_expanded);
        a.on_key(Key::Up);
        a.on_key(Key::Up);
        a.on_key(Key::Up);
        assert_eq!(a.log_selected, 0);
    }

    #[test]
    fn quit_keys() {
        let mut a = app();
        assert_eq!(a.on_key(Key::Char('q')), Some(Action::Quit));
        assert!(a.should_quit);
        let mut a = app();
        a.on_key(Key::Char('c'));
        assert_eq!(
            a.on_key(Key::CtrlC),
            Some(Action::Quit),
            "ctrl-c quits from settings too"
        );
    }

    #[test]
    fn settings_screen_flow() {
        let mut a = app();
        assert_eq!(a.on_key(Key::Char('c')), None);
        assert_eq!(a.screen, Screen::Settings);
        a.on_key(Key::Tab);
        a.on_key(Key::Tab);
        a.on_key(Key::Tab);
        a.on_key(Key::Tab);
        a.on_key(Key::Char('!'));
        assert_eq!(
            a.settings.fields[crate::settings::LLM_MODEL].value,
            "gemma!"
        );
        a.on_key(Key::Backspace);
        assert_eq!(a.on_key(Key::Esc), None);
        assert_eq!(a.screen, Screen::Run);
        assert_eq!(a.config.llm.model, "gemma", "esc discards edits");
    }

    #[test]
    fn save_returns_candidate_without_committing_it() {
        let mut a = app();
        a.on_key(Key::Char('c'));
        a.settings.fields[crate::settings::WORKERS].value = "3".into();
        let action = a.on_key(Key::CtrlS);
        match &action {
            Some(Action::SaveConfig(cfg)) => assert_eq!(cfg.run.workers, 3),
            other => panic!("{other:?}"),
        }
        assert_eq!(a.screen, Screen::Settings);
        assert_eq!(a.config.run.workers, 1);
        assert_eq!(a.footer_message, None);
    }

    #[test]
    fn saving_from_paused_resets_run_telemetry_but_keeps_log() {
        let mut a = app();
        a.on_key(Key::Char('s'));
        a.on_event(Event::PageLoaded {
            scanned: 12,
            queued: 7,
        });
        a.on_event(done("1"));
        a.on_event(Event::AssetFailed {
            id: "2".into(),
            name: "2".into(),
            error: "boom".into(),
        });
        a.on_event(Event::AssetStarted {
            id: "3".into(),
            name: "3".into(),
        });
        assert_eq!(a.on_key(Key::Char('p')), Some(Action::Send(Command::Pause)));
        assert_eq!(a.run_state, RunState::Paused);
        assert_eq!(a.log.len(), 2);
        assert!(a.run_started.is_some());
        assert_eq!(a.avg_llm(), Some(Duration::from_secs(3)));
        assert_eq!(a.avg_total(), Some(Duration::from_secs(4)));
        assert!(a.rate_per_min(Instant::now()).is_none());

        a.on_key(Key::Char('c'));
        a.settings.fields[crate::settings::WORKERS].value = "3".into();
        let action = a.on_key(Key::CtrlS);
        match &action {
            Some(Action::SaveConfig(cfg)) => assert_eq!(cfg.run.workers, 3),
            other => panic!("{other:?}"),
        }

        assert_eq!(a.run_state, RunState::Paused);
        assert_eq!(a.screen, Screen::Settings);
        assert_eq!((a.scanned, a.queued, a.done, a.failed), (12, 7, 1, 1));
        assert_eq!(a.in_flight.len(), 1);
        assert_eq!(a.log.len(), 2);
        assert_eq!(a.recent.len(), 1);
        assert_eq!(a.llm_time_total, Duration::from_secs(3));
        assert_eq!(a.asset_time_total, Duration::from_secs(4));
        assert!(a.run_started.is_some());
        assert_eq!(a.run_elapsed, None);
        assert_eq!(a.progress_ratio(), 2.0 / 7.0);
        assert_eq!(a.avg_llm(), Some(Duration::from_secs(3)));
        assert_eq!(a.avg_total(), Some(Duration::from_secs(4)));
        assert_eq!(a.rate_per_min(Instant::now()), None);

        let candidate = match action {
            Some(Action::SaveConfig(cfg)) => cfg,
            other => panic!("{other:?}"),
        };
        a.config_save_succeeded(candidate);

        assert_eq!(a.config.run.workers, 3);
        assert_eq!(a.run_state, RunState::Idle);
        assert_eq!(a.screen, Screen::Run);
        assert_eq!((a.scanned, a.queued, a.done, a.failed), (0, 0, 0, 0));
        assert!(a.in_flight.is_empty());
        assert_eq!(a.log.len(), 2);
        assert!(a.recent.is_empty());
        assert_eq!(a.llm_time_total, Duration::ZERO);
        assert_eq!(a.asset_time_total, Duration::ZERO);
        assert_eq!(a.run_started, None);
        assert_eq!(a.run_elapsed, None);
        assert_eq!(a.footer_message.as_deref(), Some("settings saved"));
    }

    #[test]
    fn failed_config_save_keeps_candidate_and_runtime_state() {
        let mut a = app();
        a.on_key(Key::Char('s'));
        a.on_event(Event::PageLoaded {
            scanned: 12,
            queued: 7,
        });
        assert_eq!(a.on_key(Key::Char('p')), Some(Action::Send(Command::Pause)));
        let committed = a.config.clone();

        a.on_key(Key::Char('c'));
        a.settings.fields[crate::settings::WORKERS].value = "3".into();
        assert!(matches!(a.on_key(Key::CtrlS), Some(Action::SaveConfig(_))));
        a.config_save_failed("save failed");

        assert_eq!(a.config, committed);
        assert_eq!(a.run_state, RunState::Paused);
        assert_eq!(a.screen, Screen::Settings);
        assert_eq!((a.scanned, a.queued), (12, 7));
        assert_eq!(
            a.settings.fields[crate::settings::WORKERS].value,
            "3",
            "candidate edits remain available for correction or retry"
        );
        assert_eq!(a.settings.message.as_deref(), Some("save failed"));
    }

    #[test]
    fn save_is_refused_while_running() {
        let mut a = app();
        a.on_key(Key::Char('s'));
        a.on_key(Key::Char('c'));
        assert_eq!(a.on_key(Key::CtrlS), None);
        assert_eq!(a.screen, Screen::Settings);
        assert!(a.settings.message.as_deref().unwrap().contains("pause"));
    }

    #[test]
    fn save_shows_validation_errors() {
        let mut a = app();
        a.on_key(Key::Char('c'));
        a.settings.fields[crate::settings::WORKERS].value = "0".into();
        assert_eq!(a.on_key(Key::CtrlS), None);
        assert!(a.settings.message.as_deref().unwrap().contains("workers"));
    }

    #[test]
    fn enter_on_last_field_saves() {
        let mut a = app();
        a.on_key(Key::Char('c'));
        a.settings.focused = crate::settings::MAX_TOKENS;
        assert!(matches!(a.on_key(Key::Enter), Some(Action::SaveConfig(_))));
    }

    #[test]
    fn connection_test_round_trip() {
        let mut a = app();
        a.on_key(Key::Char('c'));
        let id = match a.on_key(Key::CtrlT) {
            Some(Action::TestConnections { id, .. }) => id,
            other => panic!("{other:?}"),
        };
        assert!(a.settings.testing);
        a.on_event(Event::ConnectionTest {
            id,
            immich: Ok("v3.1.0".into()),
            llm: Err("HTTP 401".into()),
        });
        assert!(!a.settings.testing);
        let (i, l) = a.settings.test_result.clone().unwrap();
        assert_eq!(i, Ok("v3.1.0".into()));
        assert_eq!(l, Err("HTTP 401".into()));
    }

    #[test]
    fn stale_connection_test_result_cannot_overwrite_newer_result() {
        let mut a = app();
        a.on_key(Key::Char('c'));
        let first = match a.on_key(Key::CtrlT) {
            Some(Action::TestConnections { id, .. }) => id,
            other => panic!("{other:?}"),
        };
        let second = match a.on_key(Key::CtrlT) {
            Some(Action::TestConnections { id, .. }) => id,
            other => panic!("{other:?}"),
        };

        a.on_event(Event::ConnectionTest {
            id: second,
            immich: Ok("new immich".into()),
            llm: Ok("new llm".into()),
        });
        a.on_event(Event::ConnectionTest {
            id: first,
            immich: Err("stale immich".into()),
            llm: Err("stale llm".into()),
        });

        assert_eq!(
            a.settings.test_result,
            Some((Ok("new immich".into()), Ok("new llm".into())))
        );
        assert!(!a.settings.testing);
    }

    #[test]
    fn hosts_for_the_header() {
        let a = app();
        assert_eq!(a.immich_host(), "photos.home.lan");
        assert_eq!(a.llm_host(), "localhost:1234");
    }
}
