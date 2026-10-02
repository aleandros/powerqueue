//! Model-usage pacing.
//!
//! The problem: a subscription gives a weekly allowance (plus, for most
//! plans, a rolling 5-hour window) that is shared by every model of that
//! provider, and the best model is the scarcest. We want critical work on the
//! best model immediately, routine work on cheaper models, and no reserved
//! capacity wasted at the end of the week. Each provider (Claude, Codex,
//! Gemini) has its own allowance and its own ledger.
//!
//! * [`period`]    – where we are in a provider's period and window.
//! * [`ledger`]    – what has been spent, per model, from the usage table;
//!   [`Ledgers`] holds one ledger per enabled provider.
//! * [`probe`]     – what a provider reports about its own allowance.
//! * [`estimator`] – how much a task will probably cost, learned from history.
//! * [`policy`]    – the decision: which model (if any) a task may use now.

pub mod estimator;
pub mod ledger;
pub mod period;
pub mod policy;
pub mod probe;

pub use estimator::{Estimator, Prediction, Sample};
pub use ledger::{CALIBRATION_KEY, Calibration, Ledger, Ledgers, TierLedger, calibration_key, tier_share, tier_weight};
pub use period::{AnchorSource, Period, PeriodClock};
pub use policy::{Decision, Policy, RATE_LIMITS_KEY, RateLimitState, WINDOW_RECHECK};
pub use probe::{NoProbe, ObservedUsage, UsageProbe, apply_observed, load_observed, observed_key, save_observed};
