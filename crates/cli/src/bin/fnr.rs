#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    fast_n_rust::cli::run_cli()
}
