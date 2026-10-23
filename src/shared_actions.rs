//! Shared actions (docs/SHAREABLE_ARTIFACTS.md): Bazel's
//! `Actions.canBeShared` (actions/Actions.java:85-115) lets two actions
//! generate the same output when they are the same action — equal mnemonic,
//! equal action key (ActionKeyComputer.getKey, actions/ActionKeyComputer.java
//! :37-57: the action's `computeKey`, e.g. SpawnAction.computeKey
//! analysis/actions/SpawnAction.java:396-417 over the command lines by exec
//! path, the mnemonic, the environment and the execution info, plus the
//! execution platform and the exec properties), and equal mandatory inputs
//! and outputs compared without their owner (`Artifact.equalsWithoutOwner`,
//! actions/Artifact.java:694-696: exec path and root, in order). The owner,
//! the progress message and the configuration checksum are never compared.
//! It applies within one configured target
//! (Actions.assignOwnersAndThrowIfConflictToleratingSharedActions) and
//! between targets (MapBasedActionGraph.registerAction,
//! actions/MapBasedActionGraph.java:46-63); a shared action runs once
//! (SkyframeActionExecutor). The native profile computes one
//! [`ActionKey`] per registered action, at registration
//! (crate::analysis_runtime::Context::register_action), and both checks —
//! within a target (`register_action`) and across targets (crate::engine
//! `register_output`) — compare that same key, so they cannot drift.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths are relative to `src/main/java/com/google/devtools/build/lib/`.
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::action::{Action, ActionExtra, Transform, WriteAction};
use crate::analysis_runtime::ToolRunfilesTree;
use crate::label::Label;
use crate::starlark_types::File;

/// The owner-independent identity of one registered action: a SHA-256 over
/// a framed encoding of every part [`Sharing`] compares (the stand-in for
/// Bazel's action key plus the ownerless mandatory inputs and outputs; 32
/// bytes per action, so the build's producer map can keep one per action),
/// and the part Bazel compares that the native encoding cannot cover, if
/// any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionKey {
    digest: [u8; 32],
    uncompared: Option<&'static str>,
}

/// How two registrations of one output path relate (Actions.canBeShared).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sharing {
    /// Equal in every compared part: the second registration is the first
    /// action (Bazel shares it and runs it once).
    Shared,
    /// A compared part differs: Bazel's ActionConflictException (a compared
    /// part is part of Bazel's comparison too).
    Conflict,
    /// Equal in every part the native profile compares, but either action
    /// has a part Bazel compares that the native encoding does not cover:
    /// neither a share nor a conflict is guessed.
    Unmodelled(&'static str),
}

/// The execution part of an action's key: the execution platform
/// (`None` for a rule without toolchain resolution, whose
/// RuleContext.getExecutionPlatform is null, or an action factory without
/// a resolved exec group table, an aspect's) and the exec properties of the
/// action's exec group (ActionKeyComputer.getKey :45-55). The exec group's
/// name itself is not keyed.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyedExecution<'a> {
    pub platform: Option<&'a Label>,
    pub properties: &'a [(String, String)],
}

/// What a registration contributes beyond its native [`Action`].
pub struct KeyedParts<'a> {
    /// The mandatory inputs (StarlarkAction: inputs, tools and the
    /// executable), as registered.
    pub inputs: &'a [File],
    pub outputs: &'a [File],
    /// The tools' runfiles trees: Bazel's mandatory inputs include each
    /// tool's runfiles tree artifact; natively its directory and mapping
    /// (runfiles path -> exec path), compared without owner like files.
    pub runfiles_trees: &'a [ToolRunfilesTree],
    pub execution: KeyedExecution<'a>,
    /// Contents or command lines completed when the action executes
    /// (deferred tree-artifact expansion): Bazel keys the unexpanded
    /// directories (CommandLines.addToFingerprint), the native action holds
    /// a closure.
    pub deferred: bool,
    /// An actions.transform_{version,info}_file registration
    /// (BuildInfoFileWriteAction.computeKey keys its callback's digest).
    pub build_info: bool,
}

impl ActionKey {
    /// The key of `action` (its id and progress message excluded) with its
    /// registration's `parts`.
    pub(crate) fn of(action: &Action, parts: &KeyedParts<'_>) -> Self {
        let mut key = Fingerprint::default();
        key.action(action);
        key.files(parts.inputs);
        key.files(parts.outputs);
        key.count(parts.runfiles_trees.len());
        for tree in parts.runfiles_trees {
            key.str(&tree.directory);
            key.count(tree.mapping.len());
            for (path, file) in &tree.mapping {
                key.str(path);
                match file {
                    Some(file) => {
                        key.tag(1);
                        key.file(file);
                    }
                    None => key.tag(0),
                }
            }
        }
        match parts.execution.platform {
            Some(platform) => {
                key.tag(1);
                key.str(&platform.to_string());
            }
            None => key.tag(0),
        }
        key.pairs(parts.execution.properties);
        let uncompared = if parts.deferred {
            Some("command lines or contents expanding tree artifacts")
        } else if parts.build_info {
            Some("build-info translation callbacks")
        } else {
            None
        };
        Self {
            digest: key.0.finalize().into(),
            uncompared,
        }
    }

    /// Actions.canBeShared of the actions keyed `self` and `other`.
    pub(crate) fn sharing(&self, other: &ActionKey) -> Sharing {
        if self.digest != other.digest {
            Sharing::Conflict
        } else if let Some(reason) = self.uncompared.or(other.uncompared) {
            Sharing::Unmodelled(reason)
        } else {
            Sharing::Shared
        }
    }
}

/// The first lines of Bazel's ActionConflictException
/// (actions/ActionConflictException.java:126-131, 218-236): the output's
/// root-relative path (Artifact.prettyPrint), then the attempted and the
/// previous action's owner labels (one when they are equal); its further
/// lines name JVM action classes and keys.
pub(crate) fn conflict_message(output: &str, attempted: &str, previous: &str) -> String {
    let labels = if attempted == previous {
        attempted.to_owned()
    } else {
        format!("{attempted}, {previous}")
    };
    format!(
        "file '{}' is generated by these conflicting actions:\nLabel: {labels}",
        root_relative(output)
    )
}

/// The explicit error for two registrations of `output` that are equal in
/// every compared part but have a part the native encoding does not cover
/// ([`Sharing::Unmodelled`]).
pub(crate) fn unmodelled_message(
    output: &str,
    attempted: &str,
    previous: &str,
    reason: &str,
) -> String {
    let owners = if attempted == previous {
        format!("two actions of {attempted}")
    } else {
        format!("actions of {attempted} and {previous}")
    };
    format!(
        "file '{}' is generated by {owners}: sharing identical actions with {reason} is not implemented in the native profile (Actions.canBeShared, docs/SHAREABLE_ARTIFACTS.md)",
        root_relative(output)
    )
}

/// Artifact.prettyPrint of a derived artifact: the path below its root.
fn root_relative(path: &str) -> &str {
    crate::label::split_generated_root(path).map_or(path, |(_, relative)| relative)
}

/// A length-framed SHA-256 encoding: every variable-length part carries its
/// length and every optional or enumerated part a tag, so distinct field
/// sequences never encode alike.
#[derive(Default)]
struct Fingerprint(Sha256);

impl Fingerprint {
    fn count(&mut self, count: usize) {
        self.0.update((count as u64).to_le_bytes());
    }

    fn tag(&mut self, tag: u8) {
        self.0.update([tag]);
    }

    fn flag(&mut self, flag: bool) {
        self.tag(u8::from(flag));
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.count(bytes.len());
        self.0.update(bytes);
    }

    fn str(&mut self, text: &str) {
        self.bytes(text.as_bytes());
    }

    fn path(&mut self, path: &Path) {
        self.bytes(path.as_os_str().as_bytes());
    }

    fn paths<'a>(&mut self, paths: impl ExactSizeIterator<Item = &'a std::path::PathBuf>) {
        self.count(paths.len());
        for path in paths {
            self.path(path);
        }
    }

    fn strings<'a>(&mut self, strings: impl ExactSizeIterator<Item = &'a String>) {
        self.count(strings.len());
        for text in strings {
            self.str(text);
        }
    }

    fn pairs<'a, K, V>(&mut self, pairs: impl IntoIterator<Item = &'a (K, V)>)
    where
        K: AsRef<[u8]> + 'a,
        V: AsRef<[u8]> + 'a,
    {
        let pairs = pairs.into_iter().collect::<Vec<_>>();
        self.count(pairs.len());
        for (key, value) in pairs {
            self.bytes(key.as_ref());
            self.bytes(value.as_ref());
        }
    }

    /// A file compared without its owner: exec path and kind.
    fn file(&mut self, file: &File) {
        self.str(&file.path);
        self.flag(file.is_directory);
    }

    fn files(&mut self, files: &[File]) {
        self.count(files.len());
        for file in files {
            self.file(file);
        }
    }

    /// Every field of the native action but its owner-numbered id, the
    /// owner-specific remote gate and a symlink's progress message
    /// (destructured exhaustively: a new field must choose).
    fn action(&mut self, action: &Action) {
        let Action {
            id: _,
            command,
            inputs,
            outputs,
            write,
            program,
            arguments,
            environment,
            shell_strict,
            extra,
            shell_pad,
            executable_file,
            execution_info,
            embedded_inputs,
            remote_unsupported: _,
        } = action;
        self.str(command);
        self.count(inputs.len());
        for (real, exec) in inputs {
            self.path(real);
            self.path(exec);
        }
        self.paths(outputs.iter());
        match write {
            Some(WriteAction {
                content,
                executable,
            }) => {
                self.tag(1);
                self.bytes(content);
                self.flag(*executable);
            }
            None => self.tag(0),
        }
        match program {
            Some(program) => {
                self.tag(1);
                self.str(program);
            }
            None => self.tag(0),
        }
        self.strings(arguments.iter());
        self.count(environment.len());
        for (name, value) in environment {
            self.str(name);
            self.str(value);
        }
        self.flag(*shell_strict);
        self.extra(extra);
        self.flag(*shell_pad);
        self.flag(*executable_file);
        self.strings(execution_info.iter());
        self.count(embedded_inputs.len());
        for (path, bytes) in embedded_inputs {
            self.path(path);
            self.bytes(bytes);
        }
    }

    fn extra(&mut self, extra: &ActionExtra) {
        let ActionExtra {
            tree_outputs,
            tree_inputs,
            param_files,
            inherited_environment,
            client_environment,
            execution_info,
            mnemonic,
            transform,
            constant_metadata,
        } = extra;
        self.paths(tree_outputs.iter());
        self.paths(tree_inputs.iter());
        self.count(param_files.len());
        for (path, content) in param_files {
            self.path(path);
            self.bytes(content);
        }
        self.strings(inherited_environment.iter());
        self.count(client_environment.len());
        for (name, value) in client_environment {
            self.str(name);
            self.bytes(value.as_bytes());
        }
        self.count(execution_info.len());
        for (key, value) in execution_info {
            self.str(key);
            self.str(value);
        }
        self.str(mnemonic);
        match transform {
            None => self.tag(0),
            Some(transform) => self.transform(transform),
        }
        self.paths(constant_metadata.iter());
    }

    fn transform(&mut self, transform: &Transform) {
        match transform {
            Transform::Symlink {
                target,
                require_executable,
                progress: _,
            } => {
                self.tag(1);
                self.path(target);
                self.flag(*require_executable);
            }
            Transform::Template {
                template,
                substitutions,
                executable,
            } => {
                self.tag(2);
                self.path(template);
                self.pairs(substitutions);
                self.flag(*executable);
            }
            Transform::BuildInfo { volatile } => {
                self.tag(3);
                self.flag(*volatile);
            }
            Transform::TemplateKeyed {
                template,
                substitutions,
                keyed_substitutions,
                executable,
                volatile,
                callback_digest,
            } => {
                self.tag(4);
                self.path(template);
                self.pairs(substitutions);
                self.pairs(keyed_substitutions);
                self.flag(*executable);
                self.flag(*volatile);
                self.str(callback_digest);
            }
            Transform::Unsupported { message } => {
                self.tag(5);
                self.str(message);
            }
            Transform::WorkspaceStatus { volatile } => {
                self.tag(8);
                self.flag(*volatile);
            }
            Transform::RunfilesManifest { entries } => {
                self.tag(6);
                self.runfiles_entries(entries);
            }
            Transform::RunfilesTree {
                entries,
                manifest,
                executable,
                create,
            } => {
                self.tag(7);
                self.runfiles_entries(entries);
                self.path(manifest);
                match executable {
                    Some(executable) => {
                        self.tag(1);
                        self.path(executable);
                    }
                    None => self.tag(0),
                }
                self.flag(*create);
            }
        }
    }

    fn runfiles_entries(&mut self, entries: &[(String, Option<std::path::PathBuf>)]) {
        self.count(entries.len());
        for (path, target) in entries {
            self.str(path);
            match target {
                Some(target) => {
                    self.tag(1);
                    self.path(target);
                }
                None => self.tag(0),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, owner: &str) -> File {
        File {
            path: path.to_owned(),
            short_path: crate::label::split_generated_root(path)
                .map_or(path, |(_, relative)| relative)
                .to_owned(),
            owner: owner.to_owned(),
            is_source: !path.starts_with("bazel-out/"),
            is_directory: false,
        }
    }

    fn spawn(id: &str, argument: &str) -> Action {
        Action {
            id: id.to_owned(),
            program: Some("bazel-out/k8-opt-exec/bin/tool".to_owned()),
            arguments: vec![argument.to_owned()],
            outputs: vec!["bazel-out/k8-fastbuild/bin/p/out".into()],
            extra: ActionExtra {
                mnemonic: "Run".to_owned(),
                ..ActionExtra::default()
            },
            ..Action::default()
        }
    }

    fn key(
        action: &Action,
        owner: &str,
        tree: Option<&str>,
        platform: Option<&Label>,
    ) -> ActionKey {
        let inputs = [file("p/in.txt", "//p:in.txt")];
        let outputs = [file("bazel-out/k8-fastbuild/bin/p/out", owner)];
        let trees = tree
            .map(|target| ToolRunfilesTree {
                directory: "bazel-out/k8-opt-exec/bin/tool.runfiles".to_owned(),
                mapping: [("_main/data".to_owned(), Some(file(target, "//tools:data")))].into(),
            })
            .into_iter()
            .collect::<Vec<_>>();
        ActionKey::of(
            action,
            &KeyedParts {
                inputs: &inputs,
                outputs: &outputs,
                runfiles_trees: &trees,
                execution: KeyedExecution {
                    platform,
                    properties: &[],
                },
                deferred: false,
                build_info: false,
            },
        )
    }

    #[test]
    fn owners_ids_and_progress_messages_are_not_keyed() {
        let first = key(&spawn("//p:a#0", "x"), "//p:a", Some("tools/data"), None);
        let second = key(&spawn("//p:b#3", "x"), "//p:b", Some("tools/data"), None);
        assert_eq!(first.sharing(&second), Sharing::Shared);
        let symlink = |id: &str, progress: &str| Action {
            id: id.to_owned(),
            extra: ActionExtra {
                transform: Some(Transform::Symlink {
                    target: "p/in.txt".into(),
                    require_executable: false,
                    progress: progress.to_owned(),
                }),
                ..ActionExtra::default()
            },
            ..Action::default()
        };
        assert_eq!(
            key(&symlink("//p:a#0", "Linking a"), "//p:a", None, None).sharing(&key(
                &symlink("//p:b#0", "Linking b"),
                "//p:b",
                None,
                None
            )),
            Sharing::Shared
        );
    }

    #[test]
    fn compared_parts_conflict() {
        let host = Label::parse_canonical("@@platforms//host:host").unwrap();
        let other = Label::parse_canonical("//platforms:other").unwrap();
        let base = key(
            &spawn("//p:a#0", "x"),
            "//p:a",
            Some("tools/data"),
            Some(&host),
        );
        for different in [
            key(
                &spawn("//p:b#0", "y"),
                "//p:b",
                Some("tools/data"),
                Some(&host),
            ),
            key(
                &spawn("//p:b#0", "x"),
                "//p:b",
                Some("tools/other"),
                Some(&host),
            ),
            key(&spawn("//p:b#0", "x"), "//p:b", None, Some(&host)),
            key(
                &spawn("//p:b#0", "x"),
                "//p:b",
                Some("tools/data"),
                Some(&other),
            ),
            key(&spawn("//p:b#0", "x"), "//p:b", Some("tools/data"), None),
        ] {
            assert_eq!(base.sharing(&different), Sharing::Conflict);
        }
        let mut tagged = spawn("//p:b#0", "x");
        tagged.execution_info.insert("no-remote".to_owned());
        assert_eq!(
            base.sharing(&key(&tagged, "//p:b", Some("tools/data"), Some(&host))),
            Sharing::Conflict
        );
    }

    #[test]
    fn uncompared_parts_are_unmodelled_only_when_the_rest_is_equal() {
        let action = spawn("//p:a#0", "x");
        let deferred = ActionKey::of(
            &action,
            &KeyedParts {
                inputs: &[],
                outputs: &[],
                runfiles_trees: &[],
                execution: KeyedExecution::default(),
                deferred: true,
                build_info: false,
            },
        );
        let plain = ActionKey::of(
            &action,
            &KeyedParts {
                inputs: &[],
                outputs: &[],
                runfiles_trees: &[],
                execution: KeyedExecution::default(),
                deferred: false,
                build_info: false,
            },
        );
        assert_eq!(
            deferred.sharing(&plain),
            Sharing::Unmodelled("command lines or contents expanding tree artifacts")
        );
        assert_eq!(
            plain.sharing(&key(&action, "//p:a", None, None)),
            Sharing::Conflict
        );
    }

    #[test]
    fn conflict_messages_name_both_owners_once_each() {
        assert_eq!(
            conflict_message("bazel-out/k8-fastbuild/bin/shared/x.txt", "//b:b", "//a:a"),
            "file 'shared/x.txt' is generated by these conflicting actions:\nLabel: //b:b, //a:a"
        );
        assert_eq!(
            conflict_message("bazel-out/k8-fastbuild/bin/p/x.txt", "//p:t", "//p:t"),
            "file 'p/x.txt' is generated by these conflicting actions:\nLabel: //p:t"
        );
        assert_eq!(
            unmodelled_message("bazel-out/k8-fastbuild/bin/p/x", "//p:t", "//p:t", "R"),
            "file 'p/x' is generated by two actions of //p:t: sharing identical actions with R is not implemented in the native profile (Actions.canBeShared, docs/SHAREABLE_ARTIFACTS.md)"
        );
    }
}
