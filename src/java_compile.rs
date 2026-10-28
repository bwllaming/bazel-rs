//! Semantic JavaBuilder intent, retained in binary action plans. Attempt state
//! (decoded jdeps, reduced classpath, fallback outcome) is invocation-local.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::action::Action;
use crate::command_line::{CommandLineSpec, Group, ParamFileInfo, ParamFileType, Part};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ClasspathMode {
    Off,
    Bazel,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JavaCompileSpec {
    pub mode: ClasspathMode,
    /// Literal compiler flags, excluding the separate classpath part.
    pub flags: Vec<String>,
    #[serde(with = "crate::snapshot::codec::paths")]
    pub classpath: Vec<PathBuf>,
    #[serde(with = "crate::snapshot::codec::paths")]
    pub direct_jars: Vec<PathBuf>,
    #[serde(with = "crate::snapshot::codec::paths")]
    pub dependency_jdeps: Vec<PathBuf>,
    /// Explicit membership: a classpath jar may also be an additional/tool
    /// input and must then be staged even when reduction does not select it.
    #[serde(with = "crate::snapshot::codec::paths")]
    pub mandatory_inputs: Vec<PathBuf>,
    #[serde(with = "crate::snapshot::codec::optional_path")]
    pub output_jdeps: Option<PathBuf>,
}

impl JavaCompileSpec {
    pub(crate) fn validate(&self, action: &Action) -> Result<(), String> {
        if action.program.is_none()
            || action.write.is_some()
            || action.extra.transform.is_some()
            || !action.extra.tree_outputs.is_empty()
            || !action.extra.symlink_outputs.is_empty()
            || !action.extra.param_files.is_empty()
        {
            return Err("Java compilation requires a direct spawn with regular outputs and policy-owned parameter files".into());
        }
        if self.mode == ClasspathMode::Bazel && self.output_jdeps.is_none() {
            return Err("BAZEL Java compilation requires an output jdeps artifact".into());
        }
        if let Some(output) = &self.output_jdeps
            && !action.outputs.contains(output)
        {
            return Err("Java output jdeps is not a declared action output".into());
        }
        let inputs: HashSet<&Path> = action
            .inputs
            .iter()
            .map(|(_, dest)| dest.as_path())
            .collect();
        for path in self
            .classpath
            .iter()
            .chain(&self.direct_jars)
            .chain(&self.dependency_jdeps)
            .chain(&self.mandatory_inputs)
        {
            crate::action::validate_relative(path)?;
            if path.to_str().is_none() {
                return Err(
                    "Java compiler artifact paths containing non-UTF-8 bytes are not supported"
                        .into(),
                );
            }
        }
        for path in self
            .classpath
            .iter()
            .chain(&self.dependency_jdeps)
            .chain(&self.mandatory_inputs)
        {
            if !inputs.contains(path.as_path()) {
                return Err(format!(
                    "Java compilation policy input '{}' is not declared",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    /// JavaCompileAction.getReducedClasspath: direct jars plus ALL dependency
    /// paths, used as membership to filter the full classpath in its own order.
    pub(crate) fn reduced_classpath(
        &self,
        mut read: impl FnMut(&Path) -> Result<Vec<u8>, String>,
    ) -> Result<Vec<PathBuf>, String> {
        if self.mode == ClasspathMode::Off {
            return Ok(self.classpath.clone());
        }
        let potential: HashSet<&Path> = self.classpath.iter().map(PathBuf::as_path).collect();
        let mut selected: HashSet<&Path> = self.direct_jars.iter().map(PathBuf::as_path).collect();
        // Bound the metadata handled by one compilation; no global memo.
        let mut remaining = 64 * 1024 * 1024usize;
        for dependency in &self.dependency_jdeps {
            let bytes = read(dependency)?;
            remaining = remaining
                .checked_sub(bytes.len())
                .ok_or("Java dependency metadata exceeds 64 MiB per compilation")?;
            let deps = crate::java_deps::parse(&bytes)
                .map_err(|error| format!("{}: {error}", dependency.display()))?;
            for dependency in deps.paths {
                if let Some(path) = potential.get(Path::new(dependency)) {
                    selected.insert(*path);
                }
            }
        }
        Ok(self
            .classpath
            .iter()
            .filter(|path| selected.contains(path.as_path()))
            .cloned()
            .collect())
    }

    pub(crate) fn attempt(
        &self,
        logical: &Action,
        reduced: &[PathBuf],
        fallback: bool,
    ) -> Result<Action, String> {
        let classpath = if fallback { &self.classpath } else { reduced };
        let mut cp = Vec::new();
        if !classpath.is_empty() {
            cp.push("--classpath".to_owned());
            cp.extend(
                classpath
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned()),
            );
        }
        if self.mode == ClasspathMode::Bazel {
            cp.extend([
                "--reduce_classpath_mode".to_owned(),
                if fallback {
                    "BAZEL_FALLBACK"
                } else {
                    "BAZEL_REDUCED"
                }
                .to_owned(),
            ]);
        }
        let part = |args| Part {
            groups: vec![Group::Strings(args)],
            flag_per_line: false,
            param: Some(ParamFileInfo {
                flag_format: "@%s".into(),
                always: true,
                file_type: ParamFileType::Unquoted,
                flags_only: false,
            }),
        };
        let spec = CommandLineSpec {
            executable: crate::action::arguments(logical),
            parts: vec![part(self.flags.clone()), part(cp)],
            primary_output: logical.outputs[0].to_string_lossy().into_owned(),
            max_length: crate::command_line::MIN_PARAM_FILE_SIZE,
        };
        let (arguments, files) = spec.expand(None)?;
        let mut action = logical.clone();
        action.arguments.extend(arguments);
        action.extra.param_files = files;
        let selected: HashSet<&Path> = self
            .mandatory_inputs
            .iter()
            .chain(classpath)
            .chain(
                self.dependency_jdeps
                    .iter()
                    .filter(|_| self.mode == ClasspathMode::Off),
            )
            .map(PathBuf::as_path)
            .collect();
        action
            .inputs
            .retain(|(_, dest)| selected.contains(dest.as_path()));
        Ok(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reduction_preserves_order_and_uses_all_dependency_kinds() {
        let spec = JavaCompileSpec {
            mode: ClasspathMode::Bazel,
            flags: vec![],
            classpath: ["last.jar", "first.jar", "middle.jar"]
                .map(PathBuf::from)
                .to_vec(),
            direct_jars: vec!["first.jar".into()],
            dependency_jdeps: vec!["x.jdeps".into()],
            mandatory_inputs: vec![],
            output_jdeps: Some("out.jdeps".into()),
        };
        let selected = spec
            .reduced_classpath(|_| {
                Ok(b"\x0a\x0c\x0a\x08last.jar\x10\x02\x0a\x0b\x0a\x07unknown\x10\x03".to_vec())
            })
            .unwrap();
        assert_eq!(
            selected,
            [PathBuf::from("last.jar"), PathBuf::from("first.jar")]
        );
        assert!(
            spec.reduced_classpath(|_| Err("missing dependency output".into()))
                .is_err()
        );
        let bytes = postcard::to_stdvec(&spec).unwrap();
        assert_eq!(
            postcard::from_bytes::<JavaCompileSpec>(&bytes).unwrap(),
            spec
        );
    }

    #[test]
    fn binary_action_roundtrip_retains_raw_intent_but_not_cancellation() {
        use std::os::unix::ffi::OsStringExt;
        let raw = PathBuf::from(std::ffi::OsString::from_vec(b"cp/\xff.jar".to_vec()));
        let spec = JavaCompileSpec {
            mode: ClasspathMode::Bazel,
            flags: vec!["--output".into(), "result.jar".into()],
            classpath: vec![raw.clone()],
            direct_jars: vec![raw],
            dependency_jdeps: vec!["input.jdeps".into()],
            mandatory_inputs: vec!["compiler".into()],
            output_jdeps: Some("result.jdeps".into()),
        };
        let mut action = Action::default();
        action.extra.java_compile = Some(Box::new(spec.clone()));
        action.execution_cancel = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            true,
        )));
        let bytes = postcard::to_stdvec(&action).unwrap();
        let decoded: Action = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.extra.java_compile, Some(Box::new(spec)));
        assert!(decoded.execution_cancel.is_none());
    }
}
