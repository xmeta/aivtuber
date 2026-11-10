//! `aivtuber-setup local`: from an extracted release archive to a runnable
//! local profile. Every step is idempotent (a file that already matches its pin
//! is kept), so an interrupted or partly failed run is fixed by running it
//! again.
//!
//! Modes
//! * default: fetch what is missing, verify, extract, synthesize the starter
//!   audio, write the env file, print the duties and the start command;
//! * `--dry-run`: print the plan; no network, no file written;
//! * `--offline`: no network; verifies what is on disk, synthesizes audio and
//!   writes the env file; fails and lists what is missing.

use crate::audio;
use crate::config::{self, Paths, Secrets, WriteOutcome};
use crate::download::{self, DownloadConfig, Outcome, matches_pin};
use crate::extract::{self, Limits};
use crate::manifest::{Asset, Manifest};
use crate::prereq::{self, Vulkan};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Step ids accepted by `--skip`.
pub const STEPS: [&str; 7] = ["llama", "llm", "vad", "stt", "tts", "avatar", "audio"];

const MIB: f64 = 1024.0 * 1024.0;

#[derive(Debug, Clone)]
pub struct LocalOptions {
    /// The extracted release directory: executables, renderer page, starter pack.
    pub root: PathBuf,
    /// Where downloaded models and the llama.cpp build go.
    pub assets: PathBuf,
    pub env_file: PathBuf,
    /// `vulkan` or `cpu`; detected when absent.
    pub flavor: Option<String>,
    pub skip: BTreeSet<String>,
    pub dry_run: bool,
    pub offline: bool,
    pub force: bool,
    pub llama_port: u16,
    /// `win32-x64` or `linux-x64`.
    pub platform: String,
    pub download: DownloadConfig,
}

pub fn host_platform() -> Result<String, String> {
    if !cfg!(target_arch = "x86_64") {
        return Err("only x86_64 Windows and Linux builds are pinned".to_owned());
    }
    if cfg!(windows) {
        Ok("win32-x64".to_owned())
    } else if cfg!(target_os = "linux") {
        Ok("linux-x64".to_owned())
    } else {
        Err("only Windows and Linux are pinned (no macOS build)".to_owned())
    }
}

pub fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

#[derive(Debug, Default)]
pub struct Summary {
    pub provisioned: Vec<&'static str>,
    pub warnings: Vec<String>,
    pub env: Option<WriteOutcome>,
}

struct Ctx<'a, W: Write> {
    opts: &'a LocalOptions,
    out: &'a mut W,
    missing: Vec<String>,
}

impl<W: Write> Ctx<'_, W> {
    fn say(&mut self, line: impl AsRef<str>) {
        let _ = writeln!(self.out, "{}", line.as_ref());
    }

    /// One pinned file at `dest`. Returns true when it is (now) on disk and verified.
    fn ensure_file(
        &mut self,
        label: &str,
        asset_url: &str,
        dest: &Path,
        sha256: &str,
        size: u64,
    ) -> Result<bool, String> {
        let mb = size as f64 / MIB;
        if self.opts.dry_run {
            let present = fs::metadata(dest).is_ok_and(|m| m.len() == size);
            if present {
                self.say(format!(
                    "  have      {label} ({mb:.1} MiB; digest is checked on a real run)"
                ));
            } else {
                self.say(format!(
                    "  download  {label} ({mb:.1} MiB) from {}",
                    host_of(asset_url)
                ));
            }
            return Ok(present);
        }
        if matches_pin(dest, sha256, size) {
            self.say(format!("  have      {label}"));
            return Ok(true);
        }
        if self.opts.offline {
            self.say(format!(
                "  MISSING   {label} (--offline: nothing is fetched)"
            ));
            self.missing.push(label.to_owned());
            return Ok(false);
        }
        self.say(format!(
            "  download  {label} ({mb:.1} MiB) from {}",
            host_of(asset_url)
        ));
        let mut last = 0u64;
        let name = label.to_owned();
        // Progress goes to stderr so stdout stays the plan and the summary.
        let mut progress = |done: u64, total: u64| {
            let pct = done * 100 / total.max(1);
            if pct >= last + 10 || done == total {
                last = pct;
                eprintln!(
                    "            {name}: {pct}% ({:.1}/{:.1} MiB)",
                    done as f64 / MIB,
                    total as f64 / MIB
                );
            }
        };
        match download::fetch_verified(
            &self.opts.download,
            asset_url,
            dest,
            sha256,
            size,
            &mut progress,
        ) {
            Ok(Outcome::AlreadyPresent) => self.say(format!("  have      {label}")),
            Ok(Outcome::Downloaded { resumed_from }) if resumed_from > 0 => {
                self.say(format!(
                    "  ok        {label} (resumed from {:.1} MiB, sha256 verified)",
                    resumed_from as f64 / MIB
                ));
            }
            Ok(Outcome::Downloaded { .. }) => {
                self.say(format!("  ok        {label} (sha256 verified)"))
            }
            Err(e) => return Err(format!("{label}: {e}")),
        }
        Ok(true)
    }
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned())
}

fn find_llama_server(bin: &Path) -> Option<PathBuf> {
    let name = exe("llama-server");
    let direct = bin.join(&name);
    if direct.is_file() {
        return Some(direct);
    }
    // The Linux archive nests a build-<tag> directory; the Windows one is flat.
    fs::read_dir(bin)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path().join(&name))
        .find(|p| p.is_file())
}

fn archive_limits(asset: &Asset) -> Limits {
    Limits::with_max_bytes(asset.max_extracted_bytes.unwrap_or(1 << 30))
}

pub fn run<W: Write>(
    opts: &LocalOptions,
    manifest: &Manifest,
    probe: &prereq::Report,
    out: &mut W,
) -> Result<Summary, String> {
    let skip = |id: &str| opts.skip.contains(id);
    for s in &opts.skip {
        if !STEPS.contains(&s.as_str()) {
            return Err(format!(
                "unknown step {s:?} in --skip (steps: {})",
                STEPS.join(", ")
            ));
        }
    }
    if opts.assets.to_string_lossy().contains(char::is_whitespace) {
        return Err(format!(
            "the asset directory {} contains a space; the app splits AIVTUBER_SPEECH_SIDECAR_ARGS on whitespace, so choose a path without spaces with --assets",
            opts.assets.display()
        ));
    }
    // Is this an extracted release?
    let root = &opts.root;
    let required = [
        exe("aivtuber-app"),
        exe("speech-vad-sidecar"),
        exe("speech-stt-worker"),
        exe("local-tts-sidecar"),
        "renderer/local-avatar/dist/index.html".to_owned(),
        "examples/starter-reaction-pack/descriptors".to_owned(),
    ];
    let absent: Vec<_> = required.iter().filter(|r| !root.join(r).exists()).collect();
    if !absent.is_empty() {
        return Err(format!(
            "{} does not look like an extracted release archive (missing: {}); run aivtuber-setup from the unpacked folder, or pass --root",
            root.display(),
            absent
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let flavor = match (&opts.flavor, &probe.vulkan) {
        (Some(f), _) => f.clone(),
        (None, Vulkan::Found(_)) => "vulkan".to_owned(),
        (None, Vulkan::NotFound) => "cpu".to_owned(),
    };
    let llama = manifest.llama_asset(&opts.platform, &flavor)?;
    let mut summary = Summary::default();
    let mut ctx = Ctx {
        opts,
        out,
        missing: Vec::new(),
    };

    let mode = if opts.dry_run {
        "dry run: nothing is downloaded or written"
    } else if opts.offline {
        "offline: nothing is downloaded"
    } else {
        "fetching"
    };
    ctx.say(format!("aivtuber-setup local ({mode})"));
    ctx.say(format!("  install  {}", root.display()));
    ctx.say(format!("  assets   {}", opts.assets.display()));
    ctx.say(format!("  env file {}", opts.env_file.display()));
    ctx.say(format!(
        "  llama.cpp {} build {flavor} on {}",
        manifest.llama.tag, opts.platform
    ));

    ctx.say("\nPrerequisites");
    for (name, found) in [("ffplay", &probe.ffplay), ("ffmpeg", &probe.ffmpeg)] {
        match found {
            Some(p) => ctx.say(format!("  ok        {name}: {}", p.display())),
            None => ctx.say(format!("  WARNING   {name}: not found on PATH")),
        }
    }
    match &probe.vulkan {
        Vulkan::Found(p) => ctx.say(format!("  ok        Vulkan loader: {p}")),
        Vulkan::NotFound => ctx.say("  WARNING   Vulkan loader: not found"),
    }
    summary.warnings = probe.warnings();
    for w in &summary.warnings {
        ctx.say(format!("  -> {w}"));
    }

    let assets = &opts.assets;
    ctx.say("\nAssets");

    // llama.cpp
    let llama_dir = assets.join("llama");
    let llama_bin = llama_dir.join("bin");
    let stamp = llama_dir.join(format!(".verified-{}", &llama.sha256[..16]));
    let mut llama_server =
        find_llama_server(&llama_bin).unwrap_or_else(|| llama_bin.join(exe("llama-server")));
    if !skip("llama") {
        let have = stamp.is_file() && llama_server.is_file();
        let mb = llama.size as f64 / MIB;
        if have {
            ctx.say(format!("  have      llama.cpp {}", manifest.llama.tag));
            summary.provisioned.push("llama");
        } else if opts.dry_run {
            ctx.say(format!(
                "  download  {} ({mb:.1} MiB) from {} and extract",
                llama.file,
                host_of(&llama.url)
            ));
        } else if opts.offline {
            ctx.say(format!(
                "  MISSING   llama.cpp {} (--offline: nothing is fetched)",
                manifest.llama.tag
            ));
            ctx.missing.push("llama.cpp".to_owned());
        } else {
            let archive = llama_dir.join(&llama.file);
            if ctx.ensure_file(&llama.file, &llama.url, &archive, &llama.sha256, llama.size)? {
                let _ = fs::remove_dir_all(&llama_bin);
                extract::extract(&archive, &llama_bin, archive_limits(llama))
                    .map_err(|e| format!("{}: {e}", llama.file))?;
                llama_server = find_llama_server(&llama_bin).ok_or_else(|| {
                    format!(
                        "{} not found after extracting {}",
                        exe("llama-server"),
                        llama.file
                    )
                })?;
                fs::write(&stamp, &llama.sha256).map_err(|e| e.to_string())?;
                let _ = fs::remove_file(&archive);
                summary.provisioned.push("llama");
            }
        }
    }

    // GGUF
    let llm_path = assets.join("llm").join(&manifest.llm.file);
    if !skip("llm")
        && ctx.ensure_file(
            &manifest.llm.file,
            &manifest.llm.url,
            &llm_path,
            &manifest.llm.sha256,
            manifest.llm.size,
        )?
    {
        summary.provisioned.push("llm");
    }

    // VAD
    let vad_path = assets.join("vad").join(&manifest.vad.file);
    if !skip("vad")
        && ctx.ensure_file(
            &manifest.vad.file,
            &manifest.vad.url,
            &vad_path,
            &manifest.vad.sha256,
            manifest.vad.size,
        )?
    {
        summary.provisioned.push("vad");
    }

    // STT (ReazonSpeech k2-v2 int8, individual files)
    let stt_dir = assets.join("k2v2");
    if !skip("stt") {
        let stt = manifest
            .models
            .get("k2v2-int8")
            .ok_or("manifest has no k2v2-int8 model")?;
        let mut all = true;
        for f in &stt.files {
            let url = f.url.as_deref().ok_or("k2v2 file without a URL")?;
            all &= ctx.ensure_file(&f.file, url, &stt_dir.join(&f.file), &f.sha256, f.size)?;
        }
        if all {
            summary.provisioned.push("stt");
        }
    }

    // TTS (Supertonic 3 int8, one archive)
    let tts = manifest
        .models
        .get("supertonic-3-int8")
        .ok_or("manifest has no supertonic-3-int8 model")?;
    let tts_root = assets.join("supertonic");
    let tts_dir = tts_root.join(
        tts.inner_dir
            .as_deref()
            .ok_or("supertonic model without inner_dir")?,
    );
    let tts_complete = |dir: &Path| {
        tts.files
            .iter()
            .all(|f| matches_pin(&dir.join(&f.file), &f.sha256, f.size))
    };
    let mut tts_ready = false;
    if !skip("tts") {
        let archive = tts
            .archive
            .as_ref()
            .ok_or("supertonic model without archive")?;
        if opts.dry_run && tts_dir.is_dir() {
            ctx.say("  have      Supertonic 3 int8 voice (digests are checked on a real run)");
        } else if !opts.dry_run && tts_complete(&tts_dir) {
            ctx.say("  have      Supertonic 3 int8 voice");
            tts_ready = true;
        } else if opts.dry_run {
            ctx.say(format!(
                "  download  {} ({:.1} MiB) from {} and extract",
                archive.file,
                archive.size as f64 / MIB,
                host_of(&archive.url)
            ));
        } else if opts.offline {
            ctx.say("  MISSING   Supertonic 3 int8 voice (--offline: nothing is fetched)");
            ctx.missing.push("Supertonic 3 int8 voice".to_owned());
        } else {
            let archive_path = tts_root.join(&archive.file);
            if ctx.ensure_file(
                &archive.file,
                &archive.url,
                &archive_path,
                &archive.sha256,
                archive.size,
            )? {
                extract::extract(&archive_path, &tts_root, archive_limits(archive))
                    .map_err(|e| format!("{}: {e}", archive.file))?;
                if !tts_complete(&tts_dir) {
                    let _ = fs::remove_dir_all(&tts_dir);
                    return Err(format!(
                        "{} did not unpack to the pinned model files",
                        archive.file
                    ));
                }
                let _ = fs::remove_file(&archive_path);
                ctx.say("  ok        voice files match their pins");
                tts_ready = true;
            }
        }
        if tts_ready {
            summary.provisioned.push("tts");
        }
    }

    // Sample avatar
    let page_dir = root.join("renderer").join("local-avatar").join("dist");
    if !skip("avatar") {
        let avatar = &manifest.avatar;
        let cached = assets.join("avatar").join(&avatar.file);
        let have = ctx.ensure_file(
            &avatar.file,
            &avatar.url,
            &cached,
            &avatar.sha256,
            avatar.size,
        )?;
        let model = page_dir.join("model.vrm");
        if model.exists() {
            ctx.say("  have      avatar page model.vrm (your file is never overwritten)");
            summary.provisioned.push("avatar");
        } else if have && !opts.dry_run {
            fs::copy(&cached, &model).map_err(|e| {
                format!("cannot place the sample avatar at {}: {e}", model.display())
            })?;
            ctx.say(format!("  ok        sample avatar placed at {} (replace it with a model you may broadcast)", model.display()));
            summary.provisioned.push("avatar");
        }
    }

    // Starter audio
    let pack_root = root.join("examples").join("starter-reaction-pack");
    let audio_root = pack_root.join("audio");
    let sidecar = root.join(exe("local-tts-sidecar"));
    if !skip("audio") {
        ctx.say("\nStarter reaction audio");
        if audio::starter_audio_present(&pack_root, &audio_root) {
            ctx.say("  have      starter audio and PROVENANCE.json");
            summary.provisioned.push("audio");
        } else if opts.dry_run {
            ctx.say("  synthesize the starter reaction audio with the bundled local-tts-sidecar and ffmpeg");
        } else if !tts_ready && !tts_complete(&tts_dir) {
            ctx.say("  skipped   the voice files are not available yet");
        } else if let Some(ffmpeg) = &probe.ffmpeg {
            ctx.say("  synthesizing (the voice engine loads first; this takes a minute)");
            let p = audio::synthesize_starter_audio(
                &sidecar,
                &tts_dir,
                &pack_root,
                &audio_root,
                0,
                ffmpeg,
            )
            .map_err(|e| format!("starter audio: {e}"))?;
            ctx.say(format!(
                "  ok        {} files under {}",
                p.files.len(),
                audio_root.join("starter").display()
            ));
            summary.provisioned.push("audio");
        } else {
            ctx.say("  skipped   ffmpeg is not on PATH (see the warning above); install it and run this command again");
        }
    }

    if !ctx.missing.is_empty() {
        let list = ctx.missing.join(", ");
        return Err(format!(
            "--offline: missing {list}; run once without --offline while connected"
        ));
    }

    // Env file
    ctx.say("\nConfiguration");
    let paths = Paths {
        llama_server,
        llm_gguf: llm_path,
        vad_model: vad_path,
        stt_model_dir: stt_dir,
        tts_model_dir: tts_dir,
        tts_sidecar: sidecar,
        speech_sidecar: root.join(exe("speech-vad-sidecar")),
        stt_worker: root.join(exe("speech-stt-worker")),
        app: root.join(exe("aivtuber-app")),
        pack_root,
        audio_root,
        avatar_page_dir: page_dir,
    };
    if opts.dry_run {
        let state = if opts.env_file.exists() {
            "exists and would be kept (--force replaces it)"
        } else {
            "would be created with two generated secrets"
        };
        ctx.say(format!("  {} {state}", opts.env_file.display()));
        summary.env = None;
    } else {
        let secrets = Secrets::generate()?;
        let gpu_layers = if flavor == "vulkan" { 99 } else { 0 };
        let text = config::render(&paths, &secrets, opts.llama_port, gpu_layers);
        let outcome = config::write_env(&opts.env_file, &text, opts.force)?;
        match outcome {
            WriteOutcome::Written => ctx.say(format!("  wrote     {}", opts.env_file.display())),
            WriteOutcome::KeptExisting => ctx.say(format!(
                "  kept      {} (it already exists; --force writes a new one with new secrets)",
                opts.env_file.display()
            )),
        }
        summary.env = Some(outcome);
    }

    // Duties and start command
    ctx.say("\nLicences and what you must disclose");
    // A dry run provisions nothing, so it lists the duties of every step it would run.
    let covered: Vec<&str> = if opts.dry_run {
        STEPS.iter().copied().filter(|s| !skip(s)).collect()
    } else {
        summary.provisioned.clone()
    };
    for d in manifest.disclosures_for(&covered) {
        ctx.say(format!(
            "  * {} [{}]\n    {}",
            d.component, d.licence, d.duty
        ));
    }
    ctx.say("  The full texts are in THIRD-PARTY-NOTICES.txt and docs/local-profile-quickstart.adoc section 8.");
    ctx.say(if opts.dry_run {
        "\nStart (after a real run of this command)"
    } else {
        "\nStart"
    });
    ctx.say(format!("  {}", start_command(root, &opts.env_file)));
    ctx.say(format!(
        "  then open http://127.0.0.1:{} (or add it to OBS as a Browser Source)",
        config::AVATAR_PORT
    ));
    if !opts.dry_run && !summary.warnings.is_empty() {
        ctx.say(format!(
            "\nFinished with {} warning(s); see the Prerequisites section above.",
            summary.warnings.len()
        ));
    }
    Ok(summary)
}

/// The exact command to start the profile (quotes the paths if they need it).
pub fn start_command(root: &Path, env_file: &Path) -> String {
    let setup = root.join(exe("aivtuber-setup"));
    let q = |p: &Path| {
        let s = p.display().to_string();
        if s.contains(' ') {
            format!("\"{s}\"")
        } else {
            s
        }
    };
    let default_env = root.join("local-profile.env");
    if env_file == default_env {
        format!("{} start", q(&setup))
    } else {
        format!("{} start --env-file {}", q(&setup), q(env_file))
    }
}
