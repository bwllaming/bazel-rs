# Java option attribute expansion (Bazel 9.3.0)

`reference_receipt.json` is a compact genuine capture, not synthetic JavaInfo
or a replacement rules_java toolchain. The fixture calls the allowlisted
`java_common.internal_DO_NOT_USE().expand_java_opts` from a real rule context.
Write actions serialize its result; a dependent provider feeds it to an Args
object and a real shell action. No JVM or javac is needed.

Contract sources in the pinned 9.3 tree:

- `starlarkbuildapi/java/JavaCommonApi.java:692-703`: positional-only `ctx` and
  `attr`; required named-only `tokenize`; named-only `exec_paths=False`.
- `rules/java/JavaStarlarkCommon.java:405-419`: `getExpander()` with data/runfiles
  locations by default, exec locations **without data** when requested.
- `analysis/Expander.java`: a configured STRING_LIST attribute, expansion before
  optional ShellUtils tokenization, immutable output list; attribute errors.
- `analysis/RuleContext.java:926-975`: attribute and resolved-toolchain
  TemplateVariableInfo suppliers, then configuration Make-variable context.

The receipt pins quotes, empty elements, dollar escaping, newlines (not shell
token separators), custom `--define`, configuration roots/mode, attribute and
resolved toolchain variables, source/generated locations, aliases, immutable
results and binding diagnostics. No changes to ordinary Java fragment options,
other rule options, source-byte snapshot inputs or cache formats are needed.
Native integration tests also check warm snapshot reuse and invalidation after
BUILD source, configuration and toolchain-value edits, plus cquery without action
or workspace-status execution.

Genuine runs used the pinned ELF (SHA in receipt), batch mode, private scratch
HOME/TMPDIR/output roots, ignored rc files, a local registry and local module
overrides; `--repository_disable_download` prevented downloads. Platforms and
rules_license were unpacked from the existing genuine fixture archives. Unused
builtin modules used the existing test_flags stubs; those stubs provide no Java
helper fields. Raw evidence is under
`/home/coder/opencodetmp/java-expand-opts/genuine/`.

Explicit safe deviations: genuine crashes (exit 37) for nonexistent/wrong-typed
attributes and a context escaped through a provider. Native reports the former
with the original cause text and rejects the latter as an unsupported ctx
provider payload; live calls also require the active rule scope. Native never
crashes intentionally or uses a frozen/foreign context.
