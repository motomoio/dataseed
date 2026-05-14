use std::process::ExitCode;

use clap::Parser;

use dataseed::cli::{run, Cli};

fn main() -> ExitCode {
    run(Cli::parse())
}
