//! `aivtuber-setup local`: from an extracted release archive to a runnable
//! local profile. Every step is idempotent (a file that already matches its pin
//! is kept), so an interrupted or partly failed run is fixed by running it
//! again.
//!
//! Modes
//! * default: fetch what is missing, verify, extract, synthesize the starter
//!   audio, write the env file, print the duties and the start command;
//! * `--dry-run`: print the plan; no network, no file written;
//! * `--profile NAME`: the pinned model profile (`default`, or the opt-in
//!   `perception`); the env file and `start` follow it;
//! * `--llm-endpoint URL`: you run the model server; nothing is fetched for it;
//! * `--from-target DIR`: run from a source checkout (`--root`) with the
//!   executables of a cargo target directory instead of an extracted archive;
//! * `--offline`: no network; verifies what is on disk, synthesizes audio and
//!   writes the env file; fails and lists what is missing.

use crate::audio;
use crate::config::{self, LlmMode, LlmSetup, Paths, Secrets, WriteOutcome};
use crate::download::{self, DownloadConfig, Outcome, matches_pin};
use crate::extract::{self, Limits};
use crate::manifest::{Asset, Manifest};
use crate::prereq::{self, Vulkan};
use crate::vcruntime;
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
    /// A pinned model profile of the manifest (`default`, `perception`).
    pub profile: String,
    /// A model server the operator runs (loopback `http` URL). Implies
    /// `--skip llama,llm`: nothing is fetched for a model nobody will run.
    pub llm_endpoint: Option<String>,
    /// Where the executables are when they are not in `root` (a cargo target
    /// directory, for a developer running from a source checkout).
    pub bin_dir: Option<PathBuf>,
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
    let endpoint = opts
        .llm_endpoint
        .as_deref()
        .map(config::normalize_llm_endpoint)
        .transpose()?;
    let skip =
        |id: &str| opts.skip.contains(id) || (endpoint.is_some() && matches!(id, "llama" | "llm"));
    let profile = manifest.profile(&opts.profile)?;
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
    // Is this an extracted release (or, with --from-target, a built checkout)?
    let root = &opts.root;
    let bin = opts.bin_dir.as_deref().unwrap_or(root);
    let missing_of = |base: &Path, names: &[String]| -> Vec<String> {
        names
            .iter()
            .filter(|n| !base.join(n).exists())
            .map(|n| base.join(n).display().to_string())
            .collect()
    };
    let mut absent = missing_of(
        bin,
        &[
            exe("aivtuber-app"),
            exe("speech-vad-sidecar"),
            exe("speech-stt-worker"),
            exe("local-tts-sidecar"),
        ],
    );
    absent.extend(missing_of(
        root,
        &[
            "renderer/local-avatar/dist/index.html".to_owned(),
            "examples/starter-reaction-pack/descriptors".to_owned(),
        ],
    ));
    if !absent.is_empty() {
        return Err(if opts.bin_dir.is_some() {
            format!(
                "{} with executables from {} is not a built checkout (missing: {}); build the speech binaries and the avatar page first (docs/local-profile-quickstart.adoc, \"Run from a source checkout\"), or pass --root",
                root.display(),
                bin.display(),
                absent.join(", ")
            )
        } else {
            format!(
                "{} does not look like an extracted release archive (missing: {}); run aivtuber-setup from the unpacked folder, or pass --root",
                root.display(),
                absent.join(", ")
            )
        });
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
    if let Some(dir) = &opts.bin_dir {
        ctx.say(format!("  programs {} (--from-target)", dir.display()));
    }
    ctx.say(format!(
        "  llama.cpp {} build {flavor} on {}",
        manifest.llama.tag, opts.platform
    ));
    ctx.say(format!(
        "  profile  {} ({})",
        profile.name, profile.spec.description
    ));
    if let Some(gb) = profile.spec.min_vram_gb {
        ctx.say(format!(
            "  REQUIRES  at least about {gb} GB of VRAM for the {} profile; on a smaller GPU the model spills to system memory and replies slow down badly (EXP-0004c). Use the default profile there.",
            profile.name
        ));
    }
    if let Some(url) = &endpoint {
        ctx.say(format!(
            "  model    your own server at {url}; llama.cpp and the model are not fetched, and `start` will not start a server"
        ));
    }

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

    // The Windows llama.cpp zip imports the Visual C++ runtime and does not
    // ship it; the release archive does (runtime/msvc). Runs on every pass, so a
    // folder set up by an earlier version is repaired by running this again.
    if !skip("llama") && opts.platform == "win32-x64" {
        let runtime = vcruntime::runtime_dir(bin);
        let names = &manifest.llama.windows_runtime_dlls;
        if opts.dry_run {
            ctx.say(format!(
                "  runtime   copy the Visual C++ runtime ({}) from {} beside {}",
                names.join(", "),
                runtime.display(),
                exe("llama-server")
            ));
        } else if llama_server.is_file() {
            let dest = llama_server.parent().unwrap_or(&llama_bin);
            let r = vcruntime::place(&runtime, dest, names)?;
            if r.no_runtime_dir {
                ctx.say(format!(
                    "  WARNING   {} not found: this is not a release archive (a source checkout?). llama-server needs the Visual C++ runtime installed on this machine",
                    runtime.display()
                ));
            } else {
                for f in &r.kept_different {
                    ctx.say(format!(
                        "  WARNING   {f} beside llama-server differs from the bundled one; kept as is"
                    ));
                }
                if !r.placed.is_empty() {
                    ctx.say(format!(
                        "  ok        Visual C++ runtime placed beside llama-server ({})",
                        r.placed.join(", ")
                    ));
                } else {
                    ctx.say("  have      Visual C++ runtime beside llama-server");
                }
            }
        }
    }

    // GGUF (and the vision projector of a multimodal profile)
    let llm_path = assets.join("llm").join(&profile.llm.file);
    let mmproj_path = profile.mmproj.map(|m| assets.join("llm").join(&m.file));
    if !skip("llm") {
        let mut all = ctx.ensure_file(
            &profile.llm.file,
            &profile.llm.url,
            &llm_path,
            &profile.llm.sha256,
            profile.llm.size,
        )?;
        if let (Some(m), Some(path)) = (profile.mmproj, &mmproj_path) {
            all &= ctx.ensure_file(&m.file, &m.url, path, &m.sha256, m.size)?;
        }
        if all {
            summary.provisioned.push("llm");
        }
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
    let sidecar = bin.join(exe("local-tts-sidecar"));
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
    let on_disk = |p: &Path, a: &Asset| fs::metadata(p).is_ok_and(|m| m.len() == a.size);
    let mode = match &endpoint {
        Some(url) => LlmMode::External(url.clone()),
        None if llama_server.is_file()
            && on_disk(&llm_path, profile.llm)
            && match (&mmproj_path, profile.mmproj) {
                (Some(p), Some(a)) => on_disk(p, a),
                _ => true,
            } =>
        {
            LlmMode::Managed
        }
        None => LlmMode::Missing,
    };
    let paths = Paths {
        llama_server,
        llm_gguf: llm_path,
        llm_mmproj: mmproj_path,
        vad_model: vad_path,
        stt_model_dir: stt_dir,
        tts_model_dir: tts_dir,
        tts_sidecar: sidecar,
        speech_sidecar: bin.join(exe("speech-vad-sidecar")),
        stt_worker: bin.join(exe("speech-stt-worker")),
        app: bin.join(exe("aivtuber-app")),
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
        let llm_setup = LlmSetup {
            profile: profile.name.to_owned(),
            alias: profile.spec.alias.clone(),
            model_version: profile.spec.model_version.clone(),
            context: profile.spec.context,
            parallel: profile.spec.parallel,
            mode: mode.clone(),
        };
        let text = config::render(&paths, &secrets, opts.llama_port, gpu_layers, &llm_setup);
        if mode == LlmMode::Missing {
            ctx.say("  NOTE      no language model is provisioned (skipped, or not on disk): `start` will refuse until you run this command again without --skip, or pass --llm-endpoint <loopback url>");
        }
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
    let profile_id = format!("profile:{}", profile.name);
    let mut covered: Vec<&str> = if opts.dry_run {
        STEPS.iter().copied().filter(|s| !skip(s)).collect()
    } else {
        summary.provisioned.clone()
    };
    if covered.contains(&"llm") {
        covered.push(&profile_id);
    }
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
    ctx.say(format!("  {}", start_command(bin, &opts.env_file)));
    ctx.say(format!(
        "  the app then prints two URLs, each with a per-launch secret (do not share them): the operation panel (http://127.0.0.1:{}/#token=...; chat, status, stop and mute) and the avatar page (http://127.0.0.1:{}/?token=..., also usable as an OBS Browser Source)",
        config::PANEL_PORT,
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
