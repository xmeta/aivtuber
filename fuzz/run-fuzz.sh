#!/usr/bin/env bash
# Run a libFuzzer target from this repository (issue #146).
#
# Usage:
#   ./fuzz/run-fuzz.sh <target> [extra libFuzzer args...]
#   ./fuzz/run-fuzz.sh min <target> <artifact>   # minimize one failing input
#
# Examples:
#   ./fuzz/run-fuzz.sh event_envelope
#   ./fuzz/run-fuzz.sh event_envelope -runs=100000 -max_len=8192
#   ./fuzz/run-fuzz.sh jev_response -runs=500000 -max_total_time=300
#   ./fuzz/run-fuzz.sh min performance_asset fuzz/artifacts/performance_asset/crash-ab12
#
# Why this wrapper exists instead of calling `cargo fuzz run` directly:
#
# On a Windows/MSVC host the libFuzzer binary links the AddressSanitizer
# runtime dynamically (`clang_rt.asan_dynamic-x86_64.dll`), which ships with the
# Visual Studio toolset rather than next to the executable. Without it on PATH
# the process fails at load time with STATUS_DLL_NOT_FOUND (0xC0000135), which
# looks like a broken target rather than a missing runtime. This script locates
# that DLL and puts it on PATH before handing over to cargo-fuzz.
#
# Fuzzing needs a nightly compiler (libFuzzer support):
#
#   rustup toolchain install nightly
#   cargo +nightly install cargo-fuzz --locked
#
# Nightly is a local prerequisite only. It is deliberately absent from
# .github/workflows/ci.yml because scripts/validate-toolchain-pins.mjs requires
# every `toolchain:` in CI to equal the single channel in rust-toolchain.toml.
# Adding a nightly CI job belongs to issue #147. See docs/fuzzing.adoc.
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <target> [libFuzzer args...]" >&2
  echo "       $0 min <target> <artifact>" >&2
  exit 2
fi

# `min` is a cargo-fuzz subcommand that re-invokes the target binary. Calling
# `cargo fuzz tmin` directly therefore SKIPS the ASan PATH setup below and dies
# with STATUS_DLL_NOT_FOUND on Windows, so it is routed through the wrapper too.
mode="run"
if [[ $1 == "min" ]]; then
  mode="min"
  shift
  if [[ $# -lt 2 ]]; then
    echo "usage: $0 min <target> <artifact>" >&2
    exit 2
  fi
fi

target=$1
shift

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

# Put the ASan runtime on PATH when running on an MSVC host.
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    asan_dir=$(find "/c/Program Files/Microsoft Visual Studio" \
      -type d -path '*Tools/MSVC/*/bin/Host*64/x64' 2>/dev/null | head -n 1)
    if [[ -n "$asan_dir" && -f "$asan_dir/clang_rt.asan_dynamic-x86_64.dll" ]]; then
      PATH="$PATH:$asan_dir"
      export PATH
    else
      echo "warning: clang_rt.asan_dynamic-x86_64.dll not found; the target may fail" \
        "with STATUS_DLL_NOT_FOUND" >&2
    fi
    ;;
esac

# The fuzz crate is excluded from the workspace, so cargo needs to be told where
# it lives rather than discovering it from the root manifest.
cd "$repo_root"
if [[ "$mode" == "min" ]]; then
  exec cargo +nightly fuzz tmin "$target" "$@"
fi
exec cargo +nightly fuzz run "$target" -- "$@"