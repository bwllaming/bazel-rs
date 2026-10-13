# Commit-pinned Git overrides: bounded native profile

The Rust engine materializes Git overrides itself, using installed `/usr/bin/git`
with the subprocess isolation described below. It never delegates a build or
query to Bazel. Upstream also invokes Git (`tools/build_defs/repo/git_worker.bzl`),
but this implementation does not claim identical subprocess or isolation behavior.
This is not complete `git_repository` compatibility.

The supported source specification is an explicit full 40-hex SHA-1 commit over
HTTPS or a hostless absolute `file://` URL. Remote HTTPS requires explicit
`--lockfile_mode=off`; local file URLs do not require network opt-in. Credentials,
URL userinfo, query strings, fragments, SSH/scp syntax and other transports are
rejected. Scheme validation is not a public-IP/egress sandbox.

Tags, branches, abbreviated hashes, default-HEAD selection, submodules, patch
commands and external/generated patch inputs remain unsupported. They must fail
explicitly, not select a different revision or silently skip transformations.
The existing MODULE parser requires one selector; this remains narrower than
Bazel 9.3's support for omitted selectors.

## Discovery and materialization

Git overrides are nonregistry sources. They use the empty selected version and
ordinary canonical repository naming (`name+`); a MODULE's declared version is
retained rather than compared to the overridden registry version.

Unlike registry MODULE patch projection, Git discovery obtains the full source
tree **before evaluating MODULE**:

1. Validate source options and snapshot local patches.
2. Fetch the requested commit into a private disposable Git repository. Try a
   shallow fetch first, then one fresh non-shallow attempt if necessary.
3. Verify both fetched revision and checked-out HEAD against the full commit ID.
4. Reject unsupported tracked entry types, then validate preserved internal
   file symlinks, select `strip_prefix`, and export regular files and literal
   symlinks into a private publication stage. See `docs/GIT_SYMLINKS.md`.
5. Apply the complete ordered local patches once, using `docs/PATCHES.md`.
6. Validate the final MODULE before publishing the tree or discovering dependencies.

There is no registry MODULE replacement. A supported creation patch may introduce
a previously absent MODULE. The published tree has no Git database. Prefix
selection copies files; it does not reproduce upstream's internal symlink layout.

## Cache and bounds

Git cache identity includes a versioned implementation policy, remote URL,
normalized commit, prefix and immutable patch identities. Locked same-filesystem
staging precedes publication. Every cache hit checks a manifest of file bytes,
paths, sizes and modes, then revalidates MODULE. Corrupt entries require refetch;
valid warm entries can be used without accessing the origin.
Relative cache paths supplied to the Rust repository API are workspace-relative
and normalized once for both discovery and later repository access. CLI
`--cache-dir` paths remain relative to the invocation's working directory.

Tree and manifest publication are separate atomic renames, not an atomic pair.
Incomplete publication is detected and repaired. Cache ancestors and staging
must be trusted against concurrent hostile mutation; this is not a filesystem
sandbox.

Exported trees are limited to 100000 entries, 1 GiB of file contents and depth 64.
MODULE is limited to 16 MiB, and Git's tracked-file listing must be smaller than
1 MiB. These are export/validation bounds, not a network-pack or checkout-disk
quota. Directory/external/dangling/cyclic symlinks, gitlinks, special files,
hardlinks and unsafe/non-UTF-8 paths are rejected, including tracked unsupported
entries outside the selected prefix. Internal relative file links have the
separate validation and cache-identity contract in `docs/GIT_SYMLINKS.md`.
Published files use mode 0644 or 0755, and directories use 0755.

## Subprocess isolation

The supervisor is currently Linux-only. Git runs with an empty inherited
environment, private HOME/config/temp directories, closed stdin, disabled global
and system configuration, empty hook/template directories, no credential helper,
no interactive prompting, verified TLS and disabled HTTP redirects. Ambient
Git configuration, proxies, askpass, SSH agents and loader overrides are not
forwarded. System Git, its HTTPS helpers, system CA store and `/usr/bin/kill`
remain trusted components.

Git commands share a 120-second deadline. Captured stdout/stderr are limited to
1 MiB each. Each command owns a process group that is killed and reaped on
completion/error; descendants holding pipes are also handled. Cleanup failures
cannot produce success. A child that escapes its group, external termination of
the Rust process, or uninterruptible kernel activity is outside these guarantees.
This is not a sandbox against Git vulnerabilities or arbitrary hostile executables.

## Compatibility status

The prior regular-file Git increment passed 452 Rust tests, 216 Python tests,
formatting, strict Clippy
and release compilation. `artifacts/git-validation.json` passes four genuine
Bazel graph/source/build differentials and 15 native-only boundary/cache/isolation
checks, retaining command evidence, exact bytes, executable hashes and cleanup.
The separate uppercase-pin and relative-library-cache regressions have real CLI
or public-API evidence in `artifacts/git-uppercase-qa.json` and
`artifacts/git-relative-cache-qa.json`.

That increment's application fetch reached the pinned `rules_distroless` tree
but rejected its internal symlink requirement. The preserved-link increment
records its own subsequent validation; the earlier failure evidence remains in
`artifacts/mainrepo-git-loading.json`.

Git fetching alone does not implement repository rules, extensions, built-in
module closure, configured analysis or application builds/tests. Full Bazel 9.3
parity and application performance improvements remain unproven.
