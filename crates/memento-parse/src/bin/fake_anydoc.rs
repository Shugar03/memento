//! Test double for the anydoc CLI (T-031): `memento-parse-fake-anydoc`.
//!
//! Mirrors the real CLI shape — `anydoc <input-file>` writes Markdown to
//! stdout — while letting tests steer failure modes without touching argv
//! (the argv contract stays identical to production):
//!
//! * `FAKE_ANYDOC_MODE=echo` (default): reads the staged input file and
//!   echoes it wrapped as Markdown, exercising the staging round trip.
//! * `FAKE_ANYDOC_MODE=bomb`: floods stdout past any realistic cap.
//! * `FAKE_ANYDOC_MODE=hang`: sleeps ~300 s without producing output.
//! * `FAKE_ANYDOC_MODE=fail`: exits non-zero with a stderr diagnostic
//!   (corrupt-document path).
//!
//! This binary is a dev-only helper; it is never shipped or invoked by
//! production code.

use std::io::Write;
use std::process::exit;

const BOMB_TOTAL: usize = 8 * 1024 * 1024; // 8 MiB
const HANG_SECS: u64 = 300;

fn main() {
    let mode = std::env::var("FAKE_ANYDOC_MODE").unwrap_or_else(|_| "echo".to_string());
    let input = std::env::args().nth(1);

    match mode.as_str() {
        "bomb" => {
            let chunk = [b'x'; 64 * 1024];
            let mut out = std::io::stdout().lock();
            let mut written = 0usize;
            while written < BOMB_TOTAL {
                out.write_all(&chunk).expect("write stdout");
                written += chunk.len();
            }
            out.flush().expect("flush stdout");
        }
        "hang" => {
            std::thread::sleep(std::time::Duration::from_secs(HANG_SECS));
        }
        "fail" => {
            eprintln!("fake anydoc: unsupported or corrupt document");
            exit(3);
        }
        _ => {
            // echo: mimic `anydoc <input>` → Markdown on stdout.
            let input = input.expect("fake anydoc: input path required");
            let content = std::fs::read_to_string(&input).expect("fake anydoc: read input");
            let name = std::path::Path::new(&input)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "# {name}\n\n{content}");
            out.flush().expect("flush stdout");
        }
    }
}
