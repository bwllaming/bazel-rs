# Bazel 9.3 string methods and `repr`

Bazel 9.3 enables `--internal_starlark_utf_8_byte_strings` by default: each UTF-8
byte of a Starlark string is one Java `char` in `0..=0xFF`. Its string methods
therefore classify and map bytes, not Unicode characters. The vendored
interpreter (`vendor/starlark-0.14.2`) is patched globally (BUILD, MODULE and
every `.bzl` mode) to reproduce those results on valid UTF-8 strings. The patches
and original hashes are declared in `vendor/starlark-0.14.2.provenance.json`.

Pinned sources: `net/starlark/java/eval/StringModule.java` and `Printer.java`
of Bazel 9.3.0.

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

- `len`, indexing, slicing, `find`/`index`/`count` offsets, `elems()` and
  `codepoints()` use characters, not bytes.
- `split()`/`rsplit()` without a separator still split on Unicode whitespace.
- `strip(chars = ...)` by keyword is rejected (positional only).
- Octal escapes `\200`–`\377` in literals denote U+0080–U+00FF, not raw bytes.

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
