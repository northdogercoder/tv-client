//! Command-line entry point for `tv-client`.

use clap::Parser;

/// Headless-first TV client.
#[derive(Debug, Parser)]
#[command(name = "tv-client", version, about = "Headless-first TV client")]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
