# Bazel 9.3 string methods and `repr`

Bazel 9.3 enables `--internal_starlark_utf_8_byte_strings` by default: each UTF-8
byte of a Starlark string is one Java `char` in `0..=0xFF`. Its string methods
therefore classify and map bytes, not Unicode characters. The vendored
interpreter (`vendor/starlark-0.14.2`) is patched globally (BUILD, MODULE and
every `.bzl` mode) to reproduce those results on valid UTF-8 strings. The patches
and original hashes are declared in `vendor/starlark-0.14.2.provenance.json`.

Pinned sources: `net/starlark/java/eval/StringModule.java` and `Printer.java`
of Bazel 9.3.0.

## Two native string models (N26)

Bazel keeps every string as Latin-1 Java chars. The native interpreter stores
Unicode text and uses one of two models per evaluation thread
(`starlark::values::string::enter_latin1_char_model`):

* **BUILD/.bzl analysis (default)**: a native string is the UTF-8 decoding of
  Bazel's bytes, and methods emulate Bazel byte by byte over the UTF-8 encoding
  (the table below).
* **Repository and extension evaluation** (definition `.bzl` loads, MODULE.bazel,
  `repository_ctx`/`module_ctx` callbacks): each Bazel char is stored as one
  character U+0000..U+00FF and written back as one byte, so methods apply
  Bazel's char semantics directly: `isspace` uses Guava `CharMatcher.whitespace()`
  (StringModule.java:878; `\205` and `\240` are whitespace), `title` uses
  `Character.isLetter/isUpperCase/isLowerCase` and single-char case mappings
  (java:535-560, so `"a\351b".title() == "A\351b"`), and `capitalize`
  `Character.toUpperCase` of the first char (java:526-532).

In both models, as in Bazel 9.3:

* `split(sep, maxsplit)`/`rsplit(sep, maxsplit)` require a string `sep`
  (java:387-468): `split()` fails with `split() missing 1 required positional
  argument: sep`, `split(None)` with `in call to split(), parameter 'sep' got
  value of type 'NoneType', want 'string'`, `split('')` with `Empty separator`.
  There is no whitespace splitting.
* `elems()` returns a list of one-char strings (java:926-940), so `len()` and
  indexing work (`type` is `list`).

String literal values (vendored `eval.rs`, Lexer.java:280-430): raw strings keep
every backslash with its following character (`r"\""` is backslash + quote);
invalid escapes (`\x`, `\u`, `\U`, `\N`, unknown) and octal escapes above
`\377` are lexer errors in every BUILD, `.bzl`, MODULE and REPO.bazel file,
reported at the escape's column. In the repository model `\NNN` is the char
U+00NN (written as byte NNN). In the analysis model a literal with a byte escape
`\200`..`\377` denotes raw bytes; Bazel converts them at its boundaries:
process arguments and environments via `StringEncoding.internalToPlatform`
(UTF-8 decoding with U+FFFD replacement under a UTF-8 `sun.jnu.encoding`, so a
genrule `cmd` with `'\377'` passes `ef bf bd`, captured), while
`FileWriteAction` writes the internal bytes unchanged
(`StringUnsafe.getInternalStringBytes`, analysis/actions/FileWriteAction.java:272).
Native strings cannot hold lone bytes, so in the analysis model bytes that form
valid UTF-8 decode to their characters (`'\303\251' == 'é'`) and every other
byte `b` becomes the private-use character U+10FF00 + b
(`starlark::eval::bazel_byte_char`), keeping lone bytes distinct (bazel_lib's
`CHAR_TO_INT` tables load in BUILD code). Native action boundaries map them back
(`src/bazel_bytes.rs`): process arguments, commands and environments decode the
bytes as UTF-8 with replacement like `internalToPlatform` (a genrule `cmd` with
`'\377'` passes `ef bf bd`), and `ctx.actions.write` writes the raw bytes like
`FileWriteAction`. Other sinks (messages, `repr`, template substitutions,
parameter files) show the private-use character, a documented divergence; `len`
of a literal mixing valid UTF-8 and bytes counts characters, not bytes.

| Method | Bazel 9.3 behaviour now implemented |
| --- | --- |
| `lower`, `upper` | ASCII letters only (`Ascii.toLowerCase/toUpperCase`); all other characters unchanged |
| `capitalize` | first character ASCII-uppercased, rest ASCII-lowercased |
| `title` | ASCII letters: lowercase after a letter, uppercase otherwise. Non-ASCII characters are unchanged; one whose last UTF-8 byte is `0xAA`, `0xB5` or `0xBA` (Latin-1 `ª`, `µ`, `º`) counts as a letter for the next character |
| `isalpha`, `isalnum`, `isdigit` | non-empty and only `[a-zA-Z]`, `[a-zA-Z0-9]`, `[0-9]` |
| `isspace` | non-empty and only `\t \n \x0b \x0c \r` and space (no `\x1c`–`\x1f`, NBSP, U+0085 or U+3000) |
| `islower`, `isupper` | at least one ASCII letter and no ASCII upper/lowercase letter; non-ASCII characters are uncased |
| `istitle` | Bazel's right-to-left algorithm over bytes with ASCII casing |
| `strip`, `lstrip`, `rstrip` | no argument or `None`: trim `\t`–`\r`, `\x1c`–`\x1f` and space only (`ASCII_WHITESPACE`). Explicit `chars` is unchanged |
| `repr` (and `%r`, `{!r}`, container repr) | `"`, `\`, `\r`, `\n`, `\t` use named escapes, other bytes below 0x20 use lowercase `\xNN`; DEL and every non-ASCII character are emitted raw |

The title rule was checked exhaustively against a byte-level model of the Java
code: for every scalar value, preceded by nothing, `a`, `A` or a space and followed
by `a`, the implementation matches Bazel wherever Bazel's result is valid UTF-8.

## Known divergences

Rows where Bazel's byte-wise result is **not valid UTF-8** cannot be represented.
bazel-rs leaves the non-ASCII character unchanged instead:

- `title`: a non-ASCII character starting with a Latin-1 uppercase byte
  (U+0080–U+05BF, U+0600–U+07BF) after a letter, e.g. `"aÉ"`, `"Straße"`,
  `"\u00a0a\u00a0"`; and a 3- or 4-byte character after a non-letter, e.g.
  `"中x"`, `"\u3000a"`. Bazel also maps a `0xB5` continuation byte after a
  non-letter byte to U+039C.
- `capitalize`: strings whose first character is U+0800 or above (`"中x"`, `"\u3000"`).
- `strip`/`lstrip`/`rstrip` with explicit non-ASCII `chars` match bytes in Bazel,
  e.g. `"ê".strip("é")`; bazel-rs matches whole characters.

Not changed by this patch (structural differences remain):

- In the analysis model, `len`, indexing, slicing, `find`/`index`/`count`
  offsets and `elems()` use characters, not bytes. `codepoints()` exists natively
  but not in Bazel.
- `strip(chars = ...)` by keyword is rejected (positional only).
- Ordering differs for analysis-model lone bytes: Bazel compares Java chars
  (bytes), so `'\200' < 'é'` is True (0x80 < 0xC3), while natively the
  private-use character U+10FF80 sorts after `é` (False); `sorted`, `min`/`max`
  and comparisons over such strings can therefore differ.
- Byte-escape literals that only form UTF-8 together (`'\303' + '\251'`) stay
  two private-use characters until an action boundary decodes them.

## Tests

- Vendor unit tests cover every changed method on `é õ ß µa ºa aÉ Éa élan 中x`,
  NBSP, U+3000, `\x1c`, DEL and ASCII, plus repr escaping and SIMD paths.
- `tests/string_methods.rs` evaluates `tests/fixtures/string_methods/defs.bzl`
  in BUILD and in a rule implementation through the real CLI and compares the
  action bytes with `expected.txt` (630 rows, 11 tagged `divergent`).
  Expectations come from a byte-level model of the pinned Java sources and were
  confirmed by a genuine Bazel 9.3 capture of the same fixture (BUILD and rule
  implementation outputs): every `bazel` row matches byte for byte, and the only
  11 differences are exactly the declared `divergent` rows, where Bazel emits
  invalid UTF-8. See `tests/fixtures/string_methods/reference_receipt.json`.
- Runtime byte data that is not valid UTF-8 (Latin-1 or binary module reads) can
  still differ for `title`, `capitalize` and `isspace`; e.g. Bazel's `capitalize`
  maps a lone byte E9 to C9. The capture covers BUILD and rule-implementation code.
