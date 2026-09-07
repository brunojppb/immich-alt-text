//! Builds the prompt the model receives: the configured prompt plus the library context.

use chrono::{NaiveDateTime, Timelike};

use crate::config::ContextConfig;
use crate::immich::AssetContext;

/// Marks where the context block goes. A prompt without it gets the block appended.
pub const PLACEHOLDER: &str = "{{context}}";
const HEADER: &str = "Context from the photo library:";

/// The prompt for one photo, with the library context in place.
pub fn build(prompt: &str, context: &AssetContext, cfg: &ContextConfig) -> String {
    let block = block(context, cfg);
    if prompt.contains(PLACEHOLDER) {
        return place(prompt, &block);
    }
    if block.is_empty() {
        return prompt.to_string();
    }
    format!("{}\n\n{block}", prompt.trim_end())
}

fn block(context: &AssetContext, cfg: &ContextConfig) -> String {
    if !cfg.enabled {
        return String::new();
    }
    let mut lines = Vec::new();
    if cfg.people && !context.people.is_empty() {
        lines.push(format!("People: {}", context.people.join(", ")));
    }
    if cfg.place && !context.place.is_empty() {
        lines.push(format!("Place: {}", context.place.join(", ")));
    }
    if let (true, Some(taken)) = (cfg.date, context.taken) {
        lines.push(format!("Taken: {}", taken_line(taken)));
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("{HEADER}\n{}", lines.join("\n"))
}

fn taken_line(taken: NaiveDateTime) -> String {
    format!(
        "{}, {}",
        taken.format("%A %-d %B %Y"),
        part_of_day(taken.hour())
    )
}

const fn part_of_day(hour: u32) -> &'static str {
    match hour {
        5..=11 => "morning",
        12..=16 => "afternoon",
        17..=20 => "evening",
        _ => "night",
    }
}

/// Swaps the placeholder for the block. An empty block also takes its line.
fn place(prompt: &str, block: &str) -> String {
    prompt
        .lines()
        .filter(|line| !(block.is_empty() && line.trim() == PLACEHOLDER))
        .map(|line| line.replace(PLACEHOLDER, block))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn context() -> AssetContext {
        AssetContext {
            people: vec!["Ana".into(), "Marco".into()],
            place: vec!["Sintra".into(), "Portugal".into()],
            taken: Some(
                NaiveDate::from_ymd_opt(2019, 6, 14)
                    .unwrap()
                    .and_hms_opt(19, 23, 0)
                    .unwrap(),
            ),
        }
    }

    #[test]
    fn an_empty_context_leaves_the_prompt_alone() {
        let out = build(
            "Describe it.",
            &AssetContext::default(),
            &ContextConfig::default(),
        );
        assert_eq!(out, "Describe it.");
    }

    #[test]
    fn a_full_context_appends_the_block() {
        let out = build("Describe it.", &context(), &ContextConfig::default());
        assert_eq!(
            out,
            "Describe it.\n\nContext from the photo library:\nPeople: Ana, Marco\nPlace: Sintra, Portugal\nTaken: Friday 14 June 2019, evening"
        );
    }

    #[test]
    fn the_placeholder_takes_the_block() {
        let out = build(
            "{{context}}\n\nDescribe it.",
            &context(),
            &ContextConfig::default(),
        );
        assert_eq!(
            out,
            "Context from the photo library:\nPeople: Ana, Marco\nPlace: Sintra, Portugal\nTaken: Friday 14 June 2019, evening\n\nDescribe it."
        );
        assert_eq!(out.matches("People:").count(), 1);
    }

    #[test]
    fn an_empty_block_removes_the_placeholder_line() {
        let out = build(
            "Describe it.\n{{context}}\nBe brief.",
            &AssetContext::default(),
            &ContextConfig::default(),
        );
        assert_eq!(out, "Describe it.\nBe brief.");
    }

    #[test]
    fn each_switch_removes_its_line() {
        let cfg = ContextConfig {
            enabled: true,
            people: true,
            place: false,
            date: false,
        };
        let out = build("Describe it.", &context(), &cfg);
        assert!(out.contains("People: Ana, Marco"));
        assert!(!out.contains("Place:"));
        assert!(!out.contains("Taken:"));
    }

    #[test]
    fn the_disabled_switch_removes_the_whole_block() {
        let cfg = ContextConfig {
            enabled: false,
            people: true,
            place: true,
            date: true,
        };
        assert_eq!(build("Describe it.", &context(), &cfg), "Describe it.");
    }

    #[test]
    fn the_part_of_day_matches_the_hour() {
        assert_eq!(part_of_day(5), "morning");
        assert_eq!(part_of_day(11), "morning");
        assert_eq!(part_of_day(12), "afternoon");
        assert_eq!(part_of_day(16), "afternoon");
        assert_eq!(part_of_day(17), "evening");
        assert_eq!(part_of_day(20), "evening");
        assert_eq!(part_of_day(21), "night");
        assert_eq!(part_of_day(4), "night");
    }
}
