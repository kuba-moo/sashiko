// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::ai::{AiResponse, ErrorAction, ValidationError};
use serde_json::Value;

pub trait ReviewStage: Send + Sync {
    /// Returns the stage number (1..=11).
    fn number(&self) -> u8;

    /// Returns the stage name.
    fn name(&self) -> &'static str;

    /// Returns true if this stage should include the full log in its context.
    /// Stages 3-6 return false to optimize context size.
    fn use_log_in_context(&self) -> bool {
        true
    }

    /// Validates the LLM response for this stage.
    fn validate(&mut self, response: &AiResponse) -> Result<Value, ValidationError>;

    /// Formats the feedback message for the LLM when validation fails.
    fn format_validation_feedback(&self, violation: &str) -> String {
        format!(
            "\n\nPrevious attempt was rejected: {}. Please correct your output format.",
            violation
        )
    }

    /// Handles provider errors, specifically recitation errors.
    /// Returns `Some(ErrorAction)` if it handled the error, or `None` to fallback to default handling.
    fn handle_recitation_error(&mut self) -> Option<ErrorAction> {
        None
    }
}

macro_rules! define_standard_stage {
    ($struct_name:ident, $num:expr, $name:expr) => {
        pub struct $struct_name;
        impl ReviewStage for $struct_name {
            fn number(&self) -> u8 {
                $num
            }
            fn name(&self) -> &'static str {
                $name
            }
            fn use_log_in_context(&self) -> bool {
                !((3..=6).contains(&$num))
            }
            fn validate(&mut self, response: &AiResponse) -> Result<Value, ValidationError> {
                validate_stages_1_to_7(response, $num)
            }
            fn format_validation_feedback(&self, violation: &str) -> String {
                format_validation_feedback_stages_1_to_8(violation)
            }
        }
    };
}

define_standard_stage!(Stage1, 1, "Analyze commit main goal");
define_standard_stage!(Stage2, 2, "High-level implementation verification");
define_standard_stage!(Stage3, 3, "Execution flow verification");
define_standard_stage!(Stage4, 4, "Resource management");
define_standard_stage!(Stage5, 5, "Locking and synchronization");
define_standard_stage!(Stage6, 6, "Security audit");
define_standard_stage!(Stage7, 7, "Hardware engineer's review");

pub struct Stage8;
impl ReviewStage for Stage8 {
    fn number(&self) -> u8 {
        8
    }
    fn name(&self) -> &'static str {
        "Deduplication and Consolidation"
    }
    fn validate(&mut self, response: &AiResponse) -> Result<Value, ValidationError> {
        parse_json_response(response, 8)
    }
    fn format_validation_feedback(&self, violation: &str) -> String {
        format!(
            "\n\nYour previous Stage 8 response was rejected. {violation}\n\nReturn one corrected compact plan with `concerns` and `dismissed_concerns` objects, each containing `keep` and `merge` arrays. Every input ID must appear exactly once."
        )
    }
}

pub struct Stage9;
impl ReviewStage for Stage9 {
    fn number(&self) -> u8 {
        9
    }
    fn name(&self) -> &'static str {
        "Concern/dismissed-concern conflict resolution"
    }
    /// Stage 9 returns a keep/discard plan over input IDs, so the concern shape
    /// is checked against the inputs by the session rather than here.
    fn validate(&mut self, response: &AiResponse) -> Result<Value, ValidationError> {
        parse_json_response(response, 9)
    }
    fn format_validation_feedback(&self, violation: &str) -> String {
        format!(
            "\n\nYour previous Stage 9 response was rejected. {violation}\n\nReturn one corrected compact plan with `keep` and `discard` arrays. Every consolidated concern ID must appear exactly once."
        )
    }
}

pub struct Stage10;
impl ReviewStage for Stage10 {
    fn number(&self) -> u8 {
        10
    }
    fn name(&self) -> &'static str {
        "Verification and severity estimation"
    }
    fn validate(&mut self, response: &AiResponse) -> Result<Value, ValidationError> {
        let parsed = parse_json_response(response, 10)?;
        if let Some(f) = parsed.get("findings") {
            if !f.is_array() {
                return Err(ValidationError::FormatViolation(
                    "output 'findings' is not an array".to_string(),
                ));
            }
            validate_source_stages(f, "finding")?;
            validate_model_provenance(f, "finding")?;
            if f.as_array().is_some_and(|findings| {
                findings.iter().any(|finding| {
                    !finding
                        .get("requires_validation")
                        .is_some_and(Value::is_boolean)
                })
            }) {
                return Err(ValidationError::FormatViolation(
                    "every finding must contain a 'requires_validation' boolean".to_string(),
                ));
            }
        } else {
            return Err(ValidationError::FormatViolation(
                "missing 'findings' array in output".to_string(),
            ));
        }
        Ok(parsed)
    }
    fn format_validation_feedback(&self, violation: &str) -> String {
        format!(
            "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'findings' array.",
            violation
        )
    }
}

#[derive(Default)]
pub struct Stage11 {
    free_form_mode: bool,
}

impl ReviewStage for Stage11 {
    fn number(&self) -> u8 {
        11
    }
    fn name(&self) -> &'static str {
        "LKML-friendly report generation"
    }
    fn validate(&mut self, response: &AiResponse) -> Result<Value, ValidationError> {
        let text = response.content.as_deref().unwrap_or("");
        if self.free_form_mode {
            Ok(Value::String(text.to_string()))
        } else {
            match validate_inline_format(text) {
                Ok(_) => Ok(Value::String(text.to_string())),
                Err(violation) => Err(ValidationError::FormatViolation(violation)),
            }
        }
    }
    fn format_validation_feedback(&self, violation: &str) -> String {
        format!(
            "Previous attempt was rejected: {}. Please correct your output format.",
            violation
        )
    }
    fn handle_recitation_error(&mut self) -> Option<ErrorAction> {
        if !self.free_form_mode {
            self.free_form_mode = true;
            let fallback_reminder = "\n\nCRITICAL: The previous attempt failed due to a RECITATION policy violation. Do NOT quote the original patch code at all. Instead, provide a free-form summary of the findings. Start your report with a note explaining that the format is altered due to recitation restrictions. Do not use the inline quoting style `>`.";
            Some(ErrorAction::RetryWithFeedback(
                fallback_reminder.to_string(),
            ))
        } else {
            None
        }
    }
}

pub fn create_stage(stage: u8) -> Box<dyn ReviewStage> {
    match stage {
        1 => Box::new(Stage1),
        2 => Box::new(Stage2),
        3 => Box::new(Stage3),
        4 => Box::new(Stage4),
        5 => Box::new(Stage5),
        6 => Box::new(Stage6),
        7 => Box::new(Stage7),
        8 => Box::new(Stage8),
        9 => Box::new(Stage9),
        10 => Box::new(Stage10),
        11 => Box::new(Stage11::default()),
        _ => panic!("Unsupported stage: {}", stage),
    }
}

// Helper functions moved from prompts.rs

fn validate_stages_1_to_7(response: &AiResponse, stage: u8) -> Result<Value, ValidationError> {
    let parsed = parse_json_response(response, stage)?;
    match required_stage_arrays(&parsed) {
        Ok(_) => Ok(parsed),
        Err(violation) => Err(ValidationError::FormatViolation(violation)),
    }
}

fn format_validation_feedback_stages_1_to_8(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'concerns' and 'dismissed_concerns' arrays. If there are no concerns and no dismissed concerns, return `{{\"concerns\": [], \"dismissed_concerns\": []}}`.",
        violation
    )
}

fn validate_inline_format(content: &str) -> std::result::Result<(), String> {
    if content.lines().any(|l| l.trim_start().starts_with("```")) {
        return Err("The output contains Markdown code blocks ('```'). It must be plain text as per `inline-template.md`.".to_string());
    }
    if !content.lines().any(|l| l.trim_start().starts_with(">")) {
        return Err("The output does not appear to quote any code or context using '>'. Please follow the quoting style in `inline-template.md`.".to_string());
    }
    let has_commit_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("commit "));
    if !has_commit_header {
        return Err("The output is missing the 'commit <hash>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`.".to_string());
    }
    let has_author_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("author:"));
    if !has_author_header {
        return Err("The output is missing the 'Author: <name>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`.".to_string());
    }
    let has_comments = content.lines().any(|l| {
        let trimmed = l.trim();
        if trimmed.is_empty() || trimmed.starts_with(">") {
            return false;
        }
        let lower = trimmed.to_lowercase();
        !lower.starts_with("commit ")
            && !lower.starts_with("author:")
            && !lower.starts_with("date:")
            && !lower.starts_with("link:")
    });
    if !has_comments {
        return Err("The output appears to lack any comments or summary. You must include a summary and interspersed comments explaining the findings.".to_string());
    }
    Ok(())
}

fn required_stage_arrays(value: &Value) -> std::result::Result<(&[Value], &[Value]), String> {
    let concerns = value
        .get("concerns")
        .and_then(Value::as_array)
        .ok_or_else(|| "JSON output is missing the required 'concerns' array".to_string())?;
    let dismissed_concerns = value
        .get("dismissed_concerns")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "JSON output is missing the required 'dismissed_concerns' array".to_string()
        })?;

    Ok((concerns.as_slice(), dismissed_concerns.as_slice()))
}

fn validate_source_stages(value: &Value, item_name: &str) -> Result<(), ValidationError> {
    let Some(items) = value.as_array() else {
        return Ok(());
    };
    if items
        .iter()
        .any(|item| !item.get("source_stages").is_some_and(Value::is_array))
    {
        return Err(ValidationError::FormatViolation(format!(
            "every {} must contain a 'source_stages' array",
            item_name
        )));
    }
    Ok(())
}

fn validate_model_provenance(value: &Value, item_name: &str) -> Result<(), ValidationError> {
    let Some(items) = value.as_array() else {
        return Ok(());
    };
    if items.iter().any(|item| {
        !item.get("source_models").is_some_and(Value::is_array)
            || !item.get("finding_ids").is_some_and(Value::is_array)
    }) {
        return Err(ValidationError::FormatViolation(format!(
            "every {} must contain 'source_models' and 'finding_ids' arrays",
            item_name
        )));
    }
    Ok(())
}

fn parse_json_response(
    response: &AiResponse,
    stage: u8,
) -> Result<serde_json::Value, ValidationError> {
    let raw_text = response.content.as_deref().unwrap_or("");
    let cleaned = crate::utils::clean_json_string(raw_text);
    let parsed: serde_json::Value = match serde_json::from_str(&cleaned) {
        Ok(parsed) => parsed,
        Err(error) => {
            // A reply that only parses after salvage still signals a model that
            // is drifting from the contract, so record it either way.
            let source = crate::json_health::stage_source(stage);
            match find_json_candidates(raw_text).into_iter().next_back() {
                Some(candidate) => {
                    crate::json_health::record(
                        source,
                        crate::json_health::JsonDecodeOutcome::Salvaged,
                        &error.to_string(),
                    );
                    candidate
                }
                None => {
                    // Returning {} here lets the caller's field checks produce a
                    // FormatViolation, which drives the schema-restating retry.
                    serde_json::json!({})
                }
            }
        }
    };
    Ok(parsed)
}

/// Returns the parse error when a response contains no recoverable JSON object.
/// The session owns final outcome classification because validation may retry.
pub(crate) fn unrecoverable_json_error(response: &AiResponse) -> Option<String> {
    let raw_text = response.content.as_deref().unwrap_or("");
    let cleaned = crate::utils::clean_json_string(raw_text);
    match serde_json::from_str::<Value>(&cleaned) {
        Ok(_) => None,
        Err(error) if find_json_candidates(raw_text).is_empty() => Some(error.to_string()),
        Err(_) => None,
    }
}

/// Recovers embedded JSON objects from prose- or fence-wrapped model output.
pub(crate) fn find_json_candidates(text: &str) -> Vec<Value> {
    let mut candidates = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '{'
            && let Some(end) = find_matching_brace(&chars, i)
        {
            let candidate: String = chars[i..=end].iter().collect();
            let clean_candidate = crate::utils::clean_json_string(&candidate);
            if let Ok(v) =
                serde_json::from_str(&clean_candidate).or_else(|_| serde_json::from_str(&candidate))
            {
                candidates.push(v);
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    candidates
}

fn find_matching_brace(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 0;
    let mut in_string = false;
    let mut escape = false;

    for (i, c) in chars.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if *c == '\\' {
                escape = true;
            } else if *c == '"' {
                in_string = false;
            }
        } else if *c == '"' {
            in_string = true;
        } else if *c == '{' {
            depth += 1;
        } else if *c == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_health::{self, JsonDecodeOutcome};
    use serde_json::json;

    fn response(content: &str) -> AiResponse {
        AiResponse {
            content: Some(content.to_string()),
            thought: None,
            thought_signature: None,
            reasoning: None,
            tool_calls: None,
            usage: None,
            truncated: false,
        }
    }

    // The recorder is process-global; serialize the tests that read it.
    #[test]
    fn stage_salvage_is_recorded_against_its_stage() {
        let _guard = json_health::TEST_GUARD.blocking_lock();
        json_health::drain();

        // Fenced JSON only parses via salvage.
        let parsed = parse_json_response(
            &response("```json\n{\"concerns\": [], \"dismissed_concerns\": []}\n```"),
            8,
        )
        .unwrap();
        assert!(parsed["concerns"].is_array());

        let events = json_health::drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "stage:8");
        assert_eq!(events[0].outcome, JsonDecodeOutcome::Salvaged);
    }

    #[test]
    fn unrecoverable_stage_output_is_deferred_to_the_session() {
        let _guard = json_health::TEST_GUARD.blocking_lock();
        json_health::drain();

        let parsed = parse_json_response(&response("I cannot answer that."), 10).unwrap();
        // An empty object lets the caller's field checks drive the retry.
        assert_eq!(parsed, json!({}));

        assert!(json_health::drain().is_empty());
        assert!(unrecoverable_json_error(&response("I cannot answer that.")).is_some());
    }

    #[test]
    fn clean_stage_output_records_nothing() {
        let _guard = json_health::TEST_GUARD.blocking_lock();
        json_health::drain();

        parse_json_response(
            &response("{\"concerns\": [], \"dismissed_concerns\": []}"),
            3,
        )
        .unwrap();

        assert!(json_health::drain().is_empty());
    }

    #[test]
    fn test_required_stage_arrays_accepts_empty_arrays() {
        let output = json!({
            "concerns": [],
            "dismissed_concerns": []
        });

        let (concerns, dismissed_concerns) = required_stage_arrays(&output).unwrap();

        assert!(concerns.is_empty());
        assert!(dismissed_concerns.is_empty());
    }

    #[test]
    fn test_required_stage_arrays_rejects_missing_dismissed_concerns() {
        let output = json!({
            "concerns": []
        });

        let err = required_stage_arrays(&output).unwrap_err();

        assert!(err.contains("'dismissed_concerns'"));
    }
}
