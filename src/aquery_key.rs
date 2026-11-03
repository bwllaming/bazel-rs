//! Bazel 9.3 action keys for `aquery` (docs/AQUERY.md): the
//! `ActionKeyComputer.getKey` digest (actions/ActionKeyComputer.java:37-57)
//! of the action classes the native profile models exactly, written with
//! Bazel's `Fingerprint` encoding (util/Fingerprint.java: a SHA-256 over a
//! protobuf `CodedOutputStream` — varint-length-prefixed UTF-8 strings,
//! varint ints and longs, one-byte booleans).
//!
//! The native action cache keys (crate::action) are unrelated and unchanged;
//! this module only describes actions to `aquery`.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3.1-reference` (Release 9.3.1).
//! Paths are relative to `src/main/java/com/google/devtools/build/lib/`.

use sha2::{Digest, Sha256};

/// SpawnAction.GUID (analysis/actions/SpawnAction.java:101).
pub const SPAWN_GUID: &str = "ebd6fce3-093e-45ee-adb6-bf513b602f0d";
/// FileWriteAction.RegularFileWriteAction.GUID (FileWriteAction.java:250).
pub const FILE_WRITE_GUID: &str = "332877c7-ca9f-4731-b387-54f620408522";
/// TemplateExpansionAction.GUID (TemplateExpansionAction.java:50).
pub const TEMPLATE_GUID: &str = "786c1fe0-dca8-407a-b108-e1ecd6d1bc7f";
/// SymlinkAction.GUID (SymlinkAction.java:61).
pub const SYMLINK_GUID: &str = "7f4fab4d-d0a7-4f0f-8649-1d0337a21fee";
/// UnresolvedSymlinkAction.GUID (analysis/starlark/UnresolvedSymlinkAction.java:48).
pub const UNRESOLVED_SYMLINK_GUID: &str = "0f302651-602c-404b-881c-58913193cfe7";
/// ParameterFileWriteAction.GUID (ParameterFileWriteAction.java:60).
pub const PARAM_FILE_WRITE_GUID: &str = "45f678d8-e395-401e-8446-e795ccc6361f";
/// CommandLines.PARAM_FILE_UUID (actions/CommandLines.java:46-47).
const PARAM_FILE_UUID: &str = "106c1389-88d7-4cc1-8f05-f8a61fd8f7b1";
/// StarlarkCustomCommandLine UUIDs (analysis/starlark/StarlarkCustomCommandLine.java:171-188, 791).
pub const EXPAND_DIRECTORIES_UUID: &str = "9d7520d2-a187-11e8-98d0-529269fb1459";
pub const UNIQUIFY_UUID: &str = "7f494c3e-faea-4498-a521-5d3bc6ee19eb";
pub const OMIT_IF_EMPTY_UUID: &str = "923206f1-6474-4a8f-b30f-4dd3143622e6";
pub const ARG_NAME_UUID: &str = "2bc00382-7199-46ec-ad52-1556577cde1a";
pub const FORMAT_EACH_UUID: &str = "8e974aec-df07-4a51-9418-f4c1172b4045";
pub const BEFORE_EACH_UUID: &str = "f7e101bc-644d-4277-8562-6515ad55a988";
pub const JOIN_WITH_UUID: &str = "c227dbd3-edad-454e-bc8a-c9b5ba1c38a3";
pub const FORMAT_JOINED_UUID: &str = "528af376-4233-4c27-be4d-b0ff24ed68db";
pub const TERMINATE_WITH_UUID: &str = "a4e5e090-0dbd-4d41-899a-77cfbba58655";
pub const SINGLE_FORMATTED_ARG_UUID: &str = "8cb96642-a235-4fe0-b3ed-ebfdae8a0bd9";
/// PlatformConstants.INTERNAL_PLATFORM: the label of
/// PlatformInfo.EMPTY_PLATFORM_INFO, SymlinkAction's execution platform.
pub const INTERNAL_PLATFORM: &str = "@@bazel_tools//tools:internal_platform";

/// util/Fingerprint.java over a SHA-256 digest.
#[derive(Default)]
pub struct Fingerprint(Sha256);

fn varint(hash: &mut Sha256, mut value: u64) {
    let mut buffer = [0u8; 10];
    let mut length = 0;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            buffer[length] = byte;
            length += 1;
            break;
        }
        buffer[length] = byte | 0x80;
        length += 1;
    }
    hash.update(&buffer[..length]);
}

/// The UTF-8 encoding of the Java string holding `text`'s internal bytes as
/// Latin-1 characters (Bazel's internal string representation,
/// crate::bazel_bytes): ASCII is unchanged, any other byte becomes two.
fn java_utf8(text: &str) -> std::borrow::Cow<'_, [u8]> {
    let internal = crate::bazel_bytes::internal_bytes(text);
    if internal.is_ascii() {
        return internal;
    }
    let mut out = Vec::with_capacity(internal.len() * 2);
    for &byte in internal.iter() {
        let mut buffer = [0u8; 4];
        out.extend_from_slice(char::from(byte).encode_utf8(&mut buffer).as_bytes());
    }
    std::borrow::Cow::Owned(out)
}

impl Fingerprint {
    /// `addString`: writeStringNoTag.
    pub fn string(&mut self, text: &str) -> &mut Self {
        let bytes = java_utf8(text);
        varint(&mut self.0, bytes.len() as u64);
        self.0.update(&bytes);
        self
    }

    /// `addBoolean`: writeBoolNoTag.
    pub fn boolean(&mut self, value: bool) -> &mut Self {
        self.0.update([u8::from(value)]);
        self
    }

    /// `addInt`: writeInt32NoTag (a negative value sign-extends to 10 bytes).
    pub fn int(&mut self, value: i32) -> &mut Self {
        varint(&mut self.0, i64::from(value) as u64);
        self
    }

    /// `addLong`: writeInt64NoTag.
    pub fn long(&mut self, value: i64) -> &mut Self {
        varint(&mut self.0, value as u64);
        self
    }

    /// `addNullableString`.
    pub fn nullable_string(&mut self, text: Option<&str>) -> &mut Self {
        match text {
            None => self.boolean(false),
            Some(text) => self.boolean(true).string(text),
        }
    }

    /// `addStrings`: the count, then each string.
    pub fn strings<'a>(&mut self, items: impl ExactSizeIterator<Item = &'a str>) -> &mut Self {
        self.int(items.len() as i32);
        for item in items {
            self.string(item);
        }
        self
    }

    /// `addStringMap`: the count, then each key and value.
    pub fn string_map<'a>(
        &mut self,
        items: impl ExactSizeIterator<Item = (&'a str, &'a str)>,
    ) -> &mut Self {
        self.int(items.len() as i32);
        for (key, value) in items {
            self.string(key).string(value);
        }
        self
    }

    /// `addUUID`: the least then the most significant bits, as longs.
    pub fn uuid(&mut self, uuid: &str) -> &mut Self {
        let hex: String = uuid.chars().filter(|c| *c != '-').collect();
        let most = u64::from_str_radix(&hex[..16], 16).unwrap_or_default();
        let least = u64::from_str_radix(&hex[16..], 16).unwrap_or_default();
        self.long(least as i64).long(most as i64)
    }

    /// `hexDigestAndReset`.
    pub fn hex(self) -> String {
        self.0
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// One element of a StarlarkCustomCommandLine (an `Args` object) as its
/// `addToFingerprint` sees it (StarlarkCustomCommandLine.java:509-647,
/// 830-841, 980-990, 1052-1087).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgsOp {
    /// addSingleObjectToFingerprint: the StringificationType ordinal
    /// (DEFAULT 0, FILE 1) and the command-line form.
    Object { file: bool, text: String },
    /// A Label: its display form only.
    Label(String),
    /// A plain string added to the fingerprint (map_each results).
    Text(String),
    /// A UUID marker.
    Uuid(&'static str),
}

/// The fingerprint description of one `Args` object: its operations, or
/// why Bazel's digest of it is not reproduced (a depset value, whose
/// digest follows NestedSet's internal structure; a map_each over a
/// depset; a native-built Args).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArgsRecipe {
    pub ops: Vec<ArgsOp>,
    pub unmodelled: Option<&'static str>,
}

impl ArgsRecipe {
    pub fn push(&mut self, op: ArgsOp) {
        self.ops.push(op);
    }

    pub fn object(&mut self, file: bool, text: &str) {
        self.ops.push(ArgsOp::Object {
            file,
            text: text.to_owned(),
        });
    }

    pub fn unmodelled(&mut self, reason: &'static str) {
        self.unmodelled.get_or_insert(reason);
    }

    fn add_to(&self, fp: &mut Fingerprint) {
        for op in &self.ops {
            match op {
                ArgsOp::Object { file, text } => {
                    fp.int(i32::from(*file)).string(text);
                }
                ArgsOp::Label(text) | ArgsOp::Text(text) => {
                    fp.string(text);
                }
                ArgsOp::Uuid(uuid) => {
                    fp.uuid(uuid);
                }
            }
        }
    }
}

/// A parameter file's part of the key (CommandLines.addParamFileInfoToFingerprint).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamFileKey {
    pub flag_format: String,
    /// ParameterFileType.toString: `SHELL_QUOTED` or `UNQUOTED`.
    pub file_type: &'static str,
}

/// One CommandLines entry of a spawn: a flat command line (the executable
/// part, a run of literal strings: `addString` of each) or an Args object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandLinePart {
    Flat(Vec<String>),
    Args(ArgsRecipe, Option<ParamFileKey>),
}

/// PlatformInfo.addTo (analysis/platform/PlatformInfo.java:165-179) with
/// ConstraintCollection.addToFingerprint (:283-293).
fn add_platform(fp: &mut Fingerprint, platform: &crate::platforms::PlatformInfo) {
    fp.string(&crate::rule_declarations::canonical_form(&platform.label));
    add_constraints(fp, &platform.constraints);
    fp.nullable_string(Some(&platform.remote_execution_properties));
    fp.string_map(
        platform
            .exec_properties
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    fp.strings(platform.flags.iter().map(String::as_str));
    let required: Vec<String> = platform
        .required_settings
        .iter()
        .map(crate::rule_declarations::canonical_form)
        .collect();
    fp.strings(required.iter().map(String::as_str));
    let allowed: Vec<String> = platform
        .allowed_toolchain_types
        .iter()
        .map(crate::rule_declarations::canonical_form)
        .collect();
    fp.strings(allowed.iter().map(String::as_str));
    fp.boolean(platform.check_toolchain_types);
    fp.nullable_string(platform.missing_toolchain_error.as_deref());
}

fn add_constraints(fp: &mut Fingerprint, constraints: &crate::platforms::ConstraintCollection) {
    fp.boolean(constraints.parent.is_some());
    if let Some(parent) = &constraints.parent {
        add_constraints(fp, parent);
    }
    fp.int(constraints.values.len() as i32);
    for value in &constraints.values {
        // ConstraintValueInfo.addTo: the setting's then the value's canonical form.
        fp.string(&crate::rule_declarations::canonical_form(
            &value.setting.label,
        ));
        fp.string(&crate::rule_declarations::canonical_form(&value.label));
    }
}

/// The execution part every action key ends with (ActionKeyComputer.getKey):
/// the execution platform (absent, a resolved one, or SymlinkAction's
/// EMPTY_PLATFORM_INFO), the exec properties and ACTION_KEY_UNIQUIFIER (0).
#[derive(Clone, Copy, Debug)]
pub enum KeyPlatform<'a> {
    None,
    Resolved(&'a crate::platforms::PlatformInfo),
    Internal,
}

fn finish(
    mut fp: Fingerprint,
    platform: KeyPlatform<'_>,
    properties: &[(String, String)],
) -> String {
    match platform {
        KeyPlatform::None => {
            fp.boolean(false);
        }
        KeyPlatform::Resolved(platform) => {
            fp.boolean(true);
            add_platform(&mut fp, platform);
        }
        KeyPlatform::Internal => {
            // PlatformInfo.builder().setLabel(INTERNAL_PLATFORM).build().
            fp.boolean(true)
                .string(INTERNAL_PLATFORM)
                .boolean(false)
                .int(0)
                .nullable_string(Some(""))
                .int(0)
                .int(0)
                .int(0)
                .int(0)
                .boolean(false)
                .nullable_string(None);
        }
    }
    fp.string_map(properties.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    fp.int(0);
    fp.hex()
}

/// The class-specific `computeKey` input of one action.
#[derive(Clone, Debug)]
pub enum KeyKind<'a> {
    /// SpawnAction.computeKey (:396-417): GUID, the command lines, the
    /// mnemonic, the fixed and inherited environment and the execution info
    /// (path mapping OFF adds nothing).
    Spawn {
        command_lines: &'a [CommandLinePart],
        mnemonic: &'a str,
        environment: &'a [(String, String)],
        inherited: &'a [String],
        execution_info: &'a [(String, String)],
    },
    /// RegularFileWriteAction.computeKey (FileWriteAction.java:276-281).
    FileWrite { executable: bool, contents: &'a str },
    /// TemplateExpansionAction.computeKey (:183-196) over an artifact template.
    Template {
        executable: bool,
        template_exec_path: &'a str,
        substitutions: &'a [(String, String)],
    },
    /// SymlinkAction.computeKey (:429-439) of toArtifact/toExecutable (no inputPath).
    Symlink,
    /// UnresolvedSymlinkAction.computeKey (:96-103).
    UnresolvedSymlink {
        target: &'a str,
        target_type: &'a str,
    },
    /// ParameterFileWriteAction.computeKey (:232-245).
    ParamFileWrite {
        file_type: &'static str,
        args: &'a ArgsRecipe,
    },
}

/// `ActionKeyComputer.getKey` of one action.
pub fn action_key(
    kind: &KeyKind<'_>,
    platform: KeyPlatform<'_>,
    properties: &[(String, String)],
) -> String {
    let mut fp = Fingerprint::default();
    let platform = match kind {
        KeyKind::Spawn {
            command_lines,
            mnemonic,
            environment,
            inherited,
            execution_info,
        } => {
            fp.string(SPAWN_GUID);
            for part in *command_lines {
                match part {
                    CommandLinePart::Flat(arguments) => {
                        for argument in arguments {
                            fp.string(argument);
                        }
                    }
                    CommandLinePart::Args(recipe, param) => {
                        recipe.add_to(&mut fp);
                        if let Some(param) = param {
                            fp.uuid(PARAM_FILE_UUID)
                                .string(&param.flag_format)
                                .string(param.file_type);
                        }
                    }
                }
            }
            fp.string(mnemonic);
            fp.string_map(environment.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            fp.strings(inherited.iter().map(String::as_str));
            fp.string_map(execution_info.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            platform
        }
        KeyKind::FileWrite {
            executable,
            contents,
        } => {
            fp.string(FILE_WRITE_GUID)
                .boolean(*executable)
                .string(contents);
            platform
        }
        KeyKind::Template {
            executable,
            template_exec_path,
            substitutions,
        } => {
            fp.string(TEMPLATE_GUID)
                .string(if *executable { "true" } else { "false" })
                .string(&format!("ARTIFACT: {template_exec_path}"))
                .int(substitutions.len() as i32);
            for (key, value) in *substitutions {
                fp.string(key).string(value);
            }
            platform
        }
        KeyKind::Symlink => {
            fp.string(SYMLINK_GUID);
            // SymlinkAction.getExecutionPlatform/getExecProperties (:483-492).
            return finish(fp, KeyPlatform::Internal, &[]);
        }
        KeyKind::UnresolvedSymlink {
            target,
            target_type,
        } => {
            fp.string(UNRESOLVED_SYMLINK_GUID)
                .string(target)
                .string(target_type);
            platform
        }
        KeyKind::ParamFileWrite { file_type, args } => {
            fp.string(PARAM_FILE_WRITE_GUID).string(file_type);
            args.add_to(&mut fp);
            platform
        }
    };
    finish(fp, platform, properties)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::Label;
    use crate::platforms::{
        ConstraintCollection, ConstraintSettingInfo, ConstraintValueInfo, PlatformInfo,
    };
    use std::sync::Arc;

    /// `@@platforms//host:host` of the genuine 9.3.1 captures
    /// (tests/fixtures/aquery): linux, x86_64, the default missing-toolchain
    /// message.
    fn host() -> PlatformInfo {
        let value = |setting: &str, value: &str| ConstraintValueInfo {
            setting: Arc::new(ConstraintSettingInfo {
                label: Label::parse_canonical(setting).unwrap(),
                default_value: None,
                refined: None,
            }),
            label: Label::parse_canonical(value).unwrap(),
        };
        PlatformInfo {
            label: Label::parse_canonical("@@platforms//host:host").unwrap(),
            constraints: Arc::new(ConstraintCollection {
                parent: None,
                values: vec![
                    value("@@platforms//cpu:cpu", "@@platforms//cpu:x86_64"),
                    value("@@platforms//os:os", "@@platforms//os:linux"),
                ],
            }),
            remote_execution_properties: String::new(),
            exec_properties: Vec::new(),
            flags: Vec::new(),
            required_settings: Vec::new(),
            check_toolchain_types: false,
            allowed_toolchain_types: Vec::new(),
            missing_toolchain_error: Some(
                crate::platforms::DEFAULT_MISSING_TOOLCHAIN_ERROR.to_owned(),
            ),
        }
    }

    fn key(kind: KeyKind<'_>) -> String {
        let host = host();
        action_key(&kind, KeyPlatform::Resolved(&host), &[])
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// Keys of the genuine `aquery //pkg:all` capture of the kitchen fixture.
    #[test]
    fn genuine_keys() {
        let path = pairs(&[("PATH", "/bin:/usr/bin:/usr/local/bin")]);
        let genrule = [CommandLinePart::Flat(strings(&[
            "/bin/bash",
            "-c",
            "source external/bazel_tools/tools/genrule/genrule-setup.sh; echo '#!/bin/sh' > bazel-out/k8-fastbuild/bin/pkg/tool.sh",
        ]))];
        assert_eq!(
            key(KeyKind::Spawn {
                command_lines: &genrule,
                mnemonic: "Genrule",
                environment: &path,
                inherited: &[],
                execution_info: &[],
            }),
            "8502406d6fd846ba741cf24491fa7313913c43f0bbf54195e3f203a172e831d2"
        );
        let shell = [CommandLinePart::Flat(strings(&[
            "/bin/bash",
            "-c",
            "cat $1 > $2",
            "",
            "pkg/in.txt",
            "bazel-out/k8-fastbuild/bin/pkg/k.a",
        ]))];
        assert_eq!(
            key(KeyKind::Spawn {
                command_lines: &shell,
                mnemonic: "Shell",
                environment: &pairs(&[("Z", "1"), ("A", "x y")]),
                inherited: &[],
                execution_info: &pairs(&[("no-sandbox", "1")]),
            }),
            "177a5a2944d7f34159f6b8aeee86b0fe5bfb6c6403dbcf5ecf9b813363767c89"
        );
        let mut args = ArgsRecipe::default();
        args.object(false, "--out");
        args.object(true, "bazel-out/k8-fastbuild/bin/pkg/k.c");
        args.object(true, "pkg/in.txt");
        args.object(true, "pkg/in2.txt");
        args.push(ArgsOp::Uuid(EXPAND_DIRECTORIES_UUID));
        args.push(ArgsOp::Uuid(OMIT_IF_EMPTY_UUID));
        let run = [
            CommandLinePart::Flat(strings(&["bazel-out/k8-opt-exec/bin/pkg/tool.sh"])),
            CommandLinePart::Flat(strings(&["plain"])),
            CommandLinePart::Args(args, None),
        ];
        assert_eq!(
            key(KeyKind::Spawn {
                command_lines: &run,
                mnemonic: "RunTool",
                environment: &path,
                inherited: &[],
                execution_info: &[],
            }),
            "95f8ca076f67f60eb25ab0b3ce7fa25731d3c409c99c7415a5963dc29e22ca73"
        );
        let mut args = ArgsRecipe::default();
        args.object(false, "--flag");
        args.object(true, "pkg/in.txt");
        args.object(true, "pkg/in2.txt");
        args.push(ArgsOp::Uuid(EXPAND_DIRECTORIES_UUID));
        args.push(ArgsOp::Uuid(OMIT_IF_EMPTY_UUID));
        let params = [
            CommandLinePart::Flat(strings(&["bazel-out/k8-opt-exec/bin/pkg/tool.sh"])),
            CommandLinePart::Args(
                args,
                Some(ParamFileKey {
                    flag_format: "@%s".into(),
                    file_type: "SHELL_QUOTED",
                }),
            ),
        ];
        assert_eq!(
            key(KeyKind::Spawn {
                command_lines: &params,
                mnemonic: "Params",
                environment: &[],
                inherited: &[],
                execution_info: &[],
            }),
            "59526ef0cbf2aa6e90f3645a128583d2d8a00ab27120baaa3732bb0a80a8cfd4"
        );
        assert_eq!(
            key(KeyKind::FileWrite {
                executable: true,
                contents: "hello\nworld\n"
            }),
            "c2a9645e666db064ba3d2704c38794183b6652291bac0cce90ae48067df9c3ce"
        );
        assert_eq!(
            key(KeyKind::Template {
                executable: false,
                template_exec_path: "pkg/t.tmpl",
                substitutions: &pairs(&[("{X}", "x"), ("{Y}", "yy")]),
            }),
            "0f605bc8cd8b568d1f3f7dd58d8b6b3f7ae141cbb567c4305f31584e7b087780"
        );
        assert_eq!(
            key(KeyKind::Symlink),
            "c90914cc1e7ea06f84518cf4f179e63dbd20dad9358a550644494828e4a4ed28"
        );
        assert_eq!(
            key(KeyKind::UnresolvedSymlink {
                target: "../some/where",
                target_type: "UNSPECIFIED"
            }),
            "7d2c82a5149116bb2d33096d079881fc6021cce8883c7029142299b871c0d051"
        );
    }
}
