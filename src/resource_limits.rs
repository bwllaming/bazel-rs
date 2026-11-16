//! Bazel's `UnlimitResources` (src/main/cpp/blaze_util_posix.cc:855-900,
//! called from blaze.cc:1650 early in the client's main): the soft limits
//! of `RLIMIT_NOFILE` and `RLIMIT_NPROC` are raised to their hard limits at
//! startup, so a build's parallel actions (pipes, output files, sockets)
//! are not bound by an interactive shell's small default soft limit (often
//! 1024). Every spawned action inherits the raised limits, as under Bazel.
//!
//! On Linux `GetExplicitSystemLimit` is not implemented (returns -1,
//! blaze_util_linux.cc:231), so the soft limit becomes the hard limit
//! as-is, also when that is `RLIM_INFINITY`. A failure is not fatal and,
//! as Bazel's `BAZEL_LOG(WARNING)` goes to the client log rather than the
//! terminal, nothing is printed.

use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// The limits Bazel's client raises (blaze_util_posix.cc:895-900).
const RESOURCES: [Resource; 2] = [Resource::Nofile, Resource::Nproc];

/// The limit to set for `current`, or `None` when the soft limit already
/// equals the hard one (Bazel returns early then, blaze_util_posix.cc:863).
pub fn raised(current: &Rlimit) -> Option<Rlimit> {
    if current.current == current.maximum {
        return None;
    }
    Some(Rlimit {
        current: current.maximum,
        maximum: current.maximum,
    })
}

/// Raises the soft `RLIMIT_NOFILE` and `RLIMIT_NPROC` limits to their hard
/// limits; returns whether every raise succeeded (or was not needed).
pub fn unlimit_resources() -> bool {
    let mut success = true;
    for resource in RESOURCES {
        if let Some(new) = raised(&getrlimit(resource)) {
            success &= setrlimit(resource, new).is_ok();
        }
    }
    success
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_limit_is_raised_to_hard() {
        let low = Rlimit {
            current: Some(1024),
            maximum: Some(524_288),
        };
        let new = raised(&low).unwrap();
        assert_eq!(new.current, Some(524_288));
        assert_eq!(new.maximum, Some(524_288));
    }

    #[test]
    fn unlimited_hard_limit_is_taken_as_is() {
        let low = Rlimit {
            current: Some(4096),
            maximum: None,
        };
        let new = raised(&low).unwrap();
        assert_eq!(new.current, None);
        assert_eq!(new.maximum, None);
    }

    #[test]
    fn equal_limits_are_left_alone() {
        let same = Rlimit {
            current: Some(1024),
            maximum: Some(1024),
        };
        assert!(raised(&same).is_none());
    }

    /// In this process the raise leaves soft == hard (idempotent; the
    /// test binary's own limits are only ever raised, never lowered).
    #[test]
    fn unlimit_resources_makes_soft_equal_hard() {
        assert!(unlimit_resources());
        for resource in RESOURCES {
            let limit = getrlimit(resource);
            assert_eq!(limit.current, limit.maximum);
        }
    }
}
