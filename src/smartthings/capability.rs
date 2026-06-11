use serde_json::json;

use crate::domain::{DeviceClass, Observation, Unit, UnitSystem, Value};

/// A SmartThings capability event — the (capability, attribute, value, unit)
/// tuple that becomes one entry in a `deviceEvents` POST.
#[derive(Debug, Clone, PartialEq)]
pub struct StEvent {
    pub capability: &'static str,
    pub attribute: &'static str,
    pub value: serde_json::Value,
    pub unit: Option<&'static str>,
}

/// The (capability, attribute) a class maps to, if it has a standard SmartThings
/// mapping. Single source of truth shared by event-building and provisioning, so
/// a device's profile and the events sent to it can't drift apart.
fn standard_capability(class: DeviceClass) -> Option<(&'static str, &'static str)> {
    use DeviceClass as C;
    Some(match class {
        C::Temperature | C::ApparentTemperature | C::DewPoint => {
            ("temperatureMeasurement", "temperature")
        }
        C::Humidity => ("relativeHumidityMeasurement", "humidity"),
        C::UvIndex => ("ultravioletIndex", "ultravioletIndex"),
        C::Pm25 => ("fineDustSensor", "fineDustLevel"),
        C::BatteryLow => ("battery", "battery"),
        _ => return None,
    })
}

/// The SmartThings capability id a class maps to, if any — used to build a
/// device profile during provisioning.
pub fn capability_id(class: DeviceClass) -> Option<&'static str> {
    standard_capability(class).map(|(capability, _)| capability)
}

/// Map a canonical observation to a SmartThings standard capability event,
/// re-expressing quantities in `system`. `None` for classes with no standard
/// capability yet (wind, rain, pressure, solar, lightning).
pub fn to_event(obs: &Observation, system: UnitSystem) -> Option<StEvent> {
    let (capability, attribute) = standard_capability(obs.class)?;
    let (value, unit) = encode_value(obs, system)?;
    Some(StEvent { capability, attribute, value, unit })
}

/// Encode the value + unit for a class that has a standard capability.
fn encode_value(
    obs: &Observation,
    system: UnitSystem,
) -> Option<(serde_json::Value, Option<&'static str>)> {
    use DeviceClass as C;
    match obs.class {
        C::Temperature | C::ApparentTemperature | C::DewPoint => {
            let (value, unit) = quantity(&obs.value.in_system(system))?;
            Some((json!(round1(value)), Some(temperature_unit(unit))))
        }
        C::Humidity => {
            let (value, _) = quantity(&obs.value)?;
            Some((json!(round1(value)), Some("%")))
        }
        C::UvIndex => {
            let (value, _) = quantity(&obs.value)?;
            Some((json!(round1(value)), None))
        }
        C::Pm25 => {
            let (value, _) = quantity(&obs.value)?;
            Some((json!(round1(value)), Some("μg/m^3")))
        }
        C::BatteryLow => match obs.value {
            Value::Flag(low) => Some((json!(if low { 10 } else { 100 }), Some("%"))),
            _ => None,
        },
        _ => None,
    }
}

fn quantity(v: &Value) -> Option<(f64, Unit)> {
    match v {
        Value::Quantity { value, unit } => Some((*value, *unit)),
        _ => None,
    }
}

fn temperature_unit(unit: Unit) -> &'static str {
    match unit {
        Unit::Celsius => "C",
        _ => "F",
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EntityId;

    fn obs(class: DeviceClass, value: Value) -> Observation {
        Observation::new(EntityId::new(["t"]), class, value, None)
    }

    #[test]
    fn temperature_respects_unit_system() {
        let o = obs(DeviceClass::Temperature, Value::quantity(72.5, Unit::Fahrenheit));

        let imp = to_event(&o, UnitSystem::Imperial).unwrap();
        assert_eq!(imp.capability, "temperatureMeasurement");
        assert_eq!(imp.attribute, "temperature");
        assert_eq!(imp.unit, Some("F"));
        assert_eq!(imp.value, json!(72.5));

        let met = to_event(&o, UnitSystem::Metric).unwrap();
        assert_eq!(met.unit, Some("C"));
        assert_eq!(met.value, json!(22.5));
    }

    #[test]
    fn humidity_battery_and_unmapped() {
        let h = to_event(
            &obs(DeviceClass::Humidity, Value::quantity(55.0, Unit::Percent)),
            UnitSystem::Imperial,
        )
        .unwrap();
        assert_eq!(h.capability, "relativeHumidityMeasurement");
        assert_eq!(h.value, json!(55.0));

        let b = to_event(
            &obs(DeviceClass::BatteryLow, Value::Flag(true)),
            UnitSystem::Imperial,
        )
        .unwrap();
        assert_eq!(b.capability, "battery");
        assert_eq!(b.value, json!(10));

        let w = to_event(
            &obs(DeviceClass::WindSpeed, Value::quantity(4.0, Unit::MilesPerHour)),
            UnitSystem::Imperial,
        );
        assert!(w.is_none());
    }

    #[test]
    fn capability_id_for_provisioning() {
        assert_eq!(capability_id(DeviceClass::Temperature), Some("temperatureMeasurement"));
        assert_eq!(capability_id(DeviceClass::Humidity), Some("relativeHumidityMeasurement"));
        assert_eq!(capability_id(DeviceClass::WindSpeed), None);
    }
}
