//! The settings form: field values, focus, edits, and conversion to a `Config`.

use std::cell::Cell;

use crate::config::{Config, ThemeName};
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;

pub const IMMICH_URL: usize = 0;
pub const IMMICH_KEY: usize = 1;
pub const IMMICH_TIMEOUT: usize = 2;
pub const IMMICH_TAG: usize = 3;
pub const LLM_URL: usize = 4;
pub const LLM_KEY: usize = 5;
pub const LLM_MODEL: usize = 6;
pub const PROMPT: usize = 7;
pub const LLM_TIMEOUT: usize = 8;
pub const WORKERS: usize = 9;
pub const RETRIES: usize = 10;
pub const MAX_TOKENS: usize = 11;
pub const CONTEXT_PEOPLE: usize = 12;
pub const CONTEXT_PLACE: usize = 13;
pub const CONTEXT_DATE: usize = 14;
pub const THEME: usize = 15;
pub const DRY_RUN: usize = 16;
pub const OVERWRITE: usize = 17;

const FIELD_COUNT: usize = OVERWRITE + 1;
pub const PROMPT_WRAP_WIDTH: usize = 43;

/// Row labels for the context switches, in row order.
pub const CONTEXT_LABELS: [&str; 3] = ["context people", "context place", "context date"];

/// True when `index` is one of the context switch rows.
pub fn is_context_row(index: usize) -> bool {
    (CONTEXT_PEOPLE..=CONTEXT_DATE).contains(&index)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub label: &'static str,
    pub value: String,
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptLayout {
    pub rows: Vec<String>,
    pub row_starts: Vec<usize>,
    pub cursor_row: usize,
    pub cursor_column: usize,
}

/// Outcome of the last connection test, one entry per server.
pub type TestResult = (Result<String, String>, Result<String, String>);

#[derive(Debug, Clone, PartialEq)]
pub struct SettingsForm {
    pub fields: Vec<Field>,
    pub theme: ThemeName,
    pub dry_run: bool,
    pub overwrite: bool,
    /// The three context switches, in row order: people, place, date.
    pub context: [bool; 3],
    /// Cursor position in the prompt, measured in Unicode grapheme clusters.
    pub prompt_cursor: usize,
    prompt_width: Cell<usize>,
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
            field(
                "immich timeout (s)",
                cfg.immich.timeout_secs.to_string(),
                false,
            ),
            field("immich tag", cfg.immich.tag.clone(), false),
            field("llm base url", cfg.llm.base_url.clone(), false),
            field("llm api key", cfg.llm.api_key.clone(), true),
            field("llm model", cfg.llm.model.clone(), false),
            field("prompt", cfg.llm.prompt.clone(), false),
            field("llm timeout (s)", cfg.llm.timeout_secs.to_string(), false),
            field("workers", cfg.run.workers.to_string(), false),
            field("retries", cfg.run.retries.to_string(), false),
            field("max tokens", cfg.llm.max_tokens.to_string(), false),
        ];
        debug_assert_eq!(fields.len() + 6, FIELD_COUNT);
        Self {
            fields,
            theme: cfg.ui.theme,
            dry_run: cfg.run.dry_run,
            overwrite: cfg.run.overwrite,
            context: [
                cfg.llm.context.people,
                cfg.llm.context.place,
                cfg.llm.context.date,
            ],
            prompt_cursor: cfg.llm.prompt.graphemes(true).count(),
            prompt_width: Cell::new(PROMPT_WRAP_WIDTH),
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
        if self.focused == PROMPT {
            self.insert_prompt(c);
        } else if self.focused < self.fields.len() {
            self.fields[self.focused].value.push(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.focused == PROMPT {
            self.backspace_prompt();
        } else if self.focused < self.fields.len() {
            self.fields[self.focused].value.pop();
        }
    }

    pub fn newline(&mut self) {
        if self.focused == PROMPT {
            self.insert_prompt('\n');
        }
    }

    pub fn move_prompt_left(&mut self) {
        if self.focused == PROMPT {
            self.prompt_cursor = self.prompt_cursor.saturating_sub(1);
        }
    }

    pub fn move_prompt_right(&mut self) {
        if self.focused == PROMPT {
            self.prompt_cursor = (self.prompt_cursor + 1).min(self.prompt_len());
        }
    }

    pub fn move_prompt_up(&mut self) {
        if self.focused != PROMPT {
            return;
        }
        let layout = prompt_layout(self.prompt_value(), self.prompt_cursor, self.prompt_width());
        if layout.cursor_row == 0 {
            return;
        }
        let target_row = layout.cursor_row - 1;
        let target_column = layout.rows[target_row].graphemes(true).count();
        self.prompt_cursor =
            layout.row_starts[target_row] + layout.cursor_column.min(target_column);
    }

    pub fn move_prompt_down(&mut self) {
        if self.focused != PROMPT {
            return;
        }
        let layout = prompt_layout(self.prompt_value(), self.prompt_cursor, self.prompt_width());
        if layout.cursor_row + 1 == layout.rows.len() {
            return;
        }
        let target_row = layout.cursor_row + 1;
        let target_column = layout.rows[target_row].graphemes(true).count();
        self.prompt_cursor =
            layout.row_starts[target_row] + layout.cursor_column.min(target_column);
    }

    /// Clears the focused text field. The theme, dry run, and context switch rows are
    /// intentionally not text-editable.
    pub fn clear(&mut self) {
        if self.focused == PROMPT {
            self.fields[PROMPT].value.clear();
            self.prompt_cursor = 0;
        } else if self.focused < self.fields.len() {
            self.fields[self.focused].value.clear();
        }
    }

    fn prompt_value(&self) -> &str {
        &self.fields[PROMPT].value
    }

    fn prompt_len(&self) -> usize {
        self.prompt_value().graphemes(true).count()
    }

    fn insert_prompt(&mut self, c: char) {
        let byte_index = grapheme_to_byte(self.prompt_value(), self.prompt_cursor);
        self.fields[PROMPT].value.insert(byte_index, c);
        let inserted_end = byte_index + c.len_utf8();
        self.prompt_cursor = self
            .prompt_value()
            .grapheme_indices(true)
            .position(|(start, _)| start >= inserted_end)
            .unwrap_or_else(|| self.prompt_len());
    }

    fn backspace_prompt(&mut self) {
        if self.prompt_cursor == 0 {
            return;
        }
        let end = grapheme_to_byte(self.prompt_value(), self.prompt_cursor);
        let start = grapheme_to_byte(self.prompt_value(), self.prompt_cursor - 1);
        self.fields[PROMPT].value.drain(start..end);
        self.prompt_cursor -= 1;
    }

    pub fn select_theme_next(&mut self) {
        self.theme = self.theme.next();
    }

    pub fn select_theme_prev(&mut self) {
        self.theme = self.theme.previous();
    }

    pub fn select_dry_run_next(&mut self) {
        self.dry_run = true;
    }

    pub fn select_dry_run_prev(&mut self) {
        self.dry_run = false;
    }

    pub fn select_overwrite_next(&mut self) {
        self.overwrite = true;
    }

    pub fn select_overwrite_prev(&mut self) {
        self.overwrite = false;
    }

    /// Row index inside `context`, when a context row has focus.
    fn context_index(&self) -> Option<usize> {
        is_context_row(self.focused).then(|| self.focused - CONTEXT_PEOPLE)
    }

    pub fn select_context_next(&mut self) {
        if let Some(index) = self.context_index() {
            self.context[index] = true;
        }
    }

    pub fn select_context_prev(&mut self) {
        if let Some(index) = self.context_index() {
            self.context[index] = false;
        }
    }

    pub fn toggle_secrets(&mut self) {
        self.show_secrets = !self.show_secrets;
    }

    pub fn set_prompt_width(&self, width: usize) {
        self.prompt_width.set(width.max(1));
    }

    fn prompt_width(&self) -> usize {
        self.prompt_width.get()
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
        cfg.immich.timeout_secs = self.fields[IMMICH_TIMEOUT]
            .value
            .trim()
            .parse()
            .map_err(|_| "immich timeout must be a whole number".to_string())?;
        cfg.immich.tag = self.fields[IMMICH_TAG].value.trim().to_string();
        cfg.llm.base_url = self.fields[LLM_URL].value.trim().to_string();
        cfg.llm.api_key = self.fields[LLM_KEY].value.trim().to_string();
        cfg.llm.model = self.fields[LLM_MODEL].value.trim().to_string();
        cfg.llm.prompt = self.fields[PROMPT].value.clone();
        cfg.llm.timeout_secs = self.fields[LLM_TIMEOUT]
            .value
            .trim()
            .parse()
            .map_err(|_| "llm timeout must be a whole number".to_string())?;
        cfg.run.workers = self.fields[WORKERS]
            .value
            .trim()
            .parse()
            .map_err(|_| "workers must be a whole number".to_string())?;
        cfg.run.retries = self.fields[RETRIES]
            .value
            .trim()
            .parse()
            .map_err(|_| "retries must be a whole number".to_string())?;
        cfg.llm.max_tokens = self.fields[MAX_TOKENS]
            .value
            .trim()
            .parse()
            .map_err(|_| "max tokens must be a whole number".to_string())?;
        cfg.ui.theme = self.theme;
        cfg.run.dry_run = self.dry_run;
        cfg.run.overwrite = self.overwrite;
        cfg.llm.context.people = self.context[0];
        cfg.llm.context.place = self.context[1];
        cfg.llm.context.date = self.context[2];
        cfg.validate().map_err(|error| error.to_string())?;
        Ok(cfg)
    }
}

fn grapheme_to_byte(value: &str, grapheme_index: usize) -> usize {
    value
        .grapheme_indices(true)
        .nth(grapheme_index)
        .map_or(value.len(), |(byte_index, _)| byte_index)
}

pub fn prompt_layout(value: &str, cursor: usize, width: usize) -> PromptLayout {
    let width = width.max(1);
    let graphemes: Vec<&str> = value.graphemes(true).collect();
    let cursor = cursor.min(graphemes.len());
    let mut rows = vec![String::new()];
    let mut row_starts = vec![0];
    let mut row_widths = vec![0usize];
    let mut cursor_row = 0;
    let mut cursor_column = 0;

    for (index, grapheme) in graphemes.iter().enumerate() {
        if *grapheme == "\n" {
            if index == cursor {
                cursor_row = rows.len() - 1;
                cursor_column = rows.last().map_or(0, |row| row.graphemes(true).count());
            }
            rows.push(String::new());
            row_starts.push(index + 1);
            row_widths.push(0);
            continue;
        }
        let grapheme_width = Span::raw(*grapheme).width().max(1);
        let last = row_widths.len() - 1;
        if row_widths[last] > 0 && row_widths[last] + grapheme_width > width {
            rows.push(String::new());
            row_starts.push(index);
            row_widths.push(0);
        }
        if index == cursor {
            cursor_row = rows.len() - 1;
            cursor_column = rows.last().map_or(0, |row| row.graphemes(true).count());
        }
        rows.last_mut()
            .expect("prompt always has a row")
            .push_str(grapheme);
        *row_widths.last_mut().expect("prompt always has a row") += grapheme_width;
    }

    if cursor == graphemes.len() {
        cursor_row = rows.len() - 1;
        cursor_column = rows.last().map_or(0, |row| row.graphemes(true).count());
    }

    PromptLayout {
        rows,
        row_starts,
        cursor_row,
        cursor_column,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ThemeName};

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
        assert_eq!(f.fields.len(), 12);
        assert_eq!(f.fields[IMMICH_URL].value, "https://photos.home.lan");
        assert_eq!(f.fields[IMMICH_KEY].value, "secret-key");
        assert_eq!(f.fields[IMMICH_TAG].value, "");
        assert_eq!(f.fields[LLM_URL].value, "http://localhost:1234/v1");
        assert_eq!(f.fields[LLM_KEY].value, "");
        assert_eq!(f.fields[LLM_MODEL].value, "gemma");
        assert_eq!(f.fields[PROMPT].value, "custom prompt");
        assert_eq!(f.fields[IMMICH_TIMEOUT].value, "30");
        assert_eq!(f.fields[LLM_TIMEOUT].value, "120");
        assert_eq!(f.fields[WORKERS].value, "2");
        assert_eq!(f.fields[RETRIES].value, "3");
        assert_eq!(f.fields[MAX_TOKENS].value, "200");
        assert_eq!(f.theme, ThemeName::Btop);
        assert!(!f.dry_run);
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
        assert_eq!(f.focused, OVERWRITE);
        f.focus_next();
        assert_eq!(f.focused, IMMICH_URL);
    }

    #[test]
    fn focus_prev_reaches_every_context_row() {
        let mut f = SettingsForm::from_config(&base());
        f.focused = THEME;
        for expected in [CONTEXT_DATE, CONTEXT_PLACE, CONTEXT_PEOPLE] {
            f.focus_prev();
            assert_eq!(f.focused, expected);
        }
    }

    #[test]
    fn theme_selection_cycles_without_text_editing() {
        let mut f = SettingsForm::from_config(&base());
        f.focused = THEME;
        f.select_theme_next();
        assert_eq!(f.theme, ThemeName::Mono);
        f.select_theme_prev();
        assert_eq!(f.theme, ThemeName::Btop);
        f.insert('x');
        assert!(f.fields.iter().all(|field| !field.value.ends_with('x')));
    }

    #[test]
    fn dry_run_selection_toggles_without_text_editing() {
        let mut f = SettingsForm::from_config(&base());
        f.focused = DRY_RUN;
        f.select_dry_run_next();
        assert!(f.dry_run);
        f.select_dry_run_prev();
        assert!(!f.dry_run);
        f.insert('x');
        assert!(f.fields.iter().all(|field| !field.value.ends_with('x')));
    }

    #[test]
    fn to_config_persists_dry_run_selection() {
        let mut f = SettingsForm::from_config(&base());
        f.dry_run = true;
        let cfg = f.to_config(&base()).unwrap();
        assert!(cfg.run.dry_run);
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
    fn prompt_editor_supports_newlines_and_cursor_movement() {
        let mut f = SettingsForm::from_config(&base());
        f.focused = PROMPT;
        f.clear();
        f.insert('A');
        f.newline();
        f.insert('B');
        f.move_prompt_up();
        f.insert('!');
        f.move_prompt_down();
        f.backspace();

        assert_eq!(f.fields[PROMPT].value, "A!\n");
    }

    #[test]
    fn prompt_editor_keeps_the_full_multiline_value_when_saved() {
        let mut f = SettingsForm::from_config(&base());
        f.fields[PROMPT].value = "\nfirst line\nsecond line\n".into();
        let cfg = f.to_config(&base()).unwrap();
        assert_eq!(cfg.llm.prompt, "\nfirst line\nsecond line\n");
    }

    #[test]
    fn prompt_editor_treats_a_zwj_emoji_as_one_editable_unit() {
        let mut f = SettingsForm::from_config(&base());
        f.focused = PROMPT;
        f.clear();
        f.insert('👩');
        f.insert('\u{200d}');
        f.insert('💻');
        f.backspace();

        assert!(f.fields[PROMPT].value.is_empty());
    }

    #[test]
    fn prompt_editor_moves_across_soft_wrapped_rows() {
        let mut f = SettingsForm::from_config(&base());
        f.focused = PROMPT;
        f.clear();
        for _ in 0..(PROMPT_WRAP_WIDTH + 5) {
            f.insert('x');
        }

        f.move_prompt_up();

        assert_eq!(f.prompt_cursor, 5);
    }

    #[test]
    fn clear_removes_the_focused_text_field_but_not_theme() {
        let mut f = SettingsForm::from_config(&base());
        f.focused = PROMPT;
        f.clear();
        assert!(f.fields[PROMPT].value.is_empty());

        f.focused = THEME;
        f.clear();
        assert_eq!(f.theme, ThemeName::Btop);
    }

    #[test]
    fn to_config_keeps_file_only_values() {
        let mut f = SettingsForm::from_config(&base());
        f.fields[WORKERS].value = "4".into();
        f.fields[PROMPT].value = "updated prompt".into();
        f.fields[IMMICH_TIMEOUT].value = "45".into();
        f.fields[LLM_TIMEOUT].value = "180".into();
        f.fields[RETRIES].value = "5".into();
        f.fields[MAX_TOKENS].value = "300".into();
        f.theme = ThemeName::Mono;
        let cfg = f.to_config(&base()).unwrap();
        assert_eq!(cfg.run.workers, 4);
        assert_eq!(cfg.run.retries, 5);
        assert_eq!(cfg.llm.max_tokens, 300);
        assert_eq!(cfg.llm.prompt, "updated prompt");
        assert_eq!(cfg.immich.timeout_secs, 45);
        assert_eq!(cfg.llm.timeout_secs, 180);
        assert_eq!(cfg.ui.theme, ThemeName::Mono);
    }

    #[test]
    fn context_rows_take_their_values_from_the_config() {
        let mut cfg = base();
        cfg.llm.context.place = false;
        let f = SettingsForm::from_config(&cfg);
        assert_eq!(f.context, [true, false, true]);
    }

    #[test]
    fn context_selection_toggles_without_text_editing() {
        let mut f = SettingsForm::from_config(&base());
        f.focused = CONTEXT_PLACE;
        f.select_context_prev();
        assert_eq!(f.context, [true, false, true]);
        f.select_context_next();
        assert_eq!(f.context, [true, true, true]);
        f.insert('x');
        assert!(f.fields.iter().all(|field| !field.value.ends_with('x')));
    }

    #[test]
    fn focus_reaches_every_context_row() {
        let mut f = SettingsForm::from_config(&base());
        f.focused = MAX_TOKENS;
        for expected in [CONTEXT_PEOPLE, CONTEXT_PLACE, CONTEXT_DATE, THEME, DRY_RUN] {
            f.focus_next();
            assert_eq!(f.focused, expected);
        }
    }

    #[test]
    fn to_config_persists_the_context_switches() {
        let mut f = SettingsForm::from_config(&base());
        f.context = [true, false, false];
        let cfg = f.to_config(&base()).unwrap();
        assert!(cfg.llm.context.people);
        assert!(!cfg.llm.context.place);
        assert!(!cfg.llm.context.date);
    }

    #[test]
    fn to_config_keeps_context_enabled_as_a_file_only_value() {
        let mut base = base();
        base.llm.context.enabled = false;
        let mut f = SettingsForm::from_config(&base);
        f.context = [true, true, true];
        let cfg = f.to_config(&base).unwrap();
        assert!(!cfg.llm.context.enabled);
    }

    #[test]
    fn from_config_fills_the_tag_and_the_overwrite_value() {
        let mut cfg = base();
        cfg.immich.tag = "gen-desc".into();
        cfg.run.overwrite = true;
        let f = SettingsForm::from_config(&cfg);
        assert_eq!(f.fields.len(), 12);
        assert_eq!(f.fields[IMMICH_TAG].value, "gen-desc");
        assert!(f.overwrite);
    }

    #[test]
    fn to_config_trims_the_tag_and_keeps_the_overwrite_value() {
        let mut f = SettingsForm::from_config(&base());
        f.fields[IMMICH_TAG].value = "  gen-desc  ".into();
        f.overwrite = true;
        let cfg = f.to_config(&base()).unwrap();
        assert_eq!(cfg.immich.tag, "gen-desc");
        assert!(cfg.run.overwrite);
    }

    #[test]
    fn to_config_reports_an_invalid_tag() {
        let mut f = SettingsForm::from_config(&base());
        f.fields[IMMICH_TAG].value = "a//b".into();
        let err = f.to_config(&base()).unwrap_err();
        assert!(err.contains("immich.tag"), "{err}");
    }

    #[test]
    fn clear_empties_the_tag_row() {
        let mut cfg = base();
        cfg.immich.tag = "gen-desc".into();
        let mut f = SettingsForm::from_config(&cfg);
        f.focused = IMMICH_TAG;
        f.clear();
        assert_eq!(f.fields[IMMICH_TAG].value, "");
    }

    #[test]
    fn the_overwrite_row_is_the_last_row() {
        let f = SettingsForm::from_config(&base());
        assert_eq!(OVERWRITE + 1, FIELD_COUNT);
        let mut f = f;
        f.focused = OVERWRITE;
        assert!(f.is_last_focused());
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

        let mut f = SettingsForm::from_config(&base());
        f.fields[IMMICH_TIMEOUT].value = "not-a-number".into();
        assert!(f.to_config(&base()).unwrap_err().contains("immich timeout"));

        let mut f = SettingsForm::from_config(&base());
        f.fields[LLM_TIMEOUT].value = "not-a-number".into();
        assert!(f.to_config(&base()).unwrap_err().contains("llm timeout"));
    }
}
