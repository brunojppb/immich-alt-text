//! Messages shared by the engine, the app state, and main.

use std::time::Duration;

use crate::config::Config;

/// Which step of one asset a worker is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Fetching,
    CallingLlm,
    Writing,
}

impl Stage {
    /// Short lowercase label for the UI.
    pub fn label(self) -> &'static str {
        match self {
            Stage::Fetching => "fetching",
            Stage::CallingLlm => "calling llm",
            Stage::Writing => "writing",
        }
    }
}

/// Something the engine reports. The UI needs nothing else to draw.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Cumulative totals after one search page.
    PageLoaded {
        scanned: u64,
        queued: u64,
    },
    DiscoveryDone {
        total_queued: u64,
    },
    AssetStarted {
        id: String,
        name: String,
    },
    AssetStage {
        id: String,
        stage: Stage,
    },
    AssetDone {
        id: String,
        name: String,
        description: String,
        took: Duration,
        llm_took: Duration,
    },
    AssetFailed {
        id: String,
        name: String,
        error: String,
    },
    RunFinished {
        done: u64,
        failed: u64,
        elapsed: Duration,
    },
    /// The run stopped. Only a config change or restart helps.
    Fatal {
        error: String,
    },
    /// Result of a settings-screen connection test. `Ok` holds a short status text.
    ConnectionTest {
        immich: Result<String, String>,
        llm: Result<String, String>,
    },
}

/// Something the UI asks the engine to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Start,
    Pause,
    Resume,
    Quit,
}

/// A key press, already mapped from the terminal library so `app` stays I/O free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    CtrlC,
    CtrlS,
    CtrlT,
    CtrlR,
}

/// Side effect requested by `App::on_key`. `main` performs it.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Send(Command),
    /// Test the connections described by this candidate config.
    TestConnections(Config),
    SaveConfig(Config),
    Quit,
}
