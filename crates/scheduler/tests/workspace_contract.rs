//! Workspace-manifest contract: the fuzz crate stays outside the workspace
//! (issue #146).
//!
//! `libfuzzer-sys` is a build-time C++ toolchain. If the fuzz crate were a
//! workspace member it would be resolved by `cargo test --workspace`,
//! `cargo clippy --all-targets`, `cargo deny check` and `cargo build
//! --workspace`, so the fuzzer's dependency tree would enter the workspace lock
//! file and the supply-chain gate — and a stable-channel CI runner would fail to
//! compile it at all, because libFuzzer needs nightly.
//!
//! That isolation is what makes the exclusion worth asserting rather than
//! merely documenting. It is easy to "fix" a dependency problem later by
//! re-adding `fuzz` to `members`, and the symptom would be a confusing nightly
//! error in CI rather than an obvious mistake. These tests read the manifests as
//! text and fail loudly instead.
//!
//! They assert the *declared* shape (manifest membership, lock contents). The
//! resolved graph is covered by `cargo deny check` in CI, and both assertions
//! here read files that a workspace build would have to load anyway.

use std::fs;
use std::path::{Path, PathBuf};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn root_manifest() -> String {
    fs::read_to_string(repository_root().join("Cargo.toml")).expect("root Cargo.toml is readable")
}

#[test]
fn the_workspace_excludes_the_fuzz_crate() {
    let manifest = root_manifest();
    let excluded = manifest
        .lines()
        .find(|line| line.trim_start().starts_with("exclude"))
        .expect("the root manifest declares an `exclude` list");
    assert!(
        excluded.contains("\"fuzz\""),
        "the root manifest must exclude the fuzz crate so `libfuzzer-sys` never \
         reaches the workspace lock file or `cargo deny check`; found: {excluded}"
    );
}

#[test]
fn the_fuzz_crate_is_not_a_workspace_member() {
    let manifest = root_manifest();
    let members = manifest
        .split("[workspace]")
        .nth(1)
        .and_then(|rest| rest.split("exclude").next())
        .expect("the root manifest has a [workspace] table with a member list");
    assert!(
        !members.contains("\"fuzz\""),
        "`fuzz` must not appear in the workspace member list; found: {members}"
    );
}

/// The lock file is the artifact that actually decides what a workspace build
/// downloads and compiles. An entry here would mean the fuzzer toolchain is
/// being resolved even if nothing imports it.
#[test]
fn the_workspace_lock_file_contains_no_fuzzing_toolchain() {
    let lock =
        fs::read_to_string(repository_root().join("Cargo.lock")).expect("Cargo.lock is readable");
    for forbidden in ["libfuzzer-sys", "arbitrary"] {
        assert!(
            !lock.contains(forbidden),
            "`{forbidden}` must not appear in the workspace Cargo.lock; it belongs to \
             fuzz/Cargo.lock only"
        );
    }
}

/// The fuzz crate is excluded, so it cannot inherit `[workspace.package]`. It
/// therefore has to state `edition` and `rust-version` literally, and drift from
/// the root manifest would silently fork the toolchain policy.
#[test]
fn the_fuzz_crate_restates_the_workspace_edition_and_rust_version() {
    let root = root_manifest();
    let edition = root
        .lines()
        .find_map(|line| line.trim().strip_prefix("edition"))
        .expect("the root manifest declares a workspace edition");
    let rust_version = root
        .lines()
        .find_map(|line| line.trim().strip_prefix("rust-version"))
        .expect("the root manifest declares a workspace rust-version");

    let fuzz_manifest =
        fs::read_to_string(repository_root().join("fuzz/Cargo.toml")).expect("fuzz/Cargo.toml");
    for field in [edition, rust_version] {
        let key = field.split('=').next().unwrap_or_default().trim();
        assert!(
            fuzz_manifest.contains(field),
            "fuzz/Cargo.toml must restate `{field}` to match the workspace manifest"
        );
        assert!(
            fuzz_manifest.contains(&format!("{key} =")),
            "fuzz/Cargo.toml must declare `{key}` literally rather than inheriting it"
        );
    }
}

/// `cargo fuzz` only recognises a crate that opts in through this metadata, and
/// without `publish = false` the crate would look publishable to tooling that
/// walks the directory tree.
#[test]
fn the_fuzz_crate_is_marked_for_cargo_fuzz_and_is_unpublishable() {
    let manifest =
        fs::read_to_string(repository_root().join("fuzz/Cargo.toml")).expect("fuzz/Cargo.toml");
    assert!(
        manifest.contains("cargo-fuzz = true"),
        "fuzz/Cargo.toml must carry `[package.metadata] cargo-fuzz = true` for cargo-fuzz"
    );
    assert!(
        manifest.contains("publish = false"),
        "fuzz/Cargo.toml must not be publishable (issue #100 applies workspace-wide)"
    );
    assert!(
        manifest.contains("libfuzzer-sys"),
        "fuzz/Cargo.toml must depend on libfuzzer-sys"
    );
}
