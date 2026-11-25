use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;

pub(crate) fn required_env(name: &str) -> Result<String, Box<dyn Error>> {
    env_optional(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is required when generative fallback is enabled"),
        )
        .into()
    })
}

pub(crate) fn env_optional(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

pub(crate) fn env_string(name: &str, default: &str) -> String {
    env_optional(name).unwrap_or_else(|| default.to_owned())
}

pub(crate) fn env_path(name: &str) -> Option<PathBuf> {
    env_optional(name).map(PathBuf::from)
}

/// Expression presets the pack's descriptors use: the vocabulary the affect
/// classifier may pick from. Unreadable descriptors contribute nothing, so a
/// broken pack degrades to the neutral baseline.
pub(crate) fn pack_expression_presets(descriptors: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(descriptors) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
        .iter()
        .take(4096)
        .filter_map(|path| aivtuber_asset_store::load_asset_file(path).ok())
        .filter_map(|asset| asset.expression.map(|expression| expression.preset))
        .collect()
}

pub(crate) fn env_bool(name: &str, default: bool) -> Result<bool, Box<dyn Error>> {
    let Some(value) = env_optional(name) else {
        return Ok(default);
    };
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be true/false, 1/0, yes/no, or on/off"),
        )
        .into()),
    }
}

pub(crate) fn env_u64(name: &str, default: u64) -> Result<u64, Box<dyn Error>> {
    let Some(value) = env_optional(name) else {
        return Ok(default);
    };
    value.parse::<u64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be an unsigned integer: {error}"),
        )
        .into()
    })
}

/// Checked `u32` environment parsing. A narrowing `as u32` would silently wrap
/// (e.g. `4294967296` becoming `0`), which contradicts the promise that invalid
/// shadow configuration is rejected rather than clamped.
pub(crate) fn env_u32(name: &str, default: u32) -> Result<u32, Box<dyn Error>> {
    let value = env_u64(name, u64::from(default))?;
    narrow_u32(name, value)
}

pub(crate) fn narrow_u32(name: &str, value: u64) -> Result<u32, Box<dyn Error>> {
    u32::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must fit in 32 bits: {value}"),
        )
        .into()
    })
}

pub(crate) fn env_usize(name: &str, default: usize) -> Result<usize, Box<dyn Error>> {
    let value = env_u64(name, default as u64)?;
    usize::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is too large for this platform"),
        )
        .into()
    })
}

pub(crate) fn env_f64(name: &str, default: f64) -> Result<f64, Box<dyn Error>> {
    let Some(value) = env_optional(name) else {
        return Ok(default);
    };
    value.parse::<f64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be a number: {error}"),
        )
        .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #164 review: shadow bounds must be rejected, never silently wrapped.
    /// `4294967296 as u32` is `0`, which would then pass `validate()`.
    #[test]
    fn shadow_bounds_reject_narrowing_overflow_instead_of_wrapping() {
        assert_eq!(
            narrow_u32("AIVTUBER_SHADOW_SAMPLE_RATE", 10_000).expect("in range"),
            10_000
        );
        assert_eq!(
            narrow_u32("AIVTUBER_SHADOW_SAMPLE_RATE", u64::from(u32::MAX)).expect("boundary"),
            u32::MAX
        );
        for overflowing in [u64::from(u32::MAX) + 1, 4_294_967_296, u64::MAX] {
            let error = narrow_u32("AIVTUBER_SHADOW_SAMPLE_RATE", overflowing)
                .expect_err("must reject, never wrap");
            assert!(
                error.to_string().contains("must fit in 32 bits"),
                "unexpected error: {error}"
            );
        }
        // The value that used to wrap to 0 would otherwise have been read as
        // "sample nothing" rather than rejected as an invalid configuration.
        assert_ne!(
            narrow_u32("AIVTUBER_SHADOW_SAMPLE_RATE", 4_294_967_296).ok(),
            Some(0)
        );

        assert!(
            usize::try_from(u64::MAX).is_err() || usize::BITS == 64,
            "the usize conversion must stay platform-safe"
        );
    }
}
