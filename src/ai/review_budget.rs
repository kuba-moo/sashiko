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
    pub warn_pct: f32,
    pub severe_pct: f32,
    pub review_multiplier: f32,
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

    pub fn review_past_warn(&self) -> bool {
        self.flags() & (REVIEW_INPUT_WARN | REVIEW_OUTPUT_WARN) != 0
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
        let review_input_limit =
            (self.config.stage_input as f32 * self.config.review_multiplier) as usize;
        let review_output_limit =
            (self.config.stage_output as f32 * self.config.review_multiplier) as usize;

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
            warn_pct: 0.5,
            severe_pct: 0.9,
            review_multiplier: 2.0,
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
            warn_pct: 0.5,
            severe_pct: 0.9,
            review_multiplier: 0.0,
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
}
