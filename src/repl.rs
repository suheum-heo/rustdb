//! Interactive SQL session.

use std::io::{BufRead, Write};

use crate::database::Database;
use crate::format::render_result;

/// Read SQL from `input` and write results to `output` until `exit` or `quit`.
///
/// A statement is executed when the buffer ends in a semicolon that is not
/// inside a quote or comment. `exit` and `quit` are recognized only when no
/// partial statement is buffered.
pub fn run(db: &mut Database, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    let mut lines = input.lines();
    let mut buffer = String::new();

    loop {
        let prompt = if buffer.is_empty() {
            "rustdb> "
        } else {
            "...> "
        };
        write!(output, "{prompt}")?;
        output.flush()?;

        let Some(line) = lines.next() else {
            if !buffer.trim().is_empty() {
                writeln!(output, "Error: unexpected end of input")?;
            }
            return Ok(());
        };
        let line = line?;

        if buffer.is_empty() && is_quit(&line) {
            return Ok(());
        }
        if buffer.is_empty() && line.trim().is_empty() {
            continue;
        }

        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(&line);

        if !ends_with_statement(&buffer) {
            continue;
        }

        match db.execute(&buffer) {
            Ok(results) => {
                for result in results {
                    writeln!(output, "{}", render_result(&result))?;
                }
            }
            Err(error) => writeln!(output, "Error: {error}")?,
        }
        buffer.clear();
    }
}

fn is_quit(line: &str) -> bool {
    let trimmed = line.trim();
    let command = trimmed.strip_suffix(';').unwrap_or(trimmed).trim();
    matches!(command.to_ascii_lowercase().as_str(), "exit" | "quit")
}

fn ends_with_statement(sql: &str) -> bool {
    let mut chars = sql.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut ended = false;

    while let Some(ch) = chars.next() {
        if in_single {
            if ch == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                } else {
                    in_single = false;
                }
            }
            continue;
        }
        if in_double {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                } else {
                    in_double = false;
                }
            }
            continue;
        }
        if in_backtick {
            if ch == '`' {
                in_backtick = false;
            }
            continue;
        }

        match ch {
            '\'' => {
                in_single = true;
                ended = false;
            }
            '"' => {
                in_double = true;
                ended = false;
            }
            '`' => {
                in_backtick = true;
                ended = false;
            }
            '-' if chars.peek() == Some(&'-') => {
                chars.next();
                for next in chars.by_ref() {
                    if next == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
            }
            ';' => ended = true,
            other if other.is_whitespace() => {}
            _ => ended = false,
        }
    }

    ended
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn session(input: &str) -> String {
        let mut db = Database::new();
        let mut output = Vec::new();
        run(&mut db, Cursor::new(input), &mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn runs_the_milestone_example() {
        let output = session(
            "\
CREATE TABLE users (id INT, name TEXT, age INT);
INSERT INTO users VALUES (1, 'Alice', 24);
INSERT INTO users VALUES (2, 'Bob', 30);
SELECT name FROM users WHERE age > 25;
exit
",
        );
        assert_eq!(
            output,
            "\
rustdb> OK
rustdb> 1 row inserted
rustdb> 1 row inserted
rustdb> +------+
| name |
+------+
| Bob  |
+------+
1 row returned
rustdb> "
        );
    }

    #[test]
    fn accepts_multiline_statements_strings_containing_semicolons_and_quit() {
        let output = session(
            "\
CREATE TABLE t (
    id INT,
    name TEXT
);
INSERT INTO t VALUES (1, 'a;b');
SELECT name FROM t;
quit
",
        );
        assert_eq!(
            output,
            "\
rustdb> ...> ...> ...> OK
rustdb> 1 row inserted
rustdb> +------+
| name |
+------+
| a;b  |
+------+
1 row returned
rustdb> "
        );
    }

    #[test]
    fn reports_errors_and_keeps_reading() {
        let output = session(
            "\
SELECT * FROM missing;
CREATE TABLE t (id INT);
exit
",
        );
        assert_eq!(
            output,
            "\
rustdb> Error: table 'missing' does not exist
rustdb> OK
rustdb> "
        );
    }

    #[test]
    fn does_not_treat_a_semicolon_inside_a_comment_as_the_end() {
        assert!(!ends_with_statement("SELECT * FROM t -- still going;\n"));
        assert!(ends_with_statement("SELECT * FROM t; -- done\n"));
        assert!(ends_with_statement("SELECT * FROM t WHERE name = 'a;b';"));
    }
}
