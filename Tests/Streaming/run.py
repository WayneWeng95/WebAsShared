#!/usr/bin/env python3
"""Single-node streaming-pipeline test pass (StreamPipeline / Rust).

Covers the dimensions tracked in problems.md item #2:

  1. Per-round correctness — the software-pipelined output matches a
     deterministic analytic baseline AND a non-pipelined baseline (the same
     stages run serially via WasmGrouping).
  2. Determinism — repeated runs of the pipelined DAG are identical (guards the
     producer/consumer race that used to scramble per-round batching).
  3. Slot lifecycle across ticks — exactly N summary records, each with
     batch_count == 10, proves every stage consumed exactly one round per tick.
  4. Reset / multi-run — `mode:"reset"` with runs:K reproduces the same per-run
     result (cursors + read watermarks reset at pipeline start, slots freed).
  5. Output split_records — a pipeline emitting one I/O record per round fans
     record i -> paths[i].

The pipeline is fully deterministic:
  source round r  -> 20 records, value = r*1000 + i  (i in 0..20)
  filter          -> keeps even item index           (10 records / round)
  transform       -> appends "|T"
  sink            -> per-round summary "batch_count=10,value_sum=10000*r+90"

Run:  python3 Tests/Streaming/run.py
Exit: 0 = all pass, 1 = failure.
"""

import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
HOST = os.path.join(ROOT, "Executor", "target", "release", "host")
WASM = os.path.join(ROOT, "Executor", "target", "wasm32-unknown-unknown", "release", "guest.wasm")
PYRUNNER = os.path.join(ROOT, "Executor", "py_guest", "python", "runner.py")

SUMMARY_RE = re.compile(r"batch_count=(\d+),value_sum=(\d+)")


# ── expected (analytic) per-round summary ────────────────────────────────────
def analytic(rounds):
    """Ground-truth (batch_count, value_sum) for each round."""
    # filter keeps even item indices 0,2,..,18 (10 of them); value = r*1000 + i
    return [(10, sum(r * 1000 + i for i in range(0, 20, 2))) for r in range(rounds)]


# ── DAG builders ─────────────────────────────────────────────────────────────
def pipelined_dag(shm, rounds, out_path):
    return {
        "shm_path": shm, "wasm_path": WASM, "log_level": "error",
        "nodes": [
            {"id": "sp", "deps": [], "kind": {"StreamPipeline": {"rounds": rounds, "stages": [
                {"func": "pipeline_source", "arg0": 200, "arg1": None},
                {"func": "pipeline_filter", "arg0": 200, "arg1": 201},
                {"func": "pipeline_transform", "arg0": 201, "arg1": 202},
                {"func": "pipeline_sink", "arg0": 202, "arg1": 203},
            ]}}},
            {"id": "w", "deps": ["sp"], "kind": {"Watch": {"stream": 203, "output": out_path}}},
        ],
    }


def baseline_grouping_dag(shm, rounds, out_path):
    """Non-pipelined baseline: the same stages run strictly serially, one full
    round at a time, via WasmGrouping (no cross-round overlap)."""
    stages = []
    for r in range(rounds):
        stages += [
            {"func": "pipeline_source", "arg0": 200, "arg1": r},
            {"func": "pipeline_filter", "arg0": 200, "arg1": 201},
            {"func": "pipeline_transform", "arg0": 201, "arg1": 202},
            {"func": "pipeline_sink", "arg0": 202, "arg1": 203},
        ]
    return {
        "shm_path": shm, "wasm_path": WASM, "log_level": "error",
        "nodes": [
            {"id": "g", "deps": [], "kind": {"WasmGrouping": {"stages": stages}}},
            {"id": "w", "deps": ["g"], "kind": {"Watch": {"stream": 203, "output": out_path}}},
        ],
    }


def reset_dag(shm, rounds, runs, out_path):
    return {
        "shm_path": shm, "wasm_path": WASM, "log_level": "error",
        "mode": "reset", "runs": runs,
        "nodes": [
            {"id": "free", "deps": [], "kind": {"FreeSlots": {"stream": [200, 201, 202, 203]}}},
            {"id": "sp", "deps": ["free"], "kind": {"StreamPipeline": {"rounds": rounds, "stages": [
                {"func": "pipeline_source", "arg0": 200, "arg1": None},
                {"func": "pipeline_filter", "arg0": 200, "arg1": 201},
                {"func": "pipeline_transform", "arg0": 201, "arg1": 202},
                {"func": "pipeline_sink", "arg0": 202, "arg1": 203},
            ]}}},
            {"id": "w", "deps": ["sp"], "kind": {"Watch": {"stream": 203, "output": out_path}}},
        ],
    }


def pypipeline_dag(shm, rounds, out_path):
    return {
        "shm_path": shm, "wasm_path": WASM, "python_script": PYRUNNER, "log_level": "error",
        "nodes": [
            {"id": "pp", "deps": [], "kind": {"PyPipeline": {"rounds": rounds, "stages": [
                {"func": "pyp_source", "arg": 200, "arg2": None},
                {"func": "pyp_filter", "arg": 200, "arg2": 201},
                {"func": "pyp_transform", "arg": 201, "arg2": 202},
                {"func": "pyp_sink", "arg": 202, "arg2": 203},
            ]}}},
            {"id": "w", "deps": ["pp"], "kind": {"Watch": {"stream": 203, "output": out_path}}},
        ],
    }


def pygrouping_baseline_dag(shm, rounds, out_path):
    """Non-pipelined Python baseline: same stages run serially via PyGrouping."""
    stages = []
    for r in range(rounds):
        stages += [
            {"func": "pyp_source", "arg": 200, "arg2": r},
            {"func": "pyp_filter", "arg": 200, "arg2": 201},
            {"func": "pyp_transform", "arg": 201, "arg2": 202},
            {"func": "pyp_sink", "arg": 202, "arg2": 203},
        ]
    return {
        "shm_path": shm, "wasm_path": WASM, "python_script": PYRUNNER, "log_level": "error",
        "nodes": [
            {"id": "g", "deps": [], "kind": {"PyGrouping": {"stages": stages}}},
            {"id": "w", "deps": ["g"], "kind": {"Watch": {"stream": 203, "output": out_path}}},
        ],
    }


def pyreset_dag(shm, rounds, runs, out_path):
    return {
        "shm_path": shm, "wasm_path": WASM, "python_script": PYRUNNER, "log_level": "error",
        "mode": "reset", "runs": runs,
        "nodes": [
            {"id": "free", "deps": [], "kind": {"FreeSlots": {"stream": [200, 201, 202, 203]}}},
            {"id": "pp", "deps": ["free"], "kind": {"PyPipeline": {"rounds": rounds, "stages": [
                {"func": "pyp_source", "arg": 200, "arg2": None},
                {"func": "pyp_filter", "arg": 200, "arg2": 201},
                {"func": "pyp_transform", "arg": 201, "arg2": 202},
                {"func": "pyp_sink", "arg": 202, "arg2": 203},
            ]}}},
            {"id": "w", "deps": ["pp"], "kind": {"Watch": {"stream": 203, "output": out_path}}},
        ],
    }


def split_dag(shm, rounds, paths):
    return {
        "shm_path": shm, "wasm_path": WASM, "log_level": "error",
        "nodes": [
            {"id": "sp", "deps": [], "kind": {"StreamPipeline": {"rounds": rounds, "stages": [
                {"func": "pipeline_source", "arg0": 200, "arg1": None},
                {"func": "pipeline_filter", "arg0": 200, "arg1": 201},
                {"func": "pipeline_transform", "arg0": 201, "arg1": 202},
                {"func": "pipeline_io_sink", "arg0": 202, "arg1": 1},
            ]}}},
            {"id": "out", "deps": ["sp"], "kind": {"Output": {"slot": 1, "split_records": True, "paths": paths}}},
        ],
    }


# Real per-round image pipeline (load->rotate->grayscale->equalize->blur->export)
# emitting one image per round and fanning each round's image to its own file via
# split_records.  The image stages read one record per round (read_next), so this
# exercises a different consumer pattern than the deterministic stream pipeline.
IMG_STAGES = [
    {"func": "img_load_ppm", "a": 10, "b": 20}, {"func": "img_rotate", "a": 20, "b": 30},
    {"func": "img_grayscale", "a": 30, "b": 40}, {"func": "img_equalize", "a": 40, "b": 50},
    {"func": "img_blur", "a": 50, "b": 60}, {"func": "img_export_ppm", "a": 60, "b": 1},
]
IMG_FILES = ["img_gradient.ppm", "img_checkerboard.ppm", "img_rings.ppm"]


def _img_input_node():
    paths = [os.path.join(ROOT, "TestData", f) for f in IMG_FILES]
    return {"id": "load", "deps": [], "kind": {"Input": {"paths": paths, "slot": 10, "binary": True}}}


def img_pipelined_dag(shm, out_paths):
    stages = [{"func": s["func"], "arg0": s["a"], "arg1": s["b"]} for s in IMG_STAGES]
    return {
        "shm_path": shm, "wasm_path": WASM, "log_level": "error",
        "nodes": [
            _img_input_node(),
            {"id": "pp", "deps": ["load"], "kind": {"StreamPipeline": {"rounds": 3, "stages": stages}}},
            {"id": "save", "deps": ["pp"], "kind": {"Output": {"slot": 1, "split_records": True, "paths": out_paths}}},
        ],
    }


def img_serial_dag(shm, out_paths):
    stages = []
    for _ in range(3):
        stages += [{"func": s["func"], "arg0": s["a"], "arg1": s["b"]} for s in IMG_STAGES]
    return {
        "shm_path": shm, "wasm_path": WASM, "log_level": "error",
        "nodes": [
            _img_input_node(),
            {"id": "g", "deps": ["load"], "kind": {"WasmGrouping": {"stages": stages}}},
            {"id": "save", "deps": ["g"], "kind": {"Output": {"slot": 1, "split_records": True, "paths": out_paths}}},
        ],
    }


def _md5(path):
    return hashlib.md5(open(path, "rb").read()).hexdigest()


# ── runner / parsing ─────────────────────────────────────────────────────────
def run_dag(dag, tmp, tag, capture=False):
    shm = dag["shm_path"]
    for ext in ("", "_meta"):
        try:
            os.remove(shm + ext)
        except OSError:
            pass
    path = os.path.join(tmp, tag + ".json")
    with open(path, "w") as f:
        json.dump(dag, f)
    res = subprocess.run([HOST, "dag", path], capture_output=True, text=True, cwd=ROOT)
    if res.returncode != 0:
        raise RuntimeError(f"{tag}: host exited {res.returncode}\n{res.stderr[-2000:]}")
    return res.stdout if capture else None


def parse_summaries(text):
    return [(int(a), int(b)) for a, b in SUMMARY_RE.findall(text)]


def _have_python3():
    try:
        subprocess.run(["python3", "--version"], capture_output=True, check=True)
        return True
    except (OSError, subprocess.CalledProcessError):
        return False


# ── test harness ─────────────────────────────────────────────────────────────
class Results:
    def __init__(self):
        self.passed = 0
        self.failed = 0

    def check(self, name, cond, detail=""):
        if cond:
            self.passed += 1
            print(f"  PASS  {name}")
        else:
            self.failed += 1
            print(f"  FAIL  {name}{('  — ' + detail) if detail else ''}")


def main():
    if not (os.path.exists(HOST) and os.path.exists(WASM)):
        print(f"ERROR: build artifacts missing. Run ./build.sh first.\n  host: {HOST}\n  wasm: {WASM}")
        return 2

    R = Results()
    tmp = tempfile.mkdtemp(prefix="streamtest_")
    print(f"streaming test pass (artifacts in {tmp})\n")

    # 1+2+3: per-round correctness, determinism, lifecycle (exactly N summaries).
    print("[1] per-round correctness + determinism + slot lifecycle")
    for rounds in (1, 2, 3, 5, 8):
        exp = analytic(rounds)
        out = os.path.join(tmp, f"pipe_{rounds}.txt")
        seen = set()
        for rep in range(3):
            run_dag(pipelined_dag(f"/dev/shm/st_pipe_{rounds}", rounds, out), tmp, f"pipe_{rounds}")
            with open(out) as f:
                seen.add(tuple(parse_summaries(f.read())))
        got = next(iter(seen))
        R.check(f"rounds={rounds}: deterministic across 3 runs", len(seen) == 1)
        R.check(f"rounds={rounds}: exactly {rounds} summaries (one per round)", len(got) == rounds,
                f"got {len(got)}")
        R.check(f"rounds={rounds}: per-round values match analytic baseline", got == tuple(exp),
                f"{got} != {exp}")

    # 1b: non-pipelined baseline equality.
    print("\n[2] pipelined output == non-pipelined (WasmGrouping) baseline")
    for rounds in (3, 5, 8):
        po = os.path.join(tmp, f"p_{rounds}.txt")
        bo = os.path.join(tmp, f"b_{rounds}.txt")
        run_dag(pipelined_dag(f"/dev/shm/st_p_{rounds}", rounds, po), tmp, f"p_{rounds}")
        run_dag(baseline_grouping_dag(f"/dev/shm/st_b_{rounds}", rounds, bo), tmp, f"b_{rounds}")
        with open(po) as f:
            p = parse_summaries(f.read())
        with open(bo) as f:
            b = parse_summaries(f.read())
        R.check(f"rounds={rounds}: pipelined == serial baseline", p == b, f"{p} != {b}")

    # 4: reset / multi-run.
    print("\n[3] reset / multi-run produces identical per-run results")
    rounds, runs = 4, 3
    out = os.path.join(tmp, "reset.txt")
    log = run_dag(reset_dag("/dev/shm/st_reset", rounds, runs, out), tmp, "reset", capture=True)
    # The "[dump]"-free reset run writes Watch each iteration (file overwritten);
    # assert the engine ran all K runs and the final file is exactly correct.
    nruns = len(re.findall(r"All nodes completed \(run #", log))
    R.check(f"reset ran {runs} runs", nruns == runs, f"ran {nruns}")
    with open(out) as f:
        got = parse_summaries(f.read())
    R.check("reset final run matches analytic baseline", got == analytic(rounds), str(got))

    # 5: Output split_records.
    print("\n[4] Output split_records — one file per round")
    rounds = 4
    sd = os.path.join(tmp, "split")
    os.makedirs(sd, exist_ok=True)
    paths = [os.path.join(sd, f"round{r}.txt") for r in range(rounds)]
    run_dag(split_dag("/dev/shm/st_split", rounds, paths), tmp, "split")
    ok = True
    detail = ""
    for r, p in enumerate(paths):
        if not os.path.exists(p):
            ok, detail = False, f"missing {p}"
            break
        content = open(p).read()
        exp_val = analytic(rounds)[r][1]
        if f"value_sum={exp_val}" not in content:
            ok, detail = False, f"round{r}: {content!r} lacks value_sum={exp_val}"
            break
    R.check(f"split_records wrote {rounds} per-round files with correct content", ok, detail)

    # 6: Python PyPipeline parity (skipped gracefully if python3 unavailable).
    print("\n[5] PyPipeline (Python) correctness, determinism + Rust parity")
    if not _have_python3():
        R.check("PyPipeline tests", True, "")
        print("  SKIP  python3 not available")
    else:
        for rounds in (1, 3, 5):
            exp = analytic(rounds)
            out = os.path.join(tmp, f"pyp_{rounds}.txt")
            seen = set()
            for rep in range(3):
                run_dag(pypipeline_dag(f"/dev/shm/st_pyp_{rounds}", rounds, out), tmp, f"pyp_{rounds}")
                with open(out) as f:
                    seen.add(tuple(parse_summaries(f.read())))
            got = next(iter(seen))
            R.check(f"py rounds={rounds}: deterministic across 3 runs", len(seen) == 1)
            R.check(f"py rounds={rounds}: per-round values match analytic baseline",
                    got == tuple(exp), f"{got} != {exp}")
        # Python pipelined == Python serial (PyGrouping) baseline.
        po = os.path.join(tmp, "pyp_b_p.txt")
        bo = os.path.join(tmp, "pyp_b_b.txt")
        run_dag(pypipeline_dag("/dev/shm/st_pypp", 5, po), tmp, "pypp")
        run_dag(pygrouping_baseline_dag("/dev/shm/st_pypg", 5, bo), tmp, "pypg")
        with open(po) as f:
            p = parse_summaries(f.read())
        with open(bo) as f:
            b = parse_summaries(f.read())
        R.check("py pipelined == py serial (PyGrouping) baseline", p == b, f"{p} != {b}")
        R.check("py output == rust analytic baseline", p == analytic(5), str(p))
        # reset / multi-run.
        out = os.path.join(tmp, "pyreset.txt")
        log = run_dag(pyreset_dag("/dev/shm/st_pyreset", 4, 3, out), tmp, "pyreset", capture=True)
        nruns = len(re.findall(r"All nodes completed \(run #", log))
        R.check("py reset ran 3 runs", nruns == 3, f"ran {nruns}")
        with open(out) as f:
            got = parse_summaries(f.read())
        R.check("py reset final run matches analytic baseline", got == analytic(4), str(got))

    # 7: real image pipeline — pipelined == non-pipelined baseline, per-round split.
    print("\n[6] image StreamPipeline — pipelined == serial baseline, split_records")
    if not all(os.path.exists(os.path.join(ROOT, "TestData", f)) for f in IMG_FILES):
        R.check("image pipeline tests", True, "")
        print("  SKIP  TestData images missing")
    else:
        pd = os.path.join(tmp, "imgp")
        sd = os.path.join(tmp, "imgs")
        os.makedirs(pd, exist_ok=True)
        os.makedirs(sd, exist_ok=True)
        ppaths = [os.path.join(pd, f"r{i}.pgm") for i in range(3)]
        spaths = [os.path.join(sd, f"r{i}.pgm") for i in range(3)]
        run_dag(img_pipelined_dag("/dev/shm/st_imgp", ppaths), tmp, "imgp")
        run_dag(img_serial_dag("/dev/shm/st_imgs", spaths), tmp, "imgs")
        wrote = all(os.path.exists(p) for p in ppaths)
        R.check("image pipeline wrote 3 per-round files", wrote)
        if wrote:
            ph = [_md5(p) for p in ppaths]
            sh = [_md5(p) for p in spaths]
            R.check("image pipelined == serial baseline (per round)", ph == sh, f"{ph} != {sh}")
            valid = all(open(p, "rb").read(2) == b"P5" for p in ppaths)
            R.check("image outputs are valid PGM (P5)", valid)

    print(f"\n=== {R.passed} passed, {R.failed} failed ===")
    return 0 if R.failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
