# immich-alt-text Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A small Rust TUI that reads photos from an Immich server, asks a vision LLM for alt text, and writes the text back as the asset description, with live progress on screen.

**Architecture:** One tokio runtime. An `engine` task pages through Immich, runs a worker pool, and emits `Event`s over a channel. A pure `app` struct consumes `Event`s and `Key`s and returns `Action`s. `ui` draws `app` with Ratatui. `main` wires them and does all I/O that is not the engine's.

**Tech Stack:** Rust 1.85+, ratatui 0.30, tokio 1, reqwest 0.13 (rustls by default), serde, toml 1, wiremock 0.6 and insta 1 for tests.

**Spec:** `docs/design.md`. Read it before starting any task. The plan implements it and does not repeat every rationale.

## Global Constraints

- Edition 2021, `rust-version = "1.85"`. Toolchain on the dev machine is 1.97.
- Crate versions are pinned in Task 1's `Cargo.toml`. Do not add crates the plan does not name.
- Every implementation task runs on its own git branch off `main`. Branch name: `task-N-<slug>`. Never commit code on `main`.
- Commit messages use Conventional Commits: `feat:`, `fix:`, `test:`, `chore:`, `docs:`.
- `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must pass before every commit.
- No `unwrap()` outside tests. Use `?`, `anyhow::Context`, or an explicit `match`.
- API keys and request bodies never appear in logs.
- Out of scope, do not build: headless mode, album or date filters, a review step, prompt editing in the TUI, live worker count changes, videos, other LLM providers, image thumbnails in the terminal, a local database.
- Comments: JSDoc-style doc comments on public items say what, not how. Inline comments only for a non-obvious why.
- Prose in docs and commit messages: short sentences, active voice, plain words.

## File Structure

```
Cargo.toml
src/
  lib.rs            re-exports the modules so tests and examples can use them
  main.rs           CLI args, tracing setup, terminal lifecycle, event loop
  config.rs         Config structs, defaults, load/save/validate, XDG paths
  events.rs         Event, Command, Stage, Key, Action enums shared by all modules
  immich.rs         ImmichClient: version, list_images, preview_jpeg, set_description
  llm.rs            LlmClient: describe, ping
  engine.rs         EngineHandle, spawn, discovery, workers, retries
  app.rs            App state, on_event, on_key, rate and ETA math
  settings.rs       SettingsForm: fields, focus, edit, to_config
  theme.rs          Theme colors, btop and mono variants, graded bar
  ui/
    mod.rs          render(frame, app, now, theme), layout by terminal size
    run.rs          header, progress, counters, in-flight, log, popup, footer
    settings.rs     the settings form screen
tests/
  engine_test.rs    engine against wiremock Immich and LLM
  ui_snapshots.rs   insta snapshots at 120x40, 80x24, 40x10
examples/
  fake_servers.rs   demo servers for a manual run
```

Task order and dependencies: 1 → {2, 3, 5, 6} in parallel → 4 (needs 2, 3) → 7 (needs all).

---

### Task 1: Skeleton, config, shared enums

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `src/lib.rs`, `src/main.rs`, `src/config.rs`, `src/events.rs`
- Test: unit tests inside `src/config.rs`

**Interfaces:**
- Produces `config::Config` with nested `ImmichConfig`, `LlmConfig`, `RunConfig`, `UiConfig`, `ThemeName`, `DEFAULT_PROMPT`, `default_path()`, `state_dir()`, `load(&Path) -> Result<Option<Config>, ConfigError>`, `save(&Path, &Config) -> Result<(), ConfigError>`, `Config::validate(&self) -> Result<(), ConfigError>`.
- Produces `events::{Event, Command, Stage, Key, Action}` exactly as written below. Every other task consumes them.

- [ ] **Step 1: Create the branch and the crate**

```bash
git checkout -b task-1-skeleton
cargo init --name immich-alt-text
```

Replace `Cargo.toml` with:

```toml
[package]
name = "immich-alt-text"
version = "0.1.0"
edition = "2021"
rust-version = "1.85"
description = "Describe Immich photos with a vision LLM from a terminal UI"
license = "MIT"

[lib]
name = "immich_alt_text"
path = "src/lib.rs"

[[bin]]
name = "immich-alt-text"
path = "src/main.rs"

[dependencies]
ratatui = "0.30"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time"] }
tokio-util = "0.7"
reqwest = { version = "0.13", features = ["json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "1"
base64 = "0.23"
chrono = { version = "0.4", default-features = false, features = ["clock"] }
thiserror = "2"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tracing-appender = "0.2"
clap = { version = "4", features = ["derive"] }
url = "2"

[dev-dependencies]
wiremock = "0.6"
insta = "1"
tempfile = "3"
tokio = { version = "1", features = ["full"] }
```

Create `.gitignore`:

```
/target
*.snap.new
```

- [ ] **Step 2: Write `src/events.rs`**

```rust
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
    PageLoaded { scanned: u64, queued: u64 },
    DiscoveryDone { total_queued: u64 },
    AssetStarted { id: String, name: String },
    AssetStage { id: String, stage: Stage },
    AssetDone {
        id: String,
        name: String,
        description: String,
        took: Duration,
        llm_took: Duration,
    },
    AssetFailed { id: String, name: String, error: String },
    RunFinished { done: u64, failed: u64, elapsed: Duration },
    /// The run stopped. Only a config change or restart helps.
    Fatal { error: String },
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
```

- [ ] **Step 3: Write the failing config tests**

Create `src/config.rs` with only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Config {
        Config {
            immich: ImmichConfig {
                url: "https://photos.home.lan".into(),
                api_key: "k1".into(),
                timeout_secs: 30,
            },
            llm: LlmConfig {
                base_url: "http://localhost:1234/v1".into(),
                api_key: String::new(),
                model: "gemma-3-12b-it".into(),
                max_tokens: 200,
                timeout_secs: 120,
                prompt: "describe".into(),
            },
            run: RunConfig {
                workers: 2,
                retries: 3,
                page_size: 500,
            },
            ui: UiConfig {
                theme: ThemeName::Mono,
            },
        }
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        save(&path, &full()).unwrap();
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded, full());
    }

    #[cfg(unix)]
    #[test]
    fn saves_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(&path, &full()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn missing_file_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("nope.toml")).unwrap().is_none());
    }

    #[test]
    fn minimal_file_takes_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[immich]\nurl = \"http://x\"\napi_key = \"k\"\n[llm]\nmodel = \"m\"\n",
        )
        .unwrap();
        let cfg = load(&path).unwrap().unwrap();
        assert_eq!(cfg.run.workers, 1);
        assert_eq!(cfg.run.retries, 3);
        assert_eq!(cfg.run.page_size, 1000);
        assert_eq!(cfg.llm.max_tokens, 200);
        assert_eq!(cfg.llm.timeout_secs, 120);
        assert_eq!(cfg.immich.timeout_secs, 30);
        assert_eq!(cfg.llm.base_url, "http://localhost:1234/v1");
        assert_eq!(cfg.llm.prompt, DEFAULT_PROMPT);
        assert_eq!(cfg.ui.theme, ThemeName::Btop);
    }

    #[test]
    fn rejects_bad_url() {
        let mut cfg = full();
        cfg.immich.url = "photos.home.lan".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("immich.url"), "{err}");

        let mut cfg = full();
        cfg.llm.base_url = "ftp://x".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("llm.base_url"), "{err}");
    }

    #[test]
    fn rejects_zero_workers_and_bad_page_size() {
        let mut cfg = full();
        cfg.run.workers = 0;
        assert!(cfg.validate().is_err());

        let mut cfg = full();
        cfg.run.page_size = 1001;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_empty_key_and_model() {
        let mut cfg = full();
        cfg.immich.api_key = "  ".into();
        assert!(cfg.validate().is_err());

        let mut cfg = full();
        cfg.llm.model = String::new();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn save_refuses_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = full();
        cfg.run.workers = 0;
        assert!(save(&dir.path().join("c.toml"), &cfg).is_err());
    }

    #[test]
    fn default_paths_follow_xdg() {
        let p = default_path();
        assert!(p.ends_with("immich-alt-text/config.toml"), "{p:?}");
        let s = state_dir();
        assert!(s.ends_with("immich-alt-text"), "{s:?}");
    }
}
```

Create `src/lib.rs`:

```rust
pub mod config;
pub mod events;
```

Create `src/main.rs` as a stub for now:

```rust
fn main() {
    println!("immich-alt-text: not wired yet");
}
```

- [ ] **Step 4: Run the tests to see them fail**

Run: `cargo test config`
Expected: compile errors, `Config` and friends not found.

- [ ] **Step 5: Write `src/config.rs` above the test module**

```rust
//! Configuration file: structs, defaults, load, save, validation.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Prompt used when the config file does not set `llm.prompt`.
pub const DEFAULT_PROMPT: &str = "Write alt text for this photo: one or two plain sentences describing what is visible. No preamble, no quotes, no \"This image shows\".";

/// Whole config file. Missing keys take defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub immich: ImmichConfig,
    pub llm: LlmConfig,
    pub run: RunConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImmichConfig {
    pub url: String,
    pub api_key: String,
    pub timeout_secs: u64,
}

impl Default for ImmichConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            api_key: String::new(),
            timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
    pub timeout_secs: u64,
    pub prompt: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:1234/v1".into(),
            api_key: String::new(),
            model: String::new(),
            max_tokens: 200,
            timeout_secs: 120,
            prompt: DEFAULT_PROMPT.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunConfig {
    pub workers: usize,
    pub retries: u32,
    pub page_size: u32,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            workers: 1,
            retries: 3,
            page_size: 1000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct UiConfig {
    pub theme: ThemeName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeName {
    #[default]
    Btop,
    Mono,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read or write {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("cannot serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(var: &str, fallback: &[&str]) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.iter().fold(home(), |p, s| p.join(s)))
}

/// `$XDG_CONFIG_HOME/immich-alt-text/config.toml`, or `~/.config/...`.
pub fn default_path() -> PathBuf {
    xdg("XDG_CONFIG_HOME", &[".config"])
        .join("immich-alt-text")
        .join("config.toml")
}

/// `$XDG_STATE_HOME/immich-alt-text`, or `~/.local/state/...`. Holds the debug log.
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", &[".local", "state"]).join("immich-alt-text")
}

/// Reads the file. Returns `Ok(None)` when it does not exist.
pub fn load(path: &Path) -> Result<Option<Config>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source: e,
            })
        }
    };
    let cfg: Config = toml::from_str(&text).map_err(|e| ConfigError::Parse {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(Some(cfg))
}

/// Validates, then writes the file with owner-only permissions.
pub fn save(path: &Path, config: &Config) -> Result<(), ConfigError> {
    config.validate()?;
    let io = |source: std::io::Error| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let text = toml::to_string_pretty(config)?;
    std::fs::write(path, text).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
    }
    Ok(())
}

fn invalid(msg: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(msg.into())
}

fn check_url(field: &str, value: &str) -> Result<(), ConfigError> {
    let parsed = url::Url::parse(value).map_err(|e| invalid(format!("{field}: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(invalid(format!(
            "{field} must start with http:// or https://"
        )));
    }
    Ok(())
}

impl Config {
    /// Checks every rule from the spec's config section.
    pub fn validate(&self) -> Result<(), ConfigError> {
        check_url("immich.url", &self.immich.url)?;
        check_url("llm.base_url", &self.llm.base_url)?;
        if self.immich.api_key.trim().is_empty() {
            return Err(invalid("immich.api_key must not be empty"));
        }
        if self.llm.model.trim().is_empty() {
            return Err(invalid("llm.model must not be empty"));
        }
        if self.llm.max_tokens == 0 {
            return Err(invalid("llm.max_tokens must be at least 1"));
        }
        if self.run.workers == 0 {
            return Err(invalid("run.workers must be at least 1"));
        }
        if !(1..=1000).contains(&self.run.page_size) {
            return Err(invalid("run.page_size must be between 1 and 1000"));
        }
        Ok(())
    }
}
```

- [ ] **Step 6: Run the tests to see them pass**

Run: `cargo test config`
Expected: 9 tests pass. If `default_paths_follow_xdg` fails because `XDG_CONFIG_HOME` is set to something odd on the machine, the assertion still holds since it only checks the suffix.

- [ ] **Step 7: Lint, format, commit**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add Cargo.toml Cargo.lock .gitignore src/
git commit -m "feat: add crate skeleton, config file handling, and shared event types"
```

- [ ] **Step 8: Open a PR to main**

```bash
git push -u origin task-1-skeleton
gh pr create --fill --title "feat: skeleton, config, shared events"
```

Tasks 2, 3, 5, and 6 branch from `main` after this PR merges.

---

### Task 2: Immich client

**Files:**
- Create: `src/immich.rs`
- Modify: `src/lib.rs` (add `pub mod immich;`)
- Test: unit tests inside `src/immich.rs` using wiremock

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces:
  - `immich::ImmichError { Transient(String), Permanent(String), Fatal(String) }`
  - `immich::Asset { id: String, name: String, description: Option<String> }` with `fn needs_description(&self) -> bool`
  - `immich::Page { items: Vec<Asset>, next_page: Option<u32> }`
  - `immich::ImmichClient::new(url: &str, api_key: &str, timeout: Duration) -> Result<ImmichClient, ImmichError>` (`Clone`)
  - `async fn version(&self) -> Result<String, ImmichError>` returns `"v3.1.0"` style text
  - `async fn list_images(&self, page: u32, size: u32) -> Result<Page, ImmichError>`
  - `async fn preview_jpeg(&self, id: &str) -> Result<Vec<u8>, ImmichError>`
  - `async fn set_description(&self, id: &str, text: &str) -> Result<(), ImmichError>`

- [ ] **Step 1: Branch**

```bash
git checkout main && git pull
git checkout -b task-2-immich-client
```

- [ ] **Step 2: Write the failing tests**

Create `src/immich.rs` with the test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn client(server: &MockServer) -> ImmichClient {
        ImmichClient::new(&server.uri(), "k", Duration::from_secs(5)).unwrap()
    }

    #[tokio::test]
    async fn version_reads_semver() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/server/version"))
            .and(header("x-api-key", "k"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "major": 3, "minor": 1, "patch": 0
            })))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(client(&server).await.version().await.unwrap(), "v3.1.0");
    }

    #[tokio::test]
    async fn trailing_slash_in_url_is_tolerated() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/server/version"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "major": 3, "minor": 2, "patch": 0
            })))
            .mount(&server)
            .await;
        let c = ImmichClient::new(&format!("{}/", server.uri()), "k", Duration::from_secs(5)).unwrap();
        assert_eq!(c.version().await.unwrap(), "v3.2.0");
    }

    #[tokio::test]
    async fn list_images_sends_search_body_and_parses_page() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/search/metadata"))
            .and(header("x-api-key", "k"))
            .and(body_partial_json(json!({
                "type": "IMAGE", "withExif": true, "size": 2, "page": 1, "order": "desc"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "albums": { "count": 0, "items": [], "facets": [], "total": 0 },
                "assets": {
                    "count": 2, "total": 5, "facets": [], "nextPage": "2",
                    "items": [
                        { "id": "a1", "originalFileName": "IMG_1.HEIC", "type": "IMAGE",
                          "exifInfo": { "description": null } },
                        { "id": "a2", "originalFileName": "IMG_2.HEIC", "type": "IMAGE",
                          "exifInfo": { "description": "a dog" } }
                    ]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let page = client(&server).await.list_images(1, 2).await.unwrap();
        assert_eq!(page.next_page, Some(2));
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].id, "a1");
        assert_eq!(page.items[0].name, "IMG_1.HEIC");
        assert!(page.items[0].needs_description());
        assert!(!page.items[1].needs_description());
    }

    #[tokio::test]
    async fn list_images_handles_last_page_and_missing_exif() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/search/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "assets": { "count": 1, "total": 1, "facets": [], "nextPage": null,
                    "items": [ { "id": "a3", "originalFileName": "x.jpg", "type": "IMAGE" } ] }
            })))
            .mount(&server)
            .await;
        let page = client(&server).await.list_images(3, 1000).await.unwrap();
        assert_eq!(page.next_page, None);
        assert!(page.items[0].needs_description());
    }

    #[tokio::test]
    async fn blank_description_needs_a_new_one() {
        let a = Asset { id: "x".into(), name: "x".into(), description: Some("   ".into()) };
        assert!(a.needs_description());
    }

    #[tokio::test]
    async fn preview_jpeg_requests_preview_size() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/assets/a1/thumbnail"))
            .and(query_param("size", "preview"))
            .and(header("x-api-key", "k"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFF, 0xD8, 0xFF]))
            .expect(1)
            .mount(&server)
            .await;
        let bytes = client(&server).await.preview_jpeg("a1").await.unwrap();
        assert_eq!(bytes, vec![0xFF, 0xD8, 0xFF]);
    }

    #[tokio::test]
    async fn empty_preview_is_permanent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/assets/a1/thumbnail"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(Vec::<u8>::new()))
            .mount(&server)
            .await;
        let err = client(&server).await.preview_jpeg("a1").await.unwrap_err();
        assert!(matches!(err, ImmichError::Permanent(_)), "{err}");
    }

    #[tokio::test]
    async fn set_description_puts_json() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/assets/a1"))
            .and(header("x-api-key", "k"))
            .and(body_json(json!({ "description": "A dog on a dock." })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "a1" })))
            .expect(1)
            .mount(&server)
            .await;
        client(&server).await.set_description("a1", "A dog on a dock.").await.unwrap();
    }

    #[tokio::test]
    async fn unauthorized_is_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/server/version"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = client(&server).await.version().await.unwrap_err();
        assert!(matches!(err, ImmichError::Fatal(_)), "{err}");
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn server_error_is_transient() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/assets/a1"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let err = client(&server).await.set_description("a1", "x").await.unwrap_err();
        assert!(matches!(err, ImmichError::Transient(_)), "{err}");
    }

    #[tokio::test]
    async fn not_found_is_permanent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/assets/gone/thumbnail"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = client(&server).await.preview_jpeg("gone").await.unwrap_err();
        assert!(matches!(err, ImmichError::Permanent(_)), "{err}");
    }

    #[tokio::test]
    async fn malformed_body_is_permanent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/search/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let err = client(&server).await.list_images(1, 10).await.unwrap_err();
        assert!(matches!(err, ImmichError::Permanent(_)), "{err}");
    }

    #[tokio::test]
    async fn connection_refused_is_transient() {
        // Port 9 is the discard port. Nothing listens there on a dev machine.
        let c = ImmichClient::new("http://127.0.0.1:9", "k", Duration::from_secs(2)).unwrap();
        let err = c.version().await.unwrap_err();
        assert!(matches!(err, ImmichError::Transient(_)), "{err}");
    }
}
```

Add `pub mod immich;` to `src/lib.rs`.

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test immich`
Expected: compile errors, `ImmichClient` not found.

- [ ] **Step 4: Write the client above the tests**

```rust
//! Typed client for the parts of the Immich API this tool uses.
//! All paths sit under `/api`. Auth is the `x-api-key` header.

use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ImmichError {
    /// Network or server trouble that may pass. Worth a retry.
    #[error("immich: {0}")]
    Transient(String),
    /// Wrong for this one asset. Skip it.
    #[error("immich: {0}")]
    Permanent(String),
    /// Wrong for the whole run: bad key or bad server.
    #[error("immich: {0}")]
    Fatal(String),
}

/// One photo as the engine sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

impl Asset {
    /// True when the description is missing or only whitespace.
    pub fn needs_description(&self) -> bool {
        self.description
            .as_deref()
            .is_none_or(|d| d.trim().is_empty())
    }
}

/// One page of search results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub items: Vec<Asset>,
    pub next_page: Option<u32>,
}

#[derive(Clone)]
pub struct ImmichClient {
    http: reqwest::Client,
    base: String,
    api_key: String,
}

impl ImmichClient {
    /// `url` is the server root, with or without a trailing slash.
    pub fn new(url: &str, api_key: &str, timeout: Duration) -> Result<Self, ImmichError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ImmichError::Fatal(format!("http client: {e}")))?;
        Ok(Self {
            http,
            base: format!("{}/api", url.trim_end_matches('/')),
            api_key: api_key.to_string(),
        })
    }

    /// Adds auth, sends, logs method, path, status, and duration, then maps the status.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, ImmichError> {
        let req = req
            .header("x-api-key", &self.api_key)
            .build()
            .map_err(|e| ImmichError::Permanent(format!("bad request: {e}")))?;
        let method = req.method().clone();
        let path = req.url().path().to_string();
        let started = std::time::Instant::now();
        let resp = self.http.execute(req).await.map_err(transport)?;
        tracing::debug!(
            %method,
            %path,
            status = %resp.status(),
            ms = started.elapsed().as_millis() as u64,
            "immich"
        );
        check_status(resp).await
    }

    /// Server version as `vMAJOR.MINOR.PATCH`. Also proves the key works.
    pub async fn version(&self) -> Result<String, ImmichError> {
        #[derive(Deserialize)]
        struct Version {
            major: u32,
            minor: u32,
            patch: u32,
        }
        let resp = self
            .send(self.http.get(format!("{}/server/version", self.base)))
            .await?;
        let v: Version = resp.json().await.map_err(bad_body)?;
        Ok(format!("v{}.{}.{}", v.major, v.minor, v.patch))
    }

    /// One page of images, newest first, with EXIF so the description is present.
    pub async fn list_images(&self, page: u32, size: u32) -> Result<Page, ImmichError> {
        let body = serde_json::json!({
            "type": "IMAGE",
            "withExif": true,
            "size": size,
            "page": page,
            "order": "desc",
        });
        let resp = self
            .send(
                self.http
                    .post(format!("{}/search/metadata", self.base))
                    .json(&body),
            )
            .await?;
        let parsed: SearchResponse = resp.json().await.map_err(bad_body)?;
        let items = parsed
            .assets
            .items
            .into_iter()
            .map(|a| Asset {
                id: a.id,
                name: a.original_file_name,
                description: a.exif_info.and_then(|e| e.description),
            })
            .collect();
        let next_page = parsed.assets.next_page.and_then(|s| s.parse().ok());
        Ok(Page { items, next_page })
    }

    /// The `preview` rendition as JPEG bytes.
    pub async fn preview_jpeg(&self, id: &str) -> Result<Vec<u8>, ImmichError> {
        let resp = self
            .send(
                self.http
                    .get(format!("{}/assets/{id}/thumbnail", self.base))
                    .query(&[("size", "preview")]),
            )
            .await?;
        let bytes = resp.bytes().await.map_err(transport)?;
        if bytes.is_empty() {
            return Err(ImmichError::Permanent("empty preview body".into()));
        }
        Ok(bytes.to_vec())
    }

    /// Sets the asset description. Overwrites whatever is there.
    pub async fn set_description(&self, id: &str, text: &str) -> Result<(), ImmichError> {
        self.send(
            self.http
                .put(format!("{}/assets/{id}", self.base))
                .json(&serde_json::json!({ "description": text })),
        )
        .await
        .map(|_| ())
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    assets: SearchAssets,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchAssets {
    items: Vec<AssetDto>,
    next_page: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssetDto {
    id: String,
    original_file_name: String,
    exif_info: Option<ExifDto>,
}

#[derive(Deserialize)]
struct ExifDto {
    description: Option<String>,
}

fn transport(e: reqwest::Error) -> ImmichError {
    ImmichError::Transient(e.to_string())
}

fn bad_body(e: reqwest::Error) -> ImmichError {
    ImmichError::Permanent(format!("bad response body: {e}"))
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, ImmichError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let msg = format!("HTTP {status}");
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            Err(ImmichError::Fatal(format!("{msg}: check the API key")))
        }
        StatusCode::TOO_MANY_REQUESTS => Err(ImmichError::Transient(msg)),
        s if s.is_server_error() => Err(ImmichError::Transient(msg)),
        _ => Err(ImmichError::Permanent(msg)),
    }
}
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test immich`
Expected: 13 tests pass.

- [ ] **Step 6: Lint, format, commit, PR**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/immich.rs src/lib.rs
git commit -m "feat: add Immich client for listing images, previews, and descriptions"
git push -u origin task-2-immich-client
gh pr create --fill --title "feat: Immich client"
```

---

### Task 3: LLM client

**Files:**
- Create: `src/llm.rs`
- Modify: `src/lib.rs` (add `pub mod llm;`)
- Test: unit tests inside `src/llm.rs` using wiremock

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces:
  - `llm::LlmError { Transient(String), Permanent(String), Fatal(String) }`
  - `llm::LlmClient::new(base_url: &str, api_key: &str, model: &str, max_tokens: u32, timeout: Duration) -> Result<LlmClient, LlmError>` (`Clone`)
  - `async fn ping(&self) -> Result<String, LlmError>` calls `GET {base_url}/models`, returns the status line such as `"200 OK"`
  - `async fn describe(&self, jpeg: &[u8], prompt: &str) -> Result<String, LlmError>` returns the trimmed text

- [ ] **Step 1: Branch**

```bash
git checkout main && git pull
git checkout -b task-3-llm-client
```

- [ ] **Step 2: Write the failing tests**

Create `src/llm.rs` with the test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0];

    fn ok_body(text: &str) -> serde_json::Value {
        json!({
            "id": "chatcmpl-1", "object": "chat.completion", "model": "m",
            "choices": [ { "index": 0, "finish_reason": "stop",
                "message": { "role": "assistant", "content": text } } ]
        })
    }

    async fn client(server: &MockServer, key: &str) -> LlmClient {
        LlmClient::new(&format!("{}/v1", server.uri()), key, "gemma", 200, Duration::from_secs(5)).unwrap()
    }

    #[tokio::test]
    async fn describe_sends_openai_vision_shape() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-test"))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(json!({
                "model": "gemma",
                "max_tokens": 200,
                "messages": [ { "role": "user", "content": [
                    { "type": "text", "text": "describe it" }
                ] } ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("  A red bike.  \n")))
            .expect(1)
            .mount(&server)
            .await;

        let text = client(&server, "sk-test").await.describe(JPEG, "describe it").await.unwrap();
        assert_eq!(text, "A red bike.");

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let url = body["messages"][0]["content"][1]["image_url"]["url"].as_str().unwrap();
        assert_eq!(body["messages"][0]["content"][1]["type"], "image_url");
        assert!(url.starts_with("data:image/jpeg;base64,/9j/"), "{url}");
    }

    #[tokio::test]
    async fn empty_key_sends_no_authorization_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("x")))
            .mount(&server)
            .await;
        let text = client(&server, "").await.describe(JPEG, "p").await.unwrap();
        assert_eq!(text, "x");
    }

    #[tokio::test]
    async fn trailing_slash_in_base_url_is_tolerated() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("ok")))
            .mount(&server)
            .await;
        let c = LlmClient::new(&format!("{}/v1/", server.uri()), "", "m", 10, Duration::from_secs(5)).unwrap();
        assert_eq!(c.describe(JPEG, "p").await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn empty_content_is_permanent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("   ")))
            .mount(&server)
            .await;
        let err = client(&server, "").await.describe(JPEG, "p").await.unwrap_err();
        assert!(matches!(err, LlmError::Permanent(_)), "{err}");
    }

    #[tokio::test]
    async fn missing_choices_is_permanent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "choices": [] })))
            .mount(&server)
            .await;
        let err = client(&server, "").await.describe(JPEG, "p").await.unwrap_err();
        assert!(matches!(err, LlmError::Permanent(_)), "{err}");
    }

    #[tokio::test]
    async fn unauthorized_and_not_found_are_fatal() {
        for status in [401u16, 404] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let err = client(&server, "").await.describe(JPEG, "p").await.unwrap_err();
            assert!(matches!(err, LlmError::Fatal(_)), "{status}: {err}");
        }
    }

    #[tokio::test]
    async fn server_errors_and_rate_limits_are_transient() {
        for status in [500u16, 503, 429] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let err = client(&server, "").await.describe(JPEG, "p").await.unwrap_err();
            assert!(matches!(err, LlmError::Transient(_)), "{status}: {err}");
        }
    }

    #[tokio::test]
    async fn bad_request_is_permanent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;
        let err = client(&server, "").await.describe(JPEG, "p").await.unwrap_err();
        assert!(matches!(err, LlmError::Permanent(_)), "{err}");
    }

    #[tokio::test]
    async fn ping_hits_models() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer k"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(client(&server, "k").await.ping().await.unwrap(), "200 OK");
    }

    #[tokio::test]
    async fn ping_reports_fatal_on_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = client(&server, "k").await.ping().await.unwrap_err();
        assert!(matches!(err, LlmError::Fatal(_)), "{err}");
    }
}
```

Add `pub mod llm;` to `src/lib.rs`.

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test llm`
Expected: compile errors, `LlmClient` not found.

- [ ] **Step 4: Write the client above the tests**

```rust
//! OpenAI-compatible chat completions client. One vision call per photo.

use std::time::Duration;

use base64::Engine as _;
use reqwest::StatusCode;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// Network or server trouble that may pass. Worth a retry.
    #[error("llm: {0}")]
    Transient(String),
    /// Wrong for this one photo. Skip it.
    #[error("llm: {0}")]
    Permanent(String),
    /// Wrong for the whole run: bad key, unknown model, wrong URL.
    #[error("llm: {0}")]
    Fatal(String),
}

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl LlmClient {
    /// `base_url` ends at the API root, for example `http://localhost:1234/v1`.
    pub fn new(
        base_url: &str,
        api_key: &str,
        model: &str,
        max_tokens: u32,
        timeout: Duration,
    ) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| LlmError::Fatal(format!("http client: {e}")))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            max_tokens,
        })
    }

    fn authorize(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.api_key.is_empty() {
            req
        } else {
            req.bearer_auth(&self.api_key)
        }
    }

    /// Adds auth, sends, logs method, path, status, and duration, then maps the status.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, LlmError> {
        let req = self
            .authorize(req)
            .build()
            .map_err(|e| LlmError::Permanent(format!("bad request: {e}")))?;
        let method = req.method().clone();
        let path = req.url().path().to_string();
        let started = std::time::Instant::now();
        let resp = self.http.execute(req).await.map_err(transport)?;
        tracing::debug!(
            %method,
            %path,
            status = %resp.status(),
            ms = started.elapsed().as_millis() as u64,
            "llm"
        );
        check_status(resp).await
    }

    /// Lists models. Returns the HTTP status line. Proves URL and key.
    pub async fn ping(&self) -> Result<String, LlmError> {
        let resp = self
            .send(self.http.get(format!("{}/models", self.base_url)))
            .await?;
        Ok(resp.status().to_string())
    }

    /// Asks the model for a description of one JPEG.
    pub async fn describe(&self, jpeg: &[u8], prompt: &str) -> Result<String, LlmError> {
        let data_uri = format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(jpeg)
        );
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": prompt },
                    { "type": "image_url", "image_url": { "url": data_uri } }
                ]
            }]
        });
        let resp = self
            .send(
                self.http
                    .post(format!("{}/chat/completions", self.base_url))
                    .json(&body),
            )
            .await?;
        let parsed: Completion = resp
            .json()
            .await
            .map_err(|e| LlmError::Permanent(format!("bad response body: {e}")))?;
        let text = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();
        let text = text.trim();
        if text.is_empty() {
            return Err(LlmError::Permanent("model returned no text".into()));
        }
        Ok(text.to_string())
    }
}

#[derive(Deserialize)]
struct Completion {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    content: Option<String>,
}

fn transport(e: reqwest::Error) -> LlmError {
    LlmError::Transient(e.to_string())
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, LlmError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let msg = format!("HTTP {status}");
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            Err(LlmError::Fatal(format!("{msg}: check the API key")))
        }
        StatusCode::NOT_FOUND => Err(LlmError::Fatal(format!(
            "{msg}: check the base URL and model name"
        ))),
        StatusCode::TOO_MANY_REQUESTS => Err(LlmError::Transient(msg)),
        s if s.is_server_error() => Err(LlmError::Transient(msg)),
        _ => Err(LlmError::Permanent(msg)),
    }
}
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test llm`
Expected: 10 tests pass. In `empty_key_sends_no_authorization_header`, wiremock picks the first mounted mock that matches, so the request without the header falls through to the second mock and returns 200.

- [ ] **Step 6: Lint, format, commit, PR**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/llm.rs src/lib.rs
git commit -m "feat: add OpenAI-compatible vision client"
git push -u origin task-3-llm-client
gh pr create --fill --title "feat: LLM client"
```

---

### Task 4: Engine

**Files:**
- Create: `src/engine.rs`, `tests/engine_test.rs`
- Modify: `src/lib.rs` (add `pub mod engine;`)

**Interfaces:**
- Consumes: `config::Config`, `events::{Command, Event, Stage}`, `immich::{Asset, ImmichClient, ImmichError}`, `llm::{LlmClient, LlmError}` with the exact signatures from Tasks 1 to 3.
- Produces:
  - `engine::EngineOptions { backoff_base: Duration }` with `Default` (2 s)
  - `engine::EngineError` (`From<ImmichError>`, `From<LlmError>`)
  - `engine::spawn(config: Config, events: mpsc::Sender<Event>) -> Result<EngineHandle, EngineError>`
  - `engine::spawn_with(config, events, options: EngineOptions) -> Result<EngineHandle, EngineError>`
  - `EngineHandle::send(&self, Command)` and `async fn shutdown(self, grace: Duration)`

Retry semantics: `run.retries` is the number of retries after the first try. Default 3 means up to 4 attempts with sleeps of 2 s, 4 s, 8 s between them.

- [ ] **Step 1: Branch**

```bash
git checkout main && git pull
git checkout -b task-4-engine
```

- [ ] **Step 2: Write the failing integration tests**

Create `tests/engine_test.rs`:

```rust
use std::time::Duration;

use immich_alt_text::config::{Config, ImmichConfig, LlmConfig, RunConfig, UiConfig};
use immich_alt_text::engine::{self, EngineOptions};
use immich_alt_text::events::{Command, Event};
use serde_json::json;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0];

fn config(immich: &MockServer, llm: &MockServer) -> Config {
    Config {
        immich: ImmichConfig {
            url: immich.uri(),
            api_key: "k".into(),
            timeout_secs: 5,
        },
        llm: LlmConfig {
            base_url: format!("{}/v1", llm.uri()),
            api_key: String::new(),
            model: "m".into(),
            max_tokens: 50,
            timeout_secs: 5,
            prompt: "describe".into(),
        },
        run: RunConfig {
            workers: 1,
            retries: 3,
            page_size: 10,
        },
        ui: UiConfig::default(),
    }
}

fn fast() -> EngineOptions {
    EngineOptions {
        backoff_base: Duration::from_millis(1),
    }
}

fn search_page(items: &[(&str, &str, Option<&str>)]) -> serde_json::Value {
    let items: Vec<_> = items
        .iter()
        .map(|(id, name, desc)| {
            json!({ "id": id, "originalFileName": name, "type": "IMAGE",
                    "exifInfo": { "description": desc } })
        })
        .collect();
    json!({ "assets": { "count": items.len(), "total": items.len(), "facets": [],
                        "nextPage": null, "items": items } })
}

fn completion(text: &str) -> serde_json::Value {
    json!({ "choices": [ { "index": 0, "message": { "role": "assistant", "content": text } } ] })
}

async fn mount_immich_basics(immich: &MockServer, items: &[(&str, &str, Option<&str>)]) {
    Mock::given(method("POST"))
        .and(path("/api/search/metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_page(items)))
        .mount(immich)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/assets/[^/]+/thumbnail$"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(JPEG.to_vec()))
        .mount(immich)
        .await;
}

async fn next_event(rx: &mut mpsc::Receiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("event channel closed")
}

/// Drains events until `stop` matches. Returns everything seen, the match last.
async fn collect_until(rx: &mut mpsc::Receiver<Event>, stop: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let e = next_event(rx).await;
        let done = stop(&e);
        seen.push(e);
        if done {
            return seen;
        }
    }
}

#[tokio::test]
async fn skips_described_assets_and_writes_the_rest() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[
        ("a1", "IMG_1.HEIC", None),
        ("a2", "IMG_2.HEIC", Some("a dog")),
        ("a3", "IMG_3.HEIC", Some("  ")),
    ]).await;
    Mock::given(method("PUT")).and(path("/api/assets/a1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).expect(1).mount(&immich).await;
    Mock::given(method("PUT")).and(path("/api/assets/a3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).expect(1).mount(&immich).await;
    Mock::given(method("PUT")).and(path("/api/assets/a2"))
        .respond_with(ResponseTemplate::new(200)).expect(0).mount(&immich).await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("A dog on a dock.")))
        .expect(2).mount(&llm).await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;

    assert_eq!(events[0], Event::PageLoaded { scanned: 3, queued: 2 });
    let done: Vec<&str> = events.iter().filter_map(|e| match e {
        Event::AssetDone { name, description, .. } => {
            assert_eq!(description, "A dog on a dock.");
            Some(name.as_str())
        }
        _ => None,
    }).collect();
    assert_eq!(done, vec!["IMG_1.HEIC", "IMG_3.HEIC"]);
    assert!(events.iter().any(|e| matches!(e, Event::DiscoveryDone { total_queued: 2 })));
    assert!(!events.iter().any(|e| matches!(e, Event::AssetFailed { .. })));
    match events.last().unwrap() {
        Event::RunFinished { done, failed, .. } => {
            assert_eq!(*done, 2);
            assert_eq!(*failed, 0);
        }
        other => panic!("unexpected last event {other:?}"),
    }
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn emits_stages_in_order_for_one_asset() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "IMG_1.HEIC", None)]).await;
    Mock::given(method("PUT")).and(path("/api/assets/a1"))
        .respond_with(ResponseTemplate::new(200)).mount(&immich).await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("x"))).mount(&llm).await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;

    let stages: Vec<String> = events.iter().filter_map(|e| match e {
        Event::AssetStarted { .. } => Some("started".to_string()),
        Event::AssetStage { stage, .. } => Some(stage.label().to_string()),
        Event::AssetDone { .. } => Some("done".to_string()),
        _ => None,
    }).collect();
    assert_eq!(stages, vec!["started", "fetching", "calling llm", "writing", "done"]);
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn retries_transient_llm_errors_then_succeeds() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "IMG_1.HEIC", None)]).await;
    Mock::given(method("PUT")).and(path("/api/assets/a1"))
        .respond_with(ResponseTemplate::new(200)).expect(1).mount(&immich).await;
    // First two calls fail, then the mock below takes over.
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503)).up_to_n_times(2).expect(2).mount(&llm).await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("third time"))).expect(1).mount(&llm).await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;

    assert!(events.iter().any(|e| matches!(e, Event::AssetDone { description, .. } if description == "third time")));
    assert!(!events.iter().any(|e| matches!(e, Event::AssetFailed { .. })));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn gives_up_after_all_attempts_and_continues() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "IMG_1.HEIC", None), ("a2", "IMG_2.HEIC", None)]).await;
    Mock::given(method("PUT")).and(path_regex(r"^/api/assets/[^/]+$"))
        .respond_with(ResponseTemplate::new(200)).expect(1).mount(&immich).await;
    // a1 always fails: 1 try + 3 retries = 4 calls. a2 succeeds on its first call.
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500)).up_to_n_times(4).expect(4).mount(&llm).await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("ok"))).expect(1).mount(&llm).await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;

    let failed: Vec<&Event> = events.iter().filter(|e| matches!(e, Event::AssetFailed { .. })).collect();
    assert_eq!(failed.len(), 1);
    if let Event::AssetFailed { name, error, .. } = failed[0] {
        assert_eq!(name, "IMG_1.HEIC");
        assert!(error.contains("llm"), "{error}");
        assert!(error.contains("4 tries"), "{error}");
    }
    assert!(matches!(events.last().unwrap(), Event::RunFinished { done: 1, failed: 1, .. }));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn permanent_llm_error_does_not_retry() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "IMG_1.HEIC", None)]).await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("")))
        .expect(1).mount(&llm).await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;
    assert!(events.iter().any(|e| matches!(e, Event::AssetFailed { .. })));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn immich_write_failure_marks_asset_failed() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "IMG_1.HEIC", None)]).await;
    Mock::given(method("PUT")).and(path("/api/assets/a1"))
        .respond_with(ResponseTemplate::new(500)).expect(4).mount(&immich).await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("ok"))).mount(&llm).await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;
    assert!(events.iter().any(|e| matches!(e, Event::AssetFailed { error, .. } if error.contains("immich"))));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn pause_stops_new_assets_until_resume() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "1", None), ("a2", "2", None), ("a3", "3", None)]).await;
    Mock::given(method("PUT")).and(path_regex(r"^/api/assets/[^/]+$"))
        .respond_with(ResponseTemplate::new(200)).mount(&immich).await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("ok")).set_delay(Duration::from_millis(200)))
        .mount(&llm).await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    collect_until(&mut rx, |e| matches!(e, Event::AssetStarted { .. })).await;
    handle.send(Command::Pause).await;
    collect_until(&mut rx, |e| matches!(e, Event::AssetDone { .. })).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    while let Ok(e) = rx.try_recv() {
        assert!(!matches!(e, Event::AssetStarted { .. }), "started while paused: {e:?}");
    }

    handle.send(Command::Resume).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;
    assert!(matches!(events.last().unwrap(), Event::RunFinished { done: 3, failed: 0, .. }));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn unauthorized_immich_is_fatal_and_stops_the_run() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/search/metadata"))
        .respond_with(ResponseTemplate::new(401)).expect(1).mount(&immich).await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let e = next_event(&mut rx).await;
    assert!(matches!(&e, Event::Fatal { error } if error.contains("401")), "{e:?}");
    assert!(tokio::time::timeout(Duration::from_millis(300), rx.recv()).await.is_err(), "no more events after Fatal");
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn start_after_finish_runs_again() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[]).await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;
    assert_eq!(events[0], Event::PageLoaded { scanned: 0, queued: 0 });
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn quit_stops_everything() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "1", None)]).await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("ok")).set_delay(Duration::from_secs(3)))
        .mount(&llm).await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    collect_until(&mut rx, |e| matches!(e, Event::AssetStage { .. })).await;
    let started = std::time::Instant::now();
    handle.shutdown(Duration::from_secs(1)).await;
    assert!(started.elapsed() < Duration::from_secs(2), "shutdown must not wait for the slow LLM call");
}
```

Add `pub mod engine;` to `src/lib.rs`.

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test --test engine_test`
Expected: compile errors, `engine` module not found.

- [ ] **Step 4: Write `src/engine.rs`**

```rust
//! Discovery, worker pool, and retries. Talks to the UI only through `Event`s.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::events::{Command, Event, Stage};
use crate::immich::{Asset, ImmichClient, ImmichError};
use crate::llm::{LlmClient, LlmError};

/// Knobs that tests change. Production uses `Default`.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Sleep before the first retry. Doubles on each further retry.
    pub backoff_base: Duration,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            backoff_base: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Immich(#[from] ImmichError),
    #[error(transparent)]
    Llm(#[from] LlmError),
}

/// Handle to a running engine task.
pub struct EngineHandle {
    cmd_tx: mpsc::Sender<Command>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl EngineHandle {
    /// Sends a command. Dropped silently if the engine is gone.
    pub async fn send(&self, cmd: Command) {
        let _ = self.cmd_tx.send(cmd).await;
    }

    /// Cancels the engine and waits up to `grace` for it to stop.
    pub async fn shutdown(self, grace: Duration) {
        self.cancel.cancel();
        let _ = tokio::time::timeout(grace, self.task).await;
    }
}

/// Starts the engine with production options.
pub fn spawn(config: Config, events: mpsc::Sender<Event>) -> Result<EngineHandle, EngineError> {
    spawn_with(config, events, EngineOptions::default())
}

/// Starts the engine. It waits for `Command::Start` before doing any work.
pub fn spawn_with(
    config: Config,
    events: mpsc::Sender<Event>,
    options: EngineOptions,
) -> Result<EngineHandle, EngineError> {
    let immich = ImmichClient::new(
        &config.immich.url,
        &config.immich.api_key,
        Duration::from_secs(config.immich.timeout_secs),
    )?;
    let llm = LlmClient::new(
        &config.llm.base_url,
        &config.llm.api_key,
        &config.llm.model,
        config.llm.max_tokens,
        Duration::from_secs(config.llm.timeout_secs),
    )?;
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let engine = Arc::new(Engine {
        immich,
        llm,
        config,
        options,
        events,
        cancel: cancel.clone(),
    });
    let task = tokio::spawn(engine.control_loop(cmd_rx));
    Ok(EngineHandle {
        cmd_tx,
        cancel,
        task,
    })
}

struct Engine {
    immich: ImmichClient,
    llm: LlmClient,
    config: Config,
    options: EngineOptions,
    events: mpsc::Sender<Event>,
    cancel: CancellationToken,
}

/// One run: discovery plus workers. Dropped when the next run starts.
struct Run {
    pause_tx: watch::Sender<bool>,
    token: CancellationToken,
    /// Cleared just before `RunFinished` so a new `Start` is accepted at once.
    active: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

enum Outcome {
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
enum StageError {
    #[error("{0}")]
    Transient(String),
    #[error("{0}")]
    Permanent(String),
    #[error("{0}")]
    Fatal(String),
}

impl From<ImmichError> for StageError {
    fn from(e: ImmichError) -> Self {
        let msg = e.to_string();
        match e {
            ImmichError::Transient(_) => StageError::Transient(msg),
            ImmichError::Permanent(_) => StageError::Permanent(msg),
            ImmichError::Fatal(_) => StageError::Fatal(msg),
        }
    }
}

impl From<LlmError> for StageError {
    fn from(e: LlmError) -> Self {
        let msg = e.to_string();
        match e {
            LlmError::Transient(_) => StageError::Transient(msg),
            LlmError::Permanent(_) => StageError::Permanent(msg),
            LlmError::Fatal(_) => StageError::Fatal(msg),
        }
    }
}

impl Engine {
    async fn control_loop(self: Arc<Self>, mut cmd_rx: mpsc::Receiver<Command>) {
        let mut run: Option<Run> = None;
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                cmd = cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    match cmd {
                        Command::Start => {
                            let busy = run.as_ref().is_some_and(|r| {
                                r.active.load(Ordering::Acquire) && !r.token.is_cancelled()
                            });
                            if !busy {
                                if let Some(r) = run.take() {
                                    // Finishing, or stopped by a fatal error: let it wrap up first.
                                    let _ = r.task.await;
                                }
                                run = Some(self.clone().start_run());
                            }
                        }
                        Command::Pause => {
                            if let Some(r) = &run {
                                let _ = r.pause_tx.send(true);
                            }
                        }
                        Command::Resume => {
                            if let Some(r) = &run {
                                let _ = r.pause_tx.send(false);
                            }
                        }
                        Command::Quit => {
                            self.cancel.cancel();
                            break;
                        }
                    }
                }
            }
        }
        if let Some(r) = run {
            let _ = r.task.await;
        }
    }

    fn start_run(self: Arc<Self>) -> Run {
        let token = self.cancel.child_token();
        let (pause_tx, pause_rx) = watch::channel(false);
        let active = Arc::new(AtomicBool::new(true));
        let task = tokio::spawn(self.run(token.clone(), pause_rx, active.clone()));
        Run {
            pause_tx,
            token,
            active,
            task,
        }
    }

    async fn run(
        self: Arc<Self>,
        token: CancellationToken,
        pause_rx: watch::Receiver<bool>,
        active: Arc<AtomicBool>,
    ) {
        let started = Instant::now();
        let workers = self.config.run.workers.max(1);
        // Bounded so discovery blocks instead of loading the whole library into memory.
        let (asset_tx, asset_rx) = mpsc::channel::<Asset>(workers * 4);
        let asset_rx = Arc::new(Mutex::new(asset_rx));
        let done = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU64::new(0));

        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(tokio::spawn(self.clone().worker(
                token.clone(),
                pause_rx.clone(),
                asset_rx.clone(),
                done.clone(),
                failed.clone(),
            )));
        }
        self.clone().discover(token.clone(), asset_tx).await;
        for h in handles {
            let _ = h.await;
        }
        active.store(false, Ordering::Release);
        if !token.is_cancelled() {
            self.emit(Event::RunFinished {
                done: done.load(Ordering::Relaxed),
                failed: failed.load(Ordering::Relaxed),
                elapsed: started.elapsed(),
            })
            .await;
        }
    }

    /// Pages through Immich and queues assets that need a description.
    /// Dropping `asset_tx` at the end tells the workers to stop.
    async fn discover(self: Arc<Self>, token: CancellationToken, asset_tx: mpsc::Sender<Asset>) {
        let mut page = 1u32;
        let mut scanned = 0u64;
        let mut queued = 0u64;
        loop {
            let result = tokio::select! {
                _ = token.cancelled() => return,
                r = self.retry(|| self.immich.list_images(page, self.config.run.page_size)) => r,
            };
            let p = match result {
                Ok(p) => p,
                Err(e) => {
                    self.fail_run(&token, e.to_string()).await;
                    return;
                }
            };
            scanned += p.items.len() as u64;
            let wanted: Vec<Asset> = p.items.into_iter().filter(|a| a.needs_description()).collect();
            queued += wanted.len() as u64;
            self.emit(Event::PageLoaded { scanned, queued }).await;
            for asset in wanted {
                tokio::select! {
                    _ = token.cancelled() => return,
                    r = asset_tx.send(asset) => if r.is_err() { return },
                }
            }
            match p.next_page {
                Some(n) if n > page => page = n,
                _ => break,
            }
        }
        self.emit(Event::DiscoveryDone { total_queued: queued }).await;
    }

    async fn worker(
        self: Arc<Self>,
        token: CancellationToken,
        mut pause_rx: watch::Receiver<bool>,
        asset_rx: Arc<Mutex<mpsc::Receiver<Asset>>>,
        done: Arc<AtomicU64>,
        failed: Arc<AtomicU64>,
    ) {
        loop {
            while *pause_rx.borrow() {
                tokio::select! {
                    _ = token.cancelled() => return,
                    r = pause_rx.changed() => if r.is_err() { return },
                }
            }
            let asset = {
                let mut rx = asset_rx.lock().await;
                tokio::select! {
                    _ = token.cancelled() => return,
                    a = rx.recv() => match a {
                        Some(a) => a,
                        None => return,
                    },
                }
            };
            match self.process(&token, &asset).await {
                Outcome::Done => done.fetch_add(1, Ordering::Relaxed),
                Outcome::Failed => failed.fetch_add(1, Ordering::Relaxed),
                Outcome::Cancelled => return,
            };
        }
    }

    async fn process(&self, token: &CancellationToken, asset: &Asset) -> Outcome {
        let started = Instant::now();
        let id = asset.id.clone();
        let name = asset.name.clone();
        self.emit(Event::AssetStarted {
            id: id.clone(),
            name: name.clone(),
        })
        .await;

        self.stage(&id, Stage::Fetching).await;
        let jpeg = tokio::select! {
            _ = token.cancelled() => return Outcome::Cancelled,
            r = self.retry(|| self.immich.preview_jpeg(&id)) => r,
        };
        let jpeg = match jpeg {
            Ok(j) => j,
            Err(e) => return self.fail_asset(token, id, name, e).await,
        };

        self.stage(&id, Stage::CallingLlm).await;
        let llm_started = Instant::now();
        let text = tokio::select! {
            _ = token.cancelled() => return Outcome::Cancelled,
            r = self.retry(|| self.llm.describe(&jpeg, &self.config.llm.prompt)) => r,
        };
        let text = match text {
            Ok(t) => t,
            Err(e) => return self.fail_asset(token, id, name, e).await,
        };
        let llm_took = llm_started.elapsed();

        // A write that has started is allowed to finish, so no cancellation race here.
        self.stage(&id, Stage::Writing).await;
        if let Err(e) = self.retry(|| self.immich.set_description(&id, &text)).await {
            return self.fail_asset(token, id, name, e).await;
        }

        self.emit(Event::AssetDone {
            id,
            name,
            description: text,
            took: started.elapsed(),
            llm_took,
        })
        .await;
        Outcome::Done
    }

    async fn retry<T, E, F, Fut>(&self, mut op: F) -> Result<T, StageError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: Into<StageError>,
    {
        let attempts = self.config.run.retries + 1;
        let mut attempt = 1u32;
        loop {
            match op().await.map_err(Into::into) {
                Ok(v) => return Ok(v),
                Err(StageError::Transient(msg)) if attempt < attempts => {
                    let delay = self.options.backoff_base * 2u32.pow(attempt - 1);
                    tracing::warn!(attempt, delay_ms = delay.as_millis() as u64, %msg, "retrying");
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(StageError::Transient(msg)) => {
                    return Err(StageError::Transient(format!("{msg} ({attempts} tries)")))
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn fail_asset(
        &self,
        token: &CancellationToken,
        id: String,
        name: String,
        err: StageError,
    ) -> Outcome {
        match err {
            StageError::Fatal(msg) => {
                self.fail_run(token, msg).await;
                Outcome::Cancelled
            }
            other => {
                tracing::warn!(%id, %name, error = %other, "asset failed");
                self.emit(Event::AssetFailed {
                    id,
                    name,
                    error: other.to_string(),
                })
                .await;
                Outcome::Failed
            }
        }
    }

    async fn fail_run(&self, token: &CancellationToken, msg: String) {
        if token.is_cancelled() {
            return;
        }
        tracing::error!(error = %msg, "run stopped");
        self.emit(Event::Fatal { error: msg }).await;
        token.cancel();
    }

    async fn stage(&self, id: &str, stage: Stage) {
        self.emit(Event::AssetStage {
            id: id.to_string(),
            stage,
        })
        .await;
    }

    async fn emit(&self, event: Event) {
        let _ = self.events.send(event).await;
    }
}
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test --test engine_test`
Expected: 10 tests pass in under 10 seconds. If `pause_stops_new_assets_until_resume` is flaky, raise the LLM delay to 400 ms; do not remove the assertion.

- [ ] **Step 6: Lint, format, commit, PR**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/engine.rs src/lib.rs tests/engine_test.rs
git commit -m "feat: add engine with discovery, worker pool, and retries"
git push -u origin task-4-engine
gh pr create --fill --title "feat: engine"
```

---

### Task 5: App state and settings form

**Files:**
- Create: `src/settings.rs`, `src/app.rs`
- Modify: `src/lib.rs` (add `pub mod settings; pub mod app;`)
- Test: unit tests inside both files

**Interfaces:**
- Consumes: `config::Config`, `events::{Action, Command, Event, Key, Stage}` from Task 1. Nothing from Tasks 2 to 4.
- Produces (Task 6 draws these, Task 7 drives them):
  - `settings::{SettingsForm, Field}` with field indexes `IMMICH_URL, IMMICH_KEY, LLM_URL, LLM_KEY, LLM_MODEL, WORKERS, MAX_TOKENS`, `SettingsForm::from_config(&Config)`, `display_value(usize) -> String`, `to_config(&self, base: &Config) -> Result<Config, String>`
  - `app::{App, Screen, RunState, InFlight, LogRow, LOG_CAP, RATE_WINDOW}`
  - `App::new(config: Config, first_run: bool) -> App`
  - `App::on_event(&mut self, Event)`, `App::on_key(&mut self, Key) -> Option<Action>`
  - Read-only helpers: `progress_ratio() -> f64`, `elapsed(now: Instant) -> Duration`, `rate_per_min(now) -> Option<f64>`, `eta(now) -> Option<Duration>`, `avg_llm() -> Option<Duration>`, `avg_total() -> Option<Duration>`, `state_label() -> &str`, `immich_host() -> String`, `llm_host() -> String`
  - Public fields listed in the struct below. `ui` reads them directly.

- [ ] **Step 1: Branch**

```bash
git checkout main && git pull
git checkout -b task-5-app-state
```

- [ ] **Step 2: Write the failing settings tests**

Create `src/settings.rs` with the test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn base() -> Config {
        let mut c = Config::default();
        c.immich.url = "https://photos.home.lan".into();
        c.immich.api_key = "secret-key".into();
        c.llm.model = "gemma".into();
        c.llm.prompt = "custom prompt".into();
        c.run.workers = 2;
        c
    }

    #[test]
    fn from_config_fills_all_fields() {
        let f = SettingsForm::from_config(&base());
        assert_eq!(f.fields.len(), 7);
        assert_eq!(f.fields[IMMICH_URL].value, "https://photos.home.lan");
        assert_eq!(f.fields[IMMICH_KEY].value, "secret-key");
        assert_eq!(f.fields[LLM_URL].value, "http://localhost:1234/v1");
        assert_eq!(f.fields[LLM_KEY].value, "");
        assert_eq!(f.fields[LLM_MODEL].value, "gemma");
        assert_eq!(f.fields[WORKERS].value, "2");
        assert_eq!(f.fields[MAX_TOKENS].value, "200");
        assert_eq!(f.focused, 0);
    }

    #[test]
    fn secrets_are_masked_until_revealed() {
        let mut f = SettingsForm::from_config(&base());
        assert_eq!(f.display_value(IMMICH_KEY), "••••••••••");
        assert_eq!(f.display_value(IMMICH_URL), "https://photos.home.lan");
        f.toggle_secrets();
        assert_eq!(f.display_value(IMMICH_KEY), "secret-key");
    }

    #[test]
    fn focus_wraps_both_ways() {
        let mut f = SettingsForm::from_config(&base());
        f.focus_prev();
        assert_eq!(f.focused, MAX_TOKENS);
        f.focus_next();
        assert_eq!(f.focused, IMMICH_URL);
    }

    #[test]
    fn edits_apply_to_the_focused_field() {
        let mut f = SettingsForm::from_config(&base());
        f.focused = LLM_MODEL;
        f.backspace();
        f.insert('X');
        assert_eq!(f.fields[LLM_MODEL].value, "gemmX");
    }

    #[test]
    fn to_config_keeps_file_only_values() {
        let mut f = SettingsForm::from_config(&base());
        f.fields[WORKERS].value = "4".into();
        f.fields[MAX_TOKENS].value = "300".into();
        let cfg = f.to_config(&base()).unwrap();
        assert_eq!(cfg.run.workers, 4);
        assert_eq!(cfg.llm.max_tokens, 300);
        assert_eq!(cfg.llm.prompt, "custom prompt");
        assert_eq!(cfg.run.retries, 3);
    }

    #[test]
    fn to_config_reports_bad_numbers_and_invalid_values() {
        let mut f = SettingsForm::from_config(&base());
        f.fields[WORKERS].value = "many".into();
        assert!(f.to_config(&base()).unwrap_err().contains("workers"));

        let mut f = SettingsForm::from_config(&base());
        f.fields[WORKERS].value = "0".into();
        assert!(f.to_config(&base()).unwrap_err().contains("workers"));

        let mut f = SettingsForm::from_config(&base());
        f.fields[IMMICH_URL].value = "nope".into();
        assert!(f.to_config(&base()).unwrap_err().contains("immich.url"));
    }
}
```

- [ ] **Step 3: Write the settings form above the tests**

```rust
//! The settings form: field values, focus, edits, and conversion to a `Config`.

use crate::config::Config;

pub const IMMICH_URL: usize = 0;
pub const IMMICH_KEY: usize = 1;
pub const LLM_URL: usize = 2;
pub const LLM_KEY: usize = 3;
pub const LLM_MODEL: usize = 4;
pub const WORKERS: usize = 5;
pub const MAX_TOKENS: usize = 6;
const FIELD_COUNT: usize = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub label: &'static str,
    pub value: String,
    pub secret: bool,
}

/// Outcome of the last connection test, one entry per server.
pub type TestResult = (Result<String, String>, Result<String, String>);

#[derive(Debug, Clone, PartialEq)]
pub struct SettingsForm {
    pub fields: Vec<Field>,
    pub focused: usize,
    pub show_secrets: bool,
    pub testing: bool,
    pub test_result: Option<TestResult>,
    /// Validation error or a short status line shown under the form.
    pub message: Option<String>,
}

impl SettingsForm {
    pub fn from_config(cfg: &Config) -> Self {
        let field = |label, value: String, secret| Field { label, value, secret };
        let fields = vec![
            field("immich url", cfg.immich.url.clone(), false),
            field("immich api key", cfg.immich.api_key.clone(), true),
            field("llm base url", cfg.llm.base_url.clone(), false),
            field("llm api key", cfg.llm.api_key.clone(), true),
            field("llm model", cfg.llm.model.clone(), false),
            field("workers", cfg.run.workers.to_string(), false),
            field("max tokens", cfg.llm.max_tokens.to_string(), false),
        ];
        debug_assert_eq!(fields.len(), FIELD_COUNT);
        Self {
            fields,
            focused: 0,
            show_secrets: false,
            testing: false,
            test_result: None,
            message: None,
        }
    }

    pub fn is_last_focused(&self) -> bool {
        self.focused + 1 == FIELD_COUNT
    }

    pub fn focus_next(&mut self) {
        self.focused = (self.focused + 1) % FIELD_COUNT;
    }

    pub fn focus_prev(&mut self) {
        self.focused = (self.focused + FIELD_COUNT - 1) % FIELD_COUNT;
    }

    pub fn insert(&mut self, c: char) {
        self.fields[self.focused].value.push(c);
    }

    pub fn backspace(&mut self) {
        self.fields[self.focused].value.pop();
    }

    pub fn toggle_secrets(&mut self) {
        self.show_secrets = !self.show_secrets;
    }

    /// The text to draw for a field. Secrets show as dots unless revealed.
    pub fn display_value(&self, index: usize) -> String {
        let f = &self.fields[index];
        if f.secret && !self.show_secrets {
            "•".repeat(f.value.chars().count())
        } else {
            f.value.clone()
        }
    }

    /// Applies the fields onto a copy of `base`. File-only values stay as they are.
    pub fn to_config(&self, base: &Config) -> Result<Config, String> {
        let mut cfg = base.clone();
        cfg.immich.url = self.fields[IMMICH_URL].value.trim().to_string();
        cfg.immich.api_key = self.fields[IMMICH_KEY].value.trim().to_string();
        cfg.llm.base_url = self.fields[LLM_URL].value.trim().to_string();
        cfg.llm.api_key = self.fields[LLM_KEY].value.trim().to_string();
        cfg.llm.model = self.fields[LLM_MODEL].value.trim().to_string();
        cfg.run.workers = self.fields[WORKERS]
            .value
            .trim()
            .parse()
            .map_err(|_| "workers must be a whole number".to_string())?;
        cfg.llm.max_tokens = self.fields[MAX_TOKENS]
            .value
            .trim()
            .parse()
            .map_err(|_| "max tokens must be a whole number".to_string())?;
        cfg.validate().map_err(|e| e.to_string())?;
        Ok(cfg)
    }
}
```

- [ ] **Step 4: Run the settings tests**

Run: `cargo test settings`
Expected: 6 tests pass. Add `pub mod settings;` and `pub mod app;` to `src/lib.rs` first. The `app` module does not exist yet, so create `src/app.rs` with the tests from the next step before running.

- [ ] **Step 5: Write the failing app tests**

Create `src/app.rs` with the test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
        a.on_event(Event::PageLoaded { scanned: 10, queued: 4 });
        a.on_event(Event::AssetStarted { id: "1".into(), name: "1".into() });
        assert_eq!(a.in_flight.len(), 1);
        a.on_event(Event::AssetStage { id: "1".into(), stage: Stage::Writing });
        assert_eq!(a.in_flight[0].stage, Stage::Writing);
        a.on_event(done("1"));
        a.on_event(Event::AssetStarted { id: "2".into(), name: "2".into() });
        a.on_event(Event::AssetFailed { id: "2".into(), name: "2".into(), error: "boom".into() });
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
        a.on_event(Event::PageLoaded { scanned: 10, queued: 4 });
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
        assert!(matches!(&a.log[0], LogRow::Done { name, .. } if name == &(LOG_CAP + 24).to_string()));
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
        // 19 intervals of 3 s = 57 s for 19 completions = 20 per minute.
        assert!((a.rate_per_min(now).unwrap() - 20.0).abs() < 1e-6);
        // 80 left at 20 per minute = 4 minutes.
        assert_eq!(a.eta(now), Some(Duration::from_secs(240)));
        assert_eq!(app().rate_per_min(now), None);
    }

    #[test]
    fn start_and_pause_keys() {
        let mut a = app();
        assert_eq!(a.on_key(Key::Char('p')), None, "pause does nothing when idle");
        assert_eq!(a.on_key(Key::Char('s')), Some(Action::Send(Command::Start)));
        assert_eq!(a.run_state, RunState::Running);
        assert_eq!(a.on_key(Key::Char('s')), None, "start does nothing while running");
        assert_eq!(a.on_key(Key::Char('p')), Some(Action::Send(Command::Pause)));
        assert_eq!(a.run_state, RunState::Paused);
        assert_eq!(a.on_key(Key::Char('p')), Some(Action::Send(Command::Resume)));
        assert_eq!(a.run_state, RunState::Running);
    }

    #[test]
    fn start_resets_counters_but_keeps_log() {
        let mut a = app();
        a.on_event(Event::PageLoaded { scanned: 5, queued: 5 });
        a.on_event(done("1"));
        a.on_event(Event::RunFinished { done: 1, failed: 0, elapsed: Duration::from_secs(9) });
        assert_eq!(a.run_state, RunState::Finished);
        a.on_key(Key::Char('s'));
        assert_eq!((a.scanned, a.queued, a.done), (0, 0, 0));
        assert_eq!(a.log.len(), 1);
    }

    #[test]
    fn fatal_sets_error_state() {
        let mut a = app();
        a.on_key(Key::Char('s'));
        a.on_event(Event::Fatal { error: "HTTP 401".into() });
        assert_eq!(a.run_state, RunState::Error("HTTP 401".into()));
        assert_eq!(a.state_label(), "ERROR");
        assert_eq!(a.on_key(Key::Char('s')), Some(Action::Send(Command::Start)), "start again after an error");
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
        assert_eq!(a.log_selected, 2, "highlight follows its row when a new row arrives");
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
        assert_eq!(a.on_key(Key::CtrlC), Some(Action::Quit), "ctrl-c quits from settings too");
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
        assert_eq!(a.settings.fields[crate::settings::LLM_MODEL].value, "gemma!");
        a.on_key(Key::Backspace);
        assert_eq!(a.on_key(Key::Esc), None);
        assert_eq!(a.screen, Screen::Run);
        assert_eq!(a.config.llm.model, "gemma", "esc discards edits");
    }

    #[test]
    fn save_returns_config_and_goes_back() {
        let mut a = app();
        a.on_key(Key::Char('c'));
        a.settings.fields[crate::settings::WORKERS].value = "3".into();
        let action = a.on_key(Key::CtrlS);
        match action {
            Some(Action::SaveConfig(cfg)) => assert_eq!(cfg.run.workers, 3),
            other => panic!("{other:?}"),
        }
        assert_eq!(a.screen, Screen::Run);
        assert_eq!(a.config.run.workers, 3);
        assert_eq!(a.footer_message.as_deref(), Some("settings saved"));
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
        assert!(matches!(a.on_key(Key::CtrlT), Some(Action::TestConnections(_))));
        assert!(a.settings.testing);
        a.on_event(Event::ConnectionTest { immich: Ok("v3.1.0".into()), llm: Err("HTTP 401".into()) });
        assert!(!a.settings.testing);
        let (i, l) = a.settings.test_result.clone().unwrap();
        assert_eq!(i, Ok("v3.1.0".into()));
        assert_eq!(l, Err("HTTP 401".into()));
    }

    #[test]
    fn hosts_for_the_header() {
        let a = app();
        assert_eq!(a.immich_host(), "photos.home.lan");
        assert_eq!(a.llm_host(), "localhost:1234");
    }
}
```

- [ ] **Step 6: Run the app tests to see them fail**

Run: `cargo test app`
Expected: compile errors, `App` not found.

- [ ] **Step 7: Write the app state above the tests**

```rust
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
}

impl App {
    pub fn new(config: Config, first_run: bool) -> Self {
        let settings = SettingsForm::from_config(&config);
        Self {
            config,
            screen: if first_run { Screen::Settings } else { Screen::Run },
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
        }
    }

    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::PageLoaded { scanned, queued } => {
                self.scanned = scanned;
                self.queued = queued;
            }
            Event::DiscoveryDone { total_queued } => self.queued = total_queued,
            Event::AssetStarted { id, name } => self.in_flight.push(InFlight {
                id,
                name,
                stage: Stage::Fetching,
                started_at: Instant::now(),
            }),
            Event::AssetStage { id, stage } => {
                if let Some(f) = self.in_flight.iter_mut().find(|f| f.id == id) {
                    f.stage = stage;
                }
            }
            Event::AssetDone {
                id,
                name,
                description,
                took,
                llm_took,
            } => {
                self.in_flight.retain(|f| f.id != id);
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
                self.in_flight.retain(|f| f.id != id);
                self.failed += 1;
                self.push_log(LogRow::Failed {
                    at: timestamp(),
                    name,
                    error,
                });
            }
            Event::RunFinished { done, failed, elapsed } => {
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
            Event::ConnectionTest { immich, llm } => {
                self.settings.testing = false;
                self.settings.test_result = Some((immich, llm));
            }
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
                Ok(cfg) => {
                    self.settings.testing = true;
                    self.settings.test_result = None;
                    self.settings.message = None;
                    Some(Action::TestConnections(cfg))
                }
                Err(msg) => {
                    self.settings.message = Some(msg);
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
            Ok(cfg) => {
                self.config = cfg.clone();
                // main restarts the engine with the new config, so any paused run is gone.
                self.run_state = RunState::Idle;
                self.in_flight.clear();
                self.screen = Screen::Run;
                self.footer_message = Some("settings saved".into());
                Some(Action::SaveConfig(cfg))
            }
            Err(msg) => {
                self.settings.message = Some(msg);
                None
            }
        }
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

    fn push_log(&mut self, row: LogRow) {
        self.log.push_front(row);
        if self.log.len() > LOG_CAP {
            self.log.pop_back();
        }
        // Keep the highlight on the row the user chose, unless they sit at the top.
        if self.log_selected > 0 && self.log_selected + 1 < self.log.len() {
            self.log_selected += 1;
        }
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
            (Some(e), _) => e,
            (None, Some(s)) => now.saturating_duration_since(s),
            (None, None) => Duration::ZERO,
        }
    }

    /// Completions per minute over the last `RATE_WINDOW` results.
    pub fn rate_per_min(&self, _now: Instant) -> Option<f64> {
        let (first, last) = (self.recent.front()?, self.recent.back()?);
        if self.recent.len() < 2 {
            return None;
        }
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
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            let host = u.host_str()?.to_string();
            Some(match u.port() {
                Some(p) => format!("{host}:{p}"),
                None => host,
            })
        })
        .unwrap_or_else(|| url.to_string())
}

fn timestamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}
```

- [ ] **Step 8: Run all tests to see them pass**

Run: `cargo test app settings`
Expected: 17 app tests and 6 settings tests pass. Note `rate_and_eta_use_recent_completions` sets `queued` and `done` by hand; `eta` uses `queued - done - failed`.

- [ ] **Step 9: Lint, format, commit, PR**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/app.rs src/settings.rs src/lib.rs
git commit -m "feat: add pure app state and settings form"
git push -u origin task-5-app-state
gh pr create --fill --title "feat: app state and settings form"
```

---

### Task 6: Theme and UI

**Files:**
- Create: `src/theme.rs`, `src/ui/mod.rs`, `src/ui/run.rs`, `src/ui/settings.rs`, `tests/ui_snapshots.rs`
- Modify: `src/lib.rs` (add `pub mod theme; pub mod ui;`)

**Interfaces:**
- Consumes: `app::{App, Screen, RunState, InFlight, LogRow}`, `settings::SettingsForm` and the field index constants from Task 5. `config::ThemeName` from Task 1. `events::Stage::label()`.
- Produces:
  - `theme::Theme` with `Theme::from_name(ThemeName) -> Theme`, `Theme::btop()`, `Theme::mono()`, `fn state_style(&self, label: &str) -> Style`, `fn bar_color(&self, t: f64) -> Color`
  - `ui::render(frame: &mut Frame, app: &App, now: Instant, theme: &Theme)`
  - `ui::{fmt_clock(Duration) -> String, fmt_secs(Duration) -> String, fmt_count(u64) -> String}`

Ratatui 0.30 imports used below: `ratatui::{Frame, Terminal}`, `ratatui::backend::TestBackend`, `ratatui::layout::{Constraint, Flex, Layout, Rect}`, `ratatui::style::{Color, Modifier, Style}`, `ratatui::text::{Line, Span}`, `ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap}`.

- [ ] **Step 1: Branch**

```bash
git checkout main && git pull
git checkout -b task-6-theme-ui
```

- [ ] **Step 2: Write `src/theme.rs`**

```rust
//! Colors in one place. `btop` is the default look, `mono` turns color off.

use ratatui::style::{Color, Modifier, Style};

use crate::config::ThemeName;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub border: Style,
    pub title: Style,
    pub label: Style,
    pub value: Style,
    pub ok: Style,
    pub err: Style,
    pub warn: Style,
    pub info: Style,
    pub accent: Style,
    pub name: Style,
    pub duration: Style,
    pub dim: Style,
    pub highlight: Style,
    pub bar_empty: Style,
    gradient: bool,
}

impl Theme {
    pub fn from_name(name: ThemeName) -> Self {
        match name {
            ThemeName::Btop => Self::btop(),
            ThemeName::Mono => Self::mono(),
        }
    }

    pub fn btop() -> Self {
        let dim = Style::default().fg(Color::Indexed(243));
        Self {
            border: Style::default().fg(Color::Indexed(240)),
            title: Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            label: dim,
            value: Style::default().fg(Color::White),
            ok: Style::default().fg(Color::Indexed(114)),
            err: Style::default().fg(Color::Indexed(203)),
            warn: Style::default().fg(Color::Indexed(221)),
            info: Style::default().fg(Color::Indexed(81)),
            accent: Style::default().fg(Color::Indexed(176)),
            name: Style::default().fg(Color::Indexed(81)),
            duration: Style::default().fg(Color::Indexed(221)),
            dim,
            highlight: Style::default().add_modifier(Modifier::REVERSED),
            bar_empty: Style::default().fg(Color::Indexed(238)),
            gradient: true,
        }
    }

    pub fn mono() -> Self {
        let plain = Style::default();
        Self {
            border: plain,
            title: plain.add_modifier(Modifier::BOLD),
            label: plain,
            value: plain,
            ok: plain,
            err: plain,
            warn: plain,
            info: plain,
            accent: plain.add_modifier(Modifier::BOLD),
            name: plain,
            duration: plain,
            dim: plain.add_modifier(Modifier::DIM),
            highlight: plain.add_modifier(Modifier::REVERSED),
            bar_empty: plain,
            gradient: false,
        }
    }

    /// Style for the run-state word in the header.
    pub fn state_style(&self, label: &str) -> Style {
        match label {
            "RUNNING" => self.ok,
            "PAUSED" => self.warn,
            "ERROR" => self.err,
            _ => self.info,
        }
        .add_modifier(Modifier::BOLD)
    }

    /// Color of one bar cell at position `t` in 0.0..=1.0, green through yellow to red.
    pub fn bar_color(&self, t: f64) -> Color {
        if !self.gradient {
            return Color::Reset;
        }
        const STOPS: [u8; 10] = [46, 82, 118, 154, 190, 226, 220, 214, 208, 196];
        let i = ((t.clamp(0.0, 1.0)) * (STOPS.len() - 1) as f64).round() as usize;
        Color::Indexed(STOPS[i])
    }
}
```

- [ ] **Step 3: Write `src/ui/mod.rs`**

```rust
//! Draws `App`. Render only, no state changes.

mod run;
mod settings;

use std::time::{Duration, Instant};

use ratatui::Frame;

use crate::app::{App, Screen};
use crate::theme::Theme;

/// Draws the current screen into the whole frame.
pub fn render(frame: &mut Frame, app: &App, now: Instant, theme: &Theme) {
    match app.screen {
        Screen::Run => run::render(frame, app, now, theme),
        Screen::Settings => settings::render(frame, app, theme),
    }
}

/// `HH:MM:SS`.
pub fn fmt_clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// `4.3 s`.
pub fn fmt_secs(d: Duration) -> String {
    format!("{:.1} s", d.as_secs_f64())
}

/// `1 284`, groups of three separated by a space.
pub fn fmt_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Cuts `s` to `max` cells and appends `…` when it had to cut.
pub fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(fmt_clock(Duration::from_secs(6137)), "01:42:17");
        assert_eq!(fmt_secs(Duration::from_millis(4321)), "4.3 s");
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1284), "1 284");
        assert_eq!(fmt_count(14920), "14 920");
        assert_eq!(fmt_count(1_000_000), "1 000 000");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("x", 0), "");
    }
}
```

- [ ] **Step 4: Write `src/ui/run.rs`**

```rust
//! The run screen: header, progress, counters, in-flight, log, popup, footer.

use std::time::Instant;

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::{fmt_clock, fmt_count, fmt_secs, truncate};
use crate::app::{App, LogRow, RunState};
use crate::theme::Theme;

pub fn render(frame: &mut Frame, app: &App, now: Instant, theme: &Theme) {
    let area = frame.area();
    let outer = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border)
        .title(header_left(app, theme))
        .title(header_right(app, theme).right_aligned());
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    if inner.height < 8 {
        render_tiny(frame, inner, app, now, theme);
        return;
    }

    let stacked = inner.width < 80;
    let show_in_flight = inner.height >= 22;
    let top_height = if stacked { 9 } else { 5 };
    let in_flight_height = if show_in_flight {
        app.config.run.workers.max(1) as u16 + 2
    } else {
        0
    };
    let [top, in_flight, log, footer] = Layout::vertical([
        Constraint::Length(top_height),
        Constraint::Length(in_flight_height),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(inner);

    if stacked {
        let [p, c] = Layout::vertical([Constraint::Length(4), Constraint::Length(5)]).areas(top);
        render_progress(frame, p, app, now, theme);
        render_counters(frame, c, app, theme);
    } else {
        let [p, c] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(top);
        render_progress(frame, p, app, now, theme);
        render_counters(frame, c, app, theme);
    }
    if show_in_flight {
        render_in_flight(frame, in_flight, app, now, theme);
    }
    render_log(frame, log, app, theme);
    render_footer(frame, footer, app, theme);
    if app.log_expanded {
        render_popup(frame, area, app, theme);
    }
}

fn header_left(app: &App, theme: &Theme) -> Line<'static> {
    let workers = app.config.run.workers;
    Line::from(vec![
        Span::styled(" immich-alt-text ", theme.title),
        Span::styled("─ ", theme.border),
        Span::styled(app.immich_host(), theme.value),
        Span::styled(" ─ ", theme.border),
        Span::styled(app.config.llm.model.clone(), theme.value),
        Span::styled(" @ ", theme.dim),
        Span::styled(app.llm_host(), theme.value),
        Span::styled(" ─ ", theme.border),
        Span::styled(
            format!("{workers} worker{} ", if workers == 1 { "" } else { "s" }),
            theme.value,
        ),
    ])
}

fn header_right(app: &App, theme: &Theme) -> Line<'static> {
    let label = app.state_label();
    let mut spans = vec![Span::styled(format!(" {label} "), theme.state_style(label))];
    if let RunState::Error(msg) = &app.run_state {
        spans.insert(0, Span::styled(format!(" {} ", truncate(msg, 40)), theme.err));
    }
    Line::from(spans)
}

fn boxed<'a>(title: &'a str, theme: &Theme) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border)
        .title(Span::styled(format!(" {title} "), theme.title))
}

fn bar(width: usize, ratio: f64, theme: &Theme) -> Vec<Span<'static>> {
    let filled = (ratio.clamp(0.0, 1.0) * width as f64).round() as usize;
    (0..width)
        .map(|i| {
            if i < filled {
                let t = i as f64 / width.max(1) as f64;
                Span::styled("█", Style::default().fg(theme.bar_color(t)))
            } else {
                Span::styled("░", theme.bar_empty)
            }
        })
        .collect()
}

fn render_progress(frame: &mut Frame, area: Rect, app: &App, now: Instant, theme: &Theme) {
    let block = boxed("progress", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let counts = format!(" {} / {}", fmt_count(app.done + app.failed), fmt_count(app.queued));
    let bar_width = (inner.width as usize).saturating_sub(counts.chars().count() + 1);
    let mut line1 = bar(bar_width, app.progress_ratio(), theme);
    line1.push(Span::styled(counts, theme.value));
    let rate = app
        .rate_per_min(now)
        .map(|r| format!("{r:.1}/min"))
        .unwrap_or_else(|| "--".into());
    let eta = app.eta(now).map(fmt_clock).unwrap_or_else(|| "--:--:--".into());
    let line2 = Line::from(vec![
        Span::styled("elapsed ", theme.label),
        Span::styled(fmt_clock(app.elapsed(now)), theme.value),
        Span::styled("   rate ", theme.label),
        Span::styled(rate, theme.value),
        Span::styled("   eta ", theme.label),
        Span::styled(eta, theme.value),
    ]);
    frame.render_widget(Paragraph::new(vec![Line::from(line1), line2]), inner);
}

fn render_counters(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let block = boxed("counters", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let pair = |l1: &str, v1: String, l2: &str, v2: String, s2: Style| {
        Line::from(vec![
            Span::styled(format!("{l1:<9}"), theme.label),
            Span::styled(format!("{v1:>8}"), theme.value),
            Span::styled(format!("   {l2:<9}"), theme.label),
            Span::styled(format!("{v2:>8}"), s2),
        ])
    };
    let avg = |d: Option<std::time::Duration>| d.map(fmt_secs).unwrap_or_else(|| "--".into());
    let failed_style = if app.failed > 0 { theme.err } else { theme.value };
    let lines = vec![
        pair("scanned", fmt_count(app.scanned), "done", fmt_count(app.done), theme.value),
        pair("queued", fmt_count(app.queued), "failed", fmt_count(app.failed), failed_style),
        pair("avg llm", avg(app.avg_llm()), "avg total", avg(app.avg_total()), theme.value),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_in_flight(frame: &mut Frame, area: Rect, app: &App, now: Instant, theme: &Theme) {
    let block = boxed("in flight", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines: Vec<Line> = app
        .in_flight
        .iter()
        .map(|f| {
            Line::from(vec![
                Span::styled("● ", theme.accent),
                Span::styled(format!("{:<20}", truncate(&f.name, 20)), theme.name),
                Span::styled(format!("{:<14}", format!("{}…", f.stage.label())), theme.info),
                Span::styled(fmt_secs(now.saturating_duration_since(f.started_at)), theme.duration),
            ])
        })
        .collect();
    if lines.is_empty() {
        let text = match app.run_state {
            RunState::Idle => "press s to start",
            RunState::Paused => "paused",
            RunState::Finished => "finished",
            RunState::Error(_) => "stopped",
            RunState::Running => "waiting for the first asset…",
        };
        lines.push(Line::from(Span::styled(text, theme.dim)));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn log_line(row: &LogRow, width: usize, theme: &Theme) -> Line<'static> {
    // Fixed columns: time(8) + 2 + mark(1) + 1 + name(16) + 2 + took(6) + 2 = 38.
    let text_width = width.saturating_sub(38);
    match row {
        LogRow::Done { at, name, took, description } => Line::from(vec![
            Span::styled(at.clone(), theme.dim),
            Span::styled("  ✓ ", theme.ok),
            Span::styled(format!("{:<16}", truncate(name, 16)), theme.name),
            Span::styled(format!("  {:>6}", fmt_secs(*took)), theme.duration),
            Span::styled(format!("  {}", truncate(description, text_width)), theme.value),
        ]),
        LogRow::Failed { at, name, error } => Line::from(vec![
            Span::styled(at.clone(), theme.dim),
            Span::styled("  ✗ ", theme.err),
            Span::styled(format!("{:<16}", truncate(name, 16)), theme.name),
            Span::styled("        ", theme.dim),
            Span::styled(format!("  {}", truncate(error, text_width)), theme.err),
        ]),
    }
}

fn render_log(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let block = boxed("log", theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let items: Vec<ListItem> = app
        .log
        .iter()
        .map(|row| ListItem::new(log_line(row, inner.width as usize, theme)))
        .collect();
    let mut state = ListState::default();
    if !app.log.is_empty() {
        state.select(Some(app.log_selected.min(app.log.len() - 1)));
    }
    let list = List::new(items).highlight_style(theme.highlight);
    frame.render_stateful_widget(list, inner, &mut state);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let can_start = matches!(app.run_state, RunState::Idle | RunState::Finished | RunState::Error(_));
    let pause_label = if app.run_state == RunState::Paused { "resume" } else { "pause" };
    let can_pause = matches!(app.run_state, RunState::Running | RunState::Paused);
    let key = |k: &str, label: &str, enabled: bool| {
        let (ks, ls) = if enabled { (theme.accent, theme.label) } else { (theme.dim, theme.dim) };
        vec![Span::styled(format!(" {k} "), ks), Span::styled(format!("{label}   "), ls)]
    };
    let mut spans = Vec::new();
    spans.extend(key("s", "start", can_start));
    spans.extend(key("p", pause_label, can_pause));
    spans.extend(key("↑↓", "scroll log", !app.log.is_empty()));
    spans.extend(key("enter", "expand", !app.log.is_empty()));
    spans.extend(key("c", "settings", true));
    spans.extend(key("q", "quit", true));
    if let Some(msg) = &app.footer_message {
        spans.push(Span::styled(format!("  {msg}"), theme.info));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_popup(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let Some(row) = app.log.get(app.log_selected) else { return };
    let [v] = Layout::vertical([Constraint::Percentage(50)]).flex(Flex::Center).areas(area);
    let [popup] = Layout::horizontal([Constraint::Percentage(70)]).flex(Flex::Center).areas(v);
    let (title, body, style) = match row {
        LogRow::Done { name, description, .. } => (name.clone(), description.clone(), theme.value),
        LogRow::Failed { name, error, .. } => (name.clone(), error.clone(), theme.err),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.accent)
        .title(Span::styled(format!(" {title} "), theme.title))
        .title(Span::styled(" esc close ", theme.dim).into_right_aligned_line());
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(Span::styled(body, style)).wrap(Wrap { trim: true }), inner);
}

fn render_tiny(frame: &mut Frame, area: Rect, app: &App, now: Instant, theme: &Theme) {
    let [bar_area, info, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(area);
    let counts = format!(" {} / {}", fmt_count(app.done + app.failed), fmt_count(app.queued));
    let mut spans = bar((bar_area.width as usize).saturating_sub(counts.len() + 1), app.progress_ratio(), theme);
    spans.push(Span::styled(counts, theme.value));
    frame.render_widget(Paragraph::new(Line::from(spans)), bar_area);
    let line = Line::from(vec![
        Span::styled("elapsed ", theme.label),
        Span::styled(fmt_clock(app.elapsed(now)), theme.value),
        Span::styled(format!("   failed {}", app.failed), if app.failed > 0 { theme.err } else { theme.label }),
    ]);
    frame.render_widget(Paragraph::new(line), info);
    render_footer(frame, footer, app, theme);
}
```

Note for the implementer: `Layout::areas` returns a fixed-size array and panics if the constraint count differs from the array length. Keep the destructuring patterns exactly as written.

- [ ] **Step 5: Write `src/ui/settings.rs`**

```rust
//! The settings form screen.

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;

use super::truncate;
use crate::app::App;
use crate::settings::{IMMICH_KEY, LLM_KEY};
use crate::theme::Theme;

pub fn render(frame: &mut Frame, app: &App, theme: &Theme) {
    let area = frame.area();
    let form = &app.settings;
    let width = area.width.clamp(40, 70);
    let height = (form.fields.len() as u16 + 8).min(area.height);
    let [v] = Layout::vertical([Constraint::Length(height)]).flex(Flex::Center).areas(area);
    let [boxed] = Layout::horizontal([Constraint::Length(width)]).flex(Flex::Center).areas(v);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border)
        .title(Span::styled(" settings ", theme.title));
    let inner = block.inner(boxed);
    frame.render_widget(block, boxed);

    let value_width = (inner.width as usize).saturating_sub(19 + 13);
    let mut lines: Vec<Line> = Vec::new();
    for (i, field) in form.fields.iter().enumerate() {
        let focused = i == form.focused;
        let mut value = truncate(&form.display_value(i), value_width);
        if focused {
            value.push('▏');
        }
        let value_style = if focused { theme.accent } else { theme.value };
        let mut spans = vec![
            Span::styled(if focused { "▸ " } else { "  " }, theme.accent),
            Span::styled(format!("{:<17}", field.label), theme.label),
            Span::styled(format!("{value:<width$}", width = value_width + 1), value_style),
        ];
        if i == IMMICH_KEY || i == LLM_KEY {
            let hint = if form.show_secrets { "ctrl-r hide" } else { "ctrl-r show" };
            spans.push(Span::styled(hint, theme.dim));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());
    lines.push(test_line(app, theme));
    lines.push(match &form.message {
        Some(msg) => Line::from(Span::styled(format!("  {msg}"), theme.err)),
        None => Line::default(),
    });
    frame.render_widget(Paragraph::new(lines), inner);

    let footer = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    let key = |k: &str, label: &str| {
        vec![
            Span::styled(format!(" {k} "), theme.accent),
            Span::styled(format!("{label}   "), theme.label),
        ]
    };
    let mut spans = key("ctrl-s", "save");
    spans.extend(key("ctrl-t", "test"));
    spans.extend(key("esc", "back"));
    frame.render_widget(Paragraph::new(Line::from(spans)), footer);
}

fn test_line(app: &App, theme: &Theme) -> Line<'static> {
    let form = &app.settings;
    let mut spans = vec![
        Span::styled("  ctrl-t ", theme.accent),
        Span::styled("test connections   ", theme.label),
    ];
    if form.testing {
        spans.push(Span::styled("testing…", theme.warn));
        return Line::from(spans);
    }
    if let Some((immich, llm)) = &form.test_result {
        for (label, result) in [("immich", immich), ("llm", llm)] {
            spans.push(Span::styled(format!("{label} "), theme.label));
            match result {
                Ok(text) => {
                    spans.push(Span::styled("✓ ", theme.ok));
                    spans.push(Span::styled(format!("{}   ", truncate(text, 20)), theme.value));
                }
                Err(text) => {
                    spans.push(Span::styled("✗ ", theme.err));
                    spans.push(Span::styled(format!("{}   ", truncate(text, 28)), theme.err));
                }
            }
        }
    }
    Line::from(spans)
}
```

- [ ] **Step 6: Write the snapshot tests**

Create `tests/ui_snapshots.rs`:

```rust
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
    app.run_started = Some(now - Duration::from_secs(6137));
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
fn run_screen_80x24_stacks_counters() {
    let now = Instant::now();
    snapshot("run_80x24", 80, 24, &running_app(now), now);
}

#[test]
fn run_screen_40x10_is_tiny() {
    let now = Instant::now();
    snapshot("run_40x10", 40, 10, &running_app(now), now);
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
```

Add `pub mod theme; pub mod ui;` to `src/lib.rs`.

- [ ] **Step 7: Create the snapshots, then review them by eye**

Run: `INSTA_UPDATE=always cargo test --test ui_snapshots`
Expected: all tests pass and write `tests/snapshots/ui_snapshots__*.snap`.

Open each `.snap` file and check against the mockups in `docs/design.md` section 8:

- `run_120x40`: progress and counters side by side, one in-flight row, four log rows, footer keys. The bar shows `1 287 / 3 102`. Rate reads `12.6/min`. Elapsed reads `01:42:17`.
- `run_80x24`: counters box sits under the progress box. No in-flight box.
- `run_40x10`: only the border, one bar line, one info line, and the footer. Nothing panics.
- `run_error_120x40`: header shows the error text and `ERROR`.
- `run_popup_120x40`: a centered box with the full first description.
- `settings_80x24`: seven rows, the focused row marked with `▸` and a cursor bar, secrets as dots, the test line with `✓ v3.1.0` and `✗ HTTP 401 Unauthorized`.

If a snapshot looks wrong, fix the drawing code and rerun with `INSTA_UPDATE=always`. When they look right, run `cargo test --test ui_snapshots` once more without the variable and confirm it passes.

- [ ] **Step 8: Run the whole suite, lint, commit, PR**

```bash
cargo test
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/theme.rs src/ui tests/ui_snapshots.rs tests/snapshots src/lib.rs
git commit -m "feat: add theme and Ratatui screens with snapshot tests"
git push -u origin task-6-theme-ui
gh pr create --fill --title "feat: theme and UI"
```

---

### Task 7: Main loop, demo servers, README

**Files:**
- Modify: `src/main.rs` (replace the stub), `README.md`
- Create: `examples/fake_servers.rs`

**Interfaces:**
- Consumes everything: `config::{load, save, default_path, state_dir, Config}`, `engine::{spawn, EngineHandle}`, `events::{Action, Event, Key}`, `app::{App, RunState}`, `ui::render`, `theme::Theme`, `immich::ImmichClient`, `llm::LlmClient`.
- Produces the binary. No other task depends on it.

- [ ] **Step 1: Branch**

```bash
git checkout main && git pull
git checkout -b task-7-main
```

- [ ] **Step 2: Write `src/main.rs`**

```rust
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
                    app.footer_message = Some("open settings with c and save a config first".into());
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
    let timed = |fut| async {
        tokio::time::timeout(CONNECTION_TEST_TIMEOUT, fut)
            .await
            .unwrap_or_else(|_| Err("timed out".to_string()))
    };
    let (immich, llm) = tokio::join!(timed(immich), timed(llm));
    let _ = tx.send(Event::ConnectionTest { immich, llm }).await;
}
```

If `timed` fails to compile because the two futures have different types, replace it with two explicit `tokio::time::timeout(...)` calls, one per future, and keep the `unwrap_or_else` mapping.

- [ ] **Step 3: Build and try the binary against nothing**

Run: `cargo build && cargo run -- --config /tmp/iat-empty.toml`
Expected: the settings screen opens with LM Studio defaults. `esc` shows the run screen with `IDLE`. `s` shows the footer message about saving a config. `q` exits and the shell prompt is intact. Delete `/tmp/iat-empty.toml` if it was created.

- [ ] **Step 4: Write `examples/fake_servers.rs`**

```rust
//! Fake Immich and LLM servers for a manual run of the TUI.
//!
//! Terminal 1: `cargo run --example fake_servers`
//! Terminal 2: `cargo run -- --config target/demo-config.toml`

use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::main]
async fn main() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;

    let items: Vec<serde_json::Value> = (1..=40)
        .map(|i| {
            let description = if i % 7 == 0 { Some("already described") } else { None };
            json!({
                "id": format!("asset-{i:03}"),
                "originalFileName": format!("IMG_{:04}.HEIC", 4400 + i),
                "type": "IMAGE",
                "exifInfo": { "description": description }
            })
        })
        .collect();

    Mock::given(method("GET"))
        .and(path("/api/server/version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "major": 3, "minor": 1, "patch": 0 })))
        .mount(&immich)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/search/metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "assets": { "count": items.len(), "total": items.len(), "facets": [], "nextPage": null, "items": items }
        })))
        .mount(&immich)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/assets/[^/]+/thumbnail$"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFF, 0xD8, 0xFF, 0xD9]))
        .mount(&immich)
        .await;
    // A few writes fail so the failed counter and the red log rows show up.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/api/assets/asset-0(09|18|27|36)$"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&immich)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/api/assets/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&immich)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [ { "id": "demo-vision" } ] })))
        .mount(&llm)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(1500))
                .set_body_json(json!({ "choices": [ { "index": 0, "message": {
                    "role": "assistant",
                    "content": "A golden retriever sits on a wooden dock at sunset, looking toward the water."
                } } ] })),
        )
        .mount(&llm)
        .await;

    let config = format!(
        "[immich]\nurl = \"{}\"\napi_key = \"demo\"\ntimeout_secs = 5\n\n\
         [llm]\nbase_url = \"{}/v1\"\nmodel = \"demo-vision\"\ntimeout_secs = 10\n\n\
         [run]\nworkers = 2\nretries = 1\n",
        immich.uri(),
        llm.uri()
    );
    std::fs::create_dir_all("target").expect("create target dir");
    std::fs::write("target/demo-config.toml", config).expect("write demo config");

    println!("fake immich: {}", immich.uri());
    println!("fake llm:    {}", llm.uri());
    println!("config:      target/demo-config.toml");
    println!("now run:     cargo run -- --config target/demo-config.toml");
    println!("ctrl-c stops the servers");
    tokio::signal::ctrl_c().await.expect("ctrl-c handler");
}
```

`expect` is fine here. The example is a dev tool, not the binary.

- [ ] **Step 5: Run the demo and check the screen**

Terminal 1: `cargo run --example fake_servers`
Terminal 2: `cargo run -- --config target/demo-config.toml`

Expected in terminal 2:
- Header shows the fake host, `demo-vision`, `2 workers`, `IDLE`.
- `s` starts. `PageLoaded` sets `scanned 40`, `queued 35`. Two in-flight rows appear with a moving timer. Log rows arrive about every 0.75 s with the dock description. Four rows show red `✗` with `immich: HTTP 500` and `2 tries`.
- `p` pauses: state turns `PAUSED`, in-flight rows finish, no new ones. `p` again resumes.
- `↑` `↓` moves the highlight. `enter` opens the popup. `esc` closes it.
- `c` opens settings. `ctrl-t` shows `immich ✓ v3.1.0` and `llm ✓ 200 OK`. `esc` returns.
- Run ends with `FINISHED`, `done 31`, `failed 4`.
- `q` exits cleanly. `cat target/demo-config.toml` still shows the demo config. The debug log exists under `~/.local/state/immich-alt-text/`.

Fix anything that does not match before going on.

- [ ] **Step 6: Write the README**

Replace `README.md` with:

````markdown
# immich-alt-text

A small terminal app that describes the photos in your [Immich](https://immich.app)
library with a vision model and writes the text back as each photo's description.

Built with Rust and [Ratatui](https://ratatui.rs). Personal project, experimental.

## What it does

1. Lists every image in your Immich library whose description is empty.
2. Downloads the preview JPEG for each one.
3. Sends it to an OpenAI-compatible chat endpoint with a vision model.
4. Writes the returned sentence back to Immich.

Immich keeps the state. A photo with a description is skipped, so you can stop and
start the run at any time. Hand-written descriptions are never touched.

## Requirements

- Rust 1.85 or newer.
- An Immich server and an API key (Account settings → API keys).
- A vision model behind an OpenAI-compatible API. Tested with LM Studio at
  `http://localhost:1234/v1`. Ollama, llama.cpp server, vLLM, OpenRouter, and OpenAI
  work with the same setting.

## Run

```bash
cargo install --path .
immich-alt-text
```

The first launch opens the settings screen. Fill in the Immich URL, the API key, the
LLM base URL, the model name, and press `ctrl-t` to test both connections.
`ctrl-s` saves to `~/.config/immich-alt-text/config.toml` and returns to the run
screen. Press `s` to start.

## Keys

| Screen | Key | Action |
| --- | --- | --- |
| run | `s` | start a run |
| run | `p` | pause or resume |
| run | `↑` `↓` | move through the log |
| run | `enter` | show the full description of the highlighted row |
| run | `c` | open settings |
| run | `q` or `ctrl-c` | quit |
| settings | `tab` `shift-tab` | move between fields |
| settings | `ctrl-r` | show or hide API keys |
| settings | `ctrl-t` | test both connections |
| settings | `ctrl-s` | save and go back |
| settings | `esc` | discard edits and go back |

## Config file

```toml
[immich]
url = "https://photos.home.lan"
api_key = "..."
timeout_secs = 30

[llm]
base_url = "http://localhost:1234/v1"
api_key = ""            # optional
model = "gemma-3-12b-it"
max_tokens = 200
timeout_secs = 120
prompt = """
Write alt text for this photo: one or two plain sentences describing what is
visible. No preamble, no quotes, no "This image shows".
"""

[run]
workers = 1             # parallel LLM calls
retries = 3             # retries after the first try, backoff 2 s, 4 s, 8 s
page_size = 1000

[ui]
theme = "btop"          # or "mono"
```

`prompt`, the timeouts, `retries`, `page_size`, and `theme` are file-only. The
settings screen covers the rest.

## Logs

A debug log goes to `~/.local/state/immich-alt-text/debug.log`. Set `RUST_LOG=debug`
for more detail. Request bodies and keys are never logged.

## Try it without a real library

```bash
cargo run --example fake_servers          # terminal 1
cargo run -- --config target/demo-config.toml   # terminal 2
```

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Design: `docs/design.md`. Plan: `docs/plans/2026-09-04-immich-alt-text.md`.
````

- [ ] **Step 7: Full check, commit, PR**

```bash
cargo test
cargo fmt
cargo clippy --all-targets -- -D warnings
git add src/main.rs examples/fake_servers.rs README.md
git commit -m "feat: wire the terminal loop, add demo servers and README"
git push -u origin task-7-main
gh pr create --fill --title "feat: main loop, demo, README"
```

---

## Done when

- `cargo test` passes on `main` after all seven PRs merge.
- `cargo run --example fake_servers` plus `cargo run -- --config target/demo-config.toml` shows the screens from `docs/design.md` and finishes with `done 31`, `failed 4`.
- A run against a real Immich and LM Studio writes descriptions that show up under the photos in the Immich web UI.
