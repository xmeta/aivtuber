//! Fuzz the Performance Asset descriptor validation boundary (issue #146).
//!
//! Asset descriptors are loaded from disk and, for generated assets, from
//! provider output, so their bytes are untrusted at this boundary. Invariants:
//!
//! 1. Arbitrary bytes never panic in either deserialization or `validate()`.
//! 2. A descriptor that validates carries a usable identifier.
//! 3. `validate()` is idempotent.
//!
//! The oracle deliberately asserts **no more than the production contract
//! states**. A fuzz target that rejects inputs the runtime accepts is worse than
//! no target: every such input is reported as a crash, so the suite trains the
//! reader to ignore it.
//!
//! An earlier version of this file also asserted a non-empty `timeline`. That
//! was wrong, and this comment records why so it is not reintroduced.
//! `schemas/performance-asset.schema.json` gives `timeline` no `minItems`, and
//! `PerformanceAsset::validate()` only inspects each entry (it validates
//! `event` and checks `at_ms` is monotonic); neither requires the array to be
//! non-empty. A descriptor with `"timeline": []` therefore passes both the
//! schema and `validate()`, and asserting against it made this target fail on a
//! correct input. Requiring a non-empty timeline is a real contract change that
//! belongs in schema + Rust validation + fixtures together, not in a fuzz
//! oracle. See docs/fuzzing.adoc.

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

    // Backed by the contract: validate() rejects a blank id via
    // `is_valid_asset_id`, so a validating asset must carry a usable one.
    assert!(
        !asset.id.trim().is_empty(),
        "validated asset has a blank id: {asset:?}"
    );

    // validate() takes &self and must be idempotent, because it is called again
    // on the hot path and on every cache refresh.
    assert!(
        asset.validate().is_ok(),
        "validate() is not idempotent for {}",
        asset.id
    );
});
