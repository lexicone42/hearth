//! The canonical, source-agnostic vocabulary of the hub: entities, semantic
//! classes, values, and units.
//!
//! Sources (Ambient Weather, ...) normalize *into* this layer; sinks
//! (SmartThings, HomeKit, MQTT, ...) map *out* of it. Keeping this core free of
//! any vendor detail is what lets new sources and sinks slot in without
//! touching each other — the difference between "a bridge" and "a hub".
pub mod entity;
pub mod observation;
pub mod value;

pub use entity::{DeviceClass, EntityId};
pub use observation::Observation;
pub use value::{Unit, UnitSystem, Value};
