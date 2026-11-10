//! Best-effort prerequisite checks. Nothing here fails the setup: a missing
//! tool becomes a warning with a remedy, because the streamer can install it
//! and run `aivtuber-setup local` again (every step is idempotent).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Finds `name` in `path_var` (or `PATH`), trying `PATHEXT` suffixes on Windows.
pub fn find_on_path(name: &str, path_var: Option<OsString>) -> Option<PathBuf> {
    let path_var = path_var.or_else(|| std::env::var_os("PATH"))?;
    let suffixes: Vec<String> = if cfg!(windows) {
        let ext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
        std::iter::once(String::new())
            .chain(ext.split(';').filter(|e| !e.is_empty()).map(str::to_owned))
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path_var) {
        for suffix in &suffixes {
            let candidate = dir.join(format!("{name}{suffix}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vulkan {
    Found(String),
    NotFound,
}

/// Looks for a Vulkan loader. Presence of the loader is a hint, not proof of a
/// usable GPU: llama.cpp reports the real device when it starts.
pub fn detect_vulkan() -> Vulkan {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let dll = Path::new(&root).join("System32").join("vulkan-1.dll");
        if dll.is_file() {
            return Vulkan::Found(dll.display().to_string());
        }
    } else {
        for dir in [
            "/usr/lib/x86_64-linux-gnu",
            "/usr/lib64",
            "/usr/lib",
            "/lib/x86_64-linux-gnu",
            "/lib64",
        ] {
            let lib = Path::new(dir).join("libvulkan.so.1");
            if lib.is_file() {
                return Vulkan::Found(lib.display().to_string());
            }
        }
    }
    match find_on_path("vulkaninfo", None) {
        Some(p) => Vulkan::Found(p.display().to_string()),
        None => Vulkan::NotFound,
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    pub ffplay: Option<PathBuf>,
    pub ffmpeg: Option<PathBuf>,
    pub vulkan: Vulkan,
}

pub fn check() -> Report {
    Report {
        ffplay: find_on_path("ffplay", None),
        ffmpeg: find_on_path("ffmpeg", None),
        vulkan: detect_vulkan(),
    }
}

impl Report {
    /// Human-readable warnings, each with the remedy.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let install = if cfg!(windows) {
            "install FFmpeg (for example `winget install Gyan.FFmpeg`), then open a new terminal so it is on PATH"
        } else {
            "install FFmpeg (for example `sudo apt install ffmpeg`)"
        };
        if self.ffplay.is_none() {
            out.push(format!(
                "ffplay was not found on PATH: the avatar will be silent until it is installed; {install}."
            ));
        }
        if self.ffmpeg.is_none() {
            out.push(format!(
                "ffmpeg was not found on PATH: the starter reaction audio cannot be encoded yet; {install}, then run this command again."
            ));
        }
        if self.vulkan == Vulkan::NotFound {
            let remedy = if cfg!(windows) {
                "update the GPU driver (it ships the Vulkan loader)"
            } else {
                "install the Vulkan loader and your GPU's Vulkan driver (for example `sudo apt install libvulkan1 mesa-vulkan-drivers`)"
            };
            out.push(format!(
                "no Vulkan loader was found: the language model will run on the CPU, which is slower and competes with speech recognition. To use a GPU, {remedy}, then run this command again with `--llama-flavor vulkan`."
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_on_path_uses_the_given_directories() {
        let dir = std::env::temp_dir().join(format!("aivtuber-prereq-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) {
            "faketool.exe"
        } else {
            "faketool"
        };
        std::fs::write(dir.join(name), b"x").unwrap();
        let path = std::env::join_paths([&dir]).unwrap();
        assert!(find_on_path("faketool", Some(path.clone())).is_some());
        assert!(find_on_path("othertool", Some(path)).is_none());
    }

    #[test]
    fn warnings_name_the_missing_tool_and_a_remedy() {
        let report = Report {
            ffplay: None,
            ffmpeg: None,
            vulkan: Vulkan::NotFound,
        };
        let warnings = report.warnings();
        assert_eq!(warnings.len(), 3);
        assert!(warnings[0].contains("ffplay") && warnings[0].contains("install"));
        assert!(warnings[1].contains("ffmpeg"));
        assert!(warnings[2].contains("Vulkan") && warnings[2].contains("CPU"));

        let ok = Report {
            ffplay: Some("a".into()),
            ffmpeg: Some("b".into()),
            vulkan: Vulkan::Found("c".into()),
        };
        assert!(ok.warnings().is_empty());
    }
}
