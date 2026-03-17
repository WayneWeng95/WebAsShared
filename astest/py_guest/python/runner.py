"""Entry point for Python workloads.

Two modes:

  One-shot mode (default):
    Reads SHM_PATH, WORKLOAD_FUNC, WORKLOAD_ARG[, WORKLOAD_ARG2] from env.
    Calls the function once and exits.

  Loop mode (--loop argument):
    Reads SHM_PATH from env.
    Reads "func [arg [arg2]]\\n" lines from stdin.
    Writes "ok\\n" or "err: ...\\n" to stdout after each call.
    Exits when stdin closes (EOF).
    Used by PyPipeline to reuse one Python process across all pipeline stages.
"""

import os
import sys

_script_dir = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, _script_dir)

import word_count    # noqa: E402
import image_process  # noqa: E402


def _lookup(name):
    """Find a workload function by name, searching word_count then image_process."""
    return getattr(word_count, name, None) or getattr(image_process, name, None)


if len(sys.argv) > 1 and sys.argv[1] == "--loop":
    # ── Loop mode ──────────────────────────────────────────────────────────────
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        parts = line.split()
        func_name = parts[0]
        arg  = int(parts[1]) if len(parts) > 1 else 0
        arg2 = int(parts[2]) if len(parts) > 2 else None
        fn = _lookup(func_name)
        if fn is None:
            sys.stdout.write(f"err: unknown function {func_name!r}\n")
        else:
            try:
                fn(arg) if arg2 is None else fn(arg, arg2)
                sys.stdout.write("ok\n")
            except Exception as e:
                sys.stdout.write(f"err: {e}\n")
        sys.stdout.flush()

else:
    # ── One-shot mode ──────────────────────────────────────────────────────────
    func_name = os.environ.get("WORKLOAD_FUNC", "")
    if not func_name:
        sys.exit("[runner] WORKLOAD_FUNC not set")

    arg  = int(os.environ.get("WORKLOAD_ARG",  "0"))
    arg2 = os.environ.get("WORKLOAD_ARG2")

    fn = _lookup(func_name)
    if fn is None:
        sys.exit(f"[runner] unknown function: {func_name!r}")

    if arg2 is not None:
        fn(arg, int(arg2))
    else:
        fn(arg)
