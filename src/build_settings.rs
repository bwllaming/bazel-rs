//! Owned build-setting descriptors. These describe defaults, not configurations.
use allocative::Allocative;
use anyhow::bail;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::values::structs::AllocStruct;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Allocative)]
pub enum BuildSettingKind {
    Bool,
    Int,
    String,
    StringList,
}

// Only the upstream marker-trait derive generates unsafe code. The descriptor
// contains no heap references and can be copied unchanged across module freezes.
#[allow(unsafe_code)]
mod value {
    use super::*;
    use starlark::starlark_simple_value;
    use starlark::values::{
        NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, starlark_value,
    };
    use std::fmt;

    #[derive(
        Copy,
        Clone,
        Debug,
        Eq,
        PartialEq,
        Allocative,
        ProvidesStaticType,
        NoSerialize,
        StarlarkPagablePanic,
    )]
    pub struct BuildSetting {
        pub kind: BuildSettingKind,
        pub flag: bool,
        pub repeatable: bool,
    }

    impl fmt::Display for BuildSetting {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let kind = match self.kind {
                BuildSettingKind::Bool => "boolean",
                BuildSettingKind::Int => "int",
                BuildSettingKind::String => "string",
                BuildSettingKind::StringList => "list(string)",
            };
            write!(f, "<build_setting.{kind}>")
        }
    }

    // Also provides the public BuildSetting::from_value downcast helper.
    starlark_simple_value!(BuildSetting);
    #[starlark_value(type = "BuildSetting")]
    impl<'v> StarlarkValue<'v> for BuildSetting {}
}
pub use value::BuildSetting;

#[starlark_module]
fn config_globals(builder: &mut GlobalsBuilder) {
    fn bool(
        #[starlark(require = named, default = false)] flag: bool,
    ) -> anyhow::Result<BuildSetting> {
        Ok(BuildSetting {
            kind: BuildSettingKind::Bool,
            flag,
            repeatable: false,
        })
    }

    fn int(
        #[starlark(require = named, default = false)] flag: bool,
    ) -> anyhow::Result<BuildSetting> {
        Ok(BuildSetting {
            kind: BuildSettingKind::Int,
            flag,
            repeatable: false,
        })
    }

    fn string(
        #[starlark(require = named, default = false)] flag: bool,
        #[starlark(require = named, default = false)] allow_multiple: bool,
    ) -> anyhow::Result<BuildSetting> {
        if allow_multiple {
            bail!("config.string(allow_multiple=True) is unsupported");
        }
        Ok(BuildSetting {
            kind: BuildSettingKind::String,
            flag,
            repeatable: false,
        })
    }

    fn string_list(
        #[starlark(require = named, default = false)] flag: bool,
        #[starlark(require = named, default = false)] repeatable: bool,
    ) -> anyhow::Result<BuildSetting> {
        if repeatable && !flag {
            bail!("'repeatable' can only be set for a setting with 'flag = True'");
        }
        Ok(BuildSetting {
            kind: BuildSettingKind::StringList,
            flag,
            repeatable,
        })
    }
}

/// Install the config namespace into extension globals only.
pub fn install(globals: &mut GlobalsBuilder) {
    let config = GlobalsBuilder::new().with(config_globals).build();
    globals.frozen_heap().add_reference(config.heap());
    globals.set("config", AllocStruct(config.iter()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::Module;
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};
    use starlark::values::ValueLike;

    #[test]
    fn descriptors_have_owned_metadata_and_survive_freezing() {
        let globals = GlobalsBuilder::standard().with(install).build();
        for (constructor, kind, repr) in [
            ("bool", BuildSettingKind::Bool, "boolean"),
            ("int", BuildSettingKind::Int, "int"),
            ("string", BuildSettingKind::String, "string"),
            ("string_list", BuildSettingKind::StringList, "list(string)"),
        ] {
            for flag in [false, true] {
                let frozen = Module::with_temp_heap(|module| {
                    let source = format!(
                        "x = config.{constructor}({})",
                        if flag { "flag=True" } else { "" }
                    );
                    let ast = AstModule::parse("config.bzl", source, &Dialect::Standard).unwrap();
                    Evaluator::new(&module).eval_module(ast, &globals).unwrap();
                    let value = module.get("x").unwrap();
                    assert_eq!(value.get_type(), "BuildSetting");
                    assert_eq!(value.to_repr(), format!("<build_setting.{repr}>"));
                    assert_eq!(
                        *BuildSetting::from_value(value).unwrap(),
                        BuildSetting {
                            kind,
                            flag,
                            repeatable: false
                        }
                    );
                    module.freeze().unwrap()
                });
                let value = frozen.get("x").unwrap();
                assert_eq!(
                    *BuildSetting::from_value(value.value().to_value()).unwrap(),
                    BuildSetting {
                        kind,
                        flag,
                        repeatable: false
                    }
                );
            }
        }
    }

    #[test]
    fn constructors_validate_options_without_exposing_descriptor_fields() {
        let globals = GlobalsBuilder::standard().with(install).build();
        for (source, expected) in [
            ("config.bool(True)", "positional"),
            ("config.int(True)", "positional"),
            ("config.string(True)", "positional"),
            ("config.string_list(True)", "positional"),
            ("config.bool(flag=1)", "bool"),
            ("config.int(flag=None)", "bool"),
            ("config.string(allow_multiple=True)", "unsupported"),
            (
                "config.string(flag=True, allow_multiple=True)",
                "unsupported",
            ),
            ("config.string_list(repeatable=True)", "flag = True"),
            ("config.string_set()", "string_set"),
            ("config.bool().flag", "flag"),
        ] {
            Module::with_temp_heap(|module| {
                let ast =
                    AstModule::parse("config.bzl", source.into(), &Dialect::Standard).unwrap();
                let error = Evaluator::new(&module)
                    .eval_module(ast, &globals)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(expected), "{source}: {error}");
            });
        }
        Module::with_temp_heap(|module| {
            let ast = AstModule::parse("config.bzl", "x = config.string_list(flag=True, repeatable=True)\ny = config.string(allow_multiple=False)".into(), &Dialect::Standard).unwrap();
            Evaluator::new(&module).eval_module(ast, &globals).unwrap();
            assert_eq!(
                *BuildSetting::from_value(module.get("x").unwrap()).unwrap(),
                BuildSetting {
                    kind: BuildSettingKind::StringList,
                    flag: true,
                    repeatable: true
                }
            );
            assert_eq!(
                BuildSetting::from_value(module.get("y").unwrap())
                    .unwrap()
                    .kind,
                BuildSettingKind::String
            );
        });
    }
}
