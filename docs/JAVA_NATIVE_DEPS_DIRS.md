# Java native-library directories

`java_common.internal_DO_NOT_USE().collect_native_deps_dirs(libraries)` implements
the pinned Bazel9.3 operation (`JavaStarlarkCommon.java:295-332`,
`JavaCommonApi.java:638-644`). It takes one required positional-only depset and
checks the normal private API allowlist. No ctx, toolchain or filesystem access
is required; loading-time calls are supported.

## Selection and paths

The input must contain Starlark structs/provider instances (`StarlarkInfo`), as
used by rules_cc's actual `LibraryToLinkInfo`. Genuine does not check a particular
provider identity. The native implementation consumes existing owned struct /
provider payloads and actual File values, without synthesizing replacements.

For each element in depset traversal order:

1. Select `interface_library` if it is a File; otherwise select `dynamic_library`
   if it is a File. Missing and non-File fields are ignored.
2. Skip selected `.ifso`, `.tbd`, `.lib` and `.dll.a` files. **Do not fall back to
   dynamic_library after selecting an interface file.**
3. Other selected files must match shared-library suffixes `.so`, `.dylib`,
   `.dll`, `.pyd`, `.wasm`, `.tgt`, `.vpi`, or the versioned pattern
   `^.+\.((so)|(dylib))(\.\d\w*)+$` with Java's ASCII digit/word rules.
4. Return the parent of the selected artifact's **runfiles path**, represented
   by native File.short_path. Main root libraries contribute `""`, not `"."`.
   External libraries keep `../<canonical repository>/...`; generated artifacts
   omit their output/configuration prefix.
5. Deduplicate directory strings, retaining the first occurrence.

`resolved_symlink_dynamic_library`, `resolved_symlink_interface_library`, static
and PIC-static fields are not consulted. In particular, a solib link contributes
its link directory, not its resolved target's directory. Static plus dynamic
still selects the dynamic artifact. No basename-only, duck-typed path, file
existence or symlink-resolution heuristic replaces the File semantics.

Suffix matching follows Bazel's host OS (`FileType.hasExtension`), not the target
or execution platform. Captures are Linux. The versioned regex operates on
Bazel's internal path bytes; the native matcher is also checked against the
actual CppFileTypes class extracted from the pinned genuine ELF.

## Result and boundaries

The result is an **immutable list of strings**, including the empty list.
Normal list value equality applies; callers cannot mutate it. Empty input is an
O(1) return of the existing frozen empty list. Nonempty input uses one existing
depset traversal and a local directory set; no global memo, new persistent format
or action/configuration rekey is introduced.

A selected artifact that is neither an interface nor a shared library causes
genuine9.3 to throw unchecked `IllegalArgumentException` (captured exit37).
Native fails analysis with the same `not a shared library :<root-relative path>`
diagnostic instead of crashing. Wrong depset/element types and private calls use
the captured Starlark errors. Tree and unresolved-symlink Files are classified
by artifact filename too, without filesystem inspection.

rules_java9.1.0's caller (`java_binary_impl.bzl:153-167`,
`java_common_internal.bzl:443-452`) supplies `JavaInfo.transitive_native_libraries`
and prefixes these directories with `${JAVA_RUNFILES}/<workspace>/` when creating
`-Djava.library.path`. Runfiles/artifact producer edges remain the caller's normal
responsibility. This helper does not change JavaBuilder processor/backend policy
or claim all Java-binary launcher/deploy features.

## Evidence

`tests/fixtures/java_native_deps_dirs` captures all four orders, shared/duplicate
transitives, equal directory names, external source/generated libraries in two
repositories, root libraries, interface/resolved/static precedence, wrong/missing
fields, the complete suffix family, immutability, binding/type/private errors and
the genuine invalid-library crash boundary. Native tests also verify normal
snapshot reuse and source/flag/dependency invalidation, plus execution-free cquery.

The independent loader fixture constructs actual rules_cc LibraryToLinkInfo,
linker inputs, a linking context and CcInfo. A normal generated symlink points
to a differently named real JNI library. Only that selected link is added to
runfiles; the helper result supplies the JVM search path. Genuine and native both
print `native:42`, with byte-identical directory/artifact reports. No fake JavaInfo,
launcher API or constant-directory substitute is involved.
