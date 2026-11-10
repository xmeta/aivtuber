//! The whole `local` flow against a loopback fake server and a fake manifest:
//! fetch, verify, extract, env file, idempotence, dry-run, offline.

mod common;

use aivtuber_setup::download::DownloadConfig;
use aivtuber_setup::local::{self, LocalOptions, exe, host_platform};
use aivtuber_setup::manifest::Manifest;
use aivtuber_setup::prereq::{Report, Vulkan};
use common::*;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut cursor);
    for (name, data) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap();
    cursor.into_inner()
}

fn tar_gz_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, data) in entries {
        let mut h = tar::Header::new_gnu();
        h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        h.set_size(data.len() as u64);
        h.set_mode(0o755);
        h.set_cksum();
        builder.append(&h, *data).unwrap();
    }
    let tar_bytes = builder.into_inner().unwrap();
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(&tar_bytes).unwrap();
    enc.finish().unwrap()
}

fn tar_bz2_bytes(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, data) in entries {
        let mut h = tar::Header::new_gnu();
        h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        builder.append(&h, data.as_slice()).unwrap();
    }
    let tar_bytes = builder.into_inner().unwrap();
    let mut enc = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
    enc.write_all(&tar_bytes).unwrap();
    enc.finish().unwrap()
}

struct Fixture {
    manifest: Manifest,
    server: Server,
    root: PathBuf,
    base: PathBuf,
}

/// `evil_llama`: the llama archive carries a path-traversal entry.
fn fixture(tag: &str, evil_llama: bool) -> Fixture {
    let mut b = Behavior::default();
    let platform = host_platform().unwrap();
    let llama_name = if cfg!(windows) {
        "llama.zip"
    } else {
        "llama.tar.gz"
    };
    let server_name = exe("llama-server");
    let llama_bytes = match (cfg!(windows), evil_llama) {
        (true, false) => zip_bytes(&[(server_name.as_str(), b"MZ-fake")]),
        (true, true) => zip_bytes(&[("../escaped.txt", b"x")]),
        (false, false) => tar_gz_bytes(&[("build/llama-server", b"ELF-fake")]),
        (false, true) => tar_gz_bytes(&[("../escaped.txt", b"x")]),
    };
    let gguf = pattern(70_000, 11);
    let vad = pattern(3_000, 12);
    let avatar = pattern(5_000, 13);
    let stt: Vec<(String, Vec<u8>)> = ["enc.onnx", "dec.onnx", "join.onnx", "tokens.txt"]
        .iter()
        .enumerate()
        .map(|(i, n)| ((*n).to_owned(), pattern(2_000 + i * 100, 20 + i as u8)))
        .collect();
    let tts_files: Vec<(String, Vec<u8>)> = ["a.onnx", "b.onnx", "tts.json"]
        .iter()
        .enumerate()
        .map(|(i, n)| ((*n).to_owned(), pattern(1_500 + i * 50, 40 + i as u8)))
        .collect();
    let tts_entries: Vec<(String, Vec<u8>)> = tts_files
        .iter()
        .map(|(n, d)| (format!("voice-dir/{n}"), d.clone()))
        .collect();
    let tts_archive = tar_bz2_bytes(
        &tts_entries
            .iter()
            .map(|(n, d)| (n.as_str(), d.clone()))
            .collect::<Vec<_>>(),
    );

    b.files
        .insert(format!("/{llama_name}"), llama_bytes.clone());
    b.files.insert("/m.gguf".into(), gguf.clone());
    b.files.insert("/vad.onnx".into(), vad.clone());
    b.files.insert("/avatar.vrm".into(), avatar.clone());
    for (n, d) in &stt {
        b.files.insert(format!("/stt/{n}"), d.clone());
    }
    b.files.insert("/tts.tar.bz2".into(), tts_archive.clone());
    let server = serve(b);
    let u = |p: &str| server.url(p);

    let stt_json: Vec<_> = stt
        .iter()
        .map(|(n, d)| serde_json::json!({"file": n, "url": u(&format!("/stt/{n}")), "sha256": sha256_hex(d), "size": d.len()}))
        .collect();
    let tts_json: Vec<_> = tts_files
        .iter()
        .map(|(n, d)| serde_json::json!({"file": n, "sha256": sha256_hex(d), "size": d.len()}))
        .collect();
    let asset = |file: &str, path: &str, d: &[u8]| serde_json::json!({"file": file, "url": u(path), "sha256": sha256_hex(d), "size": d.len(), "max_extracted_bytes": 1_000_000});
    let manifest_json = serde_json::json!({
        "schema": "local-profile-pins/1",
        "llama": {"tag": "btest", "licence": "MIT", "assets": { format!("{platform}-cpu"): asset(llama_name, &format!("/{llama_name}"), &llama_bytes) }},
        "llm": asset("m.gguf", "/m.gguf", &gguf),
        "vad": asset("vad.onnx", "/vad.onnx", &vad),
        "avatar": asset("avatar.vrm", "/avatar.vrm", &avatar),
        "sherpa_lib": {"version": "x", "assets": {}},
        "models": {
            "k2v2-int8": {"files": stt_json},
            "supertonic-3-int8": {"archive": asset("tts.tar.bz2", "/tts.tar.bz2", &tts_archive), "inner_dir": "voice-dir", "files": tts_json}
        },
        "disclosures": [
            {"id": "supertonic-3-weights", "component": "Voice", "licence": "OpenRAIL-M", "applies_to": ["tts"], "duty": "Disclose that the voice is machine-generated."},
            {"id": "x", "component": "Sample avatar", "licence": "L", "applies_to": ["avatar"], "duty": "Replace it."}
        ]
    });
    let manifest = Manifest::parse(&manifest_json.to_string()).unwrap();

    // A fake extracted release.
    let base = scratch(tag);
    let root = base.join("release");
    for name in [
        "aivtuber-app",
        "speech-vad-sidecar",
        "speech-stt-worker",
        "local-tts-sidecar",
        "aivtuber-setup",
    ] {
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(exe(name)), b"bin").unwrap();
    }
    fs::create_dir_all(root.join("renderer/local-avatar/dist")).unwrap();
    fs::write(root.join("renderer/local-avatar/dist/index.html"), "<html>").unwrap();
    fs::create_dir_all(root.join("examples/starter-reaction-pack/descriptors")).unwrap();
    fs::write(
        root.join("examples/starter-reaction-pack/descriptors/a.json"),
        "{}",
    )
    .unwrap();
    Fixture {
        manifest,
        server,
        root,
        base,
    }
}

fn options(f: &Fixture) -> LocalOptions {
    LocalOptions {
        root: f.root.clone(),
        assets: f.base.join("assets"),
        env_file: f.root.join("local-profile.env"),
        flavor: Some("cpu".into()),
        // No real sidecar or ffmpeg in a unit test: the audio step is covered by its own tests.
        skip: BTreeSet::from(["audio".to_owned()]),
        dry_run: false,
        offline: false,
        force: false,
        llama_port: 18080,
        platform: host_platform().unwrap(),
        download: DownloadConfig {
            allow_loopback_http: true,
            connect_timeout: Duration::from_secs(2),
            segment_timeout: Duration::from_secs(5),
            segment_bytes: 32 * 1024,
            retries: 1,
            retry_backoff: Duration::from_millis(10),
            max_redirects: 2,
        },
    }
}

fn probe() -> Report {
    Report {
        ffplay: Some("ffplay".into()),
        ffmpeg: None,
        vulkan: Vulkan::NotFound,
    }
}

fn go(f: &Fixture, o: &LocalOptions) -> (Result<local::Summary, String>, String) {
    let mut out = Vec::new();
    let r = local::run(o, &f.manifest, &probe(), &mut out);
    (r, String::from_utf8(out).unwrap())
}

fn tree(dir: &Path) -> Vec<PathBuf> {
    let mut all = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            all.push(e.path());
            if e.path().is_dir() {
                all.extend(tree(&e.path()));
            }
        }
    }
    all
}

#[test]
fn the_happy_path_provisions_everything_writes_the_env_file_and_prints_duties() {
    let f = fixture("flow-ok", false);
    let o = options(&f);
    let (result, out) = go(&f, &o);
    let summary = result.unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert_eq!(
        summary.provisioned,
        vec!["llama", "llm", "vad", "stt", "tts", "avatar"]
    );
    // Assets, verified.
    assert!(o.assets.join("llm/m.gguf").is_file());
    assert!(o.assets.join("k2v2/tokens.txt").is_file());
    assert!(o.assets.join("supertonic/voice-dir/tts.json").is_file());
    assert!(
        f.root
            .join("renderer/local-avatar/dist/model.vrm")
            .is_file()
    );
    // The archives are deleted once extracted and verified.
    assert!(!o.assets.join("supertonic/tts.tar.bz2").exists());
    // Env file.
    let env = fs::read_to_string(&o.env_file).unwrap();
    assert!(env.contains("AIVTUBER_MODE=local"));
    assert!(
        env.contains("AIVTUBER_SETUP_LLAMA_GPU_LAYERS=0"),
        "cpu build offloads nothing"
    );
    assert!(env.contains(&format!(
        "AIVTUBER_TTS_SIDECAR_MODEL_DIR={}",
        o.assets.join("supertonic").join("voice-dir").display()
    )));
    // Duties derive from the manifest, for the provisioned steps only, and the exact start command.
    assert!(
        out.contains("Disclose that the voice is machine-generated."),
        "{out}"
    );
    assert!(out.contains("Replace it."));
    assert!(out.contains(&format!(
        "{} start",
        f.root.join(exe("aivtuber-setup")).display()
    )));
    // Warnings carry remedies.
    assert!(out.contains("ffmpeg was not found"));
    assert!(!summary.warnings.is_empty());
}

#[test]
fn a_second_run_downloads_nothing_and_keeps_the_env_file_unless_forced() {
    let f = fixture("flow-idem", false);
    let o = options(&f);
    go(&f, &o).0.unwrap();
    let requests = f.server.count();
    let env1 = fs::read_to_string(&o.env_file).unwrap();

    let (r, out) = go(&f, &o);
    r.unwrap();
    assert_eq!(
        f.server.count(),
        requests,
        "everything is already on disk and verified: {out}"
    );
    assert!(out.contains("kept"), "{out}");
    assert_eq!(
        fs::read_to_string(&o.env_file).unwrap(),
        env1,
        "an existing env file is not overwritten"
    );

    let mut forced = o.clone();
    forced.force = true;
    go(&f, &forced).0.unwrap();
    let env2 = fs::read_to_string(&o.env_file).unwrap();
    assert_ne!(env1, env2, "--force writes a new file (new secrets)");
    assert_eq!(f.server.count(), requests);
}

#[test]
fn dry_run_makes_no_request_and_writes_nothing() {
    let f = fixture("flow-dry", false);
    let mut o = options(&f);
    o.dry_run = true;
    let before = tree(&f.root);
    let (r, out) = go(&f, &o);
    r.unwrap();
    assert_eq!(f.server.count(), 0);
    assert!(!o.assets.exists());
    assert!(!o.env_file.exists());
    assert_eq!(tree(&f.root), before);
    assert!(
        out.contains("dry run") && out.contains("download  m.gguf"),
        "{out}"
    );
    assert!(
        out.contains("Disclose that the voice is machine-generated.")
            && out.contains("aivtuber-setup"),
        "{out}"
    );
}

#[test]
fn offline_uses_the_network_never_and_lists_what_is_missing() {
    let f = fixture("flow-offline", false);
    let mut o = options(&f);
    o.offline = true;
    let (r, out) = go(&f, &o);
    let err = r.unwrap_err();
    assert!(
        err.contains("--offline") && err.contains("llama.cpp"),
        "{err}"
    );
    assert_eq!(f.server.count(), 0);
    assert!(!o.env_file.exists(), "no half-configured profile: {out}");

    // After a normal run, offline succeeds on the verified files.
    let online = options(&f);
    go(&f, &online).0.unwrap();
    let requests = f.server.count();
    fs::remove_file(&o.env_file).unwrap();
    let (r, out) = go(&f, &o);
    r.unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert_eq!(f.server.count(), requests);
    assert!(o.env_file.is_file());
}

#[test]
fn a_traversal_entry_in_an_archive_fails_the_setup_and_writes_outside_nothing() {
    let f = fixture("flow-evil", true);
    let o = options(&f);
    let (r, _) = go(&f, &o);
    let err = r.unwrap_err();
    assert!(err.contains("escape"), "{err}");
    assert!(!f.base.join("escaped.txt").exists());
    assert!(!f.base.join("assets").join("escaped.txt").exists());
    assert!(!o.env_file.exists());
}

#[test]
fn a_checksum_mismatch_stops_the_setup_with_the_file_named() {
    let mut f = fixture("flow-mismatch", false);
    f.manifest.vad.sha256 = "0".repeat(64);
    let o = options(&f);
    let err = go(&f, &o).0.unwrap_err();
    assert!(
        err.contains("vad.onnx") && err.contains("sha256 mismatch"),
        "{err}"
    );
    assert!(!o.assets.join("vad/vad.onnx").exists());
    assert!(!o.env_file.exists());
}

#[test]
fn a_directory_that_is_not_a_release_and_a_spaced_asset_path_are_refused() {
    let f = fixture("flow-bad-root", false);
    let mut o = options(&f);
    o.root = f.base.join("nowhere");
    let err = go(&f, &o).0.unwrap_err();
    assert!(err.contains("extracted release archive"), "{err}");

    let mut o = options(&f);
    o.assets = f.base.join("my assets");
    let err = go(&f, &o).0.unwrap_err();
    assert!(err.contains("space") && err.contains("--assets"), "{err}");

    let mut o = options(&f);
    o.skip = BTreeSet::from(["bogus".to_owned()]);
    assert!(go(&f, &o).0.unwrap_err().contains("unknown step"));
}
