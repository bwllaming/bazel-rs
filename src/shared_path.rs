//! An immutable path shared by its copies (MEM4, docs/MEMORY.md).
//!
//! The action planner holds one path in many places: an artifact's source
//! and execution paths are listed again in every action that reads the
//! artifact (its inputs and generated dependencies), in every runfiles
//! tree that contains it and in the planned request. A [`SharedPath`] copy
//! is one reference count instead of a new allocation of the path's bytes.
//! It compares, orders, hashes and prints exactly like the [`Path`] it
//! holds, and borrows as one, so maps keyed by it are looked up with a
//! `&Path`.

use std::borrow::Borrow;
use std::ffi::OsStr;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SharedPath(Arc<Path>);

impl SharedPath {
    /// The path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// An owned copy of the path.
    pub fn to_path_buf(&self) -> PathBuf {
        self.0.to_path_buf()
    }
}

impl Default for SharedPath {
    fn default() -> Self {
        Self::from(Path::new(""))
    }
}

impl Deref for SharedPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for SharedPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<OsStr> for SharedPath {
    fn as_ref(&self) -> &OsStr {
        self.0.as_os_str()
    }
}

impl Borrow<Path> for SharedPath {
    fn borrow(&self) -> &Path {
        &self.0
    }
}

impl fmt::Debug for SharedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl From<PathBuf> for SharedPath {
    fn from(path: PathBuf) -> Self {
        Self(Arc::from(path))
    }
}

impl From<&Path> for SharedPath {
    fn from(path: &Path) -> Self {
        Self(Arc::from(path))
    }
}

impl From<&PathBuf> for SharedPath {
    fn from(path: &PathBuf) -> Self {
        Self::from(path.as_path())
    }
}

impl From<String> for SharedPath {
    fn from(path: String) -> Self {
        Self::from(PathBuf::from(path))
    }
}

impl From<&str> for SharedPath {
    fn from(path: &str) -> Self {
        Self::from(Path::new(path))
    }
}

impl From<SharedPath> for PathBuf {
    fn from(path: SharedPath) -> Self {
        path.to_path_buf()
    }
}

impl PartialEq<Path> for SharedPath {
    fn eq(&self, other: &Path) -> bool {
        *self.0 == *other
    }
}

impl PartialEq<&Path> for SharedPath {
    fn eq(&self, other: &&Path) -> bool {
        *self.0 == **other
    }
}

impl PartialEq<PathBuf> for SharedPath {
    fn eq(&self, other: &PathBuf) -> bool {
        *self.0 == *other.as_path()
    }
}

impl PartialEq<SharedPath> for PathBuf {
    fn eq(&self, other: &SharedPath) -> bool {
        *self.as_path() == *other.0
    }
}

impl PartialEq<SharedPath> for Path {
    fn eq(&self, other: &SharedPath) -> bool {
        *self == *other.0
    }
}

impl PartialEq<SharedPath> for &Path {
    fn eq(&self, other: &SharedPath) -> bool {
        **self == *other.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashMap};

    /// Orders, hashes and looks up exactly like the path it holds.
    #[test]
    fn behaves_like_its_path() {
        let paths = ["b/c", "a", "a/b", "a-b", "a/b/c"];
        let shared: BTreeSet<SharedPath> = paths.iter().map(|p| SharedPath::from(*p)).collect();
        let owned: BTreeSet<PathBuf> = paths.iter().map(PathBuf::from).collect();
        assert!(
            shared
                .iter()
                .map(SharedPath::as_path)
                .eq(owned.iter().map(PathBuf::as_path))
        );
        let map: HashMap<SharedPath, usize> = [(SharedPath::from("x/y"), 1)].into();
        assert_eq!(map.get(Path::new("x/y")), Some(&1));
        let copy = SharedPath::from("x/y");
        assert_eq!(format!("{copy:?}"), format!("{:?}", Path::new("x/y")));
        assert_eq!(copy, PathBuf::from("x/y"));
        assert_eq!(SharedPath::default(), PathBuf::new());
    }
}
