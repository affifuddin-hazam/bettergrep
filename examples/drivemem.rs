//! Open each saved drive index and report what Windows charges this process
//! for it, plus search speed: `cargo run --release --example drivemem`.
use std::process::Command;
use std::time::Instant;

use bettergrep::config::Config;
use bettergrep::drive::{DriveIndex, search, volume_info};

/// Private (committed) bytes of this process, as Task Manager's "Commit size" shows it.
fn private_mb() -> f64 {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &format!("(Get-Process -Id {}).PrivateMemorySize64", std::process::id())])
        .output()
        .expect("powershell");
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1e6
}

fn main() {
    let cfg = Config::load(None).expect("config");
    println!("start                               private {:>5.0} MB", private_mb());
    let mut indexes = Vec::new();
    for d in &cfg.drives {
        let Ok((serial, _)) = volume_info(d) else { continue };
        let t = Instant::now();
        match DriveIndex::load(&DriveIndex::snapshot_path(&cfg.home, serial), d) {
            Ok(ix) => {
                println!(
                    "{d}  {:>9} entries, opened in {:>7.1?}  private {:>5.0} MB  (snapshot file {:.0} MB, mapped)",
                    ix.len(),
                    t.elapsed(),
                    private_mb(),
                    ix.mapped_bytes() as f64 / 1e6
                );
                indexes.push(ix);
            }
            Err(e) => println!("{d}  no snapshot ({e})"),
        }
    }
    let refs: Vec<&DriveIndex> = indexes.iter().collect();
    for q in ["tax", "a", "invoice 2026", "BETTERGREP.EXE", ".psd"] {
        let r = search(&refs, q, 50, false);
        println!("search {q:>14}: {:>9} matches in {:>6.1} ms", r.total, r.millis);
    }
    println!("after searching                     private {:>5.0} MB", private_mb());
}
