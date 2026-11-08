import sys, os, json, subprocess, statistics
TB = os.environ["TURN_BENCH_DIR"]  # work directory, outside the repository
py = sys.executable
out = {}
for name in ("silero_v6_ort", "sherpa_vad_v4", "sherpa_vad_v6", "smart_turn_v3.2_cpu"):
    rs = []
    for _ in range(7):
        r = subprocess.run([py, "-I", "-X", "utf8", os.path.join(TB, "scripts", "footprint_one.py"), name], capture_output=True, text=True)
        rs.append(json.loads(r.stdout.strip().splitlines()[-1]))
    f = lambda k: statistics.median(x[k] for x in rs)
    out[name] = dict(load_s_median=f("load_s"), load_s_min=min(x["load_s"] for x in rs), load_s_max=max(x["load_s"] for x in rs),
                     rss_base_mb=f("rss_base_mb"), rss_after_mb=f("rss_after_mb"), delta_mb=f("delta_mb"), peak_wset_mb=f("peak_wset_mb"))
    print(name, {k: round(v, 3) for k, v in out[name].items()}, flush=True)
json.dump(out, open(os.path.join(TB, "out", "footprint.json"), "w"), indent=1)
