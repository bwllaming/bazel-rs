//! Source validation for the explicitly UTF-8-only repository text profile.
//!
//! Native Starlark decodes numeric escapes into Unicode scalar values, whereas
//! Bazel octal escapes denote bytes. Decoded values cannot distinguish `"ÿ"`
//! from `"\377"`, so inspect original AST literal spans before evaluation. This
//! validates code, not runtime file/template bytes, and never transcodes values.

use starlark::syntax::AstModule;
use starlark::syntax::ast::{AstLiteral, Expr};

/// Reject source spellings incompatible with Bazel's UTF-8 repository profile.
///
/// Check every string expression, including unreachable bodies and defaults.
/// Load paths are not string expressions and retain their loader validation.
pub fn validate_source(ast: &AstModule) -> Result<(), String> {
    let mut expressions = Vec::new();
    ast.statement().visit_expr(|expr| expressions.push(expr));
    while let Some(expr) = expressions.pop() {
        if let Expr::Literal(AstLiteral::String(literal)) = &expr.node {
            let location = ast.file_span(literal.span);
            validate_literal(location.source_span())
                .map_err(|message| format!("{location}: {message}"))?;
        }
        expr.visit_expr(|child| expressions.push(child));
    }
    Ok(())
}

/// Conservative execution boundary until native byte-string semantics exist.
/// ASCII-only outputs are insufficient: Unicode indexing/length may already
/// have produced a different ASCII value. Guard producers before evaluation.
pub fn validate_execution_source(ast: &AstModule) -> Result<(), String> {
    validate_source(ast)?;
    let mut expressions = Vec::new();
    ast.statement().visit_expr(|expr| expressions.push(expr));
    while let Some(expr) = expressions.pop() {
        let reason = match &expr.node {
            Expr::Literal(AstLiteral::String(literal)) if !literal.node.is_ascii() => {
                Some("non-ASCII string literals are unsupported in direct repository execution")
            }
            Expr::Identifier(identifier) if identifier.node.ident == "chr" => {
                Some("chr references are unsupported in direct repository execution")
            }
            _ => None,
        };
        if let Some(reason) = reason {
            return Err(format!("{}: {reason}", ast.file_span(expr.span)));
        }
        expr.visit_expr(|child| expressions.push(child));
    }
    Ok(())
}

/// Inspect one complete lexer token, not decoded text or a whole source file.
pub(crate) fn validate_literal(spelling: &str) -> Result<(), String> {
    const MALFORMED: &str = "malformed string literal span in repository text profile";
    let (raw, quoted) = match spelling.as_bytes().first() {
        Some(b'r' | b'R') => (true, &spelling[1..]),
        _ => (false, spelling),
    };
    let quote = match quoted.as_bytes().first() {
        Some(b'\'') => "'",
        Some(b'"') => "\"",
        _ => return Err(MALFORMED.to_owned()),
    };
    let delimiter = match quoted.as_bytes().get(..3) {
        Some(b"'''") => "'''",
        Some(b"\"\"\"") => "\"\"\"",
        _ => quote,
    };
    let body = quoted
        .strip_prefix(delimiter)
        .and_then(|rest| rest.strip_suffix(delimiter))
        .ok_or_else(|| MALFORMED.to_owned())?;
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
        let start = offset;
        offset += 1;
        let escaped = *bytes.get(offset).ok_or_else(|| MALFORMED.to_owned())?;
        match escaped {
            b'0'..=b'7' => {
                let mut value = 0u16;
                let end = (offset + 3).min(bytes.len());
                while offset < end && matches!(bytes[offset], b'0'..=b'7') {
                    value = value * 8 + u16::from(bytes[offset] - b'0');
                    offset += 1;
                }
                let message = match value {
                    0..=127 => continue,
                    128..=255 => "non-UTF-8 byte escape in repository text profile is unsupported",
                    _ => "octal escape out of range",
                };
                return Err(format!("{message}: `{}`", &body[start..offset]));
            }
            // Bazel 9.3 has no hexadecimal or Unicode escape syntax, even when
            // native Starlark accepts it and the resulting character is ASCII.
            b'x' | b'u' | b'U' => {
                let width = match escaped {
                    b'x' => 2,
                    b'u' => 4,
                    _ => 8,
                };
                offset += 1;
                let end = (offset + width).min(bytes.len());
                while offset < end && bytes[offset].is_ascii_hexdigit() {
                    offset += 1;
                }
                return Err(format!(
                    "invalid Bazel escape sequence: `{}`",
                    &body[start..offset]
                ));
            }
            b'a' | b'b' | b'f' | b'n' | b'r' | b't' | b'v' | b'\\' | b'\'' | b'"' | b'\n' => {
                offset += 1
            }
            b'\r' if bytes.get(offset + 1) == Some(&b'\n') => offset += 2,
            _ => {
                // Offset follows an ASCII backslash, hence is a UTF-8 boundary.
                let character = body[offset..].chars().next().ok_or(MALFORMED)?;
                return Err(format!("invalid Bazel escape sequence: `\\{character}`"));
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

    #[test]
    fn literal_utf8_is_preserved() {
        check("value = 'éÿ東京😀'\n").unwrap();
    }

    #[test]
    fn execution_profile_rejects_unicode_even_when_computation_becomes_ascii() {
        for source in [
            "x = str(len('é'))\n",
            "x = 'é'[0]\n",
            "x = r'ÿ'\n",
            "def f():\n    return 'é'\n",
            "unused = 'é'\n",
        ] {
            assert!(
                validate_execution_source(&parse(source))
                    .unwrap_err()
                    .contains("non-ASCII")
            );
        }
    }

    #[test]
    fn execution_profile_rejects_chr_references_including_aliases_and_shadowing() {
        for source in [
            "x = str(len(chr(233)))\n",
            "f = chr\nx = f(233)\n",
            "def f(chr):\n    return chr\n",
            "x = [chr]\n",
        ] {
            assert!(
                validate_execution_source(&parse(source))
                    .unwrap_err()
                    .contains("chr references")
            );
        }
        validate_execution_source(&parse(
            "# é and chr in a comment\nx = 'chr'\ny = r'\\377'\nz = '\\\\377'\n",
        ))
        .unwrap();
    }

    #[test]
    fn high_byte_octal_is_unsupported() {
        for escape in [r"\200", r"\300", r"\377"] {
            let error = check(&format!("value = '{escape}'\n")).unwrap_err();
            assert!(
                error.contains("non-UTF-8 byte escape in repository text profile is unsupported")
            );
            assert!(error.contains(escape));
        }
    }

    #[test]
    fn octal_above_byte_range_is_invalid() {
        for escape in [r"\400", r"\777"] {
            let error = check(&format!("value = '{escape}'\n")).unwrap_err();
            assert!(error.contains("octal escape out of range"));
            assert!(error.contains(escape));
        }
    }

    #[test]
    fn all_hex_escapes_are_invalid_bazel_syntax() {
        for escape in [r"\x00", r"\x41", r"\x7f", r"\x80", r"\xff", r"\xFF"] {
            let error = check(&format!("value = '{escape}'\n")).unwrap_err();
            assert!(error.contains("invalid Bazel escape sequence"));
            assert!(error.contains(escape));
        }
    }

    #[test]
    fn unicode_escapes_are_invalid_bazel_syntax() {
        for escape in [r"\u0041", r"\u00ff", r"\U00000041", r"\U0001f600"] {
            let error = check(&format!("value = '{escape}'\n")).unwrap_err();
            assert!(error.contains("invalid Bazel escape sequence"));
            assert!(error.contains(escape));
        }
    }

    #[test]
    fn ascii_octal_including_nul_is_allowed() {
        check("value = '\\0\\00\\000\\7\\77\\177\\178\\078\\0000'\n").unwrap();
    }

    #[test]
    fn octal_consumes_at_most_three_digits() {
        check("value = '\\0377\\0777'\n").unwrap();
        let error = check("value = '\\3770'\n").unwrap_err();
        assert!(error.contains("`\\377`"));
        assert!(!error.contains("3770"));
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
        check("value = '\\\\377\\\\xff\\\\u00ff\\\\U00000041\\\\777'\n").unwrap();
        let error = check("value = '\\\\\\377'\n").unwrap_err();
        assert!(error.contains("non-UTF-8 byte escape"));
    }

    #[test]
    fn standard_escapes_and_escaped_quotes_are_allowed() {
        check("value = '\\a\\b\\f\\n\\r\\t\\v\\\\\\\'\\\"'\n").unwrap();
        let error = check("value = 'before\\\'after\\377'\n").unwrap_err();
        assert!(error.contains("non-UTF-8 byte escape"));
    }

    #[test]
    fn triple_quotes_and_line_continuations_keep_real_escapes() {
        check("value = '''first\\\nsecond\n\\177'''\n").unwrap();
        check("value = \"\"\"first\\\r\nsecond\"\"\"\n").unwrap();
        let error = check("value = \"\"\"first\\\nsecond\n\\xff\"\"\"\n").unwrap_err();
        assert!(error.contains("invalid Bazel escape sequence: `\\xff`"));
    }

    #[test]
    fn comments_are_not_string_tokens() {
        check("# '\\377' \\xff \\u00ff\nvalue = 'ÿ' # \\777\n").unwrap();
    }

    #[test]
    fn every_nested_expression_is_checked() {
        for source in [
            "value = ['ok', ['\\377']]\n",
            "value = {'key': {'\\377': 'value'}}\n",
            "def _private(value = '\\377'):\n    return value\n",
            "def _private():\n    return ['\\377']\n",
            "def _private():\n    '''docstring \\377'''\n    return None\n",
            "def _private():\n    if False:\n        value = '\\377'\n",
            "value = [item for item in ['\\377']]\n",
        ] {
            assert!(check(source).unwrap_err().contains("non-UTF-8 byte escape"));
        }
    }

    #[test]
    fn diagnostics_have_location_and_token_but_not_source_contents() {
        let error =
            check("# heading\nvalue = 'sensitive-prefix\\377sensitive-suffix'\n").unwrap_err();
        assert!(error.contains("repository.star:2:9"), "{error}");
        assert!(error.contains("`\\377`"), "{error}");
        assert!(!error.contains("sensitive"), "{error}");
    }

    #[test]
    fn identical_decoded_strings_have_different_source_acceptance() {
        fn decoded(ast: &AstModule) -> String {
            let mut expressions = Vec::new();
            ast.statement().visit_expr(|expr| expressions.push(expr));
            let Expr::Literal(AstLiteral::String(literal)) = &expressions[0].node else {
                panic!("expected a string literal");
            };
            literal.node.clone()
        }
        let utf8 = parse("'ÿ'\n");
        let octal = parse("'\\377'\n");
        let hex = parse("'\\xff'\n");
        assert_eq!(decoded(&utf8), decoded(&octal));
        assert_eq!(decoded(&utf8), decoded(&hex));
        validate_source(&utf8).unwrap();
        assert!(validate_source(&octal).is_err());
        assert!(validate_source(&hex).is_err());
    }

    #[test]
    fn unknown_escapes_fail_closed() {
        // Native parsing may itself reject these; the source helper must never
        // admit them if a parser dialect happens to accept them.
        for spelling in ["'\\q'", "'\\8'", "'\\é'", "'\\z'"] {
            assert!(
                validate_literal(spelling)
                    .unwrap_err()
                    .contains("invalid Bazel escape sequence")
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
}
