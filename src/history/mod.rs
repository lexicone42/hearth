//! Long-term history: every observation, kept on disk.
//!
//! The rest of hearth is about the **present** — sources normalize into
//! [`Observation`](crate::domain::Observation)s, the router fans them to sinks,
//! and `api::StateStore` remembers the newest value per entity. Nothing
//! remembered yesterday.
//!
//! This module is the other half: the router hands every batch here too, and it
//! accumulates a queryable time series for any entity, from any source, with no
//! per-source code. A new device that emits observations gets history for free.
//!
//! It is deliberately **not** the home for the Whisker visit archive
//! ([`crate::whisker::history`]). Those are different shapes of data: an
//! observation is one scalar sample of one channel, while a litter-box visit is
//! a discrete event carrying several *correlated* fields (which cat, what
//! weight, what waste, how long, which box). Splitting a visit into four
//! independent observations would throw away the correlation that makes it
//! useful, so the two stores stay separate on purpose.

pub mod backup;
pub mod codec;
pub mod store;

pub use store::HistoryStore;
