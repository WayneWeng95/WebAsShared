#!/usr/bin/env python3
# gen_events.py — deterministic event-stream generator for the RTSFaaS-derived
# streaming workloads (MediaReview / SocialNetwork), re-implemented on our model.
#
# Emits one CSV event per line: "<op>,<key>[,<val>]" — consumed by the guest
# parse stage (media_review.rs / social_network.rs), which partitions BY KEY.
#
# Deterministic (fixed seed) so the run summary is an exact correctness gate:
#   login events carry the correct password "pw<key>", so login_ok == #login.
#
# Knobs mirror the RTSFaaS Env/*.env spec:
#   --events N           number of events
#   --users U            key space / seed-table size
#   --skew S             P(hot key) in [0,1]; hot set = first 10% of keys
#                        (stateAccessSkewnessForEvents)
#   --multipartition M   fraction in [0,1] tagged cross-partition; recorded for
#                        the sweep, no effect on single-node correctness — it
#                        drives the cross-node RDMA variant (TODO)
#
# Usage:
#   gen_events.py mediareview   --events 100000 --users 10000 --skew 0.0 > ev.csv
#   gen_events.py socialnetwork --events 100000 --users 10000 --skew 1.0 > ev.csv
import argparse
import random
import sys

# Event mixes (cumulative). MediaReview: login 20% / rate 40% / review 40%
# (Benchmarks/RTSFaaS WORKLOAD_SELECTION.md). SocialNetwork: even 4-way by
# default (the shipped Env drives postTweet only; we exercise all four).
MIXES = {
    "mediareview":   [("login", 0.2), ("rate", 0.4), ("review", 0.4)],
    "socialnetwork": [("login", 0.25), ("profile", 0.25),
                      ("timeline", 0.25), ("post", 0.25)],
}


def pick(rng, mix):
    r = rng.random()
    acc = 0.0
    for op, w in mix:
        acc += w
        if r < acc:
            return op
    return mix[-1][0]


def pick_key(rng, users, skew):
    hot = max(1, users // 10)
    if rng.random() < skew:
        return rng.randrange(hot)
    return rng.randrange(users)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("workload", choices=list(MIXES))
    ap.add_argument("--events", type=int, default=100000)
    ap.add_argument("--users", type=int, default=10000)
    ap.add_argument("--skew", type=float, default=0.0)
    ap.add_argument("--multipartition", type=float, default=0.0)
    ap.add_argument("--seed", type=int, default=42)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    mix = MIXES[args.workload]
    # timeline/post touch key & key+1, so keep keys in [0, users-1).
    kspace = max(1, args.users - 1)

    out = sys.stdout
    out.write(f"# {args.workload} events={args.events} users={args.users} "
              f"skew={args.skew} multipartition={args.multipartition}\n")
    for _ in range(args.events):
        op = pick(rng, mix)
        key = pick_key(rng, kspace, args.skew)
        if op == "login":
            out.write(f"login,{key},pw{key}\n")
        elif op == "rate":
            out.write(f"rate,{key},r{rng.randrange(5) + 1}\n")
        elif op == "review":
            out.write(f"review,{key},rev{key}\n")
        elif op == "profile":
            out.write(f"profile,{key}\n")
        elif op == "timeline":
            out.write(f"timeline,{key}\n")
        elif op == "post":
            out.write(f"post,{key},tw{key}\n")


if __name__ == "__main__":
    main()
