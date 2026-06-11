use std::fmt;

/// A stable, source-namespaced identifier for one observable channel.
///
/// Rendered as a dotted path, e.g. `ambient_weather.outdoor.temperature`.
/// Sinks map these to their own device models; the id itself is vendor-neutral.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntityId(String);

impl EntityId {
    /// Build an id from path segments: `EntityId::new(["ambient_weather",
    /// "outdoor", "temperature"])`.
    pub fn new(parts: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let joined = parts
            .into_iter()
            .map(|p| p.as_ref().to_owned())
            .collect::<Vec<_>>()
            .join(".");
        Self(joined)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Vendor-neutral semantic class of a measurement — the pivot every sink maps
/// from (Home Assistant calls this `device_class`). Adding a class here plus a
/// per-sink mapping is the entire cost of teaching the hub a new kind of sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceClass {
    Temperature,
    ApparentTemperature,
    DewPoint,
    Humidity,
    SoilMoisture,
    Pressure,
    WindSpeed,
    WindGust,
    WindBearing,
    PrecipitationRate,
    PrecipitationAccumulation,
    SolarRadiation,
    UvIndex,
    Pm25,
    LightningStrikeCount,
    LightningDistance,
    BatteryLow,
    Timestamp,
}
