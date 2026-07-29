//! BirdWeather PUC — bird (and bat) detections from a BirdNET listening station.
//!
//! Refreshingly ordinary after Whisker and EcoFlow: a documented REST/JSON API
//! at `app.birdweather.com/api/v1`, no signing, no SRP. Auth is a station token
//! in the path — and reads also work with the bare station number, which is how
//! BirdWeather's public map exists. hearth accepts either:
//!
//! ```text
//! GET /api/v1/stations/{token}/detections?from&to&limit   (limit caps at 100)
//! GET /api/v1/stations/{token}/species?from
//! GET /api/v1/stations/{token}/stats
//! ```
//!
//! ## What lands where
//!
//! Detections are **events**, not observations: one detection carries species,
//! confidence, score and a clip, all correlated. So they go to the event table
//! ([`crate::history::store`]) and only the *summaries* — how many species and
//! detections today, and the latest bird — ride the bus as observations.
//!
//! ## Two things the API makes you handle
//!
//! `limit` is capped at **100** regardless of what you ask for, so a full catch-up
//! has to page. hearth pages by **time**, not cursor: the response carries no
//! cursor, but `from`/`to` are honoured, and the event table's own newest
//! timestamp is a natural watermark. Re-fetching an overlapping window is free
//! because the event index dedups by detection id.
//!
//! Timestamps come back with an offset (`2026-07-29T14:34:33.000-07:00`), not as
//! UTC `Z`, so they need real offset parsing rather than string slicing.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::domain::{DeviceClass, EntityId, Observation, Value};

const BASE: &str = "https://app.birdweather.com/api/v1";

/// The event-table source key. Also the entity namespace, so a detection archive
/// and its summary observations are obviously the same thing.
pub const SOURCE: &str = "birdweather";

/// A species as BirdWeather describes it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Species {
    #[serde(default)]
    pub id: i64,
    #[serde(rename = "commonName", default)]
    pub common_name: String,
    #[serde(rename = "scientificName", default)]
    pub scientific_name: String,
    /// Species accent colour, handy for charting.
    #[serde(default)]
    pub color: Option<String>,
    #[serde(rename = "thumbnailUrl", default)]
    pub thumbnail_url: Option<String>,
    /// `avian`, and on a PUC with bat detection also other classes.
    #[serde(default)]
    pub classification: Option<String>,
}

/// The clip a detection was drawn from. Kept only in the local archive — never
/// surfaced by hearth's API or dashboard, because a yard microphone's audio is
/// the owner's business.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Soundscape {
    #[serde(default)]
    pub url: Option<String>,
}

/// One detection. Every field optional: this is someone else's evolving API.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Detection {
    #[serde(default)]
    pub id: i64,
    pub timestamp: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub probability: Option<f64>,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default)]
    pub certainty: Option<String>,
    #[serde(default)]
    pub species: Species,
    #[serde(default)]
    pub soundscape: Option<Soundscape>,
}

#[derive(Deserialize)]
struct DetectionsResponse {
    #[serde(default)]
    detections: Vec<Detection>,
}

#[derive(Deserialize)]
struct SpeciesResponse {
    #[serde(default)]
    species: Vec<Species>,
}

/// Thin client over one station.
pub struct BirdWeatherClient {
    http: reqwest::Client,
    token: String,
}

impl BirdWeatherClient {
    pub fn new(token: impl Into<String>) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("hearth/", env!("CARGO_PKG_VERSION")))
                .build()
                .context("building BirdWeather HTTP client")?,
            token: token.into(),
        })
    }

    async fn get_json(&self, path: &str) -> Result<String> {
        let url = format!("{BASE}/stations/{}/{path}", self.token);
        let resp = self
            .http
            .get(&url)
            // Also send the token as a header: BirdWeather accepts either, and a
            // private station may stop honouring the path form.
            .header("X-Auth-Token", &self.token)
            .send()
            .await
            .context("requesting BirdWeather")?;
        let status = resp.status();
        let body = resp.text().await.context("reading BirdWeather response")?;
        if !status.is_success() {
            anyhow::bail!("BirdWeather returned HTTP {status}");
        }
        Ok(body)
    }

    /// Detections since `from` (ISO-8601), newest-first as the API returns them.
    /// `limit` is capped at 100 upstream regardless of what we ask.
    pub async fn detections(&self, from: Option<&str>, limit: usize) -> Result<Vec<Detection>> {
        let mut path = format!("detections?limit={}", limit.min(100));
        if let Some(from) = from {
            path.push_str(&format!("&from={}", urlencode(from)));
        }
        let body = self.get_json(&path).await?;
        Ok(serde_json::from_str::<DetectionsResponse>(&body)
            .context("decoding BirdWeather detections")?
            .detections)
    }

    /// Species recorded since `from`.
    pub async fn species_since(&self, from: &str) -> Result<Vec<Species>> {
        let body = self
            .get_json(&format!("species?from={}", urlencode(from)))
            .await?;
        Ok(serde_json::from_str::<SpeciesResponse>(&body)
            .context("decoding BirdWeather species")?
            .species)
    }
}

/// Minimal percent-encoding for the timestamps and names we put in query
/// strings — enough for ISO-8601 (`:` and `+`), without pulling in a crate.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Parse BirdWeather's offset timestamps (`2026-07-29T14:34:33.000-07:00`) to
/// epoch ms. Written out rather than string-sliced because the offset is real and
/// ignoring it would misplace every detection by up to a day.
pub fn parse_ts(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, z: usize| ts.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // Days from civil date (inverse of clock::civil_from_days).
    let (yy, mm) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let doy = (153 * mm + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut ms = ((days * 86_400) + h * 3600 + mi * 60 + s) * 1000;

    // Trailing zone: `Z`, or `±HH:MM` — subtract the offset to reach UTC.
    let rest = &ts[19..];
    let zone = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if let Some(sign) = zone.chars().next()
        && (sign == '+' || sign == '-')
    {
        {
            let oz = &zone[1..];
            let oh: i64 = oz.get(0..2).and_then(|x| x.parse().ok()).unwrap_or(0);
            let om: i64 = oz
                .get(3..5)
                .or_else(|| oz.get(2..4))
                .and_then(|x| x.parse().ok())
                .unwrap_or(0);
            let off = (oh * 3600 + om * 60) * 1000;
            ms += if sign == '-' { off } else { -off };
        }
    }
    Some(ms)
}

/// Summary observations for the bus: how much was heard today, and the latest
/// bird. The detections themselves live in the event archive.
pub fn to_observations(
    station: &str,
    species_today: usize,
    detections_today: usize,
    latest: Option<&Detection>,
) -> Vec<Observation> {
    let mut out = vec![
        Observation::new(
            EntityId::new([SOURCE, station, "species_today"]),
            DeviceClass::SpeciesCount,
            Value::Count(species_today as i64),
            None,
        ),
        Observation::new(
            EntityId::new([SOURCE, station, "detections_today"]),
            DeviceClass::SpeciesCount,
            Value::Count(detections_today as i64),
            None,
        ),
    ];
    if let Some(d) = latest.filter(|d| !d.species.common_name.is_empty()) {
        out.push(Observation::new(
            EntityId::new([SOURCE, station, "latest_species"]),
            DeviceClass::Status,
            Value::Text(d.species.common_name.clone()),
            None,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_offset_timestamps() {
        // The shape BirdWeather actually returns: local time plus an offset.
        // 14:34:33 at -07:00 is 21:34:33 UTC.
        let ms = parse_ts("2026-07-29T14:34:33.000-07:00").unwrap();
        assert_eq!(crate::clock::iso_utc(ms), "2026-07-29T21:34:33Z");
        // A `Z` timestamp needs no shift.
        let z = parse_ts("2026-07-29T21:34:33Z").unwrap();
        assert_eq!(ms, z, "offset and UTC forms must agree");
        // A positive offset goes the other way.
        let plus = parse_ts("2026-07-30T05:34:33.000+08:00").unwrap();
        assert_eq!(crate::clock::iso_utc(plus), "2026-07-29T21:34:33Z");
        // Epoch, and garbage.
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ts("nope"), None);
        assert_eq!(parse_ts(""), None);
    }

    #[test]
    fn decodes_a_real_detection_shape() {
        // Field-for-field the live payload (values changed).
        let raw = r##"{"detections":[{"id":10066436493,"stationId":2410,
            "timestamp":"2026-07-29T14:34:33.000-07:00","confidence":0.9695,
            "probability":0.1855,"score":7.62,"certainty":"almost_certain",
            "algorithm":"BirdNET","metadata":null,
            "species":{"id":1511,"commonName":"Fixture Swift","scientificName":"Chaetura fixtura",
                       "color":"#7a41c7","classification":"avian","thumbnailUrl":"https://example/t.jpg"},
            "lat":1.0,"lon":2.0,"favorite":false,
            "soundscape":{"id":1,"url":"https://example/clip.flac"}}]}"##;
        let r: DetectionsResponse = serde_json::from_str(raw).unwrap();
        let d = &r.detections[0];
        assert_eq!(d.id, 10066436493);
        assert_eq!(d.species.common_name, "Fixture Swift");
        assert_eq!(d.certainty.as_deref(), Some("almost_certain"));
        assert!(d.soundscape.as_ref().unwrap().url.is_some());
        assert!(parse_ts(d.timestamp.as_deref().unwrap()).is_some());
    }

    #[test]
    fn tolerates_a_thinner_payload() {
        // Unknown fields ignored, missing ones defaulted: their API can change
        // without taking the source down.
        let r: DetectionsResponse =
            serde_json::from_str(r#"{"detections":[{"id":1,"somethingNew":true}],"extra":9}"#)
                .unwrap();
        assert_eq!(r.detections[0].id, 1);
        assert!(r.detections[0].timestamp.is_none());
        assert_eq!(r.detections[0].species.common_name, "");
    }

    #[test]
    fn observations_summarize_without_leaking_the_clip() {
        let d = Detection {
            species: Species {
                common_name: "Fixture Swift".into(),
                ..Default::default()
            },
            soundscape: Some(Soundscape {
                url: Some("https://example/private.flac".into()),
            }),
            ..Default::default()
        };
        let obs = to_observations("2410", 14, 147, Some(&d));
        let ids: Vec<&str> = obs.iter().map(|o| o.entity.as_str()).collect();
        assert!(ids.contains(&"birdweather.2410.species_today"));
        assert!(ids.contains(&"birdweather.2410.detections_today"));
        assert!(ids.contains(&"birdweather.2410.latest_species"));
        // The audio URL must never ride the bus — it goes only to the local archive.
        for o in &obs {
            if let Value::Text(t) = &o.value {
                assert!(!t.contains("flac"), "clip URL leaked into an observation");
            }
        }
        // A nameless detection contributes no status text rather than an empty one.
        let bare = to_observations("2410", 0, 0, Some(&Detection::default()));
        assert_eq!(bare.len(), 2);
    }

    #[test]
    fn urlencodes_iso_timestamps() {
        assert_eq!(
            urlencode("2026-07-29T00:00:00Z"),
            "2026-07-29T00%3A00%3A00Z"
        );
        assert_eq!(urlencode("a+b c"), "a%2Bb%20c");
    }
}
