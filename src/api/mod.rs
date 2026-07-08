//! Local HTTP API sink: a latest-value store fed from the event bus, served
//! over the LAN as JSON.
//!
//! Like every sink, it maps *out* of the canonical domain and is fed only by
//! the router. Unlike SmartThings it is pull-based: `StateStore::record`
//! retains the newest observation per entity, and a small axum server renders
//! a snapshot on `GET /api/latest` for local clients (the Wear OS tile being
//! the first). Omit the `[api]` config section and none of this is spawned.
pub mod server;
pub mod state;

pub use state::StateStore;
