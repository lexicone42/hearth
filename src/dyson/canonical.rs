//! Normalize a Dyson MQTT `CURRENT-STATE` / `ENVIRONMENTAL-CURRENT-SENSOR-DATA`
//! message into canonical [`crate::domain::Observation`]s.
//!
//! Field names, value encodings, and sentinel handling are verified against
//! libdyson (`shenxn/libdyson` and the maintained fork `libdyson-wg/libdyson-neon`),
//! specifically `dyson_pure_cool.py` (the sensor property getters) and
//! `dyson_device.py` (`_get_field_value` / `_get_environmental_field_value`):
//!
//! * `_get_field_value(state, field)` returns `state[field][1]` when the value
//!   is a 2-element `[old, new]` list, else the scalar. We do the same (take the
//!   LAST element of a 2-array).
//! * `_get_environmental_field_value` maps `"OFF"/"off"` -> `ENVIRONMENTAL_OFF`
//!   (-1), `"INIT"` -> `-2`, `"FAIL"` -> `-3`, `"NONE"`/None -> None. We treat
//!   every one of these (plus `"INV"` and `""`) as "no reading" and emit nothing
//!   — mirroring ambient's absent-field handling rather than shipping sentinels.
//! * Numeric decode (from the property getters):
//!     - `pm25` (key `pm25`, fallback `p25r`)  -> `int`  µg/m³
//!     - `pm10` (key `pm10`, fallback `p10r`)  -> `int`  µg/m³
//!     - `tact`                                -> `float/10` Kelvin (so °C = raw/10 − 273.15)
//!     - `hact`                                -> `int`  % humidity
//!     - `va10` (VOC), `noxl` (NO2)            -> unitless air-quality index
//!       (ha-dyson ships these with NO unit — see issue #127 — so we surface the
//!       raw integer as a `Count` index rather than inventing ppb/µg.)
//!     - `hflr` (HEPA), `cflr` (carbon)        -> `int` % filter life
//!       (`cflr == "INV"` means no carbon filter is fitted -> skip)
//!     - `fnsp`                                -> fan speed step 1–10
//!       (`"AUTO"` -> no numeric value -> skip)

use serde_json::Value as Json;

use crate::domain::{DeviceClass, EntityId, Observation, Unit, Value};

/// Source namespace for every entity this module produces.
const SOURCE: &str = "dyson";

/// Kelvin offset: °C = K − 273.15. `tact` is deci-Kelvin (raw/10 = Kelvin).
const KELVIN_OFFSET_C: f64 = 273.15;

/// Sentinel strings that mean "no reading"; emit nothing for these (mirrors
/// ambient's absent-field handling). Covers libdyson's `OFF/INIT/FAIL/NONE`,
/// the carbon-filter `INV` (no filter fitted), and an empty string.
fn is_sentinel(s: &str) -> bool {
    matches!(
        s.to_ascii_uppercase().as_str(),
        "OFF" | "INIT" | "FAIL" | "INV" | "NONE" | ""
    )
}

/// Map a Dyson state/environmental field map (the `data` object of a parsed
/// message, with `product-state`/`data` already merged in by the caller) to
/// canonical `Observation`s under `dyson.<serial>.<channel>`.
///
/// `state` is the flat `field -> Json` map of decoded fields. Pure and total:
/// absent fields and sentinel values produce nothing.
pub fn to_observations(serial: &str, state: &serde_json::Map<String, Json>) -> Vec<Observation> {
    use DeviceClass as C;
    use Unit::*;

    let mut out = Vec::new();
    // Dyson messages carry a `"time"` ISO-8601 string, not an epoch; the bus
    // doesn't require one, so we leave `observed_at` None (like ecoflow).
    let mut push = |channel: &str, class: DeviceClass, value: Value| {
        out.push(Observation::new(
            EntityId::new([SOURCE, serial, channel]),
            class,
            value,
            None,
        ));
    };

    // ----- Particulates (µg/m³) -----
    // `p25r`/`p10r` are the "raw" PM channels on newer firmware; `pm25`/`pm10`
    // the older keys. libdyson reads the raw key first, then falls back.
    if let Some(v) = field_i64(state, &["p25r", "pm25"]) {
        push("pm25", C::Pm25, Value::quantity(v as f64, MicrogramsPerCubicMeter));
    }
    if let Some(v) = field_i64(state, &["p10r", "pm10"]) {
        push("pm10", C::Pm10, Value::quantity(v as f64, MicrogramsPerCubicMeter));
    }

    // ----- Temperature: `tact` is deci-Kelvin -> Celsius -----
    if let Some(raw) = field_f64(state, &["tact"]) {
        let celsius = raw / 10.0 - KELVIN_OFFSET_C;
        push("temperature", C::Temperature, Value::quantity(celsius, Celsius));
    }

    // ----- Humidity (%) -----
    if let Some(v) = field_i64(state, &["hact"]) {
        push("humidity", C::Humidity, Value::quantity(v as f64, Percent));
    }

    // ----- Air-quality indices (unitless; ha-dyson ships no unit) -----
    if let Some(v) = field_i64(state, &["va10"]) {
        push("voc", C::VolatileOrganicCompounds, Value::Count(v));
    }
    if let Some(v) = field_i64(state, &["noxl"]) {
        push("no2", C::NitrogenDioxide, Value::Count(v));
    }

    // ----- Filter life (%) -----
    if let Some(v) = field_i64(state, &["hflr"]) {
        push("hepa_filter_life", C::FilterLife, Value::quantity(v as f64, Percent));
    }
    // `cflr == "INV"` (no carbon filter fitted) is caught by the sentinel check
    // in `field_*`, so this only fires when a real percentage is present.
    if let Some(v) = field_i64(state, &["cflr"]) {
        push("carbon_filter_life", C::FilterLife, Value::quantity(v as f64, Percent));
    }

    // ----- Fan speed (1–10 step; `AUTO` -> skip) -----
    if let Some(v) = field_i64(state, &["fnsp"]) {
        push("fan_speed", C::FanSpeed, Value::Count(v));
    }

    out
}

/// Resolve the first present, non-sentinel field among `keys`, applying
/// `_get_field_value`'s `[old, new]` array rule (take the last element). Returns
/// the raw string token (numeric parsing is left to the typed accessors).
fn field_str<'a>(
    state: &'a serde_json::Map<String, Json>,
    keys: &[&str],
) -> Option<std::borrow::Cow<'a, str>> {
    for key in keys {
        let Some(raw) = state.get(*key) else { continue };
        // `_get_field_value`: a list is `[old, new]` -> take the last element.
        let scalar = match raw {
            Json::Array(items) => items.last(),
            other => Some(other),
        }?;
        let token: std::borrow::Cow<'a, str> = match scalar {
            Json::String(s) => std::borrow::Cow::Borrowed(s.as_str()),
            // Dyson fields are JSON strings, but tolerate a bare number too.
            Json::Number(n) => std::borrow::Cow::Owned(n.to_string()),
            _ => continue,
        };
        if is_sentinel(token.trim()) {
            // A present-but-sentinel field means "no reading": stop, emit nothing
            // (don't fall through to a stale fallback key).
            return None;
        }
        return Some(token);
    }
    None
}

/// First present, non-sentinel field among `keys`, parsed as an integer
/// (Dyson encodes sensor values as zero-padded numeric strings, e.g. `"0007"`).
fn field_i64(state: &serde_json::Map<String, Json>, keys: &[&str]) -> Option<i64> {
    field_str(state, keys)?.trim().parse::<i64>().ok()
}

/// First present, non-sentinel field among `keys`, parsed as a float.
fn field_f64(state: &serde_json::Map<String, Json>, keys: &[&str]) -> Option<f64> {
    field_str(state, keys)?.trim().parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(v: Json) -> serde_json::Map<String, Json> {
        v.as_object().unwrap().clone()
    }

    fn find<'a>(obs: &'a [Observation], id: &str) -> Option<&'a Observation> {
        obs.iter().find(|o| o.entity.as_str() == id)
    }

    #[test]
    fn decodes_environmental_fields() {
        // Dyson encodes sensor values as zero-padded numeric strings.
        let s = state(json!({
            "pm25": "0012",
            "pm10": "0008",
            "va10": "0035",
            "noxl": "0004",
            "tact": "2950",   // 295.0 K -> 21.85 °C
            "hact": "0047",
        }));
        let obs = to_observations("NK6-EU-HHA1111A", &s);

        let pm25 = find(&obs, "dyson.NK6-EU-HHA1111A.pm25").expect("pm25");
        assert_eq!(pm25.class, DeviceClass::Pm25);
        assert_eq!(pm25.value, Value::quantity(12.0, Unit::MicrogramsPerCubicMeter));

        let pm10 = find(&obs, "dyson.NK6-EU-HHA1111A.pm10").expect("pm10");
        assert_eq!(pm10.class, DeviceClass::Pm10);
        assert_eq!(pm10.value, Value::quantity(8.0, Unit::MicrogramsPerCubicMeter));

        // VOC / NO2 ship as a unitless index (Count), not a quantity.
        let voc = find(&obs, "dyson.NK6-EU-HHA1111A.voc").expect("voc");
        assert_eq!(voc.class, DeviceClass::VolatileOrganicCompounds);
        assert_eq!(voc.value, Value::Count(35));
        let no2 = find(&obs, "dyson.NK6-EU-HHA1111A.no2").expect("no2");
        assert_eq!(no2.class, DeviceClass::NitrogenDioxide);
        assert_eq!(no2.value, Value::Count(4));

        // tact: 2950 deci-Kelvin -> 295.0 K -> 21.85 °C.
        let temp = find(&obs, "dyson.NK6-EU-HHA1111A.temperature").expect("temperature");
        assert_eq!(temp.class, DeviceClass::Temperature);
        let Value::Quantity { value, unit } = temp.value else { panic!("quantity") };
        assert_eq!(unit, Unit::Celsius);
        assert!((value - 21.85).abs() < 1e-9, "got {value}");

        let hum = find(&obs, "dyson.NK6-EU-HHA1111A.humidity").expect("humidity");
        assert_eq!(hum.value, Value::quantity(47.0, Unit::Percent));
    }

    #[test]
    fn takes_new_value_of_old_new_array() {
        // CURRENT-STATE deltas arrive as [old, new]; libdyson takes index [1].
        let s = state(json!({
            "fnsp": ["0003", "0007"],
            "hflr": ["0090", "0089"],
        }));
        let obs = to_observations("SN", &s);

        let fan = find(&obs, "dyson.SN.fan_speed").expect("fan speed");
        assert_eq!(fan.class, DeviceClass::FanSpeed);
        assert_eq!(fan.value, Value::Count(7)); // the NEW value

        let hepa = find(&obs, "dyson.SN.hepa_filter_life").expect("hepa filter");
        assert_eq!(hepa.class, DeviceClass::FilterLife);
        assert_eq!(hepa.value, Value::quantity(89.0, Unit::Percent));
    }

    #[test]
    fn skips_sentinels_and_auto() {
        let s = state(json!({
            "pm25": "OFF",      // sensor off -> nothing
            "va10": "INIT",     // warming up -> nothing
            "noxl": "FAIL",     // sensor fault -> nothing
            "hact": "NONE",     // no reading -> nothing
            "cflr": "INV",      // no carbon filter fitted -> nothing
            "fnsp": "AUTO",     // non-numeric fan mode -> no numeric speed
            "hflr": "0075",     // a real reading survives
        }));
        let obs = to_observations("SN", &s);

        assert!(find(&obs, "dyson.SN.pm25").is_none());
        assert!(find(&obs, "dyson.SN.voc").is_none());
        assert!(find(&obs, "dyson.SN.no2").is_none());
        assert!(find(&obs, "dyson.SN.humidity").is_none());
        assert!(find(&obs, "dyson.SN.carbon_filter_life").is_none());
        assert!(find(&obs, "dyson.SN.fan_speed").is_none());

        // Only the one valid field maps.
        let hepa = find(&obs, "dyson.SN.hepa_filter_life").expect("hepa filter");
        assert_eq!(hepa.value, Value::quantity(75.0, Unit::Percent));
        assert_eq!(obs.len(), 1);
    }

    #[test]
    fn empty_state_yields_nothing() {
        let obs = to_observations("SN", &serde_json::Map::new());
        assert!(obs.is_empty());
    }
}
