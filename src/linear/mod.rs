//! Linear GraphQL client and issue → task synchronisation.
//!
//! Authentication uses a personal API key sent as `Authorization: <key>`
//! (no `Bearer` prefix). Only the handful of queries/mutations powerqueue
//! needs are implemented; everything goes through [`LinearClient::graphql`].

pub mod client;
pub mod sync;

pub use client::{IssueFilter, LinearClient, LinearIssue, Team, Viewer, WorkflowState};
pub use sync::{SyncReport, sync_issues};
