import sys, json
for line in sys.stdin:
    d = json.loads(line); r = d.get("result", {})
    if isinstance(r, dict) and "playing" in r:
        sel = r["selection"]; lp = r["loop"]
        sel_s = (round(sel["start_seconds"], 1), round(sel["end_seconds"], 1)) if sel else None
        lp_s = (round(lp["start_seconds"], 1), round(lp["end_seconds"], 1), lp["enabled"]) if lp else None
        print(f"playing={r['playing']} playhead={r['playhead_seconds']:.2f}s sel={sel_s} loop={lp_s}")
    else:
        print(d)
