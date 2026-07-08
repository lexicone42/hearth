use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::domain::{EntityId, Observation, UnitSystem, Value};

/// Latest-value store: the newest `Observation` per entity, plus the wall
/// clock time it arrived. Cheaply cloneable (an `Arc` around the map) so the
/// router task and the HTTP server task share one instance; writes and reads
/// both hold the lock only long enough to touch the map.
#[derive(Clone, Default)]
pub struct StateStore {
    inner: Arc<RwLock<HashMap<EntityId, Entry>>>,
}

/// One retained observation: what the bus delivered and when we received it
/// (epoch ms). `received_at` is ours; `observed_at` stays the source's claim.
struct Entry {
    observation: Observation,
    received_at: i64,
}

/// Snapshot form of one entity, shaped for JSON clients. `value` is the raw
/// number/flag/text (already re-expressed in the requested unit system);
/// `display` is the human string sinks-of-eyeballs want (`"72.4 °F"`).
#[derive(Debug, Serialize)]
pub struct EntityState {
    pub entity: String,
    pub class: String,
    pub value: serde_json::Value,
    /// Unit symbol for quantities (`"°F"`, `"µg/m³"`); `None` for counts,
    /// flags, text, and unitless indices.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub display: String,
    /// Source observation time, epoch ms (UTC), if the source provided one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<i64>,
    /// When the hub received the observation, epoch ms (UTC).
    pub received_at: i64,
}

/// The full `GET /api/latest` payload.
#[derive(Debug, Serialize)]
pub struct Snapshot {
    /// When this snapshot was rendered, epoch ms (UTC).
    pub generated_at: i64,
    pub unit_system: UnitSystem,
    pub entities: Vec<EntityState>,
}

impl StateStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Retain the newest observation per entity. Last write wins — the bus
    /// preserves per-source ordering, and cross-source batches never share an
    /// entity id (ids are namespaced by source), so "last received" is also
    /// "most recent".
    pub fn record(&self, batch: &[Observation]) {
        let received_at = now_ms();
        let mut map = self.inner.write().expect("state store lock poisoned");
        for obs in batch {
            map.insert(
                obs.entity.clone(),
                Entry {
                    observation: obs.clone(),
                    received_at,
                },
            );
        }
    }

    /// Render every retained entity in `system`'s preferred units, sorted by
    /// entity id for a stable payload.
    pub fn snapshot(&self, system: UnitSystem) -> Snapshot {
        let map = self.inner.read().expect("state store lock poisoned");
        let mut entities: Vec<EntityState> = map
            .values()
            .map(|entry| render(&entry.observation, entry.received_at, system))
            .collect();
        drop(map);
        entities.sort_by(|a, b| a.entity.cmp(&b.entity));
        Snapshot {
            generated_at: now_ms(),
            unit_system: system,
            entities,
        }
    }
}

/// Map one retained observation to its JSON shape, re-expressing quantities in
/// the preferred unit system (mirrors what the log lines and SmartThings sink
/// show, so every surface agrees).
fn render(obs: &Observation, received_at: i64, system: UnitSystem) -> EntityState {
    let value = obs.value.in_system(system);
    let (json_value, unit) = match &value {
        Value::Quantity { value, unit } => {
            let symbol = unit.to_string();
            let symbol = (!symbol.is_empty()).then_some(symbol);
            (serde_json::json!(value), symbol)
        }
        Value::Count(n) => (serde_json::json!(n), None),
        Value::Flag(b) => (serde_json::json!(b), None),
        Value::Text(s) => (serde_json::json!(s), None),
    };
    EntityState {
        entity: obs.entity.to_string(),
        class: format!("{:?}", obs.class),
        value: json_value,
        unit,
        display: value.to_string(),
        observed_at: obs.observed_at,
        received_at,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DeviceClass, Unit};

    fn obs(entity: &str, class: DeviceClass, value: Value) -> Observation {
        Observation::new(EntityId::new(entity.split('.')), class, value, Some(1_000))
    }

    #[test]
    fn record_retains_newest_per_entity() {
        let store = StateStore::new();
        store.record(&[obs(
            "ambient_weather.outdoor.temperature",
            DeviceClass::Temperature,
            Value::quantity(60.0, Unit::Fahrenheit),
        )]);
        store.record(&[obs(
            "ambient_weather.outdoor.temperature",
            DeviceClass::Temperature,
            Value::quantity(72.4, Unit::Fahrenheit),
        )]);

        let snap = store.snapshot(UnitSystem::Imperial);
        assert_eq!(snap.entities.len(), 1);
        assert_eq!(snap.entities[0].value, serde_json::json!(72.4));
        assert_eq!(snap.entities[0].unit.as_deref(), Some("°F"));
        assert_eq!(snap.entities[0].display, "72.4 °F");
    }

    #[test]
    fn snapshot_converts_to_requested_system_and_sorts() {
        let store = StateStore::new();
        store.record(&[
            obs(
                "ambient_weather.outdoor.temperature",
                DeviceClass::Temperature,
                Value::quantity(32.0, Unit::Fahrenheit),
            ),
            obs(
                "ambient_weather.indoor.humidity",
                DeviceClass::Humidity,
                Value::quantity(55.0, Unit::Percent),
            ),
        ]);

        let snap = store.snapshot(UnitSystem::Metric);
        let ids: Vec<&str> = snap.entities.iter().map(|e| e.entity.as_str()).collect();
        assert_eq!(
            ids,
            [
                "ambient_weather.indoor.humidity",
                "ambient_weather.outdoor.temperature",
            ]
        );
        let temp = &snap.entities[1];
        assert_eq!(temp.unit.as_deref(), Some("°C"));
        assert_eq!(temp.value, serde_json::json!(0.0));
        // Percent is system-agnostic: untouched by the metric request.
        assert_eq!(snap.entities[0].value, serde_json::json!(55.0));
    }

    #[test]
    fn non_quantities_serialize_without_units() {
        let store = StateStore::new();
        store.record(&[obs(
            "dyson.abc123.battery_low",
            DeviceClass::BatteryLow,
            Value::Flag(false),
        )]);
        let snap = store.snapshot(UnitSystem::Imperial);
        assert_eq!(snap.entities[0].value, serde_json::json!(false));
        assert!(snap.entities[0].unit.is_none());
    }
}
