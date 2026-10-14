// Requires v2 01-boundary-tests.patch. Separate opt-in after lifecycle/gate review.
// All hooks above remain enclosed in existing cfg(test) modules.
#[test]
fn n3_actual_context_allow_fail_cannot_tolerate_deadline_or_request_cap() {
    use crate::extension_io::tests::{
        Server, n3_assert_no_destination, n3_close_and_assert_removed,
        n3_exhaust_budget_for_callback, n3_private_io, response,
    };
    for deadline in [true, false] {
        let fixture = Fixture::new();
        let server = Server::new(vec![response(200, b"unused", "")]);
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .tls_built_in_root_certs(false)
            .build()
            .unwrap();
        let io = n3_private_io(fixture.root.path()).with_client(client);
        n3_exhaust_budget_for_callback(&io, deadline);
        let context = Context {
            repositories: fixture.repositories.clone(),
            public_label: label(),
            calls: CallRecorder::default(),
            io: RefCell::new(io),
        };
        let error = n3_eval_in_context(&context, &format!(
            "r = ctx.download({:?}, 'fresh', {:?}, allow_fail=True)\nfail('N3_BUDGET_TOLERATED')",
            server.url, "00".repeat(32),
        )).unwrap_err();
        let expected = if deadline {
            "module I/O resource limit: 120s I/O deadline"
        } else {
            "module I/O resource limit: 64 HTTP requests"
        };
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("fail: N3_BUDGET_TOLERATED"), "{error}");
        assert_eq!(server.count(), 0);
        n3_assert_no_destination(&context.io.borrow(), "fresh");
        assert!(n3_close_and_assert_removed(&mut context.io.borrow_mut()).is_empty());
        assert!(!fixture.root.path().join("cache").exists());
    }
}
