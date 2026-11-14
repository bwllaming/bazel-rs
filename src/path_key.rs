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

/// A path's last component's bytes (`Path::components().next_back()`):
/// the bytes after the last separator when they are one (neither empty nor
/// `.`), else what the components give (`/` for the root, empty for the
/// empty path). Equal paths have equal last components.
fn last_component(path: &Path) -> &[u8] {
    let bytes = path.as_os_str().as_bytes();
    let start = bytes
        .iter()
        .rposition(|&byte| byte == b'/')
        .map_or(0, |separator| separator + 1);
    let last = &bytes[start..];
    if !last.is_empty() && last != b"." {
        return last;
    }
    path.components()
        .next_back()
        .map_or(&[][..], |component| component.as_os_str().as_bytes())
}

/// A prefilter of a set of paths: [`PathFilter::may_contain`] holds for
/// every path equal (as a `Path`) to one of them, and for few others. It
/// looks at a path's last component only (a bit per hashed last
/// component), so a planning loop testing every input path of every action
/// against a small set (missing sources, source directories) skips the
/// full path's normal-form check and hash for nearly all of them.
pub(crate) struct PathFilter {
    bits: Vec<u64>,
    mask: u64,
}

impl PathFilter {
    pub(crate) fn new<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Self {
        let paths: Vec<&Path> = paths.into_iter().collect();
        // ~16 bits per path (at least 4096): few false positives.
        let size = (paths.len() * 16).max(4096).next_power_of_two();
        let mut filter = Self {
            bits: vec![0; size / 64],
            mask: size as u64 - 1,
        };
        for path in paths {
            let bit = filter.bit(path);
            filter.bits[(bit / 64) as usize] |= 1 << (bit % 64);
        }
        filter
    }

    fn bit(&self, path: &Path) -> u64 {
        use std::hash::BuildHasher;
        FastState::default().hash_one(last_component(path)) & self.mask
    }

    /// False only when no path of the set equals `path`.
    pub(crate) fn may_contain(&self, path: &Path) -> bool {
        let bit = self.bit(path);
        self.bits[(bit / 64) as usize] & (1 << (bit % 64)) != 0
    }
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

    #[test]
    fn a_filter_never_misses_an_equal_path() {
        let members = ["a/b", "/x/y.txt", "c/..", "", ".", "/", "./d", "e/f/"];
        let filter = PathFilter::new(members.iter().map(Path::new));
        let equal = [
            "a/b",
            "a//b",
            "a/./b",
            "a/b/",
            "a/b/.",
            "/x/y.txt",
            "//x/y.txt",
            "c/..",
            "c/../",
            "",
            ".",
            "./",
            "/",
            "//",
            "./d",
            "././d",
            "e/f",
            "e/f/.",
        ];
        for path in equal {
            assert!(filter.may_contain(Path::new(path)), "{path}");
        }
        for (path, last) in [
            ("a/b/", "b"),
            ("a/.", "a"),
            ("/", "/"),
            ("", ""),
            (".", "."),
            ("./", "."),
            ("x/..", ".."),
        ] {
            let expected = Path::new(path)
                .components()
                .next_back()
                .map_or(String::new(), |c| {
                    c.as_os_str().to_string_lossy().into_owned()
                });
            assert_eq!(expected, last, "{path}");
            assert_eq!(last_component(Path::new(path)), last.as_bytes(), "{path}");
        }
        let filter = PathFilter::new([Path::new("only/this")]);
        assert!(
            !filter.may_contain(Path::new("only/that")) || !filter.may_contain(Path::new("x/y"))
        );
    }
}
