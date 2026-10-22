//! Action environments, as Bazel 9.3 computes them (docs/ACTION_ENV.md).
//!
//! An [`ActionEnvironment`] has a fixed part (name -> value) and an inherited
//! part (names whose values come from the client environment when the action
//! executes), like `actions/ActionEnvironment.java`. A configuration's shell
//! environment is `BazelRuleClassProvider.SHELL_ACTION_ENV`
//! (bazel/rules/BazelRuleClassProvider.java:139-202); genrules use it as is
//! (rules/genrule/GenRuleBase.java:210-219), Starlark spawns through
//! `SpawnAction.createActionEnvironment` (analysis/actions/SpawnAction.java:584-614).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;

/// The strict default `PATH` on non-Windows hosts
/// (BazelRuleClassProvider.pathOrDefault, :347-361).
pub const STRICT_PATH: &str = "/bin:/usr/bin:/usr/local/bin";

/// One `--action_env`/`--host_action_env`/`--test_env` entry
/// (common/options/Converters.java:600-618).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EnvVar {
    /// `name=value`: a fixed value.
    Set(String, String),
    /// `name`: inherited from the client environment.
    Inherit(String),
    /// `=name`: removes any earlier assignment (and an inherited default).
    Unset(String),
}

impl EnvVar {
    /// Converters.EnvVarsConverter.convert (Converters.java:627-644).
    pub fn parse(input: &str) -> Result<Self, String> {
        if input.is_empty() || input == "=" {
            return Err("Variable definitions must be in the form of a 'name=value', 'name', or '=name' assignment".to_owned());
        }
        Ok(match input.find('=') {
            Some(0) => Self::Unset(input[1..].to_owned()),
            None => Self::Inherit(input.to_owned()),
            Some(position) => Self::Set(
                input[..position].to_owned(),
                input[position + 1..].to_owned(),
            ),
        })
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Set(name, _) | Self::Inherit(name) | Self::Unset(name) => name,
        }
    }

    /// EnvVarsConverter.reverseForStarlark (Converters.java:651-663): the
    /// command-line spelling, also the configuration identity form.
    pub fn reverse(&self) -> String {
        match self {
            Self::Set(name, value) => format!("{name}={value}"),
            Self::Inherit(name) => name.clone(),
            Self::Unset(name) => format!("={name}"),
        }
    }
}

impl serde::Serialize for EnvVar {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.reverse())
    }
}

/// FragmentOptions.normalizeEnvVars (analysis/config/FragmentOptions.java:119-129):
/// a `LinkedHashMap` by name, so the last entry per name wins at the
/// position of the name's first entry.
pub fn normalize(entries: Vec<EnvVar>) -> Vec<EnvVar> {
    let mut order: Vec<String> = Vec::new();
    let mut last: BTreeMap<String, EnvVar> = BTreeMap::new();
    for entry in entries {
        if last
            .insert(entry.name().to_owned(), entry.clone())
            .is_none()
        {
            order.push(entry.name().to_owned());
        }
    }
    order
        .into_iter()
        .map(|name| last.remove(&name).expect("normalized entry"))
        .collect()
}

/// Apply entries in order: Set puts, Inherit puts an inherited marker,
/// Unset removes (BazelRuleClassProvider.java:179-187,
/// BuildConfigurationValue.setupTestEnvironment :186-193).
fn apply(env: &mut BTreeMap<String, Option<String>>, entries: &[EnvVar]) {
    for entry in entries {
        match entry {
            EnvVar::Set(name, value) => {
                env.insert(name.clone(), Some(value.clone()));
            }
            EnvVar::Inherit(name) => {
                env.insert(name.clone(), None);
            }
            EnvVar::Unset(name) => {
                env.remove(name);
            }
        }
    }
}

/// Fixed and inherited environment variables of an action or configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ActionEnvironment {
    pub fixed: BTreeMap<String, String>,
    pub inherited: BTreeSet<String>,
}

impl ActionEnvironment {
    /// ActionEnvironment.split (ActionEnvironment.java:62-79): `None` values
    /// are inherited.
    pub fn split(env: BTreeMap<String, Option<String>>) -> Self {
        let mut result = Self::default();
        for (name, value) in env {
            match value {
                Some(value) => {
                    result.fixed.insert(name, value);
                }
                None => {
                    result.inherited.insert(name);
                }
            }
        }
        result
    }

    /// A configuration's shell environment, SHELL_ACTION_ENV
    /// (BazelRuleClassProvider.java:139-202) on a non-Windows host:
    /// strict: `PATH` fixed to [`STRICT_PATH`]; otherwise `PATH` and
    /// `LD_LIBRARY_PATH` inherited; then the configuration's `--action_env`
    /// (the exec configuration's is its `--host_action_env`,
    /// crate::config_registry::exec_transition). Runfiles are always enabled
    /// natively, so `RUNFILES_MANIFEST_ONLY` is never set (:189-197).
    pub fn shell(strict: bool, action_env: &[EnvVar]) -> Self {
        let mut env = BTreeMap::new();
        if strict {
            env.insert("PATH".to_owned(), Some(STRICT_PATH.to_owned()));
        } else {
            env.insert("LD_LIBRARY_PATH".to_owned(), None);
            env.insert("PATH".to_owned(), None);
        }
        apply(&mut env, action_env);
        Self::split(env)
    }

    /// The configuration's test environment
    /// (BuildConfigurationValue.setupTestEnvironment, :180-195).
    pub fn test(test_env: &[EnvVar]) -> Self {
        let mut env = BTreeMap::new();
        apply(&mut env, test_env);
        Self::split(env)
    }

    /// `ctx.configuration.default_shell_env`:
    /// BuildConfigurationValue.getLocalShellEnvironment, the fixed part
    /// (BuildConfigurationValue.java:556-558).
    pub fn default_shell_env(&self) -> &BTreeMap<String, String> {
        &self.fixed
    }

    /// SpawnAction.createActionEnvironment (SpawnAction.java:584-614) of a
    /// Starlark spawn in a configuration with this shell environment.
    pub fn for_spawn(&self, use_default_shell_env: bool, env: BTreeMap<String, String>) -> Self {
        if !use_default_shell_env {
            return Self {
                fixed: env,
                inherited: BTreeSet::new(),
            };
        }
        // Inherited variables override fixed ones when resolved, so the
        // inherited names the action's `env` sets are filtered out; `env`
        // then overrides the fixed part (withAdditionalFixedVariables).
        let mut result = self.clone();
        result.inherited.retain(|name| !env.contains_key(name));
        result.fixed.extend(env);
        result
    }

    /// ActionEnvironment.resolve (ActionEnvironment.java:113-122) of the
    /// inherited part: the inherited names the client environment sets,
    /// with their values (they override fixed values of the same name).
    pub fn resolve_inherited(
        inherited: &BTreeSet<String>,
        client: &BTreeMap<String, OsString>,
    ) -> BTreeMap<String, OsString> {
        inherited
            .iter()
            .filter_map(|name| Some((name.clone(), client.get(name)?.clone())))
            .collect()
    }
}

/// The client environment actions inherit from: the process environment,
/// read once per invocation (names that are not UTF-8 cannot be requested
/// by `--action_env` and are skipped).
pub fn client_environment() -> BTreeMap<String, OsString> {
    // snapshot-unrecorded: inherited action-env values are re-resolved after planning on every run (PLAN-L2 §1a, finalize).
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(entries: &[&str]) -> Vec<EnvVar> {
        normalize(entries.iter().map(|e| EnvVar::parse(e).unwrap()).collect())
    }

    #[test]
    fn converter_follows_env_vars_converter() {
        assert_eq!(
            EnvVar::parse("A=b=c").unwrap(),
            EnvVar::Set("A".into(), "b=c".into())
        );
        assert_eq!(
            EnvVar::parse("A=").unwrap(),
            EnvVar::Set("A".into(), "".into())
        );
        assert_eq!(EnvVar::parse("A").unwrap(), EnvVar::Inherit("A".into()));
        assert_eq!(EnvVar::parse("=A").unwrap(), EnvVar::Unset("A".into()));
        assert_eq!(EnvVar::parse("==A").unwrap(), EnvVar::Unset("=A".into()));
        for bad in ["", "="] {
            assert_eq!(
                EnvVar::parse(bad).unwrap_err(),
                "Variable definitions must be in the form of a 'name=value', 'name', or '=name' assignment"
            );
        }
        for spelling in ["A=b=c", "A", "=A", "A="] {
            assert_eq!(EnvVar::parse(spelling).unwrap().reverse(), spelling);
        }
    }

    #[test]
    fn normalization_keeps_the_last_entry_at_the_first_position() {
        assert_eq!(
            parse(&["A=1", "B", "A=2", "=C"]),
            [
                EnvVar::Set("A".into(), "2".into()),
                EnvVar::Inherit("B".into()),
                EnvVar::Unset("C".into()),
            ]
        );
    }

    #[test]
    fn shell_environment_follows_shell_action_env() {
        let strict = ActionEnvironment::shell(true, &[]);
        assert_eq!(
            strict.fixed,
            BTreeMap::from([("PATH".into(), STRICT_PATH.into())])
        );
        assert!(strict.inherited.is_empty());
        let loose = ActionEnvironment::shell(false, &[]);
        assert!(loose.fixed.is_empty());
        assert_eq!(
            loose.inherited,
            BTreeSet::from(["LD_LIBRARY_PATH".into(), "PATH".into()])
        );
        let env = ActionEnvironment::shell(true, &parse(&["FOO=bar", "HOME", "=PATH"]));
        assert_eq!(env.fixed, BTreeMap::from([("FOO".into(), "bar".into())]));
        assert_eq!(env.inherited, BTreeSet::from(["HOME".into()]));
        // An inherited PATH replaces the fixed default.
        let env = ActionEnvironment::shell(true, &parse(&["PATH"]));
        assert!(env.fixed.is_empty());
        assert_eq!(env.inherited, BTreeSet::from(["PATH".into()]));
        assert_eq!(env.default_shell_env(), &BTreeMap::new());
    }

    #[test]
    fn spawn_environment_follows_create_action_environment() {
        let config = ActionEnvironment::shell(true, &parse(&["HOME", "USER", "FOO=bar"]));
        let env = BTreeMap::from([("HOME".to_owned(), "/x".to_owned())]);
        let spawn = config.for_spawn(true, env.clone());
        assert_eq!(
            spawn.fixed,
            BTreeMap::from([
                ("FOO".into(), "bar".into()),
                ("HOME".into(), "/x".into()),
                ("PATH".into(), STRICT_PATH.into()),
            ])
        );
        assert_eq!(spawn.inherited, BTreeSet::from(["USER".into()]));
        let own = config.for_spawn(false, env.clone());
        assert_eq!(own.fixed, env);
        assert!(own.inherited.is_empty());
        assert_eq!(config.for_spawn(true, BTreeMap::new()), config);
    }

    #[test]
    fn inherited_values_resolve_from_the_client_environment() {
        let client = BTreeMap::from([
            ("HOME".to_owned(), OsString::from("/home/u")),
            ("OTHER".to_owned(), OsString::from("x")),
        ]);
        let inherited = BTreeSet::from(["HOME".to_owned(), "MISSING".to_owned()]);
        assert_eq!(
            ActionEnvironment::resolve_inherited(&inherited, &client),
            BTreeMap::from([("HOME".to_owned(), OsString::from("/home/u"))])
        );
    }

    #[test]
    fn test_environment_splits_test_env() {
        let env = ActionEnvironment::test(&parse(&["A=1", "B", "=A"]));
        assert!(env.fixed.is_empty());
        assert_eq!(env.inherited, BTreeSet::from(["B".into()]));
    }
}
