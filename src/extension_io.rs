//! Test-only HTTP(S) fixtures shared by the download transport,
//! repository_ctx and module_ctx tests. module_ctx I/O itself is the shared
//! StarlarkBaseExternalContext implementation in `repository_ctx`.

#[cfg(test)]
pub(crate) mod tests {
    include!("../tests/support/http_fixtures.rs");
}
