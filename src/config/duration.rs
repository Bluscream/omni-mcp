//! Human-friendly duration strings for the config file (`"30s"`, `"1500ms"`, `"2m"`).

use std::time::Duration;

use serde::{Deserialize, Deserializer};

use crate::error::ConfigError;

/// Wrapper so `Duration` can be written as `"30s"` in TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanDuration(pub Duration);

impl HumanDuration {
    pub const fn secs(n: u64) -> Self {
        Self(Duration::from_secs(n))
    }

    pub const fn millis(n: u64) -> Self {
        Self(Duration::from_millis(n))
    }

    pub const fn get(self) -> Duration {
        self.0
    }
}

impl From<HumanDuration> for Duration {
    fn from(value: HumanDuration) -> Self {
        value.0
    }
}

/// Parses `<integer><unit>` where unit is one of `ms`, `s`, `m`, `h`.
/// A bare integer is interpreted as seconds.
pub fn parse(text: &str) -> Result<Duration, ConfigError> {
    let trimmed = text.trim();
    let invalid = || ConfigError::Duration { value: text.to_string() };

    let split = trimmed.find(|c: char| !c.is_ascii_digit()).unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let value: u64 = number.parse().map_err(|_| invalid())?;

    let millis = match unit.trim() {
        "ms" => value,
        "s" | "" => value.checked_mul(1_000).ok_or_else(invalid)?,
        "m" => value.checked_mul(60_000).ok_or_else(invalid)?,
        "h" => value.checked_mul(3_600_000).ok_or_else(invalid)?,
        _ => return Err(invalid()),
    };
    Ok(Duration::from_millis(millis))
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse(&raw).map(Self).map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for HumanDuration {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let millis = self.0.as_millis();
        if millis % 1000 == 0 {
            serializer.serialize_str(&format!("{}s", millis / 1000))
        } else {
            serializer.serialize_str(&format!("{millis}ms"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_supported_unit() {
        assert_eq!(parse("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn bare_integer_means_seconds() {
        assert_eq!(parse("45").unwrap(), Duration::from_secs(45));
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        assert_eq!(parse("  10s  ").unwrap(), Duration::from_secs(10));
    }

    #[test]
    fn rejects_garbage_rather_than_guessing() {
        for bad in ["", "s", "abc", "10years", "-5s", "1.5s"] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn overflow_is_rejected_not_wrapped() {
        assert!(parse("99999999999999999999h").is_err());
        assert!(parse(&format!("{}h", u64::MAX)).is_err());
    }

    #[test]
    fn round_trips_through_serde() {
        let decoded: HumanDuration = serde_json::from_str("\"90s\"").unwrap();
        assert_eq!(decoded.get(), Duration::from_secs(90));
        assert_eq!(serde_json::to_string(&decoded).unwrap(), "\"90s\"");
        assert_eq!(serde_json::to_string(&HumanDuration::millis(250)).unwrap(), "\"250ms\"");
    }
}
