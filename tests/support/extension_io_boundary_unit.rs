// Included inside extension_io::tests; private access is test-only.
pub(crate) fn n3_private_io(parent: &Path) -> ScratchExecutionIo {
    let mut io = ScratchExecutionIo::new().unwrap();
    io.container = Some(
        tempfile::Builder::new()
            .prefix("n3-owned-")
            .tempdir_in(parent)
            .unwrap(),
    );
    io
}

pub(crate) fn n3_assert_pristine(io: &ScratchExecutionIo) {
    assert!(io.container.is_none());
    assert!(io.client.is_none());
    assert!(io.files.is_empty());
    assert!(io.directories.is_empty());
    assert!(io.observations.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn n3_production_root_creation_is_private_lazy_and_expires() {
    use std::os::unix::fs::MetadataExt;

    // Read-only observation of this actual caller; never change global umask or
    // route this test through the child helper that forces0022.
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let caller_umask = u32::from_str_radix(
        status
            .lines()
            .find_map(|line| line.strip_prefix("Umask:"))
            .unwrap()
            .trim(),
        8,
    )
    .unwrap();
    let mut io = ScratchExecutionIo::new().unwrap();
    let identity = io.identity();
    n3_assert_pristine(&io);

    let work = io.root().unwrap();
    let container = io.container.as_ref().unwrap().path().to_owned();
    let metadata = fs::symlink_metadata(&container).unwrap();
    println!(
        "N3_RUNTIME_SCRATCH_ROOT path={} mode={:04o} dev={} ino={} caller_umask={:04o}",
        container.display(),
        metadata.mode() & 0o7777,
        metadata.dev(),
        metadata.ino(),
        caller_umask
    );
    assert!(metadata.file_type().is_dir());
    assert!(!metadata.file_type().is_symlink());
    assert_eq!(metadata.mode() & 0o7777, 0o700);
    assert_eq!(work, container.join("work"));
    assert!(
        matches!(fs::symlink_metadata(&work), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    );
    assert_eq!(io.root().unwrap(), work);
    assert_eq!(io.container.as_ref().unwrap().path(), container.as_path());
    let repeated = fs::symlink_metadata(&container).unwrap();
    assert_eq!(
        (repeated.dev(), repeated.ino()),
        (metadata.dev(), metadata.ino())
    );
    assert!(io.client.is_none());
    assert!(io.files.is_empty());
    assert!(io.directories.is_empty());

    assert!(io.close().unwrap().is_empty());
    assert!(
        matches!(fs::symlink_metadata(&container), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    );
    assert_eq!(io.read(identity, "", "auto"), Err(TypedIoError::Lifetime));
    assert!(io.container.is_none());
    assert!(io.client.is_none());
}

pub(crate) fn n3_assert_no_destination(io: &ScratchExecutionIo, output: &str) {
    assert!(io.files.is_empty());
    let path = io
        .container
        .as_ref()
        .unwrap()
        .path()
        .join("work")
        .join(output);
    assert!(
        matches!(fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    );
}

pub(crate) fn n3_close_and_assert_removed(io: &mut ScratchExecutionIo) -> Vec<Observation> {
    let container = io.container.as_ref().map(|c| c.path().to_owned());
    let observations = io.close().unwrap();
    if let Some(path) = container {
        assert!(
            matches!(fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        );
    }
    assert!(io.client.is_none());
    assert!(io.check_receiver(io.identity()).is_err());
    observations
}

#[cfg(unix)]
#[test]
fn n3_owned_0640_reuse_enabled_and_disabled_without_client() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // Approved fresh reference: enabled single HTTP URL; older SC: disabled HTTPS.
    let payload = b"owned old destination\n";
    let digest = "8d0650ffea8a9fa41136886f15c73af6401a4821aa28abd7d1db464ab9ee7f90";
    assert_eq!(transport::sha256(payload), digest);
    let server = N3BoundedTlsServer::new();
    let http = Server::new(vec![response(
        200,
        payload,
        "Content-Type: application/octet-stream\r\n",
    )]);
    for enabled in [true, false] {
        let private = tempfile::tempdir_in("/tmp/opencode").unwrap();
        let mut io = n3_private_io(private.path());
        seed(&mut io, "same", payload);
        let path = io.destination("same").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let before = fs::metadata(&path).unwrap();
        assert_eq!(before.permissions().mode() & 0o7777, 0o640);
        let url = if enabled {
            format!("{}/05-default-enabled-seeded", http.url)
        } else {
            format!("{}/disabled-match", server.url)
        };
        assert!(url.starts_with(if enabled { "http://" } else { "https://" }));
        let result = io
            .download(
                io.identity(),
                enabled,
                Download {
                    urls: &[&url],
                    output: "same",
                    sha256: digest,
                    integrity: "",
                    allow_fail: true,
                },
            )
            .unwrap();
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({
                "success": true, "sha256": digest, "size_bytes": 22,
                "integrity": "sha256-jQZQ/+qKn6QRNohvFcc69kAaSCGqKKvX0dtGSrnuf5A="
            })
        );
        assert!(
            io.client.is_none(),
            "enabled={enabled}: reuse allocated a client"
        );
        assert_eq!(http.count(), 0);
        assert_eq!(server.n3_http_count(), 0);
        assert_eq!(fs::read(&path).unwrap(), payload);
        assert_eq!(
            io.read(io.identity(), "same", "no").unwrap().as_bytes(),
            payload
        );
        let after = fs::metadata(&path).unwrap();
        assert_eq!(after.permissions().mode() & 0o7777, 0o640);
        // Stronger native assertions, not independently observed reference facts.
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(io.files["same"].generation, 1);
        let observations = n3_close_and_assert_removed(&mut io);
        assert!(matches!(
            &observations[0],
            Observation::Download { reused: true, .. }
        ));
        assert!(matches!(&observations[1], Observation::Read { .. }));
    }
    server.close();
}

// Approved fresh reference plan 8681ff12... binds these exact portable bytes,
// digests and HTTP fixtures. Native pairing remains UNEXECUTED, not PASS.
#[cfg(unix)]
#[test]
fn n3_fresh_destination_source_order() {
    use std::os::unix::fs::PermissionsExt;
    let private = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp/opencode")
        .unwrap();
    for dir in ["home", "tmp"] {
        fs::create_dir(private.path().join(dir)).unwrap();
    }
    n3_run_test_child(
        private.path(),
        "extension_io::tests::n3_fresh_destination_source_order_child",
        "N3_FRESH_FOUR_COMPLETED",
        &[("N3_FRESH_CHILD", "owned-022")],
    );
    assert_eq!(fs::read_dir(private.path().join("tmp")).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
#[ignore = "invoked once by n3_fresh_destination_source_order with child-only umask 022"]
fn n3_fresh_destination_source_order_child() {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::env::var("N3_FRESH_CHILD").unwrap(), "owned-022");
    const MISMATCH: &str = "0e1182ceb946a91400d5272ede85ac18985ead78fb5922380ba7e63a8a7f893c";
    const FULL_40000: &str = "8f272ca6d96caedf3d860ff34ed21868f04ce18a2f41686f513c3c989146ca79";
    for (length, route, body_sha) in [
        (
            32767,
            "/01-fresh32767",
            "12ac503ad686bf1dbc1c7b3e0d83c02a8eae02da9519d0ea51b6aa4198d08ddf",
        ),
        (
            32768,
            "/02-fresh32768",
            "09fed9cbfb98b6ab0f3e8ff63b7b1f9b0e07d58b225295c78fdc023cc4985a72",
        ),
        (
            32769,
            "/03-fresh32769",
            "346e2310c7233f7723048e8289ba7a299d635ded9723dfea5248773f5b7684b8",
        ),
        (
            33000,
            "/04-terminal-truncation",
            "2c40f737972f90afe037b64ca1f1224d320ee12053a6d4be4844143b0b7064d5",
        ),
    ] {
        let private = tempfile::tempdir_in("/tmp/opencode").unwrap();
        let body: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
        assert_eq!(transport::sha256(&body), body_sha);
        let (declared, checksum) = if length == 33000 {
            let complete: Vec<u8> = (0..40000).map(|i| (i % 251) as u8).collect();
            assert_eq!(transport::sha256(&complete), FULL_40000);
            (40000, FULL_40000)
        } else {
            (length, MISMATCH)
        };
        let mut reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n").into_bytes();
        reply.extend_from_slice(&body);
        // One response then close, never a recovery/redirect/encoding/Range response.
        let server = Server::new(vec![reply]);
        let url = format!("{}{route}", server.url);
        let mut io = n3_private_io(private.path());
        // Same-parent create control: umask alone is NOT an ACL guarantee.
        // The output remains fresh; this control is removed before download.
        io.parents("fresh").unwrap();
        let control = io
            .destination("fresh")
            .unwrap()
            .with_file_name("mode-control");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&control)
            .unwrap();
        let control_mode = file.metadata().unwrap().permissions().mode() & 0o7777;
        drop(file);
        fs::remove_file(&control).unwrap();
        assert_eq!(
            control_mode, 0o644,
            "unsupported filesystem/ACL mode-control setup"
        );
        println!("N3_MODE_CONTROL length={length} create_mode=0644 child_umask=022");
        n3_assert_no_destination(&io, "fresh");
        assert!(!download(&mut io, &url, "fresh", checksum, true).unwrap());
        assert_eq!(server.count(), 1, "{length}: unexpected retry");
        assert!(
            server.requests.lock().unwrap()[0].starts_with(&format!("GET {route} HTTP/1.1\r\n"))
        );
        let path = io.destination("fresh").unwrap();
        if length < PRECHECK {
            n3_assert_no_destination(&io, "fresh");
            assert!(matches!(
                &io.observations[0],
                Observation::Download {
                    state: FileState::Missing,
                    ..
                }
            ));
        } else {
            assert_eq!(fs::read(&path).unwrap(), body);
            assert_eq!(transport::sha256(&fs::read(&path).unwrap()), body_sha);
            assert_eq!(io.bytes("fresh").unwrap(), body);
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
                0o644
            );
            assert_eq!(io.files["fresh"].generation, 1);
            let expected_text: String = body.iter().map(|b| char::from(*b)).collect();
            assert_eq!(
                io.read(io.identity(), "fresh", "no").unwrap(),
                expected_text
            );
        }
        match &io.observations[0] {
            Observation::Download { result, reused, .. } => {
                assert!(!reused);
                assert_eq!(
                    serde_json::to_value(result).unwrap(),
                    serde_json::json!({"success": false})
                );
            }
            other => panic!("{other:?}"),
        }
        n3_close_and_assert_removed(&mut io);
    }
    println!("\nN3_FRESH_FOUR_COMPLETED");
}
