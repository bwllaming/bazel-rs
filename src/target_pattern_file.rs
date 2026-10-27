//! `--target_pattern_file` (BuildRequestOptions.targetPatternFile,
//! buildtool/BuildRequestOptions.java:395-403) read like
//! TargetPatternsHelper.readFrom (runtime/commands/TargetPatternsHelper.java:49-84):
//! `build`, `test` and `fetch` take their target patterns from the file
//! instead of the command-line residue. Paths below are under
//! src/main/java/com/google/devtools/build/lib/ of the pinned Bazel 9.3
//! sources.

use std::path::Path;

/// `Command-line target pattern and --target_pattern_file cannot both be
/// specified` (TARGET_PATTERN_FILE_WITH_COMMAND_LINE_PATTERN, exit 2).
pub const WITH_COMMAND_LINE_PATTERN: &str =
    "Command-line target pattern and --target_pattern_file cannot both be specified";

/// Whether `command` reads its target patterns through TargetPatternsHelper
/// (BuildCommand, TestCommand, FetchCommand; VendorCommand is not modelled).
/// `run` and `cquery` accept the option (it is a build option they inherit)
/// and ignore it: RunCommand reads the residue, CqueryCommand its query
/// expression.
pub fn reads_patterns(command: &str) -> bool {
    matches!(command, "build" | "test" | "fetch")
}

/// The patterns of the file's bytes: each line (ISO-8859-1, split like
/// CharStreams.readLines on `\n`, `\r\n` and `\r`) up to its first `#`,
/// trimmed like String.trim (every char <= U+0020), empty ones dropped.
pub fn parse(bytes: &[u8]) -> Vec<String> {
    let text: String = bytes.iter().map(|&byte| char::from(byte)).collect();
    let mut patterns = Vec::new();
    let mut rest = text.as_str();
    while !rest.is_empty() {
        let end = rest.find(['\n', '\r']).unwrap_or(rest.len());
        let line = &rest[..end];
        rest = &rest[end..];
        if let Some(after) = rest.strip_prefix("\r\n") {
            rest = after;
        } else if !rest.is_empty() {
            rest = &rest[1..];
        }
        let pattern = line
            .split('#')
            .next()
            .unwrap_or("")
            .trim_matches(|c: char| c <= ' ');
        if !pattern.is_empty() {
            patterns.push(pattern.to_owned());
        }
    }
    patterns
}

/// TargetPatternsHelper.readFrom: the file (relative to the client's working
/// directory, or absolute) when it is set and the residue is empty; the
/// residue otherwise. `Err` is the command's failure message (exit 2).
pub fn read_from(
    file: Option<&str>,
    residue: Vec<String>,
    working_directory: &Path,
) -> Result<Vec<String>, String> {
    let Some(file) = file.filter(|file| !file.is_empty()) else {
        return Ok(residue);
    };
    if !residue.is_empty() {
        return Err(WITH_COMMAND_LINE_PATTERN.to_owned());
    }
    let path = working_directory.join(file);
    // snapshot-unrecorded: read before the snapshot check point; the
    // patterns it yields are part of the snapshot key (targets).
    match std::fs::read(&path) {
        Ok(bytes) => Ok(parse(&bytes)),
        Err(error) => Err(format!(
            "I/O error reading from {}: {} ({})",
            path.display(),
            path.display(),
            io_reason(&error)
        )),
    }
}

/// The strerror text of an I/O error (`No such file or directory`), as the
/// JNI layer reports it, without Rust's `(os error N)` suffix.
fn io_reason(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(index) => text[..index].to_owned(),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_comments_blanks_and_negative_patterns() {
        let file = b"//p:a\n# comment\n  //p:b   # trailing\n\n-//p:b\r\n//p:c\r\t\n//p:d";
        assert_eq!(parse(file), ["//p:a", "//p:b", "-//p:b", "//p:c", "//p:d"]);
        assert!(parse(b"# nothing\n\n").is_empty());
        // ISO-8859-1: every byte is one character.
        assert_eq!(parse(b"//p:\xe9\n"), ["//p:\u{e9}"]);
    }

    #[test]
    fn residue_and_file_are_exclusive() {
        let directory = Path::new("/nonexistent-directory");
        assert_eq!(
            read_from(None, vec!["//a".into()], directory).unwrap(),
            ["//a"]
        );
        assert_eq!(
            read_from(Some(""), vec!["//a".into()], directory).unwrap(),
            ["//a"]
        );
        assert_eq!(
            read_from(Some("f.txt"), vec!["//a".into()], directory).unwrap_err(),
            WITH_COMMAND_LINE_PATTERN
        );
        assert_eq!(
            read_from(Some("f.txt"), Vec::new(), directory).unwrap_err(),
            "I/O error reading from /nonexistent-directory/f.txt: /nonexistent-directory/f.txt (No such file or directory)"
        );
    }
}
