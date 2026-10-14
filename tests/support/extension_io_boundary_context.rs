fn n3_eval_in_context(context: &Context, source: &str) -> Result<(), String> {
    Module::with_temp_heap(|module| {
        module.set(
            "ctx",
            module.heap().alloc(ModuleCtxGen {
                modules: Value::new_none(),
                os: Value::new_none(),
                root_has_non_dev_dependency: false,
                execution_identity: context.io.borrow().identity(),
            }),
        );
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(context);
        eval.eval_module(
            starlark::syntax::AstModule::parse(
                "n3-boundary.star",
                source.to_owned(),
                &starlark::syntax::Dialect::Standard,
            )
            .unwrap(),
            &crate::package::bzl_globals(),
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    })
}

#[test]
fn n3_actual_context_preflight_allocates_no_scratch_or_client() {
    use crate::extension_io::tests::{Server, n3_assert_pristine, response};
    for index in 0..10 {
        let fixture = Fixture::new();
        let server = Server::new(vec![response(200, b"unused", "")]);
        let path = fixture.root.path().join("outside");
        let url = format!("file://{}", fixture.root.path().join("owned").display());
        let cases = n3_preflight_cases(&server.url, path.to_str().unwrap(), &url);
        assert_eq!(cases.len(), 10);
        let (name, call, diagnostic) = &cases[index];
        let context = Context {
            repositories: fixture.repositories.clone(),
            public_label: label(),
            calls: CallRecorder::default(),
            io: RefCell::new(crate::extension_io::ScratchExecutionIo::new().unwrap()),
        };
        let error = n3_eval_in_context(&context, call).unwrap_err();
        assert!(error.contains(*diagnostic), "{name}: {error}");
        n3_assert_pristine(&context.io.borrow());
        assert!(context.io.borrow_mut().close().unwrap().is_empty());
        assert_eq!(server.count(), 0);
        assert!(!fixture.root.path().join("cache").exists());
    }
}

#[test]
fn n3_actual_context_checksumless_untrusted_tls_allow_fail_boundary() {
    use crate::extension_io::tests::{
        N3BoundedTlsServer, n3_assert_no_destination, n3_close_and_assert_removed, n3_private_io,
    };
    use crate::extension_io::{FileState, Observation};
    let server = N3BoundedTlsServer::new();
    for allow in [false, true] {
        let fixture = Fixture::new();
        // Explicit EMPTY trust store, not host trust or TLS verification bypass.
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .tls_built_in_root_certs(false)
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let context = Context {
            repositories: fixture.repositories.clone(),
            public_label: label(),
            calls: CallRecorder::default(),
            io: RefCell::new(n3_private_io(fixture.root.path()).with_client(client)),
        };
        let source = format!(
            "r = ctx.download({:?}, 'fresh', allow_fail={})\n{}",
            server.url,
            if allow { "True" } else { "False" },
            if allow {
                "dir(r) == ['success'] and r.success == False or fail(r)"
            } else {
                "fail('N3_UNEXPECTED_TLS_SUCCESS')"
            }
        );
        let result = n3_eval_in_context(&context, &source);
        if allow {
            result.unwrap();
        } else {
            let error = result.unwrap_err();
            assert!(
                error.contains("module I/O completion: HTTP connection failed"),
                "{error}"
            );
        }
        assert_eq!(server.n3_http_count(), 0); // TLS handshakes are not HTTP requests.
        n3_assert_no_destination(&context.io.borrow(), "fresh");
        let observations = n3_close_and_assert_removed(&mut context.io.borrow_mut());
        if allow {
            assert_eq!(observations.len(), 1);
            assert!(
                matches!(&observations[0], Observation::Download { state: FileState::Missing, result, .. } if !result.success)
            );
        } else {
            assert!(observations.is_empty());
        }
        assert!(!fixture.root.path().join("cache").exists());
    }
    server.close();
}
