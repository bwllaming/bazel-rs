//! `mod tidy` (ModCommand.runTidy, BazelModTidyFunction).
use super::{Code, Failure};
use crate::module_graph::ResolvedGraph;
use crate::repository::RepositorySet;

pub(crate) fn run(
    _repositories: &RepositorySet,
    _graph: &ResolvedGraph,
    _ignore_dev_dependencies: bool,
    _out: &mut Vec<u8>,
) -> Result<(), Failure> {
    Err(Failure::new(
        "mod tidy is not implemented in the native profile",
        Code::Unknown,
    ))
}
