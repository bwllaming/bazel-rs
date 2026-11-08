# registry_failure_exit_codes

Which failure wins when module resolution cannot read the registry, captured
with the genuine Bazel 9.3.1 binary (`--batch --ignore_all_rc_files
--output_user_root=<fresh>`, then `ARGS --registry=<REGISTRY> --curses=no
--color=no`, stdin from /dev/null) in `workspace/` (no lockfile; its
default `--lockfile_mode=update` writes none after the failure).

Each line: `ARGS<TAB>EXIT CODE<TAB>first ERROR line`, ARGS as shell words
(single quotes only). `<REGISTRY>` is the registry URL; `<M>/<V>` the
module whose MODULE.bazel failed first (the builtin closure is fetched in
parallel, so the module varies between runs).

- `genuine_501.tsv`: the registry answers every request `501 Not
  Implemented` (a status HttpConnector does not retry).
- `genuine_refused.tsv`: a closed loopback port (`Connection refused`,
  after HttpConnector's retries, about 13 s).

BlazeCommandDispatcher parses the native options first (a bad option value,
an undefined `--config`, an unknown option of a non-analyzing command: exit
2), then computes the main repository mapping, hence module resolution, for
every analyzing command and `query`/`fetch` — an unknown option of an
analyzing command may be a Starlark flag, so it, malformed target patterns,
query syntax and `--output` errors come after it: `Error computing the main
repository mapping: Error accessing registry …`, exit 32
(ExternalDeps.ERROR_ACCESSING_REGISTRY). `mod` computes no main repository
mapping: it validates its subcommand first and reports a resolution failure
as is, exit 37. Pinned by tests/exit_codes.rs
`registry_failures_match_bazel` (bazel-rs prints its command-line errors
with the `bazel-rs: ` prefix; the messages compare without the prefix).
