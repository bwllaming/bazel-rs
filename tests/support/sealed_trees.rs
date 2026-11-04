// Published repository trees are sealed: no file or directory of a tree
// has a write bit (docs/REPOSITORY_SLOTS.md, "Sealed trees"). A test that
// removes a cache or tampers with a tree uses these instead of
// `fs::remove_dir_all` / a plain write.
#[allow(dead_code)]
mod sealed_trees {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;

    /// `fs::remove_dir_all`, giving every directory its owner's permissions
    /// first (never following links). A missing path is no error.
    pub fn remove_tree(path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        let metadata = match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            other => other?,
        };
        if !metadata.is_dir() {
            return fs::remove_file(path);
        }
        let mode = metadata.permissions().mode() & 0o7777;
        if mode & 0o700 != 0o700 {
            fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))?;
        }
        for entry in fs::read_dir(path)? {
            remove_tree(entry?.path())?;
        }
        fs::remove_dir(path)
    }

    /// Run `tamper`, which writes `path` (a file or link to create, rewrite
    /// or remove) in a sealed tree, with the owner's write permission on
    /// `path` (a file or directory) and its parent meanwhile; their sealed
    /// modes are restored afterwards, so the tamper is all that differs.
    pub fn tampering<T>(path: &Path, tamper: impl FnOnce() -> T) -> T {
        let open = |path: &Path| {
            let metadata = fs::symlink_metadata(path).ok()?;
            if metadata.file_type().is_symlink() {
                return None;
            }
            let mode = metadata.permissions().mode() & 0o7777;
            fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700)).unwrap();
            Some((metadata.ino(), mode))
        };
        let parent = path.parent().expect("a path in a tree");
        let opened = [open(parent), open(path)];
        let result = tamper();
        for (path, opened) in [parent, path].into_iter().zip(opened) {
            if let Some((inode, mode)) = opened
                && fs::symlink_metadata(path).is_ok_and(|metadata| {
                    !metadata.file_type().is_symlink() && metadata.ino() == inode
                })
            {
                fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            }
        }
        result
    }
}
