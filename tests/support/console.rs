// Console text that differs between two runs of the same command (include!d
// by the tests that compare whole stderr streams).

/// `text` with the wall time of Bazel's `INFO: Elapsed time: X.XXXs` build
/// summary line replaced by `<T>`: the rest of a command's console output is
/// deterministic.
#[allow(dead_code)]
fn without_elapsed(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if line.starts_with("INFO: Elapsed time: ") {
            out.push_str("INFO: Elapsed time: <T>s");
            if line.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str(line);
        }
    }
    out
}

/// `text` with the `DEBUG: FILE:LINE:COL: ` lead of `print()` lines removed
/// (Bazel's DEBUG event of a print, crate::debug_print): for tests about
/// what a Starlark program printed, not where.
#[allow(dead_code)]
fn without_print_locations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(print_text(line));
    }
    out
}

/// One line without its `DEBUG: FILE:LINE:COL: ` lead (as it is without one).
#[allow(dead_code)]
fn print_text(line: &str) -> &str {
    let Some(rest) = line.strip_prefix("DEBUG: ") else {
        return line;
    };
    // FILE may hold ':' (a label); LINE:COL: is the first `:<digits>:<digits>: `.
    let bytes = rest.as_bytes();
    let mut at = 0;
    while let Some(found) = rest[at..].find(':') {
        let start = at + found + 1;
        let line_end = start
            + bytes[start..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
        if line_end > start && bytes.get(line_end) == Some(&b':') {
            let column = line_end + 1;
            let column_end = column
                + bytes[column..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
            if column_end > column && rest[column_end..].starts_with(": ") {
                return &rest[column_end + 2..];
            }
        }
        at = start;
    }
    line
}
