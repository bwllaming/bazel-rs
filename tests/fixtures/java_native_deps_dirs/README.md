# Native library directory fixture

Source pin: Bazel9.3.0 e89755babf3e941bc1e9d8120b1cd48854ab9dfa. Genuine ELF
SHA256 d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8.

`reference.json` is generated from the genuine report/manifest and nine
binding/type/mutation errors. The textual shared/static/interface files in
`workspace` test artifact metadata semantics, not binary linking. The consumer
is an ordinary action using the returned directories and real artifact inputs.
No new JavaBuilder/compiler matrix is run.

The `loader` fixture instead uses a real JNI shared library, real JVM and actual
rules_cc0.2.17 LibraryToLinkInfo/CcInfo/linking-context functions. Inputs are made
with the already-owned pinned JDK and system C compiler, under private scratch
directories. The generated library link is `links/libfixture_native.so`; its
resolved input is `inputs/libactual.so`. Choosing the resolved directory or an
execution path would make System.loadLibrary fail. Both engines print native:42.

Evidence root:
`/home/coder/opencodetmp/java-compilation-actions/native-deps-dirs`.

- `successful.*`, `errors.*`, `invalid-selected.*` (genuine exit37).
- `CppPatternProbe.java`, `cpp-pattern-reference.txt`: the actual pinned
  CppFileTypes class, including version and internal-byte/newline boundaries.
- `loader-genuine.*`, `loader-native.*`, `loader-comparison.json`: successful
  action/runfiles/JNI consumption and byte-identical artifact-directory reports.

Genuine calls are batch/rc-disabled/private HOME/TMP/output/cache and empty
remotes. Existing public rules/toolchain assets were reused. The JNI consumer is
an ordinary custom rule, not a claim of complete java_binary launcher support.
