//! `powerqueue` binary entry point.
//!
//! All real logic lives in the library crate so it can be tested; this file
//! only parses the command line, sets up logging, and dispatches.

use clap::Parser;
use powerqueue::cli::{Cli, run};

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(err) => {
            powerqueue::cli::report_error(&err);
            1
        }
    };
    std::process::exit(code);
}
