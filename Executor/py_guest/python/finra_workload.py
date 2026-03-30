"""FINRA workload: financial trade audit validation pipeline.

Modeled after the FINRA serverless workflow from the RMMAP paper (EuroSys'24).
The DAG has four stages:

  1. finra_fetch_private  — load trades CSV, parse into records (I/O slot → stream)
  2. finra_fetch_public   — load market reference data (I/O slot → stream)
  3. finra_audit_rule × N — validate trades against a specific rule
  4. finra_merge_results  — collect all audit results, produce summary

Each audit rule checks a different condition on the trade data:
  rule 0: price > threshold           (price outlier detection)
  rule 1: quantity > threshold        (large-order detection)
  rule 2: BUY/SELL imbalance per acct (wash-trade heuristic)
  rule 3: same-symbol rapid trades    (spoofing heuristic)
  rule 4: concentration per symbol    (concentration risk)
  rule 5: off-hours trades            (after-hours detection)
  rule 6: penny stock filter          (low-price filter)
  rule 7: round-lot filter            (round quantity detection)

All functions use only the SHM API and Python builtins — no pandas/numpy.
"""

import shm

# ── Stage 1: Fetch private data (trades CSV) ─────────────────────────────────

def finra_fetch_private(in_io_slot, out_stream_slot):
    """Read trades CSV from I/O slot, parse header + rows, write each row
    as a structured record to the stream slot for downstream audit rules."""
    records = shm.read_all_inputs_from(in_io_slot)
    header = None
    for _origin, rec in records:
        line = rec.decode('utf-8', errors='replace').strip()
        if not line:
            continue
        if header is None:
            header = line
            shm.append_stream_data(out_stream_slot, ('H:' + header).encode())
            continue
        shm.append_stream_data(out_stream_slot, ('T:' + line).encode())


# ── Stage 2: Fetch public data (market reference) ────────────────────────────

def finra_fetch_public(out_stream_slot):
    """Generate synthetic market reference data: per-symbol average price
    and daily volume.  In a real deployment this would fetch from a market
    data API.  Writes reference records to stream slot."""
    symbols = ['AAPL','GOOG','MSFT','AMZN','META','TSLA','NVDA','JPM',
               'BAC','WFC','GS','MS','V','MA','NFLX','DIS','INTC','AMD',
               'ORCL','CRM']
    # Synthetic reference: avg_price, avg_daily_volume
    ref = {
        'AAPL': (175.0, 80000000), 'GOOG': (140.0, 25000000),
        'MSFT': (370.0, 22000000), 'AMZN': (180.0, 50000000),
        'META': (500.0, 15000000), 'TSLA': (250.0, 100000000),
        'NVDA': (800.0, 40000000), 'JPM':  (195.0, 10000000),
        'BAC':  (35.0,  45000000), 'WFC':  (55.0,  20000000),
        'GS':   (400.0, 3000000),  'MS':   (90.0,  10000000),
        'V':    (280.0, 7000000),  'MA':   (450.0, 4000000),
        'NFLX': (600.0, 5000000),  'DIS':  (110.0, 12000000),
        'INTC': (45.0,  30000000), 'AMD':  (160.0, 50000000),
        'ORCL': (125.0, 8000000),  'CRM':  (300.0, 5000000),
    }
    for sym in symbols:
        avg_price, avg_vol = ref.get(sym, (100.0, 10000000))
        shm.append_stream_data(out_stream_slot,
            ('R:%s,%.2f,%d' % (sym, avg_price, avg_vol)).encode())


# ── Stage 3: Audit rules ─────────────────────────────────────────────────────

def _parse_trades(in_slot):
    """Parse trade records from a stream slot.
    Returns (header_fields, list_of_trade_dicts)."""
    header_fields = None
    trades = []
    for _origin, rec in shm.read_all_stream_records(in_slot):
        s = rec.decode('utf-8', errors='replace')
        if s.startswith('H:'):
            header_fields = s[2:].split(',')
        elif s.startswith('T:'):
            vals = s[2:].split(',')
            if header_fields and len(vals) == len(header_fields):
                trades.append(dict(zip(header_fields, vals)))
    return header_fields, trades


def _parse_reference(in_slot):
    """Parse market reference records.  Returns dict: symbol → (avg_price, avg_vol)."""
    ref = {}
    for _origin, rec in shm.read_all_stream_records(in_slot):
        s = rec.decode('utf-8', errors='replace')
        if s.startswith('R:'):
            parts = s[2:].split(',')
            if len(parts) == 3:
                ref[parts[0]] = (float(parts[1]), int(parts[2]))
    return ref


def finra_audit_rule(in_slot, rule_id):
    """Run audit rule `rule_id` on trade data from stream slot `in_slot`.
    Writes violation records to I/O output slots 1 and 2 (fanout).

    in_slot contains both trade records (T:...) and reference records (R:...).
    """
    _, trades = _parse_trades(in_slot)
    ref = _parse_reference(in_slot)
    violations = []

    if rule_id == 0:
        # Price outlier: trade price > 2× reference avg
        for t in trades:
            sym = t.get('symbol', '')
            price = float(t.get('price', 0))
            avg = ref.get(sym, (100.0, 0))[0]
            if price > 2.0 * avg:
                violations.append('PRICE_OUTLIER:%s:%.2f>%.2f' % (
                    t.get('trade_id', '?'), price, 2.0 * avg))

    elif rule_id == 1:
        # Large order: quantity > 5000
        for t in trades:
            qty = int(t.get('quantity', 0))
            if qty > 5000:
                violations.append('LARGE_ORDER:%s:qty=%d' % (
                    t.get('trade_id', '?'), qty))

    elif rule_id == 2:
        # Wash trade heuristic: same account buys+sells same symbol
        acct_sym = {}
        for t in trades:
            key = t.get('account_id', '') + ':' + t.get('symbol', '')
            sides = acct_sym.get(key, set())
            sides.add(t.get('side', ''))
            acct_sym[key] = sides
        for key, sides in acct_sym.items():
            if len(sides) > 1:
                violations.append('WASH_TRADE:%s' % key)

    elif rule_id == 3:
        # Spoofing: >10 trades from same account on same symbol
        acct_sym_count = {}
        for t in trades:
            key = t.get('account_id', '') + ':' + t.get('symbol', '')
            acct_sym_count[key] = acct_sym_count.get(key, 0) + 1
        for key, count in acct_sym_count.items():
            if count > 10:
                violations.append('SPOOFING:%s:count=%d' % (key, count))

    elif rule_id == 4:
        # Concentration: >5% of all trades in one symbol
        sym_count = {}
        total = len(trades)
        for t in trades:
            sym = t.get('symbol', '')
            sym_count[sym] = sym_count.get(sym, 0) + 1
        threshold = total * 0.05
        for sym, count in sym_count.items():
            if count > threshold + 100:  # allow margin for uniform distribution
                violations.append('CONCENTRATION:%s:%.1f%%' % (
                    sym, 100.0 * count / max(total, 1)))

    elif rule_id == 5:
        # After-hours: trades outside 09:30-16:00
        for t in trades:
            ts = t.get('timestamp', '')
            if 'T' in ts:
                hour_min = ts.split('T')[1]
                parts = hour_min.split(':')
                if len(parts) >= 2:
                    h, m = int(parts[0]), int(parts[1])
                    minutes = h * 60 + m
                    if minutes < 570 or minutes > 960:  # 9:30=570, 16:00=960
                        violations.append('AFTER_HOURS:%s:%s' % (
                            t.get('trade_id', '?'), hour_min))

    elif rule_id == 6:
        # Penny stock: price < 5.0
        for t in trades:
            price = float(t.get('price', 0))
            if price < 5.0:
                violations.append('PENNY_STOCK:%s:%.2f' % (
                    t.get('trade_id', '?'), price))

    elif rule_id == 7:
        # Round lot: quantity is exact multiple of 1000
        for t in trades:
            qty = int(t.get('quantity', 0))
            if qty >= 1000 and qty % 1000 == 0:
                violations.append('ROUND_LOT:%s:qty=%d' % (
                    t.get('trade_id', '?'), qty))

    # Write results: rule_id + violation count (summary only, no detail records —
    # writing thousands of individual violations would stress the non-atomic SHM
    # page allocator when multiple rules run concurrently).
    rule_names = {
        0: 'PRICE_OUTLIER', 1: 'LARGE_ORDER', 2: 'WASH_TRADE',
        3: 'SPOOFING', 4: 'CONCENTRATION', 5: 'AFTER_HOURS',
        6: 'PENNY_STOCK', 7: 'ROUND_LOT',
    }
    out_slot = 10 + rule_id  # stream slots 10..17 for 8 rules
    shm.append_stream_data(out_slot,
        ('rule=%d,violations=%d' % (rule_id, len(violations))).encode())
    shm.append_stream_data(out_slot,
        ('rule_name=%s' % rule_names.get(rule_id, 'UNKNOWN')).encode())


# ── Stage 4: Merge results ───────────────────────────────────────────────────

def finra_merge_results(in_slot):
    """Merge audit rule results from aggregated stream slot `in_slot`.
    Writes summary to I/O slot 1."""
    rule_summaries = {}  # rule_id_str → violation count
    rule_names = {}      # rule_id_str → name (from rule_name= records)
    total_violations = 0

    current_rule_id = None
    for _origin, rec in shm.read_all_stream_records(in_slot):
        s = rec.decode('utf-8', errors='replace')
        if s.startswith('rule='):
            parts = s.split(',')
            current_rule_id = parts[0].split('=')[1]
            count = int(parts[1].split('=')[1]) if len(parts) > 1 else 0
            rule_summaries[current_rule_id] = rule_summaries.get(current_rule_id, 0) + count
            total_violations += count
        elif s.startswith('rule_name=') and current_rule_id is not None:
            rule_names[current_rule_id] = s.split('=', 1)[1]

    # Fallback names if rule_name= records are missing
    _default_names = {
        '0': 'PRICE_OUTLIER', '1': 'LARGE_ORDER', '2': 'WASH_TRADE',
        '3': 'SPOOFING', '4': 'CONCENTRATION', '5': 'AFTER_HOURS',
        '6': 'PENNY_STOCK', '7': 'ROUND_LOT',
    }
    for k in rule_summaries:
        if k not in rule_names:
            rule_names[k] = _default_names.get(k, 'UNKNOWN')

    shm.write_output(b'=== finra_audit_results ===')
    shm.write_output(
        ('total_rules_executed=%d' % len(rule_summaries)).encode())
    shm.write_output(
        ('total_violations=%d' % total_violations).encode())
    shm.write_output(b'--- per_rule_summary ---')
    for rule_id in sorted(rule_summaries.keys()):
        name = rule_names.get(rule_id, 'UNKNOWN')
        shm.write_output(
            ('rule_%s (%s): %d violations' % (
                rule_id, name, rule_summaries[rule_id])).encode())
