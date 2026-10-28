# Direct Java header compiler fixture

The custom rule calls the real private header API and uses unchanged pinned
rules_java 9.1.0 provider, `java_toolchain` and `java_runtime` implementations.
It deliberately requests header outputs directly, without claiming that
`java_library` or the still-unimplemented JavaBuilder API works.

`header_reference.json` records a genuine Bazel 9.3.0 aquery and actual output
hashes. The native and genuine builds used the same Turbine jar and Zulu JDK25
files (public dependency artifacts already present in the private download
cache), with target `--release 11`. `base -> middle -> top` exercises actual
header use: Top compiles with only Middle's tjar, which carries transitive Base
metadata. Empty-source and source-jar-only header actions also execute.
All nine requested output files were byte-identical, including ZIP metadata and
the `.jdeps` files; no timestamp normalization was necessary.

Private capture root: `/home/coder/opencodetmp/java-compilation-actions`.
Commands/receipts: `logs/genuine-headers.{json,out,err}`,
`logs/genuine-actions.{json,out,err}`, `logs/native-headers.{json,out,err}`,
`output-comparison.json`. The genuine commands use `--batch`, disabled rc files,
private HOME/TMPDIR/output-base/repository-cache and empty remote endpoints.

Fixture-only scaffolding trims unused BUILD/module declarations and the public
`java/defs.bzl` export to the genuine java_import implementation, avoiding
unrelated extensions. Python declarations fail if analyzed. The initial genuine
capture additionally overrides bazel_tools with its extracted distribution plus
a toolchain-types-only tools/jdk BUILD. No compiler/provider implementation is
replaced. The singlejar/proguard attribute is a fail-if-used executable; header
actions do not consume it.

The ordinary Rust suite uses fail-if-executed tool files for fast, network-free
analysis checks. It verifies captured argv, input producer relationships,
execution-free cquery, snapshot invalidation, params and rejection boundaries.
Those tests do not purport to compile Java; the actual builds above provide
that separate proof. Archives are checksum-verified using the shared module
archive fixture support.
