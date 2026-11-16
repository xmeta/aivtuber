//! Terminal verbosity switch for periodic / per-event measurement lines (#391).
//!
//! One environment variable, `AIVTUBER_LOG`: `quiet` hides measurement lines
//! only; unset, empty or any other value keeps every line (today's behaviour).
//! Audit, security, degraded and error lines never consult this switch.

use std::sync::OnceLock;

/// Environment variable read once per process.
pub const LOG_ENV: &str = "AIVTUBER_LOG";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    Verbose,
    Quiet,
}

/// Case-insensitive, trimmed; anything but `quiet` is `Verbose`.
pub fn parse(value: Option<&str>) -> Verbosity {
    match value.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("quiet") => Verbosity::Quiet,
        _ => Verbosity::Verbose,
    }
}

/// True unless `AIVTUBER_LOG=quiet`. Read once; counters must still update when
/// this is false, only the printing is skipped.
pub fn diagnostics_enabled() -> bool {
    static VERBOSITY: OnceLock<Verbosity> = OnceLock::new();
    *VERBOSITY.get_or_init(|| parse(std::env::var(LOG_ENV).ok().as_deref())) == Verbosity::Verbose
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_empty_and_unknown_are_verbose() {
        assert_eq!(parse(None), Verbosity::Verbose);
        assert_eq!(parse(Some("")), Verbosity::Verbose);
        assert_eq!(parse(Some("loud")), Verbosity::Verbose);
        assert_eq!(parse(Some("verbose")), Verbosity::Verbose);
        assert_eq!(parse(Some(" VERBOSE ")), Verbosity::Verbose);
    }

    #[test]
    fn quiet_is_case_insensitive_and_trimmed() {
        assert_eq!(parse(Some("quiet")), Verbosity::Quiet);
        assert_eq!(parse(Some("  Quiet\n")), Verbosity::Quiet);
        assert_eq!(parse(Some("QUIET")), Verbosity::Quiet);
    }
}
