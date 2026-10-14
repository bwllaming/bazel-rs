// Explicit proxy and repository-environment noncapture, not an auth API or
// general-purpose secret redaction guarantee. No real credential reads.
#[test]
fn n3_repository_synthetic_credentials_not_in_io_provenance_allowed_env_still_works() {
    use crate::extension_io::tests::{Server, response};
    use std::os::unix::fs::PermissionsExt;
    let private = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp/opencode")
        .unwrap();
    for dir in ["home", "tmp"] {
        std::fs::create_dir(private.path().join(dir)).unwrap();
    }
    let proxy = Server::new(vec![response(502, b"MUST_NOT_CONTACT_PROXY", "")]);
    let proxy_url = proxy.url.replacen(
        "http://",
        "http://synthetic:N3_PROXY_CREDENTIAL_SENTINEL@",
        1,
    );
    let mut environment = vec![("N3_PROVENANCE_CHILD", "explicit-owned-env")];
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        environment.push((name, proxy_url.as_str()));
    }
    crate::extension_io::tests::n3_run_test_child(
        private.path(),
        "repository::tests::n3_synthetic_credential_provenance_child",
        "N3_PROVENANCE_THREE_COMPLETED",
        &environment,
    );
    assert_eq!(proxy.count(), 0);
    assert_eq!(
        std::fs::read_dir(private.path().join("tmp"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
#[ignore = "invoked by parent in env_clear child with only synthetic proxy credentials"]
fn n3_synthetic_credential_provenance_child() {
    assert_eq!(
        std::env::var("N3_PROVENANCE_CHILD").unwrap(),
        "explicit-owned-env"
    );
    use crate::extension_io::tests::{Server, response};
    use crate::repository_environment::{EnvironmentReader, ReadError};
    use sha2::Digest;
    struct ExplicitEnvironment {
        values: BTreeMap<String, String>,
        reads: std::sync::Mutex<Vec<String>>,
    }
    impl EnvironmentReader for ExplicitEnvironment {
        fn read(&self, name: &str) -> Result<Option<std::ffi::OsString>, ReadError> {
            self.reads.lock().unwrap().push(name.to_owned());
            Ok(self
                .values
                .get(name)
                .map(|value| std::ffi::OsString::from(value.as_str())))
        }
    }
    let private = tempfile::tempdir_in("/tmp/opencode").unwrap();
    let f = Fixture(private.path().join("workspace"));
    f.write("BUILD", "");
    f.write(
        "MODULE.bazel",
        "e=use_extension('//:extension.bzl','extension')\nuse_repo(e,'repo')\n",
    );
    f.write("rules.bzl", "def implementation(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['value'])\\n\")\n    ctx.file('value', ctx.getenv('N3_ALLOWED'), executable=False)\nR=repository_rule(implementation, environ=['N3_ALLOWED'])\n");
    let body = b"ordinary downloaded bytes";
    let digest = crate::extension_download::sha256(body);
    let server = Server::new(vec![
        response(200, body, ""),
        response(200, body, ""),
        response(200, body, ""),
    ]);
    f.write("extension.bzl", &format!("load(':rules.bzl', 'R')\ndef implementation(ctx):\n    ctx.download({:?}, 'data', {:?})\n    ctx.read('data', watch='no')\n    R(name='repo')\nextension=module_extension(implementation)\n", server.url, digest));
    let mut receipts = Vec::new();
    let mut roots = Vec::new();
    for (secret, authorized) in [
        ("N3_SYNTHETIC_CRED_A", "N3_ALLOWED_A"),
        ("N3_SYNTHETIC_CRED_B", "N3_ALLOWED_A"),
        ("N3_SYNTHETIC_CRED_B", "N3_ALLOWED_B"),
    ] {
        let reader = Arc::new(ExplicitEnvironment {
            values: BTreeMap::from([
                ("N3_ALLOWED".into(), authorized.into()),
                (
                    "HTTPS_PROXY".into(),
                    format!("http://synthetic:{secret}@127.0.0.1:9"),
                ),
                ("N3_TRANSPORT_CREDENTIAL".into(), secret.into()),
            ]),
            reads: std::sync::Mutex::new(Vec::new()),
        });
        let repos = RepositorySet::new(f.0.clone())
            .with_source_cache(private.path().join("cache"))
            .with_environment_reader(reader.clone());
        let canonical = repos.parse("@repo//:value", "", "").unwrap().repository;
        let root = repos.root(&canonical).unwrap().to_owned();
        // Authorized data must still work; never ban every environment value.
        assert_eq!(
            std::fs::read(root.join("value")).unwrap(),
            authorized.as_bytes()
        );
        assert_eq!(*reader.reads.lock().unwrap(), ["N3_ALLOWED"]);
        let prepared = &repos.ordinary_repository(&canonical).unwrap().0;
        let provenance: serde_json::Value = serde_json::from_slice(&prepared.provenance).unwrap();
        let state = serde_json::json!({"kind":"file", "generation":1, "sha256":digest, "size_bytes":body.len()});
        let observation = &provenance["extension_io"];
        assert_eq!(
            observation,
            &serde_json::json!({"version":1, "observations":[
                {"operation":"download", "output":"data", "reused":false,
                 "result":{"success":true,"sha256":digest,"size_bytes":body.len(),
                    "integrity":format!("sha256-{}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, sha2::Sha256::digest(body)))},
                 "state":state},
                {"operation":"read", "path":"data", "watch":"no", "state":state}
            ]})
        );
        let receipt = serde_json::to_string(observation).unwrap();
        assert!(!receipt.contains(secret));
        assert!(!receipt.contains("N3_PROXY_CREDENTIAL_SENTINEL"));
        assert!(!receipt.contains(&server.url));
        assert!(!receipt.contains("bazel-extension-io-"));
        receipts.push(receipt);
        roots.push(root);
    }
    assert_eq!(server.count(), 3);
    assert_eq!(receipts[0], receipts[1]);
    assert_eq!(receipts[1], receipts[2]);
    assert_eq!(roots[0], roots[1]);
    assert_ne!(roots[1], roots[2]);
    println!("\nN3_PROVENANCE_THREE_COMPLETED");
}
