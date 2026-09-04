//! Entry point: CLI args, logging, terminal lifecycle, and the event loop.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use immich_alt_text::app::{App, RunState};
use immich_alt_text::config::{self, Config, ConfigError};
use immich_alt_text::engine::{self, EngineHandle, PreparedEngine};
use immich_alt_text::events::{Action, Event, Key};
use immich_alt_text::immich::ImmichClient;
use immich_alt_text::llm::LlmClient;
use immich_alt_text::theme::Theme;
use immich_alt_text::ui;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const CONNECTION_TEST_TIMEOUT: Duration = Duration::from_secs(10);
const ENGINE_EVENT_CAPACITY: usize = 1024;

/// Describe Immich photos with a vision LLM from a terminal UI.
#[derive(Parser)]
#[command(name = "immich-alt-text", version)]
struct Cli {
    /// Path to the config file. Default: ~/.config/immich-alt-text/config.toml
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _log_guard = init_logging()?;
    let path = cli.config.unwrap_or_else(config::default_path);
    let startup = load_startup_config(&path)?;
    tracing::info!(config = %path.display(), needs_setup = startup.needs_setup, "starting");

    install_panic_hook();
    let terminal = ratatui::init();
    let result = run(
        terminal,
        startup.config,
        startup.needs_setup,
        startup.message,
        path,
    )
    .await;
    ratatui::restore();
    result
}

struct StartupConfig {
    config: Config,
    needs_setup: bool,
    message: Option<String>,
}

fn load_startup_config(path: &Path) -> Result<StartupConfig, ConfigError> {
    match config::load(path) {
        Ok(config) => {
            let config = config.unwrap_or_default();
            let needs_setup = config.validate().is_err();
            Ok(StartupConfig {
                config,
                needs_setup,
                message: None,
            })
        }
        Err(ConfigError::Parse { .. }) => {
            tracing::warn!(config = %path.display(), "config parse failed");
            Ok(StartupConfig {
                config: Config::default(),
                needs_setup: true,
                message: Some(
                    "config file could not be parsed; review the settings and save".into(),
                ),
            })
        }
        Err(error) => Err(error),
    }
}

fn init_logging() -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    let dir = config::state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let file = tracing_appender::rolling::daily(&dir, "debug.log");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Ok(guard)
}

/// Restores the terminal before the panic message prints, so the shell stays usable.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        default_hook(info);
    }));
}

/// Reads terminal keys on a plain thread and forwards mapped `Key`s.
fn spawn_key_reader() -> mpsc::UnboundedReceiver<Key> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || loop {
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => match event::read() {
                Ok(TermEvent::Key(k)) if k.kind == KeyEventKind::Press => {
                    if let Some(key) = map_key(k.code, k.modifiers) {
                        if tx.send(key).is_err() {
                            return;
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            },
            Ok(false) => {
                if tx.is_closed() {
                    return;
                }
            }
            Err(_) => return,
        }
    });
    rx
}

fn map_key(code: KeyCode, mods: KeyModifiers) -> Option<Key> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    Some(match (code, ctrl) {
        (KeyCode::Char('c'), true) => Key::CtrlC,
        (KeyCode::Char('s'), true) => Key::CtrlS,
        (KeyCode::Char('t'), true) => Key::CtrlT,
        (KeyCode::Char('r'), true) => Key::CtrlR,
        (KeyCode::Char(c), false) => Key::Char(c),
        (KeyCode::Up, _) => Key::Up,
        (KeyCode::Down, _) => Key::Down,
        (KeyCode::Enter, _) => Key::Enter,
        (KeyCode::Esc, _) => Key::Esc,
        (KeyCode::Tab, _) => Key::Tab,
        (KeyCode::BackTab, _) => Key::BackTab,
        (KeyCode::Backspace, _) => Key::Backspace,
        _ => return None,
    })
}

enum KeyRead {
    Action(Option<Action>),
    Closed,
}

struct PreparedRuntime {
    prepared: PreparedEngine,
    events: mpsc::Receiver<Event>,
}

impl PreparedRuntime {
    fn start(self) -> EngineRuntime {
        EngineRuntime {
            handle: self.prepared.start(),
            events: self.events,
        }
    }
}

struct EngineRuntime {
    handle: EngineHandle,
    events: mpsc::Receiver<Event>,
}

fn prepare_runtime(config: Config) -> Result<PreparedRuntime, engine::EngineError> {
    let (event_tx, events) = mpsc::channel(ENGINE_EVENT_CAPACITY);
    let prepared = engine::prepare(config, event_tx)?;
    Ok(PreparedRuntime { prepared, events })
}

fn spawn_runtime(config: Config) -> Result<EngineRuntime, engine::EngineError> {
    Ok(prepare_runtime(config)?.start())
}

async fn receive_engine_event(active: &mut Option<EngineRuntime>) -> Event {
    let event = match active {
        Some(runtime) => runtime.events.recv().await,
        None => None,
    };
    match event {
        Some(event) => event,
        None => std::future::pending().await,
    }
}

fn handle_key_read(app: &mut App, key: Option<Key>) -> KeyRead {
    match key {
        Some(key) => KeyRead::Action(app.on_key(key)),
        None => KeyRead::Closed,
    }
}

async fn run(
    mut terminal: DefaultTerminal,
    cfg: Config,
    needs_setup: bool,
    setup_message: Option<String>,
    path: PathBuf,
) -> anyhow::Result<()> {
    let (connection_tx, mut connection_rx) = mpsc::channel::<Event>(16);
    let mut keys = spawn_key_reader();
    let mut theme = Theme::from_name(cfg.ui.theme);
    let mut app = App::new(cfg.clone(), needs_setup);
    app.settings.message = setup_message;
    let mut engine: Option<EngineRuntime> = if needs_setup {
        None
    } else {
        Some(spawn_runtime(cfg)?)
    };
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut connection_test: Option<tokio::task::JoinHandle<()>> = None;

    let result = async {
        loop {
            terminal.draw(|f| ui::render(f, &app, Instant::now(), &theme))?;
            let key_read = tokio::select! {
                key = keys.recv() => handle_key_read(&mut app, key),
                ev = receive_engine_event(&mut engine) => {
                    app.on_event(ev);
                    KeyRead::Action(None)
                }
                Some(ev) = connection_rx.recv() => {
                    app.on_event(ev);
                    KeyRead::Action(None)
                }
                _ = tick.tick() => KeyRead::Action(None),
            };
            let action = match key_read {
                KeyRead::Action(action) => action,
                KeyRead::Closed => break,
            };
            match action {
                None => {}
                Some(Action::Send(cmd)) => match &engine {
                    Some(runtime) => runtime.handle.send(cmd).await,
                    None => {
                        app.run_state = RunState::Idle;
                        app.footer_message =
                            Some("open settings with c and save a config first".into());
                    }
                },
                Some(Action::TestConnections { id, config }) => {
                    if let Some(previous) = connection_test.take() {
                        previous.abort();
                    }
                    connection_test = Some(tokio::spawn(test_connections(
                        id,
                        config,
                        connection_tx.clone(),
                    )));
                }
                Some(Action::SaveConfig(new_cfg)) => {
                    apply_saved_config(&mut app, &mut engine, &mut theme, &path, new_cfg).await;
                }
                Some(Action::Quit) => break,
            }
            if app.should_quit {
                break;
            }
        }
        Ok(())
    }
    .await;

    if let Some(connection_test) = connection_test {
        connection_test.abort();
    }
    shutdown_engine_before_return(result, &mut engine).await
}

async fn apply_saved_config(
    app: &mut App,
    active: &mut Option<EngineRuntime>,
    theme: &mut Theme,
    path: &Path,
    candidate: Config,
) {
    apply_saved_config_with(app, active, theme, path, candidate, |config| {
        prepare_runtime(config).map_err(|_| ())
    })
    .await;
}

async fn apply_saved_config_with<F>(
    app: &mut App,
    active: &mut Option<EngineRuntime>,
    theme: &mut Theme,
    path: &Path,
    candidate: Config,
    prepare: F,
) where
    F: FnOnce(Config) -> Result<PreparedRuntime, ()>,
{
    let replacement = match prepare(candidate.clone()) {
        Ok(replacement) => replacement,
        Err(_) => {
            tracing::error!("engine preparation failed");
            app.config_save_failed("could not apply settings");
            return;
        }
    };

    if config::save(path, &candidate).is_err() {
        tracing::error!(config = %path.display(), "config save failed");
        app.config_save_failed("could not save settings");
        return;
    }

    tracing::info!(config = %path.display(), "saved");
    if let Some(old) = active.take() {
        old.handle.shutdown_for_replacement().await;
    }
    *active = Some(replacement.start());
    *theme = Theme::from_name(candidate.ui.theme);
    app.config_save_succeeded(candidate);
}

async fn shutdown_engine_before_return(
    result: anyhow::Result<()>,
    active: &mut Option<EngineRuntime>,
) -> anyhow::Result<()> {
    if let Some(runtime) = active.take() {
        runtime.handle.shutdown(SHUTDOWN_GRACE).await;
    }
    result
}

/// Checks both servers with the candidate config and reports back as an `Event`.
async fn test_connections(id: u64, cfg: Config, tx: mpsc::Sender<Event>) {
    let immich = async {
        let client = ImmichClient::new(
            &cfg.immich.url,
            &cfg.immich.api_key,
            Duration::from_secs(cfg.immich.timeout_secs),
        )
        .map_err(|e| e.to_string())?;
        client.version().await.map_err(|e| e.to_string())
    };
    let llm = async {
        let client = LlmClient::new(
            &cfg.llm.base_url,
            &cfg.llm.api_key,
            &cfg.llm.model,
            cfg.llm.max_tokens,
            Duration::from_secs(cfg.llm.timeout_secs),
        )
        .map_err(|e| e.to_string())?;
        client.ping().await.map_err(|e| e.to_string())
    };
    let immich = async {
        tokio::time::timeout(CONNECTION_TEST_TIMEOUT, immich)
            .await
            .unwrap_or_else(|_| Err("timed out".to_string()))
    };
    let llm = async {
        tokio::time::timeout(CONNECTION_TEST_TIMEOUT, llm)
            .await
            .unwrap_or_else(|_| Err("timed out".to_string()))
    };
    let (immich, llm) = tokio::join!(immich, llm);
    let _ = tx.send(Event::ConnectionTest { id, immich, llm }).await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use immich_alt_text::app::{App, RunState, Screen};
    use immich_alt_text::config::{
        Config, ImmichConfig, LlmConfig, RunConfig, ThemeName, UiConfig,
    };
    use immich_alt_text::engine;
    use immich_alt_text::events::{Action, Event, Key};
    use immich_alt_text::theme::Theme;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    use tokio::sync::mpsc;

    use super::{
        apply_saved_config, apply_saved_config_with, handle_key_read, load_startup_config, map_key,
        receive_engine_event, shutdown_engine_before_return, EngineRuntime, KeyRead,
        PreparedRuntime,
    };

    fn config() -> Config {
        Config {
            immich: ImmichConfig {
                url: "http://127.0.0.1:3001".into(),
                api_key: "immich-secret".into(),
                timeout_secs: 5,
            },
            llm: LlmConfig {
                base_url: "http://127.0.0.1:3002/v1".into(),
                api_key: "llm-secret".into(),
                model: "vision".into(),
                max_tokens: 100,
                timeout_secs: 5,
                prompt: "describe".into(),
            },
            run: RunConfig {
                workers: 1,
                retries: 1,
                page_size: 10,
            },
            ui: UiConfig::default(),
        }
    }

    #[test]
    fn maps_plain_terminal_keys_to_app_keys() {
        let cases = [
            (KeyCode::Char('s'), Key::Char('s')),
            (KeyCode::Up, Key::Up),
            (KeyCode::Down, Key::Down),
            (KeyCode::Enter, Key::Enter),
            (KeyCode::Esc, Key::Esc),
            (KeyCode::Tab, Key::Tab),
            (KeyCode::BackTab, Key::BackTab),
            (KeyCode::Backspace, Key::Backspace),
        ];

        for (code, expected) in cases {
            assert_eq!(map_key(code, KeyModifiers::NONE), Some(expected));
        }
    }

    #[test]
    fn maps_supported_control_shortcuts() {
        let cases = [
            ('c', Key::CtrlC),
            ('s', Key::CtrlS),
            ('t', Key::CtrlT),
            ('r', Key::CtrlR),
        ];

        for (character, expected) in cases {
            assert_eq!(
                map_key(KeyCode::Char(character), KeyModifiers::CONTROL),
                Some(expected)
            );
        }
    }

    #[test]
    fn ignores_unhandled_keys() {
        assert_eq!(map_key(KeyCode::Char('x'), KeyModifiers::CONTROL), None);
        assert_eq!(map_key(KeyCode::F(1), KeyModifiers::NONE), None);
    }

    #[test]
    fn closed_key_channel_stops_the_loop() {
        let mut app = App::new(config(), false);

        assert!(matches!(handle_key_read(&mut app, None), KeyRead::Closed));
        assert!(matches!(
            handle_key_read(&mut app, Some(Key::Char('s'))),
            KeyRead::Action(Some(Action::Send(_)))
        ));
    }

    #[test]
    fn malformed_toml_enters_settings_without_exposing_file_contents() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("config.toml");
        let secret = "should-never-appear-in-the-ui";
        std::fs::write(&path, format!("immich = [\"{secret}\"")).expect("write malformed config");

        let startup = load_startup_config(&path).expect("parse failure should be recoverable");

        assert!(startup.needs_setup);
        assert_eq!(startup.config, Config::default());
        let message = startup
            .message
            .expect("settings should explain the problem");
        assert!(message.contains("could not be parsed"));
        assert!(!message.contains(secret));
    }

    #[tokio::test]
    async fn replacement_failure_keeps_old_config_runtime_candidate_and_engine() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("config.toml");
        let committed = config();
        let mut candidate = committed.clone();
        candidate.run.workers = 3;
        candidate.ui.theme = ThemeName::Mono;
        let (tx, events) = mpsc::channel::<Event>(8);
        let old_engine = engine::spawn(committed.clone(), tx.clone()).expect("valid old engine");
        let mut active = Some(EngineRuntime {
            handle: old_engine,
            events,
        });
        let mut app = App::new(committed.clone(), false);
        app.run_state = RunState::Paused;
        app.scanned = 9;
        app.on_key(Key::Char('c'));
        app.settings.fields[immich_alt_text::settings::WORKERS].value = "3".into();
        let mut theme = Theme::from_name(committed.ui.theme);

        apply_saved_config_with(
            &mut app,
            &mut active,
            &mut theme,
            &path,
            candidate,
            |_config| Err(()),
        )
        .await;

        assert_eq!(app.config, committed);
        assert_eq!(app.run_state, RunState::Paused);
        assert_eq!(app.scanned, 9);
        assert_eq!(app.screen, Screen::Settings);
        assert_eq!(theme.border, Theme::btop().border);
        assert!(active.is_some());
        assert!(!path.exists());
        assert!(app
            .settings
            .message
            .as_deref()
            .is_some_and(|message| message.contains("apply")));
        shutdown_engine_before_return(Ok(()), &mut active)
            .await
            .expect("cleanup succeeds");
    }

    #[tokio::test]
    async fn persistence_failure_keeps_old_config_runtime_candidate_and_engine() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let committed = config();
        let mut candidate = committed.clone();
        candidate.run.workers = 3;
        candidate.ui.theme = ThemeName::Mono;
        let (tx, events) = mpsc::channel::<Event>(8);
        let old_engine = engine::spawn(committed.clone(), tx.clone()).expect("valid old engine");
        let mut active = Some(EngineRuntime {
            handle: old_engine,
            events,
        });
        let mut app = App::new(committed.clone(), false);
        app.run_state = RunState::Paused;
        app.done = 4;
        app.on_key(Key::Char('c'));
        app.settings.fields[immich_alt_text::settings::WORKERS].value = "3".into();
        let mut theme = Theme::from_name(committed.ui.theme);

        apply_saved_config(&mut app, &mut active, &mut theme, dir.path(), candidate).await;

        assert_eq!(app.config, committed);
        assert_eq!(app.run_state, RunState::Paused);
        assert_eq!(app.screen, Screen::Settings);
        assert_eq!(theme.border, Theme::btop().border);
        assert_eq!(app.done, 4);
        assert!(active.is_some());
        assert_eq!(
            app.settings.fields[immich_alt_text::settings::WORKERS].value,
            "3"
        );
        assert!(app
            .settings
            .message
            .as_deref()
            .is_some_and(|message| message.contains("save")));
        shutdown_engine_before_return(Ok(()), &mut active)
            .await
            .expect("cleanup succeeds");
    }

    #[tokio::test]
    async fn queued_old_terminal_event_cannot_mutate_app_after_replacement() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("config.toml");
        let committed = config();
        let mut candidate = committed.clone();
        candidate.run.workers = 2;
        let (old_tx, old_events) = mpsc::channel(8);
        let old = engine::spawn(committed.clone(), old_tx.clone()).expect("valid old engine");
        let mut active = Some(EngineRuntime {
            handle: old,
            events: old_events,
        });
        old_tx
            .send(Event::Fatal {
                error: "old engine failed".into(),
            })
            .await
            .expect("old event is queued");

        let (new_tx, new_events) = mpsc::channel(8);
        let replacement = PreparedRuntime {
            prepared: engine::prepare(candidate.clone(), new_tx.clone())
                .expect("valid replacement"),
            events: new_events,
        };
        let mut app = App::new(committed, false);
        let mut theme = Theme::btop();
        apply_saved_config_with(
            &mut app,
            &mut active,
            &mut theme,
            &path,
            candidate,
            |_config| Ok(replacement),
        )
        .await;

        new_tx
            .send(Event::PageLoaded {
                scanned: 2,
                queued: 1,
            })
            .await
            .expect("new event is queued");
        let event = tokio::time::timeout(
            Duration::from_millis(100),
            receive_engine_event(&mut active),
        )
        .await
        .expect("new engine event was not received");
        app.on_event(event);

        assert_eq!(app.run_state, RunState::Idle);
        assert_eq!((app.scanned, app.queued), (2, 1));
        shutdown_engine_before_return(Ok(()), &mut active)
            .await
            .expect("cleanup succeeds");
    }

    #[tokio::test]
    async fn loop_error_still_shuts_down_the_active_engine() {
        let (tx, events) = mpsc::channel::<Event>(8);
        let event_observer = tx.clone();
        let mut active = Some(EngineRuntime {
            handle: engine::spawn(config(), tx).expect("valid engine"),
            events,
        });
        let result =
            shutdown_engine_before_return(Err(anyhow::anyhow!("draw failed")), &mut active).await;

        assert!(result.is_err());
        assert!(active.is_none());
        assert!(event_observer.is_closed());
    }
}
