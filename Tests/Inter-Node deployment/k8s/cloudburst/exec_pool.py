#!/usr/bin/env python3
# exec_pool.py — executor-pool lifecycle helper for the Cloudburst drivers' COLD-START reps.
#
# The cold-start case does NOT pre-provision executors: the pool sits at 0 and the driver
# launches it WHEN THE JOB REACHES THE MAP WAVE (launch_on_wave), so pod scheduling+startup
# is on the job's critical path and folds into the cold makespan (the user's "cold makespan,
# launch included"). Warm reps reuse the pods left up after the launch. Placement stays
# 16/node — a fresh scale 0->n spreads evenly under the deployment's DoNotSchedule maxSkew-1
# topologySpread, and node-0 is excluded by its control-plane nodeAffinity.
#
# Runs on node-0, which has kubectl for the baselines namespace.
import subprocess
import time

NS, DEP = "baselines", "cloudburst-executor"


def _sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True)


def scale(n):
    _sh("kubectl -n %s scale deploy/%s --replicas=%d" % (NS, DEP, n))


def running():
    return int(_sh("kubectl -n %s get pods -l app=%s --no-headers 2>/dev/null "
                   "| grep -c ' Running '" % (NS, DEP)).stdout.strip() or 0)


def total():
    return int(_sh("kubectl -n %s get pods -l app=%s --no-headers 2>/dev/null "
                   "| wc -l" % (NS, DEP)).stdout.strip() or 0)


def wait_running(n, timeout=240):
    s = time.time()
    while time.time() - s < timeout:
        if running() >= n:
            return True
        time.sleep(1)
    return False


def wait_gone(timeout=180):
    s = time.time()
    while time.time() - s < timeout:
        if total() == 0:
            return True
        time.sleep(1)
    return False


def teardown():
    """Scale to 0 and wait until no pods remain — so the next cold rep starts truly cold."""
    scale(0)
    wait_gone()


def launch_on_wave(n, timeout=240):
    """Scale 0->n and block until all n are Running. Returns elapsed ms (the cold-start
    latency the caller folds into the cold makespan by timing across this call)."""
    t0 = time.time()
    scale(n)
    if not wait_running(n, timeout):
        raise RuntimeError("exec pool: only %d/%d Running after %ds" % (running(), n, timeout))
    return (time.time() - t0) * 1000.0
