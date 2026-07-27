//! Local HTTP API sink: a latest-value store fed from the event bus, served
//! over the LAN as JSON — plus hearth's own dashboard.
//!
//! Like every sink, it maps *out* of the canonical domain and is fed only by
//! the router. Unlike SmartThings it is pull-based: `StateStore::record`
//! retains the newest observation per entity, and a small axum server serves
//! `GET /` (the fridge dashboard page), `GET /api/latest` (the snapshot),
//! `GET /api/history` + `GET /api/visits` (per-cat weight history, read from
//! the Whisker archive rather than the bus), `GET /assets/cats/{name}` (cached
//! cat photos) and `GET /healthz`. Omit the `[api]` config section and none of
//! this is spawned.
pub mod server;
pub mod state;

pub use state::StateStore;
