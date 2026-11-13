//! SQL REPL for the in-memory database.

#![forbid(unsafe_code)]

use std::io::{self, Write};
use std::process::ExitCode;

use rustdb::{Database, run};

fn main() -> ExitCode {
    let mut db = Database::new();
    let stdin = io::stdin();
    let stdout = io::stdout();
    if let Err(error) = run(&mut db, stdin.lock(), stdout.lock()) {
        let _ = writeln!(io::stderr(), "Error: {error}");
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
