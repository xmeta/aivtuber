//! Fuzz the Performance Asset descriptor validation boundary (issue #146).
//!
//! Asset descriptors are loaded from disk and, for generated assets, from
//! provider output, so their bytes are untrusted at this boundary. Invariants:
//!
//! 1. Arbitrary bytes never panic in either deserialization or `validate()`.
//! 2. A descriptor that validates carries a usable identifier and at least one
//!    timeline event. A descriptor with an empty id or an empty timeline would
//!    be dispatched as work the runtime cannot address or replay.
//!
//! Validation is deliberately NOT relaxed to make inputs pass.

#![no_main]

use aivtuber_asset_store::PerformanceAsset;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(asset) = serde_json::from_slice::<PerformanceAsset>(data) else {
        return;
    };

    let is_valid = asset.validate().is_ok();
    if !is_valid {
        return;
    }

    assert!(
        !asset.id.trim().is_empty(),
        "validated asset has a blank id: {asset:?}"
    );
    assert!(
        !asset.timeline.is_empty(),
        "validated asset has an empty timeline: {asset:?}"
    );

    // validate() takes &self and must be idempotent, because it is called again
    // on the hot path and on every cache refresh.
    assert!(
        asset.validate().is_ok(),
        "validate() is not idempotent for {}",
        asset.id
    );
});
