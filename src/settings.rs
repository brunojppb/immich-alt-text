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
        let field = |label, value: String, secret| Field {
            label,
            value,
            secret,
        };
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
        let field = &self.fields[index];
        if field.secret && !self.show_secrets {
            "•".repeat(field.value.chars().count())
        } else {
            field.value.clone()
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
        cfg.validate().map_err(|error| error.to_string())?;
        Ok(cfg)
    }
}

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
