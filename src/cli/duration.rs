use std::time::Duration;

/// Parses human-friendly duration strings (e.g. `30s`, `5m`, `1h`).
pub fn parse_duration(value: &str) -> Result<Duration, String> {
    humantime::parse_duration(value).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::parse_duration;

    #[test]
    fn parses_seconds_and_minutes() {
        assert_eq!(
            parse_duration("30s").expect("seconds"),
            Duration::from_secs(30)
        );
        assert_eq!(
            parse_duration("2m").expect("minutes"),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn rejects_invalid_input() {
        let err = parse_duration("not-a-duration").expect_err("invalid duration should fail");
        assert!(
            err.contains("expected number"),
            "error should mention parse failure, got: {err}"
        );
    }

    #[test]
    fn parses_hours() {
        assert_eq!(
            parse_duration("1h").expect("hours"),
            Duration::from_secs(3600)
        );
        assert_eq!(
            parse_duration("2h30m").expect("hours and minutes"),
            Duration::from_secs(2 * 3600 + 30 * 60)
        );
    }

    #[test]
    fn parses_days() {
        assert_eq!(
            parse_duration("1d").expect("day"),
            Duration::from_secs(86400)
        );
    }

    #[test]
    fn parses_milliseconds() {
        assert_eq!(
            parse_duration("500ms").expect("milliseconds"),
            Duration::from_millis(500)
        );
    }
}
