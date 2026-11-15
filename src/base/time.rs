//! Wall-clock helpers. All persisted timestamps are milliseconds since the Unix epoch (`i64`).

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time in milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        // The clock is set before 1970; report the (negative) offset rather than panicking.
        Err(err) => -i64::try_from(err.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// Formats a millisecond timestamp as RFC 3339 (UTC), e.g. `2026-11-15T08:30:00.000Z`.
pub fn format_ms(timestamp_ms: i64) -> String {
    match chrono::DateTime::<chrono::Utc>::from_timestamp_millis(timestamp_ms) {
        Some(datetime) => datetime.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        None => timestamp_ms.to_string(),
    }
}

/// Parses an RFC 3339 / ISO 8601 timestamp (as found in Jira, GitHub and wiki exports) into
/// milliseconds since the Unix epoch. Accepts `+0000` style offsets without a colon.
pub fn parse_timestamp_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(datetime) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(datetime.timestamp_millis());
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%.f%z"] {
        if let Ok(datetime) = chrono::DateTime::parse_from_str(text, format) {
            return Some(datetime.timestamp_millis());
        }
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return Some(naive.and_utc().timestamp_millis());
        }
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|naive| naive.and_utc().timestamp_millis());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_after_2024() {
        assert!(now_ms() > 1_704_067_200_000);
    }

    #[test]
    fn parses_common_export_formats() {
        let expected = 1_718_444_096_000;
        assert_eq!(parse_timestamp_ms("2024-06-15T09:34:56Z"), Some(expected));
        assert_eq!(
            parse_timestamp_ms("2024-06-15T09:34:56.000+0000"),
            Some(expected)
        );
        assert_eq!(
            parse_timestamp_ms("2024-06-15T11:34:56+02:00"),
            Some(expected)
        );
        assert_eq!(parse_timestamp_ms("2024-06-15 09:34:56"), Some(expected));
        assert_eq!(parse_timestamp_ms("2024-06-15"), Some(1_718_409_600_000));
        assert_eq!(parse_timestamp_ms("not a date"), None);
        assert_eq!(parse_timestamp_ms(""), None);
    }

    #[test]
    fn formats_round_trip() {
        let formatted = format_ms(1_718_444_096_000);
        assert_eq!(formatted, "2024-06-15T09:34:56.000Z");
        assert_eq!(parse_timestamp_ms(&formatted), Some(1_718_444_096_000));
    }
}
