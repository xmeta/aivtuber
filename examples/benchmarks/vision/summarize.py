"""Print the EXP-0004 tables from the JSON files written by vision_bench.py.

    python -I -X utf8 summarize.py OUT_DIR           # reads vlm-style and think/both files by name pattern
"""
import json
import sys
from pathlib import Path


def f(x, n=2):
    return "-" if x is None else f"{x:.{n}f}"


def main(d):
    d = Path(d)
    for p in sorted(d.glob("*.json")):
        j = json.loads(p.read_text(encoding="utf-8"))
        if "rows" in j and "summary" in j:
            s, l = j["summary"], j["summary"]["latency_s"]
            a = j["scene_agreement_pass1"]
            print(f"{p.stem}: load {f(j['load_s'], 1)} s, VRAM base/idle/first/peak {j['vram_before_mib']}/{j['vram_idle_mib']}/{j['vram_after_first_mib']}/{j['vram_peak_mib']} MiB, "
                  f"latency p50 {f(l['p50'])} p95 {f(l['p95'])} max {f(l['max'])} s (n={l['n']}), json {s['json_rate']:.3f}, model-schema {s['model_schema_rate']:.3f}, "
                  f"s1a raw {s['s1a_rate']:.3f} repaired {s['s1a_after_repair_rate']:.3f} (dup labels {s['dup_labels']}, nfkc-unstable {s['nfkc_unstable']}), "
                  f"max caption {s['max_caption_bytes']} B, scene agreement {a['agree']}/{a['n']} (majority {a['majority_baseline']:.2f}), "
                  f"identical across passes {j.get('identical_across_passes')}, tokens in/out {s['prompt_tokens_mean']:.0f}/{s['completion_tokens_mean']:.0f}")
            if a["confusion"]:
                print("   confusion:", a["confusion"])
        elif "ttft_s" in j:
            print(f"{p.stem}: load {f(j['load_s'], 1)} s, VRAM base/idle/peak {j['vram_before_mib']}/{j['vram_idle_mib']}/{j['vram_peak_mib']} MiB, "
                  f"ttft p50 {f(j['ttft_s']['p50'], 3)} p95 {f(j['ttft_s']['p95'], 3)} max {f(j['ttft_s']['max'], 3)} s, total p50 {f(j['total_s']['p50'])} s")
        elif "conditions" in j:
            print(f"{p.stem}: llm load {f(j['llm_load_s'], 1)} s, vlm load {f(j['vlm_load_s'], 1)} s, VRAM base/llm/both-idle/after-first/peak "
                  f"{j['vram_before_mib']}/{j['vram_llm_only_mib']}/{j['vram_both_idle_mib']}/{j['vram_both_after_first_mib']}/{j['vram_peak_mib']} MiB")
            for k, c in j["conditions"].items():
                t = c.get("ttft_s_all") or c["ttft_s"]
                extra = ""
                if "n_overlap" in c:
                    o, n = c["ttft_s_overlap"], c["ttft_s_no_overlap"]
                    extra = (f"; vlm jobs {c['vlm_jobs']} (p50 {f(c['vlm_job_s']['p50'])} s), overlapped {c['n_overlap']}: p50 {f(o['p50'], 3)} p95 {f(o['p95'], 3)} max {f(o['max'], 3)}; "
                             f"not overlapped {n['n']}: p50 {f(n['p50'], 3)} p95 {f(n['p95'], 3)}")
                print(f"   {k}: ttft n={t['n']} p50 {f(t['p50'], 3)} p95 {f(t['p95'], 3)} max {f(t['max'], 3)} s{extra}")


if __name__ == "__main__":
    main(sys.argv[1])
