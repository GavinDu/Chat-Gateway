import math, statistics

def aggregate_calls(rows):
	complete=[r for r in rows if r.get("protocol_complete") is True]
	def stats(field):
		vals=[r[field] for r in complete if type(r.get(field)) in (int, float) and math.isfinite(r[field]) and r[field] >= 0]
		if not vals:
			return {"n": 0, "min": None, "median": None, "max": None}
		return {"n": len(vals), "min": min(vals), "median": statistics.median(vals), "max": max(vals)}
	under5=0
	for r in complete:
		v=r.get("total_s")
		if type(v) in (int, float) and math.isfinite(v) and 0 <= v < 5:
			under5+=1
	return {"attempts": len(rows), "protocol_complete": len(complete), "protocol_incomplete": len(rows) - len(complete), "complete_total_under5": under5, "timings": {f: stats(f) for f in ("first_content_s", "generation_seconds", "tail_wait_s", "total_s")}}
