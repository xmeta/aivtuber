//! Fuzz the `EventEnvelope` domain deserialization + validation boundary
//! (issue #146).
//!
//! This is the trust boundary every external event crosses: raw bytes from a
//! provider, a socket, or stdin become an `EventEnvelope` here, and the runtime
//! makes authority decisions from its fields. Two invariants are checked:
//!
//! 1. Arbitrary bytes never panic. Deserialization and validation must reject
//!    malformed input, not unwind.
//! 2. An envelope that `validate()` accepts is actually self-consistent. A
//!    validation bug that *accepts* forged control-plane authority is worse than
//!    a crash, because the rest of the runtime trusts `validate()`.
//!
//! Validation is deliberately NOT relaxed to make inputs pass. Malformed input
//! is expected to be rejected; the target asserts the rejection is clean.

#![no_main]

use aivtuber_domain::{EventEnvelope, SecurityPlane};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // serde_json rejects arbitrary byte strings at the JSON layer. Only spend
    // budget on inputs that are at least valid JSON.
    let Ok(envelope) = serde_json::from_slice::<EventEnvelope>(data) else {
        return;
    };

    if envelope.validate().is_ok() {
        // An accepted envelope must not carry forged authorization onto a
        // non-control plane, and must not be an operator command without the
        // matching control-plane authority. Both are authority boundaries that
        // `SecurityRuntime` relies on downstream.
        assert!(
            !(envelope.plane != SecurityPlane::Control && envelope.authorization.is_some()),
            "accepted a content-plane envelope carrying authorization: {envelope:?}"
        );
        assert!(
            !(envelope.kind == aivtuber_domain::EventKind::OperatorCommand
                && envelope.plane != SecurityPlane::Control),
            "accepted an operator command outside the control plane: {envelope:?}"
        );

        // Re-validation must be idempotent: validation is called on the hot
        // path and must not mutate what a second call would judge.
        assert_eq!(
            envelope.validate().is_ok(),
            true,
            "validate() is not idempotent for {envelope:?}"
        );
    }
});