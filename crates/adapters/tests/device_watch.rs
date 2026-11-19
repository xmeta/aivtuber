//! #354: the default-output-device rule and the native backend's device
//! bookkeeping.
//!
//! Both are device-free: the watcher thread supplies the host's default device
//! name and the cpal stream callback supplies the error, so the rules and the
//! counters are exercised here on any host, with no sound card and without
//! compiling the native backend. The play path itself only takes the flag
//! (`DeviceWatch::take_rebuild`); `crates/adapters/src/audio_native.rs` starts
//! the watcher thread that sets it.

#![cfg(feature = "gapless-queue")]

use aivtuber_adapters::{DeviceDecision, DeviceWatch, device_decision};

#[test]
fn only_a_different_named_default_device_counts_as_a_change() {
    assert_eq!(
        device_decision(Some("Speakers"), Some("Speakers")),
        DeviceDecision::Keep
    );
    assert_eq!(
        device_decision(Some("Speakers"), Some("Headset")),
        DeviceDecision::Changed
    );
    // No default device at all: keep the stream that still works, because
    // ending audio the operator can hear is worse than following the default a
    // phrase later.
    assert_eq!(
        device_decision(Some("Speakers"), None),
        DeviceDecision::Keep
    );
    // A host that will not name its device: only a stream error reopens it.
    assert_eq!(device_decision(None, Some("Headset")), DeviceDecision::Keep);
    assert_eq!(device_decision(None, None), DeviceDecision::Keep);
}

#[test]
fn a_change_or_a_stream_error_asks_for_exactly_one_rebuild() {
    let watch = DeviceWatch::default();
    assert_eq!(watch.opened(), None);
    watch.record_opened(Some("Speakers".to_owned()));
    assert_eq!(watch.opened().as_deref(), Some("Speakers"));

    // The default device is still the one that was opened: nothing to do.
    assert_eq!(watch.note_default(Some("Speakers")), DeviceDecision::Keep);
    assert!(!watch.take_rebuild(), "no rebuild for the same device");

    // A switch requests one rebuild, which the play path takes once.
    assert_eq!(watch.note_default(Some("Headset")), DeviceDecision::Changed);
    assert!(watch.take_rebuild(), "the switch requests a rebuild");
    assert!(!watch.take_rebuild(), "the flag is taken once");

    // The stream callback does the same for a removed endpoint.
    watch.note_stream_error();
    assert!(watch.take_rebuild(), "a stream error requests a rebuild");
    assert!(!watch.take_rebuild());
}

#[test]
fn the_counters_report_reconnects_and_one_logged_outage() {
    let watch = DeviceWatch::default();
    assert_eq!(watch.reconnects(), 0);
    assert_eq!(watch.reopen_failures(), 0);

    assert_eq!(watch.note_reconnected(), 1);
    assert_eq!(watch.note_reconnected(), 2);
    assert_eq!(watch.reconnects(), 2);

    // The first failure of an outage is the one worth logging; the rest of the
    // outage is counted but stays quiet.
    assert!(
        watch.note_reopen_failure(),
        "the first failure of an outage logs"
    );
    assert!(
        !watch.note_reopen_failure(),
        "the rest of the outage stays quiet"
    );
    assert_eq!(watch.reopen_failures(), 2);

    // A device coming back is another reconnect, and the next outage logs again.
    assert_eq!(watch.note_reconnected(), 3);
    assert!(watch.note_reopen_failure(), "a new outage logs again");
}
