#!/usr/bin/env python3
"""Summarize an APPOPT_TRACE csv and suggest EWMA threshold changes.

Usage: python3 trace_report.py appopt-trace.csv
"""
import csv, sys, statistics as st
from collections import defaultdict

def load(path):
    rows = []
    with open(path, newline="") as f:
        for r in csv.DictReader(f):
            try:
                rows.append({
                    "ts": int(r["ts_ms"]), "src": r["src"], "tid": int(r["tid"]),
                    "fast": int(r["fast"]), "slow": int(r["slow"]), "action": r["action"],
                })
            except (KeyError, ValueError):
                pass
    rows.sort(key=lambda x: x["ts"])
    return rows

def episodes(rows, tids, gap_ms=2500):
    """Candidate episodes per tid: consecutive candidate lines separated by <gap."""
    eps, cur = [], {}
    for r in rows:
        if r["tid"] not in tids:
            continue
        tid = r["tid"]
        if tid in cur and r["ts"] - cur[tid][-1]["ts"] <= gap_ms:
            cur[tid].append(r)
        else:
            if tid in cur:
                eps.append(cur[tid])
            cur[tid] = [r]
    eps.extend(cur.values())
    return eps

def main(path):
    rows = load(path)
    if not rows:
        print("no rows"); return
    span_h = max(1e-6, (rows[-1]["ts"] - rows[0]["ts"]) / 3.6e6)
    promoted = [r for r in rows if r["action"] in ("promote", "to-high")]
    released = [r for r in rows if r["action"] in ("release", "to-low")]
    holds_hot = [r for r in rows if r["src"] == "hot" and r["action"] == "hold" and r["fast"] >= 60]
    cand_eps = episodes(rows, {r["tid"] for r in rows if r["action"] in ("cand", "hold", "promote")})
    cand_eps = [e for e in cand_eps if max(x["fast"] for x in e) >= 60]
    missed, lat = 0, []
    for e in cand_eps:
        t0 = e[0]["ts"]
        hit = next((p for p in promoted if p["tid"] == e[0]["tid"] and 0 <= p["ts"] - t0 <= 3000), None)
        if hit is None and len(e) >= 2 and e[-1]["ts"] - t0 >= 1500:
            missed += 1
        elif hit is not None:
            lat.append(hit["ts"] - t0)
    churn = (len(promoted) + len(released)) / span_h
    weak = sum(1 for p in promoted if p["fast"] < 75 and p["fast"] - p["slow"] < 18)
    print(f"窗口            {span_h*60:.1f} min, {len(rows)} 行")
    if span_h * 60 >= 5:
        print(f"升档/降档       {len(promoted)} / {len(released)}，换手 {churn:.0f} 次/小时")
    else:
        print(f"升档/降档       {len(promoted)} / {len(released)}（样本 <5min，换手率不作判断）")
        churn = 0
    print(f"热保持(>=60)    {len(holds_hot)} 行")
    print(f"候选突发段      {len(cand_eps)}，漏检 {missed}")
    if lat:
        lat.sort()
        print(f"升档延迟        p50 {st.median(lat)}ms  p90 {lat[int(len(lat)*0.9)-1]}ms  max {lat[-1]}ms")
    print(f"弱升档          {weak}（fast<75 且突发分<18，疑似误升）")
    print()
    print("回推建议：")
    if missed:
        print(f"- 漏检 {missed} 段：先降 HOT_PROMOTE_PCT(60→55) 或 BURST_DELTA_PCT(18→14)，一次只动一个")
    if weak:
        print(f"- 弱升档 {weak} 次：升 HOT_PROMOTE_PCT 或 BURST_MIN_PCT，或加大 FAST 窗口")
    if churn > 20:
        print(f"- 换手 {churn:.0f}/h 偏高：升 HOT_MIN_HOLD(2→4s) 或降 HOT_RELEASE_PCT(22→18)")
    elif churn < 2 and promoted:
        print(f"- 换手 {churn:.0f}/h 偏低：降 HOT_MIN_HOLD 或升 HOT_RELEASE_PCT")
    if not missed and not weak and churn <= 20:
        print("- 当前阈值与这轮场景匹配，保留；换场景（游戏/相机/待机）重复采样再判断")

if __name__ == "__main__":
    main(sys.argv[1])
