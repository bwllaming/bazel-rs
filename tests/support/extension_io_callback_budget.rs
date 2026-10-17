// Requires v2 01-boundary-tests.patch. Separate opt-in after lifecycle/gate review.
// All hooks above remain enclosed in existing cfg(test) modules.
//
// N32: the former callback budget (120 s deadline, 64 requests, 32 calls,
// 8 MiB/file, 32 MiB cumulative) is gone; Bazel has no such caps. The actual
// callback context now streams a download above every former cap and runs
// more operations than the former call budget, while allow_fail still never
// tolerates a hard (non-completion) error.
#[test]
fn n3_actual_context_has_no_callback_budget_and_allow_fail_stays_typed() {
    use crate::extension_io::tests::{
        Server, n3_close_and_assert_removed, n3_private_io, response,
    };
    let fixture = Fixture::new();
    let big: Vec<u8> = (0..33 * 1024 * 1024).map(|i| (i % 241) as u8).collect();
    let server = Server::new(vec![response(200, &big, ""), response(200, b"small", "")]);
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .tls_built_in_root_certs(false)
        .build()
        .unwrap();
    let io = n3_private_io(fixture.root.path()).with_client(client);
    let context = Context {
        repositories: fixture.repositories.clone(),
        public_label: label(),
        calls: CallRecorder::default(),
        io: RefCell::new(io),
        environment: RefCell::default(),
    };
    n3_eval_in_context(
        &context,
        &format!(
            "r = ctx.download({url:?}, 'big', {big:?})\n\
             r.size_bytes == {size} or fail('size')\n\
             s = ctx.download({url:?} + '/small', 'small', {small:?})\n\
             [ctx.read('small') == 'small' or fail('read') for _ in range(40)]\n",
            url = server.url,
            big = crate::extension_download::sha256(&big),
            size = big.len(),
            small = crate::extension_download::sha256(b"small"),
        ),
    )
    .unwrap();
    assert_eq!(server.count(), 2);
    // A hard policy error (unsupported URL scheme) is never allow_fail-tolerated.
    let error = n3_eval_in_context(
        &context,
        "r = ctx.download('ftp://example.invalid/x', 'x', allow_fail=True)\nfail('N3_TOLERATED')",
    )
    .unwrap_err();
    assert!(error.contains("unsupported module I/O profile"), "{error}");
    assert!(!error.contains("N3_TOLERATED"), "{error}");
    assert!(!n3_close_and_assert_removed(&mut context.io.borrow_mut()).is_empty());
}
