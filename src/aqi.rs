//! US EPA Air Quality Index.
//!
//! Dyson reports individual pollutants; the single "air quality" figure in its
//! app is computed client-side and undocumented. Rather than invent a score,
//! hearth computes the actual **EPA AQI**, which has three properties a made-up
//! number doesn't: it's a published standard, its categories carry real health
//! guidance, and it names the pollutant responsible.
//!
//! Breakpoints are transcribed from EPA's *Technical Assistance Document for the
//! Reporting of Daily Air Quality* (**May 2026** revision — PM2.5's bands moved
//! in the 2024 NAAQS update, so older tables are wrong).
//!
//! ## Two details the standard insists on
//!
//! **Concentrations truncate before conversion** — PM2.5 to one decimal, PM10 to
//! a whole number. That's why the bands look like they have gaps (9.0 then 9.1):
//! truncation is what closes them, and skipping it puts values in the wrong band.
//!
//! **The overall AQI is the MAX of the sub-indices, not a blend.** Averaging
//! would let a clean pollutant mask a dangerous one. Taking the max also yields
//! the *driver* — the thing actually worth doing something about.
//!
//! ## And one the standard implies
//!
//! AQI is defined on a **24-hour average**, not a spot reading. An instantaneous
//! "AQI" is a category error. hearth averages the last 24 hours out of the
//! history store, which is only possible because it records everything.

/// `(conc_low, conc_high, aqi_low, aqi_high)` — a breakpoint band.
type Band = (f64, f64, f64, f64);

/// PM2.5, 24-hour, µg/m³. Concentrations truncate to 0.1 before lookup.
const PM25: &[Band] = &[
    (0.0, 9.0, 0.0, 50.0),
    (9.1, 35.4, 51.0, 100.0),
    (35.5, 55.4, 101.0, 150.0),
    (55.5, 125.4, 151.0, 200.0),
    (125.5, 225.4, 201.0, 300.0),
    (225.5, 325.4, 301.0, 500.0),
];

/// PM10, 24-hour, µg/m³. Concentrations truncate to a whole number.
const PM10: &[Band] = &[
    (0.0, 54.0, 0.0, 50.0),
    (55.0, 154.0, 51.0, 100.0),
    (155.0, 254.0, 101.0, 150.0),
    (255.0, 354.0, 151.0, 200.0),
    (355.0, 424.0, 201.0, 300.0),
    (425.0, 604.0, 301.0, 500.0),
];

/// Which pollutant a sub-index describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pollutant {
    Pm25,
    Pm10,
}

impl Pollutant {
    fn bands(self) -> &'static [Band] {
        match self {
            Pollutant::Pm25 => PM25,
            Pollutant::Pm10 => PM10,
        }
    }
    /// EPA truncation: PM2.5 keeps one decimal, PM10 keeps none.
    fn truncate(self, c: f64) -> f64 {
        match self {
            Pollutant::Pm25 => (c * 10.0).floor() / 10.0,
            Pollutant::Pm10 => c.floor(),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Pollutant::Pm25 => "PM2.5",
            Pollutant::Pm10 => "PM10",
        }
    }
}

/// An AQI category, with the health language EPA attaches to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Good,
    Moderate,
    UnhealthySensitive,
    Unhealthy,
    VeryUnhealthy,
    Hazardous,
}

impl Category {
    pub fn of(aqi: u32) -> Category {
        match aqi {
            0..=50 => Category::Good,
            51..=100 => Category::Moderate,
            101..=150 => Category::UnhealthySensitive,
            151..=200 => Category::Unhealthy,
            201..=300 => Category::VeryUnhealthy,
            _ => Category::Hazardous,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Category::Good => "Good",
            Category::Moderate => "Moderate",
            Category::UnhealthySensitive => "Unhealthy for Sensitive Groups",
            Category::Unhealthy => "Unhealthy",
            Category::VeryUnhealthy => "Very Unhealthy",
            Category::Hazardous => "Hazardous",
        }
    }
}

/// One pollutant's sub-index by EPA's piecewise linear interpolation:
/// `I = (I_hi − I_lo)/(BP_hi − BP_lo) · (C − BP_lo) + I_lo`, rounded.
///
/// `None` for a negative concentration (a sensor fault). A concentration above
/// the top band pins to 500 — the scale's defined maximum.
pub fn sub_index(p: Pollutant, conc: f64) -> Option<u32> {
    if !conc.is_finite() || conc < 0.0 {
        return None;
    }
    let c = p.truncate(conc);
    let bands = p.bands();
    for &(c_lo, c_hi, i_lo, i_hi) in bands {
        if c <= c_hi {
            // Below the first band's floor can't happen (it starts at 0).
            let c = c.max(c_lo);
            let i = (i_hi - i_lo) / (c_hi - c_lo) * (c - c_lo) + i_lo;
            return Some(i.round() as u32);
        }
    }
    Some(500)
}

/// A computed index: the value, its category, and the pollutant that set it.
#[derive(Debug, Clone, PartialEq)]
pub struct Aqi {
    pub value: u32,
    pub category: Category,
    pub driver: Pollutant,
}

/// Overall AQI from the available pollutants — the **maximum** sub-index, per
/// EPA. `None` when no pollutant yielded one.
pub fn overall(pm25: Option<f64>, pm10: Option<f64>) -> Option<Aqi> {
    let mut best: Option<(u32, Pollutant)> = None;
    for (p, conc) in [(Pollutant::Pm25, pm25), (Pollutant::Pm10, pm10)] {
        let Some(conc) = conc else { continue };
        let Some(i) = sub_index(p, conc) else {
            continue;
        };
        if best.is_none_or(|(bi, _)| i > bi) {
            best = Some((i, p));
        }
    }
    best.map(|(value, driver)| Aqi {
        value,
        category: Category::of(value),
        driver,
    })
}

/// The canonical channel an AQI observation lands on, alongside the pollutants
/// it was derived from: `dyson.<node>.aqi`.
pub const CHANNEL: &str = "aqi";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_edges_land_exactly_on_category_boundaries() {
        // The whole table is anchored by these: if interpolation or truncation is
        // wrong, an edge lands one off and every value near it is misreported.
        assert_eq!(sub_index(Pollutant::Pm25, 0.0), Some(0));
        assert_eq!(sub_index(Pollutant::Pm25, 9.0), Some(50));
        assert_eq!(sub_index(Pollutant::Pm25, 9.1), Some(51));
        assert_eq!(sub_index(Pollutant::Pm25, 35.4), Some(100));
        assert_eq!(sub_index(Pollutant::Pm25, 35.5), Some(101));
        assert_eq!(sub_index(Pollutant::Pm25, 55.4), Some(150));
        assert_eq!(sub_index(Pollutant::Pm25, 125.4), Some(200));
        assert_eq!(sub_index(Pollutant::Pm25, 225.4), Some(300));
        assert_eq!(sub_index(Pollutant::Pm25, 325.4), Some(500));

        assert_eq!(sub_index(Pollutant::Pm10, 54.0), Some(50));
        assert_eq!(sub_index(Pollutant::Pm10, 55.0), Some(51));
        assert_eq!(sub_index(Pollutant::Pm10, 154.0), Some(100));
        assert_eq!(sub_index(Pollutant::Pm10, 604.0), Some(500));
    }

    #[test]
    fn truncation_happens_before_lookup() {
        // 9.04 truncates to 9.0 -> still Good. Without truncation the
        // interpolation would push it into Moderate.
        assert_eq!(sub_index(Pollutant::Pm25, 9.04), Some(50));
        assert_eq!(
            Category::of(sub_index(Pollutant::Pm25, 9.04).unwrap()),
            Category::Good
        );
        // PM10 truncates to whole numbers: 54.9 is still 54.
        assert_eq!(sub_index(Pollutant::Pm10, 54.9), Some(50));
    }

    #[test]
    fn interpolates_within_a_band() {
        // 12.0 in 9.1–35.4 -> 51 + (100-51)/(35.4-9.1) * (12.0-9.1) = 56.4 -> 56
        assert_eq!(sub_index(Pollutant::Pm25, 12.0), Some(56));
        // Mid-band sanity: halfway up 55–154 is about halfway from 51 to 100.
        let mid = sub_index(Pollutant::Pm10, 104.5).unwrap();
        assert!((74..=76).contains(&mid), "got {mid}");
    }

    #[test]
    fn overall_takes_the_max_and_names_the_driver() {
        // Clean PM2.5, filthy PM10: averaging would hide it — the max must not.
        let a = overall(Some(2.0), Some(200.0)).unwrap();
        assert_eq!(a.driver, Pollutant::Pm10);
        assert_eq!(a.category, Category::UnhealthySensitive);

        let b = overall(Some(60.0), Some(10.0)).unwrap();
        assert_eq!(b.driver, Pollutant::Pm25);
        assert_eq!(b.category, Category::Unhealthy);

        // One pollutant is enough.
        assert_eq!(overall(Some(1.0), None).unwrap().driver, Pollutant::Pm25);
        assert_eq!(overall(None, Some(1.0)).unwrap().driver, Pollutant::Pm10);
        assert_eq!(overall(None, None), None);
    }

    #[test]
    fn rejects_sensor_faults_and_pins_the_top() {
        assert_eq!(sub_index(Pollutant::Pm25, -1.0), None);
        assert_eq!(sub_index(Pollutant::Pm25, f64::NAN), None);
        // Beyond the table: the scale stops at 500 rather than extrapolating.
        assert_eq!(sub_index(Pollutant::Pm25, 9_000.0), Some(500));
        assert_eq!(Category::of(500), Category::Hazardous);
    }

    #[test]
    fn categories_cover_the_scale() {
        assert_eq!(Category::of(50), Category::Good);
        assert_eq!(Category::of(51), Category::Moderate);
        assert_eq!(Category::of(100), Category::Moderate);
        assert_eq!(Category::of(101), Category::UnhealthySensitive);
        assert_eq!(Category::of(151), Category::Unhealthy);
        assert_eq!(Category::of(201), Category::VeryUnhealthy);
        assert_eq!(Category::of(301), Category::Hazardous);
    }
}
