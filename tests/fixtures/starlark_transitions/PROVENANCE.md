# Provenance of `go_shaped/go/private/rules/transition.bzl`

`go_shaped/go/private/rules/transition.bzl` is an unmodified copy of
`go/private/rules/transition.bzl` from rules_go 0.63.0 (module `rules_go`,
repo name `io_bazel_rules_go`), as fetched by genuine Bazel 9.3.0 into its
external cache (`external/rules_go+/go/private/rules/transition.bzl`,
SHA-256 `93b40880fe02a9baec191d53262c8a1f358b1ff978349f671c6635597d43fa9c`).
It is Copyright 2020 The Bazel Authors and licensed under the Apache License,
Version 2.0 (see the header of the file and
http://www.apache.org/licenses/LICENSE-2.0); it is used unchanged as test
data for N18c (tests/starlark_transitions.rs `rules_go_shaped_transitions`).

Every other file under `go_shaped/` is written for this repository: shaped
after rules_go 0.63.0's `go/config/BUILD.bazel`, `go/private/BUILD.bazel`,
`go/private/rules/BUILD.bazel`, `go/private/{mode,platforms,providers}.bzl`
and the go_library/go_binary/go_context_data/nogo rules (simplified
implementations; `srcs` uses `allow_files = True` instead of rules_go's
extension list, which is an explicit native gap). The test runs with the
built-in module profile, so `_allowlist_function_transition` is rules_go's
`@bazel_tools//tools/allowlists/function_transition_allowlist`.
