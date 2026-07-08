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
mod version;
#[cfg(feature = "kafka")]
mod kafka;
#[cfg(feature = "postgres")]
mod postgres;

use clap::Parser;

fn main() {
    let args = cli::Cli::parse();
    cli::run(args);
}
