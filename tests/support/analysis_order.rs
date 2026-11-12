// BAZEL_RS_ANALYSIS_ORDER (docs/EVALUATOR.md "Genuine parity, task C"):
// the comparisons of an evaluated build's output with the walk's. Exact
// (the default) compares byte for byte; the genuine order, and a whole
// suite built with BAZEL_RS_DEFAULT_ANALYSIS_ORDER=genuine, canonicalize:
// the console events as a multiset (each target's analysis-time lines are
// written when its job finishes), every other line in order; a failure's
// error the walk's or one of the walk's --keep_going errors (fail fast on
// the first confirmed error to finish).

/// Whether this suite's bazel-rs defaults to the genuine order (both are
/// built with the same environment).
#[allow(dead_code)]
fn genuine_default() -> bool {
    option_env!("BAZEL_RS_DEFAULT_ANALYSIS_ORDER") == Some("genuine")
}

/// A console event: a `DEBUG:` or `WARNING:` line with the lines that
/// continue it (a multi-line print).
#[allow(dead_code)]
fn console_event(line: &str) -> bool {
    line.starts_with("DEBUG: ") || line.starts_with("WARNING: ")
}

#[allow(dead_code)]
fn continues_event(line: &str) -> bool {
    ![
        "DEBUG: ",
        "WARNING: ",
        "INFO: ",
        "ERROR: ",
        "bazel-rs: ",
        "ACTION ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
        && !line.starts_with("  ")
}

/// `text` canonicalized for the genuine order: the other lines in order,
/// the console events sorted.
#[allow(dead_code)]
fn canonical(text: &str) -> (Vec<String>, Vec<String>) {
    let (mut rest, mut events) = (Vec::new(), Vec::<String>::new());
    let mut in_event = false;
    for line in text.lines() {
        if console_event(line) {
            events.push(line.to_owned());
            in_event = true;
        } else if in_event && continues_event(line) {
            let last = events.last_mut().expect("an event");
            last.push('\n');
            last.push_str(line);
        } else {
            in_event = false;
            rest.push(line.to_owned());
        }
    }
    events.sort();
    (rest, events)
}

/// The genuine order's comparison of an evaluated build's stderr (`got`,
/// its exit `success`) with the walk's (`walk`, of the same exit code):
/// a success the same once canonicalized; a failure's other lines the
/// walk's, or its error (`bazel-rs: <error>`) one of the walk's keep-going
/// errors (`ERROR: <error>` in `keep_going`, the walk's `--keep_going`
/// stderr, computed when needed).
#[allow(dead_code)]
fn assert_genuine_same(
    what: &str,
    success: bool,
    got: &str,
    walk: &str,
    keep_going: impl FnOnce() -> String,
) {
    let (rest, events) = canonical(got);
    let (walk_rest, walk_events) = canonical(walk);
    if success {
        assert_eq!(rest, walk_rest, "{what}");
        assert_eq!(events, walk_events, "{what}");
        return;
    }
    if rest == walk_rest {
        return;
    }
    let error = rest.join("\n");
    let reported = error
        .strip_prefix("bazel-rs: ")
        .map_or(error.clone(), |error| format!("ERROR: {error}"));
    let keep_going = keep_going();
    assert!(
        keep_going.contains(reported.trim_end()),
        "{what}: the error is not one of the walk's keep-going errors:\n{error}\nkeep going:\n{keep_going}"
    );
}
