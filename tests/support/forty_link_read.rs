// Shared by unit tests (`include!("../tests/support/forty_link_read.rs")`).
//
// Reads `path` through a chain of exactly 40 symlinks (Linux MAXSYMLINKS).
// The kernel does not always grant the 40th link: do_filp_open first walks
// in RCU mode and, if that lockless walk is aborted (-ECHILD, e.g. a
// concurrent seqcount change on a busy host), repeats the walk in ref mode
// with the SAME nameidata, whose total_link_count already includes the links
// counted by the aborted attempt (fs/namei.c v6.8: set_nameidata once at
// :3828, retry at :3829-3831, count checked in reserve_stack :1732). The
// retry then refuses a 40-link chain with ELOOP although 40 links are allowed.
// Measured with open(2) in a loop on this host: 40 links fail ~1/1000 to
// 1/4000 (tmpfs and overlayfs alike), 39 links 0/800000, 41 links always.
// So a refusal of exactly 40 links is a transient kernel artefact: retry it,
// boundedly. A chain the kernel really cannot resolve (41 links, a cycle)
// fails deterministically and is still reported by every attempt.
fn read_through_forty_links(path: &std::path::Path) -> Vec<u8> {
    const ELOOP: i32 = 40;
    let mut last = None;
    for _ in 0..64 {
        match std::fs::read(path) {
            Ok(bytes) => return bytes,
            Err(error) if error.raw_os_error() == Some(ELOOP) => last = Some(error),
            Err(error) => panic!("cannot read {}: {error}", path.display()),
        }
    }
    panic!(
        "{} stayed unresolvable (ELOOP) for 64 attempts: {:?}",
        path.display(),
        last
    );
}
