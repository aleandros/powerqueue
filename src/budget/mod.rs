//! Model-usage pacing.
//!
//! The problem: a Claude subscription gives a weekly allowance (plus a rolling
//! 5-hour window) that is shared by every model, and Fable is both the best
//! and the scarcest. We want critical work on Fable immediately, routine work
//! on cheaper tiers, and no Fable capacity wasted at the end of the week.
//!
//! * [`period`]    – where we are in the weekly period and 5h window.
//! * [`ledger`]    – what has been spent, per tier, from the usage table.
//! * [`estimator`] – how much a task will probably cost, learned from history.
//! * [`policy`]    – the decision: which tier (if any) a task may use now.

pub mod estimator;
pub mod ledger;
pub mod period;
pub mod policy;

pub use estimator::{Estimator, Prediction};
pub use ledger::{Ledger, TierLedger};
pub use period::{Period, PeriodClock};
pub use policy::{Decision, Policy, RateLimitState};
