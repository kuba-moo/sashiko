// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0

//! Dynamic embargo schedule fetched from an external master file.
//!
//! The static embargo set at ingestion (`embargo_hours` in `email_policy.toml`)
//! is a fixed offset from receipt, which says nothing about when a series is
//! actually expected to be acted on. The schedule file maps a series to a
//! *target release time*, so the embargo can track that instead: hold findings
//! until `target - release_lead_hours`.
//!
//! Parsing and the release-time decision live here, free of I/O and database
//! access; `crate::worker::embargo` drives them.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use tracing::warn;

/// Cap on the downloaded payload. The netdev file holds roughly one entry per
/// live series (~500, ~60 KB); the cap only exists so a misconfigured URL
/// pointing at something huge cannot exhaust memory.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Age past which the schedule is reported as stale. Targets are absolute
/// timestamps, so a stale file is not dangerous — it just stops covering
/// newly-posted series, which is worth a warning.
const STALE_AFTER_SECS: i64 = 24 * 3600;

/// Raw shape of the schedule JSON.
///
/// `by_series_id` is deliberately ignored: patchwork series IDs only ever reach
/// us through `patchwork_patch_state.pw_series_id`, which is populated by
/// patchwork event ingestion. Deployments that do not run it have no series IDs
/// at all, while the cover-letter message-ID is always present, so message-ID is
/// the only key we can rely on.
#[derive(Debug, Deserialize)]
struct RawSchedule {
    #[serde(default)]
    by_message_id: HashMap<String, String>,
    #[serde(default)]
    generated_at: Option<String>,
}

/// Series-to-target-release-time map, keyed by the message-ID of the series'
/// cover letter (or of the sole patch, for a single-patch series).
#[derive(Debug, Default, Clone)]
pub struct EmbargoSchedule {
    by_message_id: HashMap<String, i64>,
    generated_at: Option<i64>,
}

/// The release time a series should hold until, and how it was derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesiredEmbargo {
    /// Unix timestamp to store in `patchsets.embargo_until`.
    pub until: i64,
    /// True when `max_hold_hours` bound the result rather than the target.
    pub clamped: bool,
}

impl EmbargoSchedule {
    /// Parse a schedule from raw JSON bytes.
    ///
    /// Entries whose timestamp does not parse are dropped with a warning rather
    /// than failing the whole file: one bad row must not cost us the other 500.
    /// An empty map is rejected, because it is indistinguishable from a
    /// truncated or wrong-shaped payload and applying it would be a no-op.
    pub fn from_json(body: &str) -> Result<Self> {
        let raw: RawSchedule =
            serde_json::from_str(body).context("failed to parse embargo schedule JSON")?;

        let total = raw.by_message_id.len();
        let by_message_id: HashMap<String, i64> = raw
            .by_message_id
            .into_iter()
            .filter_map(|(msg_id, target)| {
                match crate::patchwork::parse_patchwork_timestamp(&target) {
                    Some(ts) => Some((normalize_message_id(&msg_id), ts)),
                    None => {
                        warn!(
                            "Embargo schedule: unparseable target time {:?} for {}",
                            target, msg_id
                        );
                        None
                    }
                }
            })
            .collect();

        if by_message_id.is_empty() {
            anyhow::bail!(
                "embargo schedule has no usable by_message_id entries ({} raw)",
                total
            );
        }

        Ok(Self {
            by_message_id,
            generated_at: raw
                .generated_at
                .as_deref()
                .and_then(crate::patchwork::parse_patchwork_timestamp),
        })
    }

    /// Fetch and parse the schedule.
    pub async fn fetch(client: &Client, url: &str) -> Result<Self> {
        let response = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("failed to fetch embargo schedule from {}", url))?;

        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("embargo schedule fetch returned {}", status);
        }

        let body = response
            .text()
            .await
            .context("failed to read embargo schedule body")?;
        if body.len() > MAX_RESPONSE_BYTES {
            anyhow::bail!(
                "embargo schedule is {} bytes, over the {} byte limit",
                body.len(),
                MAX_RESPONSE_BYTES
            );
        }

        Self::from_json(&body)
    }

    /// Number of series in the schedule.
    pub fn len(&self) -> usize {
        self.by_message_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_message_id.is_empty()
    }

    /// Target release time for a series, by cover-letter message-ID.
    pub fn target_for(&self, message_id: &str) -> Option<i64> {
        self.by_message_id
            .get(&normalize_message_id(message_id))
            .copied()
    }

    /// Age of the file in seconds, when it reported a generation time.
    pub fn age_secs(&self, now: i64) -> Option<i64> {
        self.generated_at.map(|generated| now - generated)
    }

    /// Whether the file is old enough to be worth warning about.
    pub fn is_stale(&self, now: i64) -> bool {
        self.age_secs(now)
            .map(|age| age > STALE_AFTER_SECS)
            .unwrap_or(false)
    }
}

/// Message-IDs reach us in both bare and bracketed form depending on the
/// source, so compare them in one shape. The netdev file uses the bare form,
/// which is also what `patchsets.cover_letter_message_id` stores.
fn normalize_message_id(message_id: &str) -> String {
    message_id
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

/// Release time for a series with a known target.
///
/// The target wins outright over the static policy value — earlier or later —
/// for any series the file lists. `max_hold_hours` measured from `series_date`
/// is the only bound, so a stale or bogus far-future target cannot bury
/// findings indefinitely.
pub fn desired_embargo_until(
    target: i64,
    series_date: i64,
    release_lead_hours: u32,
    max_hold_hours: u32,
) -> DesiredEmbargo {
    let from_target = target - (release_lead_hours as i64) * 3600;
    let cap = series_date + (max_hold_hours as i64) * 3600;

    if from_target > cap {
        DesiredEmbargo {
            until: cap,
            clamped: true,
        }
    } else {
        DesiredEmbargo {
            until: from_target,
            clamped: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape taken verbatim from
    /// https://netdev-ctrl.bots.linux.dev/suie-scores.json
    const SAMPLE: &str = r#"{
        "by_message_id": {
            "20260824175938.11143-1-skwkevin@mail.ustc.edu.cn": "2026-08-26T17:27:19Z",
            "1b7ebced14243c1b342923a25bcc44ec77c45907.1787086511.git.ralf@mandelbit.com": "2026-08-24T00:09:38Z"
        },
        "by_series_id": {
            "1146632": "2026-08-25T04:15:05Z"
        },
        "generated_at": "2026-08-24T22:44:13Z"
    }"#;

    #[test]
    fn parses_sample_payload() {
        let schedule = EmbargoSchedule::from_json(SAMPLE).unwrap();
        assert_eq!(schedule.len(), 2);
        assert_eq!(
            schedule.target_for("20260824175938.11143-1-skwkevin@mail.ustc.edu.cn"),
            Some(1787765239) // 2026-08-26T17:27:19Z
        );
        // by_series_id is intentionally not consulted.
        assert_eq!(schedule.target_for("1146632"), None);
        assert_eq!(schedule.generated_at, Some(1787611453)); // 2026-08-24T22:44:13Z
    }

    #[test]
    fn matches_bracketed_message_ids() {
        let schedule = EmbargoSchedule::from_json(SAMPLE).unwrap();
        assert_eq!(
            schedule.target_for("<20260824175938.11143-1-skwkevin@mail.ustc.edu.cn>"),
            schedule.target_for("20260824175938.11143-1-skwkevin@mail.ustc.edu.cn")
        );
    }

    #[test]
    fn unknown_message_id_has_no_target() {
        let schedule = EmbargoSchedule::from_json(SAMPLE).unwrap();
        assert_eq!(schedule.target_for("nobody@example.com"), None);
    }

    #[test]
    fn drops_unparseable_targets_but_keeps_the_rest() {
        let body = r#"{"by_message_id": {"good@example.com": "2026-08-26T17:27:19Z",
                                          "bad@example.com": "not a timestamp"}}"#;
        let schedule = EmbargoSchedule::from_json(body).unwrap();
        assert_eq!(schedule.len(), 1);
        assert!(schedule.target_for("good@example.com").is_some());
        assert!(schedule.target_for("bad@example.com").is_none());
    }

    #[test]
    fn rejects_payload_without_usable_entries() {
        assert!(
            EmbargoSchedule::from_json(r#"{"by_series_id": {"1": "2026-08-26T17:27:19Z"}}"#)
                .is_err()
        );
        assert!(EmbargoSchedule::from_json("{}").is_err());
        assert!(EmbargoSchedule::from_json("not json").is_err());
    }

    #[test]
    fn staleness_needs_a_generation_time() {
        let generated_at = 1787611453; // 2026-08-24T22:44:13Z
        let fresh = EmbargoSchedule::from_json(SAMPLE).unwrap();
        assert_eq!(fresh.age_secs(generated_at + 3600), Some(3600));
        assert!(!fresh.is_stale(generated_at + 3600));
        assert!(fresh.is_stale(generated_at + 48 * 3600));

        let undated =
            EmbargoSchedule::from_json(r#"{"by_message_id": {"a@b": "2026-08-26T17:27:19Z"}}"#)
                .unwrap();
        assert_eq!(undated.age_secs(generated_at), None);
        assert!(!undated.is_stale(i64::MAX / 2));
    }

    const DAY: i64 = 86400;

    #[test]
    fn target_extends_past_the_static_window() {
        let series_date = 1_787_000_000;
        // Target four days out, 24h lead: holds three days, well past the 24h
        // static policy value.
        let desired = desired_embargo_until(series_date + 4 * DAY, series_date, 24, 168);
        assert_eq!(desired.until, series_date + 3 * DAY);
        assert!(!desired.clamped);
    }

    #[test]
    fn target_in_the_past_releases_immediately() {
        let series_date = 1_787_000_000;
        let desired = desired_embargo_until(series_date - DAY, series_date, 24, 168);
        assert_eq!(desired.until, series_date - DAY - 24 * 3600);
        assert!(!desired.clamped);
        // Releasable: the computed time is behind the series date, so behind now.
        assert!(desired.until < series_date);
    }

    #[test]
    fn cap_bounds_a_far_future_target() {
        let series_date = 1_787_000_000;
        let desired = desired_embargo_until(series_date + 40 * DAY, series_date, 24, 168);
        assert_eq!(desired.until, series_date + 7 * DAY);
        assert!(desired.clamped);
    }

    #[test]
    fn cap_exactly_at_the_bound_is_not_clamped() {
        let series_date = 1_787_000_000;
        let target = series_date + 7 * DAY + 24 * 3600;
        let desired = desired_embargo_until(target, series_date, 24, 168);
        assert_eq!(desired.until, series_date + 7 * DAY);
        assert!(!desired.clamped);
    }

    #[test]
    fn zero_lead_holds_until_the_target_itself() {
        let series_date = 1_787_000_000;
        let target = series_date + 2 * DAY;
        let desired = desired_embargo_until(target, series_date, 0, 168);
        assert_eq!(desired.until, target);
    }
}
