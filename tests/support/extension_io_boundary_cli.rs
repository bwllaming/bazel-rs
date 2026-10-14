fn n3_absent_or_empty(path: &std::path::Path) {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Ok(m) => {
            assert!(
                m.is_dir() && !m.file_type().is_symlink(),
                "{}",
                path.display()
            );
            assert_eq!(fs::read_dir(path).unwrap().count(), 0, "{}", path.display());
        }
        Err(e) => panic!("{}: {e}", path.display()),
    }
}

#[test]
fn n3_cli_ten_allow_fail_preflights_publish_nothing() {
    for index in 0..10 {
        let server = Server::new(BODY);
        let fixture = Fixture::new(&server.url, &sha(BODY));
        // ../outside from a scratch work root remains inside private TMPDIR.
        // Absolute output and file URL also point only into this owned fixture.
        fixture.file("owned", b"N3_OWNED_INPUT_UNCHANGED");
        let absolute = fixture.0.path().join("outside");
        let file_url = format!("file://{}", fixture.0.path().join("owned").display());
        let cases = n3_preflight_cases(&server.url, absolute.to_str().unwrap(), &file_url);
        assert_eq!(cases.len(), 10);
        let (name, call, diagnostic) = &cases[index];
        fixture.extension(&format!(
            "maker(name='made', text='MUST_NOT_PUBLISH')\n{call}\nfail('N3_AFTER_PREFLIGHT')"
        ));
        let result = fixture.run(&["build", "//:copy"]);
        assert_eq!(result.status.code(), Some(1), "{name}: {result:?}");
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains(*diagnostic), "{name}: {stderr}");
        assert!(stderr.contains("ext.bzl"), "{name}: {stderr}");
        assert!(
            !stderr.contains("fail: N3_AFTER_PREFLIGHT"),
            "{name}: {stderr}"
        );
        assert!(
            result.stdout.is_empty(),
            "{name}: artifact output on preflight failure"
        );
        assert_eq!(server.count(), 0, "{name}");
        // Physical directories, including hidden .stage/.trash entries, not
        // Fixture::roots() (which deliberately filters dot entries).
        for directory in [
            "cache/repositories/direct/trees",
            "cache/repositories/direct/manifests",
            "cache/actions",
            "cache/work",
            "tmp",
            "home",
            "xdg",
        ] {
            n3_absent_or_empty(&fixture.0.path().join(directory));
        }
        for output in ["outside", "data", "versions.json", "copy.txt"] {
            assert!(
                matches!(fs::symlink_metadata(fixture.0.path().join(output)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                "{name}: {output}"
            );
        }
        assert_eq!(
            fs::read(fixture.0.path().join("owned")).unwrap(),
            b"N3_OWNED_INPUT_UNCHANGED"
        );
    }
}
