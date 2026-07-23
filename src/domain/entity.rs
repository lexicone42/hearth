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
    /// Particulate matter ≤10µm (µg/m³). Reuses the Pm25 mass-concentration unit.
    Pm10,
    /// Volatile organic compounds as a unitless air-quality index (no ppb/µg —
    /// Dyson reports an AQI-style value; see `dyson::canonical`).
    VolatileOrganicCompounds,
    /// Nitrogen dioxide as a unitless air-quality index (same rationale as VOC).
    NitrogenDioxide,
    /// Remaining filter life as a percentage (0–100).
    FilterLife,
    /// Fan speed as a unitless step (Dyson: 1–10).
    FanSpeed,
    LightningStrikeCount,
    LightningDistance,
    BatteryLow,
    /// Battery state-of-charge as a percentage (0–100).
    Battery,
    /// Instantaneous electrical power (watts).
    Power,
    /// Accumulated electrical energy (watt-hours).
    Energy,
    /// Lock state as text (`locked`/`unlocked`/`jammed`/`unknown`), read back
    /// from a cloud sink (SmartThings). No standard SmartThings *write* mapping —
    /// it flows to the local API sink / watch, not back to SmartThings.
    Lock,
    /// A measured mass/weight (pounds/kilograms) — e.g. a Litter-Robot's last
    /// recorded pet weight. No standard SmartThings *write* mapping; flows to the
    /// local API sink / watch.
    Weight,
    /// Remaining litter level as a percentage (0–100). A low value is the
    /// "litter is low, refill it" signal (Whisker LR4).
    LitterLevel,
    /// Waste-drawer fullness as a percentage (0–100). A high value is the "empty
    /// the drawer" signal (Whisker LR4).
    WasteDrawer,
    /// A device's unit status as free text (e.g. a Litter-Robot's `Ready` /
    /// `Drawer Full` / `Offline`). No standard SmartThings mapping — local only.
    Status,
    /// A binary "needs attention" signal ([`crate::domain::Value::Flag`]): `true`
    /// = something needs changing. Maps to SmartThings `contactSensor` (`open` =
    /// attention) so an in-app Routine can push a notification. First used for a
    /// Litter-Robot's drawer-full / litter-low "time to change it" alert.
    Alert,
    Timestamp,
}
