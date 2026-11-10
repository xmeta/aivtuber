//! Starter reaction audio (issue #289): speaks every descriptor's
//! `speech.text` with the bundled `local-tts-sidecar` and encodes the result to
//! the Opus file the descriptor references, then writes `PROVENANCE.json`.
//!
//! This is the Rust port of `scripts/synthesize-starter-audio.mjs`, with the
//! same determinism rules: the sidecar always uses lang=ja and a seed derived
//! from the text alone, the encode is bit-exact for a given ffmpeg build, and
//! the provenance carries no timestamp. The core ([`materialize`]) takes the
//! synthesizer and the encoder as closures so tests need no model and no ffmpeg.

use crate::download::sha256_file;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const OPENRAIL_NOTICE: &str = "Supertonic 3 weights are OpenRAIL-M: use restrictions apply, including the duty to disclose that published audio is machine-generated (docs/supply-chain.adoc section 10).";
pub const SEED_RULE: &str = "lang=ja; seed = little-endian u32 of sha256(utf8 text)[0..4] & 0x7fffffff (speech_tts::text_seed); no random seed";

pub fn text_seed(text: &str) -> u32 {
    let digest = Sha256::digest(text.as_bytes());
    u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]) & 0x7fff_ffff
}

/// `audio://starter/<name>.opus` only: one flat directory, no traversal.
pub fn starter_file_name(audio_ref: &str) -> Result<String, String> {
    let bad =
        || format!("unsupported audio_ref {audio_ref:?} (expected audio://starter/<name>.opus)");
    let name = audio_ref.strip_prefix("audio://starter/").ok_or_else(bad)?;
    let stem = name.strip_suffix(".opus").ok_or_else(bad)?;
    let mut chars = stem.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    if !first_ok
        || !stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(bad());
    }
    Ok(name.to_owned())
}

#[derive(Debug, Clone)]
pub struct Target {
    pub id: String,
    pub text: String,
    pub audio_ref: String,
    pub file_name: String,
}

pub fn read_targets(pack_root: &Path) -> Result<(Vec<Target>, Option<String>), String> {
    let dir = pack_root.join("descriptors");
    let mut names: Vec<_> = fs::read_dir(&dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json"))
        .collect();
    names.sort();
    let mut targets = Vec::new();
    let mut voices = std::collections::BTreeSet::new();
    for name in names {
        let text = fs::read_to_string(dir.join(&name)).map_err(|e| format!("{name}: {e}"))?;
        let d: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{name}: {e}"))?;
        if let Some(v) = d["compatibility"]["voice_model"].as_str() {
            voices.insert(v.to_owned());
        }
        let speech = &d["speech"];
        let Some(audio_ref) = speech["audio_ref"].as_str() else {
            continue; // speechless descriptor (reaction.neutral)
        };
        let Some(speech_text) = speech["text"].as_str().filter(|t| !t.is_empty()) else {
            return Err(format!("{name}: audio_ref without speech.text"));
        };
        targets.push(Target {
            id: d["id"].as_str().unwrap_or_default().to_owned(),
            text: speech_text.to_owned(),
            audio_ref: audio_ref.to_owned(),
            file_name: starter_file_name(audio_ref)?,
        });
    }
    if voices.len() > 1 {
        return Err(format!(
            "descriptors expect more than one voice_model: {}",
            voices.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    Ok((targets, voices.into_iter().next()))
}

#[derive(Debug, Clone, Serialize)]
pub struct Engine {
    pub backend: String,
    pub model_alias: String,
    pub model_version: String,
    pub speaker_id: u32,
    pub steps: u32,
    /// Always 1 (the sidecar default); an integer so the JSON matches the Node script byte for byte.
    pub speed: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileRecord {
    pub descriptor_id: String,
    pub audio_ref: String,
    pub text: String,
    pub seed: u32,
    pub wav_sha256: String,
    pub opus_sha256: String,
    pub synthesized_duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Provenance {
    pub schema: &'static str,
    pub issue: u32,
    pub generated_by: &'static str,
    pub committed_to_git: bool,
    pub voice_model_identity: Option<String>,
    pub voice_model_identity_note: &'static str,
    pub engine: Engine,
    pub seed_rule: &'static str,
    pub encoding: &'static str,
    pub licence_notice: &'static str,
    pub files: Vec<FileRecord>,
}

pub struct Synthesized {
    pub wav_path: PathBuf,
    pub duration_ms: Option<u64>,
}

pub fn materialize(
    pack_root: &Path,
    audio_root: &Path,
    synthesize: &mut dyn FnMut(&str) -> Result<Synthesized, String>,
    encode: &mut dyn FnMut(&Path, &Path) -> Result<(), String>,
    engine: Engine,
) -> Result<Provenance, String> {
    let (targets, voice_model) = read_targets(pack_root)?;
    if targets.is_empty() {
        return Err("no descriptor names speech audio".to_owned());
    }
    let out_dir = audio_root.join("starter");
    fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let mut files = Vec::new();
    for t in targets {
        let synthesized = synthesize(&t.text)?;
        let opus_path = out_dir.join(&t.file_name);
        encode(&synthesized.wav_path, &opus_path)?;
        files.push(FileRecord {
            descriptor_id: t.id,
            audio_ref: t.audio_ref,
            seed: text_seed(&t.text),
            text: t.text,
            wav_sha256: sha256_file(&synthesized.wav_path).map_err(|e| e.to_string())?,
            opus_sha256: sha256_file(&opus_path).map_err(|e| e.to_string())?,
            synthesized_duration_ms: synthesized.duration_ms,
        });
    }
    let provenance = Provenance {
        schema: "starter-audio-provenance/1",
        issue: 289,
        generated_by: "aivtuber-setup local (crates/setup)",
        committed_to_git: false,
        voice_model_identity: voice_model,
        voice_model_identity_note: "the descriptors' compatibility.voice_model; leave AIVTUBER_VOICE_MODEL unset or set to this exact value",
        engine,
        seed_rule: SEED_RULE,
        encoding: "opus (libopus, 32 kbit/s, mono, bit-exact muxer, no metadata)",
        licence_notice: OPENRAIL_NOTICE,
        files,
    };
    let json = serde_json::to_string_pretty(&provenance).map_err(|e| e.to_string())?;
    fs::write(out_dir.join("PROVENANCE.json"), format!("{json}\n")).map_err(|e| e.to_string())?;
    Ok(provenance)
}

/// True when every referenced starter file exists (the step can be skipped).
pub fn starter_audio_present(pack_root: &Path, audio_root: &Path) -> bool {
    let Ok((targets, _)) = read_targets(pack_root) else {
        return false;
    };
    let dir = audio_root.join("starter");
    !targets.is_empty()
        && dir.join("PROVENANCE.json").is_file()
        && targets.iter().all(|t| dir.join(&t.file_name).is_file())
}

pub fn ffmpeg_has_libopus(ffmpeg: &Path) -> Result<(), String> {
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .map_err(|e| format!("could not run {}: {e}", ffmpeg.display()))?;
    if String::from_utf8_lossy(&out.stdout).contains("libopus") {
        Ok(())
    } else {
        Err(format!(
            "{} has no libopus encoder; install a full FFmpeg build (Windows: `winget install Gyan.FFmpeg`)",
            ffmpeg.display()
        ))
    }
}

pub fn ffmpeg_encoder(ffmpeg: PathBuf) -> impl FnMut(&Path, &Path) -> Result<(), String> {
    move |wav: &Path, opus: &Path| {
        let out = Command::new(&ffmpeg)
            .args(["-y", "-v", "error", "-i"])
            .arg(wav)
            .args([
                "-map_metadata",
                "-1",
                "-ac",
                "1",
                "-c:a",
                "libopus",
                "-b:a",
                "32k",
                "-application",
                "voip",
                "-fflags",
                "+bitexact",
                "-flags:a",
                "+bitexact",
            ])
            .arg(opus)
            .output()
            .map_err(|e| format!("ffmpeg could not start: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "ffmpeg failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

struct Sidecar {
    child: Child,
    listen: String,
    model_id: String,
    model_version: String,
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        drop(self.child.stdin.take()); // the sidecar exits at stdin EOF
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_sidecar(
    sidecar: &Path,
    model_dir: &Path,
    audio_root: &Path,
    speaker_id: u32,
) -> Result<Sidecar, String> {
    let mut child = Command::new(sidecar)
        .arg("--model-dir")
        .arg(model_dir)
        .arg("--audio-root")
        .arg(audio_root)
        .args([
            "--listen",
            "127.0.0.1:0",
            "--speaker-id",
            &speaker_id.to_string(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", sidecar.display()))?;
    let stdout = child.stdout.take().ok_or("sidecar has no stdout")?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                let tts = &value["tts"];
                if tts["state"] == "ready" {
                    let field = |k: &str| tts[k].as_str().unwrap_or_default().to_owned();
                    return Ok(Sidecar {
                        listen: field("listen"),
                        model_id: field("model_id"),
                        model_version: field("model_version"),
                        child,
                    });
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                return Err("local-tts-sidecar did not become ready within 120 s".to_owned());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let status = child.wait().map(|s| s.to_string()).unwrap_or_default();
                return Err(format!(
                    "local-tts-sidecar exited before it was ready ({status}); it refuses a model directory that does not match the pinned digests, or engine libraries that are not beside it"
                ));
            }
        }
    }
}

fn synthesize_once(listen: &str, audio_root: &Path, text: &str) -> Result<Synthesized, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None) // loopback stays direct whatever HTTP_PROXY says
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into();
    let payload = serde_json::json!({ "text": text }).to_string();
    let mut response = agent
        .post(format!("http://{listen}/v1/synthesize"))
        .header("content-type", "application/json")
        .send(payload.as_bytes())
        .map_err(|e| format!("sidecar request failed: {e}"))?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("sidecar response: {e}"))?;
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let audio_ref = value["audio_ref"].as_str().unwrap_or_default();
    let name = audio_ref
        .strip_prefix("audio://generated/")
        .filter(|n| {
            n.ends_with(".wav")
                && !n.is_empty()
                && n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
        .ok_or_else(|| format!("unexpected audio_ref {audio_ref:?}"))?;
    Ok(Synthesized {
        wav_path: audio_root.join("generated").join(name),
        duration_ms: value["duration_ms"].as_u64(),
    })
}

/// The real path: bundled sidecar plus ffmpeg.
pub fn synthesize_starter_audio(
    sidecar: &Path,
    model_dir: &Path,
    pack_root: &Path,
    audio_root: &Path,
    speaker_id: u32,
    ffmpeg: &Path,
) -> Result<Provenance, String> {
    ffmpeg_has_libopus(ffmpeg)?;
    fs::create_dir_all(audio_root).map_err(|e| e.to_string())?;
    let sc = start_sidecar(sidecar, model_dir, audio_root, speaker_id)?;
    let engine = Engine {
        backend: "local-tts-sidecar".to_owned(),
        model_alias: sc.model_id.clone(),
        model_version: sc.model_version.clone(),
        speaker_id,
        steps: 5,
        speed: 1,
    };
    let listen = sc.listen.clone();
    let root = audio_root.to_path_buf();
    let mut synth = move |text: &str| synthesize_once(&listen, &root, text);
    let mut encode = ffmpeg_encoder(ffmpeg.to_path_buf());
    let result = materialize(pack_root, audio_root, &mut synth, &mut encode, engine);
    drop(sc);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/starter-reaction-pack")
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aivtuber-audio-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_seed_rule_equals_the_runtime_one() {
        for text in ["えーっと…", "なるほど", "hello", ""] {
            assert_eq!(
                text_seed(text) as i32,
                aivtuber_adapters::speech_tts::text_seed(text),
                "{text}"
            );
        }
    }

    #[test]
    fn audio_refs_are_flat_starter_opus_names() {
        assert_eq!(
            starter_file_name("audio://starter/a-1.opus").unwrap(),
            "a-1.opus"
        );
        for bad in [
            "audio://starter/../x.opus",
            "audio://starter/a/b.opus",
            "audio://generated/a.opus",
            "audio://starter/.hidden.opus",
            "audio://starter/a.wav",
            "https://x/a.opus",
        ] {
            assert!(starter_file_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_real_pack_yields_six_speech_targets_and_one_voice() {
        let (targets, voice) = read_targets(&pack()).unwrap();
        assert_eq!(targets.len(), 6);
        assert_eq!(voice.as_deref(), Some("example-voice-v1"));
    }

    #[test]
    fn materialize_writes_files_and_a_timestamp_free_provenance() {
        let audio = scratch("mat");
        let wavs = scratch("mat-wav");
        let mut n = 0u64;
        let mut synth = |text: &str| {
            n += 1;
            let p = wavs.join(format!("{n}.wav"));
            fs::write(&p, format!("wav:{text}")).unwrap();
            Ok(Synthesized {
                wav_path: p,
                duration_ms: Some(100 + n),
            })
        };
        let mut encode =
            |wav: &Path, opus: &Path| fs::copy(wav, opus).map(|_| ()).map_err(|e| e.to_string());
        let engine = Engine {
            backend: "local-tts-sidecar".into(),
            model_alias: "supertonic-3-int8".into(),
            model_version: "test".into(),
            speaker_id: 0,
            steps: 5,
            speed: 1,
        };
        let p = materialize(&pack(), &audio, &mut synth, &mut encode, engine.clone()).unwrap();
        assert_eq!(p.files.len(), 6);
        assert!(starter_audio_present(&pack(), &audio));
        let text = fs::read_to_string(audio.join("starter/PROVENANCE.json")).unwrap();
        assert!(text.contains("\"schema\": \"starter-audio-provenance/1\""));
        assert!(
            text.contains("\"speed\": 1\n"),
            "integer speed, as the Node script writes it"
        );
        assert!(text.contains("OpenRAIL-M"));
        assert!(!text.contains("timestamp"));
        // A rerun reproduces the provenance byte for byte.
        n = 0;
        let mut synth2 = |text: &str| {
            n += 1;
            let p = wavs.join(format!("{n}.wav"));
            fs::write(&p, format!("wav:{text}")).unwrap();
            Ok(Synthesized {
                wav_path: p,
                duration_ms: Some(100 + n),
            })
        };
        materialize(&pack(), &audio, &mut synth2, &mut encode, engine).unwrap();
        assert_eq!(
            text,
            fs::read_to_string(audio.join("starter/PROVENANCE.json")).unwrap()
        );
    }

    #[test]
    fn missing_starter_audio_is_reported_absent() {
        let audio = scratch("absent");
        assert!(!starter_audio_present(&pack(), &audio));
    }
}
