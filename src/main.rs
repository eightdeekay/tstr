#![allow(dead_code)]

mod ast;
mod parser;
mod discovery;
mod scheduler;
mod value;
mod eval;
mod http;
mod runner;
mod output;
mod filter;
mod cli;
mod config;
mod secrets;
mod stats;
mod version;
#[cfg(feature = "kafka")]
mod kafka;
#[cfg(feature = "postgres")]
mod postgres;

use clap::Parser;

fn main() {
    reset_sigpipe();
    let args = cli::Cli::parse();
    cli::run(args);
}

/// Rust's runtime sets `SIGPIPE` to `SIG_IGN` at startup, which turns a closed
/// downstream pipe (e.g. `tstr stats | head`) into an `EPIPE` write error that
/// `println!` then panics on. Restoring the default disposition makes tstr die
/// quietly on a broken pipe like every other Unix filter (`cat`, `ls`, …).
/// No-op off Unix, where there is no `SIGPIPE`.
#[cfg(unix)]
fn reset_sigpipe() {
    // SAFETY: `signal(2)` with SIG_DFL on SIGPIPE is async-signal-safe and is
    // the documented way to opt back into default pipe termination.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}
