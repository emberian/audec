import sys,json
d=json.loads(sys.stdin.readline())
if not d.get("ok"):
    print("error:", d.get("error")); sys.exit(1)
modes=d["result"]
want=sys.argv[1:] or ["Project"]
def walk(n,depth=0):
    t=n["target"]; tag=t.get("kind") or t.get("category") or t.get("mode")
    extra=""
    if n.get("detail"): extra+=f"  ({n['detail']})"
    if n.get("diagnostic"): extra+=f"  !{n['diagnostic']}"
    if depth<=4: print("  "*depth+f"{n['label']}  [{tag}]{extra}")
    for c in n["children"]: walk(c,depth+1)
for m in modes:
    if m["label"] in want or m["target"].get("mode") in want: walk(m)
