use std::collections::BTreeMap;

use crate::ambient::model::AwReading;
use crate::domain::{DeviceClass, EntityId, Observation, Unit, Value};

/// Source namespace for every entity this module produces.
const SOURCE: &str = "ambient_weather";

/// Number of indexed remote sensor slots Ambient Weather supports.
const MAX_REMOTE_SENSORS: u8 = 8;

/// Normalize a raw Ambient Weather observation into canonical `Observation`s.
///
/// Pure and total: each present field becomes exactly one observation, absent
/// sensors produce nothing. This is the *only* place in the codebase that knows
/// Ambient Weather's field names and units — everything downstream sees the
/// vendor-neutral [`Observation`] model.
///
/// Base-station fields are strongly typed on [`AwReading`]; the variable-count
/// indexed remote sensors are discovered dynamically from `AwReading::extra`.
pub fn to_observations(reading: &AwReading) -> Vec<Observation> {
    use DeviceClass as C;
    use Unit::*;

    let ts = reading.dateutc;
    let mut out = Vec::new();
    let mut push = |node: &str, channel: &str, class: DeviceClass, value: Value| {
        out.push(Observation::new(
            EntityId::new([SOURCE, node, channel]),
            class,
            value,
            ts,
        ));
    };

    // ----- Outdoor -----
    if let Some(v) = reading.tempf {
        push(
            "outdoor",
            "temperature",
            C::Temperature,
            Value::quantity(v, Fahrenheit),
        );
    }
    if let Some(v) = reading.humidity {
        push(
            "outdoor",
            "humidity",
            C::Humidity,
            Value::quantity(v, Percent),
        );
    }
    if let Some(v) = reading.feels_like {
        push(
            "outdoor",
            "feels_like",
            C::ApparentTemperature,
            Value::quantity(v, Fahrenheit),
        );
    }
    if let Some(v) = reading.dew_point {
        push(
            "outdoor",
            "dew_point",
            C::DewPoint,
            Value::quantity(v, Fahrenheit),
        );
    }
    if let Some(v) = reading.battout {
        // Ambient encodes batteries as 1 = OK, 0 = low.
        push("outdoor", "battery_low", C::BatteryLow, Value::Flag(v == 0));
    }

    // ----- Indoor (console) -----
    if let Some(v) = reading.tempinf {
        push(
            "indoor",
            "temperature",
            C::Temperature,
            Value::quantity(v, Fahrenheit),
        );
    }
    if let Some(v) = reading.humidityin {
        push(
            "indoor",
            "humidity",
            C::Humidity,
            Value::quantity(v, Percent),
        );
    }
    if let Some(v) = reading.feels_like_in {
        push(
            "indoor",
            "feels_like",
            C::ApparentTemperature,
            Value::quantity(v, Fahrenheit),
        );
    }
    if let Some(v) = reading.dew_point_in {
        push(
            "indoor",
            "dew_point",
            C::DewPoint,
            Value::quantity(v, Fahrenheit),
        );
    }
    if let Some(v) = reading.battin {
        push("indoor", "battery_low", C::BatteryLow, Value::Flag(v == 0));
    }

    // ----- Wind -----
    if let Some(v) = reading.windspeedmph {
        push(
            "wind",
            "speed",
            C::WindSpeed,
            Value::quantity(v, MilesPerHour),
        );
    }
    if let Some(v) = reading.windspdmph_avg10m {
        push(
            "wind",
            "speed_avg_10m",
            C::WindSpeed,
            Value::quantity(v, MilesPerHour),
        );
    }
    if let Some(v) = reading.windgustmph {
        push(
            "wind",
            "gust",
            C::WindGust,
            Value::quantity(v, MilesPerHour),
        );
    }
    if let Some(v) = reading.maxdailygust {
        push(
            "wind",
            "max_daily_gust",
            C::WindGust,
            Value::quantity(v, MilesPerHour),
        );
    }
    if let Some(v) = reading.winddir {
        push(
            "wind",
            "bearing",
            C::WindBearing,
            Value::quantity(v, Degrees),
        );
    }
    if let Some(v) = reading.winddir_avg10m {
        push(
            "wind",
            "bearing_avg_10m",
            C::WindBearing,
            Value::quantity(v, Degrees),
        );
    }

    // ----- Barometer -----
    if let Some(v) = reading.baromrelin {
        push(
            "barometer",
            "relative",
            C::Pressure,
            Value::quantity(v, InchesOfMercury),
        );
    }
    if let Some(v) = reading.baromabsin {
        push(
            "barometer",
            "absolute",
            C::Pressure,
            Value::quantity(v, InchesOfMercury),
        );
    }

    // ----- Rain -----
    if let Some(v) = reading.hourlyrainin {
        push(
            "rain",
            "rate",
            C::PrecipitationRate,
            Value::quantity(v, Inches),
        );
    }
    if let Some(v) = reading.dailyrainin {
        push(
            "rain",
            "daily",
            C::PrecipitationAccumulation,
            Value::quantity(v, Inches),
        );
    }
    if let Some(v) = reading.weeklyrainin {
        push(
            "rain",
            "weekly",
            C::PrecipitationAccumulation,
            Value::quantity(v, Inches),
        );
    }
    if let Some(v) = reading.monthlyrainin {
        push(
            "rain",
            "monthly",
            C::PrecipitationAccumulation,
            Value::quantity(v, Inches),
        );
    }
    if let Some(v) = reading.yearlyrainin {
        push(
            "rain",
            "yearly",
            C::PrecipitationAccumulation,
            Value::quantity(v, Inches),
        );
    }
    if let Some(v) = reading.totalrainin {
        push(
            "rain",
            "total",
            C::PrecipitationAccumulation,
            Value::quantity(v, Inches),
        );
    }
    if let Some(v) = reading.eventrainin {
        push(
            "rain",
            "event",
            C::PrecipitationAccumulation,
            Value::quantity(v, Inches),
        );
    }
    if let Some(v) = &reading.last_rain {
        push("rain", "last", C::Timestamp, Value::Text(v.clone()));
    }

    // ----- Sun -----
    if let Some(v) = reading.uv {
        push("sun", "uv_index", C::UvIndex, Value::quantity(v, Index));
    }
    if let Some(v) = reading.solarradiation {
        push(
            "sun",
            "solar_radiation",
            C::SolarRadiation,
            Value::quantity(v, WattsPerSquareMeter),
        );
    }

    // ----- Air quality -----
    if let Some(v) = reading.pm25 {
        push(
            "air_quality",
            "pm25",
            C::Pm25,
            Value::quantity(v, MicrogramsPerCubicMeter),
        );
    }
    if let Some(v) = reading.pm25_24h {
        push(
            "air_quality",
            "pm25_24h_avg",
            C::Pm25,
            Value::quantity(v, MicrogramsPerCubicMeter),
        );
    }

    // ----- Lightning -----
    if let Some(v) = reading.lightning_day {
        push(
            "lightning",
            "strikes_today",
            C::LightningStrikeCount,
            Value::Count(v),
        );
    }
    if let Some(v) = reading.lightning_distance {
        push(
            "lightning",
            "distance",
            C::LightningDistance,
            Value::quantity(v, Miles),
        );
    }
    if let Some(v) = reading.lightning_time {
        push(
            "lightning",
            "last_strike",
            C::Timestamp,
            Value::Text(v.to_string()),
        );
    }

    // ----- Indexed remote sensors, parsed from `extra` -----
    // Ambient numbers add-on temp/humidity sensors 1..=8 (`temp{n}f`, etc.) and
    // soil-moisture probes (`soilhum{n}`/`battsm{n}`). Their count varies per
    // station, so we discover them from the flatten-captured `extra` map rather
    // than modeling 40+ fixed fields. This is what the capture was for.
    let extra = &reading.extra;
    for n in 1..=MAX_REMOTE_SENSORS {
        let node = format!("sensor_{n}");
        if let Some(v) = extra_f64(extra, &format!("temp{n}f")) {
            push(
                &node,
                "temperature",
                C::Temperature,
                Value::quantity(v, Fahrenheit),
            );
        }
        if let Some(v) = extra_f64(extra, &format!("humidity{n}")) {
            push(&node, "humidity", C::Humidity, Value::quantity(v, Percent));
        }
        if let Some(v) = extra_f64(extra, &format!("feelsLike{n}")) {
            push(
                &node,
                "feels_like",
                C::ApparentTemperature,
                Value::quantity(v, Fahrenheit),
            );
        }
        if let Some(v) = extra_f64(extra, &format!("dewPoint{n}")) {
            push(
                &node,
                "dew_point",
                C::DewPoint,
                Value::quantity(v, Fahrenheit),
            );
        }
        if let Some(v) = extra_i64(extra, &format!("batt{n}")) {
            push(&node, "battery_low", C::BatteryLow, Value::Flag(v == 0));
        }
    }
    for n in 1..=MAX_REMOTE_SENSORS {
        let node = format!("soil_{n}");
        if let Some(v) = extra_f64(extra, &format!("soilhum{n}")) {
            push(
                &node,
                "moisture",
                C::SoilMoisture,
                Value::quantity(v, Percent),
            );
        }
        if let Some(v) = extra_i64(extra, &format!("battsm{n}")) {
            push(&node, "battery_low", C::BatteryLow, Value::Flag(v == 0));
        }
    }

    out
}

fn extra_f64(extra: &BTreeMap<String, serde_json::Value>, key: &str) -> Option<f64> {
    extra.get(key).and_then(serde_json::Value::as_f64)
}

fn extra_i64(extra: &BTreeMap<String, serde_json::Value>, key: &str) -> Option<i64> {
    extra.get(key).and_then(serde_json::Value::as_i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ambient::model::AwReading;
    use serde_json::json;

    fn find<'a>(obs: &'a [Observation], id: &str) -> Option<&'a Observation> {
        obs.iter().find(|o| o.entity.as_str() == id)
    }

    #[test]
    fn maps_present_fields_only() {
        let reading = AwReading {
            dateutc: Some(123),
            tempf: Some(72.5),
            humidity: Some(55.0),
            windspeedmph: Some(4.0),
            dailyrainin: Some(0.1),
            battout: Some(0),
            ..Default::default()
        };
        let obs = to_observations(&reading);

        let temp = find(&obs, "ambient_weather.outdoor.temperature").expect("temperature");
        assert_eq!(temp.class, DeviceClass::Temperature);
        assert_eq!(temp.value, Value::quantity(72.5, Unit::Fahrenheit));
        assert_eq!(temp.observed_at, Some(123));

        // 0 => battery low.
        let batt = find(&obs, "ambient_weather.outdoor.battery_low").expect("battery");
        assert_eq!(batt.value, Value::Flag(true));

        // An absent sensor yields nothing, and only the 5 set fields mapped.
        assert!(find(&obs, "ambient_weather.sun.uv_index").is_none());
        assert_eq!(obs.len(), 5);
    }

    #[test]
    fn maps_indexed_remote_sensors_from_extra() {
        let mut extra = BTreeMap::new();
        extra.insert("temp7f".to_string(), json!(-2.6)); // a freezer, perhaps
        extra.insert("humidity7".to_string(), json!(72)); // integer in the JSON
        extra.insert("batt7".to_string(), json!(1));
        extra.insert("soilhum3".to_string(), json!(9));
        extra.insert("battsm3".to_string(), json!(0));

        let reading = AwReading {
            dateutc: Some(1),
            extra,
            ..Default::default()
        };
        let obs = to_observations(&reading);

        let t = find(&obs, "ambient_weather.sensor_7.temperature").expect("sensor 7 temp");
        assert_eq!(t.class, DeviceClass::Temperature);
        assert_eq!(t.value, Value::quantity(-2.6, Unit::Fahrenheit));

        let h = find(&obs, "ambient_weather.sensor_7.humidity").expect("sensor 7 humidity");
        assert_eq!(h.value, Value::quantity(72.0, Unit::Percent));

        // batt7 = 1 => OK (not low).
        let b = find(&obs, "ambient_weather.sensor_7.battery_low").expect("sensor 7 battery");
        assert_eq!(b.value, Value::Flag(false));

        let soil = find(&obs, "ambient_weather.soil_3.moisture").expect("soil 3 moisture");
        assert_eq!(soil.class, DeviceClass::SoilMoisture);
        assert_eq!(soil.value, Value::quantity(9.0, Unit::Percent));

        // battsm3 = 0 => low.
        let sb = find(&obs, "ambient_weather.soil_3.battery_low").expect("soil 3 battery");
        assert_eq!(sb.value, Value::Flag(true));

        // An unpopulated index produces nothing.
        assert!(find(&obs, "ambient_weather.sensor_2.temperature").is_none());
    }
}
