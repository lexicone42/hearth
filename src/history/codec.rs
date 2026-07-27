//! On-disk encoding for a [`Value`].
//!
//! This is a **wire format**: bytes written today must still decode years from
//! now, so nothing here may derive its meaning from Rust's memory layout or from
//! the declaration order of an enum. Every unit gets an explicit, permanent
//! numeric code, and [`unit_code`] deliberately has **no wildcard arm** — adding
//! a `Unit` variant fails to compile until it is given a code, which is the one
//! way to guarantee we never silently write an un-decodable point.
//!
//! Layout: a one-byte tag, then the payload.
//!
//! | tag | value      | payload                                  |
//! |-----|------------|------------------------------------------|
//! | `0` | `Quantity` | `f64` little-endian (8B) + unit `u16` (2B) |
//! | `1` | `Count`    | `i64` little-endian (8B)                 |
//! | `2` | `Flag`     | `u8`, `0` or `1` (1B)                    |
//! | `3` | `Text`     | UTF-8 bytes, to the end                  |

use crate::domain::{Unit, Value};

const TAG_QUANTITY: u8 = 0;
const TAG_COUNT: u8 = 1;
const TAG_FLAG: u8 = 2;
const TAG_TEXT: u8 = 3;

/// Encode a value for storage.
pub fn encode(value: &Value) -> Vec<u8> {
    match value {
        Value::Quantity { value, unit } => {
            let mut out = Vec::with_capacity(11);
            out.push(TAG_QUANTITY);
            out.extend_from_slice(&value.to_le_bytes());
            out.extend_from_slice(&unit_code(*unit).to_le_bytes());
            out
        }
        Value::Count(n) => {
            let mut out = Vec::with_capacity(9);
            out.push(TAG_COUNT);
            out.extend_from_slice(&n.to_le_bytes());
            out
        }
        Value::Flag(b) => vec![TAG_FLAG, u8::from(*b)],
        Value::Text(s) => {
            let mut out = Vec::with_capacity(1 + s.len());
            out.push(TAG_TEXT);
            out.extend_from_slice(s.as_bytes());
            out
        }
    }
}

/// Decode a stored value. `None` for anything we don't recognize — a point
/// written by a newer schema, or a corrupt row. Callers skip those rather than
/// failing the whole read, so one bad row can't hide a year of good ones.
///
/// Used by [`super::store::HistoryStore::range`], whose HTTP read path arrives
/// in the next change — so the binary writes history before it can serve it.
#[allow(dead_code)]
pub fn decode(bytes: &[u8]) -> Option<Value> {
    let (&tag, rest) = bytes.split_first()?;
    match tag {
        TAG_QUANTITY => {
            let (num, unit) = rest.split_at_checked(8)?;
            Some(Value::Quantity {
                value: f64::from_le_bytes(num.try_into().ok()?),
                unit: unit_from_code(u16::from_le_bytes(unit.get(..2)?.try_into().ok()?))?,
            })
        }
        TAG_COUNT => Some(Value::Count(i64::from_le_bytes(
            rest.get(..8)?.try_into().ok()?,
        ))),
        TAG_FLAG => Some(Value::Flag(*rest.first()? != 0)),
        TAG_TEXT => Some(Value::Text(String::from_utf8(rest.to_vec()).ok()?)),
        _ => None,
    }
}

/// The permanent on-disk code for a unit.
///
/// **These numbers are frozen.** Changing one silently reinterprets every point
/// ever written with it. New units take the next unused number; retired units
/// keep their number reserved. The match is intentionally exhaustive (no `_`
/// arm) so a new `Unit` breaks the build here instead of at runtime.
fn unit_code(unit: Unit) -> u16 {
    match unit {
        Unit::Fahrenheit => 1,
        Unit::Celsius => 2,
        Unit::InchesOfMercury => 3,
        Unit::Hectopascal => 4,
        Unit::MilesPerHour => 5,
        Unit::KilometersPerHour => 6,
        Unit::Inches => 7,
        Unit::Millimeters => 8,
        Unit::Miles => 9,
        Unit::Kilometers => 10,
        Unit::Degrees => 11,
        Unit::Percent => 12,
        Unit::WattsPerSquareMeter => 13,
        Unit::MicrogramsPerCubicMeter => 14,
        Unit::Watts => 15,
        Unit::WattHours => 16,
        Unit::Pounds => 17,
        Unit::Kilograms => 18,
        Unit::Index => 19,
    }
}

/// Inverse of [`unit_code`]; `None` for a code this build doesn't know.
fn unit_from_code(code: u16) -> Option<Unit> {
    Some(match code {
        1 => Unit::Fahrenheit,
        2 => Unit::Celsius,
        3 => Unit::InchesOfMercury,
        4 => Unit::Hectopascal,
        5 => Unit::MilesPerHour,
        6 => Unit::KilometersPerHour,
        7 => Unit::Inches,
        8 => Unit::Millimeters,
        9 => Unit::Miles,
        10 => Unit::Kilometers,
        11 => Unit::Degrees,
        12 => Unit::Percent,
        13 => Unit::WattsPerSquareMeter,
        14 => Unit::MicrogramsPerCubicMeter,
        15 => Unit::Watts,
        16 => Unit::WattHours,
        17 => Unit::Pounds,
        18 => Unit::Kilograms,
        19 => Unit::Index,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every unit that exists. `unit_code`'s exhaustive match means a new
    /// variant won't compile until it's coded; this list then proves the code
    /// survives a round trip.
    const ALL_UNITS: &[Unit] = &[
        Unit::Fahrenheit,
        Unit::Celsius,
        Unit::InchesOfMercury,
        Unit::Hectopascal,
        Unit::MilesPerHour,
        Unit::KilometersPerHour,
        Unit::Inches,
        Unit::Millimeters,
        Unit::Miles,
        Unit::Kilometers,
        Unit::Degrees,
        Unit::Percent,
        Unit::WattsPerSquareMeter,
        Unit::MicrogramsPerCubicMeter,
        Unit::Watts,
        Unit::WattHours,
        Unit::Pounds,
        Unit::Kilograms,
        Unit::Index,
    ];

    #[test]
    fn every_unit_round_trips_and_has_a_distinct_code() {
        let mut seen = std::collections::HashSet::new();
        for &u in ALL_UNITS {
            let code = unit_code(u);
            assert!(seen.insert(code), "unit code {code} is used twice");
            assert_eq!(unit_from_code(code), Some(u), "{u:?} failed to round trip");
        }
        assert_eq!(seen.len(), ALL_UNITS.len());
    }

    #[test]
    fn values_round_trip() {
        let cases = [
            Value::quantity(72.4, Unit::Fahrenheit),
            Value::quantity(-3.8, Unit::Fahrenheit),
            Value::quantity(0.0, Unit::Percent),
            Value::Count(35),
            Value::Count(-1),
            Value::Flag(true),
            Value::Flag(false),
            Value::Text("Drawer Full".to_string()),
            Value::Text(String::new()),
            Value::Text("unicode: °F µg/m³ ✓".to_string()),
        ];
        for v in cases {
            assert_eq!(decode(&encode(&v)), Some(v.clone()), "{v:?}");
        }
    }

    #[test]
    fn quantity_encoding_is_exactly_the_documented_layout() {
        // Guards the wire format itself: if this changes, previously written
        // points stop decoding.
        let bytes = encode(&Value::quantity(1.0, Unit::Celsius));
        assert_eq!(bytes.len(), 11);
        assert_eq!(bytes[0], TAG_QUANTITY);
        assert_eq!(&bytes[1..9], &1.0f64.to_le_bytes());
        assert_eq!(&bytes[9..11], &2u16.to_le_bytes()); // Celsius is permanently 2
    }

    #[test]
    fn garbage_decodes_to_none_rather_than_panicking() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[99]), None); // unknown tag
        assert_eq!(decode(&[TAG_QUANTITY, 1, 2, 3]), None); // truncated
        assert_eq!(decode(&[TAG_COUNT]), None); // truncated
        assert_eq!(decode(&[TAG_FLAG]), None); // truncated
        // A quantity carrying a unit code this build doesn't know.
        let mut future = vec![TAG_QUANTITY];
        future.extend_from_slice(&1.0f64.to_le_bytes());
        future.extend_from_slice(&9999u16.to_le_bytes());
        assert_eq!(decode(&future), None);
    }
}
