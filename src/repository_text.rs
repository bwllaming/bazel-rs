//! Source validation for direct repository/extension execution.
//!
//! Bazel 9.3 lexes a file as Latin-1 characters of its UTF-8 bytes
//! (`skyframe/StarlarkUtil.java:43-67`, `ParserInput.fromLatin1`), and
//! repository/extension evaluation here models those Java `char`s as Rust
//! characters U+0000..U+00FF that are written back as single bytes
//! (`repository_ctx.file`). Escapes keep that model exactly: an octal escape
//! `\NNN` denotes the char `NNN & 0xff` (`net/starlark/java/syntax/Lexer.java:
//! 372-395`), which the native lexer decodes to the same scalar U+0000..U+00FF.
//! Loaded `.bzl` files of that profile are decoded as Latin-1 before parsing
//! (`crate::loading`), so a raw non-ASCII source character is one char per
//! UTF-8 byte there too; only their `load()` labels must stay ASCII
//! ([`validate_execution_load_labels`]). MODULE.bazel is parsed once as UTF-8
//! for both models, so its raw non-ASCII literals remain rejected for execution
//! ([`validate_execution_source`]). Decoded values cannot distinguish `"ÿ"`
//! from `"\377"`, so original AST literal spans are inspected before
//! evaluation. This validates code, not runtime file/template bytes, and never
//! transcodes values.

use starlark::codemap::{Pos, Span};
use starlark::syntax::AstModule;
use starlark::syntax::ast::{AstLiteral, Expr};

/// Reject escape spellings that Bazel's lexer rejects (or that the native
/// lexer would decode differently), in every string expression, including
/// unreachable bodies and defaults. Load paths are not string expressions and
/// retain their loader validation.
///
/// Like Bazel's lexer (Lexer.java:389-403, `error(..., pos - 1)`), the error
/// points at the offending character: the escape letter, or the last digit of
/// an out-of-range octal escape. The earliest error in the file is reported.
pub fn validate_source(ast: &AstModule) -> Result<(), String> {
    let mut expressions = Vec::new();
    ast.statement().visit_expr(|expr| expressions.push(expr));
    let mut first: Option<(u32, String)> = None;
    while let Some(expr) = expressions.pop() {
        if let Expr::Literal(AstLiteral::String(literal)) = &expr.node {
            let location = ast.file_span(literal.span);
            if let Err((offset, message)) = validate_literal(location.source_span()) {
                let begin = literal.span.begin().get() + offset as u32;
                if first.as_ref().is_none_or(|(at, _)| begin < *at) {
                    let width = location.source_span()[offset..]
                        .chars()
                        .next()
                        .map_or(1, char::len_utf8) as u32;
                    let span = Span::new(Pos::new(begin), Pos::new(begin + width));
                    first = Some((begin, format!("{}: {message}", ast.file_span(span))));
                }
            }
        }
        expr.visit_expr(|child| expressions.push(child));
    }
    match first {
        Some((_, error)) => Err(error),
        None => Ok(()),
    }
}

/// Execution profile: Bazel-valid escapes plus ASCII-only source spelling.
/// Raw non-ASCII source characters are guarded before evaluation because
/// Unicode indexing/length may already have produced a different ASCII value.
/// Octal escapes up to `\377` are accepted: they denote the same Latin-1 char
/// in both models. `chr`/`ord` are not Bazel builtins at all; free references
/// fail in the resolver (`guarded_globals::check_universe`).
pub fn validate_execution_source(ast: &AstModule) -> Result<(), String> {
    validate_source(ast)?;
    let mut expressions = Vec::new();
    ast.statement().visit_expr(|expr| expressions.push(expr));
    while let Some(expr) = expressions.pop() {
        if let Expr::Literal(AstLiteral::String(literal)) = &expr.node
            && !ast.file_span(literal.span).source_span().is_ascii()
        {
            return Err(format!(
                "{}: non-ASCII string literals are unsupported in direct repository execution",
                ast.file_span(expr.span)
            ));
        }
        expr.visit_expr(|child| expressions.push(child));
    }
    Ok(())
}

/// A Latin-1-decoded `.bzl` of the execution profile: a `load()` label with a
/// non-ASCII character would name the Latin-1 decoding of its bytes as a path
/// natively, unlike Bazel, which converts it back to the original bytes.
pub fn validate_execution_load_labels(ast: &AstModule) -> Result<(), String> {
    for load in ast.loads() {
        if !load.module_id.is_ascii() {
            return Err(format!(
                "{}: non-ASCII load() labels are unsupported in direct repository execution",
                load.span
            ));
        }
    }
    Ok(())
}

/// Inspect one complete lexer token, not decoded text or a whole source file.
/// Errors carry the byte offset (within `spelling`) of the reported character.
pub(crate) fn validate_literal(spelling: &str) -> Result<(), (usize, String)> {
    const MALFORMED: &str = "malformed string literal span in repository text profile";
    let malformed = || (0, MALFORMED.to_owned());
    let (raw, quoted) = match spelling.as_bytes().first() {
        Some(b'r' | b'R') => (true, &spelling[1..]),
        _ => (false, spelling),
    };
    let quote = match quoted.as_bytes().first() {
        Some(b'\'') => "'",
        Some(b'"') => "\"",
        _ => return Err(malformed()),
    };
    let delimiter = match quoted.as_bytes().get(..3) {
        Some(b"'''") => "'''",
        Some(b"\"\"\"") => "\"\"\"",
        _ => quote,
    };
    let body = quoted
        .strip_prefix(delimiter)
        .and_then(|rest| rest.strip_suffix(delimiter))
        .ok_or_else(malformed)?;
    let body_start = spelling.len() - quoted.len() + delimiter.len();
    if raw {
        return Ok(());
    }

    let bytes = body.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] != b'\\' {
            offset += 1;
            continue;
        }
        offset += 1;
        let escaped = *bytes.get(offset).ok_or_else(malformed)?;
        match escaped {
            b'0'..=b'7' => {
                let mut value = 0u16;
                let end = (offset + 3).min(bytes.len());
                while offset < end && matches!(bytes[offset], b'0'..=b'7') {
                    value = value * 8 + u16::from(bytes[offset] - b'0');
                    offset += 1;
                }
                // Lexer.java:389-394: up to 0xff denotes the char `octal`
                // (U+0000..U+00FF, the same scalar the native lexer decodes).
                if value > 0xff {
                    return Err((
                        body_start + offset - 1,
                        "octal escape sequence out of range (maximum is \\377)".to_owned(),
                    ));
                }
            }
            b'a' | b'b' | b'f' | b'n' | b'r' | b't' | b'v' | b'\\' | b'\'' | b'"' | b'\n' => {
                offset += 1
            }
            b'\r' if bytes.get(offset + 1) == Some(&b'\n') => offset += 2,
            // Lexer.java:396-403: `\x`, `\u`, `\U`, `\N` and every other
            // character are invalid in Bazel 9.3, even where the native lexer
            // accepts them (and even when the decoded character is ASCII).
            _ => {
                // Offset follows an ASCII backslash, hence is a UTF-8 boundary.
                let character = body[offset..].chars().next().ok_or_else(malformed)?;
                return Err((
                    body_start + offset,
                    format!("invalid escape sequence: \\{character}. Use '\\\\' to insert '\\'."),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::syntax::Dialect;

    fn parse(source: &str) -> AstModule {
        AstModule::parse("repository.star", source.to_owned(), &Dialect::Standard)
            .unwrap_or_else(|error| panic!("test source must parse: {error:#}"))
    }

    fn check(source: &str) -> Result<(), String> {
        validate_source(&parse(source))
    }

    fn execution(source: &str) -> Result<(), String> {
        validate_execution_source(&parse(source))
    }

    const INVALID: &str = ". Use '\\\\' to insert '\\'.";

    #[test]
    fn literal_utf8_is_preserved_by_the_escape_check() {
        check("value = 'éÿ東京😀'\n").unwrap();
    }

    #[test]
    fn execution_profile_rejects_raw_unicode_even_when_computation_becomes_ascii() {
        for source in [
            "x = str(len('é'))\n",
            "x = 'é'[0]\n",
            "x = r'ÿ'\n",
            "def f():\n    return 'é'\n",
            "unused = 'é'\n",
            "x = '\\377é'\n",
        ] {
            assert!(
                execution(source).unwrap_err().contains("non-ASCII"),
                "{source}"
            );
        }
    }

    #[test]
    fn execution_profile_accepts_latin1_octal_escapes_and_chr_identifiers() {
        // strings.bzl:262-518 (bazel_lib) spells every byte as an octal escape.
        execution("INT_TO_CHAR = ['\\000', '\\177', '\\200', '\\351', '\\377']\n").unwrap();
        execution("x = '''\\377\n\\200'''\n").unwrap();
        // A loaded or locally defined `chr` is an ordinary identifier; free
        // references to Bazel's absent builtin fail in the resolver instead.
        execution("def chr(i):\n    return INT_TO_CHAR[i]\nx = chr(1)\n").unwrap();
        execution("# é and chr in a comment\nx = 'chr'\ny = r'\\377'\nz = '\\\\377'\n").unwrap();
    }

    #[test]
    fn octal_escapes_decode_to_the_same_latin1_scalar_natively() {
        fn decoded(ast: &AstModule) -> String {
            let mut expressions = Vec::new();
            ast.statement().visit_expr(|expr| expressions.push(expr));
            let Expr::Literal(AstLiteral::String(literal)) = &expressions[0].node else {
                panic!("expected a string literal");
            };
            literal.node.clone()
        }
        for value in 0u32..=0xff {
            let ast = parse(&format!("'\\{value:o}'\n"));
            validate_execution_source(&ast).unwrap();
            assert_eq!(decoded(&ast), char::from_u32(value).unwrap().to_string());
        }
        // A raw "ÿ" decodes to the same scalar but is two chars in Bazel.
        let raw = parse("'ÿ'\n");
        assert_eq!(decoded(&raw), "\u{ff}");
        assert!(validate_execution_source(&raw).is_err());
    }

    #[test]
    fn octal_above_byte_range_uses_bazel_lexer_text() {
        for escape in [r"\400", r"\777"] {
            let error = check(&format!("value = '{escape}'\n")).unwrap_err();
            assert!(
                error.ends_with("octal escape sequence out of range (maximum is \\377)"),
                "{error}"
            );
        }
    }

    #[test]
    fn hex_unicode_and_unknown_escapes_use_bazel_lexer_text() {
        for (escape, shown) in [
            (r"\x00", "\\x"),
            (r"\x41", "\\x"),
            (r"\xff", "\\x"),
            (r"\xFF", "\\x"),
            (r"\u0041", "\\u"),
            (r"\U0001f600", "\\U"),
            (r"\N", "\\N"),
            (r"\q", "\\q"),
        ] {
            let error = check(&format!("value = '{escape}'\n")).unwrap_err();
            assert!(
                error.ends_with(&format!("invalid escape sequence: {shown}{INVALID}")),
                "{error}"
            );
        }
    }

    #[test]
    fn ascii_octal_including_nul_is_allowed() {
        check("value = '\\0\\00\\000\\7\\77\\177\\178\\078\\0000'\n").unwrap();
    }

    #[test]
    fn octal_consumes_at_most_three_digits() {
        check("value = '\\0377\\0777\\3770'\n").unwrap();
        let error = check("value = '\\4000'\n").unwrap_err();
        assert!(error.contains("out of range"), "{error}");
    }

    #[test]
    fn raw_strings_do_not_interpret_escapes() {
        check("a = r'\\377\\xff\\u00ff\\777'\nb = r\"\"\"\\200\n\\x41\"\"\"\n").unwrap();
        // The helper recognizes either raw prefix without requiring that the
        // native parser support uppercase R in its selected dialect.
        validate_literal("R'\\377\\xff'").unwrap();
    }

    #[test]
    fn escaped_backslashes_are_not_numeric_escapes() {
        check("value = '\\\\777\\\\xff\\\\u00ff\\\\U00000041'\n").unwrap();
        let error = check("value = '\\\\\\777'\n").unwrap_err();
        assert!(error.contains("out of range"), "{error}");
    }

    #[test]
    fn standard_escapes_and_escaped_quotes_are_allowed() {
        check("value = '\\a\\b\\f\\n\\r\\t\\v\\\\\\\'\\\"'\n").unwrap();
        let error = check("value = 'before\\\'after\\xff'\n").unwrap_err();
        assert!(error.contains("invalid escape sequence: \\x"), "{error}");
    }

    #[test]
    fn triple_quotes_and_line_continuations_keep_real_escapes() {
        check("value = '''first\\\nsecond\n\\377'''\n").unwrap();
        check("value = \"\"\"first\\\r\nsecond\"\"\"\n").unwrap();
        let error = check("value = \"\"\"first\\\nsecond\n\\xff\"\"\"\n").unwrap_err();
        assert!(error.contains("invalid escape sequence: \\x"), "{error}");
    }

    #[test]
    fn comments_are_not_string_tokens() {
        check("# '\\777' \\xff \\u00ff\nvalue = 'ÿ' # \\777\n").unwrap();
    }

    #[test]
    fn every_nested_expression_is_checked() {
        for source in [
            "value = ['ok', ['\\xff']]\n",
            "value = {'key': {'\\xff': 'value'}}\n",
            "def _private(value = '\\xff'):\n    return value\n",
            "def _private():\n    return ['\\xff']\n",
            "def _private():\n    '''docstring \\xff'''\n    return None\n",
            "def _private():\n    if False:\n        value = '\\xff'\n",
            "value = [item for item in ['\\xff']]\n",
        ] {
            assert!(
                check(source)
                    .unwrap_err()
                    .contains("invalid escape sequence"),
                "{source}"
            );
        }
    }

    #[test]
    fn diagnostics_have_location_but_not_source_contents() {
        let error =
            check("# heading\nvalue = 'sensitive-prefix\\xffsensitive-suffix'\n").unwrap_err();
        // Bazel reports the escape character's column (Lexer.java:402).
        assert!(error.contains("repository.star:2:27"), "{error}");
        assert!(!error.contains("sensitive"), "{error}");
    }

    #[test]
    fn unknown_escapes_fail_closed() {
        // Native parsing may itself reject these; the source helper must never
        // admit them if a parser dialect happens to accept them.
        for spelling in ["'\\q'", "'\\8'", "'\\é'", "'\\z'"] {
            assert!(
                validate_literal(spelling)
                    .unwrap_err()
                    .1
                    .contains("invalid escape sequence")
            );
            if let Ok(ast) = AstModule::parse(
                "repository.star",
                format!("{spelling}\n"),
                &Dialect::Standard,
            ) {
                assert!(validate_source(&ast).is_err());
            }
        }
    }

    #[test]
    fn malformed_spans_return_errors_without_panicking() {
        for spelling in ["", "r", "unquoted", "'", "'unterminated", "'''", "'\\'"] {
            assert!(validate_literal(spelling).is_err(), "{spelling:?}");
        }
    }

    #[test]
    fn error_columns_point_at_the_escape_like_bazel() {
        for (source, column) in [
            ("V = \"\\x41\"\n", "1:7"),
            ("V = \"\\400\"\n", "1:9"),
            ("V = r'ok' + '\\q'\n", "1:15"),
            ("V = '''a\nb\\N'''\n", "2:3"),
        ] {
            let error = check(source).unwrap_err();
            assert!(
                error.contains(&format!("repository.star:{column}")),
                "{source}: {error}"
            );
        }
        // The earliest error in the file wins.
        let error = check("a = ['\\q', '\\x41']\n").unwrap_err();
        assert!(error.contains("1:8") && error.contains("\\q"), "{error}");
    }
}
