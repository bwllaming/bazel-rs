//! Non-local publication receipts. A receipt is a hint, never current intent
//! or permission to bind a repository. Historical graph switching is disabled
//! until acceptance and all public binders share an adoption protocol.

use serde::{Deserialize, Serialize};

/// The preparation path that computed the specification. The version refers
/// to the specification recipe, not the publication manifest format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Origin {
    DirectV2,
    ExtensionV3,
}

/// The selected pointer's scope. Workspace names retain raw path bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Scope {
    Shared,
    Workspace(Vec<u8>),
}

/// Persistable evidence of a successful live selection. Deserialization does
/// not authorize reuse: current preparation, runtime probes, integrity and
/// pointer preference must be proved again before adoption can be considered.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Receipt {
    pub canonical: String,
    pub origin: Origin,
    pub spec: String,
    pub publication: String,
    pub scope: Scope,
    pub manifest_digest: [u8; 32],
}

/// A read-only store probe succeeded. This deliberately has no bind/publish
/// method and is not serializable. It does not prove the specification is
/// current; only RepositorySet's isolated preparation can establish that.
#[derive(Debug)]
pub struct Candidate {
    pub(crate) receipt: Receipt,
}

impl Candidate {
    pub fn receipt(&self) -> &Receipt {
        &self.receipt
    }
}
