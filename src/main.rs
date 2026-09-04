//! Entry point: CLI args, logging, terminal lifecycle, and the event loop.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use immich_alt_text::app::{App, RunState};
use immich_alt_text::config::{self, Config};
use immich_alt_text::engine::{self, EngineHandle};
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
    let cfg = config::load(&path)?.unwrap_or_default();
    // A missing or broken file both land on the settings screen.
    let needs_setup = cfg.validate().is_err();
    tracing::info!(config = %path.display(), needs_setup, "starting");

    install_panic_hook();
    let terminal = ratatui::init();
    let result = run(terminal, cfg, needs_setup, path).await;
    ratatui::restore();
    result
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
            Ok(true) => {
                if let Ok(TermEvent::Key(k)) = event::read() {
                    if k.kind == KeyEventKind::Press {
                        if let Some(key) = map_key(k.code, k.modifiers) {
                            if tx.send(key).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
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

async fn run(
    mut terminal: DefaultTerminal,
    cfg: Config,
    needs_setup: bool,
    path: PathBuf,
) -> anyhow::Result<()> {
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(1024);
    let mut keys = spawn_key_reader();
    let mut theme = Theme::from_name(cfg.ui.theme);
    let mut app = App::new(cfg.clone(), needs_setup);
    let mut engine: Option<EngineHandle> = if needs_setup {
        None
    } else {
        Some(engine::spawn(cfg, event_tx.clone())?)
    };
    let mut tick = tokio::time::interval(Duration::from_millis(250));

    loop {
        terminal.draw(|f| ui::render(f, &app, Instant::now(), &theme))?;
        let action = tokio::select! {
            Some(key) = keys.recv() => app.on_key(key),
            Some(ev) = event_rx.recv() => {
                app.on_event(ev);
                None
            }
            _ = tick.tick() => None,
        };
        match action {
            None => {}
            Some(Action::Send(cmd)) => match &engine {
                Some(e) => e.send(cmd).await,
                None => {
                    app.run_state = RunState::Idle;
                    app.footer_message =
                        Some("open settings with c and save a config first".into());
                }
            },
            Some(Action::TestConnections(candidate)) => {
                tokio::spawn(test_connections(candidate, event_tx.clone()));
            }
            Some(Action::SaveConfig(new_cfg)) => {
                if let Err(e) = config::save(&path, &new_cfg) {
                    tracing::error!(error = %e, "save failed");
                    app.footer_message = Some(format!("save failed: {e}"));
                    continue;
                }
                tracing::info!(config = %path.display(), "saved");
                if let Some(old) = engine.take() {
                    old.shutdown(SHUTDOWN_GRACE).await;
                }
                theme = Theme::from_name(new_cfg.ui.theme);
                engine = Some(engine::spawn(new_cfg, event_tx.clone())?);
            }
            Some(Action::Quit) => break,
        }
        if app.should_quit {
            break;
        }
    }

    if let Some(e) = engine {
        e.shutdown(SHUTDOWN_GRACE).await;
    }
    Ok(())
}

/// Checks both servers with the candidate config and reports back as an `Event`.
async fn test_connections(cfg: Config, tx: mpsc::Sender<Event>) {
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
    let _ = tx.send(Event::ConnectionTest { immich, llm }).await;
}

#[cfg(test)]
mod tests {
    use immich_alt_text::events::Key;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use super::map_key;

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
}
