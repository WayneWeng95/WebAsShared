#!/usr/bin/env python3
"""Per-stage fan-out width test for StreamPipeline (Phase 1, single node).

Verifies that running a stage with `width: N` (N parallel workers, host
scatter/gather across private sub-slots) produces the SAME result as the
`width: 1` baseline, while actually splitting the per-tick batch across workers.

  1. word-count pipeline (20 records/round): filter×3 + transform×2 must equal
     the width:1 baseline AND the analytic ground truth; deterministic.
  2. image pipeline (1 record/round, 1-in-1-out stages): widening a stage must
     leave every output image byte-identical to the width:1 baseline.

Run from the project root:  python3 Tests/Streaming/width_test.py
Exit 0 = all pass.
"""
import os, sys, re, json, hashlib, tempfile, subprocess

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
HOST = os.path.join(ROOT, "Executor", "target", "release", "host")
WASM = os.path.join(ROOT, "Executor", "target", "wasm32-unknown-unknown", "release", "guest.wasm")

IMG_STAGES = [
    {"func": "img_load_ppm", "a": 10, "b": 20}, {"func": "img_rotate", "a": 20, "b": 30},
    {"func": "img_grayscale", "a": 30, "b": 40}, {"func": "img_equalize", "a": 40, "b": 50},
    {"func": "img_blur", "a": 50, "b": 60}, {"func": "img_export_ppm", "a": 60, "b": 1},
]
IMG_FILES = ["img_gradient.ppm", "img_checkerboard.ppm", "img_rings.ppm"]


def analytic(rounds):
    return [(10, sum(r * 1000 + i for i in range(0, 20, 2))) for r in range(rounds)]


def wc_dag(shm, rounds, out, fw, tw):
    return {
        "shm_path": shm, "wasm_path": WASM, "log_level": "error",
        "nodes": [
            {"id": "sp", "deps": [], "kind": {"StreamPipeline": {"rounds": rounds, "stages": [
                {"func": "pipeline_source", "arg0": 200, "arg1": None},
                {"func": "pipeline_filter", "arg0": 200, "arg1": 201, "width": fw},
                {"func": "pipeline_transform", "arg0": 201, "arg1": 202, "width": tw},
                {"func": "pipeline_sink", "arg0": 202, "arg1": 203},
            ]}}},
            {"id": "w", "deps": ["sp"], "kind": {"Watch": {"stream": 203, "output": out}}},
        ],
    }


def wc_var_dag(shm, rounds, out, filter_max):
    """Variable-load word-count: pipeline_source_var emits a round-dependent
    batch.  `filter_max` set → the filter autoscales in [1, filter_max]."""
    filt = {"func": "pipeline_filter", "arg0": 200, "arg1": 201}
    if filter_max:
        filt["max_width"] = filter_max
    return {
        "shm_path": shm, "wasm_path": WASM, "log_level": "error",
        "nodes": [
            {"id": "sp", "deps": [], "kind": {"StreamPipeline": {"rounds": rounds, "stages": [
                {"func": "pipeline_source_var", "arg0": 200, "arg1": None},
                filt,
                {"func": "pipeline_transform", "arg0": 201, "arg1": 202},
                {"func": "pipeline_sink", "arg0": 202, "arg1": 203},
            ]}}},
            {"id": "w", "deps": ["sp"], "kind": {"Watch": {"stream": 203, "output": out}}},
        ],
    }


def img_dag(shm, out_paths, widen_func, width):
    stages = []
    for s in IMG_STAGES:
        st = {"func": s["func"], "arg0": s["a"], "arg1": s["b"]}
        if s["func"] == widen_func:
            st["width"] = width
        stages.append(st)
    paths = [os.path.join(ROOT, "TestData", f) for f in IMG_FILES]
    return {
        "shm_path": shm, "wasm_path": WASM, "log_level": "error",
        "nodes": [
            {"id": "load", "deps": [], "kind": {"Input": {"paths": paths, "slot": 10, "binary": True}}},
            {"id": "pp", "deps": ["load"], "kind": {"StreamPipeline": {"rounds": 3, "stages": stages}}},
            {"id": "save", "deps": ["pp"], "kind": {"Output": {"slot": 1, "split_records": True, "paths": out_paths}}},
        ],
    }


def run_dag(dag, tmp, tag):
    shm = dag["shm_path"]
    for ext in ("", "_meta"):
        try: os.remove(shm + ext)
        except OSError: pass
    path = os.path.join(tmp, tag + ".json")
    with open(path, "w") as f:
        json.dump(dag, f)
    res = subprocess.run([HOST, "dag", path], capture_output=True, text=True, cwd=ROOT)
    if res.returncode != 0:
        print(res.stdout[-2000:]); print(res.stderr[-2000:])
        raise SystemExit(f"host dag {tag} failed ({res.returncode})")
    return res.stdout


def summaries(path):
    out = []
    for line in open(path):
        m = re.search(r"batch_count=(\d+),value_sum=(\d+)", line)
        if m:
            out.append((int(m.group(1)), int(m.group(2))))
    return out


def md5(path):
    return hashlib.md5(open(path, "rb").read()).hexdigest()


ok = True
def check(name, cond, detail=""):
    global ok
    print(("  PASS  " if cond else "  FAIL  ") + name + (("  — " + detail) if (detail and not cond) else ""))
    ok = ok and cond


def main():
    if not os.path.exists(HOST):
        raise SystemExit("host binary missing — run ./build.sh first")
    tmp = tempfile.mkdtemp(prefix="widthtest_")
    print(f"stage-width test (artifacts in {tmp})\n")
    rounds = 6
    exp = analytic(rounds)

    # [1] word-count: width:1 baseline vs filter×3 + transform×2
    print("[1] word-count pipeline — width:N == width:1 == analytic")
    base = os.path.join(tmp, "wc_base.txt")
    wide = os.path.join(tmp, "wc_wide.txt")
    run_dag(wc_dag("/dev/shm/wt_base", rounds, base, 1, 1), tmp, "wc_base")
    run_dag(wc_dag("/dev/shm/wt_wide", rounds, wide, 3, 2), tmp, "wc_wide")
    b, w = summaries(base), summaries(wide)
    check("width:1 baseline matches analytic", b == exp, f"{b} != {exp}")
    check("filter×3 + transform×2 == width:1 baseline", w == b, f"{w} != {b}")
    check("widened result == analytic", w == exp, f"{w} != {exp}")

    # determinism of the widened run
    seen = set()
    for i in range(3):
        o = os.path.join(tmp, f"wc_det{i}.txt")
        run_dag(wc_dag(f"/dev/shm/wt_det{i}", rounds, o, 3, 2), tmp, f"wc_det{i}")
        seen.add(tuple(summaries(o)))
    check("widened run deterministic across 3 runs", len(seen) == 1)

    # [2] image pipeline: widening a 1-in-1-out stage stays byte-identical
    print("\n[2] image pipeline — widened stage == width:1, byte-identical")
    base_imgs = [os.path.join(tmp, f"imgb_{i}.pgm") for i in range(3)]
    wide_imgs = [os.path.join(tmp, f"imgw_{i}.pgm") for i in range(3)]
    run_dag(img_dag("/dev/shm/wt_imgb", base_imgs, None, 1), tmp, "imgb")
    run_dag(img_dag("/dev/shm/wt_imgw", wide_imgs, "img_blur", 2), tmp, "imgw")
    for i in range(3):
        bm, wm = md5(base_imgs[i]), md5(wide_imgs[i])
        check(f"image {i}: widened blur == width:1 ({bm[:8]})", bm == wm, f"{bm} != {wm}")

    # [3] dynamic autoscaling: width tracks time-varying load, result unchanged
    print("\n[3] dynamic autoscaling — width tracks load, result == width:1")
    vr = 14  # two cycles of the 7-round burst pattern
    vbase = os.path.join(tmp, "var_base.txt")
    vdyn = os.path.join(tmp, "var_dyn.txt")
    run_dag(wc_var_dag("/dev/shm/wt_vbase", vr, vbase, None), tmp, "var_base")
    out = run_dag(wc_var_dag("/dev/shm/wt_vdyn", vr, vdyn, 6), tmp, "var_dyn")
    check("dynamic-width result == width:1 baseline", summaries(vdyn) == summaries(vbase),
          f"{summaries(vdyn)} != {summaries(vbase)}")
    m = re.search(r"pipeline_filter.*?peak (\d+)", out)
    peak = int(m.group(1)) if m else 0
    check(f"filter autoscaled above width 1 under load (peak={peak})", peak > 1,
          "autoscaler never widened — check load profile in stdout")

    print("\n=== " + ("ALL PASS" if ok else "FAILED") + " ===")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
