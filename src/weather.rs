//! `!weather`: current conditions and a three-day forecast from Open-Meteo, which needs
//! no API key. The place is found with Open-Meteo's own geocoder, so only a place name
//! goes out, never a URL from chat.

use anyhow::{Context, Result};
use serde::Deserialize;

const GEOCODING_URL: &str = "https://geocoding-api.open-meteo.com/v1/search";
const FORECAST_URL: &str = "https://api.open-meteo.com/v1/forecast";
/// Enough candidates for a qualifier like "Portland, Maine" to find the right one.
const CANDIDATES: &str = "10";
const FORECAST_DAYS: &str = "3";

#[derive(Debug, Deserialize)]
struct Places {
    #[serde(default)]
    results: Vec<Place>,
}

#[derive(Debug, Clone, Deserialize)]
struct Place {
    name: String,
    latitude: f64,
    longitude: f64,
    country: Option<String>,
    country_code: Option<String>,
    admin1: Option<String>,
    admin2: Option<String>,
}

impl Place {
    /// "Cherry Hill, NJ, USA" for a US place, the way people there write it, and
    /// "Paris, Île-de-France, France" elsewhere.
    fn label(&self) -> String {
        let american = self.country_code.as_deref() == Some("US");
        let state = self.admin1.as_deref().map(|state| if american { state_abbreviation(state).unwrap_or(state) } else { state });
        let country = if american { Some("USA") } else { self.country.as_deref() };
        let mut parts: Vec<&str> = [Some(self.name.as_str()), state, country].into_iter().flatten().filter(|part| !part.is_empty()).collect();
        // A city-state like Singapore is its own state and country.
        parts.dedup();
        parts.join(", ")
    }

    /// Whether a qualifier after the comma, like "MA", "Maine", "UK", or "France",
    /// names this place's state, county, or country.
    fn matches(&self, qualifier: &str) -> bool {
        let qualifier = qualifier.trim().to_lowercase();
        let qualifier = match qualifier.as_str() {
            "uk" | "england" | "scotland" | "wales" | "great britain" => "gb".to_string(),
            "usa" | "united states of america" => "us".to_string(),
            _ => us_state(&qualifier).map(str::to_lowercase).unwrap_or(qualifier),
        };
        [&self.country_code, &self.country, &self.admin1, &self.admin2]
            .into_iter()
            .flatten()
            .any(|field| field.to_lowercase() == qualifier)
    }
}

#[derive(Debug, Deserialize)]
struct Forecast {
    timezone_abbreviation: Option<String>,
    current: Current,
    daily: Daily,
}

#[derive(Debug, Deserialize)]
struct Current {
    time: String,
    temperature_2m: f64,
    apparent_temperature: f64,
    relative_humidity_2m: f64,
    weather_code: u8,
    wind_speed_10m: f64,
    wind_gusts_10m: f64,
    precipitation: f64,
    is_day: u8,
}

#[derive(Debug, Deserialize)]
struct Daily {
    time: Vec<String>,
    weather_code: Vec<Option<u8>>,
    temperature_2m_max: Vec<Option<f64>>,
    temperature_2m_min: Vec<Option<f64>>,
    precipitation_probability_max: Vec<Option<f64>>,
    sunrise: Vec<String>,
    sunset: Vec<String>,
}

/// The weather for `query`, as context for the skill's prompt. A place that can't be
/// found is `Ok` with a note saying so; only a failed request is `Err`.
pub async fn lookup(http: &reqwest::Client, query: &str) -> Result<String> {
    let (name, qualifier) = match query.split_once(',') {
        Some((name, rest)) => (name.trim(), Some(rest.trim()).filter(|rest| !rest.is_empty())),
        None => (query.trim(), None),
    };
    let zip = zip_code(name);
    let mut params = vec![("name", zip.unwrap_or(name)), ("count", CANDIDATES), ("language", "en"), ("format", "json")];
    // The geocoder knows postcodes worldwide, so 08003 is Barcelona unless it's told
    // to look in the US.
    if zip.is_some() {
        params.push(("countryCode", "US"));
    }
    let places: Places = http
        .get(GEOCODING_URL)
        .query(&params)
        .send()
        .await
        .context("geocoding request failed")?
        .error_for_status()
        .context("geocoding returned an error status")?
        .json()
        .await
        .context("failed to parse geocoding response")?;
    let Some(place) = choose(&places.results, qualifier) else {
        return Ok(format!("No place called \"{query}\" was found.\n\n"));
    };

    let forecast: Forecast = http
        .get(FORECAST_URL)
        .query(&[
            ("latitude", place.latitude.to_string().as_str()),
            ("longitude", place.longitude.to_string().as_str()),
            (
                "current",
                "temperature_2m,apparent_temperature,relative_humidity_2m,weather_code,wind_speed_10m,wind_gusts_10m,precipitation,is_day",
            ),
            ("daily", "weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max,sunrise,sunset"),
            ("timezone", "auto"),
            ("forecast_days", FORECAST_DAYS),
        ])
        .send()
        .await
        .context("forecast request failed")?
        .error_for_status()
        .context("forecast returned an error status")?
        .json()
        .await
        .context("failed to parse forecast response")?;

    let mut context = String::new();
    if let Some(qualifier) = qualifier
        && !place.matches(qualifier)
    {
        context.push_str(&format!("(Nothing called \"{name}\" was found in \"{qualifier}\", so this is the best match for \"{name}\".)\n"));
    }
    context.push_str(&format_forecast(place, &forecast));
    Ok(context)
}

/// The place the qualifier names the state or country of, if any. Otherwise the room
/// is assumed to be in the US, so Paris is Paris, Texas, and the geocoder's population
/// ranking picks among US places, or among all of them when none is in the US.
fn choose<'a>(places: &'a [Place], qualifier: Option<&str>) -> Option<&'a Place> {
    qualifier
        .and_then(|qualifier| places.iter().find(|place| place.matches(qualifier)))
        .or_else(|| places.iter().find(|place| place.country_code.as_deref() == Some("US")))
        .or_else(|| places.first())
}

/// The five digits of a US ZIP code, alone or as ZIP+4.
fn zip_code(text: &str) -> Option<&str> {
    let five = text.get(..5).filter(|five| five.bytes().all(|byte| byte.is_ascii_digit()))?;
    let rest = &text[5..];
    let plus_four = rest.strip_prefix('-').is_some_and(|four| four.len() == 4 && four.bytes().all(|byte| byte.is_ascii_digit()));
    (rest.is_empty() || plus_four).then_some(five)
}

fn format_forecast(place: &Place, forecast: &Forecast) -> String {
    let now = &forecast.current;
    let zone = forecast.timezone_abbreviation.as_deref().unwrap_or("local time");
    let mut out = format!(
        "Weather for {} (from Open-Meteo, https://open-meteo.com):\n\
         Now ({} {zone}, {}): {}, {}, feels like {}. Humidity {}%. Wind {}, gusting to {}.{}\n",
        place.label(),
        clock(&now.time),
        if now.is_day == 1 { "day" } else { "night" },
        describe(now.weather_code),
        temperature(now.temperature_2m),
        temperature(now.apparent_temperature),
        now.relative_humidity_2m.round(),
        wind(now.wind_speed_10m),
        wind(now.wind_gusts_10m),
        if now.precipitation > 0.0 { format!(" {} mm of precipitation in the last 15 minutes.", now.precipitation) } else { String::new() },
    );
    out.push_str("Forecast:\n");
    let daily = &forecast.daily;
    for (day, date) in daily.time.iter().enumerate() {
        let label = match day {
            0 => "Today".to_string(),
            1 => "Tomorrow".to_string(),
            _ => chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").map(|date| date.format("%A").to_string()).unwrap_or_else(|_| date.clone()),
        };
        let code = daily.weather_code.get(day).copied().flatten().map(describe).unwrap_or("unknown conditions");
        let high = daily.temperature_2m_max.get(day).copied().flatten().map(temperature).unwrap_or_else(|| "?".to_string());
        let low = daily.temperature_2m_min.get(day).copied().flatten().map(temperature).unwrap_or_else(|| "?".to_string());
        let rain = daily
            .precipitation_probability_max
            .get(day)
            .copied()
            .flatten()
            .map(|chance| format!(", {}% chance of precipitation", chance.round()))
            .unwrap_or_default();
        let sun = match (daily.sunrise.get(day), daily.sunset.get(day)) {
            (Some(rise), Some(set)) => format!(", sunrise {}, sunset {}", clock(rise), clock(set)),
            _ => String::new(),
        };
        out.push_str(&format!("- {label}: {code}, high {high}, low {low}{rain}{sun}\n"));
    }
    out.push('\n');
    out
}

/// "2026-11-13T16:00" as "4:00 PM".
fn clock(time: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(time, "%Y-%m-%dT%H:%M")
        .map(|time| time.format("%-I:%M %p").to_string())
        .unwrap_or_else(|_| time.to_string())
}

/// Both scales, so nobody in the room has to convert.
fn temperature(celsius: f64) -> String {
    // Adding zero turns a rounded -0 into 0, so -0.4° doesn't read as "-0°C".
    format!("{}°C ({}°F)", celsius.round() + 0.0, (celsius * 9.0 / 5.0 + 32.0).round() + 0.0)
}

fn wind(kmh: f64) -> String {
    format!("{} km/h ({} mph)", kmh.round(), (kmh / 1.609_344).round())
}

/// WMO weather interpretation codes, as Open-Meteo documents them.
fn describe(code: u8) -> &'static str {
    match code {
        0 => "clear sky",
        1 => "mainly clear",
        2 => "partly cloudy",
        3 => "overcast",
        45 => "fog",
        48 => "freezing fog",
        51 => "light drizzle",
        53 => "drizzle",
        55 => "heavy drizzle",
        56 => "light freezing drizzle",
        57 => "freezing drizzle",
        61 => "light rain",
        63 => "rain",
        65 => "heavy rain",
        66 => "light freezing rain",
        67 => "freezing rain",
        71 => "light snow",
        73 => "snow",
        75 => "heavy snow",
        77 => "snow grains",
        80 => "light showers",
        81 => "showers",
        82 => "violent showers",
        85 => "light snow showers",
        86 => "heavy snow showers",
        95 => "thunderstorm",
        96 => "thunderstorm with light hail",
        99 => "thunderstorm with heavy hail",
        _ => "unknown conditions",
    }
}

/// US postal abbreviations, since the geocoder only gives states' names.
const STATES: &[(&str, &str)] = &[
    ("AL", "Alabama"), ("AK", "Alaska"), ("AZ", "Arizona"), ("AR", "Arkansas"), ("CA", "California"),
    ("CO", "Colorado"), ("CT", "Connecticut"), ("DE", "Delaware"), ("DC", "District of Columbia"),
    ("FL", "Florida"), ("GA", "Georgia"), ("HI", "Hawaii"), ("ID", "Idaho"), ("IL", "Illinois"),
    ("IN", "Indiana"), ("IA", "Iowa"), ("KS", "Kansas"), ("KY", "Kentucky"), ("LA", "Louisiana"),
    ("ME", "Maine"), ("MD", "Maryland"), ("MA", "Massachusetts"), ("MI", "Michigan"), ("MN", "Minnesota"),
    ("MS", "Mississippi"), ("MO", "Missouri"), ("MT", "Montana"), ("NE", "Nebraska"), ("NV", "Nevada"),
    ("NH", "New Hampshire"), ("NJ", "New Jersey"), ("NM", "New Mexico"), ("NY", "New York"),
    ("NC", "North Carolina"), ("ND", "North Dakota"), ("OH", "Ohio"), ("OK", "Oklahoma"), ("OR", "Oregon"),
    ("PA", "Pennsylvania"), ("RI", "Rhode Island"), ("SC", "South Carolina"), ("SD", "South Dakota"),
    ("TN", "Tennessee"), ("TX", "Texas"), ("UT", "Utah"), ("VT", "Vermont"), ("VA", "Virginia"),
    ("WA", "Washington"), ("WV", "West Virginia"), ("WI", "Wisconsin"), ("WY", "Wyoming"),
];

/// The state a postal abbreviation stands for.
fn us_state(abbreviation: &str) -> Option<&'static str> {
    STATES.iter().find(|(short, _)| short.eq_ignore_ascii_case(abbreviation)).map(|(_, state)| *state)
}

/// "NJ" for New Jersey.
fn state_abbreviation(state: &str) -> Option<&'static str> {
    STATES.iter().find(|(_, name)| name.eq_ignore_ascii_case(state)).map(|(short, _)| *short)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(name: &str, admin1: &str, country: &str, code: &str) -> Place {
        Place {
            name: name.to_string(),
            latitude: 0.0,
            longitude: 0.0,
            country: Some(country.to_string()),
            country_code: Some(code.to_string()),
            admin1: Some(admin1.to_string()),
            admin2: None,
        }
    }

    #[test]
    fn a_qualifier_picks_between_places_of_the_same_name() {
        let places = [
            place("Portland", "Oregon", "United States", "US"),
            place("Portland", "Maine", "United States", "US"),
            place("Portland", "Victoria", "Australia", "AU"),
        ];
        assert_eq!(choose(&places, None).unwrap().admin1.as_deref(), Some("Oregon"));
        assert_eq!(choose(&places, Some("Me")).unwrap().admin1.as_deref(), Some("Maine"));
        assert_eq!(choose(&places, Some("ME")).unwrap().admin1.as_deref(), Some("Maine"));
        assert_eq!(choose(&places, Some("maine")).unwrap().admin1.as_deref(), Some("Maine"));
        assert_eq!(choose(&places, Some("Australia")).unwrap().admin1.as_deref(), Some("Victoria"));
        assert_eq!(choose(&places, Some("AU")).unwrap().admin1.as_deref(), Some("Victoria"));
        // An unknown qualifier still gets the likeliest place rather than nothing.
        assert_eq!(choose(&places, Some("Narnia")).unwrap().admin1.as_deref(), Some("Oregon"));
        assert!(choose(&[], Some("ME")).is_none());

        let london = [place("London", "Ontario", "Canada", "CA"), place("London", "England", "United Kingdom", "GB")];
        assert_eq!(choose(&london, Some("UK")).unwrap().country_code.as_deref(), Some("GB"));
    }

    #[test]
    fn assumes_the_us_unless_told_otherwise() {
        // The geocoder's own order for "Paris" and "08003".
        let paris = [place("Paris", "Île-de-France", "France", "FR"), place("Paris", "Texas", "United States", "US")];
        assert_eq!(choose(&paris, None).unwrap().admin1.as_deref(), Some("Texas"));
        assert_eq!(choose(&paris, Some("France")).unwrap().country_code.as_deref(), Some("FR"));
        let tokyo = [place("Tokyo", "Tokyo", "Japan", "JP")];
        assert_eq!(choose(&tokyo, None).unwrap().country_code.as_deref(), Some("JP"), "with no US place, the best one anywhere");
    }

    #[test]
    fn reads_zip_codes() {
        assert_eq!(zip_code("08003"), Some("08003"));
        assert_eq!(zip_code("08003-1234"), Some("08003"));
        for not_a_zip in ["0800", "080031", "08003-12", "Cherry Hill", "1234a", "08003 x", "é8003"] {
            assert_eq!(zip_code(not_a_zip), None, "{not_a_zip}");
        }
    }

    #[test]
    fn labels_skip_repeated_parts() {
        assert_eq!(place("Cherry Hill", "New Jersey", "United States", "US").label(), "Cherry Hill, NJ, USA");
        assert_eq!(place("Paris", "Île-de-France", "France", "FR").label(), "Paris, Île-de-France, France");
        assert_eq!(place("Singapore", "Singapore", "Singapore", "SG").label(), "Singapore");
    }

    #[test]
    fn formats_both_scales_and_a_readable_forecast() {
        let forecast: Forecast = serde_json::from_str(
            r#"{"timezone_abbreviation":"GMT-5",
                "current":{"time":"2026-11-13T16:00","temperature_2m":4.8,"apparent_temperature":2.1,"relative_humidity_2m":84,
                           "weather_code":3,"wind_speed_10m":6.3,"wind_gusts_10m":14.4,"precipitation":0.0,"is_day":1},
                "daily":{"time":["2026-11-13","2026-11-14","2026-11-15"],"weather_code":[71,3,null],
                         "temperature_2m_max":[7.8,6.2,8.9],"temperature_2m_min":[0.7,0.0,0.3],
                         "precipitation_probability_max":[97,10,null],
                         "sunrise":["2026-11-13T06:32","2026-11-14T06:33","2026-11-15T06:35"],
                         "sunset":["2026-11-13T16:23","2026-11-14T16:23","2026-11-15T16:22"]}}"#,
        )
        .unwrap();
        let text = format_forecast(&place("Boston", "Massachusetts", "United States", "US"), &forecast);
        assert!(text.contains("Weather for Boston, MA, USA"), "{text}");
        assert!(text.contains("Now (4:00 PM GMT-5, day): overcast, 5°C (41°F), feels like 2°C (36°F). Humidity 84%. Wind 6 km/h (4 mph), gusting to 14 km/h (9 mph)."), "{text}");
        assert!(text.contains("- Today: light snow, high 8°C (46°F), low 1°C (33°F), 97% chance of precipitation, sunrise 6:32 AM, sunset 4:23 PM"), "{text}");
        assert!(text.contains("- Tomorrow: overcast"), "{text}");
        assert!(text.contains("- Sunday: unknown conditions, high 9°C (48°F)"), "{text}");
        assert!(!text.contains("precipitation in the last"), "{text}");
    }

    #[test]
    fn never_says_minus_zero() {
        assert_eq!(temperature(-0.4), "0°C (31°F)");
        assert_eq!(temperature(-17.9), "-18°C (0°F)");
    }

    #[tokio::test]
    #[ignore = "calls Open-Meteo"]
    async fn looks_up_live_weather() {
        let http = reqwest::Client::new();
        let text = lookup(&http, "Portland, ME").await.unwrap();
        assert!(text.contains("Portland, ME, USA"), "{text}");
        let text = lookup(&http, "08003").await.unwrap();
        assert!(text.contains("Cherry Hill, NJ, USA"), "{text}");
        println!("{text}");
    }
}
