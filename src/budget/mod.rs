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
//! * [`probe`]     – what a provider reports about its own allowance;
//!   [`probes`] runs the real probes (Codex app-server, Claude status line, agy `/usage`).
//! * [`estimator`] – how much a task will probably cost, learned from history.
//! * [`policy`]    – the decision: which model (if any) a task may use now.
//! * [`io`]        – the only module here that touches the store: it reads
//!   the rows the pure builders above work on and persists observations.
//!
//! `period`, `ledger`, `probe`, `estimator` and `policy` are pure: plain
//! data in, plain data out, so the pacing can be tested (and simulated by
//! `budget plan` / `priority simulate`) without a database.

pub mod estimator;
pub mod io;
pub mod ledger;
pub mod period;
pub mod policy;
pub mod probe;
pub mod probes;

pub use estimator::{Estimator, Prediction, Sample};
pub use io::{load_observations, load_observed, record_observation, save_observed};
pub use ledger::{
    CALIBRATION_KEY, LearnedRate, Ledger, LedgerSource, Ledgers, MIN_MEASURED_DELTA, MIN_OBSERVED_DELTA, RateEstimate,
    TierLedger, WINDOW_LEARN_SPAN, calibration_key, learn_rate, resolve_clock, tier_share, tier_weight,
};
pub use period::{AnchorSource, Period, PeriodClock};
pub use policy::{Decision, OBSERVED_EXHAUSTED_TTL, Policy, RATE_LIMITS_KEY, RateLimitState, WINDOW_RECHECK};
pub use probe::{
    NoProbe, ObservationSample, ObservedUsage, UsageProbe, append_observation, apply_observed, fold_legacy_calibration,
    observations_key, observed_key, thin_samples,
};
pub use probes::{ProbeStatus, load_probe_status, probe_all, probe_providers};
