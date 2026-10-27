// Shared by the library's cfg(test) modules and integration-test harnesses.
// Resolve only the trusted fixture parent, before creating any test data.
// Never canonicalize paths below it: tests deliberately create symlinks,
// dangling links and denied paths which production must continue to reject.
#[allow(dead_code)]
mod test_temp {
    pub fn root() -> std::path::PathBuf {
        std::env::temp_dir()
            .canonicalize()
            .expect("the test temporary parent must exist and be accessible")
    }
}
