//! The pure PyBuiltins.regexMatch helper (Bazel 9.3), not an analysis API.
//!
//! Java String.matches is a *whole string* match. Never pass Java syntax
//! straight to Rust: in particular (?U) is Unicode classes, not ungreedy.
//! This allowlisted translator deliberately rejects constructs whose Java
//! semantics we do not implement. See docs/PY_REGEX_MATCH.md for the bounds.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use fancy_regex::{Regex, RegexBuilder};
use starlark::eval::Arguments;
use starlark::values::{Heap, Value};

use crate::language_modules::{failure, type_error};

const MAX_PATTERN: usize = 4096;
const MAX_SUBJECT: usize = 256 * 1024;
const MAX_WORK: usize = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 32;
const CACHE_ENTRIES: usize = 16;

pub(crate) fn call<'v>(args: &Arguments<'v, '_>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
    const FUNCTION: &str = "regex_match";
    // BuiltinFunction checks positional types before kwargs and excess
    // arguments. Both @Params are positional-only with an unbound default.
    let positions: Vec<_> = args.positions(heap)?.collect();
    let mut strings = [None, None];
    for (i, name) in ["subject", "pattern"].iter().enumerate() {
        if let Some(value) = positions.get(i) {
            strings[i] = Some(
                value
                    .unpack_str()
                    .ok_or_else(|| type_error(FUNCTION, name, *value, "string"))?,
            );
        }
    }
    if let Some((name, _)) = args.names_map()?.iter().next() {
        let name = name.as_str();
        let message = if matches!(name, "subject" | "pattern") {
            format!("regex_match() got named argument for positional-only parameter '{name}'")
        } else {
            format!(
                "regex_match() got unexpected keyword argument '{name}'{}",
                crate::native_rules::did_you_mean(name, &["subject", "pattern"])
            )
        };
        return Err(failure(message));
    }
    if positions.len() > 2 {
        return Err(failure(format!(
            "regex_match() accepts no more than 2 positional arguments but got {}",
            positions.len()
        )));
    }
    let [Some(subject), Some(pattern)] = strings else {
        return Err(failure(
            "py_internal.regex_match() requires subject and pattern: genuine Bazel 9.3 crashes on missing arguments (IllegalArgumentException: unbound reaches PyBuiltins.regexMatch); native reports an error instead".into(),
        ));
    };
    // The two existing interpreter storage models encode the *same* Java
    // chars differently. Adapt only at this Java API boundary, without
    // changing any Starlark value, source text, escape, or loader semantics.
    check_lengths(subject, pattern).map_err(failure)?;
    let subject = java_chars(subject);
    let pattern = java_chars(pattern);
    matches(&subject, &pattern)
        .map(Value::new_bool)
        .map_err(failure)
}

fn java_chars(text: &str) -> Cow<'_, str> {
    if text.is_ascii() || starlark::values::string::latin1_char_model() {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(
            crate::bazel_bytes::internal_bytes(text)
                .iter()
                .copied()
                .map(char::from)
                .collect(),
        )
    }
}

type Compiled = Result<Regex, String>;

#[derive(Default)]
struct Cache {
    entries: VecDeque<(String, Compiled)>,
    #[cfg(test)]
    compilations: usize,
}

impl Cache {
    fn get(&mut self, pattern: &str) -> &Compiled {
        if let Some(index) = self.entries.iter().position(|(key, _)| key == pattern) {
            let entry = self.entries.remove(index).expect("found");
            self.entries.push_back(entry);
        } else {
            // Evict BEFORE compiling; no cache-size + 1 peak or concurrent
            // duplicate compiles. Cache errors too, with the same bound.
            if self.entries.len() == CACHE_ENTRIES {
                self.entries.pop_front();
            }
            #[cfg(test)]
            {
                self.compilations += 1;
            }
            self.entries
                .push_back((pattern.to_owned(), compile(pattern)));
        }
        &self.entries.back().expect("inserted").1
    }
}

fn limit(message: &str) -> String {
    format!("py_internal.regex_match: native regular expression resource limit: {message}")
}

fn unsupported(message: &str) -> String {
    format!("py_internal.regex_match: unsupported Java regular expression construct: {message}")
}

fn check_lengths(subject: &str, pattern: &str) -> Result<(), String> {
    if pattern.len() > MAX_PATTERN {
        return Err(limit("pattern exceeds 4096 bytes"));
    }
    if subject.len() > MAX_SUBJECT || subject.len() * pattern.len().max(1) > MAX_WORK {
        return Err(limit(
            "subject exceeds 256 KiB or subject × pattern exceeds 16 MiB",
        ));
    }
    Ok(())
}

fn matches(subject: &str, pattern: &str) -> Result<bool, String> {
    check_lengths(subject, pattern)?;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    // Keep the lock through the bounded match: compiled regexes and their
    // internal scratch caches are never cloned per thread/call. No eval,
    // heap, rule context, repository identity, or on-disk cache dependency.
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "py_internal.regex_match: compiled pattern cache lock poisoned".to_owned())?;
    cache
        .get(pattern)
        .as_ref()
        .map_err(Clone::clone)?
        .is_match(subject)
        .map_err(|error| limit(&format!("match failed: {error}")))
}

fn compile(pattern: &str) -> Compiled {
    let mut parser = Parser {
        pattern,
        chars: pattern.char_indices().collect(),
        at: 0,
        dollars: 0,
        repeat_budget: 1,
    };
    let body = parser.sequence(Flags::default(), 0)?;
    // \z, not $, is necessary even when the subject ends in a newline.
    RegexBuilder::new(&format!(r"\A(?:{body})\z"))
        .delegate_size_limit(2 * 1024 * 1024)
        .delegate_dfa_size_limit(256 * 1024)
        .backtrack_limit(100_000)
        .build()
        .map_err(|error| {
            format!("py_internal.regex_match: native regex compilation failed (syntax or resource limit): {error}")
        })
}

#[derive(Clone, Copy, Default)]
struct Flags {
    unicode_classes: bool,
    dotall: bool,
    unix_lines: bool,
}

struct Parser<'a> {
    pattern: &'a str,
    chars: Vec<(usize, char)>,
    at: usize,
    dollars: usize,
    repeat_budget: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).map(|(_, c)| *c)
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += 1;
        Some(c)
    }

    fn invalid(&self, message: &str, at: usize) -> String {
        let byte = self.chars.get(at).map_or(self.pattern.len(), |(i, _)| *i);
        let index = self.pattern[..byte].encode_utf16().count();
        format!(
            "py_internal.regex_match: invalid Java regular expression: {message} near index {index}"
        )
    }

    fn sequence(&mut self, mut flags: Flags, depth: usize) -> Result<String, String> {
        if depth > MAX_DEPTH {
            return Err(limit("group nesting exceeds 32"));
        }
        let mut out = String::new();
        let mut atom = false;
        while let Some(c) = self.next() {
            match c {
                ')' if depth > 0 => return Ok(out),
                ')' => return Err(self.invalid("Unmatched closing ')'", self.at - 1)),
                '(' => {
                    let mut scoped = flags;
                    if self.peek() == Some('?') {
                        self.next();
                        if self.peek() == Some(':') {
                            self.next();
                        } else {
                            scoped = self.flags(flags)?;
                            if self.next() == Some(')') {
                                flags = scoped;
                                atom = false;
                                continue;
                            }
                        }
                    }
                    out.push_str("(?:");
                    out.push_str(&self.sequence(scoped, depth + 1)?);
                    out.push(')');
                    atom = true;
                }
                '[' => {
                    out.push_str(&self.class(flags)?);
                    atom = true;
                }
                '\\' => {
                    if self.peek() == Some('Q') {
                        self.next();
                        // Java quotes through \E (or the end of the pattern).
                        // Each quoted scalar remains an atom: \Qab\E+ = ab+.
                        while let Some(c) = self.next() {
                            if c == '\\' && self.peek() == Some('E') {
                                self.next();
                                break;
                            }
                            out.push_str(&literal(c));
                            atom = true;
                        }
                    } else {
                        let escaped = self.escape(flags, false)?;
                        out.push_str(&escaped.0);
                        atom = !matches!(escaped.0.as_str(), r"\A" | r"\z");
                    }
                }
                '.' => {
                    out.push_str(if flags.dotall {
                        r"(?s:.)"
                    } else if flags.unix_lines {
                        r"[^\n]"
                    } else {
                        r"[^\n\r\x{85}\x{2028}\x{2029}]"
                    });
                    atom = true;
                }
                '^' => {
                    out.push_str(r"\A");
                    atom = false;
                }
                '$' => {
                    self.dollars += 1;
                    // Bounds the number of fancy-regex delegated programs,
                    // each of which has its own compile and DFA memory limit.
                    if self.dollars > 1 {
                        return Err(unsupported("more than one $ anchor"));
                    }
                    out.push_str(if flags.unix_lines {
                        r"(?=\z|\n\z)"
                    } else {
                        r"(?=\z|\r\n\z|(?<!\r)\n\z|[\r\x{85}\x{2028}\x{2029}]\z)"
                    });
                    atom = false;
                }
                '*' | '+' | '?' | '{' => {
                    if !atom {
                        return Err(
                            self.invalid(&format!("Dangling meta character '{c}'"), self.at - 1)
                        );
                    }
                    if c == '{' {
                        out.push_str(&self.counted()?);
                    } else {
                        out.push(c);
                    }
                    if self.peek() == Some('+') {
                        return Err(unsupported("possessive quantifier"));
                    }
                    if self.peek() == Some('?') {
                        self.next();
                        out.push('?');
                    }
                    atom = false;
                }
                '|' => {
                    out.push('|');
                    atom = false;
                }
                _ => {
                    out.push_str(&literal(c));
                    atom = true;
                }
            }
        }
        if depth > 0 {
            Err(self.invalid("Unclosed group", self.at))
        } else {
            Ok(out)
        }
    }

    fn flags(&mut self, mut flags: Flags) -> Result<Flags, String> {
        let mut enabled = true;
        let mut seen = false;
        while let Some(c) = self.peek() {
            if matches!(c, ':' | ')') && seen {
                return Ok(flags);
            }
            self.next();
            match c {
                'U' => flags.unicode_classes = enabled,
                // Java Unicode case has no effect without i (rejected).
                'u' => (),
                's' => flags.dotall = enabled,
                'd' => flags.unix_lines = enabled,
                '-' if enabled => {
                    enabled = false;
                    continue;
                }
                _ => {
                    return Err(unsupported(
                        "group extension or inline flag (supported: U, u, s, d)",
                    ));
                }
            }
            seen = true;
        }
        Err(self.invalid("Unclosed group", self.at))
    }

    fn counted(&mut self) -> Result<String, String> {
        let start = self.at - 1;
        let low = self.number()?;
        let high = if self.peek() == Some(',') {
            self.next();
            if self.peek() == Some('}') {
                None
            } else {
                Some(self.number()?)
            }
        } else {
            Some(low)
        };
        if self.next() != Some('}') || high.is_some_and(|high| high < low) {
            return Err(self.invalid("Illegal repetition range", self.at.saturating_sub(1)));
        }
        self.repeat_budget = self
            .repeat_budget
            .saturating_mul(high.unwrap_or(low).max(1));
        if self.repeat_budget > 4096 {
            return Err(limit("product of counted repetitions exceeds 4096"));
        }
        Ok(self.chars[start..self.at].iter().map(|(_, c)| c).collect())
    }

    fn number(&mut self) -> Result<usize, String> {
        let start = self.at;
        let mut n = 0usize;
        while let Some(c @ '0'..='9') = self.peek() {
            self.next();
            n = n
                .saturating_mul(10)
                .saturating_add(c as usize - '0' as usize);
            if n > 4096 {
                return Err(limit("counted repetition exceeds 4096"));
            }
        }
        if self.at == start {
            return Err(self.invalid("Illegal repetition", self.at));
        }
        Ok(n)
    }

    fn class(&mut self, flags: Flags) -> Result<String, String> {
        let start = self.at - 1;
        let mut out = String::from("[");
        if self.peek() == Some('^') {
            self.next();
            out.push('^');
        }
        let mut first = true;
        while let Some(c) = self.peek() {
            if c == ']' && !first {
                self.next();
                out.push(']');
                return Ok(out);
            }
            first = false;
            let (text, scalar) = self.class_atom(flags)?;
            out.push_str(&text);
            if self.peek() == Some('-')
                && self.chars.get(self.at + 1).is_some_and(|(_, c)| *c != ']')
            {
                self.next();
                let (end_text, end) = self.class_atom(flags)?;
                let (Some(low), Some(high)) = (scalar, end) else {
                    return Err(unsupported(
                        "character-class range with a non-literal endpoint",
                    ));
                };
                if low > high {
                    return Err(self.invalid("Illegal character range", self.at - 1));
                }
                out.push('-');
                out.push_str(&end_text);
            }
        }
        Err(self.invalid("Unclosed character class", start))
    }

    fn class_atom(&mut self, flags: Flags) -> Result<(String, Option<char>), String> {
        match self.next() {
            Some('[') => Err(unsupported("nested/intersected Java character classes")),
            Some(c @ ('&' | '-' | '~')) if self.peek() == Some(c) => {
                Err(unsupported("character-class set operator"))
            }
            Some('\\') => self.escape(flags, true),
            Some(c) => Ok((literal(c), Some(c))),
            None => Err(self.invalid("Unclosed character class", self.at)),
        }
    }

    fn escape(&mut self, flags: Flags, in_class: bool) -> Result<(String, Option<char>), String> {
        let Some(c) = self.next() else {
            return Err(self.invalid("Unexpected internal error (trailing backslash)", self.at));
        };
        let class = match (c, flags.unicode_classes) {
            // Both the pinned JDK 25 and regex-syntax use Unicode 16.0.
            // Java U word = Alphabetic + M + Nd + Pc + Join_Control,
            // exactly regex-syntax's Unicode Perl word class (not XID).
            ('w', true) => Some(r"[\w]"),
            ('W', true) => Some(r"[^\w]"),
            ('d', true) => Some(r"[\p{Nd}]"),
            ('D', true) => Some(r"[^\p{Nd}]"),
            ('s', true) => Some(r"[\p{White_Space}]"),
            ('S', true) => Some(r"[^\p{White_Space}]"),
            ('w', false) => Some("[a-zA-Z_0-9]"),
            ('W', false) => Some("[^a-zA-Z_0-9]"),
            ('d', false) => Some("[0-9]"),
            ('D', false) => Some("[^0-9]"),
            ('s', false) => Some(r"[ \t\n\x{b}\f\r]"),
            ('S', false) => Some(r"[^ \t\n\x{b}\f\r]"),
            _ => None,
        };
        if let Some(class) = class {
            return Ok((class.into(), None));
        }
        let scalar = match c {
            't' => '\t',
            'n' => '\n',
            'r' => '\r',
            'f' => '\x0c',
            'a' => '\x07',
            'e' => '\x1b',
            'x' | 'u' => self.hex(c)?,
            'A' | 'z' if !in_class => return Ok((format!("\\{c}"), None)),
            // These are valid Java syntax, but outside this translator.
            '0'..='9'
            | 'p'
            | 'P'
            | 'b'
            | 'B'
            | 'G'
            | 'Z'
            | 'R'
            | 'h'
            | 'H'
            | 'v'
            | 'V'
            | 'k'
            | 'Q'
            | 'E'
            | 'c'
            | 'N'
            | 'X' => {
                return Err(unsupported(&format!("escape \\{c}")));
            }
            c if c.is_ascii_alphabetic() => {
                return Err(self.invalid("Illegal/unsupported escape sequence", self.at - 1));
            }
            c => c,
        };
        Ok((literal(scalar), Some(scalar)))
    }

    fn hex(&mut self, kind: char) -> Result<char, String> {
        let braced = kind == 'x' && self.peek() == Some('{');
        let count = if braced {
            self.next();
            6
        } else if kind == 'u' {
            4
        } else {
            2
        };
        let mut n = 0;
        let mut digits = 0;
        for _ in 0..count {
            let Some(digit) = self.peek().and_then(|c| c.to_digit(16)) else {
                break;
            };
            self.next();
            n = n * 16 + digit;
            digits += 1;
        }
        if digits == 0 || (braced && self.next() != Some('}')) || (!braced && digits != count) {
            return Err(self.invalid("Illegal hexadecimal escape sequence", self.at));
        }
        char::from_u32(n).ok_or_else(|| unsupported("surrogate or out-of-range code point escape"))
    }
}

fn literal(c: char) -> String {
    // No Rust-only syntax can leak through from a Java literal, even inside
    // a class (notably &, ~, #, and whitespace).
    format!(r"\x{{{:x}}}", u32::from(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_modules::tests::{fails, pass};

    const FILE: &str = "//tools/build_defs/python:regex.bzl";
    const RECEIPT: &str = include_str!("../tests/fixtures/py_regex_match/reference_receipt.json");

    fn receipt() -> serde_json::Value {
        serde_json::from_str(RECEIPT).unwrap()
    }

    #[test]
    fn genuine_elf_byte_strings_and_embedded_jdk_unicode_matrix() {
        for row in receipt()["matrix"].as_array().unwrap() {
            let subject = row["subject"].as_str().unwrap();
            let pattern = row["pattern"].as_str().unwrap();
            if !row["supported"].as_bool().unwrap() {
                assert!(
                    matches(subject, pattern)
                        .unwrap_err()
                        .contains("unsupported Java"),
                    "{row}"
                );
                continue;
            }
            // Genuine's Latin-1 view of UTF-8 source bytes, after the
            // boundary adapter (which does not mutate any Starlark value).
            let bytes = |text: &str| text.bytes().map(char::from).collect::<String>();
            assert_eq!(
                matches(&bytes(subject), &bytes(pattern)).unwrap(),
                row["expected"].as_bool().unwrap(),
                "ELF: {row}"
            );
            assert_eq!(
                matches(subject, pattern).unwrap(),
                row["unicode_expected"].as_bool().unwrap(),
                "embedded JDK: {row}"
            );
        }
    }

    #[test]
    fn genuine_binding_errors_and_declared_crash_boundaries() {
        for (name, case) in receipt()["cases"].as_object().unwrap() {
            let Some(expr) = case["expression"].as_str() else {
                continue;
            };
            if name == "build_direct" {
                continue;
            }
            let source = format!("f = py_internal.regex_match\nx = {expr}\n");
            let stderr = case["stderr"].as_str().unwrap();
            let message = if name.starts_with("missing") {
                "genuine Bazel 9.3 crashes on missing arguments"
            } else if name.starts_with("invalid_") {
                stderr
                    .split("Caused by: java.util.regex.PatternSyntaxException: ")
                    .nth(1)
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap()
            } else {
                stderr
                    .split("Error in regex_match: ")
                    .nth(1)
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap()
            };
            fails(FILE, &source, message);
        }
        pass(
            FILE,
            r#"
f = py_internal.regex_match
f("abc", "(?U)\\w+") == True or fail("bool")
f("a-b", "(?U)\\w+") == False or fail("bool")
f(*["abc", "[a-z]+"], **{}) == True or fail("star args")
f("é", "(?U)\\w+") == False or fail("UTF-8 source bytes")
f("\351", "(?U)\\w+") == True or fail("lone octal byte")
f("é", "é") == True or fail("pattern byte model")
"#,
        );
    }

    #[test]
    fn cache_reuses_success_and_error_and_evicts_before_compile() {
        let mut cache = Cache::default();
        for _ in 0..100 {
            assert!(
                cache
                    .get("(?U)\\w+")
                    .as_ref()
                    .unwrap()
                    .is_match("é")
                    .unwrap()
            );
        }
        assert_eq!(cache.compilations, 1);
        for _ in 0..100 {
            assert!(cache.get("[").is_err());
        }
        assert_eq!(cache.compilations, 2);
        for i in 0..CACHE_ENTRIES * 2 {
            assert!(cache.get(&format!("a{i}")).is_ok());
            assert!(cache.entries.len() <= CACHE_ENTRIES);
        }
        let before = cache.compilations;
        assert!(cache.get("[").is_err());
        assert_eq!(cache.compilations, before + 1);
    }

    #[test]
    fn resource_bounds_and_no_silent_java_construct_fallback() {
        for pattern in [
            r"\p{L}",
            r"\bword\b",
            r"\R",
            r"(a)\1",
            "(?=a)a",
            "(?>a)",
            "a++",
            "[a-z&&[^x]]",
            "(?i)a",
            "(?m)^a$",
            "(?x)a b",
            "(?U)\\w+$$",
            r"\uD800",
        ] {
            assert!(
                matches("a", pattern)
                    .unwrap_err()
                    .contains("unsupported Java"),
                "{pattern}"
            );
        }
        for pattern in [
            "a".repeat(MAX_PATTERN + 1),
            "(".repeat(MAX_DEPTH + 1),
            "a{4097}".into(),
            "(a{65}){65}".into(),
        ] {
            assert!(
                matches("", &pattern)
                    .unwrap_err()
                    .contains("resource limit")
            );
        }
        assert!(
            matches(&"a".repeat(MAX_SUBJECT + 1), "a*")
                .unwrap_err()
                .contains("resource limit")
        );
        assert!(
            matches(&"a".repeat(65536), &"a".repeat(300))
                .unwrap_err()
                .contains("resource limit")
        );
        // The only generated lookaround is $. Even a hostile ambiguous
        // repetition cannot consume unlimited backtracking time/stack.
        let outcome = matches(&"a".repeat(512), "(a|aa)*$b");
        assert!(matches!(outcome, Ok(false)) || outcome.unwrap_err().contains("resource limit"));
    }

    #[test]
    #[ignore = "bounded manual microbenchmark; run with --ignored --nocapture"]
    fn representative_filename_batch_benchmark() {
        let names = [
            "ansible",
            "ansible_core-2.19.0.dist-info",
            "_vendor",
            "plugins",
            "not-importable",
            "9pkg",
            "module_utils",
            "a.b",
        ];
        let start = std::time::Instant::now();
        let mut accepted = 0;
        for _ in 0..12_500 {
            for name in names {
                accepted += usize::from(matches(std::hint::black_box(name), "(?U)\\w+").unwrap());
            }
        }
        assert_eq!(accepted, 62_500);
        eprintln!("100000 cached filename matches: {:?}", start.elapsed());
        let start = std::time::Instant::now();
        for _ in 0..100 {
            assert!(compile("(?U)\\w+").unwrap().is_match("ansible").unwrap());
        }
        eprintln!(
            "100 uncached compile+matches (comparison only): {:?}",
            start.elapsed()
        );
    }
}
