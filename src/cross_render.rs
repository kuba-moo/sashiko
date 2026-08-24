// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0

//! Rendering of accepted cross-instance findings into an existing inline report.
//!
//! Local reviews build their report in stage 11, which needs the review tools and
//! the patch worktree. Cross-review results arrive hours later, once that worktree
//! is gone, so a full stage 11 re-run is not available here. Instead this module
//! renders only the comment block for each newly accepted remote finding and
//! splices it into the report next to the code it is about.
//!
//! The split of responsibilities is deliberate:
//!
//! * the model writes prose only, in the voice of the surrounding report;
//! * Rust owns the `[Severity:]` / `[Finding:]` / `[Sources:]` tag lines, so
//!   untrusted remote text can never forge a severity, a finding ID or a source
//!   attribution in the web UI;
//! * Rust owns placement, so a bad or missing anchor degrades to a worse position
//!   rather than to a wrong one.
//!
//! Rendering is best effort. When the model call fails the caller falls back to
//! [`fallback_comment`], which is still anchored and still tagged; publication of
//! a verified remote finding never depends on a model being reachable.
//!
//! Replacing this with a full stage 11 re-run needs worktree and tool
//! provisioning first. `designs/DESIGN_CROSS_INSTANCE_REVIEWS.md`, under "If A
//! Full Re-render Is Revisited", lists the corner cases that path has to answer.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

use crate::ai::AiProvider;
use crate::ai::review_budget::ReviewBudget;
use crate::cross_review::{RemoteFinding, id_map_schema, request_json};

/// Upper bound on a single rendered comment, before the tag lines are added.
const MAX_COMMENT_BYTES: usize = 4096;

/// Upper bound on the authoritative diff inlined into the render prompt. The
/// prepared context already carries the patch; this copy only exists so anchors
/// are quoted from something we can match against, and it is not worth spending
/// unbounded budget on.
const MAX_PROMPT_DIFF_BYTES: usize = 200_000;

/// Shortest anchor payload we will trust. Lines like `}` occur dozens of times in
/// a patch, so anchoring on them is worse than not anchoring at all.
const MIN_ANCHOR_PAYLOAD: usize = 4;

/// How far past an anchored line we will look for the end of its quoted run.
/// Snipped reports quote roughly a hunk at a time, so this is generous for them
/// and still keeps a comment near its subject in a report that never snips.
const MAX_QUOTE_RUN_EXTENSION: usize = 40;

/// The report text for one accepted remote finding.
#[derive(Debug, Clone, Default)]
pub struct RenderedComment {
    /// Reviewer prose, without any tag lines.
    pub comment: String,
    /// A line the model copied from the patch, used as the preferred anchor.
    pub anchor: Option<String>,
}

/// Renders one comment per accepted remote finding for a single patch.
///
/// Returns a map keyed by [`RemoteFinding::finding_id`]. Findings missing from the
/// map (because the model omitted them) are the caller's problem to fall back for.
#[allow(clippy::too_many_arguments)]
pub async fn render_remote_comments(
    provider: &dyn AiProvider,
    source_name: &str,
    context: &str,
    diff: &str,
    inline_review: &str,
    findings: &[&RemoteFinding],
    budget: Option<&ReviewBudget>,
    usage: &mut crate::cross_review::CrossReviewUsage,
) -> Result<HashMap<String, RenderedComment>> {
    if findings.is_empty() {
        return Ok(HashMap::new());
    }
    let finding_ids: Vec<&str> = findings
        .iter()
        .map(|finding| finding.finding_id.as_str())
        .collect();
    let expected: HashSet<&str> = finding_ids.iter().copied().collect();
    let prompt = render_prompt(source_name, context, diff, inline_review, findings)?;
    let value_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "comment": {"type": "string"},
            "anchor": {"type": "string"},
        },
        "required": ["comment", "anchor"],
        "additionalProperties": false,
    });
    let response = request_json(
        provider,
        "cross-review:render",
        prompt,
        id_map_schema(&finding_ids, value_schema),
        budget,
        usage,
        |value| {
            let object = value
                .as_object()
                .context("render response is not an object")?;
            let actual: HashSet<&str> = object.keys().map(String::as_str).collect();
            if actual != expected {
                anyhow::bail!("render response must map exactly the requested findings");
            }
            if object.values().any(|entry| {
                entry["comment"]
                    .as_str()
                    .is_none_or(|comment| comment.trim().is_empty())
            }) {
                anyhow::bail!("every render entry needs a non-empty comment string");
            }
            Ok(())
        },
    )
    .await?;
    let object = response
        .as_object()
        .context("render response is not an object")?;
    Ok(object
        .iter()
        .filter_map(|(finding_id, entry)| {
            let comment = sanitize_comment(entry["comment"].as_str().unwrap_or_default())?;
            let anchor = entry["anchor"]
                .as_str()
                .map(normalize_anchor)
                .filter(|anchor| !anchor.trim().is_empty());
            Some((finding_id.clone(), RenderedComment { comment, anchor }))
        })
        .collect())
}

fn render_prompt(
    source_name: &str,
    context: &str,
    diff: &str,
    inline_review: &str,
    findings: &[&RemoteFinding],
) -> Result<String> {
    let existing = if inline_review.trim().is_empty() {
        "(this patch has no local findings yet, so there is no report to match)".to_string()
    } else {
        inline_review.to_string()
    };
    Ok(format!(
        "{context}\n\n\
         Authoritative diff for this patch:\n{}\n\n\
         Review report already written for this patch, in the house style you must match:\n\
         {existing}\n\n\
         A peer review instance named {source_name} reported the findings below and local \
         verification already accepted them, so every one of them will be published. Your only \
         job is to write the reviewer comment for each.\n\n\
         For every finding_id return an object with these two fields:\n\
         - comment: the reviewer comment as plain text, in the voice of the report above, as a \
         kernel maintainer replying on the mailing list. Say what goes wrong and why it matters, \
         name the concrete functions and variables involved, and where the right fix is not \
         obvious ask the submitter instead of asserting one. Wrap lines at 72 columns. Do not use \
         backticks. Do not write a [Severity: ...], [Finding: ...] or [Sources: ...] line and do \
         not name the reporting instance; those are added for you. Do not quote the diff; the \
         relevant hunk is quoted for you.\n\
         - anchor: one line copied verbatim from the authoritative diff above, including its \
         leading plus, minus or space character, identifying where the comment belongs. Prefer an \
         added line inside the hunk the finding is about.\n\n\
         Remote findings:\n{}",
        truncate_bytes(diff, MAX_PROMPT_DIFF_BYTES, "\n[remaining diff truncated]"),
        serde_json::to_string_pretty(findings)?,
    ))
}

/// Builds the display ID used in the report and in the stored finding provenance.
///
/// The full remote finding ID is a sha256 over the finding, which is the right
/// join and idempotency key but reads as line noise in an email. Truncating keeps
/// it greppable against `cross_review_findings` without the 64-character line.
pub fn display_finding_id(source_name: &str, finding_id: &str) -> String {
    let short: String = finding_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if short.is_empty() {
        source_name.to_string()
    } else {
        format!("{source_name}-{short}")
    }
}

/// Assembles the tagged comment block that gets spliced into the report.
pub fn comment_block(severity: &str, display_id: &str, source_name: &str, comment: &str) -> String {
    format!(
        "[Severity: {severity}]\n[Finding: {display_id}]\n[Sources: {source_name}]\n{}",
        comment.trim_end()
    )
}

/// Comment text used when the render call is unavailable.
///
/// The remote's own `problem` and `reasoning` are reused verbatim. That reads more
/// like a bug report than like the surrounding review, but it carries the whole
/// finding rather than its first sentence, and it needs no model.
pub fn fallback_comment(finding: &RemoteFinding) -> String {
    let mut text = finding.problem.replace('`', "").trim().to_string();
    let reasoning = finding.reasoning.replace('`', "").trim().to_string();
    if !reasoning.is_empty() && !text.contains(&reasoning) {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&reasoning);
    }
    truncate_comment(&neutralize_tags(&text))
}

/// Splices `block` into `inline_review` next to the code the finding is about.
///
/// Placement degrades in three steps: after the anchored line if the report
/// already quotes it, else after a freshly quoted copy of the anchored hunk, else
/// at the end of the report.
pub fn splice_comment_block(
    inline_review: &str,
    diff: &str,
    block: &str,
    anchor: Option<&str>,
    locations: &Value,
) -> String {
    let hunks = parse_diff(diff);
    let report = drop_no_issues_line(inline_review);
    let mut lines: Vec<String> = report.lines().map(ToString::to_string).collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let candidates = anchor_candidates(anchor, &hunks, locations);

    for candidate in &candidates {
        if let Some(position) = find_quoted_line(&lines, candidate) {
            let position = end_of_quoted_run(&lines, position);
            let mut spliced = lines[..=position].to_vec();
            spliced.push(String::new());
            spliced.extend(block.lines().map(ToString::to_string));
            if lines
                .get(position + 1)
                .is_some_and(|line| !line.trim().is_empty())
            {
                spliced.push(String::new());
            }
            spliced.extend(lines[position + 1..].iter().cloned());
            return spliced.join("\n");
        }
    }

    for candidate in &candidates {
        if let Some(hunk) = hunks.iter().find(|hunk| {
            hunk.lines
                .iter()
                .any(|line| anchor_matches(candidate, line))
        }) {
            let mut spliced = lines.clone();
            if !spliced.is_empty() {
                if spliced.last().is_some_and(|line| line.trim() != "[ ... ]") {
                    spliced.push(String::new());
                    spliced.push("[ ... ]".to_string());
                }
                spliced.push(String::new());
            }
            // Repeating a file header that the report already quotes reads as
            // noise, but a hunk with no header at all is unattributable.
            if !report.contains(&format!("> diff --git a/{}", hunk.file)) {
                spliced.extend(hunk.header.iter().map(|line| quote_line(line)));
            }
            spliced.extend(hunk.lines.iter().map(|line| quote_line(line)));
            spliced.push(String::new());
            spliced.extend(block.lines().map(ToString::to_string));
            return spliced.join("\n");
        }
    }

    if lines.is_empty() {
        return block.to_string();
    }
    lines.push(String::new());
    lines.extend(block.lines().map(ToString::to_string));
    lines.join("\n")
}

/// Index of the last line of the quoted run that `position` belongs to.
///
/// Replies belong under the whole quoted hunk, not inside it: anchoring on the one
/// line a finding names would otherwise cut a quoted function in half. Reports that
/// quote without snipping are left anchored on the line itself, because walking to
/// the end of a hundred-line quote separates the comment from its subject.
fn end_of_quoted_run(lines: &[String], position: usize) -> usize {
    for (index, line) in lines.iter().enumerate().skip(position + 1) {
        if !line.starts_with('>') {
            return index - 1;
        }
        if index - position > MAX_QUOTE_RUN_EXTENSION {
            return position;
        }
    }
    lines.len().saturating_sub(1).max(position)
}

/// A report that only says there are no issues must stop saying so once a remote
/// finding is published into it.
fn drop_no_issues_line(inline_review: &str) -> String {
    if inline_review.contains("[Severity:") {
        return inline_review.to_string();
    }
    let kept: Vec<&str> = inline_review
        .lines()
        .filter(|line| line.trim() != "No issues found.")
        .collect();
    kept.join("\n")
}

fn anchor_candidates(anchor: Option<&str>, hunks: &[DiffHunk], locations: &Value) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    let mut push = |candidate: String| {
        if diff_payload(&candidate).trim().len() >= MIN_ANCHOR_PAYLOAD
            && !candidates.contains(&candidate)
        {
            candidates.push(candidate);
        }
    };
    if let Some(anchor) = anchor {
        push(normalize_anchor(anchor));
    }
    for location in locations.as_array().into_iter().flatten() {
        if let Some(line) = location_anchor_line(hunks, location) {
            push(line);
        }
    }
    // The remote's quoted snippet is the last resort: it is often reflowed or
    // abridged, so it matches less reliably than a line number does.
    for location in locations.as_array().into_iter().flatten() {
        for line in location["code_snippet"]
            .as_str()
            .unwrap_or_default()
            .lines()
        {
            push(normalize_anchor(line));
        }
    }
    candidates
}

/// Turns a `file` plus post-image `line` from a remote finding into the exact diff
/// line at that position, which is a far more reliable anchor than quoted text.
fn location_anchor_line(hunks: &[DiffHunk], location: &Value) -> Option<String> {
    let file = location["file"].as_str()?;
    let target = match &location["line"] {
        Value::Number(number) => number.as_u64()? as usize,
        Value::String(text) => text.trim().parse().ok()?,
        _ => return None,
    };
    let hunk = hunks.iter().find(|hunk| {
        same_file(&hunk.file, file)
            && hunk.new_start <= target
            && target < hunk.new_start + hunk.new_count.max(1)
    })?;
    let mut current = hunk.new_start;
    for line in hunk.lines.iter().skip(1) {
        if line.starts_with('-') || line.starts_with('\\') {
            continue;
        }
        if current == target {
            return Some(line.trim_end().to_string());
        }
        current += 1;
    }
    None
}

fn same_file(hunk_file: &str, location_file: &str) -> bool {
    let hunk_file = hunk_file.trim();
    let location_file = location_file.trim();
    !hunk_file.is_empty()
        && !location_file.is_empty()
        && (hunk_file == location_file
            || hunk_file.ends_with(location_file)
            || location_file.ends_with(hunk_file))
}

fn find_quoted_line(lines: &[String], candidate: &str) -> Option<usize> {
    lines.iter().position(|line| {
        quoted_payload(line).is_some_and(|payload| anchor_matches(candidate, payload))
    })
}

fn quote_line(line: &str) -> String {
    if line.is_empty() {
        ">".to_string()
    } else {
        format!("> {line}")
    }
}

fn quoted_payload(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("> ") {
        Some(rest)
    } else if line.trim_end() == ">" {
        Some("")
    } else {
        None
    }
}

fn normalize_anchor(raw: &str) -> String {
    let trimmed = raw.trim_end_matches(['\r', '\n']);
    let trimmed = trimmed.strip_prefix("> ").unwrap_or(trimmed);
    trimmed.trim_end().to_string()
}

/// Compares an anchor against a diff line, tolerating a dropped or rewritten
/// leading `+`/`-`/space marker, which models get wrong more often than the text.
fn anchor_matches(candidate: &str, line: &str) -> bool {
    let line = line.trim_end();
    if line == candidate {
        return true;
    }
    let candidate = diff_payload(candidate).trim_end();
    let line = diff_payload(line).trim_end();
    !candidate.trim().is_empty() && candidate == line
}

fn diff_payload(line: &str) -> &str {
    let mut characters = line.chars();
    match characters.next() {
        Some('+') | Some('-') | Some(' ') => characters.as_str(),
        _ => line,
    }
}

#[derive(Debug, Clone)]
struct DiffHunk {
    /// Path without the `a/` or `b/` prefix.
    file: String,
    /// The `diff --git` / `---` / `+++` lines owning this hunk.
    header: Vec<String>,
    /// The `@@` line followed by the hunk body.
    lines: Vec<String>,
    new_start: usize,
    new_count: usize,
}

fn parse_diff(diff: &str) -> Vec<DiffHunk> {
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut header: Vec<String> = Vec::new();
    let mut file = String::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            header = vec![line.to_string()];
            file = git_header_path(line).unwrap_or_default();
            continue;
        }
        if line.starts_with("--- ") {
            if header.is_empty() {
                file = String::new();
            }
            header.push(line.to_string());
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            header.push(line.to_string());
            if file.is_empty() {
                file = strip_diff_prefix(path.split('\t').next().unwrap_or_default());
            }
            continue;
        }
        if line.starts_with("@@") {
            let (new_start, new_count) = parse_hunk_range(line).unwrap_or((0, 0));
            hunks.push(DiffHunk {
                file: file.clone(),
                header: header.clone(),
                lines: vec![line.to_string()],
                new_start,
                new_count,
            });
            continue;
        }
        // A mail signature ends the diff; anything after it is not patch content.
        if line.trim_end() == "--" {
            break;
        }
        if let Some(hunk) = hunks.last_mut()
            && (line.starts_with(' ')
                || line.starts_with('+')
                || line.starts_with('-')
                || line.starts_with('\\')
                || line.is_empty())
        {
            hunk.lines.push(line.to_string());
        }
    }
    hunks
}

fn git_header_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?;
    let (_, new) = rest.split_once(" b/")?;
    Some(new.trim().to_string())
}

fn strip_diff_prefix(path: &str) -> String {
    let path = path.trim();
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

fn parse_hunk_range(header: &str) -> Option<(usize, usize)> {
    let plus = header
        .split_whitespace()
        .find(|part| part.starts_with('+'))?;
    let mut parts = plus[1..].split(',');
    let start: usize = parts.next()?.parse().ok()?;
    let count = match parts.next() {
        Some(count) => count.parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

/// Strips what the report style guide forbids and what remote text must not be
/// able to inject, then bounds the length.
fn sanitize_comment(raw: &str) -> Option<String> {
    let cleaned = neutralize_tags(&raw.replace('`', ""));
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return None;
    }
    Some(truncate_comment(cleaned))
}

/// Defuses report tag lines inside untrusted prose so it cannot forge a severity,
/// a finding ID or a source attribution in the web UI's block parser.
fn neutralize_tags(text: &str) -> String {
    const TAGS: [&str; 3] = ["severity:", "finding:", "sources:"];
    // `to_ascii_lowercase` leaves non-ASCII bytes alone, so offsets into the
    // lowercased copy are valid offsets into the original.
    let lower = text.to_ascii_lowercase();
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find('[') {
        let open = cursor + offset;
        result.push_str(&text[cursor..open]);
        let after = &lower[open + 1..];
        if !TAGS.iter().any(|tag| after.trim_start().starts_with(tag)) {
            result.push('[');
            cursor = open + 1;
            continue;
        }
        result.push('(');
        match after.find(']') {
            Some(index) => {
                let close = open + 1 + index;
                result.push_str(&text[open + 1..close]);
                result.push(')');
                cursor = close + 1;
            }
            None => cursor = open + 1,
        }
    }
    result.push_str(&text[cursor..]);
    result
}

fn truncate_comment(text: &str) -> String {
    if text.len() <= MAX_COMMENT_BYTES {
        return text.to_string();
    }
    let cut = text[..MAX_COMMENT_BYTES]
        .rfind('\n')
        .unwrap_or_else(|| floor_char_boundary(text, MAX_COMMENT_BYTES));
    format!("{}\n[comment truncated]", text[..cut].trim_end())
}

fn truncate_bytes(text: &str, limit: usize, suffix: &str) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    format!("{}{suffix}", &text[..floor_char_boundary(text, limit)])
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiRequest, AiResponse, AiUsage, ProviderCapabilities};
    use serde_json::json;
    use tokio::sync::Mutex;

    struct ScriptedProvider {
        responses: Mutex<std::collections::VecDeque<Value>>,
    }

    impl ScriptedProvider {
        fn new(responses: &[Value]) -> Self {
            Self {
                responses: Mutex::new(responses.iter().cloned().collect()),
            }
        }
    }

    #[async_trait::async_trait]
    impl AiProvider for ScriptedProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            let response = self
                .responses
                .lock()
                .await
                .pop_front()
                .context("the test scripted no further responses")?;
            Ok(AiResponse {
                content: Some(response.to_string()),
                thought: None,
                thought_signature: None,
                reasoning: None,
                tool_calls: None,
                usage: Some(AiUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: None,
                    cache_write_tokens: None,
                }),
                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            1
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "scripted".to_string(),
                context_window_size: 1000,
            }
        }
    }

    /// Context lines carry a leading space, which a `\`-continued string literal
    /// would eat, so fixtures are joined instead of written as one literal.
    fn text(lines: &[&str]) -> String {
        lines.join("\n")
    }

    fn diff() -> String {
        text(&[
            "diff --git a/drivers/net/foo.c b/drivers/net/foo.c",
            "index 1111111..2222222 100644",
            "--- a/drivers/net/foo.c",
            "+++ b/drivers/net/foo.c",
            "@@ -10,3 +10,5 @@ static int foo_probe(struct platform_device *pdev)",
            " \tint err;",
            "+\tfoo->wdt_irq = platform_get_irq(pdev, 0);",
            "+\tif (foo->wdt_irq < 0)",
            " \t\treturn -EINVAL;",
            " \treturn 0;",
            "@@ -40,2 +42,3 @@ static void foo_remove(struct platform_device *pdev)",
            " \tfoo_teardown(foo);",
            "+\tkfree(foo->ring);",
            " \t}",
            "",
        ])
    }

    fn block() -> String {
        comment_block(
            "High",
            "peer-abcd1234",
            "peer",
            "Does this leak the mapping?",
        )
    }

    #[test]
    fn splices_after_the_quoted_hunk_holding_the_anchored_line() {
        let report = text(&[
            "> @@ -10,3 +10,5 @@ static int foo_probe(struct platform_device *pdev)",
            "> +\tfoo->wdt_irq = platform_get_irq(pdev, 0);",
            "> +\tif (foo->wdt_irq < 0)",
            "> \t\treturn -EINVAL;",
            "> \treturn 0;",
            "",
            "[ ... ]",
        ]);
        let spliced = splice_comment_block(
            &report,
            &diff(),
            &block(),
            Some("+\tif (foo->wdt_irq < 0)"),
            &json!([]),
        );
        let lines: Vec<&str> = spliced.lines().collect();
        // The comment follows the whole quote, the way a reply does, rather than
        // splitting the hunk at the one line the finding named.
        let end = lines
            .iter()
            .position(|line| line.contains("\treturn 0;"))
            .unwrap();
        assert_eq!(lines[end + 1], "");
        assert_eq!(lines[end + 2], "[Severity: High]");
        assert!(spliced.ends_with("[ ... ]"));
    }

    #[test]
    fn an_unsnipped_quote_keeps_the_comment_beside_its_anchor() {
        let mut report = vec!["> @@ -10,300 +10,300 @@ static int foo_probe(void)".to_string()];
        report.push("> +\tif (foo->wdt_irq < 0)".to_string());
        report
            .extend((0..MAX_QUOTE_RUN_EXTENSION + 10).map(|index| format!("> \tline_{index}();")));
        let spliced = splice_comment_block(
            &report.join("\n"),
            &diff(),
            &block(),
            Some("+\tif (foo->wdt_irq < 0)"),
            &json!([]),
        );
        let lines: Vec<&str> = spliced.lines().collect();
        let anchor = lines
            .iter()
            .position(|line| line.contains("if (foo->wdt_irq < 0)"))
            .unwrap();
        assert_eq!(lines[anchor + 2], "[Severity: High]");
        assert!(spliced.ends_with("> \tline_49();"));
    }

    #[test]
    fn quotes_the_hunk_when_the_report_snipped_it() {
        let report = text(&[
            "> diff --git a/drivers/net/foo.c b/drivers/net/foo.c",
            "> --- a/drivers/net/foo.c",
            "> +++ b/drivers/net/foo.c",
            "> @@ -10,3 +10,5 @@ static int foo_probe(struct platform_device *pdev)",
            "> +\tfoo->wdt_irq = platform_get_irq(pdev, 0);",
            "",
            "[ ... ]",
        ]);
        let spliced = splice_comment_block(
            &report,
            &diff(),
            &block(),
            Some("+\tkfree(foo->ring);"),
            &json!([]),
        );
        assert!(spliced.contains("> @@ -40,2 +42,3 @@"));
        assert!(spliced.contains("> +\tkfree(foo->ring);"));
        // The file is already quoted, so its header is not repeated.
        assert_eq!(spliced.matches("> diff --git").count(), 1);
        let hunk = spliced.find("> +\tkfree(foo->ring);").unwrap();
        assert!(spliced.find("[Severity: High]").unwrap() > hunk);
        assert_eq!(spliced.matches("[ ... ]").count(), 1);
    }

    #[test]
    fn quotes_the_file_header_when_the_report_never_mentioned_the_file() {
        let spliced = splice_comment_block(
            "Some prose about another file.",
            &diff(),
            &block(),
            Some("+\tkfree(foo->ring);"),
            &json!([]),
        );
        assert!(spliced.contains("> diff --git a/drivers/net/foo.c b/drivers/net/foo.c"));
        assert!(spliced.contains("> --- a/drivers/net/foo.c"));
        assert!(spliced.contains("> @@ -40,2 +42,3 @@"));
        assert!(spliced.contains("[ ... ]"));
    }

    #[test]
    fn falls_back_to_locations_when_the_model_anchor_is_wrong() {
        let report = text(&[
            "> @@ -10,3 +10,5 @@ static int foo_probe(struct platform_device *pdev)",
            "> +\tfoo->wdt_irq = platform_get_irq(pdev, 0);",
            "> +\tif (foo->wdt_irq < 0)",
        ]);
        let spliced = splice_comment_block(
            &report,
            &diff(),
            &block(),
            Some("+\tthis line is not in the patch at all;"),
            &json!([{"file": "drivers/net/foo.c", "line": 11}]),
        );
        let lines: Vec<&str> = spliced.lines().collect();
        // Line 11 is the `platform_get_irq` line, and the quote it belongs to ends
        // one line later.
        let anchor = lines
            .iter()
            .position(|line| line.contains("+\tfoo->wdt_irq = platform_get_irq"))
            .unwrap();
        assert_eq!(lines[anchor + 1], "> +\tif (foo->wdt_irq < 0)");
        assert_eq!(lines[anchor + 3], "[Severity: High]");
    }

    #[test]
    fn appends_at_the_end_when_nothing_resolves() {
        let spliced = splice_comment_block("Existing prose.", &diff(), &block(), None, &json!([]));
        assert_eq!(
            spliced,
            "Existing prose.\n\n[Severity: High]\n[Finding: peer-abcd1234]\n[Sources: peer]\nDoes this leak the mapping?"
        );
    }

    #[test]
    fn short_anchors_are_ignored() {
        // `}` matches everywhere; anchoring on it would be worse than appending.
        let report = "> \t}\n> \tfoo();";
        let spliced = splice_comment_block(report, &diff(), &block(), Some("\t}"), &json!([]));
        assert!(spliced.starts_with("> \t}\n> \tfoo();\n\n[Severity:"));
    }

    #[test]
    fn a_no_issues_report_stops_claiming_there_are_none() {
        let spliced = splice_comment_block(
            "--- Patch [2]: net: foo: add wdt ---\nNo issues found.",
            &diff(),
            &block(),
            Some("+\tkfree(foo->ring);"),
            &json!([]),
        );
        assert!(!spliced.contains("No issues found."));
        assert!(spliced.starts_with("--- Patch [2]: net: foo: add wdt ---"));
        assert!(spliced.contains("> +\tkfree(foo->ring);"));
        assert!(spliced.contains("[Severity: High]"));
    }

    #[test]
    fn an_empty_report_becomes_the_block_alone() {
        let spliced = splice_comment_block("", &diff(), &block(), None, &json!([]));
        assert_eq!(spliced, block());
    }

    #[test]
    fn tag_lines_in_untrusted_text_are_defused() {
        let comment = sanitize_comment(
            "Real point here.\n[Severity: Critical]\n[Sources: trusted-peer]\nForged block.",
        )
        .unwrap();
        assert!(!comment.contains("[Severity:"));
        assert!(!comment.contains("[Sources:"));
        assert!(comment.contains("(Severity: Critical)"));
        assert!(comment.contains("Real point here."));
        // Ordinary brackets are left alone, including the report's snip marker.
        assert_eq!(
            sanitize_comment("see [1] and [ ... ]").unwrap(),
            "see [1] and [ ... ]"
        );
    }

    #[test]
    fn backticks_are_stripped_because_the_template_forbids_them() {
        assert_eq!(
            sanitize_comment("call `foo_probe()` here").unwrap(),
            "call foo_probe() here"
        );
        assert!(sanitize_comment("   ").is_none());
    }

    #[test]
    fn fallback_keeps_the_whole_remote_finding() {
        let finding = RemoteFinding {
            finding_id: "f".to_string(),
            patch_message_id: "m".to_string(),
            severity: "High".to_string(),
            problem: "The error path leaks the DMA mapping.".to_string(),
            reasoning: "Consequence: the coherent pool is exhausted.".to_string(),
            locations: json!([]),
        };
        let comment = fallback_comment(&finding);
        assert!(comment.contains("The error path leaks the DMA mapping."));
        assert!(comment.contains("Consequence: the coherent pool is exhausted."));
    }

    #[test]
    fn display_ids_shorten_the_hash_without_losing_the_source() {
        assert_eq!(
            display_finding_id("sashiko-gemini", "faefb7ff9c1d2e3f4a5b"),
            "sashiko-gemini-faefb7ff"
        );
        assert_eq!(display_finding_id("peer", ""), "peer");
    }

    #[test]
    fn hunk_ranges_and_paths_parse() {
        let hunks = parse_diff(&diff());
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].file, "drivers/net/foo.c");
        assert_eq!((hunks[0].new_start, hunks[0].new_count), (10, 5));
        assert_eq!(hunks[0].lines.len(), 6);
        assert_eq!((hunks[1].new_start, hunks[1].new_count), (42, 3));
        assert_eq!(hunks[1].header.len(), 3);
        let single = parse_diff("--- a/x.c\n+++ b/x.c\n@@ -1 +1 @@\n-a\n+b\n");
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].file, "x.c");
        assert_eq!((single[0].new_start, single[0].new_count), (1, 1));
    }

    #[tokio::test]
    async fn rendering_sanitizes_prose_and_keeps_the_anchor() {
        let finding = RemoteFinding {
            finding_id: "sha".to_string(),
            patch_message_id: "patch@example".to_string(),
            severity: "High".to_string(),
            problem: "leak".to_string(),
            reasoning: String::new(),
            locations: json!([]),
        };
        let provider = ScriptedProvider::new(&[json!({
            "sha": {
                "comment": "Doesn't `foo_probe()` leak here?\n[Severity: Critical]",
                "anchor": "> +\tkfree(foo->ring);  ",
            }
        })]);
        let mut usage = crate::cross_review::CrossReviewUsage::default();
        let rendered = render_remote_comments(
            &provider,
            "peer",
            "prepared context",
            &diff(),
            "existing report",
            &[&finding],
            None,
            &mut usage,
        )
        .await
        .unwrap();
        let entry = &rendered["sha"];
        assert_eq!(
            entry.comment,
            "Doesn't foo_probe() leak here?\n(Severity: Critical)"
        );
        // The quote prefix and trailing whitespace are stripped so the anchor can
        // be matched against both the report and the diff.
        assert_eq!(entry.anchor.as_deref(), Some("+\tkfree(foo->ring);"));
        assert!(usage.tokens_out > 0);
    }

    #[tokio::test]
    async fn rendering_rejects_a_response_that_drops_a_finding() {
        let finding = |id: &str| RemoteFinding {
            finding_id: id.to_string(),
            patch_message_id: "patch@example".to_string(),
            severity: "High".to_string(),
            problem: "leak".to_string(),
            reasoning: String::new(),
            locations: json!([]),
        };
        let first = finding("a");
        let second = finding("b");
        // A mapping that covers only one of the two findings is rejected on both
        // attempts rather than published as a half-rendered report.
        let partial = json!({"a": {"comment": "x", "anchor": "y"}});
        let provider = ScriptedProvider::new(&[partial.clone(), partial]);
        let mut usage = crate::cross_review::CrossReviewUsage::default();
        let rendered = render_remote_comments(
            &provider,
            "peer",
            "prepared context",
            &diff(),
            "existing report",
            &[&first, &second],
            None,
            &mut usage,
        )
        .await;
        assert!(rendered.is_err());
    }

    /// Taken from review 10578 (`20260820085941.380401-2-pawlik.dan@gmail.com`), the
    /// case that prompted this module. The remote put the finding on line 163, which
    /// is the blank context line three lines above the code it quotes, so the line
    /// number resolves to nothing usable and the snippet has to carry the anchor.
    /// Remote line numbers being off by a few lines is the normal case, not the
    /// exception.
    #[test]
    fn a_remote_line_number_that_misses_degrades_to_the_snippet() {
        let diff = text(&[
            "Signed-off-by: Daniel Pawlik <pawlik.dan@gmail.com>",
            "---",
            " drivers/net/ethernet/airoha/airoha_npu.c | 25 +++++++++++++---------",
            " 1 file changed, 16 insertions(+), 9 deletions(-)",
            "",
            "diff --git a/drivers/net/ethernet/airoha/airoha_npu.c b/drivers/net/ethernet/airoha/airoha_npu.c",
            "index 4045d1eb93ea..3416f921f961 100644",
            "--- a/drivers/net/ethernet/airoha/airoha_npu.c",
            "+++ b/drivers/net/ethernet/airoha/airoha_npu.c",
            "@@ -160,15 +161,21 @@ struct wlan_mbox_data {",
            " \tDECLARE_FLEX_ARRAY(u8, d);",
            " };",
            " ",
            "+static size_t airoha_npu_mbox_size(size_t len)",
            "+{",
            "+\treturn ALIGN(len, dma_get_cache_alignment());",
            "+}",
            "+",
            " static int airoha_npu_send_msg(struct airoha_npu *npu, int func_id,",
            "",
        ]);
        let report = text(&[
            "> @@ -160,15 +161,21 @@ struct wlan_mbox_data {",
            "> \tDECLARE_FLEX_ARRAY(u8, d);",
            "> };",
            "> ",
            "> +static size_t airoha_npu_mbox_size(size_t len)",
            "> +{",
            "> +\treturn ALIGN(len, dma_get_cache_alignment());",
            "> +}",
            "",
            "[ ... ]",
        ]);
        let locations = json!([{
            "file": "drivers/net/ethernet/airoha/airoha_npu.c",
            "line": 163,
            "function_or_symbol": "airoha_npu_mbox_size",
            "code_snippet": "\treturn ALIGN(len, dma_get_cache_alignment());",
        }]);
        let spliced = splice_comment_block(&report, &diff, &block(), None, &locations);
        let lines: Vec<&str> = spliced.lines().collect();
        let end = lines.iter().position(|line| *line == "> +}").unwrap();
        assert_eq!(lines[end + 2], "[Severity: High]");
        // The mail preamble did not become a hunk, so nothing was re-quoted.
        assert_eq!(spliced.matches("> +\treturn ALIGN").count(), 1);
        assert!(!spliced.contains("1 file changed"));
    }

    #[test]
    fn long_comments_are_bounded() {
        let long = "x".repeat(MAX_COMMENT_BYTES * 2);
        let truncated = truncate_comment(&long);
        assert!(truncated.len() < long.len());
        assert!(truncated.ends_with("[comment truncated]"));
    }
}
