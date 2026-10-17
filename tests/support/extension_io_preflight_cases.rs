// Shared only by the real CLI tests and injected live-context tests.
fn n3_preflight_cases(
    url: &str,
    absolute: &str,
    file_url: &str,
) -> Vec<(&'static str, String, &'static str)> {
    let base = format!(
        "ctx.download({url:?}, sha256={:?}, allow_fail=True",
        "00".repeat(32)
    );
    let controls =
        "unsupported module I/O profile: executable, async block=False, or canonical_id cache";
    let path =
        "unsupported module I/O profile: output/read requires clean relative ASCII path components";
    vec![
        (
            "auth",
            format!(
                "{base}, auth={{{url:?}: {{'type': 'basic', 'login': 'fixture', 'password': 'N3_AUTH_SYNTHETIC'}}}})"
            ),
            "unsupported module I/O profile: nonempty auth or headers",
        ),
        (
            "headers",
            format!("{base}, headers={{'X-Fixture': '1'}})"),
            "unsupported module I/O profile: nonempty auth or headers",
        ),
        ("async", format!("{base}, block=False)"), controls),
        ("executable", format!("{base}, executable=True)"), controls),
        (
            "canonical",
            format!("{base}, canonical_id='fixture-id')"),
            controls,
        ),
        ("escape", format!("{base}, output='../outside')"), path),
        ("absolute", format!("{base}, output={absolute:?})"), path),
        (
            "label",
            format!("{base}, output=Label('//:owned'))"),
            // The native profile accepts only a string output (BuiltinFunction text).
            "in call to download(), parameter 'output' got value of type 'Label', want 'string'",
        ),
        (
            "file-scheme",
            format!(
                "ctx.download({file_url:?}, sha256={:?}, allow_fail=True)",
                "00".repeat(32)
            ),
            "unsupported module I/O profile: only HTTP(S) URLs are supported",
        ),
        (
            "mixed-http-file-scheme",
            format!(
                "ctx.download([{url:?}, {file_url:?}], sha256={:?}, allow_fail=True)",
                "00".repeat(32)
            ),
            "unsupported module I/O profile: only HTTP(S) URLs are supported",
        ),
    ]
}
