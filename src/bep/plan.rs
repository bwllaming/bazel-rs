//! What the Build Event Protocol reads from a planned request
//! (crate::request::PlannedRequest::bep, docs/BEP.md): the requested
//! targets' TargetConfigured data, their reported output groups and the
//! configurations they name. Planning always records it — it is a few
//! strings per top-level target — so a snapshot hit streams exactly the
//! events of the full analysis it replays (the BEP options are not part of
//! the snapshot key, crate::snapshot::key::EXCLUDED).

use serde::{Deserialize, Serialize};

/// One configuration a TargetConfigured/TargetComplete event names
/// (BuildConfigurationEvent): its BEP id and Configuration payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BepConfiguration {
    pub id: String,
    pub mnemonic: String,
    pub platform_name: String,
    pub cpu: String,
    /// BuildConfigurationValue's global make variables, in order.
    pub make_variables: Vec<(String, String)>,
    pub is_tool: bool,
}

/// One requested top-level target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Compatibility {
    #[default]
    Compatible,
    Skipped,
    Rejected,
}

/// One requested top-level target.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BepTarget {
    pub compatibility: Compatibility,
    /// The label as Bazel prints it in events (`//pkg:name`, `@@repo//...`).
    pub label: String,
    /// TargetConfigured.target_kind (`genrule rule`, `source file`).
    pub kind: String,
    /// The rule's `tags` (TargetConfigured.tag).
    pub tags: Vec<String>,
    /// TargetComplete.tag: a test rule's Rule.getRuleTags (`tags` plus the
    /// taggable attributes), any other rule's `tags`.
    pub completion_tags: Vec<String>,
    /// A test rule's size (`SMALL`..`ENORMOUS`) and timeout in seconds.
    pub test: Option<(String, u64)>,
    /// Index into [`BepPlan::configurations`].
    pub configuration: usize,
    /// The output directory mnemonic of the configuration the target was
    /// analyzed in, after its rule transition (an output file: its
    /// generating rule's; `None` for a source file): what the convenience
    /// symlinks point into (crate::convenience_symlinks::TopLevelConfigs).
    pub output_mnemonic: Option<String>,
    /// The reported output groups in request order: name and the indices
    /// of their files in the planned request's `requested` artifacts.
    pub groups: Vec<(String, Vec<usize>)>,
}

impl BepPlan {
    /// The top-level configuration and the requested targets'
    /// configurations, for the convenience symlinks.
    pub fn top_level_configs(&self) -> crate::convenience_symlinks::TopLevelConfigs {
        let mut targets: Vec<String> = Vec::new();
        for target in &self.targets {
            if let Some(mnemonic) = &target.output_mnemonic
                && !targets.contains(mnemonic)
            {
                targets.push(mnemonic.clone());
            }
        }
        crate::convenience_symlinks::TopLevelConfigs {
            top: self
                .configurations
                .first()
                .map(|configuration| configuration.mnemonic.clone())
                .unwrap_or_default(),
            targets,
        }
    }
}

/// The BEP part of a planned request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BepPlan {
    pub configurations: Vec<BepConfiguration>,
    pub targets: Vec<BepTarget>,
}
