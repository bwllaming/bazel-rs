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

## Executable/params and full-compilation boundary capture

A second native/genuine pair uses the real Graal native Turbine ELF
`e57dfd0321aec0edbf4718b22298de4107d23ee0ba23443887e3f11d10b95618`,
an explicit bootclasspath jar extracted from the same JDK's `java.base.jmod`,
additional rule/toolchain inputs and 400 distinct javac options to force a real
unquoted params file. All three outputs are again byte-identical:

| Output | SHA-256 |
|---|---|
| params-hjar.jar | `2ac4bdc471c73b15f505f38a2acf9f81a30db1fcf07b1f32fdbae5f73eb5a83a` |
| params-tjar.jar | `9267d8fade2fe866efdbe21e39208a18848e12e38b6c10d4800efcfd975f6a0d` |
| params-hjar.jdeps | `4e761e214736de220ca19406da7f7ebbfd00952eccd0b1e880c3913f8dd33a57` |

Receipts: `logs/{genuine,native}-executable-params.*`,
`logs/genuine-executable-actions.*`, `executable-output-comparison.json`.
ZIP entry bytes and timestamps (2010-01-01) are recorded; no normalization.
Real native reuse checks show:

- warm: binary analysis snapshot hit, 0 executed / 1 action cache hit;
- source field edit: snapshot hit, 1 executed, changed header bytes;
- javac source/target edit: BUILD observation invalidates the snapshot and the
  new flags select a different action result;
- dependency constant edit: snapshot hit, all four affected header actions
  execute (the params target and the three-library chain).

`logs/genuine-full-library.*` and `genuine-full-actions.*` additionally capture
**genuine-only** ordinary java_library builds: default reduced-classpath
interdependent libraries, no-source library and resources-only library. These
use real JavaBuilder, singlejar and the pinned default JVM flags. This is
reference material for the next stage, not native full-compilation proof.
`logs/native-full-boundary.*` shows the tiny native java_library passing header
registration and failing at `java_common_internal.bzl:279` on the still-explicit
`create_compilation_action` error. No application clone cquery was attempted.
