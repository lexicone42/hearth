use std::fmt;

use serde::{Deserialize, Serialize};

/// Physical dimension shared by a set of interchangeable units. Conversion is
/// only ever defined *within* a dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Temperature,
    Pressure,
    Speed,
    /// Rainfall depth (small lengths): inches / millimetres.
    Precipitation,
    /// Geographic distance (large lengths): miles / kilometres.
    Distance,
    Angle,
    Ratio,
    Irradiance,
    MassConcentration,
    /// Instantaneous power: watts.
    Power,
    /// Accumulated energy: watt-hours.
    Energy,
    /// Dimensionless indices (UV index, ...).
    Index,
}

/// A unit of measure. Each unit knows its dimension and how to convert to/from
/// that dimension's base unit, which is all that's needed to convert between
/// any two units of the same dimension: `to.from_base(from.to_base(x))`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Fahrenheit,
    Celsius,
    InchesOfMercury,
    Hectopascal,
    MilesPerHour,
    KilometersPerHour,
    Inches,
    Millimeters,
    Miles,
    Kilometers,
    Degrees,
    Percent,
    WattsPerSquareMeter,
    MicrogramsPerCubicMeter,
    Watts,
    WattHours,
    Index,
}

impl Unit {
    pub fn dimension(self) -> Dimension {
        use Dimension as D;
        match self {
            Unit::Fahrenheit | Unit::Celsius => D::Temperature,
            Unit::InchesOfMercury | Unit::Hectopascal => D::Pressure,
            Unit::MilesPerHour | Unit::KilometersPerHour => D::Speed,
            Unit::Inches | Unit::Millimeters => D::Precipitation,
            Unit::Miles | Unit::Kilometers => D::Distance,
            Unit::Degrees => D::Angle,
            Unit::Percent => D::Ratio,
            Unit::WattsPerSquareMeter => D::Irradiance,
            Unit::MicrogramsPerCubicMeter => D::MassConcentration,
            Unit::Watts => D::Power,
            Unit::WattHours => D::Energy,
            Unit::Index => D::Index,
        }
    }

    /// Value in this unit -> the dimension's base unit.
    /// Bases: Celsius, hectopascal, m/s, millimetre, kilometre.
    fn to_base(self, v: f64) -> f64 {
        match self {
            Unit::Fahrenheit => (v - 32.0) * 5.0 / 9.0,
            Unit::InchesOfMercury => v * 33.863_886,
            Unit::MilesPerHour => v * 0.447_04,
            Unit::KilometersPerHour => v / 3.6,
            Unit::Inches => v * 25.4,
            Unit::Miles => v * 1.609_344,
            // Already a base unit (or dimensionless).
            Unit::Celsius
            | Unit::Hectopascal
            | Unit::Millimeters
            | Unit::Kilometers
            | Unit::Degrees
            | Unit::Percent
            | Unit::WattsPerSquareMeter
            | Unit::MicrogramsPerCubicMeter
            | Unit::Watts
            | Unit::WattHours
            | Unit::Index => v,
        }
    }

    /// Value in the dimension's base unit -> this unit.
    fn from_base(self, v: f64) -> f64 {
        match self {
            Unit::Fahrenheit => v * 9.0 / 5.0 + 32.0,
            Unit::InchesOfMercury => v / 33.863_886,
            Unit::MilesPerHour => v / 0.447_04,
            Unit::KilometersPerHour => v * 3.6,
            Unit::Inches => v / 25.4,
            Unit::Miles => v / 1.609_344,
            Unit::Celsius
            | Unit::Hectopascal
            | Unit::Millimeters
            | Unit::Kilometers
            | Unit::Degrees
            | Unit::Percent
            | Unit::WattsPerSquareMeter
            | Unit::MicrogramsPerCubicMeter
            | Unit::Watts
            | Unit::WattHours
            | Unit::Index => v,
        }
    }

    /// Convert a value to another unit, or `None` if the dimensions differ.
    pub fn convert(self, value: f64, to: Unit) -> Option<f64> {
        (self.dimension() == to.dimension()).then(|| to.from_base(self.to_base(value)))
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Unit::Fahrenheit => "°F",
            Unit::Celsius => "°C",
            Unit::InchesOfMercury => "inHg",
            Unit::Hectopascal => "hPa",
            Unit::MilesPerHour => "mph",
            Unit::KilometersPerHour => "km/h",
            Unit::Inches => "in",
            Unit::Millimeters => "mm",
            Unit::Miles => "mi",
            Unit::Kilometers => "km",
            Unit::Degrees => "°",
            Unit::Percent => "%",
            Unit::WattsPerSquareMeter => "W/m²",
            Unit::MicrogramsPerCubicMeter => "µg/m³",
            Unit::Watts => "W",
            Unit::WattHours => "Wh",
            Unit::Index => "",
        })
    }
}

/// Preferred unit system for display/output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnitSystem {
    Metric,
    Imperial,
}

impl Default for UnitSystem {
    fn default() -> Self {
        UnitSystem::Imperial
    }
}

impl UnitSystem {
    /// The unit this system prefers for a dimension, or `None` when the
    /// dimension is system-agnostic (percent, degrees, UV index, W/m², ...).
    pub fn preferred_unit(self, dim: Dimension) -> Option<Unit> {
        use Dimension as D;
        use UnitSystem::{Imperial, Metric};
        Some(match (self, dim) {
            (Imperial, D::Temperature) => Unit::Fahrenheit,
            (Metric, D::Temperature) => Unit::Celsius,
            (Imperial, D::Pressure) => Unit::InchesOfMercury,
            (Metric, D::Pressure) => Unit::Hectopascal,
            (Imperial, D::Speed) => Unit::MilesPerHour,
            (Metric, D::Speed) => Unit::KilometersPerHour,
            (Imperial, D::Precipitation) => Unit::Inches,
            (Metric, D::Precipitation) => Unit::Millimeters,
            (Imperial, D::Distance) => Unit::Miles,
            (Metric, D::Distance) => Unit::Kilometers,
            _ => return None,
        })
    }
}

/// A measured value — carrying its unit when the value is a physical quantity.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Quantity { value: f64, unit: Unit },
    Count(i64),
    Flag(bool),
    Text(String),
}

impl Value {
    pub fn quantity(value: f64, unit: Unit) -> Self {
        Value::Quantity { value, unit }
    }

    /// Re-express a quantity in `system`'s preferred unit. Non-quantities and
    /// system-agnostic quantities are returned unchanged.
    pub fn in_system(&self, system: UnitSystem) -> Value {
        let Value::Quantity { value, unit } = self else {
            return self.clone();
        };
        match system.preferred_unit(unit.dimension()) {
            Some(target) => Value::Quantity {
                value: unit.convert(*value, target).unwrap_or(*value),
                unit: target,
            },
            None => self.clone(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // UV index etc. read better without a trailing unit symbol.
            Value::Quantity { value, unit } if matches!(unit, Unit::Index) => {
                write!(f, "{value:.1}")
            }
            Value::Quantity { value, unit } => write!(f, "{value:.1} {unit}"),
            Value::Count(n) => write!(f, "{n}"),
            Value::Flag(b) => write!(f, "{b}"),
            Value::Text(s) => f.write_str(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-2
    }

    #[test]
    fn temperature_conversions() {
        assert!(approx(Unit::Fahrenheit.convert(32.0, Unit::Celsius).unwrap(), 0.0));
        assert!(approx(Unit::Fahrenheit.convert(212.0, Unit::Celsius).unwrap(), 100.0));
        assert!(approx(Unit::Celsius.convert(20.0, Unit::Fahrenheit).unwrap(), 68.0));
    }

    #[test]
    fn other_dimension_conversions() {
        assert!(approx(Unit::InchesOfMercury.convert(29.92, Unit::Hectopascal).unwrap(), 1013.21));
        assert!(approx(Unit::MilesPerHour.convert(10.0, Unit::KilometersPerHour).unwrap(), 16.0934));
        assert!(approx(Unit::Inches.convert(1.0, Unit::Millimeters).unwrap(), 25.4));
        assert!(approx(Unit::Miles.convert(1.0, Unit::Kilometers).unwrap(), 1.60934));
    }

    #[test]
    fn cross_dimension_conversion_is_rejected() {
        assert!(Unit::Fahrenheit.convert(50.0, Unit::MilesPerHour).is_none());
    }

    #[test]
    fn in_system_only_touches_convertible_quantities() {
        let f = Value::quantity(72.0, Unit::Fahrenheit);
        let Value::Quantity { value, unit } = f.in_system(UnitSystem::Metric) else {
            panic!("expected a quantity");
        };
        assert_eq!(unit, Unit::Celsius);
        assert!(approx(value, 22.22));

        // System-agnostic and non-quantity values pass through untouched.
        let pct = Value::quantity(55.0, Unit::Percent);
        assert_eq!(pct.in_system(UnitSystem::Metric), pct);
        assert_eq!(Value::Count(3).in_system(UnitSystem::Metric), Value::Count(3));
    }
}
