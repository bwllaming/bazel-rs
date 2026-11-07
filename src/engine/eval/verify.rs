//! `BAZEL_RS_EVAL_VERIFY=1` (docs/EVALUATOR.md "Test strategy"): the
//! dual-engine oracle. The build plans twice — the walk, then the
//! evaluator, two Engines over one `Arc<RepositorySet>` (repository work
//! done once) — and compares, in order: the plan (keys and order), the
//! configured nodes, the toolchain/exec-group contexts, the rule
//! delegations, the action dump (BAZEL_RS_DUMP_ACTIONS form), stderr
//! (the linearized console), the incremental index, and the cquery graph;
//! per node, BAZEL_RS_VERIFY_DELTA-style delta comparison
//! ([`super::super::delta::compare_deltas`]). The first difference is an
//! internal error naming the key and the field.
//!
//! S0: the entry point only; worker C implements it in S1 (pass 1 +
//! the walk driven by the tape) and extends it with each stage.

/// Whether `BAZEL_RS_EVAL_VERIFY=1` asks for the comparison.
pub(crate) fn enabled() -> bool {
    std::env::var_os("BAZEL_RS_EVAL_VERIFY").is_some_and(|value| value == "1")
}

/// The comparison's entry point (S0: not implemented, an error).
pub(crate) fn run() -> Result<(), String> {
    Err(
        "BAZEL_RS_EVAL_VERIFY=1: the evaluator's dual-engine verification is not implemented yet (stage S0, docs/EVALUATOR.md); unset it"
            .to_owned(),
    )
}

/// What the comparison compares, in order (each a stage of the oracle;
/// the first difference stops it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Compared {
    Plan,
    ConfiguredNodes,
    Contexts,
    Delegations,
    Actions,
    Stderr,
    IncrementalIndex,
    CqueryGraph,
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_stub_is_an_explicit_error() {
        assert!(super::run().unwrap_err().contains("not implemented yet"));
    }
}
