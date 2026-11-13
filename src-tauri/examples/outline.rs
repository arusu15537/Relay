//! Print the outline Relay would send for a read-only context file, with token estimates.
//!
//!   cargo run --example outline -- path/to/file.py [--quiet]

use relay_lib::budget::estimate_tokens;
use relay_lib::outline::outline;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quiet = args.iter().any(|a| a == "--quiet");
    let mut failed = false;
    for path in args.iter().filter(|a| !a.starts_with("--")) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{path}: {e}");
                failed = true;
                continue;
            }
        };
        let full = estimate_tokens(&text);
        match outline(path, &text) {
            Some(o) => {
                let t = estimate_tokens(&o.text);
                println!(
                    "{path}: ≈{full} → ≈{t} tokens ({:.0}%), {} of {} lines omitted",
                    t as f64 * 100.0 / full.max(1) as f64,
                    o.omitted_lines,
                    text.lines().count()
                );
                if !quiet {
                    println!("{}", o.text);
                }
            }
            None => println!("{path}: ≈{full} tokens, no outline (unsupported language or nothing to omit)"),
        }
    }
    std::process::exit(if failed { 1 } else { 0 });
}
