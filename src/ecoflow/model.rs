use std::collections::BTreeMap;

use serde::Deserialize;

/// Standard EcoFlow IoT Open API response envelope: `{code, message, data}`.
/// `code` is `"0"` on success (the API returns it as a string). `data` is
/// endpoint-specific, hence the generic parameter.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope<T> {
    pub code: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default = "Option::default")]
    pub data: Option<T>,
}

impl<T> Envelope<T> {
    /// `"0"` is EcoFlow's success code.
    pub fn is_ok(&self) -> bool {
        self.code == "0"
    }
}

/// One entry of the `GET /iot-open/sign/device/list` response.
///
/// Only the serial number and online flag are load-bearing for us; anything
/// EcoFlow adds later is ignored rather than rejected (no `deny_unknown_fields`).
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceListEntry {
    /// Device serial number, e.g. `R331ZEB4ZEAL0001`. This is what the
    /// quota/all endpoint keys off.
    pub sn: String,
    /// 1 = online, 0 = offline. Optional because the field name has varied
    /// across firmware/device generations. Read out-of-band (logs/debugger)
    /// for now, hence `allow`.
    #[serde(default)]
    #[allow(dead_code)]
    pub online: Option<i64>,
    /// Human-readable device name set in the EcoFlow app, when present.
    /// Read out-of-band for now, hence `allow`.
    #[serde(rename = "deviceName", default)]
    #[allow(dead_code)]
    pub device_name: Option<String>,
}

/// The `data` payload of `GET /iot-open/sign/device/quota/all?sn=...`.
///
/// EcoFlow returns a *flat* map whose keys are dotted property paths
/// (e.g. `bms_bmsStatus.soc`, `inv.outputWatts`, `pd.wattsOutSum`) and whose
/// values are JSON scalars. The exact key set depends on the device model, so
/// this is intentionally a `BTreeMap<String, serde_json::Value>` rather than a
/// fixed struct — [`crate::ecoflow::canonical`] interprets it.
pub type QuotaAll = BTreeMap<String, serde_json::Value>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_list_envelope() {
        let raw = r#"{
            "code": "0",
            "message": "Success",
            "data": [
                {"sn": "R331ZEB4ZEAL0001", "online": 1, "deviceName": "Delta 2"},
                {"sn": "R601ZEB4ZEAL0002"}
            ]
        }"#;
        let env: Envelope<Vec<DeviceListEntry>> = serde_json::from_str(raw).unwrap();
        assert!(env.is_ok());
        let data = env.data.unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0].sn, "R331ZEB4ZEAL0001");
        assert_eq!(data[0].online, Some(1));
        assert_eq!(data[0].device_name.as_deref(), Some("Delta 2"));
        // Missing optional fields default to None rather than failing.
        assert_eq!(data[1].online, None);
    }

    #[test]
    fn parses_flat_quota_map() {
        let raw = r#"{
            "code": "0",
            "message": "Success",
            "data": {
                "bms_bmsStatus.soc": 73,
                "inv.outputWatts": 120.5,
                "pd.wattsOutSum": 121,
                "pd.wattsInSum": 0
            }
        }"#;
        let env: Envelope<QuotaAll> = serde_json::from_str(raw).unwrap();
        assert!(env.is_ok());
        let data = env.data.unwrap();
        assert_eq!(data.get("bms_bmsStatus.soc").and_then(|v| v.as_f64()), Some(73.0));
        assert_eq!(data.get("inv.outputWatts").and_then(|v| v.as_f64()), Some(120.5));
    }

    #[test]
    fn error_envelope_is_not_ok() {
        let raw = r#"{"code": "7017", "message": "accessKey is invalid"}"#;
        let env: Envelope<QuotaAll> = serde_json::from_str(raw).unwrap();
        assert!(!env.is_ok());
        assert_eq!(env.message.as_deref(), Some("accessKey is invalid"));
        assert!(env.data.is_none());
    }
}
