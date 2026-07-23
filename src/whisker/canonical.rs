use crate::domain::{DeviceClass, EntityId, Observation, Unit, Value};
use crate::whisker::model::{Pet, Robot, RobotState};

/// Source namespace for every entity this module produces.
const SOURCE: &str = "whisker";

/// Normalize one Litter-Robot 5 box into canonical `Observation`s. Each box hangs
/// under its own node: `whisker.<box_slug>.<channel>`, where `<box_slug>` is a
/// slug of the box's `name` (falling back to its serial).
///
/// Pure and total — no I/O, no clock. Emits (each only when the field is present):
///   - `whisker.<box>.litter_level` ([`DeviceClass::LitterLevel`]):
///     `litterLevelPercent` — the "refill litter" signal.
///   - `whisker.<box>.waste_drawer` ([`DeviceClass::WasteDrawer`]):
///     `dfiLevelPercent` — the graduated "empty it" signal.
///   - `whisker.<box>.status` ([`DeviceClass::Status`]): friendly unit status
///     text. The waste-drawer-full and offline alerts are *folded in here*
///     (they take precedence over the raw status), matching the LR4 approach.
///   - `whisker.<box>.last_visit_weight` ([`DeviceClass::Weight`]): the LAST
///     visitor's weight (`weightSensor / 100`, lb). This is NOT a per-cat weight
///     — the tracked per-cat weight comes from [`pet_observations`]; the distinct
///     channel name keeps the two from being confused.
///
/// The caller (`run_whisker`) is responsible for the offline `warn!` — this
/// function stays pure so it's unit-testable.
pub fn robot_observations(robot: &Robot) -> Vec<Observation> {
    let node = node_slug(&robot.name, &robot.serial);
    let state = &robot.state;
    let mut out = Vec::new();
    let mut push = |channel: &str, class: DeviceClass, value: Value| {
        out.push(Observation::new(
            EntityId::new([SOURCE, node.as_str(), channel]),
            class,
            value,
            // The payload carries no per-field observation timestamp; the caller
            // stamps wall-clock time if it needs one.
            None,
        ));
    };

    // ----- Litter level (percent) — the "refill" signal -----
    if let Some(pct) = state.litter_level_percent {
        push(
            "litter_level",
            DeviceClass::LitterLevel,
            Value::quantity(pct, Unit::Percent),
        );
    }

    // ----- Waste-drawer fullness (percent) — the "empty it" signal -----
    if let Some(pct) = state.dfi_level_percent {
        push(
            "waste_drawer",
            DeviceClass::WasteDrawer,
            Value::quantity(pct, Unit::Percent),
        );
    }

    // ----- Unit status (text), with the drawer-full/offline alerts folded in -----
    if let Some(status) = status_text(state) {
        push("status", DeviceClass::Status, Value::Text(status));
    }

    // ----- Last visitor's weight (informational; NOT per-cat) -----
    // weightSensor is pounds × 100. Skip a missing / zero reading.
    if let Some(raw) = state.weight_sensor
        && raw > 0.0
    {
        push(
            "last_visit_weight",
            DeviceClass::Weight,
            Value::quantity(raw / 100.0, Unit::Pounds),
        );
    }

    out
}

/// Normalize one pet into its weight observation: `whisker.<cat_slug>.weight`
/// ([`DeviceClass::Weight`], pounds), preferring the most recent MEASURED weight
/// (`lastWeightReading`) and falling back to the profile `weight`. Emits nothing
/// when neither is present (or both are `0`). `<cat_slug>` is a slug of the pet's
/// `name` (cat names and box names don't collide in practice). Pure and total.
pub fn pet_observations(pet: &Pet) -> Vec<Observation> {
    // Prefer the measured reading; fall back to the profile weight.
    let Some(weight) = pet.last_weight_reading.or(pet.weight).filter(|w| *w > 0.0) else {
        return Vec::new();
    };
    let node = node_slug(&pet.name, &pet.pet_id);
    vec![Observation::new(
        EntityId::new([SOURCE, node.as_str(), "weight"]),
        DeviceClass::Weight,
        Value::quantity(weight, Unit::Pounds),
        None,
    )]
}

/// The unit status as friendly text. Offline and drawer-full take precedence
/// over the reported status so the two "needs attention" states are never masked
/// (matches the LR4 approach). Otherwise prefer `statusIndicator.title`, then the
/// raw `displayCode`. `None` when the box reports none of them (nothing to say).
fn status_text(state: &RobotState) -> Option<String> {
    if state.is_online == Some(false) {
        return Some("Offline".to_string());
    }
    if state.is_drawer_full == Some(true) {
        return Some("Drawer Full".to_string());
    }
    if let Some(title) = state
        .status_indicator
        .as_ref()
        .and_then(|s| s.title.as_deref())
        .filter(|t| !t.is_empty())
    {
        return Some(title.to_string());
    }
    state
        .display_code
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

/// The entity node: a slug of `primary` (name), or of `fallback` (serial / pet
/// id) when the name is empty, or `unknown` if both are empty.
fn node_slug(primary: &str, fallback: &str) -> String {
    let from_name = slugify(primary);
    if !from_name.is_empty() {
        return from_name;
    }
    let from_fallback = slugify(fallback);
    if !from_fallback.is_empty() {
        return from_fallback;
    }
    "unknown".to_string()
}

/// Lowercase, non-alphanumeric -> `_`, collapse repeats, trim leading/trailing
/// `_`. "piano room" -> "piano_room".
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

    // Synthetic fixtures only (public repo): box "test room", serial
    // "LR5-TEST-000000", cats "Fixture One" / "Fixture Two".

    fn robot_from(json: serde_json::Value) -> Robot {
        serde_json::from_value(json).unwrap()
    }

    fn pet_from(json: serde_json::Value) -> Pet {
        serde_json::from_value(json).unwrap()
    }

    fn find<'a>(obs: &'a [Observation], id: &str) -> Option<&'a Observation> {
        obs.iter().find(|o| o.entity.as_str() == id)
    }

    #[test]
    fn maps_a_full_robot_sample() {
        let robot = robot_from(serde_json::json!({
            "serial": "LR5-TEST-000000",
            "name": "test room",
            "type": "LR5_PRO",
            "state": {
                "isOnline": true,
                "weightSensor": 943.0,
                "litterLevelPercent": 100.0,
                "dfiLevelPercent": 39,
                "isDrawerFull": false,
                "displayCode": "DcModeIdle",
                "statusIndicator": { "title": "Ready", "type": "READY" }
            }
        }));
        let obs = robot_observations(&robot);

        let ll = find(&obs, "whisker.test_room.litter_level").expect("litter_level");
        assert_eq!(ll.class, DeviceClass::LitterLevel);
        assert_eq!(ll.value, Value::quantity(100.0, Unit::Percent));

        let wd = find(&obs, "whisker.test_room.waste_drawer").expect("waste_drawer");
        assert_eq!(wd.class, DeviceClass::WasteDrawer);
        assert_eq!(wd.value, Value::quantity(39.0, Unit::Percent));

        // statusIndicator.title preferred (not full, online).
        let st = find(&obs, "whisker.test_room.status").expect("status");
        assert_eq!(st.class, DeviceClass::Status);
        assert_eq!(st.value, Value::Text("Ready".to_string()));

        // weightSensor 943 -> 9.43 lb, on a distinct informational channel.
        let lv = find(&obs, "whisker.test_room.last_visit_weight").expect("last_visit_weight");
        assert_eq!(lv.class, DeviceClass::Weight);
        assert_eq!(lv.value, Value::quantity(9.43, Unit::Pounds));

        assert_eq!(obs.len(), 4);
    }

    #[test]
    fn drawer_full_folds_into_status() {
        let robot = robot_from(serde_json::json!({
            "serial": "LR5-TEST-000000", "name": "test room",
            "state": { "isOnline": true, "isDrawerFull": true,
                       "statusIndicator": { "title": "Ready" } }
        }));
        let obs = robot_observations(&robot);
        // isDrawerFull true overrides the "Ready" title.
        assert_eq!(
            find(&obs, "whisker.test_room.status").unwrap().value,
            Value::Text("Drawer Full".to_string())
        );
    }

    #[test]
    fn offline_folds_into_status_and_takes_precedence() {
        let robot = robot_from(serde_json::json!({
            "serial": "LR5-TEST-000000", "name": "test room",
            "state": { "isOnline": false, "isDrawerFull": true,
                       "statusIndicator": { "title": "Ready" } }
        }));
        let obs = robot_observations(&robot);
        // Offline wins over both drawer-full and the title.
        assert_eq!(
            find(&obs, "whisker.test_room.status").unwrap().value,
            Value::Text("Offline".to_string())
        );
    }

    #[test]
    fn status_falls_back_to_display_code() {
        // No statusIndicator -> raw displayCode is the fallback.
        let robot = robot_from(serde_json::json!({
            "serial": "LR5-TEST-000000", "name": "test room",
            "state": { "isOnline": true, "isDrawerFull": false, "displayCode": "DcModeIdle" }
        }));
        let obs = robot_observations(&robot);
        assert_eq!(
            find(&obs, "whisker.test_room.status").unwrap().value,
            Value::Text("DcModeIdle".to_string())
        );
    }

    #[test]
    fn zero_or_missing_weight_sensor_is_not_emitted() {
        let robot = robot_from(serde_json::json!({
            "serial": "LR5-TEST-000000", "name": "test room",
            "state": { "isOnline": true, "weightSensor": 0.0 }
        }));
        assert!(
            find(
                &robot_observations(&robot),
                "whisker.test_room.last_visit_weight"
            )
            .is_none()
        );
    }

    #[test]
    fn robot_falls_back_to_serial_when_name_empty() {
        let robot = robot_from(serde_json::json!({
            "serial": "LR5-TEST-000000", "name": "",
            "state": { "isOnline": true, "litterLevelPercent": 50.0 }
        }));
        let obs = robot_observations(&robot);
        // "LR5-TEST-000000" -> "lr5_test_000000".
        assert!(find(&obs, "whisker.lr5_test_000000.litter_level").is_some());
    }

    #[test]
    fn pet_weight_prefers_last_reading() {
        let pet = pet_from(serde_json::json!({
            "petId": "PET-TEST-1", "name": "Fixture One", "type": "CAT",
            "weight": 7.456, "lastWeightReading": 7.37
        }));
        let obs = pet_observations(&pet);
        let w = find(&obs, "whisker.fixture_one.weight").expect("weight");
        assert_eq!(w.class, DeviceClass::Weight);
        assert_eq!(w.value, Value::quantity(7.37, Unit::Pounds));
        assert_eq!(obs.len(), 1);
    }

    #[test]
    fn pet_weight_falls_back_to_profile_weight() {
        // lastWeightReading absent -> use profile weight.
        let pet = pet_from(serde_json::json!({
            "petId": "PET-TEST-2", "name": "Fixture Two", "type": "CAT", "weight": 9.815
        }));
        let obs = pet_observations(&pet);
        assert_eq!(
            find(&obs, "whisker.fixture_two.weight").unwrap().value,
            Value::quantity(9.815, Unit::Pounds)
        );
    }

    #[test]
    fn pet_with_no_weight_emits_nothing() {
        let pet = pet_from(serde_json::json!({
            "petId": "PET-TEST-3", "name": "Fixture Three", "type": "CAT"
        }));
        assert!(pet_observations(&pet).is_empty());
        // A zero reading is also skipped.
        let zero = pet_from(serde_json::json!({
            "petId": "PET-TEST-4", "name": "Fixture Four", "lastWeightReading": 0.0
        }));
        assert!(pet_observations(&zero).is_empty());
    }

    #[test]
    fn slug_matches_existing_convention() {
        assert_eq!(slugify("piano room"), "piano_room");
        assert_eq!(slugify("  The  Box!! "), "the_box");
        assert_eq!(slugify("LR5-TEST-000000"), "lr5_test_000000");
        assert_eq!(slugify(""), "");
    }
}
