//! SmartThings *source*: polls configured devices' status and maps known
//! capabilities (lock, battery) to canonical observations.
//!
//! This is the hub's first read-*back* — SmartThings as a source, not just a
//! sink — so a cloud-linked Wi-Fi lock's state can flow to the local API sink
//! and the watch tile. Polling (not webhooks) keeps hearth outbound-only, in
//! keeping with the no-inbound-endpoint posture of the rest of the hub.

use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::config::ReadDevice;
use crate::domain::{DeviceClass, EntityId, Observation, Unit, Value};
use crate::smartthings::client::{MainStatus, SmartThingsClient};

/// Source namespace for every entity this module produces.
const SOURCE: &str = "smartthings";

/// Poll each configured device every `interval_secs`, mapping its known
/// capabilities onto the bus. Per-device errors are logged, never fatal —
/// SmartThings trouble must not take down the bridge.
pub async fn run(
    client: SmartThingsClient,
    devices: Vec<ReadDevice>,
    interval_secs: u64,
    tx: mpsc::Sender<Vec<Observation>>,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    loop {
        ticker.tick().await;

        let mut batch = Vec::new();
        for dev in &devices {
            match client.device_status(&dev.device_id).await {
                Ok(main) => batch.extend(to_observations(&dev.node, &main)),
                Err(e) => {
                    error!(device_id = %dev.device_id, error = ?e, "failed to read SmartThings device")
                }
            }
        }
        if batch.is_empty() {
            continue;
        }

        info!(count = batch.len(), "mapped SmartThings observations");
        for obs in &batch {
            debug!(entity = %obs.entity, class = ?obs.class, value = %obs.value, "observation");
        }
        if tx.send(batch).await.is_err() {
            // Router gone (shutdown).
            break;
        }
    }
}

/// Map a device's `main` status to canonical observations. Known capabilities
/// only — a lock reports `lock` + `battery`; anything else is ignored (and
/// picked up when we teach the source a new capability).
fn to_observations(node: &str, main: &MainStatus) -> Vec<Observation> {
    let mut out = Vec::new();
    let mut push = |channel: &str, class: DeviceClass, value: Value| {
        out.push(Observation::new(
            EntityId::new([SOURCE, node, channel]),
            class,
            value,
            None,
        ));
    };

    // Lock state: "locked" / "unlocked" / "jammed" / "unknown".
    if let Some(state) = attr_str(main, "lock", "lock") {
        push("lock", DeviceClass::Lock, Value::Text(state));
    }
    // Battery percent — locks (and many devices) report it.
    if let Some(pct) = attr_f64(main, "battery", "battery") {
        push(
            "battery",
            DeviceClass::Battery,
            Value::quantity(pct, Unit::Percent),
        );
    }

    out
}

/// The string value of `main.<capability>.<attribute>`, if present.
fn attr_str(main: &MainStatus, capability: &str, attribute: &str) -> Option<String> {
    main.get(capability)?
        .get(attribute)?
        .value
        .as_str()
        .map(str::to_owned)
}

/// The numeric value of `main.<capability>.<attribute>`, if present.
fn attr_f64(main: &MainStatus, capability: &str, attribute: &str) -> Option<f64> {
    main.get(capability)?.get(attribute)?.value.as_f64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a `MainStatus` from a `{capability: {attribute: {value, unit}}}`
    /// JSON object, matching the shape `GET /devices/{id}/status` returns.
    fn status(v: serde_json::Value) -> MainStatus {
        serde_json::from_value(v).unwrap()
    }

    fn find<'a>(obs: &'a [Observation], id: &str) -> Option<&'a Observation> {
        obs.iter().find(|o| o.entity.as_str() == id)
    }

    #[test]
    fn maps_lock_and_battery() {
        let main = status(json!({
            "lock": { "lock": { "value": "locked", "timestamp": "2026-07-08T00:00:00Z" } },
            "battery": { "battery": { "value": 87, "unit": "%" } },
            "refresh": { "refresh": {} }
        }));
        let obs = to_observations("front_door", &main);

        let lock = find(&obs, "smartthings.front_door.lock").expect("lock");
        assert_eq!(lock.class, DeviceClass::Lock);
        assert_eq!(lock.value, Value::Text("locked".to_string()));
        assert_eq!(lock.value.to_string(), "locked");

        let batt = find(&obs, "smartthings.front_door.battery").expect("battery");
        assert_eq!(batt.class, DeviceClass::Battery);
        assert_eq!(batt.value, Value::quantity(87.0, Unit::Percent));

        // Only the two known channels; `refresh` is ignored.
        assert_eq!(obs.len(), 2);
    }

    #[test]
    fn unlocked_and_missing_battery() {
        let main = status(json!({
            "lock": { "lock": { "value": "unlocked" } }
        }));
        let obs = to_observations("back_door", &main);
        assert_eq!(
            find(&obs, "smartthings.back_door.lock").unwrap().value,
            Value::Text("unlocked".to_string())
        );
        assert!(find(&obs, "smartthings.back_door.battery").is_none());
        assert_eq!(obs.len(), 1);
    }

    #[test]
    fn a_device_with_no_known_capabilities_yields_nothing() {
        let main = status(json!({
            "switch": { "switch": { "value": "on" } },
            "motionSensor": { "motion": { "value": "inactive" } }
        }));
        assert!(to_observations("some_switch", &main).is_empty());
    }
}
