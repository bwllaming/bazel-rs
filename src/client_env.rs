//! The client environment of the command (Bazel's `clientEnv`): the
//! process environment with `--client_env` applied.
//!
//! Bazel's client passes its own environment to the server as
//! `--client_env=NAME=VALUE` command-line options, after the rc options
//! (`--default_override`) and before the user's arguments
//! (OptionProcessor::GetBlazercAndEnvCommandArgs, src/main/cpp/option_processor.cc:694-728).
//! `--client_env` is a repeatable ClientOptions option
//! (runtime/ClientOptions.java:100-109, AssignmentConverter) whose values the
//! options parser orders by priority: rc-file values first, then the
//! client's, then the command line's. CommandEnvironment folds them into a
//! map where a later value wins (CommandEnvironment.java:333, 598-605), so a
//! `--client_env` in an rc file only sets a name the process environment
//! lacks, and one on the command line overrides it. That map is the client
//! environment of everything downstream: the repository environment
//! (`--repo_env` over it, :342-416), inherited action and test environment
//! values, `--incompatible_strict_action_env=false`'s `PATH`.
//!
//! The overrides are installed once per command, after option parsing and
//! before anything reads the client environment ([`install`]); readers use
//! [`vars_os`] and [`var_os`] instead of `std::env`.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::sync::OnceLock;

/// `--client_env` values of the command, by priority.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    /// Values from rc files, in order: under the process environment.
    pub rc: Vec<(String, String)>,
    /// Values from the command line, in order: over it.
    pub command_line: Vec<(String, String)>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self.rc.is_empty() && self.command_line.is_empty()
    }

    /// One `--client_env` value (Converters.AssignmentConverter,
    /// common/options/Converters.java:483-501): `NAME=VALUE`, the name not
    /// empty.
    pub fn parse(value: &str) -> Result<(String, String), String> {
        match value.split_once('=') {
            Some((name, value)) if !name.is_empty() => Ok((name.to_owned(), value.to_owned())),
            _ => Err(
                "Variable definitions must be in the form of a 'name=value' assignment".to_owned(),
            ),
        }
    }

    /// The client environment over `process`.
    pub fn apply(
        &self,
        process: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Vec<(OsString, OsString)> {
        let mut environment: BTreeMap<OsString, OsString> = self
            .rc
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect();
        environment.extend(process);
        environment.extend(
            self.command_line
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value))),
        );
        environment.into_iter().collect()
    }
}

static OVERRIDES: OnceLock<Overrides> = OnceLock::new();

/// Install the command's `--client_env` values (once per process; a second
/// call keeps the first values).
pub fn install(overrides: Overrides) {
    let _ = OVERRIDES.set(overrides);
}

fn overrides() -> Option<&'static Overrides> {
    OVERRIDES.get().filter(|overrides| !overrides.is_empty())
}

/// The whole client environment.
pub fn vars_os() -> Vec<(OsString, OsString)> {
    // snapshot-unrecorded: the callers' own records apply (each reader was a std::env::vars_os read before; the overrides come from the keyed options).
    let process = std::env::vars_os();
    match overrides() {
        None => process.collect(),
        Some(overrides) => overrides.apply(process),
    }
}

/// The client environment's UTF-8 entries.
pub fn vars() -> Vec<(String, String)> {
    vars_os()
        .into_iter()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// One client environment value.
pub fn var_os(name: impl AsRef<OsStr>) -> Option<OsString> {
    let name = name.as_ref();
    if let Some(overrides) = overrides() {
        let find = |entries: &[(String, String)]| {
            entries
                .iter()
                .rev()
                .find(|(given, _)| OsStr::new(given) == name)
                .map(|(_, value)| OsString::from(value))
        };
        if let Some(value) = find(&overrides.command_line) {
            return Some(value);
        }
        // snapshot-unrecorded: as vars_os.
        if let Some(value) = std::env::var_os(name) {
            return Some(value);
        }
        return find(&overrides.rc);
    }
    // snapshot-unrecorded: as vars_os.
    std::env::var_os(name)
}

/// One client environment value as UTF-8 (`None` when unset or not UTF-8).
pub fn var(name: impl AsRef<OsStr>) -> Option<String> {
    var_os(name)?.into_string().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc_values_fill_in_and_command_line_values_override() {
        let overrides = Overrides {
            rc: vec![
                ("CC".to_owned(), "clang".to_owned()),
                ("HOME".to_owned(), "/rc".to_owned()),
                ("CC".to_owned(), "clang-2".to_owned()),
            ],
            command_line: vec![("HOME".to_owned(), "/cli".to_owned())],
        };
        let process = vec![
            (OsString::from("HOME"), OsString::from("/home")),
            (OsString::from("PATH"), OsString::from("/bin")),
        ];
        assert_eq!(
            overrides.apply(process),
            [
                (OsString::from("CC"), OsString::from("clang-2")),
                (OsString::from("HOME"), OsString::from("/cli")),
                (OsString::from("PATH"), OsString::from("/bin")),
            ]
        );
    }

    #[test]
    fn values_are_assignments() {
        assert_eq!(
            Overrides::parse("CC=clang=x"),
            Ok(("CC".to_owned(), "clang=x".to_owned()))
        );
        assert_eq!(
            Overrides::parse("CC="),
            Ok(("CC".to_owned(), String::new()))
        );
        for bad in ["CC", "=x", ""] {
            assert_eq!(
                Overrides::parse(bad).unwrap_err(),
                "Variable definitions must be in the form of a 'name=value' assignment"
            );
        }
    }
}
