// Shared by integration tests (`include!("support/verify_env.rs")`) whose
// harness starts the CLI with a cleared environment (`env_clear()`).
//
// The engine's debug verifiers are switched on by `BAZEL_RS_VERIFY_*`
// variables of the bazel-rs process (`BAZEL_RS_VERIFY_ARTIFACTS=1` and
// `BAZEL_RS_VERIFY_VIEWS=1` in src/engine.rs, `BAZEL_RS_VERIFY_REPOS=full`
// in src/repository_store.rs). A cleared environment drops them, so a
// `cargo test` run with them exported would silently not verify these
// tests. Chained right after `env_clear()`, `.envs(verifier_environment())`
// forwards every `BAZEL_RS_VERIFY_*` variable of the test process (none when
// they are unset, so a plain run is unchanged); variables the harness sets
// afterwards win. The planned request's codec round trip
// (`BAZEL_RS_SNAPSHOT_ROUNDTRIP=1`, src/snapshot/codec.rs: every build
// executes its decoded plan) is such a verifier too, and so is the analysis
// snapshot's mode (`BAZEL_RS_SNAPSHOT`, on when unset, L2-W7) with its strict
// switch: `BAZEL_RS_SNAPSHOT=verify BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1` runs
// the full analysis beside every hit and fails the command (exit 70) when
// they disagree — like the harnesses that do not clear the environment.
// `BAZEL_RS_RESULT_LINES` (the native `LABEL<TAB>PATH` result lines the
// harnesses read, set for every test by .cargo/config.toml) is forwarded too.
// Harnesses whose tests pin the exact client environment (tests/host_os.rs,
// tests/repository_environment.rs) do not forward.
fn verifier_environment() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    std::env::vars_os()
        .filter(|(key, _)| {
            key.to_str().is_some_and(|key| {
                key.starts_with("BAZEL_RS_VERIFY_")
                    || matches!(
                        key,
                        "BAZEL_RS_RESULT_LINES"
                            | "BAZEL_RS_SNAPSHOT_ROUNDTRIP"
                            | "BAZEL_RS_SNAPSHOT"
                            | "BAZEL_RS_SNAPSHOT_VERIFY_STRICT"
                    )
            })
        })
        .collect()
}
