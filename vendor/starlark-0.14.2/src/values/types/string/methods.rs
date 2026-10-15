/*
 * Copyright 2018 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

// Modified for native Bazel 9.3 string method compatibility; see vendor provenance.

//! Methods for the `string` type.

use std::cmp;

use starlark_derive::starlark_module;
use starlark_syntax::fast_string;
use starlark_syntax::fast_string::StrIndices;
use starlark_syntax::fast_string::convert_str_indices;

use crate as starlark;
use crate::environment::MethodsBuilder;
use crate::eval::Arguments;
use crate::eval::Evaluator;
use crate::values::Heap;
use crate::values::StringValue;
use crate::values::UnpackValue;
use crate::values::Value;
use crate::values::ValueOfUnchecked;
use crate::values::list::AllocList;
use crate::values::list::UnpackList;
use crate::values::none::NoneOr;
use crate::values::string::dot_format;
use crate::values::tuple::UnpackTuple;
use crate::values::type_repr::StarlarkTypeRepr;
use crate::values::types::string::iter::iterate_chars;
use crate::values::types::string::iter::iterate_codepoints;
use crate::values::typing::iter::StarlarkIter;

// This does not exists in rust, split would cut the string incorrectly and
// split_whitespace cannot take a n parameter.
fn splitn_whitespace(s: &str, maxsplit: usize) -> Vec<String> {
    let mut v = Vec::new();
    let mut cur = String::new();
    let mut split = 1;
    let mut eat_ws = true;
    for c in s.chars() {
        if split >= maxsplit && !eat_ws {
            cur.push(c)
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                v.push(cur);
                cur = String::new();
                split += 1;
                eat_ws = true;
            }
        } else {
            eat_ws = false;
            cur.push(c)
        }
    }
    if !cur.is_empty() {
        v.push(cur)
    }
    v
}

fn rsplitn_whitespace(s: &str, maxsplit: usize) -> Vec<String> {
    let mut v = Vec::new();
    let mut cur = String::new();
    let mut split = 1;
    let mut eat_ws = true;
    for c in s.chars().rev() {
        if split >= maxsplit && !eat_ws {
            cur.push(c)
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                v.push(cur.chars().rev().collect());
                cur = String::new();
                split += 1;
                eat_ws = true;
            }
        } else {
            eat_ws = false;
            cur.push(c)
        }
    }
    if !cur.is_empty() {
        v.push(cur.chars().rev().collect());
    }
    v.reverse();
    v
}

// Modified: the string model of the current thread. Bazel 9.3 keeps every
// string as Latin-1 Java chars. In BUILD/.bzl analysis evaluation the native
// interpreter stores the UTF-8 decoding of those bytes, so methods emulate
// Bazel byte by byte over the UTF-8 encoding (the default). Repository and
// extension evaluation stores each Java char directly as U+0000..U+00FF, so
// methods apply Bazel's char semantics to the characters themselves.
thread_local! {
    static LATIN1_CHAR_MODEL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the current thread evaluates in the Latin-1 char model.
pub fn latin1_char_model() -> bool {
    LATIN1_CHAR_MODEL.with(|model| model.get())
}

/// Restores the previous model of this thread when dropped.
pub struct Latin1CharModelGuard(bool);

impl Drop for Latin1CharModelGuard {
    fn drop(&mut self) {
        LATIN1_CHAR_MODEL.with(|model| model.set(self.0));
    }
}

/// Select the string model for evaluation on this thread until the guard drops.
pub fn enter_latin1_char_model(enabled: bool) -> Latin1CharModelGuard {
    Latin1CharModelGuard(LATIN1_CHAR_MODEL.with(|model| model.replace(enabled)))
}

/// Guava `CharMatcher.whitespace()` (Bazel `isspace`, StringModule.java:878).
fn is_guava_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t'..='\r'
            | ' '
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

/// `Character.isLetter` for one Java char.
fn java_is_letter(c: char) -> bool {
    c.is_alphabetic() && !c.is_numeric() && !matches!(c, '\u{345}' | '\u{2160}'..='\u{2188}')
}

/// `Character.toUpperCase(char)`/`toLowerCase(char)`: a single-char mapping.
fn java_simple_case(c: char, upper: bool) -> char {
    let mut mapped = if upper {
        c.to_uppercase().collect::<Vec<_>>()
    } else {
        c.to_lowercase().collect::<Vec<_>>()
    };
    if mapped.len() == 1 {
        mapped.pop().unwrap_or(c)
    } else {
        c
    }
}

/// Split `this` at `sep` from the left (Bazel StringModule.split, java:387-422):
/// `maxsplit` < 0 means no limit.
fn bazel_split<'a>(this: &'a str, sep: &str, maxsplit: i64) -> Vec<&'a str> {
    let mut res = Vec::new();
    let mut remaining = maxsplit;
    let mut start = 0;
    loop {
        let end = this[start..].find(sep).map(|i| start + i);
        let stop = remaining == 0;
        remaining -= 1;
        match end {
            Some(end) if !stop => {
                res.push(&this[start..end]);
                start = end + sep.len();
            }
            _ => {
                res.push(&this[start..]);
                return res;
            }
        }
    }
}

/// StringModule.rsplit (java:426-468).
fn bazel_rsplit<'a>(this: &'a str, sep: &str, maxsplit: i64) -> Vec<&'a str> {
    let mut res = Vec::new();
    let mut remaining = maxsplit;
    let mut end = this.len();
    loop {
        let start = this[..end].rfind(sep);
        let stop = remaining == 0;
        remaining -= 1;
        match start {
            Some(start) if !stop => {
                res.push(&this[start + sep.len()..end]);
                end = start;
            }
            _ => {
                res.push(&this[..end]);
                res.reverse();
                return res;
            }
        }
    }
}

/// Bazel's `split`/`rsplit` parameters: `sep` is a required string and
/// `maxsplit` an optional int (java:387-400, the interpreter's messages).
fn bazel_split_args<'a>(
    name: &str,
    sep: Option<Value<'a>>,
    maxsplit: Option<Value<'a>>,
) -> anyhow::Result<(&'a str, i64)> {
    let Some(sep) = sep else {
        return Err(anyhow::anyhow!(
            "{name}() missing 1 required positional argument: sep"
        ));
    };
    let Some(sep) = sep.unpack_str() else {
        return Err(anyhow::anyhow!(
            "in call to {name}(), parameter 'sep' got value of type '{}', want 'string'",
            sep.get_type()
        ));
    };
    if sep.is_empty() {
        return Err(anyhow::anyhow!("Empty separator"));
    }
    let maxsplit = match maxsplit {
        None => -1,
        Some(value) => match value.unpack_i32() {
            Some(value) => i64::from(value),
            None => {
                return Err(anyhow::anyhow!(
                    "in call to {name}(), parameter 'maxsplit' got value of type '{}', want 'int'",
                    value.get_type()
                ));
            }
        },
    };
    Ok((sep, maxsplit))
}

// Modified: Bazel 9.3 represents strings as UTF-8 byte strings, so its case
// mappings and character classes only recognise ASCII. `strip()` without
// arguments uses Bazel's `ASCII_WHITESPACE`; `isspace()` uses Guava's
// whitespace over single bytes, which only matches these ASCII bytes.
fn is_bazel_strip_whitespace(c: char) -> bool {
    matches!(c, '\t'..='\r' | '\x1c'..='\x1f' | ' ')
}

fn is_bazel_space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r' | b' ')
}

/// Whether Bazel's byte-wise `title()` treats the last UTF-8 byte of the
/// non-ASCII character `c` as a Latin-1 letter (`ª`, `µ` or `º`).
fn bazel_title_ends_with_letter(c: char) -> bool {
    // The last byte of a multi-byte UTF-8 sequence is `0x80 | (c & 0x3f)`.
    matches!(c as u32 & 0x3f, 0x2a | 0x35 | 0x3a)
}

#[derive(StarlarkTypeRepr, UnpackValue)]
enum StringOrTuple<'v> {
    String(&'v str),
    Tuple(UnpackTuple<&'v str>),
}

#[starlark_module]
pub(crate) fn string_methods(builder: &mut MethodsBuilder) {
    /// [string.elems](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·elems
    /// ): returns an iterable of the bytes values of a string.
    ///
    /// `S.elems()` returns an iterable value containing the
    /// sequence of numeric bytes values in the string S.
    ///
    /// To materialize the entire sequence of bytes, apply `list(...)` to the
    /// result.
    ///
    /// ```
    /// # starlark::assert::is_true(r#"
    /// list("Hello, 世界".elems()) == ["H", "e", "l", "l", "o", ",", " ", "世", "界"]
    /// # "#);
    /// ```
    fn elems<'v>(
        this: StringValue<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<ValueOfUnchecked<'v, UnpackList<String>>> {
        // Modified: Bazel returns a list of one-char strings (StringModule
        // .elems, java:926-940), so `len()` and indexing work.
        let _ = iterate_chars;
        let chars: Vec<String> = this.as_str().chars().map(String::from).collect();
        Ok(heap.alloc_typed_unchecked(AllocList(chars)).cast())
    }

    /// [string.capitalize](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string%C2%B7capitalize
    /// ): returns a copy of string S, where the first character (if any) is converted to uppercase;
    /// all other characters are converted to lowercase. Only ASCII letters are converted.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "hello, world!".capitalize() == "Hello, world!"
    /// "Hello, World!".capitalize() == "Hello, world!"
    /// "".capitalize() == ""
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn capitalize(this: &str) -> anyhow::Result<String> {
        let mut chars = this.chars();
        let mut result = String::with_capacity(this.len());
        if let Some(first) = chars.next() {
            // Modified: Character.toUpperCase of the first Java char (a
            // Latin-1 char in the repository model, java:526-532).
            if latin1_char_model() {
                result.push(java_simple_case(first, true));
            } else {
                result.push(first.to_ascii_uppercase());
            }
            result.push_str(&chars.as_str().to_ascii_lowercase());
        }
        Ok(result)
    }

    /// [string.codepoints](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·codepoints
    /// ): returns an iterable of the unicode codepoint of a string.
    ///
    /// `S.codepoints()` returns an iterable value containing the
    /// sequence of integer Unicode code points encoded by the string S.
    /// Each invalid code within the string is treated as if it encodes the
    /// Unicode replacement character, U+FFFD.
    ///
    /// By returning an iterable, not a list, the cost of decoding the string
    /// is deferred until actually needed; apply `list(...)` to the result to
    /// materialize the entire sequence.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// list("Hello, 世界".codepoints()) == [72, 101, 108, 108, 111, 44, 32, 19990, 30028]
    /// # "#);
    /// ```
    fn codepoints<'v>(
        this: StringValue<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<ValueOfUnchecked<'v, StarlarkIter<String>>> {
        Ok(iterate_codepoints(this, heap))
    }

    /// [string.count](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·count
    /// ): count the number of occurrences of a string in another string.
    ///
    /// `S.count(sub[, start[, end]])` returns the number of occurrences of
    /// `sub` within the string S, or, if the optional substring indices
    /// `start` and `end` are provided, within the designated substring of S.
    /// They are interpreted according to Skylark's [indexing conventions](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#indexing).
    ///
    /// This implementation does not count occurrence of `sub` in the string `S`
    /// that overlap other occurrence of S (which can happen if some suffix of S
    /// is a prefix of S). For instance, `"abababa".count("aba")` returns 2
    /// for `[aba]a[aba]`, not counting the middle occurrence: `ab[aba]ba`
    /// (this is following Python behavior).
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "hello, world!".count("o") == 2
    /// "abababa".count("aba") == 2
    /// "hello, world!".count("o", 7, 12) == 1  # in "world"
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn count(
        this: &str,
        #[starlark(require = pos)] needle: &str,
        #[starlark(require = pos, default = NoneOr::None)] start: NoneOr<i32>,
        #[starlark(require = pos, default = NoneOr::None)] end: NoneOr<i32>,
    ) -> anyhow::Result<i32> {
        if let Some(StrIndices { haystack, .. }) =
            convert_str_indices(this, start.into_option(), end.into_option())
        {
            Ok(fast_string::count_matches(haystack, needle) as i32)
        } else {
            Ok(0)
        }
    }

    /// [string.endswith](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·endswith
    /// ): determine if a string ends with a given suffix.
    ///
    /// `S.endswith(suffix[, start[, end]])` reports whether the string
    /// `S[start:end]` has the specified suffix.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "filename.sky".endswith(".sky") == True
    /// "hello".endswith("ell", 0, 4) == True
    /// "hello".endswith("lo", 3) == True
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn endswith(
        this: &str,
        #[starlark(require = pos)] suffix: StringOrTuple,
        #[starlark(require = pos, default = NoneOr::None)] start: NoneOr<i32>,
        #[starlark(require = pos, default = NoneOr::None)] end: NoneOr<i32>,
    ) -> anyhow::Result<bool> {
        let haystack = match convert_str_indices(this, start.into_option(), end.into_option()) {
            Some(StrIndices { haystack, .. }) => haystack,
            None => return Ok(false),
        };
        match suffix {
            StringOrTuple::String(x) => Ok(haystack.ends_with(x)),
            StringOrTuple::Tuple(xs) => Ok(xs.items.iter().any(|x| haystack.ends_with(x))),
        }
    }

    /// [string.find](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·find
    /// ): find a substring in a string.
    ///
    /// `S.find(sub[, start[, end]])` returns the index of the first
    /// occurrence of the substring `sub` within S.
    ///
    /// If either or both of `start` or `end` are specified,
    /// they specify a subrange of S to which the search should be restricted.
    /// They are interpreted according to Skylark's [indexing
    /// conventions](https://github.com/bazelbuild/starlark/blob/master/spec.md#indexing).
    ///
    /// If no occurrence is found, `found` returns -1.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "bonbon".find("on") == 1
    /// "bonbon".find("on", 2) == 4
    /// "bonbon".find("on", 2, 5) == -1
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn find(
        this: &str,
        #[starlark(require = pos)] needle: &str,
        #[starlark(require = pos, default = NoneOr::None)] start: NoneOr<i32>,
        #[starlark(require = pos, default = NoneOr::None)] end: NoneOr<i32>,
    ) -> anyhow::Result<i32> {
        if let Some(StrIndices { start, haystack }) =
            convert_str_indices(this, start.into_option(), end.into_option())
        {
            if let Some(index) = haystack.find(needle) {
                let index = fast_string::len(&haystack[..index]);
                return Ok((start + index).0 as i32);
            }
        }
        Ok(-1)
    }

    /// [string.format](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·format
    /// ): format a string.
    ///
    /// `S.format(*args, **kwargs)` returns a version of the format string S
    /// in which bracketed portions `{...}` are replaced
    /// by arguments from `args` and `kwargs`.
    ///
    /// Within the format string, a pair of braces `{{` or `}}` is treated as
    /// a literal open or close brace.
    /// Each unpaired open brace must be matched by a close brace `}`.
    /// The optional text between corresponding open and close braces
    /// specifies which argument to use and how to format it, and consists of
    /// three components, all optional:
    /// a field name, a conversion preceded by '`!`', and a format specifier
    /// preceded by '`:`'.
    ///
    /// ```text
    /// {field}
    /// {field:spec}
    /// {field!conv}
    /// {field!conv:spec}
    /// ```
    ///
    /// The *field name* may be either a decimal number or a keyword.
    /// A number is interpreted as the index of a positional argument;
    /// a keyword specifies the value of a keyword argument.
    /// If all the numeric field names form the sequence 0, 1, 2, and so on,
    /// they may be omitted and those values will be implied; however,
    /// the explicit and implicit forms may not be mixed.
    ///
    /// Modified: Bazel 9.3 has no *conversion* (`!r`/`!s`): such a field text
    /// names a keyword argument, so `"{0!r}".format(x)` fails with
    /// `Missing argument '0!r'` (FormatParser.java:110-116). Values are
    /// converted with `str(x)`.
    ///
    /// The *format specifier*, after a colon, specifies field width,
    /// alignment, padding, and numeric precision.
    /// Currently it must be empty, but it is reserved for future use.
    ///
    /// ```rust
    /// # starlark::assert::all_true(r#"
    /// "a {} c".format(3) == "a 3 c"
    /// "a{x}b{y}c{}".format(1, x=2, y=3) == "a2b3c1"
    /// "a{}b{}c".format(1, 2) == "a1b2c"
    /// "({1}, {0})".format("zero", "one") == "(one, zero)"
    /// "Is {0} {1}?".format(repr("heterological"), "heterological") == "Is \"heterological\" heterological?"
    /// "{x!r}".format(**{"x!r": "kw"}) == "kw"
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn format<'v>(
        this: &str,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StringValue<'v>> {
        let iter = args.positions(eval.heap())?;
        dot_format::format(
            this,
            iter,
            args.names()?,
            &mut eval.string_pool,
            eval.module_env.heap(),
        )
        .map_err(Into::into)
    }

    /// [string.index](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·index
    /// ): search a substring inside a string, failing on not found.
    ///
    /// `S.index(sub[, start[, end]])` returns the index of the first
    /// occurrence of the substring `sub` within S, like `S.find`, except
    /// that if the substring is not found, the operation fails.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "bonbon".index("on") == 1
    /// "bonbon".index("on", 2) == 4
    /// # "#);
    /// # starlark::assert::fail(r#"
    /// "bonbon".index("on", 2, 5)    # error: not found
    /// # "#, "not found");
    /// ```
    #[starlark(speculative_exec_safe)]
    fn index(
        this: &str,
        #[starlark(require = pos)] needle: &str,
        #[starlark(require = pos, default = NoneOr::None)] start: NoneOr<i32>,
        #[starlark(require = pos, default = NoneOr::None)] end: NoneOr<i32>,
    ) -> anyhow::Result<i32> {
        if let Some(StrIndices { start, haystack }) =
            convert_str_indices(this, start.into_option(), end.into_option())
        {
            if let Some(index) = haystack.find(needle) {
                let index = fast_string::len(&haystack[..index]);
                return Ok((start + index).0 as i32);
            }
        }
        Err(anyhow::anyhow!(
            "Substring '{}' not found in '{}'",
            needle,
            this
        ))
    }

    /// [string.isalnum](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·isalnum
    /// ): test if a string is composed only of letters and digits.
    ///
    /// `S.isalnum()` reports whether the string S is non-empty and consists
    /// only of ASCII letters and digits (`[a-zA-Z0-9]`).
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "base64".isalnum() == True
    /// "Catch-22".isalnum() == False
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn isalnum(this: &str) -> anyhow::Result<bool> {
        Ok(!this.is_empty() && this.bytes().all(|b| b.is_ascii_alphanumeric()))
    }

    /// [string.isalpha](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·isalpha
    /// ): test if a string is composed only of letters.
    ///
    /// `S.isalpha()` reports whether the string S is non-empty and consists
    /// only of ASCII letters (`[a-zA-Z]`).
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "ABC".isalpha() == True
    /// "Catch-22".isalpha() == False
    /// "".isalpha() == False
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn isalpha(this: &str) -> anyhow::Result<bool> {
        Ok(!this.is_empty() && this.bytes().all(|b| b.is_ascii_alphabetic()))
    }

    /// [string.isdigit](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·isdigit
    /// ): test if a string is composed only of digits.
    ///
    /// `S.isdigit()` reports whether the string S is non-empty and consists
    /// only of ASCII digits (`[0-9]`).
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "123".isdigit() == True
    /// "Catch-22".isdigit() == False
    /// "".isdigit() == False
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn isdigit(this: &str) -> anyhow::Result<bool> {
        Ok(!this.is_empty() && this.bytes().all(|b| b.is_ascii_digit()))
    }

    /// [string.islower](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·islower
    /// ): test if all letters of a string are lowercase.
    ///
    /// `S.islower()` reports whether the string S contains at least one cased
    /// ASCII letter, and all such letters are lowercase.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "hello, world".islower() == True
    /// "Catch-22".islower() == False
    /// "123".islower() == False
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn islower(this: &str) -> anyhow::Result<bool> {
        let mut result = false;
        for b in this.bytes() {
            if b.is_ascii_uppercase() {
                return Ok(false);
            } else if b.is_ascii_lowercase() {
                result = true;
            }
        }
        Ok(result)
    }

    /// [string.isspace](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·isspace
    /// ): test if all characters of a string are whitespaces.
    ///
    /// `S.isspace()` reports whether the string S is non-empty and consists
    /// only of the ASCII spaces `\t`, `\n`, `\x0b`, `\x0c`, `\r` and ` `.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "    ".isspace() == True
    /// "\r\t\n".isspace() == True
    /// "".isspace() == False
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn isspace(this: &str) -> anyhow::Result<bool> {
        if latin1_char_model() {
            return Ok(!this.is_empty() && this.chars().all(is_guava_whitespace));
        }
        Ok(!this.is_empty() && this.bytes().all(is_bazel_space))
    }

    /// [string.istitle](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·istitle
    /// ): test if the string is title cased.
    ///
    /// `S.istitle()` reports whether the string S contains at least one cased
    /// ASCII letter, every uppercase letter follows an uncased character and
    /// every lowercase letter follows a cased one.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "Hello, World!".istitle() == True
    /// "Catch-22".istitle() == True
    /// "HAL-9000".istitle() == False
    /// "123".istitle() == False
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn istitle(this: &str) -> anyhow::Result<bool> {
        // Bazel's right-to-left algorithm over bytes; non-ASCII bytes are uncased.
        // `left_must_be_cased`: None accepts anything; Some(b) requires the left
        // neighbour to be (b = true) or not to be (b = false) an ASCII letter.
        let mut left_must_be_cased: Option<bool> = None;
        let mut left_most_cased = b' ';
        for &b in this.as_bytes().iter().rev() {
            if left_must_be_cased.is_some_and(|cased| b.is_ascii_alphabetic() != cased) {
                return Ok(false);
            }
            left_must_be_cased = if b.is_ascii_lowercase() {
                Some(true)
            } else if b.is_ascii_uppercase() {
                Some(false)
            } else {
                None
            };
            if b.is_ascii_alphabetic() {
                left_most_cased = b;
            }
        }
        Ok(left_most_cased.is_ascii_uppercase())
    }

    /// [string.isupper](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·isupper
    /// ): test if all letters of a string are uppercase.
    ///
    /// `S.isupper()` reports whether the string S contains at least one cased
    /// ASCII letter, and all such letters are uppercase.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "HAL-9000".isupper() == True
    /// "Catch-22".isupper() == False
    /// "123".isupper() == False
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn isupper(this: &str) -> anyhow::Result<bool> {
        let mut result = false;
        for b in this.bytes() {
            if b.is_ascii_lowercase() {
                return Ok(false);
            } else if b.is_ascii_uppercase() {
                result = true;
            }
        }
        Ok(result)
    }

    /// [string.lower](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·lower
    /// ): convert a string to all lowercase.
    ///
    /// `S.lower()` returns a copy of the string S with ASCII letters converted
    /// to lowercase. Non-ASCII characters are unchanged.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "Hello, World!".lower() == "hello, world!"
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn lower(this: &str) -> anyhow::Result<String> {
        Ok(this.to_ascii_lowercase())
    }

    /// [string.join](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·join
    /// ): join elements with a separator.
    ///
    /// `S.join(iterable)` returns the string formed by concatenating each
    /// element of its argument, with a copy of the string S between
    /// successive elements. The argument must be an iterable whose elements
    /// are strings.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// ", ".join([]) == ""
    /// ", ".join(("x", )) == "x"
    /// ", ".join(["one", "two", "three"]) == "one, two, three"
    /// "a".join("ctmrn".elems()) == "catamaran"
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn join<'v>(
        this: &str,
        #[starlark(require = pos)] to_join: ValueOfUnchecked<'v, StarlarkIter<String>>,
        heap: Heap<'v>,
    ) -> starlark::Result<ValueOfUnchecked<'v, String>> {
        #[inline(always)]
        fn as_str<'v>(x: Value<'v>) -> crate::Result<StringValue<'v>> {
            StringValue::unpack_named_param(x, "to_join")
        }

        let mut it = to_join.get().iterate(heap)?;
        match it.next() {
            None => Ok(ValueOfUnchecked::new(Value::new_empty_string())),
            Some(x1) => {
                match it.next() {
                    None => {
                        // If there is a singleton we can avoid reallocation
                        Ok(as_str(x1)?.to_value_of_unchecked().cast())
                    }
                    Some(x2) => {
                        let s1 = as_str(x1)?.as_str();
                        let s2 = as_str(x2)?.as_str();
                        // guess towards the upper bound, since we throw away over-allocations quickly
                        // include a buffer (20 bytes)
                        let n = it.size_hint().0 + 2;
                        let guess =
                            (cmp::max(s1.len(), s2.len()) * n) + (this.len() * (n - 1)) + 20;
                        let mut r = String::with_capacity(guess);
                        r.push_str(s1);
                        r.push_str(this);
                        r.push_str(s2);
                        for x in it {
                            r.push_str(this);
                            r.push_str(as_str(x)?.as_str());
                        }
                        Ok(heap.alloc_typed_unchecked(r))
                    }
                }
            }
        }
    }

    /// [string.lstrip](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·lstrip
    /// ): trim leading whitespaces.
    ///
    /// `S.lstrip()` returns a copy of the string S with leading ASCII whitespace
    /// (`\t` to `\r`, `\x1c` to `\x1f` and ` `) removed.
    /// In most cases instead of passing an argument you should use `removeprefix`.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "  hello  ".lstrip() == "hello  "
    /// "x!hello  ".lstrip("!x ") == "hello  "
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn lstrip<'v>(
        this: StringValue<'v>,
        #[starlark(require = pos, default = NoneOr::None)] chars: NoneOr<&str>,
        heap: Heap<'v>,
    ) -> anyhow::Result<StringValue<'v>> {
        // Modified: Bazel accepts an explicit `None` for the default whitespace.
        let res = match chars.into_option() {
            None => this.trim_start_matches(is_bazel_strip_whitespace),
            Some(s) => this.trim_start_matches(|c| s.contains(c)),
        };
        if res.len() == this.len() {
            Ok(this)
        } else {
            Ok(heap.alloc_str(res))
        }
    }

    /// [string.partition](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·partition
    /// ): partition a string in 3 components
    ///
    /// `S.partition(x = " ")` splits string S into three parts and returns them
    /// as a tuple: the portion before the first occurrence of string `x`,
    /// `x` itself, and the portion following it.
    /// If S does not contain `x`, `partition` returns `(S, "", "")`.
    ///
    /// `partition` fails if `x` is not a string, or is the empty string.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "one/two/three".partition("/") == ("one", "/", "two/three")
    /// "one".partition("/") == ("one", "", "")
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn partition<'v>(
        this: StringValue<'v>,
        #[starlark(require = pos)] needle: StringValue<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<(StringValue<'v>, StringValue<'v>, StringValue<'v>)> {
        if needle.is_empty() {
            return Err(anyhow::anyhow!(
                "Empty separator cannot be used for partitioning"
            ));
        }
        if let Some(offset) = this.find(needle.as_str()) {
            let offset2 = offset + needle.len();
            Ok((
                heap.alloc_str(this.get(..offset).unwrap()),
                needle,
                heap.alloc_str(this.get(offset2..).unwrap()),
            ))
        } else {
            let empty = StringValue::default();
            Ok((this, empty, empty))
        }
    }

    /// [string.replace](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·replace
    /// ): replace all occurrences of a substring.
    ///
    /// `S.replace(old, new[, count])` returns a copy of string S with all
    /// occurrences of substring `old` replaced by `new`. If the optional
    /// argument `count`, which must be an `int`, is non-negative, it
    /// specifies a maximum number of occurrences to replace.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "banana".replace("a", "o") == "bonono"
    /// "banana".replace("a", "o", 2) == "bonona"
    /// "banana".replace("z", "x") == "banana"
    /// "banana".replace("", "x") == "xbxaxnxaxnxax"
    /// "banana".replace("", "x", 2) == "xbxanana"
    /// "".replace("", "x") == "x"
    /// # "# );
    /// # starlark::assert::fail(r#"
    /// "banana".replace("a", "o", -2)  # error: argument was negative
    /// # "#, "argument was negative");
    /// ```
    #[starlark(speculative_exec_safe)]
    fn replace<'v>(
        this: StringValue<'v>,
        #[starlark(require = pos)] old: &str,
        #[starlark(require = pos)] new: &str,
        #[starlark(require = pos)] count: Option<i32>,
        heap: Heap<'v>,
    ) -> anyhow::Result<StringValue<'v>> {
        match count {
            Some(count) if count >= 0 => {
                Ok(heap.alloc_str(&this.replacen(old, new, count as usize)))
            }
            Some(count) => Err(anyhow::anyhow!(
                "Replace final argument was negative '{}'",
                count
            )),
            None => {
                // Optimise `replace` using the Rust standard library definition,
                // but avoiding redundant allocation in the last step
                let x = this.as_str();
                let mut result = String::new();
                let mut last_end = 0;
                for (start, part) in x.match_indices(old) {
                    result.push_str(unsafe { x.get_unchecked(last_end..start) });
                    result.push_str(new);
                    last_end = start + part.len();
                }
                if result.is_empty() && last_end == 0 {
                    Ok(this)
                } else {
                    Ok(heap
                        .alloc_str_concat(&result, unsafe { x.get_unchecked(last_end..x.len()) }))
                }
            }
        }
    }

    /// [string.rfind](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·rfind
    /// ): find the last index of a substring.
    ///
    /// `S.rfind(sub[, start[, end]])` returns the index of the substring `sub`
    /// within S, like `S.find`, except that `rfind` returns the index of
    /// the substring's _last_ occurrence.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "bonbon".rfind("on") == 4
    /// "bonbon".rfind("on", None, 5) == 1
    /// "bonbon".rfind("on", 2, 5) == -1
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn rfind(
        this: &str,
        #[starlark(require = pos)] needle: &str,
        #[starlark(require = pos, default = NoneOr::None)] start: NoneOr<i32>,
        #[starlark(require = pos, default = NoneOr::None)] end: NoneOr<i32>,
    ) -> anyhow::Result<i32> {
        if let Some(StrIndices { start, haystack }) =
            convert_str_indices(this, start.into_option(), end.into_option())
        {
            if let Some(index) = haystack.rfind(needle) {
                let index = fast_string::len(&haystack[..index]);
                return Ok((start + index).0 as i32);
            }
        }
        Ok(-1)
    }

    /// [string.rindex](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·rindex
    /// ): find the last index of a substring, failing on not found.
    ///
    /// `S.rindex(sub[, start[, end]])` returns the index of the substring `sub`
    /// within S, like `S.index`, except that `rindex` returns the index of
    /// the substring's _last_ occurrence.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "bonbon".rindex("on") == 4
    /// "bonbon".rindex("on", None, 5) == 1  # in "bonbo"
    /// # "#);
    /// # starlark::assert::fail(r#"
    /// "bonbon".rindex("on", 2, 5) #   error: not found
    /// # "#, "not found");
    /// ```
    #[starlark(speculative_exec_safe)]
    fn rindex(
        this: &str,
        #[starlark(require = pos)] needle: &str,
        #[starlark(require = pos, default = NoneOr::None)] start: NoneOr<i32>,
        #[starlark(require = pos, default = NoneOr::None)] end: NoneOr<i32>,
    ) -> anyhow::Result<i32> {
        if let Some(StrIndices { start, haystack }) =
            convert_str_indices(this, start.into_option(), end.into_option())
        {
            if let Some(index) = haystack.rfind(needle) {
                let index = fast_string::len(&haystack[..index]);
                return Ok((start + index).0 as i32);
            }
        }
        Err(anyhow::anyhow!(
            "Substring '{}' not found in '{}'",
            needle,
            this
        ))
    }

    /// [string.rpartition](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·rpartition
    /// ): partition a string in 3 elements.
    ///
    /// `S.rpartition([x = ' '])` is like `partition`, but splits `S` at the
    /// last occurrence of `x`.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "one/two/three".rpartition("/") == ("one/two", "/", "three")
    /// "one".rpartition("/") == ("", "", "one")
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn rpartition<'v>(
        this: StringValue<'v>,
        #[starlark(require = pos)] needle: StringValue<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<(StringValue<'v>, StringValue<'v>, StringValue<'v>)> {
        if needle.is_empty() {
            return Err(anyhow::anyhow!(
                "Empty separator cannot be used for partitioning"
            ));
        }
        if let Some(offset) = this.rfind(needle.as_str()) {
            let offset2 = offset + needle.len();
            Ok((
                heap.alloc_str(this.get(..offset).unwrap()),
                needle,
                heap.alloc_str(this.get(offset2..).unwrap()),
            ))
        } else {
            let empty = StringValue::default();
            Ok((empty, empty, this))
        }
    }

    /// [string.rsplit](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·rsplit
    /// ): splits a string into substrings.
    ///
    /// `S.rsplit(sep[, maxsplit])` splits a string into substrings like
    /// `S.split`, except that when a maximum number of splits is specified,
    /// `rsplit` chooses the rightmost splits. Modified: as in Bazel 9.3, `sep`
    /// is a required string.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "banana".rsplit("n") == ["ba", "a", "a"]
    /// "banana".rsplit("n", 1) == ["bana", "a"]
    /// "one two  three".rsplit(" ", 1) == ["one two ", "three"]
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn rsplit<'v>(
        this: &str,
        sep: Option<Value<'v>>,
        maxsplit: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<ValueOfUnchecked<'v, UnpackList<String>>> {
        // Modified: Bazel 9.3 requires `sep` (StringModule.rsplit, java:426-468).
        let _ = rsplitn_whitespace;
        let (sep, maxsplit) = bazel_split_args("rsplit", sep, maxsplit)?;
        Ok(heap
            .alloc_typed_unchecked(AllocList(bazel_rsplit(this, sep, maxsplit)))
            .cast())
    }

    /// [string.rstrip](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·rstrip
    /// ): trim trailing whitespace.
    ///
    /// `S.rstrip()` returns a copy of the string S with trailing ASCII whitespace
    /// (`\t` to `\r`, `\x1c` to `\x1f` and ` `) removed.
    /// In most cases instead of passing an argument you should use `removesuffix`.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "  hello  ".rstrip() == "  hello"
    /// "  hello!x".rstrip(" x!") == "  hello"
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn rstrip<'v>(
        this: StringValue<'v>,
        #[starlark(require = pos, default = NoneOr::None)] chars: NoneOr<&str>,
        heap: Heap<'v>,
    ) -> anyhow::Result<StringValue<'v>> {
        // Modified: Bazel accepts an explicit `None` for the default whitespace.
        let res = match chars.into_option() {
            None => this.trim_end_matches(is_bazel_strip_whitespace),
            Some(s) => this.trim_end_matches(|c| s.contains(c)),
        };
        if res.len() == this.len() {
            Ok(this)
        } else {
            Ok(heap.alloc_str(res))
        }
    }

    /// [string.split](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·split
    /// ): split a string in substrings.
    ///
    /// `S.split(sep [, maxsplit])` returns the list of substrings of S,
    /// splitting at occurrences of the delimiter string `sep`.
    ///
    /// Consecutive occurrences of `sep` are considered to delimit empty
    /// strings, so `'food'.split('o')` returns `['f', '', 'd']`.
    /// Splitting an empty string with a specified separator returns `['']`.
    /// If `sep` is the empty string, `split` fails.
    ///
    /// Modified: as in Bazel 9.3 (StringModule.split), `sep` is a required
    /// string; there is no whitespace splitting.
    ///
    /// If `maxsplit` is given and non-negative, it specifies a maximum number
    /// of splits.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "one two  three".split(" ") == ["one", "two", "", "three"]
    /// "one two  three".split(" ", 1) == ["one", "two  three"]
    /// "banana".split("n") == ["ba", "a", "a"]
    /// "banana".split("n", 1) == ["ba", "ana"]
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn split<'v>(
        this: &str,
        sep: Option<Value<'v>>,
        maxsplit: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<ValueOfUnchecked<'v, UnpackList<String>>> {
        // Modified: Bazel 9.3 requires `sep` (StringModule.split, java:387-422);
        // there is no whitespace splitting.
        let _ = splitn_whitespace;
        let (sep, maxsplit) = bazel_split_args("split", sep, maxsplit)?;
        Ok(heap
            .alloc_typed_unchecked(AllocList(bazel_split(this, sep, maxsplit)))
            .cast())
    }

    /// [string.splitlines](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·splitlines
    /// ): return the list of lines of a string.
    ///
    /// `S.splitlines([keepends])` returns a list whose elements are the
    /// successive lines of S, that is, the strings formed by splitting S at
    /// line terminators ('\n', '\r' or '\r\n').
    ///
    /// The optional argument, `keepends`, is interpreted as a Boolean.
    /// If true, line terminators are preserved in the result, though
    /// the final element does not necessarily end with a line terminator.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "one\n\ntwo".splitlines() == ["one", "", "two"]
    /// "one\n\ntwo".splitlines(True) == ["one\n", "\n", "two"]
    /// "a\nb".splitlines() == ["a", "b"]
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn splitlines<'v>(
        this: &str,
        #[starlark(require = pos, default = false)] keepends: bool,
        heap: Heap<'v>,
    ) -> anyhow::Result<Vec<StringValue<'v>>> {
        let mut s = this;
        let mut lines: Vec<StringValue> = Vec::new();
        loop {
            if let Some(x) = s.find(['\n', '\r']) {
                let y = x;
                let x = match s.get(y..y + 2) {
                    Some("\r\n") => y + 2,
                    _ => y + 1,
                };
                if keepends {
                    lines.push(heap.alloc_str(s.get(..x).unwrap()))
                } else {
                    lines.push(heap.alloc_str(s.get(..y).unwrap()))
                }
                if x == s.len() {
                    return Ok(lines);
                }
                s = s.get(x..).unwrap();
            } else {
                if !s.is_empty() {
                    lines.push(heap.alloc_str(s));
                }
                return Ok(lines);
            }
        }
    }

    /// [string.startswith](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·startswith
    /// ): test whether a string starts with a given prefix.
    ///
    /// `S.startswith(prefix[, start[, end]])` reports whether the string
    /// `S[start:end]` has the specified prefix.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "filename.sky".startswith("filename") == True
    /// "filename.sky".startswith("sky") == False
    /// 'abc'.startswith(('a', 'A')) == True
    /// 'ABC'.startswith(('a', 'A')) == True
    /// 'def'.startswith(('a', 'A')) == False
    /// "//foo".startswith("//", 0, 2) == True
    /// "//foo".startswith("foo", 2) == True
    /// "hello".startswith("ell", 1, 4) == True
    /// "hello".startswith("ell", 2, 4) == False
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn startswith(
        this: &str,
        #[starlark(require = pos)] prefix: StringOrTuple,
        #[starlark(require = pos, default = NoneOr::None)] start: NoneOr<i32>,
        #[starlark(require = pos, default = NoneOr::None)] end: NoneOr<i32>,
    ) -> anyhow::Result<bool> {
        let haystack = match convert_str_indices(this, start.into_option(), end.into_option()) {
            Some(StrIndices { haystack, .. }) => haystack,
            None => return Ok(false),
        };
        match prefix {
            StringOrTuple::String(x) => Ok(haystack.starts_with(x)),
            StringOrTuple::Tuple(xs) => Ok(xs.items.iter().any(|x| haystack.starts_with(x))),
        }
    }

    /// [string.strip](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·strip
    /// ): trim leading and trailing whitespaces.
    ///
    /// `S.strip()` returns a copy of the string S with leading and trailing
    /// ASCII whitespace (`\t` to `\r`, `\x1c` to `\x1f` and ` `) removed.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "  hello  ".strip() == "hello"
    /// "xxhello!!".strip("x!") == "hello"
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn strip<'v>(
        this: StringValue<'v>,
        #[starlark(require = pos, default = NoneOr::None)] chars: NoneOr<&str>,
        heap: Heap<'v>,
    ) -> anyhow::Result<StringValue<'v>> {
        // Modified: Bazel accepts an explicit `None` for the default whitespace.
        let res = match chars.into_option() {
            None => this.trim_matches(is_bazel_strip_whitespace),
            Some(s) => this.trim_matches(|c| s.contains(c)),
        };
        if res.len() == this.len() {
            Ok(this)
        } else {
            Ok(heap.alloc_str(res))
        }
    }

    /// [string.title](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·title
    /// ): convert a string to title case.
    ///
    /// `S.title()` returns a copy of the string S with letters converted to
    /// titlecase.
    ///
    /// ASCII letters are converted to uppercase at the start of words, lowercase
    /// elsewhere. Non-ASCII characters are unchanged; as in Bazel's byte-wise
    /// implementation, one whose last UTF-8 byte encodes Latin-1 `ª`, `µ` or `º`
    /// counts as a letter for the following character.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "hElLo, WoRlD!".title() == "Hello, World!"
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn title(this: &str) -> anyhow::Result<String> {
        let mut previous_was_letter = false;
        let mut result = String::with_capacity(this.len());
        if latin1_char_model() {
            // StringModule.title (java:535-560) over Java chars.
            for c in this.chars() {
                let letter = java_is_letter(c);
                if letter && previous_was_letter && c.is_uppercase() {
                    result.push(java_simple_case(c, false));
                } else if letter && !previous_was_letter && c.is_lowercase() {
                    result.push(java_simple_case(c, true));
                } else {
                    result.push(c);
                }
                previous_was_letter = letter;
            }
            return Ok(result);
        }
        for c in this.chars() {
            if c.is_ascii() {
                if previous_was_letter && c.is_ascii_uppercase() {
                    result.push(c.to_ascii_lowercase());
                } else if !previous_was_letter && c.is_ascii_lowercase() {
                    result.push(c.to_ascii_uppercase());
                } else {
                    result.push(c);
                }
                previous_was_letter = c.is_ascii_alphabetic();
            } else {
                result.push(c);
                previous_was_letter = bazel_title_ends_with_letter(c);
            }
        }
        Ok(result)
    }

    /// [string.upper](
    /// https://github.com/bazelbuild/starlark/blob/master/spec.md#string·upper
    /// ): convert a string to all uppercase.
    ///
    /// `S.upper()` returns a copy of the string S with ASCII letters converted
    /// to uppercase. Non-ASCII characters are unchanged.
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "Hello, World!".upper() == "HELLO, WORLD!"
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn upper(this: &str) -> anyhow::Result<String> {
        Ok(this.to_ascii_uppercase())
    }

    /// [string.removeprefix](
    /// https://docs.python.org/3.9/library/stdtypes.html#str.removeprefix
    /// ): remove a prefix from a string. _Not part of standard Starlark._
    ///
    /// If the string starts with the prefix string, return `string[len(prefix):]`.
    /// Otherwise, return a copy of the original string:
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "Hello, World!".removeprefix("Hello") == ", World!"
    /// "Hello, World!".removeprefix("Goodbye") == "Hello, World!"
    /// "Hello".removeprefix("Hello") == ""
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn removeprefix<'v>(
        this: StringValue<'v>,
        #[starlark(require = pos)] prefix: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<StringValue<'v>> {
        let x = this.as_str();
        if x.starts_with(prefix) && !prefix.is_empty() {
            Ok(heap.alloc_str(&x[prefix.len()..]))
        } else {
            Ok(this)
        }
    }

    /// [string.removesuffix](
    /// https://docs.python.org/3.9/library/stdtypes.html#str.removesuffix
    /// ): remove a prefix from a string. _Not part of standard Starlark._
    ///
    /// If the string starts with the prefix string, return `string[len(prefix):]`.
    /// Otherwise, return a copy of the original string:
    ///
    /// ```
    /// # starlark::assert::all_true(r#"
    /// "Hello, World!".removesuffix("World!") == "Hello, "
    /// "Hello, World!".removesuffix("World") == "Hello, World!"
    /// "Hello".removesuffix("Hello") == ""
    /// # "#);
    /// ```
    #[starlark(speculative_exec_safe)]
    fn removesuffix<'v>(
        this: StringValue<'v>,
        #[starlark(require = pos)] suffix: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<StringValue<'v>> {
        let x = this.as_str();
        if x.ends_with(suffix) && !suffix.is_empty() {
            Ok(heap.alloc_str(&x[..x.len() - suffix.len()]))
        } else {
            Ok(this)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::assert;

    #[test]
    fn test_error_codes() {
        assert::fail(r#""bonbon".index("on", 2, 5)"#, "not found in");
        assert::fail(r#"("banana".replace("a", "o", -2))"#, "negative");
        assert::fail(r#""bonbon".rindex("on", 2, 5)"#, "not found in");
    }

    #[test]
    fn test_count() {
        assert::eq("'abc'.count('a', 10, -10)", "0");
    }

    #[test]
    fn test_find() {
        assert::eq("'Троянская война окончена'.find('война')", "10");
    }

    #[test]
    fn test_bazel_split_requires_sep() {
        // Modified: StringModule.split/rsplit (java:387-468).
        assert::all_true(
            r#"
"a,b,,c".split(",") == ["a", "b", "", "c"]
"a,b,c".split(sep = ",", maxsplit = 1) == ["a", "b,c"]
"a,b,c".rsplit(",", 1) == ["a,b", "c"]
"a,b,c".split(",", -1) == ["a", "b", "c"]
"a,b,c".split(",", 0) == ["a,b,c"]
"".split(",") == [""]
"#,
        );
        assert::fail("'a b'.split()", "split() missing 1 required positional argument: sep");
        assert::fail("'a b'.rsplit()", "rsplit() missing 1 required positional argument: sep");
        assert::fail(
            "'a b'.split(None)",
            "in call to split(), parameter 'sep' got value of type 'NoneType', want 'string'",
        );
        assert::fail(
            "'a b'.split(' ', None)",
            "in call to split(), parameter 'maxsplit' got value of type 'NoneType', want 'int'",
        );
        assert::fail("'a b'.split('')", "Empty separator");
    }

    #[test]
    fn test_latin1_char_model_methods() {
        // Modified: Java char semantics in the repository/extension model.
        let _model = super::enter_latin1_char_model(true);
        assert::all_true(
            r#"
"\240".isspace() and "\205".isspace() and " \t".isspace()
not "\241".isspace()
"a\351b".title() == "A\351b"
"\351x".capitalize() == "\311x"
"\351".upper() == "\351"
len("\303\251".elems()) == 2
"#,
        );
        drop(_model);
        // The default (BUILD/analysis) model keeps byte-wise emulation.
        assert::all_true(
            r#"
not "\u00a0".isspace()
"aéb".title() == "AéB"
"#,
        );
    }

    #[test]
    fn test_bazel_string_literal_values() {
        // Modified: Lexer.java raw strings keep the backslash before a quote;
        // the default model decodes byte escapes as UTF-8 with replacement.
        assert::all_true(
            r#"
r"\"" == "\\\""
len(r'\'') == 2
r"\\" == "\\\\"
r'a\nb' == "a\\nb"
"\303\251" == "é"
"\377" == "\U0010ffff"
len("\377") == 1
"\200" != "\201"
{"\200": 1, "\201": 2}["\201"] == 2
"A\303" == "A\U0010ffc3"
"\101" == "A"
"#,
        );
        let _model = super::enter_latin1_char_model(true);
        assert::all_true(
            r#"
"\377" == "\u00ff"
len("\303\251") == 2
r"\"" == "\\\""
"#,
        );
    }

    #[test]
    fn test_opaque_iterator() {
        // Modified: Bazel's elems() returns a list (StringModule.java:926-940).
        assert::is_true("type('foo'.elems()) == type([])");
        assert::is_true("type('foo'.codepoints()) != type([])");
    }

    #[test]
    fn test_startswith_with_start_end() {
        assert::all_true(
            r#"
"//foo".startswith("//", 0, 2) == True
"//foo".startswith("foo", 2) == True
"hello".startswith("ell", 1, 4) == True
"hello".startswith("ell", 2, 4) == False
"hello".startswith("hel", -5) == True
"hello".startswith(("he", "wo"), 0, 3) == True
"#,
        );
    }

    // Modified: Bazel 9.3 UTF-8 byte-string semantics; expected values follow
    // StringModule.java applied to each UTF-8 byte.
    #[test]
    fn test_bazel_ascii_case_mapping() {
        assert::all_true(
            r#"
"Hello, World!".lower() == "hello, world!"
"aÉ ÉLAN Õ".lower() == "aÉ Élan Õ"
"é õ ß µa ºa élan 中x".upper() == "é õ ß µA ºA éLAN 中X"
"\u00a0\u3000\x1c\x7f".upper() == "\u00a0\u3000\x1c\x7f"
"".capitalize() == ""
"hELLO wORLD".capitalize() == "Hello world"
"élan".capitalize() == "élan"
"aÉ".capitalize() == "AÉ"
"ÉLAN".capitalize() == "Élan"
"ßA".capitalize() == "ßa"
"µA".capitalize() == "µa"
"中X".capitalize() == "中x"
"#,
        );
    }

    #[test]
    fn test_bazel_title_letter_state() {
        assert::all_true(
            r#"
"hElLo, WoRlD!".title() == "Hello, World!"
"a1b c_d".title() == "A1B C_D"
"é".title() == "é"
"õa".title() == "õa"
"µa".title() == "µa"
"ºa".title() == "ºa"
"ªa".title() == "ªa"
"ßa".title() == "ßA"
"Éa".title() == "ÉA"
"élan".title() == "éLan"
"ÉLAN".title() == "ÉLan"
"aÉ".title() == "AÉ"
"中x".title() == "中X"
"x中".title() == "X中"
"\u00a0a".title() == "\u00a0A"
"\u3000a".title() == "\u3000A"
"\x1ca\x7fb".title() == "\x1cA\x7fB"
"#,
        );
    }

    #[test]
    fn test_bazel_ascii_predicates() {
        assert::all_true(
            r#"
"abcXYZ".isalpha() == True
"é".isalpha() == False
"aÉ".isalpha() == False
"".isalpha() == False
"a1".isalnum() == True
"é1".isalnum() == False
"µa".isalnum() == False
"0123456789".isdigit() == True
"²".isdigit() == False
"١".isdigit() == False
"".isdigit() == False
" \t\n\x0b\x0c\r".isspace() == True
"".isspace() == False
"\x1c".isspace() == False
"\x7f".isspace() == False
"\u00a0".isspace() == False
"\u0085".isspace() == False
"\u3000".isspace() == False
"é".islower() == False
"ß".islower() == False
"élan".islower() == True
"aÉ".islower() == True
"Éa".islower() == True
"µa".islower() == True
"ABC".islower() == False
"É".isupper() == False
"ÉA".isupper() == True
"ÉLAN".isupper() == True
"aÉ".isupper() == False
"中X".isupper() == True
"#,
        );
    }

    #[test]
    fn test_bazel_istitle() {
        assert::all_true(
            r#"
"".istitle() == False
"123".istitle() == False
"Hello, World!".istitle() == True
"Catch-22".istitle() == True
"HAL-9000".istitle() == False
"Hello world".istitle() == False
"aB".istitle() == False
"é".istitle() == False
"ÉA".istitle() == True
"Éa".istitle() == False
"Élan".istitle() == False
"aÉ".istitle() == False
"é Hello".istitle() == True
"中X".istitle() == True
"X中y".istitle() == False
"#,
        );
    }

    #[test]
    fn test_bazel_strip_whitespace() {
        assert::all_true(
            r#"
" \t\n\x0b\x0c\r\x1c\x1d\x1e\x1fa\x1f\x1e\x1d\x1c\r\x0c\x0b\n\t ".strip() == "a"
"\x1ca\x1f".lstrip() == "a\x1f"
"\x1ca\x1f".rstrip() == "\x1ca"
"\x1ca\x1f".strip(None) == "a"
"\x1ca\x1f".lstrip(None) == "a\x1f"
"\x1ca\x1f".rstrip(None) == "\x1ca"
" a\x7f ".strip() == "a\x7f"
"\x7fa\x7f".strip() == "\x7fa\x7f"
"\u00a0a\u00a0".strip() == "\u00a0a\u00a0"
"\u0085a".lstrip() == "\u0085a"
"\u3000a\u3000".rstrip() == "\u3000a\u3000"
"\u2028a".strip() == "\u2028a"
"xxaxx".strip("x") == "a"
"éaé".strip("é") == "a"
" \x1ca\x1c ".strip("\x1c") == " \x1ca\x1c "
"#,
        );
    }

    #[test]
    fn test_endswith_with_start_end() {
        assert::all_true(
            r#"
"hello".endswith("ell", 0, 4) == True
"hello".endswith("lo", 3) == True
"hello".endswith("llo", 0, -1) == False
"hello".endswith(("lo", "la"), 3) == True
"#,
        );
    }
}
