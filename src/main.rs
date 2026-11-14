//! SQL REPL.

#![forbid(unsafe_code)]

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

use rustdb::{Database, run};

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let database = match open_database(&arguments) {
        Ok(database) => database,
        Err(StartupError::Usage(message)) => {
            let _ = writeln!(io::stderr(), "{message}");
            return ExitCode::from(2);
        }
        Err(StartupError::Database(error)) => {
            let _ = writeln!(io::stderr(), "Error: {error}");
            return ExitCode::from(1);
        }
    };
    let mut database = database;
    let stdin = io::stdin();
    let stdout = io::stdout();
    if let Err(error) = run(&mut database, stdin.lock(), stdout.lock()) {
        let _ = writeln!(io::stderr(), "Error: {error}");
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

#[derive(Debug)]
enum StartupError {
    Usage(String),
    Database(rustdb::DbError),
}

fn open_database(arguments: &[String]) -> Result<Database, StartupError> {
    match database_path(arguments)? {
        None => Ok(Database::new()),
        Some(path) => Database::open(path).map_err(StartupError::Database),
    }
}

fn database_path(arguments: &[String]) -> Result<Option<&str>, StartupError> {
    match arguments {
        [] => Ok(None),
        [flag, path] if flag == "--database" => Ok(Some(path)),
        _ => Err(StartupError::Usage(
            "Usage: rustdb [--database PATH]".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_database_path() {
        assert_eq!(database_path(&[]).unwrap(), None);
        assert_eq!(
            database_path(&["--database".to_string(), "rustdb.db".to_string()]).unwrap(),
            Some("rustdb.db")
        );
        assert!(database_path(&["--database".to_string()]).is_err());
        assert!(database_path(&["--other".to_string()]).is_err());
    }
}
