//! Path mapping: `--experimental_output_paths=strip` (docs/PATH_MAPPING.md).
//!
//! Genuine Bazel 9.3.1 (analysis/actions/PathMappers.java,
//! analysis/actions/StrippingPathMapper.java, actions/PathMapper.java,
//! actions/PathMapperConstants.java): an action whose execution info has
//! `supports-path-mapping` (or whose mnemonic is one of
//! [`SUPPORTED_MNEMONICS`]) and that may run sandboxed or remotely runs
//! with the configuration segment of every output path replaced by `cfg`
//! (`bazel-out/k8-fastbuild/bin/x` → `bazel-out/cfg/bin/x`): its command
//! line (File arguments, `map_each` results reading `File.path`,
//! `File.dirname` and `File.root.path`, the executable, parameter files and
//! their paths), its staged inputs and its outputs, which are moved back to
//! their real paths when the action completes. Unless two of its inputs
//! (or an input and an output) from different configurations map to the
//! same path with different contents (StrippingPathMapper.isPathStrippable),
//! when it runs unmapped.
//!
//! This module holds the pure parts: the mapping functions, the effective
//! mode and the collision check, and the thread-local mode under which a
//! File's Starlark attributes report mapped paths (PathMapper.storeIn: the
//! map_each callbacks of a mapped command line).

use std::borrow::Cow;
use std::cell::Cell;

/// StrippingPathMapper.GUID: SpawnAction.computeKey adds it for an action
/// whose effective mode is STRIP (PathMappers.addToFingerprint).
pub const GUID: &str = "8eb2ad5a-85d4-435b-858f-5c192e91997d";

/// The execution requirement opting an action into path mapping
/// (ExecutionRequirements.SUPPORTS_PATH_MAPPING).
pub const REQUIREMENT: &str = "supports-path-mapping";

/// The output tree's exec-root-relative root (`bazel-out`).
const OUTPUT_ROOT: &str = "bazel-out";

/// StrippingPathMapper.FIXED_CONFIG_SEGMENT.
const FIXED_CONFIG_SEGMENT: &str = "cfg";

/// PathMappers.SUPPORTED_MNEMONICS: mnemonics mapped without the
/// execution requirement.
pub const SUPPORTED_MNEMONICS: &[&str] = &[
    "AndroidLint",
    "CompileAndroidResources",
    "DeJetify",
    "DejetifySrcs",
    "Desugar",
    "DexBuilder",
    "Jetify",
    "JetifySrcs",
    "LinkAndroidResources",
    "MergeAndroidAssets",
    "MergeManifests",
    "ParseAndroidResources",
    "StarlarkAARGenerator",
    "StarlarkMergeCompiledAndroidResources",
    "StarlarkRClassGenerator",
    "Mock action",
];

/// PathMappers.getEffectiveOutputPathsMode under `strip`: whether an action
/// with this mnemonic and these execution requirements (`has`: the merged
/// keys) is mapped. `local`, or `no-sandbox` with `no-remote`, rules it out
/// (path mapping needs a sandbox or remote execution).
pub fn effective(mnemonic: &str, has: impl Fn(&str) -> bool) -> bool {
    if has("local") || (has("no-sandbox") && has("no-remote")) {
        return false;
    }
    SUPPORTED_MNEMONICS.contains(&mnemonic) || has(REQUIREMENT)
}

/// Whether `path` lies below the output root (PathFragment.startsWith of
/// `bazel-out`, segment-wise).
fn is_output_path(path: &str) -> bool {
    path == OUTPUT_ROOT
        || path
            .strip_prefix(OUTPUT_ROOT)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// StrippingPathMapper.map: `bazel-out/<cfg>/rest` → `bazel-out/cfg/rest`
/// (`bazel-out/<cfg>` → `bazel-out/cfg`); any other path unchanged.
pub fn strip(path: &str) -> Cow<'_, str> {
    let Some(rest) = path
        .strip_prefix(OUTPUT_ROOT)
        .and_then(|rest| rest.strip_prefix('/'))
    else {
        return Cow::Borrowed(path);
    };
    match rest.split_once('/') {
        Some((config, tail)) if config == FIXED_CONFIG_SEGMENT => {
            let _ = tail;
            Cow::Borrowed(path)
        }
        Some((_, tail)) => Cow::Owned(format!("{OUTPUT_ROOT}/{FIXED_CONFIG_SEGMENT}/{tail}")),
        None if rest == FIXED_CONFIG_SEGMENT => Cow::Borrowed(path),
        None => Cow::Owned(format!("{OUTPUT_ROOT}/{FIXED_CONFIG_SEGMENT}")),
    }
}

/// [`strip`] of a path.
pub fn strip_path(path: &std::path::Path) -> Cow<'_, std::path::Path> {
    use std::os::unix::ffi::OsStrExt;
    let Ok(text) = std::str::from_utf8(path.as_os_str().as_bytes()) else {
        return Cow::Borrowed(path);
    };
    match strip(text) {
        Cow::Borrowed(_) => Cow::Borrowed(path),
        Cow::Owned(mapped) => Cow::Owned(std::path::PathBuf::from(mapped)),
    }
}

/// [`strip`] of an executable path (CommandLines.SingletonCommandLine with
/// `hasExecutablePath`: a File's exec path, or a string executable that is
/// a normalized path), `None` when the mapping leaves it unchanged.
pub fn map_executable(program: &str) -> Option<String> {
    let normalized = !program.contains("//")
        && !program.ends_with('/')
        && program
            .split('/')
            .all(|segment| segment != "." && segment != "..");
    if !normalized {
        return None;
    }
    match strip(program) {
        Cow::Owned(mapped) => Some(mapped),
        Cow::Borrowed(_) => None,
    }
}

/// PathMapperConstants.FOR_FINGERPRINTING: the mapper of the action key of
/// an action whose effective mode is STRIP. It keeps the configuration
/// segment (so distinct configurations stay distinct) but marks it, so a
/// hard-coded path string differs from a mapped one:
/// `bazel-out/k8-fastbuild/x` → `bazel-out/pm-k8-fastbuild/x`.
pub fn fingerprint(path: &str) -> Cow<'_, str> {
    if !is_output_path(path) && !path.starts_with("blaze-out") {
        return Cow::Borrowed(path);
    }
    match path.find('/') {
        Some(slash) => Cow::Owned(format!("{}pm-{}", &path[..=slash], &path[slash + 1..])),
        None => Cow::Borrowed(path),
    }
}

/// StrippingPathMapper.StringStripper.strip (PathMapper.mapHeuristically):
/// every `bazel-out/<segment>/` in `text`, `<segment>` one or more of
/// `[A-Za-z0-9_.-]`, becomes `bazel-out/cfg/` (Matcher.replaceAll: left to
/// right, without overlaps).
pub fn strip_heuristically(text: &str) -> Cow<'_, str> {
    let prefix = "bazel-out/";
    if !text.contains(prefix) {
        return Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let mut result = String::with_capacity(text.len());
    let mut copied = 0;
    let mut position = 0;
    let mut changed = false;
    while let Some(found) = text[position..].find(prefix) {
        let start = position + found;
        let segment_start = start + prefix.len();
        let mut end = segment_start;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'_' | b'.' | b'-'))
        {
            end += 1;
        }
        if end > segment_start && end < bytes.len() && bytes[end] == b'/' {
            result.push_str(&text[copied..start]);
            result.push_str(prefix);
            result.push_str(FIXED_CONFIG_SEGMENT);
            result.push('/');
            copied = end + 1;
            position = end + 1;
            changed = true;
        } else {
            position = segment_start;
        }
    }
    if !changed {
        return Cow::Borrowed(text);
    }
    result.push_str(&text[copied..]);
    Cow::Owned(result)
}

/// The root-relative part of an output path (`bin/pkg/x` of
/// `bazel-out/k8-fastbuild/bin/pkg/x`; ExecPath.subFragment(2)), the key
/// of StrippingPathMapper.isPathStrippable; `None` for other paths.
pub fn collision_key(path: &str) -> Option<&str> {
    if !is_output_path(path) {
        return None;
    }
    let mut slashes = path.match_indices('/').map(|(index, _)| index);
    let _ = slashes.next();
    Some(match slashes.next() {
        Some(second) => &path[second + 1..],
        None => "",
    })
}

/// StrippingPathMapper.isPathStrippable over paths only (no input
/// metadata: the analysis view, where any two distinct artifacts mapping
/// to the same path collide): whether no two distinct `paths` collide.
pub fn strippable_by_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> bool {
    let mut seen: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for path in paths {
        let Some(key) = collision_key(path) else {
            continue;
        };
        match seen.entry(key) {
            std::collections::hash_map::Entry::Occupied(previous) => {
                if *previous.get() != path {
                    return false;
                }
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(path);
            }
        }
    }
    true
}

/// How a File's Starlark attributes (`path`, `dirname`, `root`) report
/// paths on this thread: the mapper stored in the Starlark semantics of a
/// map_each callback (PathMapper.storeIn, Artifact.getExecPathStringForStarlark).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// PathMapper.NOOP.
    Off,
    /// StrippingPathMapper: derived files report their stripped paths.
    Strip,
    /// PathMapperConstants.FOR_FINGERPRINTING (aquery's action key).
    Fingerprint,
}

thread_local! {
    static MODE: Cell<Mode> = const { Cell::new(Mode::Off) };
}

/// The current thread's [`Mode`].
pub fn mode() -> Mode {
    MODE.with(Cell::get)
}

/// Run `f` with File attributes reporting paths under `mode`.
pub fn with_mode<R>(mode: Mode, f: impl FnOnce() -> R) -> R {
    struct Restore(Mode);
    impl Drop for Restore {
        fn drop(&mut self) {
            MODE.with(|cell| cell.set(self.0));
        }
    }
    let _restore = Restore(MODE.with(|cell| cell.replace(mode)));
    f()
}

/// A File's exec path as Starlark reads it under the current [`Mode`]
/// (StrippingPathMapper.getMappedExecPathString: derived artifacts only;
/// FOR_FINGERPRINTING's map: any output path).
pub fn file_path(path: &str, is_source: bool) -> Cow<'_, str> {
    match mode() {
        Mode::Off => Cow::Borrowed(path),
        Mode::Strip if is_source => Cow::Borrowed(path),
        Mode::Strip => strip(path),
        Mode::Fingerprint => fingerprint(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heuristic_stripping_replaces_every_output_path_prefix() {
        assert_eq!(
            strip_heuristically("-Lbazel-out/k8-fastbuild/bin/a:bazel-out/k8-opt/bin/b"),
            "-Lbazel-out/cfg/bin/a:bazel-out/cfg/bin/b"
        );
        // Not followed by a segment and a slash: unchanged.
        assert_eq!(
            strip_heuristically("bazel-out/k8-fastbuild"),
            "bazel-out/k8-fastbuild"
        );
        assert_eq!(strip_heuristically("bazel-out;foo"), "bazel-out;foo");
        assert_eq!(strip_heuristically("bazel-out//x"), "bazel-out//x");
        assert_eq!(
            strip_heuristically("x bazel-out/a+b/c"),
            "x bazel-out/a+b/c"
        );
    }

    #[test]
    fn strip_replaces_the_configuration_segment() {
        assert_eq!(
            strip("bazel-out/k8-fastbuild-ST-0695294e3b4b/bin/pkg/g.txt"),
            "bazel-out/cfg/bin/pkg/g.txt"
        );
        assert_eq!(strip("bazel-out/k8-opt-exec/bin"), "bazel-out/cfg/bin");
        // A two-segment output path keeps only the root (subFragment(2)).
        assert_eq!(strip("bazel-out/stable-status.txt"), "bazel-out/cfg");
        assert_eq!(strip("pkg/src.txt"), "pkg/src.txt");
        assert_eq!(strip("external/r/x"), "external/r/x");
        assert_eq!(strip("bazel-outx/k8/bin/x"), "bazel-outx/k8/bin/x");
        assert!(matches!(strip("bazel-out/cfg/bin/x"), Cow::Borrowed(_)));
    }

    #[test]
    fn fingerprint_marks_the_configuration_segment() {
        assert_eq!(
            fingerprint("bazel-out/k8-fastbuild/bin/x"),
            "bazel-out/pm-k8-fastbuild/bin/x"
        );
        assert_eq!(fingerprint("pkg/x"), "pkg/x");
        assert_eq!(fingerprint("bazel-out"), "bazel-out");
    }

    #[test]
    fn collisions_are_distinct_paths_with_one_root_relative_path() {
        assert!(strippable_by_paths([
            "bazel-out/a/bin/x",
            "bazel-out/a/bin/x",
            "bazel-out/a/bin/y",
            "pkg/x",
        ]));
        assert!(!strippable_by_paths([
            "bazel-out/a/bin/x",
            "bazel-out/b/bin/x"
        ]));
        assert_eq!(collision_key("bazel-out/a/bin/x"), Some("bin/x"));
        assert_eq!(collision_key("bazel-out/stable-status.txt"), Some(""));
        assert_eq!(collision_key("pkg/x"), None);
    }

    #[test]
    fn effective_mode_needs_the_requirement_and_a_sandbox() {
        let has = |keys: &'static [&'static str]| move |key: &str| keys.contains(&key);
        assert!(effective("X", has(&[REQUIREMENT])));
        assert!(!effective("X", has(&[])));
        assert!(effective("Desugar", has(&[])));
        assert!(!effective("X", has(&[REQUIREMENT, "local"])));
        assert!(!effective(
            "X",
            has(&[REQUIREMENT, "no-sandbox", "no-remote"])
        ));
        assert!(effective("X", has(&[REQUIREMENT, "no-sandbox"])));
    }

    #[test]
    fn file_paths_follow_the_thread_mode() {
        assert_eq!(file_path("bazel-out/a/bin/x", false), "bazel-out/a/bin/x");
        with_mode(Mode::Strip, || {
            assert_eq!(file_path("bazel-out/a/bin/x", false), "bazel-out/cfg/bin/x");
            assert_eq!(file_path("pkg/x", true), "pkg/x");
            with_mode(Mode::Fingerprint, || {
                assert_eq!(
                    file_path("bazel-out/a/bin/x", false),
                    "bazel-out/pm-a/bin/x"
                );
            });
            assert_eq!(mode(), Mode::Strip);
        });
        assert_eq!(mode(), Mode::Off);
    }
}
