//! SQL REPL.

#![forbid(unsafe_code)]

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

use rustdb::{Database, run};

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let database = match open_database(&arguments) {
        Ok(Some(database)) => database,
        Ok(None) => return ExitCode::SUCCESS,
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

fn open_database(arguments: &[String]) -> Result<Option<Database>, StartupError> {
    match arguments {
        [] => Ok(Some(Database::new())),
        [flag, path] if flag == "--database" => Database::open(path)
            .map(Some)
            .map_err(StartupError::Database),
        [flag, path] if flag == "--migrate" => {
            Database::migrate(path).map_err(StartupError::Database)?;
            Ok(None)
        }
        _ => Err(StartupError::Usage(
            "Usage: rustdb [--database PATH | --migrate PATH]".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_database_path() {
        assert!(matches!(open_database(&[]), Ok(Some(_))));
        let dir = std::env::temp_dir().join(format!("rustdb-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("demo.db");
        assert!(matches!(
            open_database(&["--database".to_string(), path.display().to_string()]),
            Ok(Some(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(
            open_database(&["--database".to_string()]),
            Err(StartupError::Usage(_))
        ));
        assert!(matches!(
            open_database(&["--other".to_string()]),
            Err(StartupError::Usage(_))
        ));
        assert!(matches!(
            open_database(&["--migrate".to_string()]),
            Err(StartupError::Usage(_))
        ));
    }
}
