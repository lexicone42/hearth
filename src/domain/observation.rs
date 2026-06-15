use crate::domain::entity::{DeviceClass, EntityId};
use crate::domain::value::Value;

/// A single normalized state update for one entity — the unit that flows onto
/// the event bus (Phase 4+) and into every sink. Source-agnostic by
/// construction: nothing here knows about Ambient Weather or SmartThings.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub entity: EntityId,
    pub class: DeviceClass,
    pub value: Value,
    /// Source observation time, epoch milliseconds (UTC), if provided.
    pub observed_at: Option<i64>,
}

impl Observation {
    pub fn new(
        entity: EntityId,
        class: DeviceClass,
        value: Value,
        observed_at: Option<i64>,
    ) -> Self {
        Self {
            entity,
            class,
            value,
            observed_at,
        }
    }
}
