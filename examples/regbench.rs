//! Time a trace-scope registry baseline: `cargo run --release --example regbench`.
use std::time::Instant;

use bettergrep::config::{default_trace_registry_exclude, default_trace_registry_keys};
use bettergrep::registry::{Exclusions, default_threads, diff, snapshot, snapshot_parallel};

fn main() {
    let exclude = Exclusions::new(&default_trace_registry_exclude());
    let keys = default_trace_registry_keys();

    let t = Instant::now();
    let serial: usize = keys.iter().map(|k| snapshot(k, None, &exclude).unwrap().len()).sum();
    println!("serial:   {serial} keys in {:.1?}", t.elapsed());

    let t = Instant::now();
    let before = snapshot_parallel(&keys, None, &exclude, default_threads());
    let count: usize = before.values().map(|s| s.len()).sum();
    println!("parallel: {count} keys in {:.1?} ({} threads)", t.elapsed(), default_threads());

    let t = Instant::now();
    let after = snapshot_parallel(&keys, Some(&before), &exclude, default_threads());
    let changes: usize = keys.iter().map(|k| diff(&before[k], &after[k]).len()).sum();
    println!("re-snapshot + diff: {changes} changes in {:.1?}", t.elapsed());
    for k in &keys {
        for c in diff(&before[k], &after[k]) {
            println!("  {} {} {:?}", c.kind, c.key, c.value);
        }
    }
}
