//! First-run setup for the local profile (issue #294 S2).
//!
//! `aivtuber-setup local` takes a streamer from an extracted release archive to
//! a runnable profile with no Bun, Node or Cargo on the machine: it fetches the
//! pinned third-party assets (HTTPS only, SHA-256 and size verified, resumable),
//! extracts them without path traversal, synthesizes the starter reaction audio
//! with the bundled sidecar, writes the env file and prints the licence duties
//! and the start command. `aivtuber-setup start` then runs the profile.
//!
//! Every pin comes from `packaging/local-profile-pins.json`, the one manifest
//! shared with the Node fetch scripts.

pub mod audio;
pub mod config;
pub mod download;
pub mod extract;
pub mod local;
pub mod manifest;
pub mod prereq;
pub mod start;
pub mod vcruntime;
