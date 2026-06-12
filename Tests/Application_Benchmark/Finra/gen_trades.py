#!/usr/bin/env python3
# gen_trades.py — deterministic trade generator for the FINRA benchmark.
#
# Produces N trades in the exact CSV shape the workload parses
# (`trade_id,symbol,price,quantity,side,time,account`), with field
# distributions that exercise all 8 audit rules at stable rates, so the total
# violation count is reproducible and identical across every system (the
# correctness gate). Fixed RNG seed → byte-identical output for a given N.
#
# Usage: gen_trades.py N OUT_PATH [SEED]
import random
import sys

# The 20 reference symbols (must match finra.rs `finra_fetch_public`); the
# ref avg price (cents) drives the price-outlier rule.
REF = {
    'AAPL': 17500, 'GOOG': 14000, 'MSFT': 37000, 'AMZN': 18000, 'META': 50000,
    'TSLA': 25000, 'NVDA': 80000, 'JPM': 19500, 'BAC': 3500, 'WFC': 5500,
    'GS': 40000, 'MS': 9000, 'V': 28000, 'MA': 45000, 'NFLX': 60000,
    'DIS': 11000, 'INTC': 4500, 'AMD': 16000, 'ORCL': 12500, 'CRM': 30000,
}
SYMBOLS = list(REF)


def main():
    n = int(sys.argv[1])
    out = sys.argv[2]
    seed = int(sys.argv[3]) if len(sys.argv) > 3 else 42
    rng = random.Random(seed)

    # ~100 accounts so per-(account,symbol) rules (wash, spoofing) trigger.
    accounts = ['ACC%03d' % i for i in range(100)]

    lines = ['trade_id,symbol,price,quantity,side,time,account']
    for tid in range(1, n + 1):
        sym = rng.choice(SYMBOLS)
        avg = REF[sym]

        # price (cents): mostly near the ref avg; some outliers / penny stocks.
        r = rng.random()
        if r < 0.04:                       # >2x avg → price-outlier (rule 0)
            cents = int(avg * rng.uniform(2.1, 3.0))
        elif r < 0.08:                     # < $5 → penny stock (rule 6)
            cents = rng.randint(50, 499)
        else:
            cents = max(1, int(avg * rng.uniform(0.7, 1.3)))
        price = '%d.%02d' % (cents // 100, cents % 100)

        # quantity: some > 5000 (rule 1); some exact 1000-multiples (rule 7).
        rq = rng.random()
        if rq < 0.05:
            qty = rng.randint(5001, 20000)
        elif rq < 0.20:
            qty = rng.randint(1, 20) * 1000
        else:
            qty = rng.randint(1, 4999)

        side = 'BUY' if rng.random() < 0.5 else 'SELL'

        # time: mostly regular hours (09:30–16:00); ~6% after-hours (rule 5).
        if rng.random() < 0.06:
            hour = rng.choice([4, 5, 6, 7, 8, 17, 18, 19, 20])
        else:
            hour = rng.randint(9, 15)
        minute = rng.randint(0, 59)
        if hour == 9:
            minute = rng.randint(30, 59)
        tstr = '%02d:%02d' % (hour, minute)

        acct = rng.choice(accounts)
        lines.append('%d,%s,%s,%d,%s,%s,%s' % (tid, sym, price, qty, side, tstr, acct))

    with open(out, 'w') as f:
        f.write('\n'.join(lines) + '\n')
    print('wrote %s (%d trades)' % (out, n))


if __name__ == '__main__':
    main()
