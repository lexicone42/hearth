//! Serde structs for the Litter-Robot 5 cloud responses.
//!
//! LR5 exposes the data this source needs across TWO live-verified endpoints,
//! both authed with `Authorization: Bearer <IdToken>`:
//!   - **Robots** — `GET https://ub.prod.iothings.site/robots` returns a JSON
//!     *array* of robots; the box's litter/waste/status live under each robot's
//!     `state` object. See [`Robot`] / [`RobotState`].
//!   - **Pets** — `POST https://pet-profile.iothings.site/graphql/`
//!     (`getPetsByUser`) returns the per-cat weights — the hub owner's #1 goal.
//!     See [`Pet`].
//!
//! Field tolerance is total: every optional field is `Option<_>` + defaulted, so
//! a firmware/schema tweak that drops or renames a field degrades gracefully
//! rather than failing the whole poll. A wholesale shape change (the array/GraphQL
//! envelope no longer matching, or a field flipping type) surfaces as
//! [`WhiskerError::Decode`] — the loud "their unofficial API changed" signal.

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::whisker::WhiskerError;

// ---------------------------------------------------------------------------
// Source A — Robots (REST array)
// ---------------------------------------------------------------------------

/// One Litter-Robot from `GET /robots`. Box telemetry lives under [`Self::state`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Robot {
    /// Device serial number; the `[whisker].serial` filter key and node fallback.
    #[serde(default)]
    pub serial: String,
    /// User-given box name, e.g. "piano room". Slugified into the entity node.
    #[serde(default)]
    pub name: String,
    /// Product type, e.g. "LR5" | "LR5_PRO". Retained for completeness.
    #[serde(rename = "type", default)]
    #[allow(dead_code)]
    pub robot_type: String,
    /// Owning user id (the `mid`). Retained for completeness / future use.
    #[serde(rename = "userId", default)]
    #[allow(dead_code)]
    pub user_id: String,
    /// The box's live telemetry.
    #[serde(default)]
    pub state: RobotState,
}

/// The `state` object on an LR5 robot — the live telemetry (verified field names).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RobotState {
    /// The LAST visitor's weight, in **pounds × 100** (e.g. `943.0` = 9.43 lb).
    /// This is NOT a per-cat weight — per-cat weight comes from [`Pet`]. Surfaced
    /// only as an informational `.last_visit_weight` channel.
    #[serde(rename = "weightSensor", default)]
    pub weight_sensor: Option<f64>,
    /// Remaining litter level as a percent (0–100) — the "refill litter" signal.
    #[serde(rename = "litterLevelPercent", default)]
    pub litter_level_percent: Option<f64>,
    /// Waste-drawer fullness as a percent (0–100) — the "empty the drawer" signal.
    #[serde(rename = "dfiLevelPercent", default)]
    pub dfi_level_percent: Option<f64>,
    /// Whether the waste drawer is full (the alert boolean).
    #[serde(rename = "isDrawerFull", default)]
    pub is_drawer_full: Option<bool>,
    /// Whether the robot is currently online (Wi-Fi connected).
    #[serde(rename = "isOnline", default)]
    pub is_online: Option<bool>,
    /// Raw display code, e.g. "DcModeIdle" — status fallback when there is no
    /// `statusIndicator.title`.
    #[serde(rename = "displayCode", default)]
    pub display_code: Option<String>,
    /// Friendly status object, e.g. `{ "title": "Ready", "type": "READY" }`.
    #[serde(rename = "statusIndicator", default)]
    pub status_indicator: Option<StatusIndicator>,
    /// Lifetime scoops saved (informational; not currently mapped).
    #[serde(rename = "scoopsSaved", default)]
    #[allow(dead_code)]
    pub scoops_saved: Option<i64>,
}

/// The LR5 `statusIndicator` object: a preferred human-readable status.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StatusIndicator {
    /// Human title, e.g. "Ready". Preferred as the status text.
    #[serde(default)]
    pub title: Option<String>,
    /// Machine type, e.g. "READY". Retained for completeness / future use.
    #[serde(rename = "type", default)]
    #[allow(dead_code)]
    pub kind: Option<String>,
}

/// Parse the `GET /robots` response body (a JSON array) into robots. A shape
/// mismatch becomes [`WhiskerError::Decode`].
pub fn parse_robots(body: &str) -> Result<Vec<Robot>, WhiskerError> {
    decode(body, "robots")
}

// ---------------------------------------------------------------------------
// Source B — Pets (GraphQL) — per-cat weight
// ---------------------------------------------------------------------------

/// One pet from `getPetsByUser`. The per-cat weight is the hub owner's #1 goal.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Pet {
    /// Whisker-internal pet id. Retained for completeness / future use.
    #[serde(rename = "petId", default)]
    #[allow(dead_code)]
    pub pet_id: String,
    /// The cat's name, e.g. "Ava". Slugified into the entity node.
    #[serde(default)]
    pub name: String,
    /// Pet type, e.g. "CAT". Retained for completeness / future use.
    #[serde(rename = "type", default)]
    #[allow(dead_code)]
    pub pet_type: String,
    /// Profile/estimated weight (lb). Used as the weight fallback.
    #[serde(default)]
    pub weight: Option<f64>,
    /// Most recent MEASURED weight (lb). Preferred as the canonical weight.
    #[serde(rename = "lastWeightReading", default)]
    pub last_weight_reading: Option<f64>,
    /// Whether Whisker's weight-ID feature is on for this pet.
    #[serde(rename = "weightIdFeatureEnabled", default)]
    #[allow(dead_code)]
    pub weight_id_feature_enabled: Option<bool>,
}

/// `data` shape for `getPetsByUser`.
#[derive(Debug, Deserialize)]
struct PetsData {
    #[serde(rename = "getPetsByUser", default = "none")]
    pets: Option<Vec<Pet>>,
}

/// Parse a `getPetsByUser` GraphQL response body into the user's pets. A present
/// `errors` array (their API drifted) or a shape mismatch is
/// [`WhiskerError::Decode`].
pub fn parse_pets(body: &str) -> Result<Vec<Pet>, WhiskerError> {
    let resp: GraphQlResponse<PetsData> = decode(body, "getPetsByUser")?;
    check_errors(resp.errors.as_deref())?;
    Ok(resp.data.and_then(|d| d.pets).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Source C — Activities (REST array) — the weight-history feed
// ---------------------------------------------------------------------------

/// One event from a box's activity feed
/// (`GET /robots/{serial}/activities`, a JSON array, most-recent first).
///
/// The feed carries many event types — CYCLE_COMPLETED, DRAWER_FULL, LITTER_LOW,
/// LITTER_CRITICALLY_LOW, POSITION_FAULT, and the one this source archives,
/// **PET_VISIT** (a per-cat weight reading). Only PET_VISIT is consumed today
/// (see [`crate::whisker::history`]); every other type still deserializes fine
/// and is simply ignored.
///
/// Field tolerance is total: every field is `Option`/defaulted and unknown
/// fields are ignored, so a firmware/schema tweak degrades gracefully rather than
/// failing the whole scan.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Activity {
    /// Message id (delivery envelope id). Retained for completeness.
    #[serde(rename = "messageId", default)]
    #[allow(dead_code)]
    pub message_id: Option<String>,
    /// Stable event id — the archive's dedup key. Present on real events.
    #[serde(rename = "eventId", default)]
    pub event_id: Option<String>,
    /// The box's serial number.
    #[serde(default)]
    pub serial: Option<String>,
    /// The box's user-given name, e.g. "piano room".
    #[serde(rename = "robotName", default)]
    pub robot_name: Option<String>,
    /// Event type, e.g. "PET_VISIT" | "DRAWER_FULL" | "LITTER_LOW". Only
    /// PET_VISIT is archived.
    #[serde(rename = "type", default)]
    pub r#type: Option<String>,
    /// ISO-8601 event timestamp, e.g. "2026-07-23T19:35:18.182000Z".
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Visit duration in seconds (PET_VISIT).
    #[serde(default)]
    pub duration: Option<i64>,
    /// Waste type on a PET_VISIT, e.g. "Urine" / "Feces".
    #[serde(rename = "wasteType", default)]
    pub waste_type: Option<String>,
    /// The pet id(s) the visit was attributed to (usually one).
    #[serde(rename = "petIds", default)]
    pub pet_ids: Vec<String>,
    /// The measured weight, in **pounds × 100** (e.g. `943.0` = 9.43 lb).
    #[serde(rename = "petWeight", default)]
    pub pet_weight: Option<f64>,
    /// Waste weight (grams, as the feed reports it).
    #[serde(rename = "wasteWeight", default)]
    pub waste_weight: Option<f64>,
    /// Whether the visit was later reassigned to a different pet.
    #[serde(rename = "isReassigned", default)]
    #[allow(dead_code)]
    pub is_reassigned: Option<bool>,
}

/// Parse a `GET /robots/{serial}/activities` response body (a JSON array) into
/// activity events. A shape mismatch (e.g. an error object instead of an array,
/// or a field flipping type) becomes [`WhiskerError::Decode`]. An empty array
/// (offset past the retained window) parses to an empty `Vec`.
pub fn parse_activities(body: &str) -> Result<Vec<Activity>, WhiskerError> {
    decode(body, "activities")
}

// ---------------------------------------------------------------------------
// GraphQL envelope + shared helpers
// ---------------------------------------------------------------------------

/// A GraphQL response envelope: `{ "data": {...}, "errors": [...] }`.
#[derive(Debug, Deserialize)]
struct GraphQlResponse<T> {
    #[serde(default = "none")]
    data: Option<T>,
    #[serde(default)]
    errors: Option<Vec<GraphQlError>>,
}

/// One entry of a GraphQL `errors` array (we only surface the message).
#[derive(Debug, Deserialize)]
struct GraphQlError {
    #[serde(default)]
    message: String,
}

fn none<T>() -> Option<T> {
    None
}

/// Deserialize `body` into `T`, mapping a shape mismatch to
/// [`WhiskerError::Decode`] with `ctx` for orientation.
fn decode<T: DeserializeOwned>(body: &str, ctx: &str) -> Result<T, WhiskerError> {
    serde_json::from_str(body).map_err(|e| WhiskerError::decode(format!("{ctx}: {e}")))
}

/// A present, non-empty GraphQL `errors` array is a [`WhiskerError::Decode`] —
/// on an unofficial API a query error most likely means the schema drifted.
fn check_errors(errors: Option<&[GraphQlError]>) -> Result<(), WhiskerError> {
    match errors {
        Some(errs) if !errs.is_empty() => {
            let joined = errs
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            Err(WhiskerError::decode(format!("GraphQL errors: {joined}")))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic fixtures only — the repo is public, so no real serials, user
    // ids, or pet data appear here.

    #[test]
    fn parses_robots_array_with_state() {
        let body = r#"[
            {
                "serial": "LR5-TEST-000000",
                "name": "test room",
                "type": "LR5_PRO",
                "userId": "test-user",
                "state": {
                    "isOnline": true,
                    "weightSensor": 943.0,
                    "litterLevelPercent": 100.0,
                    "dfiLevelPercent": 39,
                    "isDrawerFull": false,
                    "displayCode": "DcModeIdle",
                    "statusIndicator": { "title": "Ready", "type": "READY" },
                    "scoopsSaved": 2324
                }
            }
        ]"#;
        let robots = parse_robots(body).unwrap();
        assert_eq!(robots.len(), 1);
        let r = &robots[0];
        assert_eq!(r.serial, "LR5-TEST-000000");
        assert_eq!(r.name, "test room");
        assert_eq!(r.robot_type, "LR5_PRO");
        assert_eq!(r.state.weight_sensor, Some(943.0));
        assert_eq!(r.state.litter_level_percent, Some(100.0));
        assert_eq!(r.state.dfi_level_percent, Some(39.0));
        assert_eq!(r.state.is_drawer_full, Some(false));
        assert_eq!(r.state.is_online, Some(true));
        assert_eq!(r.state.display_code.as_deref(), Some("DcModeIdle"));
        assert_eq!(
            r.state.status_indicator.as_ref().unwrap().title.as_deref(),
            Some("Ready")
        );
    }

    #[test]
    fn robots_tolerate_missing_state_fields() {
        // A sparse robot (only serial+name, empty state) still decodes.
        let body = r#"[{"serial":"LR5-TEST-000000","name":"test room","state":{}}]"#;
        let robots = parse_robots(body).unwrap();
        let r = &robots[0];
        assert_eq!(r.state.litter_level_percent, None);
        assert_eq!(r.state.is_drawer_full, None);
        assert!(r.state.status_indicator.is_none());
    }

    #[test]
    fn robots_shape_change_is_decode_not_panic() {
        // The array becomes an error object -> Decode, not a panic.
        let err = parse_robots(r#"{"message":"gone"}"#).unwrap_err();
        assert_eq!(err.kind(), "decode");
        // A field flips type (litterLevelPercent -> string) -> Decode.
        let err =
            parse_robots(r#"[{"serial":"S","state":{"litterLevelPercent":"lots"}}]"#).unwrap_err();
        assert_eq!(err.kind(), "decode");
    }

    #[test]
    fn parses_pets_graphql() {
        let body = r#"{
            "data": {
                "getPetsByUser": [
                    {"petId":"PET-TEST-1","name":"Fixture One","type":"CAT",
                     "weight":7.456,"lastWeightReading":7.37,"weightIdFeatureEnabled":true},
                    {"petId":"PET-TEST-2","name":"Fixture Two","type":"CAT",
                     "weight":9.815,"lastWeightReading":null}
                ]
            }
        }"#;
        let pets = parse_pets(body).unwrap();
        assert_eq!(pets.len(), 2);
        assert_eq!(pets[0].name, "Fixture One");
        assert_eq!(pets[0].last_weight_reading, Some(7.37));
        assert_eq!(pets[0].weight, Some(7.456));
        assert_eq!(pets[1].last_weight_reading, None);
        assert_eq!(pets[1].weight, Some(9.815));
    }

    #[test]
    fn empty_or_null_pets_is_empty() {
        assert!(
            parse_pets(r#"{"data":{"getPetsByUser":[]}}"#)
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_pets(r#"{"data":{"getPetsByUser":null}}"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn pets_graphql_errors_are_decode() {
        let body = r#"{"errors":[{"message":"Cannot query field \"weight\""}],"data":null}"#;
        let err = parse_pets(body).unwrap_err();
        assert_eq!(err.kind(), "decode");
        assert!(format!("{err}").contains("weight"));
    }

    #[test]
    fn parses_a_mixed_activity_array_tolerating_unknown_fields() {
        // A PET_VISIT (with fields the archive ignores, e.g. subtype/visitTime)
        // alongside a non-PET_VISIT event. Both must decode.
        let body = r#"[
            {"messageId":"MSG-TEST-1","eventId":"EV-TEST-1","serial":"LR5-TEST-000000",
             "robotName":"test room","type":"PET_VISIT","timestamp":"2026-01-01T00:00:00Z",
             "subtype":null,"duration":65,"wasteType":"Urine","petIds":["PET-TEST-1"],
             "petWeight":943.0,"wasteWeight":48.0,"visitTime":"2026-01-01T00:00:00Z",
             "isReassigned":false,"reassignedAt":null,"isWasteWeightValid":true},
            {"eventId":"EV-TEST-2","serial":"LR5-TEST-000000","type":"DRAWER_FULL",
             "timestamp":"2026-01-01T01:00:00Z"}
        ]"#;
        let acts = parse_activities(body).unwrap();
        assert_eq!(acts.len(), 2);
        assert_eq!(acts[0].r#type.as_deref(), Some("PET_VISIT"));
        assert_eq!(acts[0].event_id.as_deref(), Some("EV-TEST-1"));
        assert_eq!(acts[0].pet_ids, vec!["PET-TEST-1".to_string()]);
        assert_eq!(acts[0].pet_weight, Some(943.0));
        assert_eq!(acts[0].waste_type.as_deref(), Some("Urine"));
        assert_eq!(acts[1].r#type.as_deref(), Some("DRAWER_FULL"));
        assert!(acts[1].pet_weight.is_none());
        assert!(acts[1].pet_ids.is_empty());
    }

    #[test]
    fn empty_activity_array_is_empty() {
        // Offset past the retained window returns `[]`.
        assert!(parse_activities("[]").unwrap().is_empty());
    }

    #[test]
    fn activities_shape_change_is_decode_not_panic() {
        // An error object instead of an array -> Decode.
        let err = parse_activities(r#"{"message":"gone"}"#).unwrap_err();
        assert_eq!(err.kind(), "decode");
        // A field flips type (petWeight -> string) -> Decode.
        let err = parse_activities(r#"[{"type":"PET_VISIT","petWeight":"heavy"}]"#).unwrap_err();
        assert_eq!(err.kind(), "decode");
    }
}
