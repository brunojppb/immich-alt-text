//! Configuration file: structs, defaults, load, save, validation.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Prompt used when the config file does not set `llm.prompt`.
pub const DEFAULT_PROMPT: &str = "Write alt text for this photo: one or two plain sentences describing what is visible. No preamble, no quotes, no \"This image shows\".";
pub const MAX_WORKERS: usize = 64;
pub const MAX_RETRIES: u32 = 10;
pub const MAX_PAGE_SIZE: u32 = 1000;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        .unwrap_or_else(|| {
            fallback
                .iter()
                .fold(home(), |path, segment| path.join(segment))
        })
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
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    let config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;

    Ok(Some(config))
}

/// Validates, then atomically replaces the file with owner-only permissions.
pub fn save(path: &Path, config: &Config) -> Result<(), ConfigError> {
    save_with_checkpoint(path, config, |_| Ok(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveStage {
    Write,
    Flush,
    Sync,
}

fn save_with_checkpoint(
    path: &Path,
    config: &Config,
    mut checkpoint: impl FnMut(SaveStage) -> std::io::Result<()>,
) -> Result<(), ConfigError> {
    config.validate()?;

    let io = |source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    };

    let text = toml::to_string_pretty(config)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(&io)?;

    let (mut file, pending) = create_temp_file(path).map_err(&io)?;
    checkpoint(SaveStage::Write).map_err(&io)?;
    file.write_all(text.as_bytes()).map_err(&io)?;
    checkpoint(SaveStage::Flush).map_err(&io)?;
    file.flush().map_err(&io)?;
    checkpoint(SaveStage::Sync).map_err(&io)?;
    file.sync_all().map_err(&io)?;
    drop(file);
    pending.persist(path).map_err(&io)?;

    Ok(())
}

struct PendingTempFile {
    path: PathBuf,
    persisted: bool,
}

impl PendingTempFile {
    fn persist(mut self, destination: &Path) -> std::io::Result<()> {
        std::fs::rename(&self.path, destination)?;
        self.persisted = true;
        Ok(())
    }
}

impl Drop for PendingTempFile {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn create_temp_file(path: &Path) -> std::io::Result<(File, PendingTempFile)> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "config path must name a file",
        )
    })?;

    for _ in 0..128 {
        let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = OsString::from(".");
        temp_name.push(file_name);
        temp_name.push(format!(".{}.{sequence}.tmp", std::process::id()));
        let temp_path = parent.join(temp_name);

        match create_owner_only_file(&temp_path) {
            Ok(file) => {
                return Ok((
                    file,
                    PendingTempFile {
                        path: temp_path,
                        persisted: false,
                    },
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not create a unique config temporary file",
    ))
}

fn create_owner_only_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let file = options.open(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = file.set_permissions(std::fs::Permissions::from_mode(0o600)) {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
    }

    Ok(file)
}

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(message.into())
}

fn check_url(field: &str, value: &str) -> Result<(), ConfigError> {
    let parsed = url::Url::parse(value).map_err(|error| invalid(format!("{field}: {error}")))?;
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
        if !(1..=MAX_WORKERS).contains(&self.run.workers) {
            return Err(invalid(format!(
                "run.workers must be between 1 and {MAX_WORKERS}"
            )));
        }
        if self.run.retries > MAX_RETRIES {
            return Err(invalid(format!(
                "run.retries must be at most {MAX_RETRIES}"
            )));
        }
        if !(1..=MAX_PAGE_SIZE).contains(&self.run.page_size) {
            return Err(invalid(format!(
                "run.page_size must be between 1 and {MAX_PAGE_SIZE}"
            )));
        }

        Ok(())
    }
}

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
    fn creates_a_new_file_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let file = create_owner_only_file(&path).unwrap();
        drop(file);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn save_keeps_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(&path, &full()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn save_tightens_existing_file_permissions_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        save(&path, &full()).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn failed_write_flush_or_sync_preserves_existing_config() {
        for fail_at in [SaveStage::Write, SaveStage::Flush, SaveStage::Sync] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            let original = b"existing config\n";
            std::fs::write(&path, original).unwrap();

            let result = save_with_checkpoint(&path, &full(), |stage| {
                if stage == fail_at {
                    Err(std::io::Error::other("injected save failure"))
                } else {
                    Ok(())
                }
            });

            assert!(result.is_err(), "{fail_at:?} must fail the save");
            assert_eq!(std::fs::read(&path).unwrap(), original, "{fail_at:?}");
            let entries = std::fs::read_dir(dir.path()).unwrap().count();
            assert_eq!(entries, 1, "temporary file leaked after {fail_at:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn save_stages_owner_only_temp_in_destination_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "existing config\n").unwrap();
        let mut inspected = false;

        save_with_checkpoint(&path, &full(), |stage| {
            if stage == SaveStage::Write {
                let temp = std::fs::read_dir(dir.path())?
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|entry| entry != &path)
                    .ok_or_else(|| std::io::Error::other("temporary file was not staged"))?;
                let mode = std::fs::metadata(&temp)?.permissions().mode() & 0o777;
                if temp.parent() != Some(dir.path()) || mode != 0o600 {
                    return Err(std::io::Error::other(
                        "temporary file was not same-directory and owner-only",
                    ));
                }
                inspected = true;
            }
            Ok(())
        })
        .unwrap();

        assert!(inspected);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
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
    fn rejects_extreme_worker_retry_and_page_limits() {
        let mut cfg = full();
        cfg.run.workers = usize::MAX;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("run.workers"));

        let mut cfg = full();
        cfg.run.retries = u32::MAX;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("run.retries"));

        let mut cfg = full();
        cfg.run.page_size = u32::MAX;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("run.page_size"));
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
