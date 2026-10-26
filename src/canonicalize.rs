//! `canonicalize-flags` (runtime/commands/CanonicalizeCommand.java): the
//! canonical form of the options after `--` for `--for_command` (default
//! `build`), OptionsParserImpl.asCanonicalizedList's deprecated canonical
//! form: one `--name=value` per effective option instance in the order they
//! were applied (a single-valued option once, at its last occurrence;
//! allowMultiple ones every time), booleans as `1`/`0`, old names and
//! abbreviations resolved, expansion options replaced by their expansion.
//! Options are the genuine Bazel 9.3.0 tables (src/option_metadata.rs,
//! src/command_options.rs). Starlark options (`--//pkg:flag`) and
//! `--invocation_policy` are not supported natively (explicit errors).
use crate::option_metadata::{COMMANDS, O, OPTIONS};

/// runtime.getCommandMap().keySet() of genuine Bazel 9.3.0.
const COMMAND_MAP_ORDER: &str = "mobile-install, fetch, mod, vendor, build, canonicalize-flags, clean, coverage, dump, help, info, print_action, query, run, shutdown, test, version, aquery, cquery, config, license";

fn find(name: &str, bit: usize) -> Option<&'static O> {
    let canonical = crate::command_options::COMMAND_OPTIONS
        .iter()
        .find(|(_, old, ..)| !old.is_empty() && *old == name)
        .map_or(name, |(new, ..)| *new);
    OPTIONS
        .iter()
        .find(|option| option.name == canonical && option.commands & (1 << bit) != 0)
}

/// BooleanConverter.
fn boolean(arg: &str, value: &str) -> Result<&'static str, String> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "t" | "y" => Ok("1"),
        "false" | "0" | "no" | "f" | "n" => Ok("0"),
        lower => Err(format!(
            "While parsing option {arg}: '{lower}' is not a boolean"
        )),
    }
}

struct Parsed {
    /// (priority, name, value), single-valued options keep only the last.
    instances: Vec<((usize, usize), &'static str, String)>,
}

impl Parsed {
    fn put(&mut self, priority: (usize, usize), option: &'static O, value: String) {
        if !option.multiple {
            self.instances.retain(|(_, name, _)| *name != option.name);
        }
        self.instances.push((priority, option.name, value));
    }

    /// One option occurrence (and its expansion).
    fn apply(
        &mut self,
        bit: usize,
        args: &[String],
        index: &mut usize,
        priority: (usize, usize),
    ) -> Result<(), String> {
        let arg = args[*index].clone();
        *index += 1;
        let unrecognized = || format!("Unrecognized option: {arg}");
        let (option, value): (&O, Option<String>) = if let Some(rest) = arg.strip_prefix("--") {
            if rest.starts_with("//") || rest.starts_with('@') {
                return Err(format!(
                    "{arg}: Starlark options are not supported by native canonicalize-flags"
                ));
            }
            let (name, value) = match rest.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (rest, None),
            };
            if let Some(option) = find(name, bit) {
                (option, value)
            } else if let Some(option) = name
                .strip_prefix("no")
                .and_then(|name| find(name, bit))
                .filter(|option| option.boolean)
            {
                if value.is_some() {
                    return Err(format!("Unexpected value after boolean option: {arg}"));
                }
                (option, Some("0".to_owned()))
            } else {
                return Err(unrecognized());
            }
        } else if let Some(abbreviation) = arg.strip_prefix('-').filter(|rest| rest.len() == 1) {
            let letter = abbreviation.chars().next().unwrap_or('\0');
            let option = OPTIONS
                .iter()
                .find(|option| option.abbrev == letter && option.commands & (1 << bit) != 0)
                .ok_or_else(unrecognized)?;
            (option, None)
        } else {
            return Err(unrecognized());
        };
        if !option.expansion.is_empty() {
            if value.is_some() && option.converter == "Void" {
                return Err(format!(
                    "Unexpected value after option without a value: {arg}"
                ));
            }
            let expansion: Vec<String> = option.expansion.iter().map(|s| (*s).to_owned()).collect();
            let mut inner = 0;
            while inner < expansion.len() {
                let sub = (priority.0, priority.1 * 1000 + inner + 1);
                self.apply(bit, &expansion, &mut inner, sub)?;
            }
            return Ok(());
        }
        let value = if option.boolean {
            match value {
                None => "1".to_owned(),
                Some(value) if value == "0" && arg.starts_with("--no") => value,
                Some(value) if option.converter == "Boolean" => boolean(&arg, &value)?.to_owned(),
                Some(value) => value,
            }
        } else {
            match value {
                Some(value) => value,
                None => {
                    let next = args
                        .get(*index)
                        .ok_or_else(|| format!("Expected value after {arg}"))?
                        .clone();
                    *index += 1;
                    next
                }
            }
        };
        self.put(priority, option, value);
        Ok(())
    }
}

/// `canonicalize-flags ARGS`: the lines to print, or the error (exit 2).
pub fn run(args: &[String]) -> Result<String, String> {
    let separator = args.iter().position(|arg| arg == "--");
    let (own, flags) = match separator {
        Some(position) => (&args[..position], &args[position + 1..]),
        None => (args, &[][..]),
    };
    let own_bit = COMMANDS
        .iter()
        .position(|name| *name == "canonicalize-flags")
        .expect("canonicalize-flags");
    let mut for_command = "build".to_owned();
    let mut residue: Vec<String> = Vec::new();
    let mut index = 0;
    while index < own.len() {
        let arg = &own[index];
        index += 1;
        if let Some(value) = arg.strip_prefix("--for_command=") {
            for_command = value.to_owned();
        } else if arg == "--for_command" {
            for_command = own
                .get(index)
                .ok_or("Expected value after --for_command")?
                .clone();
            index += 1;
        } else if arg.starts_with("--invocation_policy") && arg != "--invocation_policy=" {
            return Err(
                "--invocation_policy is not supported by native canonicalize-flags".to_owned(),
            );
        } else if let Some(rest) = arg.strip_prefix("--") {
            // The command's own (and inherited build) options; no effect here.
            let name = rest.split_once('=').map_or(rest, |(name, _)| name);
            let known = find(name, own_bit).or_else(|| {
                name.strip_prefix("no")
                    .and_then(|name| find(name, own_bit))
                    .filter(|option| option.boolean)
            });
            match known {
                Some(option) if !option.boolean && !rest.contains('=') => index += 1,
                Some(_) => {}
                None => return Err(format!("Unrecognized option: {arg}")),
            }
        } else {
            residue.push(arg.clone());
        }
    }
    let Some(bit) = COMMANDS
        .iter()
        .position(|name| *name == for_command && *name != "startup")
    else {
        return Err(format!(
            "Not a valid command: '{for_command}' (should be one of {COMMAND_MAP_ORDER})"
        ));
    };
    let mut parsed = Parsed {
        instances: Vec::new(),
    };
    let flags: Vec<String> = residue.into_iter().chain(flags.iter().cloned()).collect();
    // Non-option arguments are the parser's residue.
    let mut leftover = Vec::new();
    let mut index = 0;
    while index < flags.len() {
        if !flags[index].starts_with('-') || flags[index] == "-" {
            leftover.push(flags[index].clone());
            index += 1;
            continue;
        }
        let position = index;
        parsed.apply(bit, &flags, &mut index, (position, 0))?;
    }
    if !leftover.is_empty() {
        return Err(format!("Unrecognized arguments: {}", leftover.join(" ")));
    }
    parsed.instances.sort_by_key(|(priority, ..)| *priority);
    Ok(parsed
        .instances
        .iter()
        .map(|(_, name, value)| format!("--{name}={value}\n"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn canonical_forms() {
        assert_eq!(
            run(&args(&[
                "--",
                "--jobs",
                "3",
                "-c",
                "opt",
                "--keep_going",
                "--nokeep_going"
            ]))
            .unwrap(),
            "--jobs=3\n--compilation_mode=opt\n--keep_going=0\n"
        );
        assert_eq!(
            run(&args(&["--", "--experimental_spawn_scheduler", "--jobs=4"])).unwrap(),
            "--internal_spawn_scheduler=1\n--spawn_strategy=dynamic\n--jobs=4\n"
        );
        assert_eq!(
            run(&args(&["--", "--bogus"])).unwrap_err(),
            "Unrecognized option: --bogus"
        );
    }
}
