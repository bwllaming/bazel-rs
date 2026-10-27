# MODULE flag aliases (Bazel 9.3)

`flag_alias(name = "flavor", starlark_flag = "//settings:flavor")` is retained
as typed module metadata and applied to real configuration consumers. It does
**not** generate repositories or BUILD `alias` targets.

## Ownership and precedence

* Labels normalize in the declaring module's namespace, including its own
  `repo_name`, relative labels and root `@//` references. Each selected module's
  full repository mapping resolves its destinations, including transitive-only
  dependencies and extension imports.
* Root then selected dependencies in breadth-first/declaration order contribute
  aliases; a later module replaces an earlier module's same-name alias. Unselected
  versions contribute none; overrides replace the supplying module normally.
* A duplicate name within one MODULE is an error. Calls are keyword-only,
  strings are required, labels are validated, and a later `module()` fails.
* Build/run/test/cquery inject MODULE aliases before the rc/CLI second parse.
  Explicit rc/CLI definitions override them for subsequent arguments. Like the
  actual 9.3 dispatcher, this parse starts with first-pass explicit aliases, so
  an otherwise unknown CLI name can precede its CLI definition. Substitutions
  follow parse order; values retain rc/config/CLI priority order.
* `--name=value` and bare boolean `--name` use the setting's actual type.
  `--noname` is **not** alias negation. A colliding native option's `--no` form
  and abbreviation still refer to that native option. Exact long names can
  shadow native options: aliased `--compilation_mode=opt` leaves native mode
  at `fastbuild` and sets the Starlark destination to `opt`.

Loading-only `query` accepts MODULE declarations but does not expose their
names as CLI options. Analyzing commands must resolve the selected module graph
even when the requested target is local: otherwise dependency aliases would be
silently lost. This intentionally replaces the previous explicit-module-profile
laziness for unused dependencies/overrides; loading-only query stays lazy.

## Consumers and identities

Resolved aliases are part of the native configuration (omitted when empty, so
old default identities do not change), visible in cquery/config, and used by
`config_setting(values=...)` including its real prerequisites and configured
`:flag_alias_settings` attribute. Conflicting alias/explicit flag expectations
fail. Starlark transitions keep their given dictionary keys but read/write the
destination settings; conflicting writes fail. Exec transitions reset aliases,
as 9.3's built-in exec transition does; the settings themselves retain their
existing exec-scope rules.

Resolution/reparsing takes place inside the existing snapshot recording window.
MODULE changes and selected graph inputs invalidate snapshots; alias destinations
and explicit definitions affect configuration identity. No new cache/store or
global action-key version is introduced. Lockfile registry hashes and extension
usage identities retain their existing representation.

## Explicit native boundaries

* Aliasing bootstrap/execution options is not implemented. Used collisions fail
  instead of retaining the first pass's native side effects. Names excluded from
  snapshot keys (for example `jobs`), plus rc/parser controls `config`,
  `enable_platform_specific_config`, and `flag_alias`, fail even when unused,
  preventing a warm snapshot from hiding an unsupported use.
* Split values of aliased native options (`--compilation_mode opt`) fail explicitly;
  use `--compilation_mode=opt`. Starlark flags require equals syntax for values.
* CLI `--flag_alias` on non-analyzing commands is still unsupported, not accepted
  as a no-op. Platform-based flags remain unsupported by the existing platform
  consumer. No new hard-coded Python compatibility aliases are synthesized for
  old rules_python versions that do not declare them.
* Native diagnostics use the existing native exit codes and source formatting.
  Genuine 9.3 crashes with exit 37 on duplicate MODULE aliases and invalid MODULE
  alias names during injection; native reports ordinary errors instead.

## Evidence

`tests/module_flag_alias.rs` exercises root/dependency local-registry fixtures,
typed action outputs, native collisions, rc/config/CLI precedence, transitive
ownership, selected versions/overrides, warm changed destinations, cquery,
`select()` and transitions, declaration errors and fail-closed surfaces.
`tests/fixtures/module_flag_alias/reference.tsv` is a compact independent genuine
9.3 ELF capture; no genuine Bazel runs in these tests or on user repositories.

Pinned source: `ModuleFileGlobals.java:1187-1212,486-520`,
`ModuleBase.getFlagAliases`, `InterimModule.toModule`,
`SkyframeExecutor.getFlagAliases`, `BlazeCommandDispatcher.java:645-776`,
`OptionsParserImpl.swapShorthandAlias`, `CoreOptionConverters.FlagAliasConverter`,
`ConfigSetting.getSettings`, `ConfigRuleClasses.FLAG_ALIAS_REFERENCES`,
`FunctionTransitionUtil` and `builtin_exec_platforms.bzl`.
