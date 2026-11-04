# Java action parity fixture

`genuine/all.txt` is genuine Bazel 9.3.1's `aquery //pkg:all
--extra_toolchains=//:registered` of `ws/` (reference_receipt.json), with
the real BCR rules_java 9.1.0. The main-repository `java_toolchain` uses
fail-if-executed tool files, so analysis needs no JDK or java_tools
download. `tests/java_action_parity.rs` runs the native aquery over the same
BUILD files with the same rules_java 9.1.0 Starlark (the offline scaffold of
`tests/support/java_scaffold.rs`) and requires every action block to be
identical, descriptions, command lines, inputs, environment, execution info
and ActionKeys included; only the configuration checksum is normalized.

`pkg/a` covers deps, exports, resources, javacopts, a propagated tag and an
annotation processor (GenClass); `pkg/api_user` an API-generating processor
(the JavacTurbine header compilation); `pkg/mixed` a generated source jar
and `resource_strip_prefix`; `pkg/imported` java_import's ijar.
