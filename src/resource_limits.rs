//! Bazel's `UnlimitResources` (src/main/cpp/blaze_util_posix.cc:855-900,
//! called from blaze.cc:1650 early in the client's main): the soft limits
//! of `RLIMIT_NOFILE` and `RLIMIT_NPROC` are raised to their hard limits at
//! startup, so a build's parallel actions (pipes, output files, sockets)
//! are not bound by an interactive shell's small default soft limit (often
//! 1024). Every spawned action inherits the raised limits, as under Bazel.
//!
//! On Linux `GetExplicitSystemLimit` is not implemented (returns -1,
//! blaze_util_linux.cc:231), so the soft limit becomes the hard limit
//! as-is, also when that is `RLIM_INFINITY`. On macOS it is the
//! `kern.maxfilesperproc` / `kern.maxprocperuid` sysctl
//! (blaze_util_darwin.cc:277-305, crate::platform): an unlimited hard limit,
//! or one above it, makes the soft limit that kernel ceiling instead
//! (UnlimitResource with allow_infinity false). A failure is not fatal and,
//! as Bazel's `BAZEL_LOG(WARNING)` goes to the client log rather than the
//! terminal, nothing is printed.

use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// The limits Bazel's client raises (blaze_util_posix.cc:895-900).
const RESOURCES: [Resource; 2] = [Resource::Nofile, Resource::Nproc];

/// The limit to set for `current`, or `None` when the soft limit already
/// equals the hard one (Bazel returns early then, blaze_util_posix.cc:863).
pub fn raised(current: &Rlimit) -> Option<Rlimit> {
    raised_with(current, None)
}

/// [`raised`] with the kernel-level ceiling `explicit`
/// (GetExplicitSystemLimit; None: not implemented, as on Linux):
/// UnlimitResource (blaze_util_posix.cc:855-893) with allow_infinity false.
pub fn raised_with(current: &Rlimit, explicit: Option<u64>) -> Option<Rlimit> {
    if current.current == current.maximum {
        return None;
    }
    let soft = match explicit {
        Some(explicit) if current.maximum.is_none_or(|maximum| maximum > explicit) => {
            Some(explicit)
        }
        _ => current.maximum,
    };
    Some(Rlimit {
        current: soft,
        maximum: current.maximum,
    })
}

/// Raises the soft `RLIMIT_NOFILE` and `RLIMIT_NPROC` limits to their hard
/// limits; returns whether every raise succeeded (or was not needed).
pub fn unlimit_resources() -> bool {
    let mut success = true;
    for resource in RESOURCES {
        let explicit = crate::platform::explicit_system_limit(resource);
        if let Some(new) = raised_with(&getrlimit(resource), explicit) {
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
    /// macOS (blaze_util_darwin.cc's sysctl ceiling): an unlimited hard
    /// limit, or one above the ceiling, gives the ceiling; a hard limit
    /// below it is taken as-is.
    #[test]
    fn explicit_ceiling_caps_the_soft_limit() {
        let unlimited = Rlimit {
            current: Some(256),
            maximum: None,
        };
        let new = raised_with(&unlimited, Some(61_440)).unwrap();
        assert_eq!((new.current, new.maximum), (Some(61_440), None));
        let above = Rlimit {
            current: Some(256),
            maximum: Some(1 << 20),
        };
        let new = raised_with(&above, Some(61_440)).unwrap();
        assert_eq!(new.current, Some(61_440));
        let below = Rlimit {
            current: Some(256),
            maximum: Some(10_240),
        };
        assert_eq!(
            raised_with(&below, Some(61_440)).unwrap().current,
            Some(10_240)
        );
        assert_eq!(raised_with(&unlimited, None).unwrap().current, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unlimit_resources_makes_soft_equal_hard() {
        assert!(unlimit_resources());
        for resource in RESOURCES {
            let limit = getrlimit(resource);
            assert_eq!(limit.current, limit.maximum);
        }
    }

    /// macOS: the soft limits end at the hard limit or the sysctl ceiling.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn unlimit_resources_raises_to_the_ceiling() {
        assert!(unlimit_resources());
        for resource in RESOURCES {
            let limit = getrlimit(resource);
            let ceiling = crate::platform::explicit_system_limit(resource);
            assert!(
                limit.current == limit.maximum || limit.current == ceiling,
                "{resource:?}: {limit:?} (ceiling {ceiling:?})"
            );
        }
    }
}
