#!/usr/bin/env python3
"""Measure Hindsight on the portable memory fixture (issue #105).

Reads ``examples/memory-long-range/cases.json``, retains every claim in a fresh
Hindsight bank (one ``retain`` per claim, fixture order, ``document_id`` = claim
id), recalls once per case, maps every recalled item back to claim ids through
Hindsight's own provenance (``document_id`` for a fact, ``source_fact_ids`` for
an observation), and writes a ``memory-long-range-eval/v1`` report that
``memory-eval-compare`` accepts.

What this adapter deliberately does NOT do (it would give the runtime a home
advantage, or Hindsight a handicap):

* no supersession, expiry, retention-bound or correction handling: the service
  is told every claim and decides for itself what to recall;
* no re-ranking, no text similarity, no shortening of text.

What it does do on the output side, because the report contract says so:

* ``bounded_prefix`` packing (same rule as ``aivtuber_app::bounded_prefix``):
  whole items in Hindsight's rank order, an item over the per-item byte bound
  skipped, packing stops at the first item that would overflow the request
  byte bound or at the item bound;
* claim ids are de-duplicated, first rank kept;
* an item whose claim is another viewer's, or that cannot be traced to a
  claim, is not delivered; it is recorded in ``refused_items``
  (``out_of_scope`` / ``unknown``) exactly as the Rust sidecar runner does.

Standard library plus ``requests``. See README.md.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import sys
import time
import uuid
from pathlib import Path

import requests

REPORT_SCHEMA = "memory-long-range-eval/v1"
FIXTURE_SCHEMA = "memory-eval-v1"
ACTOR_TAG = "actor:"
# Transport bound on one recall: large enough that Hindsight's own token
# budget never cuts the ranking below what the project's request bounds admit
# (12 items, 8192 bytes). The project bounds are applied afterwards.
RECALL_MAX_TOKENS = 16384
SOURCE_FACTS_MAX_TOKENS = 16384
# The fixture's synthetic request text when a query carries no topic
# (MEMORY_EVAL_REQUEST_TEXT in crates/app/src/memory_eval.rs).
REQUEST_TEXT = "what happened next?"


class EvalError(RuntimeError):
    pass


# --------------------------------------------------------------------------
# fixture
# --------------------------------------------------------------------------


def claim_text(claim: dict) -> str:
    """The claim text, tiled to ``text_bytes`` exactly as the Rust runner does."""
    size = claim.get("text_bytes")
    seed = claim["text"]
    if size is None:
        return seed
    if size == 0 or not seed.isascii():
        raise EvalError(f"claim {claim['event_id']!r}: text_bytes needs a non-empty ASCII seed")
    out = []
    length = 0
    while length < size:
        piece = seed[: min(len(seed), size - length)]
        out.append(piece)
        length += len(piece)
    return "".join(out)


def event_at(claim: dict) -> int:
    value = claim.get("event_at_ms")
    return claim["recorded_at_ms"] if value is None else value


def iso8601(epoch_ms: int) -> str:
    moment = dt.datetime.fromtimestamp(epoch_ms / 1000.0, tz=dt.timezone.utc)
    return moment.strftime("%Y-%m-%dT%H:%M:%S.") + f"{epoch_ms % 1000:03d}Z"


# --------------------------------------------------------------------------
# Hindsight REST client (plain HTTP)
# --------------------------------------------------------------------------


class Hindsight:
    def __init__(self, endpoint: str, timeout: float):
        self.base = endpoint.rstrip("/")
        self.timeout = timeout
        self.session = requests.Session()

    def _call(self, method: str, path: str, body=None, params=None, ok=(200,)):
        response = self.session.request(
            method, self.base + path, json=body, params=params, timeout=self.timeout
        )
        if response.status_code not in ok:
            raise EvalError(
                f"{method} {path} -> {response.status_code}: {response.text[:400]}"
            )
        return response.json() if response.content else {}

    def version(self) -> dict:
        return self._call("GET", "/version")

    def delete_bank(self, bank: str) -> None:
        self._call("DELETE", f"/v1/default/banks/{bank}", ok=(200, 202, 204, 404))

    def retain(self, bank: str, item: dict) -> dict:
        return self._call(
            "POST", f"/v1/default/banks/{bank}/memories", {"items": [item], "async": False}
        )

    def stats(self, bank: str) -> dict:
        return self._call("GET", f"/v1/default/banks/{bank}/stats")

    def recall(self, bank: str, body: dict) -> dict:
        return self._call("POST", f"/v1/default/banks/{bank}/memories/recall", body)

    def memory(self, bank: str, memory_id: str) -> dict:
        return self._call("GET", f"/v1/default/banks/{bank}/memories/{memory_id}")

    def wait_settled(self, bank: str, timeout_s: float) -> dict:
        """Wait until retain and consolidation have nothing pending.

        Observations are produced by background consolidation; recalling
        before it ran would measure a service that was not finished.
        """
        deadline = time.monotonic() + timeout_s
        quiet = 0
        while True:
            stats = self.stats(bank)
            if stats.get("failed_operations") or stats.get("failed_consolidation"):
                raise EvalError(f"bank {bank}: failed operations reported: {stats}")
            pending = (stats.get("pending_operations") or 0) + (
                stats.get("pending_consolidation") or 0
            )
            quiet = quiet + 1 if pending == 0 else 0
            # Two consecutive empty reads: consolidation may enqueue after the
            # last retain completes.
            if quiet >= 2:
                return stats
            if time.monotonic() > deadline:
                raise EvalError(f"bank {bank}: not settled after {timeout_s}s: {stats}")
            time.sleep(3)


# --------------------------------------------------------------------------
# attribution
# --------------------------------------------------------------------------


def attribute(result: dict, source_facts: dict, claim_ids: set[str], lookup) -> tuple[list[str], str]:
    """Claim ids a recalled item traces to, through Hindsight's provenance.

    Returns ``(ids, basis)``; ``ids`` is empty when the item cannot be traced.
    Only ``document_id`` (set to the claim id at retain) is used; text is never
    compared.
    """
    document = result.get("document_id")
    if document:
        return [document], "document_id"
    source_ids = result.get("source_fact_ids") or []
    ids: list[str] = []
    for fact_id in source_ids:
        fact = source_facts.get(fact_id)
        if fact is None:
            fact = lookup(fact_id)
        doc = (fact or {}).get("document_id")
        if not doc:
            return [], "source_fact_without_document"
        if doc not in ids:
            ids.append(doc)
    if ids:
        return ids, "source_fact_ids"
    return [], "no_provenance"


# --------------------------------------------------------------------------
# one case
# --------------------------------------------------------------------------


def run_case(client: Hindsight, case: dict, runtime_row: dict, budgets: dict, bank: str, args) -> tuple[dict, dict]:
    started = time.monotonic()
    claims = {c["event_id"]: c for c in case["claims"]}
    texts = {cid: claim_text(c) for cid, c in claims.items()}
    query = case["query"]
    trace: dict = {"case": case["id"], "retain": [], "recall": [], "unattributed": []}

    client.delete_bank(bank)  # a stale bank from an interrupted run
    try:
        # --- retain: one call per claim, fixture order -------------------
        for claim in case["claims"]:
            cid = claim["event_id"]
            item = {
                "content": texts[cid],
                "timestamp": iso8601(event_at(claim)),
                "document_id": cid,
                "metadata": {"claim_id": cid, "recorded_at_ms": str(claim["recorded_at_ms"])},
            }
            if claim.get("actor"):
                item["tags"] = [ACTOR_TAG + claim["actor"]]
            if claim.get("topic"):
                item["metadata"]["topic"] = claim["topic"]
            t0 = time.monotonic()
            response = client.retain(bank, item)
            if not response.get("success", False):
                raise EvalError(f"case {case['id']}: retain {cid!r} failed: {response}")
            trace["retain"].append({"claim": cid, "seconds": round(time.monotonic() - t0, 1)})
        stats = client.wait_settled(bank, args.settle_timeout)
        trace["bank_stats"] = {
            k: stats.get(k)
            for k in ("total_nodes", "nodes_by_fact_type", "total_documents", "total_observations")
        }

        # --- recall ------------------------------------------------------
        body: dict = {
            "query": query.get("topic") or REQUEST_TEXT,
            "query_timestamp": iso8601(query["now_ms"]),
            "max_tokens": RECALL_MAX_TOKENS,
            "include": {"source_facts": {"max_tokens": SOURCE_FACTS_MAX_TOKENS}},
        }
        actor = query.get("actor")
        if actor:
            body["tags"] = [ACTOR_TAG + actor]
            body["tags_match"] = "any"  # the viewer's claims plus unscoped (shared) ones
        else:
            body["tags"] = []
            body["tags_match"] = "exact"  # only unscoped claims
        response = client.recall(bank, body)
        results = response.get("results") or []
        source_facts = response.get("source_facts") or {}
        lookup = lambda fid: client.memory(bank, fid)  # noqa: E731

        # --- map, scope-check, de-duplicate -----------------------------
        accepted: list[dict] = []  # {ids, text}
        refused: list[dict] = []
        seen: set[str] = set()
        for rank, result in enumerate(results):
            ids, basis = attribute(result, source_facts, set(claims), lookup)
            entry = {
                "rank": rank,
                "type": result.get("type"),
                "text": result.get("text"),
                "document_id": result.get("document_id"),
                "source_fact_ids": result.get("source_fact_ids"),
                "mapped_to": ids,
                "basis": basis,
            }
            trace["recall"].append(entry)
            if not ids:
                refused.append({"claim_id": None, "reason": "unknown"})
                trace["unattributed"].append(entry)
                continue
            unknown = [i for i in ids if i not in claims]
            if unknown:
                for i in unknown:
                    refused.append({"claim_id": i, "reason": "unknown"})
                trace["unattributed"].append(entry)
                continue
            foreign = [
                i for i in ids
                if claims[i].get("actor") is not None and claims[i].get("actor") != actor
            ]
            if foreign:
                for i in foreign:
                    refused.append({"claim_id": i, "reason": "out_of_scope"})
                continue
            fresh = [i for i in ids if i not in seen]
            seen.update(ids)
            if not fresh:
                continue
            accepted.append({"ids": fresh, "all_ids": ids, "text": result["text"]})

        # --- pack whole items (aivtuber_app::bounded_prefix) -------------
        used = query.get("charged_bytes", 0)
        packed: list[dict] = []
        for item in accepted:
            if len(packed) >= budgets["context_item_budget"]:
                break
            size = len(item["text"].encode("utf-8"))
            if size > budgets["item_byte_budget"]:
                continue
            if used + size > budgets["context_byte_budget"]:
                break
            used += size
            packed.append({**item, "bytes": size})
    finally:
        client.delete_bank(bank)

    delivered = [i for item in packed for i in item["ids"]]
    # An item is unshortened only when it is exactly the one claim it carries.
    truncated = sum(
        1
        for item in packed
        if not (len(item["all_ids"]) == 1 and item["text"] == texts[item["all_ids"][0]])
    )
    must = case.get("must_deliver", [])
    refused_claims = case.get("must_not_deliver", [])
    stale = case.get("stale", [])
    claim_bytes = sum(item["bytes"] for item in packed)
    request_bytes = query.get("charged_bytes", 0) + claim_bytes
    hits = sum(1 for c in must if c in delivered)
    when = {cid: event_at(c) for cid, c in claims.items()}
    row = {
        "case": case["id"],
        "delivered": delivered,
        "must_deliver": must,
        "refused_claims": refused_claims,
        "stale_claims": stale,
        "retained_causal_spans": 0,
        "current_fact_correctness": 1.0 if not must else hits / len(must),
        "stale_memory_use": sum(1 for i in delivered if i in stale),
        "absent_violations": sum(1 for i in delivered if i in refused_claims),
        "refused_items": refused,
        "temporal_order_errors": sum(
            1 for a, b in zip(delivered, delivered[1:]) if when[b] > when[a]
        ),
        "context_items": len(packed),
        "claim_bytes": claim_bytes,
        "request_bytes": request_bytes,
        "within_item_budget": len(packed) <= budgets["context_item_budget"]
        and all(item["bytes"] <= budgets["item_byte_budget"] for item in packed),
        "within_byte_budget": request_bytes <= budgets["context_byte_budget"],
        "truncated_claims": truncated,
        # Nothing was retired, so the service held every claim it was told.
        "entries_high_water": len(case["claims"]),
        "links_high_water": 0,
        "max_entries": runtime_row["max_entries"],
        "max_links": runtime_row["max_links"],
    }
    trace["seconds"] = round(time.monotonic() - started, 1)
    return row, trace


def totals_for(rows: list[dict]) -> dict:
    n = len(rows)
    total = 0.0
    for row in rows:
        total += row["current_fact_correctness"]
    return {
        "cases": n,
        "current_fact_correctness": total / n,
        "stale_memory_use": sum(r["stale_memory_use"] for r in rows),
        "absent_violations": sum(r["absent_violations"] for r in rows),
        "retained_causal_spans": sum(r["retained_causal_spans"] for r in rows),
        "refused_items": sum(len(r["refused_items"]) for r in rows),
        "temporal_order_errors": sum(r["temporal_order_errors"] for r in rows),
        "truncated_claims": sum(r["truncated_claims"] for r in rows),
        "max_context_items": max(r["context_items"] for r in rows),
        "max_claim_bytes": max(r["claim_bytes"] for r in rows),
        "max_request_bytes": max(r["request_bytes"] for r in rows),
        "max_entries_high_water": max(r["entries_high_water"] for r in rows),
        "max_links_high_water": max(r["links_high_water"] for r in rows),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("fixture", type=Path)
    parser.add_argument("runtime_report", type=Path, help="runtime memory-long-range-eval report for the same fixture")
    parser.add_argument("--endpoint", default="http://localhost:8888")
    parser.add_argument("--out", type=Path, required=True, help="sidecar report path")
    parser.add_argument("--trace-out", type=Path, required=True, help="per-item recall trace path")
    parser.add_argument("--timeout", type=float, default=1800.0, help="seconds per HTTP call")
    parser.add_argument("--settle-timeout", type=float, default=3600.0)
    parser.add_argument("--only", help="comma-separated case ids (debugging; the report is then not comparable)")
    args = parser.parse_args()

    fixture = json.loads(args.fixture.read_text(encoding="utf-8"))
    runtime = json.loads(args.runtime_report.read_text(encoding="utf-8"))
    if fixture.get("schema_version") != FIXTURE_SCHEMA:
        raise EvalError("unexpected fixture schema_version")
    if runtime.get("schema_version") != REPORT_SCHEMA:
        raise EvalError("runtime report has an unexpected schema_version")
    runtime_rows = {r["case"]: r for r in runtime["cases"]}
    if [c["id"] for c in fixture["cases"]] != [r["case"] for r in runtime["cases"]]:
        raise EvalError("runtime report does not cover the fixture's cases in order")
    budgets = {k: runtime[k] for k in ("context_item_budget", "context_byte_budget", "item_byte_budget")}

    client = Hindsight(args.endpoint, args.timeout)
    version = client.version()
    only = set(args.only.split(",")) if args.only else None
    run = uuid.uuid4().hex[:8]
    rows, traces = [], []
    started = time.monotonic()
    for index, case in enumerate(fixture["cases"]):
        if only and case["id"] not in only:
            continue
        bank = f"eval-{run}-{index:02d}"
        print(f"[{index + 1}/{len(fixture['cases'])}] {case['id']}", file=sys.stderr, flush=True)
        row, trace = run_case(client, case, runtime_rows[case["id"]], budgets, bank, args)
        rows.append(row)
        traces.append(trace)
        print(f"    delivered={row['delivered']} ({trace['seconds']}s)", file=sys.stderr, flush=True)

    report = {
        "schema_version": REPORT_SCHEMA,
        "fixture_schema_version": fixture["schema_version"],
        # The digest names the fixture document; the runtime report was
        # produced from the same file by `memory-long-range-eval`, and
        # `memory-eval-compare` re-derives it and refuses a mismatch.
        "fixture_digest": runtime["fixture_digest"],
        **budgets,
        "totals": totals_for(rows),
        "cases": rows,
    }
    args.out.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    args.trace_out.write_text(
        json.dumps(
            {
                "hindsight_version": version,
                "total_seconds": round(time.monotonic() - started, 1),
                "cases": traces,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    print(f"wrote {args.out} ({round(time.monotonic() - started)}s)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except EvalError as error:
        print(f"hindsight_eval: {error}", file=sys.stderr)
        sys.exit(1)
