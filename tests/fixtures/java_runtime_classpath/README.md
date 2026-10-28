# Runtime archive classpath fixture

`reference.json` is generated from the genuine 9.3 `report.json` and the nine
binding/type failures in an isolated scratch capture. `identity_reference.json`
is an additional genuine case: a source, generated artifact and external file
share the basename `a.jar`; source/generated also share a short path. Excluding
the generated/external Files preserves the source; an alias is the same source.

The `workspace` fixture needs no Java toolchain and can run with builtin modules
disabled. Its textual `.jar` files test File semantics, not Java compilation.
The poison producer fails if the excluded artifact is accidentally made an
input of the manifest consumer. Loading-time calls and the frozen provider
consumer pin the lack of any analysis-context requirement.

The separate `archive` fixture uses unchanged rules_java 9.1.0 JavaInfo and
JavaRuntimeClasspathInfo definitions and its actual java_binary
`create_deploy_archive` implementation. Input class files are made with the
already-owned pinned JDK, then placed in deterministic input jars. A generated
kept jar contributes `cpfixture/Kept.class`; deploy_env and neverlink jars have
failing producers. Both genuine and native builds succeed, omit those producers,
and produce identical archive bytes and classpath reports. The archive's
Main-Class manifest is consumed by the real JDK (`archive:kept`).

Capture root: `/home/coder/opencodetmp/java-compilation-actions/runtime-classpath`.
Commands/receipts: `successful.*`, `errors.*`, `private-error.*`, `identity.*`,
`archive.*`, `archive-native.*`; comparison `archive-comparison.json`.
Archive SHA256: `671f86a00a136b243191f515509db95af4366f3e9bdbcb4ad3e7e97602ed2c12`.
Classpath-report SHA256: `b0182309d6e3a281e38457465a9a86b8268053efff3c3f55956f3698b4754257`.

All genuine runs use batch mode, disabled rc, private HOME/TMP/output/cache and
empty remote endpoints. Existing public toolchain/scratch data was reused; the
previous JavaBuilder/header matrices were not rerun. The complete java_binary
path is a separate capability boundary, not replaced by a fake launcher/provider.
