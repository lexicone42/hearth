use std::collections::BTreeMap;

use serde::Deserialize;

/// A single observation (Ambient Weather calls this `lastData`) from a station.
///
/// Which fields are present depends entirely on the station model and the
/// sensors attached to it, so almost everything is `Option`. JSON keys mirror
/// the Ambient Weather API exactly (https://ambientweather.docs.apiary.io/);
/// where the API uses camelCase we keep an idiomatic snake_case Rust name and
/// bridge with `#[serde(rename = ...)]`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AwReading {
    /// Observation time as epoch milliseconds (UTC). The raw ISO `date` string
    /// is preserved in `extra`.
    pub dateutc: Option<i64>,

    // ----- Outdoor -----
    pub tempf: Option<f64>,
    pub humidity: Option<f64>,
    #[serde(rename = "feelsLike")]
    pub feels_like: Option<f64>,
    #[serde(rename = "dewPoint")]
    pub dew_point: Option<f64>,

    // ----- Indoor (console) -----
    pub tempinf: Option<f64>,
    pub humidityin: Option<f64>,
    #[serde(rename = "feelsLikein")]
    pub feels_like_in: Option<f64>,
    #[serde(rename = "dewPointin")]
    pub dew_point_in: Option<f64>,

    // ----- Wind -----
    pub windspeedmph: Option<f64>,
    pub windspdmph_avg10m: Option<f64>,
    pub windgustmph: Option<f64>,
    pub maxdailygust: Option<f64>,
    pub winddir: Option<f64>,
    pub winddir_avg10m: Option<f64>,

    // ----- Barometer -----
    pub baromrelin: Option<f64>,
    pub baromabsin: Option<f64>,

    // ----- Rain -----
    pub hourlyrainin: Option<f64>,
    pub dailyrainin: Option<f64>,
    pub weeklyrainin: Option<f64>,
    pub monthlyrainin: Option<f64>,
    pub yearlyrainin: Option<f64>,
    pub totalrainin: Option<f64>,
    pub eventrainin: Option<f64>,
    #[serde(rename = "lastRain")]
    pub last_rain: Option<String>,

    // ----- Sun -----
    pub uv: Option<f64>,
    pub solarradiation: Option<f64>,

    // ----- Air quality -----
    pub pm25: Option<f64>,
    pub pm25_24h: Option<f64>,

    // ----- Lightning -----
    pub lightning_day: Option<i64>,
    pub lightning_distance: Option<f64>,
    pub lightning_time: Option<i64>,

    // ----- Batteries (1 = OK, 0 = low for most sensors) -----
    pub battout: Option<i64>,
    pub battin: Option<i64>,

    /// Any field not modeled above (extra remote sensors, soil probes, CO2,
    /// the raw `date` string, ...) is captured here so nothing is silently
    /// dropped. Inspect it against a live station to discover what to promote
    /// into typed fields. Read out-of-band (logs/debugger), hence `allow`.
    #[serde(flatten)]
    #[allow(dead_code)]
    pub extra: BTreeMap<String, serde_json::Value>,
}
