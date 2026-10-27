// The exit code of a rejected command by its diagnostic (include!d by the
// harnesses whose rejection helpers serve option errors and other failures).

/// 2 for Bazel's command-line errors (crate::exit_code: an unrecognized
/// option, an option value that does not parse, an rc file or `--config`
/// error), 1 for the native profile's own rejection of an option Bazel
/// accepts and for every other failure.
#[allow(dead_code)]
fn rejection_code(stderr: &str) -> i32 {
    let limitation = stderr.contains("bazel-rs: unsupported ")
        || stderr.contains("options are never silently ignored")
        || stderr.contains("not implemented in the native profile");
    let command_line = [
        "Unrecognized option: ",
        ":: Expected value after ",
        ":: Error loading option ",
        "Import loop detected",
        "While parsing option ",
        "Invalid options syntax",
        "is not defined in any .rc file",
        "Unable to read .bazelrc file",
        "Can't specify ",
        "native-only option ",
        "Config expansion has a cycle",
        "Unexpected value after boolean option",
        "Illegal use of 'no' prefix",
        "import declaration",
        "Can't load flag ",
        ":: Failed to load build setting ",
        "Unknown startup option",
        "requires a positive integer",
        "expects true or false",
        "expected a boolean",
    ]
    .iter()
    .any(|pattern| stderr.contains(pattern));
    if command_line && !limitation { 2 } else { 1 }
}
