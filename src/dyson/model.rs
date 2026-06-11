//! Parsing of inbound Dyson MQTT `status/current` messages.
//!
//! Verified against libdyson (`dyson_device.py` / `dyson_pure_cool.py`): a
//! status message is a JSON object with a `"msg"` discriminator and the sensor
//! fields nested under one of two keys depending on the message type:
//!
//!   * `CURRENT-STATE` / `STATE-CHANGE` -> fields under `"product-state"`
//!     (fan speed `fnsp`, filter life `hflr`/`cflr`, ...). For `STATE-CHANGE`
//!     each value is a 2-element `[old, new]` array; `canonical` takes `[new]`.
//!   * `ENVIRONMENTAL-CURRENT-SENSOR-DATA` -> fields under `"data"`
//!     (`pm25`, `pm10`, `va10`, `noxl`, `tact`, `hact`, ...).
//!
//! We don't model every field as a struct (the set varies by model/firmware);
//! instead we surface the relevant flat field map and let [`super::canonical`]
//! interpret it, mirroring how `ecoflow` treats its quota map.

use serde_json::Value as Json;

/// The flat field map carried by a status message, plus its `msg` type, ready
/// for [`super::canonical::to_observations`].
#[derive(Debug, Clone, PartialEq)]
pub struct StatusMessage {
    /// The `"msg"` discriminator, e.g. `CURRENT-STATE`.
    pub msg: String,
    /// The decoded field map (`product-state` or `data`, depending on `msg`).
    pub fields: serde_json::Map<String, Json>,
}

/// Parse a raw MQTT payload into a [`StatusMessage`], or `None` if it isn't a
/// recognized state/environmental message we can map. Unknown `msg` types
/// (acks, faults we don't model, ...) return `None` rather than erroring —
/// they're simply not observations.
pub fn parse(payload: &[u8]) -> Option<StatusMessage> {
    let root: Json = serde_json::from_slice(payload).ok()?;
    let obj = root.as_object()?;
    let msg = obj.get("msg")?.as_str()?.to_string();

    let fields = match msg.as_str() {
        // State snapshots and deltas carry fields under `product-state`.
        "CURRENT-STATE" | "STATE-CHANGE" => obj.get("product-state")?.as_object()?.clone(),
        // Environmental sensor data carries fields under `data`.
        "ENVIRONMENTAL-CURRENT-SENSOR-DATA" => obj.get("data")?.as_object()?.clone(),
        _ => return None,
    };
    Some(StatusMessage { msg, fields })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_environmental_message() {
        let payload = json!({
            "msg": "ENVIRONMENTAL-CURRENT-SENSOR-DATA",
            "time": "2026-06-11T00:00:00Z",
            "data": { "pm25": "0012", "tact": "2950" }
        })
        .to_string();
        let m = parse(payload.as_bytes()).expect("environmental");
        assert_eq!(m.msg, "ENVIRONMENTAL-CURRENT-SENSOR-DATA");
        assert_eq!(m.fields.get("pm25").unwrap(), &json!("0012"));
    }

    #[test]
    fn parses_current_state_message() {
        let payload = json!({
            "msg": "CURRENT-STATE",
            "time": "2026-06-11T00:00:00Z",
            "product-state": { "fnsp": "0007", "hflr": "0090" }
        })
        .to_string();
        let m = parse(payload.as_bytes()).expect("current state");
        assert_eq!(m.msg, "CURRENT-STATE");
        assert_eq!(m.fields.get("fnsp").unwrap(), &json!("0007"));
    }

    #[test]
    fn parses_state_change_with_old_new_arrays() {
        let payload = json!({
            "msg": "STATE-CHANGE",
            "product-state": { "fnsp": ["0003", "0007"] }
        })
        .to_string();
        let m = parse(payload.as_bytes()).expect("state change");
        assert_eq!(m.msg, "STATE-CHANGE");
        assert_eq!(m.fields.get("fnsp").unwrap(), &json!(["0003", "0007"]));
    }

    #[test]
    fn ignores_unknown_and_malformed() {
        // Unknown msg type -> None (not an error).
        let other = json!({"msg": "STATE-SET", "data": {}}).to_string();
        assert!(parse(other.as_bytes()).is_none());
        // Not JSON -> None.
        assert!(parse(b"not json").is_none());
        // No `msg` -> None.
        assert!(parse(json!({"data": {}}).to_string().as_bytes()).is_none());
    }
}
