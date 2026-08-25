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

//! Keeps `patchsets.embargo_until` tracking the external release schedule.
//!
//! Runs as its own task rather than inside the reviewer service loop: that loop
//! runs embargo release and cross-review ahead of dispatch specifically so
//! neither is starved, and an HTTP fetch there would stall both for as long as
//! the request takes.

use crate::db::Database;
use crate::embargo_schedule::{EmbargoSchedule, desired_embargo_until};
use crate::settings::EmbargoSettings;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

/// How far back to consider embargoed patchsets. The schedule only covers
/// recent series, so a wider window would just re-scan rows that can never
/// match.
const LOOKBACK_SECS: i64 = 30 * 24 * 3600;

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct EmbargoScheduleWorker {
    db: Arc<Database>,
    settings: EmbargoSettings,
    schedule_url: String,
    client: reqwest::Client,
}

/// Per-cycle tally, kept separate from the logging so it can be asserted on.
#[derive(Debug, Default, PartialEq, Eq)]
struct RefreshStats {
    candidates: usize,
    matched: usize,
    updated: usize,
    clamped: usize,
    skipped: usize,
}

impl EmbargoScheduleWorker {
    /// Returns `None` when dynamic scheduling is not configured.
    pub fn new(db: Arc<Database>, settings: EmbargoSettings) -> Option<Self> {
        let schedule_url = settings.schedule_url.clone()?;
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .unwrap_or_default();
        Some(Self {
            db,
            settings,
            schedule_url,
            client,
        })
    }

    pub async fn run(&self) {
        info!(
            "Starting Embargo Schedule Worker: {} every {}min, releasing {}h before target, \
             holding at most {}h{}",
            self.schedule_url,
            self.settings.refresh_minutes,
            self.settings.release_lead_hours,
            self.settings.max_hold_hours,
            if self.settings.dry_run {
                " (dry run)"
            } else {
                ""
            }
        );

        let interval = Duration::from_secs(self.settings.refresh_minutes.max(1) * 60);
        loop {
            match self.refresh_once().await {
                Ok(stats) => info!(
                    "Embargo schedule: {} embargoed patchsets, {} in schedule, {} release times \
                     updated ({} capped at {}h){}",
                    stats.candidates,
                    stats.matched,
                    stats.updated,
                    stats.clamped,
                    self.settings.max_hold_hours,
                    if stats.skipped > 0 {
                        format!(", {} skipped (released or releasing)", stats.skipped)
                    } else {
                        String::new()
                    }
                ),
                // A failed refresh leaves every stored embargo alone, so
                // network trouble can neither publish findings early nor
                // extend a hold.
                Err(e) => error!("Embargo schedule refresh failed: {}", e),
            }
            sleep(interval).await;
        }
    }

    async fn refresh_once(&self) -> Result<RefreshStats> {
        let schedule = EmbargoSchedule::fetch(&self.client, &self.schedule_url).await?;
        let now = chrono::Utc::now().timestamp();

        if schedule.is_stale(now) {
            warn!(
                "Embargo schedule is {}h old; newly posted series may not be covered yet",
                schedule.age_secs(now).unwrap_or_default() / 3600
            );
        }

        let candidates = self
            .db
            .get_dynamic_embargo_candidates(now, now - LOOKBACK_SECS)
            .await?;
        debug!(
            "Embargo schedule: {} entries, {} embargoed patchsets to check",
            schedule.len(),
            candidates.len()
        );

        let mut stats = RefreshStats {
            candidates: candidates.len(),
            ..Default::default()
        };

        for candidate in candidates {
            // A series the schedule does not list keeps whatever the static
            // policy set at ingestion. Absence is not a signal: entries age out
            // of the file, and treating that as "release now" would publish on
            // the file's retention window rather than on a decision.
            let Some(target) = schedule.target_for(&candidate.message_id) else {
                continue;
            };
            stats.matched += 1;

            let desired = desired_embargo_until(
                target,
                candidate.date,
                self.settings.release_lead_hours,
                self.settings.max_hold_hours,
            );
            if desired.clamped {
                stats.clamped += 1;
            }
            if desired.until == candidate.embargo_until {
                continue;
            }

            info!(
                "Patchset {}: embargo {} -> {} ({} target {}, lead {}h{})",
                candidate.id,
                format_time(candidate.embargo_until),
                format_time(desired.until),
                if self.settings.dry_run {
                    "dry run,"
                } else {
                    "schedule"
                },
                format_time(target),
                self.settings.release_lead_hours,
                if desired.clamped {
                    format!(
                        ", capped at {}h from series date",
                        self.settings.max_hold_hours
                    )
                } else {
                    String::new()
                }
            );

            if self.settings.dry_run {
                stats.updated += 1;
                continue;
            }

            match self
                .db
                .set_patchset_embargo_until_if_unclaimed(candidate.id, desired.until, now)
                .await
            {
                // The row was released or claimed between the read and the
                // write. Leaving it alone is the correct outcome.
                Ok(false) => stats.skipped += 1,
                Ok(true) => stats.updated += 1,
                Err(e) => {
                    error!(
                        "Failed to update embargo for patchset {}: {}",
                        candidate.id, e
                    );
                }
            }
        }

        Ok(stats)
    }
}

fn format_time(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%MZ").to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_timestamps_as_utc_minutes() {
        assert_eq!(format_time(1787611453), "2026-08-24T22:44Z");
    }
}
