use crate::domain::{DeviceClass, EntityId, Observation, Unit, Value};
use crate::schlage::model::Device;

/// Source namespace for every entity this module produces.
const SOURCE: &str = "schlage";

/// Normalize one Schlage lock into canonical `Observation`s. Multiple locks
/// don't collide because each hangs under its own node: `schlage.<node>.<channel>`,
/// where `<node>` is a slug of the lock's `name` (falling back to its device id).
///
/// Pure and total — no I/O, no clock. Emits:
///   - `schlage.<node>.lock`    ([`DeviceClass::Lock`], always): `Value::Text`
///     of `locked` / `unlocked` / `jammed`, or `unknown` when the lock is
///     unavailable (no `lockState`) *or offline* (`connected == false`) — a
///     stale reading is never presented as current.
///   - `schlage.<node>.battery` ([`DeviceClass::Battery`], when `batteryLevel`
///     is present): `Value::quantity(pct, Percent)`. Emitted even when offline.
///
/// The caller (`run_schlage`) is responsible for the offline `warn!` — this
/// function stays pure so it's unit-testable.
pub fn to_observations(device: &Device) -> Vec<Observation> {
    let node = node_slug(device);
    let mut out = Vec::new();
    let mut push = |channel: &str, class: DeviceClass, value: Value| {
        out.push(Observation::new(
            EntityId::new([SOURCE, node.as_str(), channel]),
            class,
            value,
            // The device JSON carries no observation timestamp for these
            // channels; the caller stamps wall-clock time if it needs one.
            None,
        ));
    };

    // Lock state is always emitted — a lock reporting nothing back is itself
    // meaningful (surfaced as `unknown`), unlike a purely optional sensor. An
    // offline lock's last-known state may be stale, so we deliberately report
    // `unknown` rather than a possibly-wrong `locked`/`unlocked`.
    let state = if device.connected {
        lock_state_text(device.attributes.lock_state)
    } else {
        "unknown"
    };
    push("lock", DeviceClass::Lock, Value::Text(state.to_string()));

    // Battery percent, only when the lock reports it.
    if let Some(pct) = device.attributes.battery_level {
        push(
            "battery",
            DeviceClass::Battery,
            Value::quantity(pct as f64, Unit::Percent),
        );
    }

    out
}

/// Map Schlage's numeric `lockState` to canonical lock text. `1` = locked,
/// `2` = jammed, any other value = unlocked (mirrors pyschlage's
/// `is_locked = lockState == 1; is_jammed = lockState == 2`). Absent state
/// (offline/unavailable lock) reads as `unknown` rather than falsely `unlocked`.
fn lock_state_text(lock_state: Option<i64>) -> &'static str {
    match lock_state {
        Some(1) => "locked",
        Some(2) => "jammed",
        Some(_) => "unlocked",
        None => "unknown",
    }
}

/// The entity node for a lock: a slug of its `name`, or of its `deviceId` when
/// the name is empty, or `unknown` if both are empty.
fn node_slug(device: &Device) -> String {
    let from_name = slugify(&device.name);
    if !from_name.is_empty() {
        return from_name;
    }
    let from_id = slugify(&device.device_id);
    if !from_id.is_empty() {
        return from_id;
    }
    "unknown".to_string()
}

/// Lowercase, non-alphanumeric -> `_`, collapse repeats, trim leading/trailing
/// `_`. "Front Door" -> "front_door".
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_sep = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('_');
            }
            pending_sep = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            // Defer the separator so trailing runs never emit one.
            pending_sep = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device_from(json: serde_json::Value) -> Device {
        serde_json::from_value(json).unwrap()
    }

    fn find<'a>(obs: &'a [Observation], id: &str) -> Option<&'a Observation> {
        obs.iter().find(|o| o.entity.as_str() == id)
    }

    #[test]
    fn locked_with_battery() {
        let dev = device_from(serde_json::json!({
            "deviceId": "abc123",
            "name": "Front Door",
            "connected": true,
            "attributes": { "lockState": 1, "batteryLevel": 87 }
        }));
        let obs = to_observations(&dev);

        let lock = find(&obs, "schlage.front_door.lock").expect("lock");
        assert_eq!(lock.class, DeviceClass::Lock);
        assert_eq!(lock.value, Value::Text("locked".to_string()));
        assert_eq!(lock.value.to_string(), "locked");

        let batt = find(&obs, "schlage.front_door.battery").expect("battery");
        assert_eq!(batt.class, DeviceClass::Battery);
        assert_eq!(batt.value, Value::quantity(87.0, Unit::Percent));

        assert_eq!(obs.len(), 2);
    }

    #[test]
    fn jammed_state() {
        let dev = device_from(serde_json::json!({
            "name": "Front Door", "connected": true, "attributes": { "lockState": 2 }
        }));
        let obs = to_observations(&dev);
        assert_eq!(
            find(&obs, "schlage.front_door.lock").unwrap().value,
            Value::Text("jammed".to_string())
        );
        // No battery reported.
        assert!(find(&obs, "schlage.front_door.battery").is_none());
        assert_eq!(obs.len(), 1);
    }

    #[test]
    fn other_lock_state_is_unlocked() {
        // Any value that is not 1 or 2 (e.g. 0) reads as unlocked (while online).
        let dev = device_from(serde_json::json!({
            "name": "Front Door", "connected": true, "attributes": { "lockState": 0 }
        }));
        let obs = to_observations(&dev);
        assert_eq!(
            find(&obs, "schlage.front_door.lock").unwrap().value,
            Value::Text("unlocked".to_string())
        );
    }

    #[test]
    fn absent_lock_state_is_unknown() {
        // Online lock but no lockState -> `unknown`, never a false `unlocked`.
        let dev = device_from(serde_json::json!({
            "name": "Front Door", "connected": true, "attributes": {}
        }));
        let obs = to_observations(&dev);
        assert_eq!(
            find(&obs, "schlage.front_door.lock").unwrap().value,
            Value::Text("unknown".to_string())
        );
        assert_eq!(obs.len(), 1);
    }

    #[test]
    fn offline_lock_is_unknown_but_battery_still_emitted() {
        // connected == false: the last-known lockState may be stale, so we
        // report `unknown` regardless of it — but still surface the battery.
        let dev = device_from(serde_json::json!({
            "name": "Front Door",
            "connected": false,
            "attributes": { "lockState": 1, "batteryLevel": 50 }
        }));
        let obs = to_observations(&dev);
        assert_eq!(
            find(&obs, "schlage.front_door.lock").unwrap().value,
            Value::Text("unknown".to_string())
        );
        let batt = find(&obs, "schlage.front_door.battery").expect("battery");
        assert_eq!(batt.value, Value::quantity(50.0, Unit::Percent));
        assert_eq!(obs.len(), 2);
    }

    #[test]
    fn falls_back_to_device_id_when_name_empty() {
        let dev = device_from(serde_json::json!({
            "deviceId": "AB12-CD34", "name": "", "connected": true,
            "attributes": { "lockState": 1 }
        }));
        let obs = to_observations(&dev);
        // deviceId is slugified too: "AB12-CD34" -> "ab12_cd34".
        assert!(find(&obs, "schlage.ab12_cd34.lock").is_some());
    }

    #[test]
    fn slug_collapses_and_trims() {
        assert_eq!(slugify("Front Door"), "front_door");
        assert_eq!(slugify("  Side   Gate!! "), "side_gate");
        assert_eq!(slugify("Garage-Door #2"), "garage_door_2");
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("---"), "");
    }
}
