// finra_wasi.rs — the WASM (Faaslet) audit-rule function for the Faasm-like demo.
//
// One module: `finra <rule_id>` reads the trades CSV from stdin, runs audit rule
// `rule_id`, and writes the violation count to stdout. A faithful port of the 8
// rules in Executor/guest/src/workloads/finra.rs (== finra_rules.py), so every
// system reports the same total_violations. Compiled to wasm32-wasip1 and run as
// a fresh wasmtime instance per rule (the Faaslet isolation we abstract); the
// host driver moves the trades through Redis (the serialized broadcast).
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

fn ref_cents(sym: &str) -> u32 {
    // Must match finra.rs `finra_fetch_public` (avg price, cents).
    match sym {
        "AAPL" => 17500, "GOOG" => 14000, "MSFT" => 37000, "AMZN" => 18000,
        "META" => 50000, "TSLA" => 25000, "NVDA" => 80000, "JPM" => 19500,
        "BAC" => 3500, "WFC" => 5500, "GS" => 40000, "MS" => 9000,
        "V" => 28000, "MA" => 45000, "NFLX" => 60000, "DIS" => 11000,
        "INTC" => 4500, "AMD" => 16000, "ORCL" => 12500, "CRM" => 30000,
        _ => 10000,
    }
}

fn parse_cents(s: &str) -> u32 {
    let mut it = s.splitn(2, '.');
    let whole: u32 = it.next().unwrap_or("0").parse().unwrap_or(0);
    let mut c = whole * 100;
    if let Some(f) = it.next() {
        c += if f.len() >= 2 {
            f[..2].parse().unwrap_or(0)
        } else {
            f.parse::<u32>().unwrap_or(0) * 10
        };
    }
    c
}

struct Trade {
    symbol: String,
    price: u32,
    quantity: u32,
    side: u8,
    minutes: u32,
    account: String,
}

fn parse_trade(line: &str) -> Option<Trade> {
    let p: Vec<&str> = line.splitn(7, ',').collect();
    if p.len() < 7 {
        return None;
    }
    let (h, m) = {
        let mut t = p[5].splitn(3, ':');
        let h: u32 = t.next().unwrap_or("0").parse().unwrap_or(0);
        let m: u32 = t.next().unwrap_or("0").parse().unwrap_or(0);
        (h, m)
    };
    Some(Trade {
        symbol: p[1].to_string(),
        price: parse_cents(p[2]),
        quantity: p[3].parse().unwrap_or(0),
        side: if p[4] == "BUY" { 0 } else { 1 },
        minutes: h * 60 + m,
        account: p[6].to_string(),
    })
}

fn audit_rule(trades: &[Trade], rule_id: u32) -> u64 {
    let mut v: u64 = 0;
    match rule_id {
        0 => for t in trades { if t.price > 2 * ref_cents(&t.symbol) { v += 1; } },
        1 => for t in trades { if t.quantity > 5000 { v += 1; } },
        2 => {
            let mut mask: BTreeMap<(String, String), u8> = BTreeMap::new();
            for t in trades {
                *mask.entry((t.account.clone(), t.symbol.clone())).or_insert(0) |= 1 << t.side;
            }
            v = mask.values().filter(|&&m| m == 3).count() as u64;
        }
        3 => {
            let mut cnt: BTreeMap<(String, String), u32> = BTreeMap::new();
            for t in trades {
                *cnt.entry((t.account.clone(), t.symbol.clone())).or_insert(0) += 1;
            }
            v = cnt.values().filter(|&&n| n > 10).count() as u64;
        }
        4 => {
            let mut cnt: BTreeMap<String, u32> = BTreeMap::new();
            for t in trades {
                *cnt.entry(t.symbol.clone()).or_insert(0) += 1;
            }
            let threshold = trades.len() as u32 / 20 + 100;
            v = cnt.values().filter(|&&n| n > threshold).count() as u64;
        }
        5 => for t in trades { if t.minutes < 570 || t.minutes > 960 { v += 1; } },
        6 => for t in trades { if t.price < 500 { v += 1; } },
        7 => for t in trades { if t.quantity >= 1000 && t.quantity % 1000 == 0 { v += 1; } },
        _ => {}
    }
    v
}

fn main() {
    let rule_id: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut trades = Vec::new();
    let stdin = io::stdin();
    let mut first = true;
    for line in stdin.lock().lines() {
        let line = match line { Ok(l) => l, Err(_) => break };
        if first { first = false; continue; }   // header
        if line.trim().is_empty() { continue; }
        if let Some(t) = parse_trade(&line) {
            trades.push(t);
        }
    }
    let v = audit_rule(&trades, rule_id);
    let mut out = io::stdout();
    let _ = writeln!(out, "{}", v);
}
