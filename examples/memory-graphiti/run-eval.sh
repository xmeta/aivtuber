#!/usr/bin/env bash
# Run the #105 long-range evaluation against the local Graphiti stack.
#
# The script deliberately leaves the stack running after a successful run so
# the report can be inspected against the live service. Use --down to stop it;
# the named FalkorDB volume is kept either way.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
EXAMPLE_DIR="$SCRIPT_DIR"
COMPOSE=(docker compose --project-directory "$EXAMPLE_DIR" -f "$EXAMPLE_DIR/docker-compose.yml")

ENDPOINT="http://127.0.0.1:8000"
OUT_DIR=""
CHECK_ONLY=0
STOP_AFTER=0
BUILD=1
INGEST_TIMEOUT_MS="${INGEST_TIMEOUT_MS:-300000}"
STATUS_WRITTEN=0

usage() {
    cat <<'EOF'
usage: run-eval.sh [options]

  --check              validate prerequisites and exit without starting anything
  --endpoint <url>     service endpoint (default: http://127.0.0.1:8000)
  --out <dir>          artifact directory (default: target/graphiti-eval/<utc>)
  --no-build           use an existing image instead of rebuilding
  --down               stop the compose stack after the run
  -h, --help           show this help

The stack reads examples/memory-graphiti/.env. A missing key or a provider
that rejects requests is recorded in <out>/status.txt as `blocked: ...`;
it is never converted into a measured zero.
EOF
}

while (($# > 0)); do
    case "$1" in
        --check) CHECK_ONLY=1; shift ;;
        --endpoint) ENDPOINT="${2:?--endpoint needs a value}"; shift 2 ;;
        --out) OUT_DIR="${2:?--out needs a value}"; shift 2 ;;
        --no-build) BUILD=0; shift ;;
        --down) STOP_AFTER=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [[ -z "$OUT_DIR" ]]; then
    OUT_DIR="$ROOT/target/graphiti-eval/$(date -u +%Y%m%dT%H%M%SZ)"
fi
mkdir -p "$OUT_DIR"
STATUS="$OUT_DIR/status.txt"

blocked() {
    printf 'blocked: %s\n' "$*" >"$STATUS"
    STATUS_WRITTEN=1
    echo "graphiti-eval: blocked: $*" >&2
    exit 2
}

on_error() {
    local line="$1"
    if [[ "$STATUS_WRITTEN" == 0 ]]; then
        printf 'failed: line %s exited non-zero\n' "$line" >"$STATUS"
    fi
}
trap 'on_error "$LINENO"' ERR

for command in docker curl cargo; do
    command -v "$command" >/dev/null 2>&1 || blocked "missing command: $command"
done
docker info >/dev/null 2>&1 || blocked "Docker daemon is not reachable"

[[ -f "$EXAMPLE_DIR/.env" ]] || blocked "missing $EXAMPLE_DIR/.env (copy .env.example and fill it in)"
git -C "$ROOT" check-ignore -q "$EXAMPLE_DIR/.env" \
    || blocked "$EXAMPLE_DIR/.env is not ignored; refusing to run with a possibly committed key"

grep -Eq '^[[:space:]]*(LLM_API_KEY|GEMINI_API_KEY|GOOGLE_API_KEY)=[^[:space:]]+' "$EXAMPLE_DIR/.env" \
    || blocked "no non-empty LLM_API_KEY / GEMINI_API_KEY / GOOGLE_API_KEY in .env"

grep -Eq '^[[:space:]]*EMBEDDER_DIM=[0-9]+' "$EXAMPLE_DIR/.env" \
    || blocked "EMBEDDER_DIM must be set in .env to match the embedding model"

"${COMPOSE[@]}" config >/dev/null || blocked "docker compose config is invalid"

if [[ "$CHECK_ONLY" == 1 ]]; then
    printf 'ok: prerequisites present; endpoint %s; artifacts would go to %s\n' "$ENDPOINT" "$OUT_DIR"
    exit 0
fi

printf 'running: endpoint=%s out=%s\n' "$ENDPOINT" "$OUT_DIR" >"$STATUS"

if [[ "$BUILD" == 1 ]]; then
    "${COMPOSE[@]}" up -d --build
else
    "${COMPOSE[@]}" up -d
fi

echo "graphiti-eval: waiting for $ENDPOINT/healthcheck"
healthy=0
for _ in $(seq 1 90); do
    if curl --noproxy '*' -fsS "$ENDPOINT/healthcheck" >/dev/null 2>&1; then
        healthy=1
        break
    fi
    sleep 2
done
if [[ "$healthy" != 1 ]]; then
    "${COMPOSE[@]}" logs --tail=80 graphiti >&2 || true
    blocked "Graphiti did not become healthy at $ENDPOINT within 180s"
fi

# The runtime baseline is local and deterministic; the sidecar report is the
# candidate. Both must be produced from the same fixture for the comparison.
cargo run --locked -p aivtuber-app --bin memory-long-range-eval -- \
    "$ROOT/examples/memory-long-range/cases.json" \
    "$OUT_DIR/memory-runtime.json"

# Git Bash on Windows rewrites an argument that looks like a POSIX path
# ('/episode/{uuid}/cleanup') into a Windows path before cargo sees it, so this
# command runs with the rewrite disabled. That also stops the fixture and
# report paths from being translated, so they are handed over as native paths.
# Exporting the variable for the whole script would break 'git -C' and compose.
native_path() {
    if command -v cygpath >/dev/null 2>&1; then
        cygpath -w "$1"
    else
        printf '%s\n' "$1"
    fi
}

# A report with guardrail violations exits non-zero by design: that is a
# rejected candidate, which still has to reach the comparison. Only a run that
# produced no report at all is a runner failure.
sidecar_status=0
MSYS_NO_PATHCONV=1 cargo run --locked -p aivtuber-app --bin memory-sidecar-eval -- \
    --provider graphiti \
    --cleanup-path '/group/{scope}/episode/{uuid}/cleanup' \
    --ingest-timeout-ms "$INGEST_TIMEOUT_MS" \
    "$(native_path "$ROOT/examples/memory-long-range/cases.json")" \
    "$ENDPOINT" \
    "$(native_path "$OUT_DIR/memory-graphiti.json")" \
    >"$OUT_DIR/memory-graphiti.stdout.json" || sidecar_status=$?

if [[ ! -s "$OUT_DIR/memory-graphiti.json" ]]; then
    printf 'failed: sidecar produced no report (exit %s)\n' "$sidecar_status" >"$STATUS"
    STATUS_WRITTEN=1
    exit 1
fi


# A rejected candidate or an incomparable pair exits non-zero by design. That
# is a comparison result, not a runner failure: keep the verdict and the
# status distinct so a reader cannot mistake one for the other.
compare_status=0
cargo run --locked -p aivtuber-app --bin memory-eval-compare -- \
    "$ROOT/examples/memory-long-range/cases.json" \
    "$OUT_DIR/memory-runtime.json" \
    "$OUT_DIR/memory-graphiti.json" \
    --out "$OUT_DIR/memory-verdict.json" || compare_status=$?

if [[ "$compare_status" == 0 ]]; then
    printf 'completed: endpoint=%s verdict=clean\n' "$ENDPOINT" >"$STATUS"
else
    printf 'completed: endpoint=%s compare_exit=%s (see memory-verdict.json)\n' \
        "$ENDPOINT" "$compare_status" >"$STATUS"
fi
STATUS_WRITTEN=1

if [[ "$STOP_AFTER" == 1 ]]; then
    "${COMPOSE[@]}" down
fi

echo "graphiti-eval: artifacts in $OUT_DIR"
exit "$compare_status"
