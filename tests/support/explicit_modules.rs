// Shared by integration tests (`include!("support/explicit_modules.rs")`)
// written for the native explicit-module profile.
//
// Like Bazel 9.3, `query`, `build`, `run` and `test` inject the built-in
// modules by default: `@bazel_tools` and its MODULE closure (rules_license,
// platforms, rules_cc, ...), which resolve through the selected registries
// (the default https://bcr.bazel.build/ without `--registry`). Tests whose
// fixtures predate that default rely on no `@bazel_tools` and on no registry
// access for the built-in closure (offline file registries without those
// modules, explicit-profile error texts), so they select the explicit-module
// profile with the native-only `--experimental-builtin-modules=false`.
// Tests of the built-in profile use the default together with the offline
// built-in registry fixture (support/module_archives.rs).
//
// `command.explicit_modules()`, chained after the last argument (right before
// `.output()`, `.status()` or `.spawn()`), appends the switch to a bazel-rs
// command line whose command is `query`, `build`, `run`, `test` or `mod`, unless
// the arguments already select a profile. Native options may follow the
// positional arguments; a residue after `--` (run arguments) cannot take it,
// so harnesses that run with one insert the switch right after the command
// word of their arguments instead: `with_explicit_modules(args)`.

/// The native-only switch selecting the explicit-module profile.
#[allow(dead_code)]
const EXPLICIT_MODULES: &str = "--experimental-builtin-modules=false";

/// Whether `arg` selects a module profile itself.
#[allow(dead_code)]
fn selects_module_profile(arg: &str) -> bool {
    arg.starts_with("--experimental-builtin-modules")
}

/// Whether `arg` is a command that takes the switch.
#[allow(dead_code)]
fn takes_module_profile(arg: &str) -> bool {
    matches!(arg, "query" | "build" | "run" | "test" | "mod")
}

/// `args` with [`EXPLICIT_MODULES`] right after the command word (see above).
#[allow(dead_code)]
fn with_explicit_modules<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut args = args.to_vec();
    if !args.iter().any(|arg| selects_module_profile(arg))
        && let Some(command) = args.iter().position(|arg| takes_module_profile(arg))
    {
        args.insert(command + 1, EXPLICIT_MODULES);
    }
    args
}

#[allow(dead_code)]
trait ExplicitModules {
    /// Append [`EXPLICIT_MODULES`] (see above).
    fn explicit_modules(&mut self) -> &mut Self;
}

impl ExplicitModules for std::process::Command {
    fn explicit_modules(&mut self) -> &mut Self {
        let args: Vec<String> = self
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        if args.iter().any(|arg| selects_module_profile(arg)) {
            return self;
        }
        let Some(command) = args.iter().position(|arg| takes_module_profile(arg)) else {
            return self;
        };
        assert!(
            !args[command..].iter().any(|arg| arg == "--"),
            "a `--` residue cannot take {EXPLICIT_MODULES}; pass it before `--` \
             (with_explicit_modules): {args:?}"
        );
        self.arg(EXPLICIT_MODULES)
    }
}
