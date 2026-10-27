//! Fast hashed path sets for planning's hot loops (docs/INCREMENTAL_ANALYSIS.md, "Profile-driven fixes").
//!
//! `HashSet<&Path>` hashes a path component by component (std's `Path`
//! hash walks every byte to skip redundant separators and `.` components)
//! and feeds the pieces to SipHash: the source-input deduplications of
//! every planned action spent ~2 s of the analysis coordinator there on
//! ~/code's `//src/beyondwork.ai/...`. [`PathKey`] keeps `Path` equality
//! exactly (component-wise) but hashes the path's bytes in one call when
//! they already are the normal form `Path::components` describes — which
//! planned paths almost always are — with crate::fast_hash's FxHash.
//! Unlike crate::fast_hash's byte keys it takes any path: an unnormal
//! spelling hashes its normal form. Iteration order of these sets is never
//! observable: they only answer membership.
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The hasher of [`PathSet`]: crate::fast_hash's FxHash (no per-process
/// randomness needed: nothing iterates these sets into output).
pub(crate) type FastState = crate::fast_hash::FxBuildHasher;

/// A set of borrowed paths with `Path` equality and a fast hash.
pub(crate) type PathSet<'a> = HashSet<PathKey<'a>, FastState>;

/// A borrowed path compared like `Path` (component-wise) and hashed from
/// its normal form's bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PathKey<'a>(pub &'a Path);

impl PartialEq for PathKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for PathKey<'_> {}

impl Hash for PathKey<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let bytes = self.0.as_os_str().as_bytes();
        if is_normal(bytes) {
            state.write(bytes);
        } else {
            // The same bytes a normal path equal to this one has.
            let normal: PathBuf = self.0.components().collect();
            state.write(normal.as_os_str().as_bytes());
        }
    }
}

/// Whether `bytes` are exactly the path `Path::components` collects back
/// to: no empty segment but a leading one (an absolute path), no `.`
/// segment, no trailing separator. Two normal paths are equal as `Path`s
/// exactly when their bytes are equal.
pub(crate) fn is_normal(bytes: &[u8]) -> bool {
    let mut segments = bytes.split(|&byte| byte == b'/');
    // A leading empty segment is the root of an absolute path ("/a"); the
    // empty path itself has one empty segment and is normal.
    if segments.next().is_some_and(|first| first == b".") {
        return false;
    }
    segments.all(|segment| !segment.is_empty() && segment != b".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::BuildHasher;

    fn hash(path: &str) -> u64 {
        FastState::default().hash_one(PathKey(Path::new(path)))
    }

    #[test]
    fn equal_paths_hash_alike() {
        let equal = [
            ("a/b", "a//b"),
            ("a/b", "a/./b"),
            ("a/b", "a/b/"),
            ("a/b", "a/b/."),
            ("/a/b", "//a/b"),
            ("/a", "/a/"),
            ("./a", "././a"),
            ("./a", "./a/"),
            ("", ""),
        ];
        for (a, b) in equal {
            assert_eq!(PathKey(Path::new(a)), PathKey(Path::new(b)), "{a} {b}");
            assert_eq!(hash(a), hash(b), "{a} {b}");
        }
        for (a, b) in [("a/b", "a/c"), ("./a", "a"), ("/a", "a"), ("a/../b", "b")] {
            assert_ne!(PathKey(Path::new(a)), PathKey(Path::new(b)), "{a} {b}");
        }
    }

    #[test]
    fn normal_bytes_are_the_collected_components() {
        for path in [
            "a", "a/b", "/a/b", "/", "", "./a", ".", "a/./b", "a//b", "a/", "../a", "a/..",
        ] {
            let collected: PathBuf = Path::new(path).components().collect();
            if is_normal(path.as_bytes()) {
                assert_eq!(collected.as_os_str().as_bytes(), path.as_bytes(), "{path}");
            }
        }
        assert!(is_normal(b"a/b"));
        assert!(is_normal(b"/a/b"));
        assert!(is_normal(b"../a"));
        assert!(!is_normal(b"/"));
        assert!(!is_normal(b"./a"));
        assert!(!is_normal(b"a/"));
    }

    #[test]
    fn a_set_answers_membership() {
        let mut set = PathSet::default();
        assert!(set.insert(PathKey(Path::new("x/y"))));
        assert!(!set.insert(PathKey(Path::new("x//y"))));
        assert!(set.contains(&PathKey(Path::new("x/y/"))));
        assert!(!set.contains(&PathKey(Path::new("x"))));
    }
}
