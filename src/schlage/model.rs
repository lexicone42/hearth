use serde::Deserialize;

use crate::schlage::SchlageError;

/// Parse the `GET /devices?archetype=lock` response body (a JSON array) into
/// locks. A shape mismatch becomes [`SchlageError::Decode`] — the loud "their
/// API changed under us" signal — rather than a panic or a wrong observation.
/// Pure, so the decode path is unit-testable straight from a `&str`.
pub fn parse_devices(body: &str) -> Result<Vec<Device>, SchlageError> {
    serde_json::from_str(body).map_err(|e| SchlageError::decode(format!("device list: {e}")))
}

/// Parse the `GET /users/@me` response body into [`Me`]. Same `Decode`-on-drift
/// contract as [`parse_devices`].
pub fn parse_me(body: &str) -> Result<Me, SchlageError> {
    serde_json::from_str(body).map_err(|e| SchlageError::decode(format!("users/@me: {e}")))
}

/// One lock from `GET {BASE_URL}/devices?archetype=lock` — the JSON array
/// elements. Field names and semantics are verified against `pyschlage`'s
/// `lock.py::Lock.from_json` / `device.py`.
///
/// Everything is tolerant of missing fields (no `deny_unknown_fields`, defaults
/// throughout): Schlage's unofficial API varies by firmware/model, and a lock
/// that's offline omits most of `attributes`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Device {
    /// Schlage-generated unique device identifier. Used as the node slug when
    /// `name` is empty.
    #[serde(rename = "deviceId", default)]
    pub device_id: String,
    /// User-specified name of the lock, e.g. "Front Door". Slugified into the
    /// entity node (`schlage.<node>.lock`).
    #[serde(default)]
    pub name: String,
    /// Model name of the lock, e.g. "BE489WB". Retained for completeness /
    /// future use (not yet surfaced as an observation).
    #[serde(rename = "modelName", default)]
    #[allow(dead_code)]
    pub model_name: String,
    /// Whether the lock is currently connected to Wi-Fi. When false, its
    /// reported `lockState` may be stale, so canonical mapping reports `unknown`.
    #[serde(default)]
    pub connected: bool,
    /// The live state map. `lockState` and `batteryLevel` live here.
    #[serde(default)]
    pub attributes: Attributes,
}

/// The `attributes` object on a lock device. Both fields are optional: an
/// unavailable/offline lock omits them, in which case the lock reads as
/// `unknown` and no battery observation is emitted.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Attributes {
    /// `1` = locked, `2` = jammed, any other value = unlocked (pyschlage:
    /// `is_locked = lockState == 1; is_jammed = lockState == 2`). Absent when
    /// the lock is unavailable.
    #[serde(rename = "lockState", default)]
    pub lock_state: Option<i64>,
    /// Remaining battery level as an integer percent (0–100). Absent when the
    /// lock is unavailable.
    #[serde(rename = "batteryLevel", default)]
    pub battery_level: Option<i64>,
}

/// The `GET {BASE_URL}/users/@me` response: `{"identityId": ...}`.
#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    #[serde(rename = "identityId")]
    pub identity_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_locked_device_with_battery() {
        // Shape matches one element of the /devices?archetype=lock array.
        let raw = r#"{
            "deviceId": "abc123",
            "name": "Front Door",
            "modelName": "BE489WB",
            "connected": true,
            "attributes": {
                "lockState": 1,
                "batteryLevel": 87,
                "macAddress": "AA:BB:CC:DD:EE:FF"
            }
        }"#;
        let dev: Device = serde_json::from_str(raw).unwrap();
        assert_eq!(dev.device_id, "abc123");
        assert_eq!(dev.name, "Front Door");
        assert_eq!(dev.model_name, "BE489WB");
        assert!(dev.connected);
        assert_eq!(dev.attributes.lock_state, Some(1));
        assert_eq!(dev.attributes.battery_level, Some(87));
    }

    #[test]
    fn tolerates_missing_optional_fields() {
        // An offline lock: no modelName/connected, empty attributes.
        let raw = r#"{ "deviceId": "d2", "name": "Back Door", "attributes": {} }"#;
        let dev: Device = serde_json::from_str(raw).unwrap();
        assert_eq!(dev.name, "Back Door");
        assert_eq!(dev.model_name, "");
        assert!(!dev.connected);
        assert_eq!(dev.attributes.lock_state, None);
        assert_eq!(dev.attributes.battery_level, None);
    }

    #[test]
    fn parses_me() {
        let me: Me = serde_json::from_str(r#"{"identityId": "user-uuid-42"}"#).unwrap();
        assert_eq!(me.identity_id, "user-uuid-42");
    }

    #[test]
    fn parse_devices_reads_the_array() {
        let body = r#"[
            {"deviceId": "d1", "name": "Front Door", "connected": true,
             "attributes": {"lockState": 1, "batteryLevel": 90}},
            {"deviceId": "d2", "name": "Back Door", "attributes": {}}
        ]"#;
        let devices = parse_devices(body).unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name, "Front Door");
        assert_eq!(devices[0].attributes.lock_state, Some(1));
    }

    #[test]
    fn parse_devices_shape_change_is_decode_not_panic() {
        // Their cloud swaps the array for an error object -> Decode, not a panic.
        let err = parse_devices(r#"{"message": "gone"}"#).unwrap_err();
        assert_eq!(err.kind(), "decode");

        // A field changes type (lockState becomes a string) -> Decode, never a
        // silently-wrong observation.
        let err = parse_devices(
            r#"[{"deviceId":"d1","name":"Front Door","attributes":{"lockState":"LOCKED"}}]"#,
        )
        .unwrap_err();
        assert_eq!(err.kind(), "decode");
    }

    #[test]
    fn parse_me_missing_identity_is_decode() {
        assert!(parse_me(r#"{"identityId": "abc"}"#).is_ok());
        let err = parse_me(r#"{"somethingElse": "abc"}"#).unwrap_err();
        assert_eq!(err.kind(), "decode");
    }
}
