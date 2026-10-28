//! JavaBuilder's two unpublished local attempts inside the parent's single
//! cache key/lock/publication transaction. Ordinary spawns do not enter here.
use super::*;
use crate::java_compile::ClasspathMode;

fn cancelled(action: &Action) -> Result<(), String> {
    if action
        .execution_cancel
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Acquire))
    {
        Err(format!(
            "action '{}' cancelled between Java compilation attempts",
            action.id
        ))
    } else {
        Ok(())
    }
}

fn bounded_jdeps(path: &Path, cache: Option<&Path>) -> Result<Vec<u8>, String> {
    let metadata = regular_file(path, cache)?;
    if metadata.len() > crate::java_deps::MAX_BYTES as u64 {
        return Err(format!(
            "Java .jdeps '{}' exceeds 16 MiB limit",
            path.display()
        ));
    }
    let file = File::open(path).map_err(|e| io_error("cannot read Java .jdeps", path, e))?;
    let mut bytes = Vec::new();
    file.take(crate::java_deps::MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error("cannot read Java .jdeps", path, e))?;
    if bytes.len() > crate::java_deps::MAX_BYTES {
        return Err(format!(
            "Java .jdeps '{}' exceeds 16 MiB limit",
            path.display()
        ));
    }
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    logical: &Action,
    prepared: &PreparedAction,
    work: &Path,
    execroot: &Path,
    tmp: &Path,
    cas: &Path,
    root: &Path,
    strategy: crate::sandbox::Strategy,
) -> Result<(), String> {
    let spec = logical
        .extra
        .java_compile
        .as_ref()
        .expect("Java action policy");
    cancelled(logical)?;
    let sources: BTreeMap<&Path, &Path> = logical
        .inputs
        .iter()
        .map(|(source, dest)| (dest.as_path(), source.as_path()))
        .collect();
    let reduced = spec.reduced_classpath(|path| {
        let source = sources.get(path).ok_or_else(|| {
            format!(
                "Java dependency '{}' has no resolved producer input",
                path.display()
            )
        })?;
        if let Some(file) = crate::remote_outputs::get(source)
            && file.size > crate::java_deps::MAX_BYTES as u64
        {
            return Err(format!(
                "remote Java .jdeps '{}' exceeds 16 MiB limit",
                path.display()
            ));
        }
        materialize(cas, root, source)?;
        bounded_jdeps(source, Some(root))
    })?;
    let mut info = logical.execution_info.clone();
    info.extend(logical.extra.execution_info.keys().cloned());
    for fallback in [false, true] {
        cancelled(logical)?;
        if fallback {
            // Checked cleanup, not WorkDir's best-effort Drop. This also
            // discards undeclared temporaries and every staged-input view.
            clear_directory(execroot)?;
            for name in ["stdout.log", "stderr.log"] {
                fs::remove_file(work.join(name)).map_err(|e| {
                    io_error("cannot discard reduced Java log", &work.join(name), e)
                })?;
            }
        }
        let attempt = spec.attempt(logical, &reduced, fallback)?;
        let selected: BTreeSet<&Path> = attempt
            .inputs
            .iter()
            .map(|(_, dest)| dest.as_path())
            .collect();
        let inputs: BTreeMap<PathBuf, Staged> = prepared
            .inputs
            .iter()
            .filter(|(name, _)| selected.contains(name.as_path()))
            .map(|(name, input)| (name.clone(), input.clone()))
            .collect();
        let staged = stage_inputs(cas, root, &inputs, execroot)?;
        for (path, content) in &attempt.extra.param_files {
            write_file(&execroot.join(path), content, 0)?;
        }
        for output in &logical.outputs {
            if let Some(parent) = execroot.join(output).parent() {
                ensure_directory(parent)?;
            }
        }
        let result = spawn(&attempt, work, execroot, tmp, strategy, &info)?;
        check_staged(&staged, execroot).map_err(|e| format!("action '{}': {e}", logical.id))?;
        // Only field 5, after process success/input integrity, requests the
        // second attempt. success=false is valid in a fallback-only proto.
        let request_fallback =
            if result.status.success() && !fallback && spec.mode == ClasspathMode::Bazel {
                let output = spec.output_jdeps.as_ref().expect("validated BAZEL jdeps");
                let bytes = bounded_jdeps(&execroot.join(output), None)?;
                crate::java_deps::parse(&bytes)?.requires_fallback
            } else {
                false
            };
        if crate::exec_log::active() {
            let view = PreparedAction {
                inputs,
                origins: BTreeMap::new(),
                outputs: prepared.outputs.clone(),
                key: prepared.key.clone(),
            };
            log_spawn(
                &attempt,
                &view,
                execroot,
                strategy.runner_name(),
                result.status.code().unwrap_or(1),
                false,
                (result.started, result.wall),
            );
        }
        if !result.status.success() {
            return Err(format!(
                "action '{}' failed ({}):\nstdout:\n{}\nstderr:\n{}",
                logical.id, result.status, result.stdout, result.stderr
            ));
        }
        if request_fallback {
            // The buffers are dropped; the next spawn truncates its log files.
            // No output metadata/digest/manifest has been produced yet.
            continue;
        }
        cancelled(logical)?;
        // All final outputs are checked before publishing any logs. The
        // parent's unchanged publication path revalidates and hashes them.
        for output in &logical.outputs {
            regular_file(&execroot.join(output), None).map_err(|error| {
                format!(
                    "action '{}' has missing or invalid output '{}': {error}",
                    logical.id,
                    output.display()
                )
            })?;
        }
        replay_logs(&logical.id, &result.stdout, &result.stderr);
        return Ok(());
    }
    unreachable!("fallback attempt is terminal")
}

struct AttemptResult {
    stdout: String,
    stderr: String,
    status: std::process::ExitStatus,
    started: SystemTime,
    wall: std::time::Duration,
}

fn spawn(
    action: &Action,
    work: &Path,
    execroot: &Path,
    tmp: &Path,
    strategy: crate::sandbox::Strategy,
    info: &BTreeSet<String>,
) -> Result<AttemptResult, String> {
    let mut stdout = create_log(&work.join("stdout.log"))?;
    let mut stderr = create_log(&work.join("stderr.log"))?;
    let environment = resolved_environment(action);
    let argv = arguments(action);
    let sandboxed = strategy == crate::sandbox::Strategy::Sandboxed;
    report_subcommand(action, execroot, strategy.runner_name());
    crate::progress::spawn_started(match strategy {
        crate::sandbox::Strategy::Sandboxed => crate::progress::Strategy::LinuxSandbox,
        crate::sandbox::Strategy::ProcessWrapper => {
            crate::progress::Strategy::ProcessWrapperSandbox
        }
        _ => crate::progress::Strategy::Local,
    });
    let mut launch = if sandboxed {
        crate::process_wrapper::Launch::sandbox_worker(crate::sandbox::wrap(
            execroot,
            tmp,
            &environment,
            crate::sandbox::denies_network(info),
            &argv,
            work,
            None,
        ))
    } else if action.executable_file && !argv[0].starts_with('/') {
        crate::process_wrapper::Launch::new(
            execroot.join(&argv[0]),
            Some(OsStr::new(&argv[0])),
            &argv[1..],
        )
    } else {
        crate::process_wrapper::Launch::new(&argv[0], None, &argv[1..])
    };
    launch
        .command()
        .current_dir(execroot)
        .env_clear()
        .envs(&environment)
        .env("TMPDIR", tmp)
        .stdout(Stdio::from(stdout.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(stderr.try_clone().map_err(|e| e.to_string())?));
    let started = SystemTime::now();
    let clock = std::time::Instant::now();
    let mut child = retry_spawn(|| launch.start(), std::thread::sleep)
        .map_err(|e| format!("cannot execute action '{}': {e}", action.id))?;
    let status = child
        .wait()
        .map_err(|e| format!("cannot wait for action '{}': {e}", action.id))?;
    crate::sandbox::count(strategy);
    let stdout = log_tail(&mut stdout).map_err(|e| e.to_string())?;
    let stderr = log_tail(&mut stderr).map_err(|e| e.to_string())?;
    if sandboxed
        && let Ok(error) = fs::read_to_string(work.join("sandbox-setup-error"))
        && !error.is_empty()
    {
        crate::sandbox::note_setup_failure();
        return Err(format!(
            "action '{}' failed: could not set up the sandbox: {}",
            action.id,
            error.trim_end()
        ));
    }
    Ok(AttemptResult {
        stdout,
        stderr,
        status,
        started,
        wall: clock.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::java_compile::JavaCompileSpec;

    const COMPILER: &str = r#"set -eu
mode=$(tail -n 1 "${2#@}")
echo "$mode" >> "$TRACE"
test -f compiler.sh && test -f source && test -f a.jar
test ! -e input.jdeps
if [ "$mode" = BAZEL_REDUCED ]; then
  test ! -e b.jar
  case "$CASE" in
    missing) exit 0;;
    corrupt) printf '\000' > out/result.jdeps; exit 0;;
    absent_output) printf '\030\001' > out/result.jdeps; exit 0;;
    success) ;;
    *)
      echo 'discarded-attempt-stdout'
      echo 'discarded-attempt-stderr' >&2
      echo partial > out/result.jar
      echo undeclared > partial
      printf '\030\000\050\001' > out/result.jdeps
      if [ "$CASE" = initial_failure ]; then exit 4; fi
      if [ "$CASE" = cancel ]; then
        for i in $(seq 1 100); do
          if [ -e "$RELEASE" ]; then exit 0; fi
          sleep 0.02
        done
        exit 99
      fi
      exit 0;;
  esac
else
  test "$mode" = BAZEL_FALLBACK
  test -f b.jar
  test ! -e out/result.jar && test ! -e partial
  if [ "$CASE" = final_failure ]; then echo final-error >&2; exit 5; fi
fi
echo terminal-stdout
echo terminal-stderr >&2
cat source a.jar > out/result.jar
if [ -e b.jar ]; then cat b.jar >> out/result.jar; fi
echo headers > out/native.jar
echo manifest > out/result.manifest
printf '\030\001' > out/result.jdeps
if [ "$CASE" = second_fallback ]; then printf '\050\001' >> out/result.jdeps; fi
"#;

    struct Fixture {
        dir: tempfile::TempDir,
        action: Action,
    }
    impl Fixture {
        fn new(case: &str) -> Self {
            let dir = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
            let mut action = Action {
                id: "//java:compile#0".into(),
                program: Some("/bin/sh".into()),
                arguments: vec!["compiler.sh".into()],
                outputs: [
                    "out/result.jar",
                    "out/result.jdeps",
                    "out/native.jar",
                    "out/result.manifest",
                ]
                .map(PathBuf::from)
                .to_vec(),
                execution_info: BTreeSet::from(["local".into()]),
                environment: BTreeMap::from([
                    ("PATH".into(), "/usr/bin:/bin".into()),
                    ("CASE".into(), case.into()),
                    (
                        "TRACE".into(),
                        dir.path().join("trace").display().to_string(),
                    ),
                    (
                        "RELEASE".into(),
                        dir.path().join("release").display().to_string(),
                    ),
                ]),
                ..Default::default()
            };
            for (name, bytes) in [
                ("compiler.sh", COMPILER),
                ("source", "source-v1"),
                ("a.jar", "direct"),
                ("b.jar", "excluded-v1"),
                ("input.jdeps", ""),
            ] {
                fs::write(dir.path().join(name), bytes).unwrap();
                action
                    .inputs
                    .push((dir.path().join(name).into(), name.into()));
            }
            action.extra.mnemonic = "Javac".into();
            action.extra.java_compile = Some(JavaCompileSpec {
                mode: ClasspathMode::Bazel,
                flags: vec!["--fake-compiler".into()],
                classpath: vec!["a.jar".into(), "b.jar".into()],
                direct_jars: vec!["a.jar".into()],
                dependency_jdeps: vec!["input.jdeps".into()],
                mandatory_inputs: vec!["compiler.sh".into(), "source".into()],
                output_jdeps: Some("out/result.jdeps".into()),
            });
            Self { dir, action }
        }
        fn execute(&self) -> Result<ActionResult, String> {
            execute_with_remote(&self.dir.path().join("cache"), &self.action, None)
        }
        fn trace(&self) -> String {
            fs::read_to_string(self.dir.path().join("trace")).unwrap_or_default()
        }
    }

    #[test]
    fn reduced_and_fallback_results_are_terminal_cached_transactions() {
        for (case, count) in [("success", 1), ("fallback", 2), ("second_fallback", 2)] {
            let mut f = Fixture::new(case);
            let first = f.execute().unwrap();
            assert!(!first.cache_hit);
            assert_eq!(first.outputs.len(), 4);
            assert_eq!(f.trace().lines().count(), count);
            assert!(f.execute().unwrap().cache_hit);
            assert_eq!(f.trace().lines().count(), count);
            if count == 2 {
                assert!(
                    fs::read_to_string(&first.outputs[0])
                        .unwrap()
                        .contains("excluded-v1")
                );
                fs::write(f.dir.path().join("b.jar"), "excluded-v2").unwrap();
                let second = f.execute().unwrap();
                assert!(
                    !second.cache_hit,
                    "excluded full-classpath jar must key the fallback result"
                );
                assert!(
                    fs::read_to_string(&second.outputs[0])
                        .unwrap()
                        .contains("excluded-v2")
                );
                assert_eq!(f.trace().lines().count(), 4);
            } else {
                f.action
                    .extra
                    .java_compile
                    .as_mut()
                    .unwrap()
                    .flags
                    .push("--changed-intent".into());
                assert!(!f.execute().unwrap().cache_hit);
                assert_eq!(f.trace().lines().count(), 2);
            }
        }
    }

    #[test]
    fn failed_or_incomplete_attempts_never_publish_or_request_spurious_fallback() {
        for (case, count, message) in [
            ("missing", 1, "result.jdeps"),
            ("corrupt", 1, "invalid Java .jdeps"),
            ("absent_output", 1, "missing or invalid output"),
            ("initial_failure", 1, "exit status: 4"),
            ("final_failure", 2, "final-error"),
        ] {
            let f = Fixture::new(case);
            let error = f.execute().unwrap_err();
            assert!(error.contains(message), "{case}: {error}");
            assert_eq!(f.trace().lines().count(), count, "{case}");
            assert_eq!(
                fs::read_dir(f.dir.path().join("cache/actions"))
                    .unwrap()
                    .count(),
                0
            );
            assert!(f.execute().is_err());
            assert_eq!(f.trace().lines().count(), 2 * count);
        }
        let f = Fixture::new("success");
        fs::write(f.dir.path().join("input.jdeps"), [0]).unwrap();
        assert!(f.execute().unwrap_err().contains("invalid Java .jdeps"));
        assert!(f.trace().is_empty());
        fs::remove_file(f.dir.path().join("input.jdeps")).unwrap();
        assert!(f.execute().is_err());
        assert!(f.trace().is_empty());
    }

    #[test]
    fn cancellation_between_attempts_prevents_fallback_and_publication() {
        let mut f = Fixture::new("cancel");
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        f.action.execution_cancel = Some(flag.clone());
        let result = std::thread::scope(|scope| {
            let dir = f.dir.path();
            scope.spawn(|| {
                for _ in 0..200 {
                    if dir.join("trace").exists() {
                        flag.store(true, Ordering::Release);
                        fs::write(dir.join("release"), "").unwrap();
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                panic!("fake Java attempt did not start");
            });
            f.execute()
        });
        assert!(
            result
                .unwrap_err()
                .contains("cancelled between Java compilation attempts")
        );
        assert_eq!(f.trace().lines().count(), 1);
        assert_eq!(
            fs::read_dir(f.dir.path().join("cache/actions"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn fallback_discards_initial_streams() {
        const CHILD: &str = "BAZEL_RS_TEST_JAVA_PROTOCOL_CHILD";
        if std::env::var_os(CHILD).is_some() {
            Fixture::new("fallback").execute().unwrap();
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "action::java::tests::fallback_discards_initial_streams",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let combined = [out.stdout, out.stderr].concat();
        let text = String::from_utf8_lossy(&combined);
        assert!(text.contains("terminal-stderr"), "{text}");
        assert!(!text.contains("discarded-attempt-"), "{text}");
    }
}
