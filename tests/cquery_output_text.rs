//! `--output=jsonproto|textproto` printing, checked byte-for-byte against
//! genuine Bazel 9.3.1: the genuine streamed_proto capture of a query,
//! transcoded natively, must equal genuine's jsonproto/textproto of the same
//! query (tests/fixtures/cquery_output/README.md).
#![allow(clippy::disallowed_methods)]
use std::io::Write;
use std::path::Path;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cquery_output/genuine")
            .join(name),
    )
    .unwrap()
}

fn transcode(streamed: &[u8], json: bool, configurations: bool, chunk: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut transcoder = bazel_rs::cquery_text::Transcoder::new(&mut out, json, configurations);
    for piece in streamed.chunks(chunk) {
        transcoder.write_all(piece).unwrap();
    }
    transcoder.finish().unwrap();
    out
}

fn check(prefix: &str, configurations: bool) {
    let streamed = fixture(&format!("{prefix}.streamed_proto"));
    for (json, extension) in [(true, "jsonproto"), (false, "textproto")] {
        let expected = fixture(&format!("{prefix}.{extension}"));
        // Arbitrary write boundaries must not change the result.
        for chunk in [1, 7, 4096, usize::MAX] {
            let actual = transcode(
                &streamed,
                json,
                configurations,
                chunk.min(streamed.len().max(1)),
            );
            assert!(
                actual == expected,
                "{prefix}.{extension} (chunk {chunk}) differs:\n{}",
                first_difference(&expected, &actual)
            );
        }
    }
}

fn first_difference(expected: &[u8], actual: &[u8]) -> String {
    let expected = String::from_utf8_lossy(expected);
    let actual = String::from_utf8_lossy(actual);
    for (index, (a, b)) in expected.lines().zip(actual.lines()).enumerate() {
        if a != b {
            return format!("line {}: expected {a:?}, got {b:?}", index + 1);
        }
    }
    format!(
        "lengths: expected {} lines, got {}",
        expected.lines().count(),
        actual.lines().count()
    )
}

#[test]
fn configured_results_match_genuine_json_and_text() {
    check("configured", true);
}

#[test]
fn unconfigured_results_match_genuine_json_and_text() {
    check("unconfigured", false);
}

#[test]
fn empty_results_are_an_empty_message() {
    assert_eq!(transcode(&[], true, true, 1), b"{\n}\n");
    assert_eq!(transcode(&[], false, true, 1), b"");
}
