// ─────────────────────────────────────────────────────────────────────────────
// FINRA demo — financial trade audit validation pipeline
//
// Modeled after the FINRA serverless workflow from RMMAP (EuroSys'24).
//
// Slot layout:
//   I/O slot 0      : raw trades CSV (written by host Input node)
//   stream slot 5   : parsed trade records  (written by finra_fetch_private)
//   stream slot 6   : market reference data (written by finra_fetch_public)
//   stream slot 200 : aggregated inputs (trades + reference)
//   stream slots 10–17 : per-rule audit results (written by finra_audit_rule)
//   stream slot 300 : merged rule results (written by host Aggregate node)
//
// DAG stages:
//   Input                → load trades.csv into I/O slot 0
//   finra_fetch_private  → parse CSV, write to stream slot 5
//   finra_fetch_public   → synthetic market ref, write to stream slot 6
//   Aggregate            → merge slots 5+6 → slot 200
//   finra_audit_rule × 8 → each rule reads slot 200, writes to slot 10+rule_id
//   Aggregate            → merge slots 10–17 → slot 300
//   finra_merge_results  → summarize violations, write to OUTPUT_IO_SLOT
//   Output               → flush to file
// ─────────────────────────────────────────────────────────────────────────────

use alloc::vec::Vec;
use alloc::string::String;
use crate::api::ShmApi;

const TRADE_STREAM_SLOT: u32 = 5;
const REF_STREAM_SLOT: u32 = 6;
const INPUT_AGG_SLOT: u32 = 200;
const RULE_OUT_BASE: u32 = 10;

// ── Helpers ──────────────────────────────────────────────────────────────────

struct Trade {
    trade_id: u32,
    symbol: String,
    price: u32,     // price × 100  (fixed-point cents)
    quantity: u32,
    side: u8,       // 0 = BUY, 1 = SELL
    hour: u8,
    minute: u8,
    account_id: String,
}

fn parse_trades(slot: u32) -> Vec<Trade> {
    let mut trades = Vec::new();
    let records = ShmApi::read_all_stream_records(slot);
    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("");
        let body = match s.strip_prefix("T:") {
            Some(b) => b,
            None => continue,
        };
        let parts: Vec<&str> = body.splitn(7, ',').collect();
        if parts.len() < 7 { continue; }
        let trade_id: u32 = parts[0].parse().unwrap_or(0);
        let symbol = String::from(parts[1]);
        // Parse price as cents to avoid float
        let price = parse_cents(parts[2]);
        let quantity: u32 = parts[3].parse().unwrap_or(0);
        let side = if parts[4] == "BUY" { 0 } else { 1 };
        let (hour, minute) = parse_time(parts[5]);
        let account_id = String::from(parts[6]);
        trades.push(Trade { trade_id, symbol, price, quantity, side, hour, minute, account_id });
    }
    trades
}

fn parse_cents(s: &str) -> u32 {
    // "175.50" → 17550
    let parts: Vec<&str> = s.splitn(2, '.').collect();
    let whole: u32 = parts[0].parse().unwrap_or(0);
    let frac: u32 = if parts.len() > 1 {
        let f = parts[1];
        if f.len() >= 2 { f[..2].parse().unwrap_or(0) }
        else { f.parse::<u32>().unwrap_or(0) * 10 }
    } else { 0 };
    whole * 100 + frac
}

fn parse_time(ts: &str) -> (u8, u8) {
    // "2024-01-15T10:30:00" → (10, 30)
    if let Some(pos) = ts.find('T') {
        let time_part = &ts[pos + 1..];
        let parts: Vec<&str> = time_part.splitn(3, ':').collect();
        if parts.len() >= 2 {
            let h: u8 = parts[0].parse().unwrap_or(0);
            let m: u8 = parts[1].parse().unwrap_or(0);
            return (h, m);
        }
    }
    (0, 0)
}

struct MarketRef {
    symbol: String,
    avg_price: u32,  // cents
}

fn parse_reference(slot: u32) -> Vec<MarketRef> {
    let mut refs = Vec::new();
    let records = ShmApi::read_all_stream_records(slot);
    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("");
        let body = match s.strip_prefix("R:") {
            Some(b) => b,
            None => continue,
        };
        let parts: Vec<&str> = body.splitn(3, ',').collect();
        if parts.len() >= 2 {
            refs.push(MarketRef {
                symbol: String::from(parts[0]),
                avg_price: parse_cents(parts[1]),
            });
        }
    }
    refs
}

fn get_ref_price(refs: &[MarketRef], symbol: &str) -> u32 {
    refs.iter()
        .find(|r| r.symbol == symbol)
        .map(|r| r.avg_price)
        .unwrap_or(10000) // default $100
}

// ── Exported functions ───────────────────────────────────────────────────────

/// Parse trades CSV from I/O slot 0, write structured records to stream slot 5.
#[no_mangle]
pub extern "C" fn finra_fetch_private(_unused: u32) {
    let records = ShmApi::read_all_inputs();
    let mut first = true;
    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("");
        let line = s.trim();
        if line.is_empty() { continue; }
        if first {
            ShmApi::append_stream_data(TRADE_STREAM_SLOT,
                alloc::format!("H:{}", line).as_bytes());
            first = false;
            continue;
        }
        ShmApi::append_stream_data(TRADE_STREAM_SLOT,
            alloc::format!("T:{}", line).as_bytes());
    }
}

/// Generate synthetic market reference data to stream slot 6.
#[no_mangle]
pub extern "C" fn finra_fetch_public(_unused: u32) {
    let data: &[(&str, u32, u32)] = &[
        ("AAPL", 17500, 80000000), ("GOOG", 14000, 25000000),
        ("MSFT", 37000, 22000000), ("AMZN", 18000, 50000000),
        ("META", 50000, 15000000), ("TSLA", 25000, 100000000),
        ("NVDA", 80000, 40000000), ("JPM",  19500, 10000000),
        ("BAC",  3500,  45000000), ("WFC",  5500,  20000000),
        ("GS",   40000, 3000000),  ("MS",   9000,  10000000),
        ("V",    28000, 7000000),  ("MA",   45000, 4000000),
        ("NFLX", 60000, 5000000),  ("DIS",  11000, 12000000),
        ("INTC", 4500,  30000000), ("AMD",  16000, 50000000),
        ("ORCL", 12500, 8000000),  ("CRM",  30000, 5000000),
    ];
    for &(sym, avg_cents, avg_vol) in data {
        let avg_price = alloc::format!("{}.{:02}", avg_cents / 100, avg_cents % 100);
        ShmApi::append_stream_data(REF_STREAM_SLOT,
            alloc::format!("R:{},{},{}", sym, avg_price, avg_vol).as_bytes());
    }
}

/// Run audit rule `rule_id` on aggregated data in slot 200.
/// Writes results to stream slot 10 + rule_id.
#[no_mangle]
pub extern "C" fn finra_audit_rule(rule_id: u32) {
    let trades = parse_trades(INPUT_AGG_SLOT);
    let refs = parse_reference(INPUT_AGG_SLOT);
    let out_slot = RULE_OUT_BASE + rule_id;
    let mut violations: u32 = 0;

    match rule_id {
        0 => {
            // Price outlier: trade price > 2× reference avg
            for t in &trades {
                let avg = get_ref_price(&refs, &t.symbol);
                if t.price > 2 * avg {
                    violations += 1;
                }
            }
        }
        1 => {
            // Large order: quantity > 5000
            for t in &trades {
                if t.quantity > 5000 {
                    violations += 1;
                }
            }
        }
        2 => {
            // Wash trade: same account buys + sells same symbol
            let mut keys: Vec<(String, u8)> = Vec::new(); // (acct:sym, side_mask)
            for t in &trades {
                let key = alloc::format!("{}:{}", t.account_id, t.symbol);
                let bit = 1u8 << t.side;
                match keys.iter_mut().find(|(k, _)| k == &key) {
                    Some((_, mask)) => *mask |= bit,
                    None => keys.push((key, bit)),
                }
            }
            for (_, mask) in &keys {
                if *mask == 3 { violations += 1; }
            }
        }
        3 => {
            // Spoofing: >10 trades same account+symbol
            let mut counts: Vec<(String, u32)> = Vec::new();
            for t in &trades {
                let key = alloc::format!("{}:{}", t.account_id, t.symbol);
                match counts.iter_mut().find(|(k, _)| k == &key) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((key, 1)),
                }
            }
            for (_, n) in &counts {
                if *n > 10 { violations += 1; }
            }
        }
        4 => {
            // Concentration: >5% of trades in one symbol
            let mut sym_counts: Vec<(String, u32)> = Vec::new();
            let total = trades.len() as u32;
            for t in &trades {
                match sym_counts.iter_mut().find(|(s, _)| s == &t.symbol) {
                    Some((_, n)) => *n += 1,
                    None => sym_counts.push((t.symbol.clone(), 1)),
                }
            }
            let threshold = total / 20 + 100;
            for (_, n) in &sym_counts {
                if *n > threshold { violations += 1; }
            }
        }
        5 => {
            // After-hours: outside 09:30-16:00
            for t in &trades {
                let minutes = t.hour as u32 * 60 + t.minute as u32;
                if minutes < 570 || minutes > 960 {
                    violations += 1;
                }
            }
        }
        6 => {
            // Penny stock: price < $5.00 (500 cents)
            for t in &trades {
                if t.price < 500 {
                    violations += 1;
                }
            }
        }
        7 => {
            // Round lot: quantity is exact multiple of 1000
            for t in &trades {
                if t.quantity >= 1000 && t.quantity % 1000 == 0 {
                    violations += 1;
                }
            }
        }
        _ => {}
    }

    let rule_names: &[&str] = &[
        "PRICE_OUTLIER", "LARGE_ORDER", "WASH_TRADE", "SPOOFING",
        "CONCENTRATION", "AFTER_HOURS", "PENNY_STOCK", "ROUND_LOT",
    ];
    let name = if (rule_id as usize) < rule_names.len() {
        rule_names[rule_id as usize]
    } else { "UNKNOWN" };

    ShmApi::append_stream_data(out_slot,
        alloc::format!("rule={},violations={}", rule_id, violations).as_bytes());
    ShmApi::append_stream_data(out_slot,
        alloc::format!("rule_name={}", name).as_bytes());
}

/// Merge all audit rule results from stream slot `agg_slot`,
/// write summary to OUTPUT_IO_SLOT.
#[no_mangle]
pub extern "C" fn finra_merge_results(agg_slot: u32) {
    let mut rule_violations: Vec<(u32, u32, String)> = Vec::new(); // (rule_id, count, name)

    let records = ShmApi::read_all_stream_records(agg_slot);
    let mut current_rule: u32 = 0;
    let mut current_count: u32 = 0;
    let mut current_name;

    for (_origin, rec) in &records {
        let s = core::str::from_utf8(rec).unwrap_or("");
        if s.starts_with("rule=") {
            let parts: Vec<&str> = s.splitn(2, ',').collect();
            current_rule = parts[0].strip_prefix("rule=")
                .and_then(|v| v.parse().ok()).unwrap_or(0);
            current_count = if parts.len() > 1 {
                parts[1].strip_prefix("violations=")
                    .and_then(|v| v.parse().ok()).unwrap_or(0)
            } else { 0 };
        } else if s.starts_with("rule_name=") {
            current_name = String::from(s.strip_prefix("rule_name=").unwrap_or(""));
            rule_violations.push((current_rule, current_count, current_name.clone()));
        }
    }

    let total: u32 = rule_violations.iter().map(|(_, c, _)| c).sum();

    ShmApi::write_output_str("=== finra_audit_results ===");
    ShmApi::write_output_str(&alloc::format!("total_rules_executed={}", rule_violations.len()));
    ShmApi::write_output_str(&alloc::format!("total_violations={}", total));
    ShmApi::write_output_str("--- per_rule_summary ---");
    rule_violations.sort_by_key(|(id, _, _)| *id);
    for (id, count, name) in &rule_violations {
        ShmApi::write_output_str(&alloc::format!(
            "rule_{} ({}): {} violations", id, name, count));
    }
}
