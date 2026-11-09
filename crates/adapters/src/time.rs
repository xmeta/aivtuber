//! Event-time formatting shared by the memory-sidecar adapters (#105).
//!
//! A sidecar reads event times in ISO 8601, so the project's millisecond
//! clock has to be spelled out exactly: a claim that happened at a known
//! moment must not drift by a second, or the service's chronology stops
//! matching the project's. Hindsight and Graphiti carry event time the same
//! way, and they share one conversion rather than two that could disagree.

/// Epoch milliseconds as an ISO 8601 UTC timestamp.
pub(crate) fn iso8601_utc(epoch_ms: u64) -> String {
    let total_seconds = i64::try_from(epoch_ms / 1000).unwrap_or(i64::MAX);
    let days = total_seconds.div_euclid(86_400);
    let seconds_of_day = total_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

/// The civil date of a day count since the Unix epoch.
///
/// Howard Hinnant's `civil_from_days`: the epoch shift puts the year at the
/// start of a 400-year era, so leap days land where the rule says.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_is_exact() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_000), "1970-01-01T00:00:01Z");
        assert_eq!(iso8601_utc(1_609_459_200_000), "2021-01-01T00:00:00Z");
        // A leap day, so the civil-date rule is exercised rather than assumed.
        assert_eq!(iso8601_utc(1_582_934_400_000), "2020-02-29T00:00:00Z");
        assert_eq!(iso8601_utc(1_735_689_599_000), "2024-12-31T23:59:59Z");
    }
}
