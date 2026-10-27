# Independent flag-alias fixture

`tests/module_flag_alias.rs` constructs a fresh root and local `file://` registry
from these files. `dep.MODULE.bazel` is the registry's dep@1 MODULE; source.json
is `{"type":"local_path","path":"dep-1"}`, with registry
`{"module_base_path":"sources"}`. The dep source uses `defs.bzl` with its first
load changed to `:info.bzl`, the shared `info.bzl`, and public string settings
`text` (default `dep`) and `other` (default `second`). The root owns a public
`//tools/allowlists/function_transition_allowlist` package_group. Native tests
choose the fixture's `//:platform` and explicit-module profile.

Genuine capture used this independent scratch workspace and registry plus
`tests/fixtures/builtin_modules/registry` for Bazel's injected module closure:

```
bazel-9.3.0 --batch --ignore_all_rc_files --output_user_root=PRIVATE COMMAND \
  --registry=file://PRIVATE/registry \
  --registry=file://REPO/tests/fixtures/builtin_modules/registry \
  --lockfile_mode=off --color=no --curses=no --noshow_progress OPTIONS TARGET
```

For rc cases `--nosystem_rc --nohome_rc` replaces `--ignore_all_rc_files` and
only the scratch workspace rc is read. Every row of `reference.tsv` is an
actual genuine result; expected file contents are read from the output path
Bazel reports, including transitioned outputs. Genuine's hashes are not native
hash goldens. Native deliberately reports ordinary errors rather than reproducing
the genuine JVM crashes for duplicate or badly named MODULE aliases.

The transitive case's root sees only local `bridge`; bridge depends on registry
dep and loads dep's reader with `flags=['@dep//:text']`. Version 2 changes depflag
to `//:other` and introduces flavor -> `//:text`. The Rust tests retain these
mutations, so registry ownership, selection, overrides and changed destinations
are exercised rather than just declaration acceptance. No user repository is
needed for this capture or these tests.
