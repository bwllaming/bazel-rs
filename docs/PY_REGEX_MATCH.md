# `py_internal.regex_match` (Bazel 9.3)

Pinned source: `rules/python/PyBuiltins.java:91-100` and
`builtins_bzl/common/python/py_internal.bzl:76-77`. The Starlark wrapper forwards
to `boolean regexMatch(String subject, String pattern)`, implemented as
`subject.matches(pattern)`. Both parameters are **positional-only strings**.
The result is a bool and the match must consume the **entire subject**, not a
prefix or a search result, including when the subject ends in a newline.

This is a pure helper, available wherever the guarded `py_internal` callable
can be reached: `.bzl` initialization, an imported callable in BUILD, analysis,
and repository callbacks. It needs no rule/action context. `py_internal` itself
is still not a BUILD global. No new dependencies, subprocesses, persistent
stores, repository fingerprints, snapshot formats or changes to string storage,
escapes or source decoding are introduced. Existing Behavior variants retain
their hash discriminants.

## Deliberately bounded Java subset

`src/py_regex.rs` translates an allowlisted syntax before invoking the existing
`fancy-regex` dependency. It does **not** pass arbitrary Java patterns through
as Rust patterns. Implemented:

* Literals, escaped punctuation, concatenation, alternation, capturing and
  noncapturing groups (captures are unobservable; backreferences are rejected).
* Greedy and reluctant `?`, `*`, `+`, `{n}`, `{n,}`, `{n,m}` quantifiers.
* Simple positive/negative classes and literal ranges; predefined classes
  inside and outside character classes. Nested classes/intersection are rejected.
* Java `\w`, `\d`, `\s` and complements: **ASCII by default**.
  `(?U)` enables Unicode classes, **not Rust's ungreedy mode**. Unicode word is
  Alphabetic + Mark + Decimal_Number + Connector_Punctuation + Join_Control;
  digit is Decimal_Number; whitespace is White_Space. This is not a Python
  importability or XID test: `9pkg`, combining marks and join controls can match.
* Inline/scoped flags `U`, `u`, `s`, `d`, including disabling flags. `u` only
  changes Unicode case folding in Java and is inert without `i` (which is
  rejected). Disabling `u` does not disable Unicode classes enabled by `U`.
* Java dot: excludes LF, CR, NEL, LINE SEPARATOR, PARAGRAPH SEPARATOR by default;
  `d` excludes only LF; `s` includes every scalar. `^`, `\A`, `\z`, and one `$`
  assertion per pattern. `$` also recognizes the position before a final Java
  line terminator (not between CR/LF); `d` changes this to final LF only. The
  outer full-match `\z` still prevents a partial match before a final newline.
* `\t`, `\n`, `\r`, `\f`, `\a`, `\e`, `\xHH`, `\x{...}`, `\uHHHH` for scalar
  values, and `\Q...\E` quotation outside a class (also unterminated quotation).

Explicit unsupported errors, **not approximate answers**, for `i`, `m`, `x`,
other group extensions (lookaround, named groups, atomic groups), backreferences,
possessive quantifiers, property escapes, word boundaries, octal/control/named
escapes, surrogate escapes, `\R`, `\h`, `\v`, `\G`, `\Z`, `\X`, class set
operations, more than one `$`, and class ranges with nonliteral endpoints.
Zero-width assertions are not quantifiable in this subset. These boundaries
can be extended with additional genuine captures; this is not all of
`java.util.regex.Pattern`.

Invalid expressions are native evaluation errors. The captured Java cases
(`[`/`(`/`[z-a]`/`a**`/`\q`) actually crash Bazel with an unchecked
`PatternSyntaxException` (exit 37); native reports its description/index without
crashing. Missing arguments similarly crash genuine with
`IllegalArgumentException` from the `unbound` defaults; native reports an
explicit error. Ordinary type/argument-binding diagnostics match genuine,
including type validation before surplus positional arguments.

## Resource and reuse policy

* Pattern <= 4096 UTF-8 bytes; subject <= 256 KiB; subject length times pattern
  length <= 16 MiB; group nesting <= 32; product of counted repetitions <= 4096.
* Each delegated regular program has a 2 MiB compile-size limit and a 256 KiB
  lazy-DFA cache limit. Only the generated `$` assertion can introduce fancy
  execution; permitting at most one bounds the number of delegated programs
  independently of input pattern length. Backtracking is limited to 100,000.
  Limits return errors, never false matches. This is bounded work, not a full
  wall-clock timeout or an exact RSS quota.
* A process-local **16-entry LRU**, including compile failures, evicts before
  compiling. Compilation and matching use the same mutex: no duplicate
  in-flight compiles and no per-thread clones of compiled programs/scratch
  caches. A hot pattern is not compiled once per filename. This trades
  parallel matching throughput for a simple bounded memory policy.
* Java-character pattern strings are the keys; subjects and Starlark heap/context
  references are never retained. There is no on-disk cache or cache identity
  change for users that do not call the helper.

## Genuine boundary matrix and byte strings

`tests/fixtures/py_regex_match/reference_receipt.json` records the pinned ELF
SHA, commands, stdout/stderr, fixture hashes, 136 boolean matrix rows, 17
error/context probes, build outputs and a cquery closure. Only a tiny scratch
workspace/private HOME/TMP/output/cache was used. The real repository rule
consumes returned booleans to write namespace `__init__.py` files and an accepted
names manifest, rejecting `has-hyphen`, `ansible.dist-info`, punctuation and emoji.

**Bazel parses UTF-8 source bytes as Latin-1 Java characters.** A source literal
`"é"` therefore supplies two Java characters, and does not match `(?U)\w+`.
The source spellings/loader are not changed by this implementation. The helper
adapts the two existing interpreter representations at its Java API boundary:
repository/extension values already hold Latin-1 chars; BUILD/analysis values
hold UTF-8 text plus private-use stand-ins for lone bytes, so `internal_bytes`
provides their original bytes, viewed as Java chars **only for this call**.
ASCII takes a borrowed fast path. Both subject and pattern use this adapter;
neither stored value is mutated. The fixture
matrix uses octal escapes for those exact byte values, so BUILD's existing
native Unicode-source model cannot accidentally turn it into a different test.
Repository-loaded `.bzl` already uses the Latin-1 byte model; the repository
fixture intentionally contains raw UTF-8 directory names and agrees with the
genuine callback (only `ansible`, `_vendor`, `9pkg` are accepted).

For actual Unicode Java strings, a second recorded matrix calls `String.matches`
using the **same ELF's extracted JDK 25.0.2**, not Rust-generated expectations.
The receipt includes the Java source, output, runtime path/SHA and release.
Both that JDK and the locked `regex-syntax` tables use Unicode 16.0. Cases include
ASCII, underscores/digits, Latin, CJK, non-BMP letters/emoji/digits, combining
marks, join controls, connector punctuation, Unicode 15/16 additions, all Java
line terminators, anchors, empty strings, alternation, classes and flags.
Changing the JDK or regex tables requires rechecking this version boundary.

Tests: `py_regex::tests` and `tests/py_regex_match.rs`. The ignored
`representative_filename_batch_benchmark` runs a bounded 100,000 cached filename
matches and a 100-compile comparison; run manually with `--ignored --nocapture`.
