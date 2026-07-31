// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

//! Tracks how often models return structured responses we cannot use.
//!
//! A malformed reply that is silently swallowed looks identical to a model that
//! simply found nothing, which is how a 69% confirmation failure rate went
//! unnoticed. Recording decode and response-assembly failures, and whether
//! restating the schema recovered them, makes that class of regression visible.
//!
//! Reviews run in a subprocess without database access, so events are collected
//! in process memory, serialized into the review result, and persisted by the
//! parent. Parent-side callers (cross-review) drain and persist directly.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// How a decode problem ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonDecodeOutcome {
    /// The reply was not valid JSON but an embedded object was recovered
    /// without another model call.
    Salvaged,
    /// A retry that restated the schema produced a usable reply.
    RecoveredOnRetry,
    /// No attempt produced usable JSON; the caller gave up.
    Fatal,
}

impl JsonDecodeOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Salvaged => "salvaged",
            Self::RecoveredOnRetry => "recovered_on_retry",
            Self::Fatal => "fatal",
        }
    }
}

/// One structured-response problem, attributed to the request that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonDecodeEvent {
    /// Which request produced it, e.g. `stage:10`, `confirmation`,
    /// `cross-review:dedup`. Stays low-cardinality so it groups usefully.
    pub source: String,
    pub outcome: JsonDecodeOutcome,
    /// First line of the parse error, for display. Truncated to stay bounded.
    pub detail: String,
}

static EVENTS: Mutex<Vec<JsonDecodeEvent>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) static TEST_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Caps retained events so a pathological model cannot exhaust memory.
const MAX_EVENTS: usize = 512;

/// Trims an error to its first line so grouping stays meaningful.
fn summarize(detail: &str) -> String {
    let first_line = detail.lines().next().unwrap_or("").trim();
    let mut summary: String = first_line.chars().take(160).collect();
    if summary.is_empty() {
        summary.push_str("unspecified decode failure");
    }
    summary
}

/// Records a decode problem. Never panics: a poisoned lock or a full buffer
/// drops the event rather than disrupting the review.
pub fn record(source: impl Into<String>, outcome: JsonDecodeOutcome, detail: &str) {
    let event = JsonDecodeEvent {
        source: source.into(),
        outcome,
        detail: summarize(detail),
    };
    tracing::warn!(
        "JSON decode {} for {}: {}",
        outcome.as_str(),
        event.source,
        event.detail
    );
    if let Ok(mut events) = EVENTS.lock()
        && events.len() < MAX_EVENTS
    {
        events.push(event);
    }
}

/// Removes and returns everything recorded so far.
pub fn drain() -> Vec<JsonDecodeEvent> {
    EVENTS
        .lock()
        .map(|mut events| std::mem::take(&mut *events))
        .unwrap_or_default()
}

/// Drains into the JSON array carried in a review result.
pub fn drain_to_json() -> serde_json::Value {
    serde_json::to_value(drain()).unwrap_or_else(|_| serde_json::json!([]))
}

/// Parses the array produced by [`drain_to_json`], ignoring malformed entries.
pub fn from_json(value: &serde_json::Value) -> Vec<JsonDecodeEvent> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| serde_json::from_value(item.clone()).ok())
        .collect()
}

/// Names the request behind an analysis or merge stage.
pub fn stage_source(stage: u8) -> String {
    format!("stage:{stage}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // The recorder is process-global, so these run under one lock to stay
    // independent of test execution order.
    #[test]
    fn events_round_trip_through_json() {
        let _guard = TEST_GUARD.blocking_lock();
        drain();
        record("stage:10", JsonDecodeOutcome::Salvaged, "trailing garbage");
        record("confirmation", JsonDecodeOutcome::Fatal, "expected value");
        let encoded = drain_to_json();
        let decoded = from_json(&encoded);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].source, "stage:10");
        assert_eq!(decoded[0].outcome, JsonDecodeOutcome::Salvaged);
        assert_eq!(decoded[1].outcome, JsonDecodeOutcome::Fatal);
        // Draining clears the buffer.
        assert!(drain().is_empty());
    }

    #[test]
    fn details_are_summarized_to_one_bounded_line() {
        let _guard = TEST_GUARD.blocking_lock();
        drain();
        record(
            "stage:3",
            JsonDecodeOutcome::Fatal,
            &format!("first line\nsecond line{}", "x".repeat(500)),
        );
        let events = drain();
        assert_eq!(events[0].detail, "first line");

        record("stage:3", JsonDecodeOutcome::Fatal, &"y".repeat(400));
        let events = drain();
        assert_eq!(events[0].detail.chars().count(), 160);

        record("stage:3", JsonDecodeOutcome::Fatal, "   \n  ");
        let events = drain();
        assert_eq!(events[0].detail, "unspecified decode failure");
    }

    #[test]
    fn stage_source_is_stable() {
        assert_eq!(stage_source(8), "stage:8");
    }
}
