/*
 * Copyright 2019 The Starlark in Rust Authors.
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

// Modified for native Bazel 9.3 spelling suggestions (`bazel_did_you_mean`,
// net/starlark/java/spelling/SpellChecker.java), public through
// `crate::errors`; see vendor provenance.

//! Spelling suggestions.

use strsim::levenshtein;

/// Modified: Bazel's `SpellChecker.editDistance` with its `maxEditDistance`
/// cut-off (net/starlark/java/spelling/SpellChecker.java:33-74), over UTF-16
/// code units like Java strings; `None` stands for Java's `-1`. The strings
/// are compared as given (`bazel_suggest` lower-cases them first).
pub fn bazel_edit_distance(s1: &str, s2: &str, max: usize) -> Option<usize> {
    let s1: Vec<u16> = s1.encode_utf16().collect();
    let s2: Vec<u16> = s2.encode_utf16().collect();
    edit_distance_utf16(&s1, &s2, max)
}

fn edit_distance_utf16(s1: &[u16], s2: &[u16], max: usize) -> Option<usize> {
    if s1 == s2 {
        return Some(0);
    }
    if s1.len().abs_diff(s2.len()) > max {
        return None;
    }
    let mut row: Vec<usize> = (0..=s2.len()).collect();
    for i in 1..=s1.len() {
        row[0] = i;
        let mut best_in_the_row = row[0];
        let mut previous = i - 1;
        for j in 1..=s2.len() {
            let old = row[j];
            row[j] = cmp_min(
                previous + usize::from(s1[i - 1] != s2[j - 1]),
                1 + cmp_min(row[j - 1], row[j]),
            );
            previous = old;
            best_in_the_row = cmp_min(best_in_the_row, row[j]);
        }
        if best_in_the_row > max {
            return None;
        }
    }
    let result = row[s2.len()];
    (result <= max).then_some(result)
}

fn cmp_min(a: usize, b: usize) -> usize {
    if a < b { a } else { b }
}

/// Modified: Bazel's `SpellChecker.suggest`
/// (net/starlark/java/spelling/SpellChecker.java:81-94): the candidate with
/// the smallest case-insensitive edit distance below
/// `min(5, (len + 1) / 2)`; on a tie the first candidate wins.
pub fn bazel_suggest<'a>(input: &str, words: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let lowered: Vec<u16> = input.to_lowercase().encode_utf16().collect();
    let mut best_distance = cmp_min(5, (input.encode_utf16().count() + 1) / 2);
    let mut best = None;
    for candidate in words {
        let other: Vec<u16> = candidate.to_lowercase().encode_utf16().collect();
        if let Some(d) = edit_distance_utf16(&lowered, &other, best_distance) {
            if d < best_distance {
                best_distance = d;
                best = Some(candidate);
            }
        }
    }
    best
}

/// Modified: Bazel's `SpellChecker.didYouMean`
/// (net/starlark/java/spelling/SpellChecker.java:100-107): an empty string,
/// or ` (did you mean 'x'?)`.
pub fn bazel_did_you_mean<'a>(input: &str, words: impl IntoIterator<Item = &'a str>) -> String {
    match bazel_suggest(input, words) {
        None => String::new(),
        Some(suggestion) => format!(" (did you mean '{suggestion}'?)"),
    }
}

/// Find a suggestion for a typo.
pub(crate) fn did_you_mean<'a>(
    value: &str,
    variants: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    if value.is_empty() {
        return None;
    }

    let max_dist = if value.len() <= 2 {
        // we don't want to suggest "cd" for "a"
        1
    } else {
        2
    };

    variants
        .into_iter()
        .map(|v| (v, levenshtein(value, v)))
        .filter(|(_, dist)| *dist <= max_dist)
        .min_by_key(|(_v, sim)| *sim)
        .map(|(v, _)| v)
}

#[cfg(test)]
mod tests {
    use crate::errors::did_you_mean::did_you_mean;

    #[test]
    fn prefixes() {
        assert_eq!(
            Some("cxx_library"),
            did_you_mean("cxx_librar", vec!["cxx_library"])
        );
        assert_eq!(
            Some("cxx_library"),
            did_you_mean("cxx_libra", vec!["cxx_library"])
        );
        assert_eq!(None, did_you_mean("cxx_libr", vec!["cxx_library"]));
    }

    #[test]
    fn typos() {
        assert_eq!(
            Some("cxx_library"),
            did_you_mean("cxx_librarx", vec!["cxx_library"])
        );
        assert_eq!(
            Some("cxx_library"),
            did_you_mean("cxx_libraxx", vec!["cxx_library"])
        );
        assert_eq!(None, did_you_mean("cxx_librxxx", vec!["cxx_library"]));
    }

    // Modified: Bazel SpellChecker parity.
    #[test]
    fn bazel_spell_checker() {
        use crate::errors::did_you_mean::bazel_did_you_mean;
        use crate::errors::did_you_mean::bazel_suggest;
        assert_eq!(Some("append"), bazel_suggest("appen", ["append", "clear"]));
        // Case-insensitive; the first of equally close candidates wins.
        assert_eq!(Some("Foo"), bazel_suggest("fOO", ["Foo", "foo"]));
        assert_eq!(Some("abd"), bazel_suggest("abc", ["abd", "abe"]));
        // The bound is min(5, (len + 1) / 2) and the distance must be below it.
        assert_eq!(None, bazel_suggest("a", ["b"]));
        assert_eq!(None, bazel_suggest("abcd", ["abxy"]));
        assert_eq!(Some("abcx"), bazel_suggest("abcd", ["abcx"]));
        assert_eq!(
            " (did you mean 'srcs'?)",
            bazel_did_you_mean("src", ["srcs"])
        );
        assert_eq!("", bazel_did_you_mean("zzz", ["srcs"]));
    }

    #[test]
    fn best() {
        assert_eq!(Some("abc"), did_you_mean("abx", vec!["abc", "abcd"]));
    }

    #[test]
    fn very_short() {
        assert_eq!(Some("a"), did_you_mean("b", vec!["a"]));
        assert_eq!(Some("ab"), did_you_mean("b", vec!["ab"]));
        assert_eq!(None, did_you_mean("b", vec!["cd"]));

        assert_eq!(None, did_you_mean("bc", vec!["de"]));
    }

    #[test]
    fn earlier_variants_are_more_important() {
        assert_eq!(Some("aaaay"), did_you_mean("aaaax", vec!["aaaay", "aaaaz"]));
        assert_eq!(Some("aaaaz"), did_you_mean("aaaax", vec!["aaaaz", "aaaay"]));
    }
}
