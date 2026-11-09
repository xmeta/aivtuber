//! Issue #220 S2b, decision Q7: the amplitude mouth track the TTS sidecar
//! writes is a valid viseme track for the store the app resolves `viseme_ref`
//! with, and it is turned into avatar commands by the same playback path as any
//! cached track. No model is involved: the audio is synthetic.

use aivtuber_adapters::speech_tts::envelope::amplitude_track_json;
use aivtuber_domain::Viseme;
use aivtuber_runtime::{LocalVisemeStore, VisemeResolver};

fn speechlike(rate: u32) -> Vec<f32> {
    // 3 syllable-like bursts separated by short pauses.
    let mut samples = Vec::new();
    for _ in 0..3 {
        samples.extend((0..(rate as usize * 25 / 100)).map(|i| 0.35 * (i as f32 * 0.07).sin()));
        samples.extend(std::iter::repeat_n(0.0_f32, rate as usize * 12 / 100));
    }
    samples
}

#[test]
fn the_amplitude_track_resolves_through_the_apps_viseme_store() {
    let rate = 44_100;
    let samples = speechlike(rate);
    let duration_ms = samples.len() as u64 * 1000 / u64::from(rate);
    let json = amplitude_track_json(&samples, rate, duration_ms);

    let root = std::env::temp_dir().join(format!("aivtuber-amplitude-{}", std::process::id()));
    let dir = root.join("viseme").join("generated");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("t.json"), &json).unwrap();

    let track = LocalVisemeStore::new(&root)
        .resolve("viseme/generated/t.json")
        .expect("the store accepts the track the sidecar writes");
    assert_eq!(track.duration_ms, duration_ms);
    assert!(
        track
            .cues
            .iter()
            .any(|c| c.viseme == Viseme::Aa && c.weight > 0.5)
    );
    assert_eq!(track.cues.last().unwrap().viseme, Viseme::Neutral);
    // Amplitude only: no phonetic claim beyond open or closed.
    assert!(
        track
            .cues
            .iter()
            .all(|c| matches!(c.viseme, Viseme::Aa | Viseme::Neutral))
    );
    // Bounded: a few cues per second, never one per frame.
    assert!(track.cues.len() as u64 <= duration_ms / 80 + 2);
    let _ = std::fs::remove_dir_all(&root);
}
