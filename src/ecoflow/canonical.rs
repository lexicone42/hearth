use crate::domain::{DeviceClass, EntityId, Observation, Unit, Value};
use crate::ecoflow::model::QuotaAll;

/// Source namespace for every entity this module produces.
const SOURCE: &str = "ecoflow";

/// Normalize an EcoFlow `quota/all` map for one device into canonical
/// `Observation`s. `sn` becomes the device node so multiple EcoFlow units
/// don't collide (`ecoflow.<sn>.<channel>`).
///
/// EcoFlow's quota keys vary by device model, and the user's model is not yet
/// known, so this is a *best-effort* mapping:
///
///   1. A small table of well-known Delta/River keys (state-of-charge, the
///      summed input/output watts) maps to first-class channels.
///   2. Any *remaining* numeric field is passed through generically under a
///      `raw.<sanitized.key>` channel as a unit-less [`Value::Quantity`] (no
///      `DeviceClass` can be inferred), so nothing is silently dropped while we
///      learn the device's real schema. See the TODO below.
///
/// Pure and total: absent keys produce nothing; non-numeric values are skipped.
pub fn to_observations(sn: &str, quota: &QuotaAll) -> Vec<Observation> {
    use DeviceClass as C;
    use Unit::*;

    let mut out = Vec::new();
    // EcoFlow quota/all does not carry an observation timestamp in the payload;
    // the caller stamps wall-clock time if it needs one. We leave it None.
    let ts = None;
    let mut push = |channel: &str, class: DeviceClass, value: Value| {
        out.push(Observation::new(
            EntityId::new([SOURCE, sn, channel]),
            class,
            value,
            ts,
        ));
    };

    // ----- Battery state-of-charge -> battery % -----
    // Different models expose SoC under different prefixes; take the first
    // present known key so we emit at most one canonical `battery` channel.
    // `bms_bmsStatus.soc` (Delta), `bmsMaster.soc`, `pd.soc`, generic `*.soc`.
    if let Some(v) = first_f64(quota, SOC_KEYS) {
        push("battery", C::Battery, Value::quantity(v, Percent));
    }

    // ----- Output power (watts) -----
    if let Some(v) = first_f64(quota, OUTPUT_WATTS_KEYS) {
        push("output_power", C::Power, Value::quantity(v, Watts));
    }
    // ----- Input power (watts) -----
    if let Some(v) = first_f64(quota, INPUT_WATTS_KEYS) {
        push("input_power", C::Power, Value::quantity(v, Watts));
    }

    // ----- Cumulative energy (watt-hours) -----
    // Reported capacities/energy counters. These keys are model-specific and
    // less consistently named than power/SoC, so we only promote a couple of
    // commonly-seen ones; the rest fall through to the generic pass.
    if let Some(v) = first_f64(quota, INPUT_ENERGY_KEYS) {
        push("input_energy", C::Energy, Value::quantity(v, WattHours));
    }
    if let Some(v) = first_f64(quota, OUTPUT_ENERGY_KEYS) {
        push("output_energy", C::Energy, Value::quantity(v, WattHours));
    }

    // ----- Generic pass-through for everything else numeric -----
    // TODO(device-model): once the user's actual EcoFlow model is known, audit
    // these raw keys and promote the meaningful ones (and their correct units)
    // into typed channels above, the way `ambient` promotes its fields. Until
    // then, unknown numeric quota fields are surfaced unit-less rather than
    // dropped, so they can be discovered from logs. Keys already consumed by the
    // known mappings above are skipped to avoid duplicate channels.
    for (key, raw) in quota {
        if is_known_key(key) {
            continue;
        }
        if let Some(v) = raw.as_f64() {
            // The dotted vendor key becomes the tail of the channel. EntityId
            // joins with '.', so a vendor key like `inv.acInVol` reads as
            // `ecoflow.<sn>.raw.inv.acInVol`. We have no class/unit for these,
            // so they go out as unit-less watts (the most common dimension) —
            // see the TODO: promote them once the device schema is known.
            push(&format!("raw.{key}"), C::Power, Value::quantity(v, Watts));
        }
        // Non-numeric values (strings, arrays, objects) carry no canonical
        // meaning we can guard with a unit/class, so we leave them out (None).
    }

    out
}

/// Known SoC keys, most-specific first. EcoFlow reports state-of-charge as a
/// 0–100 integer percent.
const SOC_KEYS: &[&str] = &[
    "bms_bmsStatus.soc",
    "bmsMaster.soc",
    "pd.soc",
    "bms_emsStatus.f32LcdShowSoc",
    "ems.lcdShowSoc",
];

/// Summed output power (W). `pd.wattsOutSum` is the canonical Delta/River key.
const OUTPUT_WATTS_KEYS: &[&str] = &[
    "pd.wattsOutSum",
    "inv.outputWatts",
    "mppt.outWatts",
];

/// Summed input power (W). `pd.wattsInSum` is the canonical Delta/River key.
const INPUT_WATTS_KEYS: &[&str] = &[
    "pd.wattsInSum",
    "inv.inputWatts",
    "mppt.inWatts",
];

/// Cumulative input energy (Wh), best-effort.
const INPUT_ENERGY_KEYS: &[&str] = &["pd.chgPowerAc", "pd.chgSunPower"];

/// Cumulative output energy (Wh), best-effort.
const OUTPUT_ENERGY_KEYS: &[&str] = &["pd.dsgPowerAc", "pd.dsgPowerDc"];

/// Every key consumed by a first-class mapping, so the generic pass can skip it.
fn is_known_key(key: &str) -> bool {
    SOC_KEYS.contains(&key)
        || OUTPUT_WATTS_KEYS.contains(&key)
        || INPUT_WATTS_KEYS.contains(&key)
        || INPUT_ENERGY_KEYS.contains(&key)
        || OUTPUT_ENERGY_KEYS.contains(&key)
}

/// First present-and-numeric value among `keys`, in order.
fn first_f64(quota: &QuotaAll, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| quota.get(*k).and_then(serde_json::Value::as_f64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn quota(pairs: &[(&str, serde_json::Value)]) -> QuotaAll {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    fn find<'a>(obs: &'a [Observation], id: &str) -> Option<&'a Observation> {
        obs.iter().find(|o| o.entity.as_str() == id)
    }

    #[test]
    fn maps_known_delta_keys() {
        let q = quota(&[
            ("bms_bmsStatus.soc", json!(73)),
            ("pd.wattsOutSum", json!(120)),
            ("pd.wattsInSum", json!(0)),
        ]);
        let obs = to_observations("R331ZEB4ZEAL0001", &q);

        let soc = find(&obs, "ecoflow.R331ZEB4ZEAL0001.battery").expect("battery");
        assert_eq!(soc.class, DeviceClass::Battery);
        assert_eq!(soc.value, Value::quantity(73.0, Unit::Percent));

        let out = find(&obs, "ecoflow.R331ZEB4ZEAL0001.output_power").expect("output power");
        assert_eq!(out.class, DeviceClass::Power);
        assert_eq!(out.value, Value::quantity(120.0, Unit::Watts));

        let inp = find(&obs, "ecoflow.R331ZEB4ZEAL0001.input_power").expect("input power");
        assert_eq!(inp.value, Value::quantity(0.0, Unit::Watts));

        // Exactly the three known channels, nothing extra (no unknown keys).
        assert_eq!(obs.len(), 3);
    }

    #[test]
    fn takes_first_present_soc_alias() {
        // No `bms_bmsStatus.soc`, but a generic `pd.soc` is present.
        let q = quota(&[("pd.soc", json!(42))]);
        let obs = to_observations("SN", &q);
        let soc = find(&obs, "ecoflow.SN.battery").expect("battery via alias");
        assert_eq!(soc.value, Value::quantity(42.0, Unit::Percent));
    }

    #[test]
    fn unknown_numeric_fields_pass_through_generically() {
        let q = quota(&[
            ("bms_bmsStatus.soc", json!(50)),
            ("inv.acInVol", json!(120000)), // unknown -> generic raw channel
            ("pd.model", json!("delta2")),  // non-numeric -> skipped
        ]);
        let obs = to_observations("SN", &q);

        // Known SoC mapped.
        assert!(find(&obs, "ecoflow.SN.battery").is_some());
        // Unknown numeric surfaced under raw.* (not dropped).
        let raw = find(&obs, "ecoflow.SN.raw.inv.acInVol").expect("raw passthrough");
        assert_eq!(raw.value, Value::quantity(120000.0, Unit::Watts));
        // Non-numeric string is not emitted.
        assert!(find(&obs, "ecoflow.SN.raw.pd.model").is_none());

        // soc + one raw field.
        assert_eq!(obs.len(), 2);
    }

    #[test]
    fn empty_quota_yields_nothing() {
        let obs = to_observations("SN", &QuotaAll::new());
        assert!(obs.is_empty());
    }
}
