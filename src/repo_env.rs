//! `--repo_env` (runtime/CommonCommandOptions.java:607-622, every command):
//! Converters.EnvVarsConverter values (common/options/Converters.java:627-644)
//! applied over the client environment like CommandEnvironment
//! (runtime/CommandEnvironment.java:342-416, pinned Bazel 9.3 source). The
//! result is the repository environment of repository rules and module
//! extensions (`repository_ctx.os.environ`, `getenv`, `execute`) and the
//! environment of the downloader (crate::download_proxy).
//!
//! `--action_env` never reaches it: `--incompatible_repo_env_ignores_action_env`
//! defaults to true (CommonCommandOptions.java:624-635) and `--action_env` is
//! not a native option. `--experimental_strict_repo_env` (default false) is not
//! modelled, so this is also the strict map. Values are never printed.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;

/// One converted `--repo_env` value.
#[derive(Clone, PartialEq, Eq)]
pub enum EnvVar {
    /// `NAME=VALUE`.
    Set(String, String),
    /// `NAME`: the client environment's value, when it has one.
    Inherit(String),
    /// `=NAME`.
    Unset(String),
}

impl fmt::Debug for EnvVar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Set(name, _) => write!(f, "Set({name:?}, <redacted>)"),
            Self::Inherit(name) => write!(f, "Inherit({name:?})"),
            Self::Unset(name) => write!(f, "Unset({name:?})"),
        }
    }
}

/// EnvVarsConverter.convert (without the `While parsing option` wrapping).
pub fn convert(input: &str) -> Result<EnvVar, String> {
    if input.is_empty() || input == "=" {
        return Err("Variable definitions must be in the form of a 'name=value', 'name', or '=name' assignment".to_owned());
    }
    Ok(match input.find('=') {
        Some(0) => EnvVar::Unset(input[1..].to_owned()),
        None => EnvVar::Inherit(input.to_owned()),
        Some(position) => EnvVar::Set(
            input[..position].to_owned(),
            input[position + 1..].to_owned(),
        ),
    })
}

/// The non-strict repository environment (CommandEnvironment.java:347, :393-416):
/// `client` with each value applied in order; `%bazel_workspace%` in a set
/// value becomes the workspace path.
pub fn repository_environment(
    client: impl IntoIterator<Item = (OsString, OsString)>,
    values: &[EnvVar],
    workspace: Option<&str>,
) -> Vec<(OsString, OsString)> {
    let client: BTreeMap<OsString, OsString> = client.into_iter().collect();
    let mut environment = client.clone();
    for value in values {
        match value {
            EnvVar::Set(name, value) => {
                let value = match workspace {
                    Some(workspace) => value.replace("%bazel_workspace%", workspace),
                    None => value.clone(),
                };
                environment.insert(name.into(), value.into());
            }
            EnvVar::Inherit(name) => {
                if let Some(value) = client.get(OsString::from(name).as_os_str()) {
                    environment.insert(name.into(), value.clone());
                }
            }
            EnvVar::Unset(name) => {
                environment.remove(OsString::from(name).as_os_str());
            }
        }
    }
    environment.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converter_forms() {
        assert_eq!(convert("A=b=c"), Ok(EnvVar::Set("A".into(), "b=c".into())));
        assert_eq!(convert("A="), Ok(EnvVar::Set("A".into(), String::new())));
        assert_eq!(convert("A"), Ok(EnvVar::Inherit("A".into())));
        assert_eq!(convert("=A"), Ok(EnvVar::Unset("A".into())));
        assert_eq!(convert("==A"), Ok(EnvVar::Unset("=A".into())));
        for bad in ["", "="] {
            assert!(
                convert(bad)
                    .unwrap_err()
                    .starts_with("Variable definitions")
            );
        }
        assert_eq!(
            format!("{:?}", convert("K=secret").unwrap()),
            "Set(\"K\", <redacted>)"
        );
    }

    #[test]
    fn values_apply_in_order_over_the_client_environment() {
        let client = [("KEEP", "c"), ("GONE", "g"), ("BACK", "b")]
            .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let values = [
            "NEW=%bazel_workspace%/x",
            "=GONE",
            "=BACK",
            "BACK",
            "MISSING",
            "KEEP=over",
            "=NEVER",
        ]
        .map(|v| convert(v).unwrap());
        let env = repository_environment(client, &values, Some("/ws"));
        let env = env
            .iter()
            .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
            .collect::<Vec<_>>();
        assert_eq!(env, ["BACK=b", "KEEP=over", "NEW=/ws/x"]);
        // Inherit after Set keeps the set value when the client lacks it.
        let values = ["A=1", "A"].map(|v| convert(v).unwrap());
        let env = repository_environment(Vec::new(), &values, None);
        assert_eq!(env, [(OsString::from("A"), OsString::from("1"))]);
    }
}
