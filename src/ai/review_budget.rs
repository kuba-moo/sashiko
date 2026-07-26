use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

pub const STAGE_INPUT_WARN: u8 = 1 << 0;
pub const STAGE_INPUT_SEVERE: u8 = 1 << 1;
pub const STAGE_OUTPUT_WARN: u8 = 1 << 2;
pub const STAGE_OUTPUT_SEVERE: u8 = 1 << 3;
pub const REVIEW_INPUT_WARN: u8 = 1 << 4;
pub const REVIEW_INPUT_SEVERE: u8 = 1 << 5;
pub const REVIEW_OUTPUT_WARN: u8 = 1 << 6;
pub const REVIEW_OUTPUT_SEVERE: u8 = 1 << 7;

#[derive(Clone, Copy)]
pub struct BudgetConfig {
    pub stage_input: usize,
    pub stage_output: usize,
    pub review_input: usize,
    pub review_output: usize,
    pub warn_pct: f32,
    pub severe_pct: f32,
    pub review_multiplier: f32,
    pub enforce_hard_limits: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSnapshot {
    pub input: usize,
    pub output: usize,
    pub flags: u8,
}

#[derive(Clone)]
pub struct ReviewBudget {
    config: BudgetConfig,
    review_input: Arc<AtomicUsize>,
    review_output: Arc<AtomicUsize>,
    flags: Arc<AtomicU8>,
}

pub enum BudgetLevel {
    Warn,
    Severe,
}

impl ReviewBudget {
    pub fn new(config: BudgetConfig) -> Self {
        Self {
            config,
            review_input: Arc::new(AtomicUsize::new(0)),
            review_output: Arc::new(AtomicUsize::new(0)),
            flags: Arc::new(AtomicU8::new(0)),
        }
    }

    pub fn flags(&self) -> u8 {
        self.flags.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            input: self.review_input.load(Ordering::Relaxed),
            output: self.review_output.load(Ordering::Relaxed),
            flags: self.flags(),
        }
    }

    pub fn review_past_warn(&self) -> bool {
        self.flags() & (REVIEW_INPUT_WARN | REVIEW_OUTPUT_WARN) != 0
    }

    pub fn allows(&self, stage_input: usize, stage_output: usize) -> bool {
        let snapshot = self.snapshot();
        let review_input_limit = self.review_input_limit();
        let review_output_limit = self.review_output_limit();
        (self.config.stage_input == 0 || stage_input <= self.config.stage_input)
            && (self.config.stage_output == 0 || stage_output <= self.config.stage_output)
            && (review_input_limit == 0
                || snapshot.input.saturating_add(stage_input) <= review_input_limit)
            && (review_output_limit == 0
                || snapshot.output.saturating_add(stage_output) <= review_output_limit)
    }

    pub fn allows_request_input(&self, stage_input: usize) -> bool {
        !self.config.enforce_hard_limits
            || self.config.stage_input == 0
            || stage_input <= self.config.stage_input
    }

    pub fn hard_limit_exceeded(&self, stage_input: usize, stage_output: usize) -> bool {
        if !self.config.enforce_hard_limits {
            return false;
        }
        let snapshot = self.snapshot();
        (self.config.stage_input > 0 && stage_input > self.config.stage_input)
            || (self.config.stage_output > 0 && stage_output > self.config.stage_output)
            || (self.review_input_limit() > 0 && snapshot.input > self.review_input_limit())
            || (self.review_output_limit() > 0 && snapshot.output > self.review_output_limit())
    }

    fn review_input_limit(&self) -> usize {
        if self.config.review_input > 0 {
            self.config.review_input
        } else {
            (self.config.stage_input as f32 * self.config.review_multiplier) as usize
        }
    }

    fn review_output_limit(&self) -> usize {
        if self.config.review_output > 0 {
            self.config.review_output
        } else {
            (self.config.stage_output as f32 * self.config.review_multiplier) as usize
        }
    }

    pub fn record_and_check(
        &self,
        stage_flags: &mut u8,
        stage_input: usize,
        stage_output: usize,
        input_delta: usize,
        output_delta: usize,
    ) -> Option<BudgetLevel> {
        let review_input =
            self.review_input.fetch_add(input_delta, Ordering::Relaxed) + input_delta;
        let review_output = self
            .review_output
            .fetch_add(output_delta, Ordering::Relaxed)
            + output_delta;
        let review_input_limit = self.review_input_limit();
        let review_output_limit = self.review_output_limit();

        let stage_values = [
            (
                stage_input,
                self.config.stage_input,
                STAGE_INPUT_WARN,
                STAGE_INPUT_SEVERE,
            ),
            (
                stage_output,
                self.config.stage_output,
                STAGE_OUTPUT_WARN,
                STAGE_OUTPUT_SEVERE,
            ),
        ];

        let mut level = None;
        for (used, limit, warn_flag, severe_flag) in stage_values {
            if limit == 0 {
                continue;
            }
            let severe = (limit as f32 * self.config.severe_pct) as usize;
            let warn = (limit as f32 * self.config.warn_pct) as usize;
            if severe > 0 && used >= severe {
                let old = *stage_flags;
                *stage_flags |= severe_flag | warn_flag;
                self.flags
                    .fetch_or(severe_flag | warn_flag, Ordering::Relaxed);
                if old & severe_flag == 0 {
                    level = Some(BudgetLevel::Severe);
                }
            } else if warn > 0 && used >= warn {
                let old = *stage_flags;
                *stage_flags |= warn_flag;
                self.flags.fetch_or(warn_flag, Ordering::Relaxed);
                if old & warn_flag == 0 && level.is_none() {
                    level = Some(BudgetLevel::Warn);
                }
            }
        }

        let review_values = [
            (
                review_input,
                review_input_limit,
                REVIEW_INPUT_WARN,
                REVIEW_INPUT_SEVERE,
            ),
            (
                review_output,
                review_output_limit,
                REVIEW_OUTPUT_WARN,
                REVIEW_OUTPUT_SEVERE,
            ),
        ];
        for (used, limit, warn_flag, severe_flag) in review_values {
            if limit == 0 {
                continue;
            }
            let severe = (limit as f32 * self.config.severe_pct) as usize;
            let warn = (limit as f32 * self.config.warn_pct) as usize;
            if severe > 0 && used >= severe {
                let old = self
                    .flags
                    .fetch_or(severe_flag | warn_flag, Ordering::Relaxed);
                if old & severe_flag == 0 {
                    level = Some(BudgetLevel::Severe);
                }
            } else if warn > 0 && used >= warn {
                let old = self.flags.fetch_or(warn_flag, Ordering::Relaxed);
                if old & warn_flag == 0 && level.is_none() {
                    level = Some(BudgetLevel::Warn);
                }
            }
        }
        level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_stage_and_review_thresholds_once() {
        let budget = ReviewBudget::new(BudgetConfig {
            stage_input: 100,
            stage_output: 50,
            review_input: 0,
            review_output: 0,
            warn_pct: 0.5,
            severe_pct: 0.9,
            review_multiplier: 2.0,
            enforce_hard_limits: false,
        });
        let mut stage_flags = 0;

        assert!(matches!(
            budget.record_and_check(&mut stage_flags, 50, 0, 50, 0),
            Some(BudgetLevel::Warn)
        ));
        assert!(
            budget
                .record_and_check(&mut stage_flags, 60, 0, 10, 0)
                .is_none()
        );
        assert!(matches!(
            budget.record_and_check(&mut stage_flags, 90, 0, 30, 0),
            Some(BudgetLevel::Severe)
        ));
        assert_eq!(
            budget.flags() & (STAGE_INPUT_WARN | STAGE_INPUT_SEVERE),
            STAGE_INPUT_WARN | STAGE_INPUT_SEVERE
        );
    }

    #[test]
    fn stage_thresholds_are_independent_between_sessions() {
        let budget = ReviewBudget::new(BudgetConfig {
            stage_input: 100,
            stage_output: 0,
            review_input: 0,
            review_output: 0,
            warn_pct: 0.5,
            severe_pct: 0.9,
            review_multiplier: 0.0,
            enforce_hard_limits: false,
        });
        let mut first_stage = 0;
        let mut second_stage = 0;

        assert!(matches!(
            budget.record_and_check(&mut first_stage, 50, 0, 50, 0),
            Some(BudgetLevel::Warn)
        ));
        assert!(matches!(
            budget.record_and_check(&mut second_stage, 50, 0, 50, 0),
            Some(BudgetLevel::Warn)
        ));
    }

    #[test]
    fn explicit_review_limits_override_the_multiplier() {
        let budget = ReviewBudget::new(BudgetConfig {
            stage_input: 100,
            stage_output: 100,
            review_input: 50,
            review_output: 25,
            warn_pct: 0.5,
            severe_pct: 0.9,
            review_multiplier: 100.0,
            enforce_hard_limits: true,
        });
        let mut stage_flags = 0;

        assert!(matches!(
            budget.record_and_check(&mut stage_flags, 25, 13, 25, 13),
            Some(BudgetLevel::Warn)
        ));
        assert_eq!(budget.snapshot().input, 25);
        assert_eq!(budget.snapshot().output, 13);
    }

    #[test]
    fn allows_checks_source_owned_stage_and_review_limits() {
        let budget = ReviewBudget::new(BudgetConfig {
            stage_input: 100,
            stage_output: 50,
            review_input: 150,
            review_output: 75,
            warn_pct: 0.5,
            severe_pct: 0.9,
            review_multiplier: 0.0,
            enforce_hard_limits: true,
        });
        let mut flags = 0;
        assert!(budget.allows(100, 50));
        budget.record_and_check(&mut flags, 80, 30, 80, 30);
        assert!(!budget.allows(80, 20));
        assert!(!budget.allows(20, 50));
        assert!(budget.allows(20, 20));
    }

    #[test]
    fn explicit_limits_check_stage_estimates_and_actual_review_usage() {
        let budget = ReviewBudget::new(BudgetConfig {
            stage_input: 100,
            stage_output: 0,
            review_input: 150,
            review_output: 0,
            warn_pct: 0.5,
            severe_pct: 0.9,
            review_multiplier: 0.0,
            enforce_hard_limits: true,
        });
        assert!(budget.allows_request_input(80));
        assert!(!budget.allows_request_input(101));
        let mut flags = 0;
        budget.record_and_check(&mut flags, 80, 30, 80, 30);
        assert!(!budget.hard_limit_exceeded(80, 30));
        budget.record_and_check(&mut flags, 80, 50, 80, 20);
        assert!(budget.hard_limit_exceeded(80, 50));
    }
}
