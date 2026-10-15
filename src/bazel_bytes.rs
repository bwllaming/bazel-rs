//! Action-boundary conversion of BUILD/analysis-model strings.
//!
//! Bazel 9.3 keeps strings as raw bytes (Latin-1 Java chars). A BUILD/.bzl
//! literal with byte escapes that do not form UTF-8 (`'\377'`) therefore holds
//! a lone byte, which the vendored interpreter represents as the private-use
//! character U+10FF00 + byte (`starlark::eval::bazel_byte_char`). Bazel turns
//! internal strings into process arguments and environment values with
//! `StringEncoding.internalToPlatform` (lib/util/StringEncoding.java:105-110:
//! a UTF-8 decoding with U+FFFD replacement when `sun.jnu.encoding` is not
//! ISO-8859-1), and `FileWriteAction` writes the internal bytes unchanged
//! (analysis/actions/FileWriteAction.java:272).

use std::borrow::Cow;

fn byte_of(c: char) -> Option<u8> {
    let value = u32::from(c);
    (0x10_ff00..=0x10_ffff)
        .contains(&value)
        .then(|| (value - 0x10_ff00) as u8)
}

/// The internal (Bazel) bytes of a native string.
pub fn internal_bytes(text: &str) -> Cow<'_, [u8]> {
    if !text.chars().any(|c| byte_of(c).is_some()) {
        return Cow::Borrowed(text.as_bytes());
    }
    let mut out = Vec::with_capacity(text.len());
    for c in text.chars() {
        match byte_of(c) {
            Some(byte) => out.push(byte),
            None => {
                let mut buffer = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
    Cow::Owned(out)
}

/// `StringEncoding.internalToPlatform` with a UTF-8 platform encoding.
pub fn platform(text: &str) -> Cow<'_, str> {
    match internal_bytes(text) {
        Cow::Borrowed(_) => Cow::Borrowed(text),
        Cow::Owned(bytes) => Cow::Owned(String::from_utf8_lossy(&bytes).into_owned()),
    }
}

/// The action as Bazel spawns it: program, command, arguments and environment
/// converted with [`platform`]. Borrowed when nothing changes.
pub fn platform_action(action: &crate::action::Action) -> Cow<'_, crate::action::Action> {
    let changes = |text: &str| matches!(platform(text), Cow::Owned(_));
    if !changes(&action.command)
        && !action.program.as_deref().is_some_and(changes)
        && !action.arguments.iter().any(|argument| changes(argument))
        && !action
            .environment
            .iter()
            .any(|(key, value)| changes(key) || changes(value))
    {
        return Cow::Borrowed(action);
    }
    let mut converted = action.clone();
    converted.command = platform(&action.command).into_owned();
    converted.program = action
        .program
        .as_deref()
        .map(|program| platform(program).into_owned());
    converted.arguments = action
        .arguments
        .iter()
        .map(|argument| platform(argument).into_owned())
        .collect();
    converted.environment = action
        .environment
        .iter()
        .map(|(key, value)| (platform(key).into_owned(), platform(value).into_owned()))
        .collect();
    Cow::Owned(converted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lone_bytes_map_back_and_decode_with_replacement() {
        let ff = starlark::eval::bazel_byte_char(0xff);
        let c3 = starlark::eval::bazel_byte_char(0xc3);
        let a9 = starlark::eval::bazel_byte_char(0xa9);
        assert_eq!(internal_bytes(&format!("A{ff}")).as_ref(), b"A\xff");
        assert_eq!(platform(&format!("A{ff}")), "A\u{fffd}");
        // Bytes that only form UTF-8 together decode like Bazel's boundary.
        assert_eq!(platform(&format!("{c3}{a9}")), "é");
        assert!(matches!(platform("plain é"), Cow::Borrowed("plain é")));
        assert!(matches!(internal_bytes("plain"), Cow::Borrowed(b"plain")));
    }

    #[test]
    fn actions_are_converted_only_when_they_carry_lone_bytes() {
        let ff = starlark::eval::bazel_byte_char(0xff);
        let action = crate::action::Action {
            id: "a".into(),
            command: "echo ok".into(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            write: None,
            program: None,
            arguments: vec!["x".into()],
            environment: [("K".to_owned(), "v".to_owned())].into(),
            shell_strict: true,
        };
        assert!(matches!(platform_action(&action), Cow::Borrowed(_)));
        let mut bytes = action.clone();
        bytes.command = format!("printf {ff}");
        bytes.arguments = vec![format!("a{ff}")];
        bytes.environment = [(format!("K{ff}"), format!("v{ff}"))].into();
        let converted = platform_action(&bytes);
        assert_eq!(converted.command, "printf \u{fffd}");
        assert_eq!(converted.arguments, ["a\u{fffd}"]);
        assert_eq!(converted.environment["K\u{fffd}"], "v\u{fffd}");
    }
}
