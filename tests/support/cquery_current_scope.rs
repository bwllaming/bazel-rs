/// These cases need configured-analysis locals or phase-owned diagnostic replay,
/// beyond the current-loading prefix. Preserve semantic/output/action assertions
/// and require this exact conservative boundary rather than any generic failure.
fn assert_current_loading_scope_miss(output: &std::process::Output, reason: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains(&format!("cquery-snapshot: miss: {reason}")),
        "{stderr}"
    );
    assert!(!stderr.contains("cquery-snapshot: hit"), "{stderr}");
}
