"""Entry point for Python workloads.

The host spawns this script (via python.wasm or native python3) with the
following environment variables:

  SHM_PATH        — absolute path to the SHM file (e.g. /dev/shm/my_dag)
  WORKLOAD_FUNC   — name of the workload function to call
  WORKLOAD_ARG    — first argument (u32, defaults to 0)
  WORKLOAD_ARG2   — second argument (u32, optional; used by two-arg workloads)
"""

import importlib.util
import os
import sys

# ── Resolve the workloads module ──────────────────────────────────────────────

_script_dir = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, _script_dir)

import workloads  # noqa: E402  (placed after sys.path update)

# ── Read configuration from env ───────────────────────────────────────────────

func_name = os.environ.get("WORKLOAD_FUNC", "")
if not func_name:
    sys.exit("[runner] WORKLOAD_FUNC not set")

arg  = int(os.environ.get("WORKLOAD_ARG",  "0"))
arg2 = os.environ.get("WORKLOAD_ARG2")

fn = getattr(workloads, func_name, None)
if fn is None:
    sys.exit(f"[runner] unknown function: {func_name!r}")

# ── Dispatch ──────────────────────────────────────────────────────────────────

if arg2 is not None:
    fn(arg, int(arg2))
else:
    fn(arg)
