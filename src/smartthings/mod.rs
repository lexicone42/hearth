//! SmartThings sink: maps canonical [`crate::domain::Observation`]s to standard
//! SmartThings capability events and pushes them to virtual devices.
//!
//! This is the first concrete *sink*. When a second one lands (HomeKit, MQTT, a
//! dashboard), the shared shape — `publish(&[Observation])` — graduates into a
//! `Sink` trait; until then a concrete type keeps the async signatures simple.
pub mod auth;
pub mod capability;
pub mod client;
pub mod provision;
pub mod sink;

pub use sink::SmartThingsSink;
