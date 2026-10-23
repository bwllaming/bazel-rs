# `apple_common.dotted_version` and `XcodeVersionInfo`

On Linux, rules_go's `CGO_ATTRS._xcode_config` and local_config_cc's
`cc_toolchain_config._xcode_config` both resolve to
`@bazel_tools//tools/cpp:host_xcodes`, which is `@local_config_xcode//:host_xcodes`:
apple_support's `xcode_config` with no versions. Its implementation
(apple_support `xcode/xcode_config.bzl:76-113`) calls `apple_common.dotted_version`
and constructs `apple_common.XcodeVersionConfig(...)`. Both now work natively.
Source: `src/apple_xcode.rs`.

## `apple_common.dotted_version(version)`

`apple_common.dotted_version` is the `apple_common_bazel` lambda
(`builtins_bzl/common/objc/apple_common.bzl:46`) around
`AppleStarlarkCommon.dottedVersion` (`rules/objc/AppleStarlarkCommon.java:50-57`).
That method calls `DottedVersion.fromString`, which is ported from
`rules/apple/DottedVersion.java`:

- **Arguments.** Lambda binding gives `lambda() missing 1 required positional argument: version`
  and `lambda() accepts no more than 1 positional argument but got 2`. `version`
  can also be passed by name. A value that is not a string fails with
  `in call to dotted_version(), parameter 'version' got value of type 'NoneType', want 'string'`.
  The `@Param` is `String` (`AppleCommonApi.java:146-152`).
- **Parsing** (`DottedVersion.java:151-259`). The string is split on every
  `.`, and empty pieces are kept. Parsing stops at the first *descriptive*
  piece (`[a-z]\w*`, ASCII, case-insensitive), so `1.2.beta.!!` is `1.2`.
  Each piece before that must fully match `(\d+)([a-z0-9]*?)?(\d+)?`
  (case-insensitive). Group 1 takes every leading digit. The lazy group 2 stops
  before the longest run of trailing digits, which group 3 takes. For example,
  `5beta2` is (5, `beta`, 2) and `1a2b` is (1, `a2b`, 0). Numbers go through
  `Integer.parseInt`, so leading zeros are fine but values above 2147483647 are
  illegal. An empty string, an all-descriptive string or a bad piece fails with
  ``Dotted version components must all start with the form \d+([a-z0-9]*?)?(\d+)? but got '<version>'``.
- **Ordering** (`:277-289`, `:415-422`). Components compare by first number,
  then by alpha sequence (natural order, with no-alpha last), then by second
  number. Missing components count as `0`, and trailing zero components are
  dropped when parsing. So `3.1 == 3.1.0.0`, `01.2 == 1.2`, `1.2.beta == 1.2`,
  `1.0beta < 1` and `1Beta < 1beta`.
- **Value.** `type` is `DottedVersion`. `str`/`repr` return the input string
  unchanged (`1.0.00.0` stays as written). `dir` is `["compare_to", "to_string"]`.
  `to_string` is a field, not a method. Equality is `compareTo == 0` and the
  hash covers the normalized components, so `{dv("1.2"): x}[dv("1.2.0")]`
  works. A `DottedVersion` is always truthy. Comparing it with another type
  fails with `unsupported comparison: DottedVersion <=> T`
  (`Starlark.java:494-517`). `<`, `>`, `sorted`, `max` and `min` use Java's
  order.
- **`compare_to(other)`** (`DottedVersionApi.java:36-44`). Returns -1, 0 or 1.
  `other` is positional-only, and a value that is not a `DottedVersion` fails
  with `in call to compare_to(), parameter 'other' got value of type 'string', want 'DottedVersion'`.
  `repr` is `<built-in method compare_to of DottedVersion value>`.

## `apple_common.XcodeVersionConfig` (`XcodeVersionInfo`)

The provider is not re-implemented. The pinned
`builtins_bzl/common/xcode/providers.bzl` is kept verbatim in
`resources/builtins_bzl/common/xcode/providers.bzl` (with provenance and
SHA-256). It is evaluated once, under the label
`@@_builtins//:common/xcode/providers.bzl`, with the native `provider()`. Two
changes are needed:

- Its one `load(":common/objc/apple_platform.bzl", "PLATFORM", "PLATFORM_TYPE")`
  line is blanked, so line numbers stay the same, and the two tables are bound
  as globals instead.
- `_builtins.internal.apple_common.dotted_version` is the Java method
  (positional-only).

As a result:

- The provider key is `@@_builtins//:common/xcode/providers.bzl%XcodeVersionInfo`,
  as shown by Bazel. `repr` is `<provider>` and `type` is `Provider`. It is
  valid in `attr.label(providers=[...])`, `target[...]` and `in`.
- The `init` (`providers.bzl:19-134`) is Bazel's own code. Every
  `*_sdk_version` and `*_minimum_os_version` is eagerly turned into a
  DottedVersion, so a bad or `None` string fails at construction. The
  instance is a `struct` with fields `availability`, `execution_info`,
  `minimum_os_for_platform_type`, `sdk_version_for_platform` and
  `xcode_version`. Each field is a function, for example
  `<function _minimum_os_for_platform_type from @@_builtins//:common/xcode/providers.bzl>`.
  `availability()` returns the lowercased value. `xcode_version()` returns
  `None`, a path, or a DottedVersion. Unknown platforms fail with
  `Unhandled platform type: X` and `Unhandled platform: X`.
- A rule may return an instance. Since W1b no returned-provider file walk
  gates analysis (every File resolves through the build's artifact index;
  `providers::provider_files` only serves the `BAZEL_RS_VERIFY_ARTIFACTS`
  verifier, where it accepts DottedVersions and the functions defined by this
  embedded file, recognised by the vendored `def` repr
  `<function NAME from FILE>`; no user `.bzl` is evaluated under the
  `@@_builtins` label).
- **Closure capture guard.** The init closures (`providers.bzl:72-134`)
  capture the raw `availability` and `xcode_version` arguments, which the
  pre-W1b file walk could not see, so a File, depset or struct could have been
  hidden from dependency tracking, for example with
  `availability = struct(lower = lambda: depset([f]))`. Bazel accepts any
  value there. Natively every `XcodeVersionInfo(...)` argument must still be
  None, bool, int, float or string (no dependency tracking needs it since
  W1b; lifting it is left to the walker's removal, W1d). The check runs in
  `providers.rs` `init_call`, before `init`.
  Anything else fails explicitly with
  `XcodeVersionInfo argument 'availability' of type 'File' is not supported in the native profile (...)`.
  This is a deliberate, explicit divergence: genuine Bazel would build the
  provider, and would only fail later if `.lower()` or `.startswith` is called.
  Scalars keep Bazel's own errors, for example the dotted_version type error
  for `None`. Tests:
  - `tests/xcode_version_info.rs` `files_cannot_hide_in_xcode_version_info_closures` (a real File);
  - `src/apple_xcode.rs` `xcode_version_info_is_the_builtins_provider` (struct, depset and list).

The no-versions `host_xcodes` provider gives these values:

- `minimum_os_for_platform_type`: ios and catalyst `8.4`, macos `10.11`, tvos
  `9.0`, visionos `1.0`, watchos `2.0`.
- `sdk_version_for_platform`: the same values, except that catalyst gives
  `10.11`.
- `availability()` is `"unknown"` and `xcode_version()` is `None`.
- `execution_info()` is `{"requires-darwin": "", "supports-xcode-requirements-set": ""}`.

## End-to-end `@local_config_xcode//:host_xcodes`

Analysis needs the builtin-module profile (`docs/BUILTIN_MODULES.md`):

```sh
bazel-rs build --experimental-builtin-modules \
  --registry=file://<tests/fixtures/builtin_modules/registry> --lockfile_mode=off //e2e:host
```

With this profile, native resolves `@bazel_tools` and creates
`@@bazel_tools+xcode_configure_extension+local_config_xcode`. It loads
apple_support and analyses `host_xcodes`. It then stops at the next missing
piece, `ctx.fragments` (N23c):

```
error: Object of type `ctx` has no attribute `fragments`
  --> @@apple_support+//xcode:xcode_config.bzl:37:22
```

This is the receipt's `e2e` EXPECTED_NATIVE_GAP (recorded before N23c).
Genuine Bazel builds `//e2e:host`, and its dump of the real `host_xcodes`
equals the fixture's `host_xcodes` lines. N23c added `ctx.fragments`; with the
genuine apple_support module the native profile now analyzes
`@local_config_xcode//:host_xcodes` (the `XcodeVersionConfig(...)` call at
`xcode_config.bzl:98-113` builds the provider above) as the `_xcode_config` of
rules_cc's `unix_cc_toolchain_config` (tests/cc_toolchain.rs,
docs/CC_TOOLCHAIN.md).

Without the builtin profile (the default explicit-module profile),
`bazel_tools` has no contents (its mapping entry exists in every module, like
Bazel, since N51), so analysis never reaches this point:

- `build @local_config_xcode//:host_xcodes` fails with
  `unknown apparent repository @local_config_xcode from canonical repository ""`.
- `@@bazel_tools//tools/cpp:host_xcodes` and a main-repository attribute
  default `"@bazel_tools//tools/cpp:host_xcodes"` fail with `the built-in
  repository @@bazel_tools is visible to every module but has no contents in
  this module profile; use --experimental-builtin-modules`.

The F3 fixture (`//:dump`) covers the provider surface without `ctx.fragments`.
It copies apple_support's no-versions computation.

## Evidence

- `tests/fixtures/xcode_dotted/workspace`: the F3 fixture. It has `//:dump`,
  `//:consume`, the 28 `//err:*` cases and `//e2e:host`.
- `tests/fixtures/xcode_dotted/expected_*.txt`: the bytes written natively.
  These are compared with genuine Bazel in the receipt.
- `tests/xcode_version_info.rs` (CLI) and the unit tests in `src/apple_xcode.rs`.
- `tests/fixtures/xcode_dotted/reference_receipt.json`: the one paired
  native-versus-genuine run. No test reads it.

## Known differences

Paired capture result (`reference_receipt.json`): 25 EXACT, 4 MISMATCH,
1 EXPECTED_NATIVE_GAP. The `//:dump` and `//:consume` bytes are identical to
genuine Bazel's. Genuine `//e2e:host`, the real `host_xcodes`, writes exactly
the fixture's `host_xcodes` lines. Natively, with the builtin profile, that
target stops at `ctx.fragments` (N23c), as described above. The native e2e
side was re-run with that profile, and the receipt's `history` explains why.

The receipt's four MISMATCH rows were interpreter wording, declared before
the run. Since N33 (Bazel 9.3's interpreter error texts in the vendored
Starlark) all four print genuine Bazel's message exactly, and
`tests/xcode_version_info.rs` asserts each reference text:

| Case | Genuine Bazel 9.3 = native since N33 | Native at capture time |
| --- | --- | --- |
| `1 < dv("1")` | `unsupported comparison: int <=> DottedVersion` | ``Operation `compare` not supported for types `int` and `DottedVersion` `` |
| `dv("1").nope` | `'DottedVersion' value has no field or method 'nope'` | ``Object of type `DottedVersion` has no attribute `nope` `` |
| `XcodeVersionConfig()` | `_xcode_version_info_init() missing 14 required keyword-only arguments: ios_sdk_version, …, include_xcode_execution_info` | ``Missing named-only parameter `ios_sdk_version` for call to `@@_builtins//:common/xcode/providers.bzl._xcode_version_info_init` `` |
| `XcodeVersionConfig("8.4")` | `_xcode_version_info_init() does not accept positional arguments, but got 1` | the same native text as the previous row |

The receipt itself is historical: it records the capture-time native texts
and is not re-labelled (no test reads it). The remaining difference is the
`e2e` EXPECTED_NATIVE_GAP above.

The first capture run showed that unknown keyword arguments lacked
`SpellChecker.didYouMean`. `language_modules::bind_styled` now follows Bazel
for each call style:

- **Builtins** (`BuiltinFunction.java:322-337`) fail at the first unknown
  keyword with a hint: `got unexpected keyword argument 'x' (did you mean 'y'?)`.
  Every argument is added before the positional count
  (`BuiltinFunction.java:420`), so an unknown or duplicate keyword is
  reported before surplus positional arguments.
- **`def`/lambda** (`StarlarkFunction.java:389-403`) collects every unknown
  keyword and pluralizes: `got unexpected keyword arguments: a, b`. The hint
  is added only when exactly one keyword is unknown. Duplicates say
  `got multiple values for parameter 'x'` (`:482-485`) as they are added;
  the positional count comes next, then the unknown keywords.

Tracebacks and frames keep the existing native format.
