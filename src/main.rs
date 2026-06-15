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

use clap::Parser;

fn main() {
    let args = cli::Cli::parse();
    cli::run(args);
}
