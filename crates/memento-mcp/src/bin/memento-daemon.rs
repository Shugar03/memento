//! `memento-daemon` binary entry point.
//!
//! The Windows implementation lives in [`memento_daemon_win.rs`] so Linux
//! CI can compile the crate (stub main) without pulling named-pipe / Job
//! Object APIs.

#[cfg(not(windows))]
fn main() {
    eprintln!("memento-daemon is only available on Windows");
    std::process::exit(2);
}

#[cfg(windows)]
include!("../memento_daemon_win.rs");
