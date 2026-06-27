# finra_rules.py — the frozen 8-rule FINRA audit spec, in Python (shared by the Cloudburst
# driver + executor and the RMMap-RDMA harness). A faithful copy of the intra-node
# Intra-Node Application_Benchmark/Finra/finra_rules.py (itself a port of finra.rs), so every
# Python-based baseline computes the SAME violations as the wasm bars — gate 2,271,415.
#
# The ONLY addition vs the intra-node file is the `skip_header` flag on parse_trades: the
# RMMap-RDMA workers read raw newline-aligned byte SLICES of the corpus, where only slice 0
# carries the header — interior slices start on a real trade and must NOT drop their first
# line. Cloudburst prepends the header to every shard (faasm-style) so it uses the default
# skip_header=True. The per-line parse + the 8 rule bodies are byte-for-byte the intra file.

REF_TABLE = {
    'AAPL': 17500, 'GOOG': 14000, 'MSFT': 37000, 'AMZN': 18000, 'META': 50000,
    'TSLA': 25000, 'NVDA': 80000, 'JPM': 19500, 'BAC': 3500, 'WFC': 5500,
    'GS': 40000, 'MS': 9000, 'V': 28000, 'MA': 45000, 'NFLX': 60000,
    'DIS': 11000, 'INTC': 4500, 'AMD': 16000, 'ORCL': 12500, 'CRM': 30000,
}

RULE_NAMES = ['PRICE_OUTLIER', 'LARGE_ORDER', 'WASH_TRADE', 'SPOOFING',
              'CONCENTRATION', 'AFTER_HOURS', 'PENNY_STOCK', 'ROUND_LOT']

STATELESS = (0, 1, 5, 6, 7)   # per-record rules — shardable over disjoint trade slices
STATEFUL = (2, 3, 4)          # WASH / SPOOFING / CONCENTRATION — need all records


def _cents(s):
    whole, _, frac = s.partition('.')
    c = int(whole or 0) * 100
    if frac:
        c += int(frac[:2]) if len(frac) >= 2 else int(frac) * 10
    return c


def parse_trades(csv_text, skip_header=True):
    """Parse the trades CSV into trade dicts, matching finra.rs. `skip_header` drops line 0
    (a real header); set False for an interior byte-slice whose first line is a trade."""
    if isinstance(csv_text, (bytes, bytearray)):
        csv_text = csv_text.decode('utf-8', 'ignore')
    trades = []
    for i, line in enumerate(csv_text.split('\n')):
        line = line.strip()
        if not line or (skip_header and i == 0):
            continue
        p = line.split(',')
        if len(p) < 7:
            continue
        h, _, m = p[5].partition(':')
        trades.append({
            'symbol': p[1],
            'price': _cents(p[2]),
            'quantity': int(p[3] or 0),
            'side': 0 if p[4] == 'BUY' else 1,
            'minutes': int(h or 0) * 60 + int(m or 0),
            'account': p[6],
        })
    return trades


def audit_rule(trades, rule_id):
    """Violation count for `rule_id` over `trades` — mirrors finra.rs exactly."""
    v = 0
    if rule_id == 0:                     # PRICE_OUTLIER: price > 2× ref avg
        for t in trades:
            if t['price'] > 2 * REF_TABLE.get(t['symbol'], 10000):
                v += 1
    elif rule_id == 1:                   # LARGE_ORDER: quantity > 5000
        v = sum(1 for t in trades if t['quantity'] > 5000)
    elif rule_id == 2:                   # WASH_TRADE: same acct+sym both BUY+SELL
        mask = {}
        for t in trades:
            k = (t['account'], t['symbol'])
            mask[k] = mask.get(k, 0) | (1 << t['side'])
        v = sum(1 for m in mask.values() if m == 3)
    elif rule_id == 3:                   # SPOOFING: >10 trades same acct+sym
        cnt = {}
        for t in trades:
            k = (t['account'], t['symbol'])
            cnt[k] = cnt.get(k, 0) + 1
        v = sum(1 for n in cnt.values() if n > 10)
    elif rule_id == 4:                   # CONCENTRATION: sym count > total/20 + 100
        cnt = {}
        for t in trades:
            cnt[t['symbol']] = cnt.get(t['symbol'], 0) + 1
        threshold = len(trades) // 20 + 100
        v = sum(1 for n in cnt.values() if n > threshold)
    elif rule_id == 5:                   # AFTER_HOURS: outside 09:30-16:00
        v = sum(1 for t in trades if t['minutes'] < 570 or t['minutes'] > 960)
    elif rule_id == 6:                   # PENNY_STOCK: price < $5.00
        v = sum(1 for t in trades if t['price'] < 500)
    elif rule_id == 7:                   # ROUND_LOT: qty >= 1000 and a 1000-multiple
        v = sum(1 for t in trades if t['quantity'] >= 1000 and t['quantity'] % 1000 == 0)
    return v
