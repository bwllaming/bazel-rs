//! `print_action` (runtime/commands/PrintActionCommand.java,
//! docs/COMMANDS.md): the ExtraActionInfo of every action of each requested
//! target reachable from its files to build, as an ExtraActionSummary text
//! proto (TextFormat.printer().printToString). Natively the targets are
//! analyzed with aquery's action planning (crate::engine::aquery) and not
//! built.
use std::collections::{BTreeSet, HashMap};

use crate::aquery::ActionRecord;
use crate::engine::aquery::{Analysis, TopLevel};
use crate::starlark_doc_extract::Message;

/// A print_action failure: the error event and the exit code
/// (FailureDetails.PrintActionCommand codes map to exit 1).
#[derive(Debug, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub code: i32,
}

/// `--print_action_mnemonics` (PrintActionOptions), and whether an option
/// consumed the next argument.
pub fn parse_option(arg: &str, next: Option<&str>, mnemonics: &mut Vec<String>) -> Option<bool> {
    if let Some(value) = arg.strip_prefix("--print_action_mnemonics=") {
        mnemonics.push(value.to_owned());
        return Some(false);
    }
    if arg == "--print_action_mnemonics" {
        mnemonics.push(next.unwrap_or_default().to_owned());
        return Some(next.is_some());
    }
    None
}

/// The query expression of the requested target patterns (their union;
/// a `-` pattern subtracts).
pub fn expression(patterns: &[String]) -> String {
    let mut expression = String::new();
    for pattern in patterns {
        if let Some(negative) = pattern.strip_prefix('-') {
            expression.push_str(&format!(" - {negative}"));
        } else if expression.is_empty() {
            expression.push_str(pattern);
        } else {
            expression.push_str(&format!(" + {pattern}"));
        }
    }
    expression
}

/// Spawn.getInputFiles in NestedSet order: a Starlark spawn's recorded
/// order; otherwise the generated inputs before the source ones (the
/// order C++ archive and link actions list them), each once.
fn ordered_inputs(action: &ActionRecord) -> Vec<String> {
    let known: BTreeSet<&str> = action.inputs.iter().map(String::as_str).collect();
    let mut seen = BTreeSet::new();
    let mut ordered = Vec::new();
    if let Some(order) = &action.input_order {
        for input in order {
            if known.contains(input.as_str()) && seen.insert(input.clone()) {
                ordered.push(input.clone());
            }
        }
    }
    let (generated, sources): (Vec<&String>, Vec<&String>) = action
        .inputs
        .iter()
        .partition(|input| input.starts_with("bazel-out/"));
    for input in generated.into_iter().chain(sources) {
        if seen.insert(input.clone()) {
            ordered.push(input.clone());
        }
    }
    ordered
}

fn variables(info: &mut Message, number: u32, environment: &[(String, String)]) {
    for (name, value) in environment {
        let mut variable = Message::default();
        variable.set_str(1, "name", name.as_str());
        variable.set_str(2, "value", value.as_str());
        info.msg(number, "variable", variable);
    }
}

/// Action.getExtraActionInfo: AbstractAction's owner, id and mnemonic,
/// with SpawnAction's SpawnInfo or CppCompileAction's CppCompileInfo.
fn extra_action_info(action: &ActionRecord) -> Result<Message, Failure> {
    let mut info = Message::default();
    info.set_str(1, "owner", action.target.as_str());
    info.set_str(2, "id", action.action_key.as_str());
    info.set_str(5, "mnemonic", action.mnemonic.as_str());
    let outputs = || action.outputs.iter().map(|(path, _)| path.clone());
    match (&action.arguments, action.mnemonic.as_str()) {
        (_, "Javac" | "JavacTurbine" | "Turbine") => {
            return Err(Failure {
                message: format!(
                    "print_action: the JavaCompileInfo of the {} action of {} is not implemented in the native profile",
                    action.mnemonic, action.target
                ),
                code: 1,
            });
        }
        (Some(arguments), "CppCompile") => {
            let mut compile = Message::default();
            if let Some(tool) = arguments.first() {
                compile.set_str(1, "tool", tool.as_str());
            }
            for option in arguments.iter().skip(1) {
                compile.set_str(2, "compiler_option", option.as_str());
            }
            let source = arguments
                .iter()
                .position(|argument| argument == "-c")
                .and_then(|index| arguments.get(index + 1));
            if let Some(source) = source {
                compile.set_str(3, "source_file", source.as_str());
            }
            if let Some(output) = outputs().next() {
                compile.set_str(4, "output_file", output);
            }
            // The declared inputs (genuine lists the executed action's,
            // after include scanning): non-headers, then headers.
            let header = |input: &&String| {
                [".h", ".hh", ".hpp", ".hxx", ".inc", ".ipp", ".tcc"]
                    .iter()
                    .any(|extension| input.ends_with(extension))
            };
            for input in action.inputs.iter().filter(|input| !header(input)) {
                compile.set_str(5, "sources_and_headers", input.as_str());
            }
            for input in action.inputs.iter().filter(header) {
                compile.set_str(5, "sources_and_headers", input.as_str());
            }
            variables(&mut compile, 6, &action.environment);
            info.msg(1001, "[blaze.CppCompileInfo.cpp_compile_info]", compile);
        }
        (Some(arguments), _) => {
            let mut spawn = Message::default();
            for argument in arguments {
                spawn.set_str(1, "argument", argument.as_str());
            }
            variables(&mut spawn, 2, &action.environment);
            for input in ordered_inputs(action) {
                spawn.set_str(4, "input_file", input);
            }
            for output in outputs() {
                spawn.set_str(5, "output_file", output);
            }
            info.msg(1003, "[blaze.SpawnInfo.spawn_info]", spawn);
        }
        (None, _) => {}
    }
    Ok(info)
}

/// PrintActionVisitor over the files to build: an artifact's generating
/// action owned by the target (same label and configuration) is visited
/// before its inputs, each action and artifact once.
fn target_actions<'a>(
    target: &TopLevel,
    generating: &HashMap<(&str, &str), &'a ActionRecord>,
    mnemonics: &BTreeSet<String>,
) -> Vec<&'a ActionRecord> {
    let mut visited_artifacts: BTreeSet<String> = BTreeSet::new();
    let mut visited_actions: BTreeSet<*const ActionRecord> = BTreeSet::new();
    let mut found = Vec::new();
    let mut stack: Vec<String> = target.files_to_build.iter().rev().cloned().collect();
    while let Some(artifact) = stack.pop() {
        if !visited_artifacts.insert(artifact.clone()) {
            continue;
        }
        let Some(action) = generating.get(&(artifact.as_str(), target.checksum.as_str())) else {
            continue;
        };
        if action.target != target.label || !visited_actions.insert(std::ptr::from_ref(*action)) {
            continue;
        }
        if mnemonics.is_empty() || mnemonics.contains(&action.mnemonic) {
            found.push(*action);
        }
        stack.extend(ordered_inputs(action).into_iter().rev());
    }
    found
}

/// PrintActionRunner.printActionsForTargets over an analysis of the
/// requested targets: the summary text, or the failure.
pub fn summary(analysis: &Analysis, mnemonics: &[String]) -> Result<String, Failure> {
    let mnemonics: BTreeSet<String> = mnemonics.iter().cloned().collect();
    let mut generating: HashMap<(&str, &str), &ActionRecord> = HashMap::new();
    for (action, checksum) in analysis.actions_with_checksums() {
        if !action.aspects.is_empty() {
            continue;
        }
        for (output, _) in &action.outputs {
            generating
                .entry((output.as_str(), checksum))
                .or_insert(action);
        }
    }
    let mut targets: Vec<&TopLevel> = analysis.top_level().iter().collect();
    targets.sort_by(|a, b| a.label.cmp(&b.label));
    let mut summary = Message::default();
    for target in targets {
        if target.files_to_compile.is_empty() {
            return Err(Failure {
                message: format!(
                    "ConfiguredTarget({}, {}) is not a supported target kind",
                    target.label, target.checksum
                ),
                code: 1,
            });
        }
        for action in target_actions(target, &generating, &mnemonics) {
            let mut detail = Message::default();
            detail.msg(2, "action", extra_action_info(action)?);
            summary.msg(1, "action", detail);
        }
    }
    if summary.is_empty() {
        return Err(Failure {
            message: "no actions to print were found".to_owned(),
            code: 1,
        });
    }
    Ok(summary.to_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mnemonics_and_patterns() {
        let mut mnemonics = Vec::new();
        assert_eq!(
            parse_option("--print_action_mnemonics=A", None, &mut mnemonics),
            Some(false)
        );
        assert_eq!(
            parse_option("--print_action_mnemonics", Some("B"), &mut mnemonics),
            Some(true)
        );
        assert_eq!(parse_option("--other", None, &mut mnemonics), None);
        assert_eq!(mnemonics, ["A", "B"]);
        assert_eq!(
            expression(&["//a".into(), "//b:all".into(), "-//b:c".into()]),
            "//a + //b:all - //b:c"
        );
    }

    #[test]
    fn spawn_info_text() {
        let action = ActionRecord {
            target: "//pkg:s".into(),
            mnemonic: "MyShell".into(),
            action_key: "k".into(),
            arguments: Some(vec!["/bin/bash".into(), String::new()]),
            environment: vec![("ZED".into(), "1".into())],
            inputs: vec!["pkg/a.h".into()],
            outputs: vec![("bazel-out/x".into(), false)],
            ..ActionRecord::default()
        };
        let mut detail = Message::default();
        detail.msg(2, "action", extra_action_info(&action).unwrap());
        let mut summary = Message::default();
        summary.msg(1, "action", detail);
        assert_eq!(
            summary.to_text(),
            "action {\n  action {\n    owner: \"//pkg:s\"\n    id: \"k\"\n    mnemonic: \"MyShell\"\n    [blaze.SpawnInfo.spawn_info] {\n      argument: \"/bin/bash\"\n      argument: \"\"\n      variable {\n        name: \"ZED\"\n        value: \"1\"\n      }\n      input_file: \"pkg/a.h\"\n      output_file: \"bazel-out/x\"\n    }\n  }\n}\n"
        );
    }
}
