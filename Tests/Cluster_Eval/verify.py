#!/usr/bin/env python3
"""Compare two word-count result files for identical content (order-independent).

Usage:  python3 Tests/Cluster_Eval/verify.py <baseline.txt> <result.txt>
Exit 0 = identical (distributed run matches the single-node baseline).
"""
import sys

def norm(p):
    return sorted(l.rstrip("\n") for l in open(p) if l.strip())

def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: verify.py <baseline.txt> <result.txt>")
    a, b = norm(sys.argv[1]), norm(sys.argv[2])
    if a == b:
        print(f"PASS — identical ({len(a)} lines)"); sys.exit(0)
    only_a = [l for l in a if l not in set(b)]
    only_b = [l for l in b if l not in set(a)]
    print(f"FAIL — baseline-only: {only_a[:5]}\n       result-only:   {only_b[:5]}")
    sys.exit(1)

if __name__ == "__main__":
    main()
