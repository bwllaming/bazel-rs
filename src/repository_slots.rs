//! Stable repository directories ("slots") of the native repository store.
//!
//! Bazel 9.3 fetches every repository in `<output_base>/external/<canonical
//! name>` (RepositoryFetchFunction.java:158-160) and the repository stays
//! there; a rule's `repository_ctx.path(".")` and every path a tool derives
//! from it (gazelle's `${SRCDIR}`, language/go/generate.go:130-144:
//! `external/<name>` when the parent directory is `external`, else
//! `../<name>`) name that directory. Natively the result is published as a
//! content-addressed tree (`direct/trees/<identity>`, crate::repository_store)
//! that other repositories and actions use, and a rule runs in its SLOT
//! `<store>/external/<canonical name>`: the same path in every process,
//! invocation and worktree sharing the store, with `external` as its parent
//! directory like genuine's.
//!
//! After a successful execution the slot of an ORDINARY repository is
//! renamed into its tree and replaced by the symlink `../direct/trees/<id>`
//! (genuine moves a reproducible repository into its contents cache and
//! leaves a symlink, LocalRepoContentsCache.java:157-185), so relative paths
//! another rule derives from its own directory (`../<name>/...`) resolve
//! through the slot as in genuine's `external/`. A SELF-REFERENCING
//! repository — its published files or link targets contain its own slot
//! path, as gazelle's `go_repository_cache` does in `go.env` (GOPATH and
//! GOCACHE are its own directory, which other repositories' `go` commands
//! write into) — keeps its slot as a real, mutable directory; a copy is
//! published as the tree and a marker `direct/slots/<slug>` records that the
//! slot is complete (genuine's marker file is written after the repository,
//! DigestWriter.java:76-89, and deleted before a fetch,
//! RepositoryFetchFunction.java:311).
//!
//! Locks (`direct/locks/`), all `flock`s on open file descriptions:
//! - WRITERS (an execution, a replay, a conversion) take
//!   `slot-<slug>.gate` exclusively, then `slot-<slug>` exclusively, and
//!   release the gate ([`SlotWriter`]);
//! - READERS (another repository's `execute` that may write into a mutable
//!   slot, crate::repository_ctx) take the gate shared, then `slot-<slug>`
//!   shared, and release the gate, for the duration of one subprocess
//!   ([`SlotReaders`]): a waiting writer holds the gate, so new readers wait
//!   behind it (flock is not fair; the gate keeps a stream of readers from
//!   starving the writer). Several slots are taken all or nothing, in slug
//!   order, holding nothing while waiting.
//!
//! Ordinary reuse binding also participates in the writer protocol ([`bind`]),
//! but never waits: its caller may hold a dependent repository's writer lock.
//! A busy slot is left alone; adoption can therefore exclude binders without
//! creating a dependency-lock inversion. Mutable marked directories are kept.
//! Slot maintenance uses the raw filesystem, never crate::observed_fs: slots
//! are no inputs of the analysis snapshot (its records name trees only).

use fs2::FileExt;
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The directory of the slots below the store root.
pub const SLOTS: &str = "external";

/// How a published repository's slot relates to its tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlotClass {
    /// The slot is the symlink `../direct/trees/<identity>`.
    Ordinary,
    /// The tree embeds the slot's own path: the slot is a real, mutable
    /// directory (with a marker), the tree a copy of what the rule wrote.
    SelfReferencing,
}

/// The slot of repository `canonical` in the store rooted at `store`.
pub fn slot_path(store: &Path, canonical: &str) -> PathBuf {
    store.join(SLOTS).join(slot_name(canonical).as_ref())
}

/// The directory name of `canonical`'s slot: the canonical name itself, or
/// for a name longer than a Linux file name may be (NAME_MAX, 255 bytes:
/// genuine cannot create its `external/<name>` at all and fails with "File
/// name too long") `#long-<128-bit digest>`, which no repository name can
/// spell (`#` is no repository-name character).
pub fn slot_name(canonical: &str) -> std::borrow::Cow<'_, str> {
    if canonical.len() <= 255 {
        std::borrow::Cow::Borrowed(canonical)
    } else {
        std::borrow::Cow::Owned(format!("#long-{}", slug(canonical)))
    }
}

/// The 128-bit name of `canonical`'s locks and marker (the slot itself is
/// named by the canonical name).
pub fn slug(canonical: &str) -> String {
    blake3::hash(canonical.as_bytes()).to_hex()[..32].to_owned()
}

/// A canonical name usable as one slot directory name ([`slot_name`]).
fn checked_name(canonical: &str) -> Result<(), String> {
    let name = slot_name(canonical);
    let mut components = Path::new(name.as_ref()).components();
    if canonical.is_empty()
        || canonical.contains(['/', '\0'])
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || name.starts_with('.')
    {
        return Err(format!(
            "repository name {canonical:?} cannot name a repository directory"
        ));
    }
    Ok(())
}

fn locks(store: &Path) -> PathBuf {
    store.join("direct").join("locks")
}

/// Open (creating) the lock file `name` below `locks`: a regular file, never
/// through a symlink.
fn lock_file(locks: &Path, name: &str) -> Result<File, String> {
    let path = locks.join(name);
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW | O_NONBLOCK, as the store's other locks.
        options.custom_flags(0x20000 | 0x800);
    }
    let file = options
        .open(&path)
        .map_err(|e| format!("cannot open repository lock {}: {e}", path.display()))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!(
            "repository lock {} is not a regular file",
            path.display()
        ));
    }
    Ok(file)
}

fn would_block(error: &std::io::Error) -> bool {
    error.kind() == ErrorKind::WouldBlock || error.raw_os_error() == Some(11)
}

thread_local! {
    /// The slots this thread holds as a writer (store, canonical): its own
    /// execution's `execute` calls never take them as a reader.
    static WRITING: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Whether this thread holds the slot `slot` as a writer.
pub fn writing(slot: &Path) -> bool {
    WRITING.with(|writing| writing.borrow().iter().any(|held| held == slot))
}

/// The exclusive hold of one slot (the writer protocol, see the module
/// docs): released when dropped, explicitly — a copy of the descriptor in a
/// child another thread is starting (until its exec closes it) would keep a
/// merely closed flock alive.
pub struct SlotWriter {
    store: PathBuf,
    canonical: String,
    slot: PathBuf,
    lock: File,
}

impl SlotWriter {
    /// Take `canonical`'s slot exclusively, waiting for its readers and any
    /// other writer.
    pub fn acquire(store: &Path, canonical: &str) -> Result<Self, String> {
        Self::take(store, canonical, true).map(|writer| writer.expect("a blocking acquisition"))
    }

    /// [`Self::acquire`] without waiting: `None` when anyone holds the slot
    /// or its gate.
    pub fn try_acquire(store: &Path, canonical: &str) -> Result<Option<Self>, String> {
        Self::take(store, canonical, false)
    }

    fn take(store: &Path, canonical: &str, wait: bool) -> Result<Option<Self>, String> {
        checked_name(canonical)?;
        let locks = locks(store);
        let slug = slug(canonical);
        let gate = lock_file(&locks, &format!("slot-{slug}.gate"))?;
        let lock = |file: &File| -> Result<bool, String> {
            let taken = if wait {
                FileExt::lock_exclusive(file)
            } else {
                FileExt::try_lock_exclusive(file)
            };
            match taken {
                Ok(()) => Ok(true),
                Err(error) if !wait && would_block(&error) => Ok(false),
                Err(error) => Err(format!(
                    "cannot lock the repository directory of @@{canonical}: {error}"
                )),
            }
        };
        if !lock(&gate)? {
            return Ok(None);
        }
        let slot_lock = lock_file(&locks, &format!("slot-{slug}"))?;
        let taken = lock(&slot_lock);
        let _ = FileExt::unlock(&gate);
        if !taken? {
            return Ok(None);
        }
        let slot = slot_path(store, canonical);
        WRITING.with(|writing| writing.borrow_mut().push(slot.clone()));
        Ok(Some(Self {
            store: store.to_path_buf(),
            canonical: canonical.to_owned(),
            slot,
            lock: slot_lock,
        }))
    }

    /// The slot this writer holds.
    pub fn path(&self) -> &Path {
        &self.slot
    }

    /// A fresh, empty real directory at the slot: whatever was there — the
    /// link of an earlier publication, an interrupted execution, a mutable
    /// slot — is removed first. Ordinary binders share this writer protocol.
    pub fn fresh(&self) -> Result<PathBuf, String> {
        ensure_slots_directory(&self.store)?;
        for _ in 0..64 {
            match fs::symlink_metadata(&self.slot) {
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    match fs::create_dir(&self.slot) {
                        Ok(()) => return Ok(self.slot.clone()),
                        Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                        Err(error) => {
                            return Err(format!(
                                "cannot create the repository directory {}: {error}",
                                self.slot.display()
                            ));
                        }
                    }
                }
                Err(error) => {
                    return Err(format!("cannot inspect {}: {error}", self.slot.display()));
                }
                Ok(_) => crate::repository_ctx::remove_tree(&self.slot).map_err(|error| {
                    format!(
                        "cannot clear the repository directory {}: {error}",
                        self.slot.display()
                    )
                })?,
            }
        }
        Err(format!(
            "cannot create the repository directory {}: it keeps reappearing",
            self.slot.display()
        ))
    }

    /// Bind the slot to publication `identity`: the link
    /// `../direct/trees/<identity>` replaces whatever is at the slot — the
    /// writer's own finished directory included (exchanged atomically where
    /// the filesystem can, so a reader through the slot always finds a
    /// directory there).
    pub fn bind(&self, identity: &str) -> Result<(), String> {
        let target = link_target(identity);
        if fs::read_link(&self.slot).is_ok_and(|current| current == target) {
            return Ok(());
        }
        ensure_slots_directory(&self.store)?;
        let link = temporary_link(&self.store, &target)?;
        let is_directory = fs::symlink_metadata(&self.slot).is_ok_and(|metadata| metadata.is_dir());
        #[cfg(target_os = "linux")]
        if is_directory
            && rustix::fs::renameat_with(
                rustix::fs::CWD,
                &link,
                rustix::fs::CWD,
                &self.slot,
                rustix::fs::RenameFlags::EXCHANGE,
            )
            .is_ok()
        {
            // The directory is now at the temporary name.
            return crate::repository_ctx::remove_tree(&link).map_err(|error| {
                format!(
                    "cannot remove the replaced repository directory {}: {error}",
                    link.display()
                )
            });
        }
        if is_directory {
            crate::repository_ctx::remove_tree(&self.slot).map_err(|error| {
                let _ = fs::remove_file(&link);
                format!(
                    "cannot clear the repository directory {}: {error}",
                    self.slot.display()
                )
            })?;
        }
        fs::rename(&link, &self.slot).map_err(|error| {
            let _ = fs::remove_file(&link);
            format!(
                "cannot bind the repository directory {}: {error}",
                self.slot.display()
            )
        })
    }

    /// Mark the slot as the complete mutable directory of publication
    /// `identity` (a self-referencing repository).
    pub fn mark(&self, identity: &str) -> Result<(), String> {
        write_marker(&self.store, &self.canonical, identity)
    }

    /// Delete the slot's marker: from now on the slot is incomplete until a
    /// writer marks it again.
    pub fn unmark(&self) -> Result<(), String> {
        remove_marker(&self.store, &self.canonical)
    }

    /// Historical adoption may replace only an unmarked link (or a missing
    /// link), never repair or replace a real mutable/in-progress root.
    pub(crate) fn immutable(&self) -> bool {
        immutable_slot(&self.store, &self.canonical)
    }

    pub(crate) fn bind_immutable(&self, identity: &str) -> Result<(), String> {
        if !self.immutable() {
            return Err(format!(
                "repository @@{} is no longer an immutable slot",
                self.canonical
            ));
        }
        self.bind(identity)?;
        let expected = fs::canonicalize(self.store.join("direct/trees").join(identity))
            .map_err(|error| format!("adopted publication disappeared: {error}"))?;
        if fs::read_link(&self.slot).is_ok_and(|target| target == link_target(identity))
            && fs::canonicalize(&self.slot).ok() == Some(expected)
        {
            Ok(())
        } else {
            Err(format!(
                "repository @@{} binding did not verify",
                self.canonical
            ))
        }
    }
}

/// The writer's directory at its slot until it is published, bound or kept
/// ([`Self::keep`]): removed when dropped otherwise (a failed or interrupted
/// execution, Rust unwinding), like the stage it replaces. Only a real
/// directory is removed — under the writer lock any real directory at the
/// slot is this writer's — never a link.
pub struct SlotScratch<'a> {
    writer: &'a SlotWriter,
    armed: bool,
}

impl<'a> SlotScratch<'a> {
    pub fn new(writer: &'a SlotWriter) -> Self {
        Self {
            writer,
            armed: true,
        }
    }

    /// The slot holds what it should hold now: nothing to remove.
    pub fn keep(mut self) {
        self.armed = false;
    }
}

impl Drop for SlotScratch<'_> {
    fn drop(&mut self) {
        let slot = &self.writer.slot;
        if self.armed
            && fs::symlink_metadata(slot).is_ok_and(|metadata| metadata.is_dir())
            && let Err(error) = crate::repository_ctx::remove_tree(slot)
        {
            crate::note!(
                "bazel-rs: warning: cannot remove {} ({error}); left for the next execution",
                slot.display()
            );
        }
    }
}

impl Drop for SlotWriter {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock);
        WRITING.with(|writing| {
            let mut writing = writing.borrow_mut();
            if let Some(index) = writing.iter().rposition(|held| *held == self.slot) {
                writing.remove(index);
            }
        });
    }
}

/// The shared hold of a set of slots (the reader protocol, see the module
/// docs), released when dropped.
#[derive(Default)]
pub struct SlotReaders {
    held: Vec<File>,
}

impl SlotReaders {
    /// Take every slot of `canonicals` shared, all or nothing in slug order:
    /// a slot that is not free (a writer holds it or waits for it) releases
    /// everything taken so far, is waited for alone, and the acquisition
    /// starts over — so a reader never holds one slot while it waits for
    /// another.
    pub fn acquire(store: &Path, canonicals: &[String]) -> Result<Self, String> {
        let mut wanted: Vec<(String, &str)> = canonicals
            .iter()
            .map(|canonical| {
                checked_name(canonical).map(|()| (slug(canonical), canonical.as_str()))
            })
            .collect::<Result<_, _>>()?;
        wanted.sort();
        wanted.dedup();
        let locks = locks(store);
        loop {
            let mut held = Self::default();
            let mut busy = None;
            for (slug, canonical) in &wanted {
                match Self::take(&locks, slug, canonical, false)? {
                    Some(file) => held.held.push(file),
                    None => {
                        busy = Some((slug, canonical));
                        break;
                    }
                }
            }
            let Some((slug, canonical)) = busy else {
                return Ok(held);
            };
            drop(held);
            // Wait for the busy slot alone, then try everything again.
            let waited =
                Self::take(&locks, slug, canonical, true)?.expect("a blocking acquisition");
            let _ = FileExt::unlock(&waited);
        }
    }

    fn take(locks: &Path, slug: &str, canonical: &str, wait: bool) -> Result<Option<File>, String> {
        let error = |error: std::io::Error| {
            format!("cannot lock the repository directory of @@{canonical} for reading: {error}")
        };
        let gate = lock_file(locks, &format!("slot-{slug}.gate"))?;
        if wait {
            FileExt::lock_shared(&gate).map_err(error)?;
        } else {
            match FileExt::try_lock_shared(&gate) {
                Ok(()) => {}
                Err(failure) if would_block(&failure) => return Ok(None),
                Err(failure) => return Err(error(failure)),
            }
        }
        let slot = lock_file(locks, &format!("slot-{slug}"))?;
        let taken = if wait {
            FileExt::lock_shared(&slot).map(|()| true)
        } else {
            match FileExt::try_lock_shared(&slot) {
                Ok(()) => Ok(true),
                Err(failure) if would_block(&failure) => Ok(false),
                Err(failure) => Err(failure),
            }
        };
        let _ = FileExt::unlock(&gate);
        Ok(taken.map_err(error)?.then_some(slot))
    }

    /// How many slots are held.
    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// Whether no slot is held.
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

impl Drop for SlotReaders {
    fn drop(&mut self) {
        for file in &self.held {
            let _ = FileExt::unlock(file);
        }
    }
}

/// The link target of a slot bound to publication `identity`.
pub fn link_target(identity: &str) -> PathBuf {
    Path::new("../direct/trees").join(identity)
}

fn ensure_slots_directory(store: &Path) -> Result<(), String> {
    let slots = store.join(SLOTS);
    match fs::create_dir(&slots) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("cannot create {}: {error}", slots.display())),
    }
    if !fs::symlink_metadata(&slots).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(format!(
            "repository directories require a real directory: {}",
            slots.display()
        ));
    }
    Ok(())
}

/// A new symlink to `target` at a private temporary name in the slots
/// directory.
fn temporary_link(store: &Path, target: &Path) -> Result<PathBuf, String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let slots = store.join(SLOTS);
    for _ in 0..1000 {
        // A crashed process's leftover may hold a name: the next one.
        let name = format!(
            ".bind-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        let link = slots.join(name);
        match std::os::unix::fs::symlink(target, &link) {
            Ok(()) => return Ok(link),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("cannot create {}: {error}", link.display()));
            }
        }
    }
    Err(format!(
        "cannot create a temporary link in {}",
        slots.display()
    ))
}

/// Bind `canonical`'s slot to publication `identity` under the writer lock (a
/// warm reuse, in any process): nothing when the slot already links there;
/// a link (missing, or naming another publication) is replaced by renaming
/// a new one over it; a real directory is never replaced — a writer's
/// execution or a mutable slot — except a leftover of an interrupted
/// writer: a directory without a marker whose locks can be taken without
/// waiting. A busy writer/reader leaves the slot alone. Nested callers holding
/// this slot's writer already reuse that hold; callers holding a different
/// slot never wait, avoiding dependency-lock inversion.
pub fn bind(store: &Path, canonical: &str, identity: &str) -> Result<(), String> {
    checked_name(canonical)?;
    let slot = slot_path(store, canonical);
    let writer = if writing(&slot) {
        None
    } else {
        let Some(writer) = SlotWriter::try_acquire(store, canonical)? else {
            return Ok(());
        };
        Some(writer)
    };
    let target = link_target(identity);
    match fs::read_link(&slot) {
        Ok(current) if current == target => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(_) => {
            // Not a link: a real directory (or another file).
            if marker(store, canonical).is_some() {
                return Ok(());
            }
            // A nested warm probe never clears its writer's in-progress
            // directory. Only a newly acquired writer may repair a leftover.
            let Some(writer) = &writer else {
                return Ok(());
            };
            return writer.bind(identity);
        }
    }
    ensure_slots_directory(store)?;
    let link = temporary_link(store, &target)?;
    match fs::rename(&link, &slot) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&link);
            // A directory appeared meanwhile: never replaced.
            if fs::symlink_metadata(&slot).is_ok_and(|metadata| metadata.is_dir()) {
                Ok(())
            } else {
                Err(format!(
                    "cannot bind the repository directory {}: {error}",
                    slot.display()
                ))
            }
        }
    }
}

const MARKER_MAGIC: &str = "bazel-rs repository directory marker 1";

pub(crate) fn immutable_slot(store: &Path, canonical: &str) -> bool {
    fs::symlink_metadata(marker_path(store, canonical))
        .is_err_and(|error| error.kind() == ErrorKind::NotFound)
        && match fs::symlink_metadata(slot_path(store, canonical)) {
            Ok(metadata) => metadata.file_type().is_symlink(),
            Err(error) => error.kind() == ErrorKind::NotFound,
        }
}

/// A snapshot fast path's current public view, not a planning observation.
/// A complete mutable slot keeps its established ordinary validation path;
/// an immutable selection must have the exact link. A mismatching view needs
/// guarded adoption or full analysis, even if its pointer is already current.
pub(crate) fn selected_view(store: &Path, canonical: &str, identity: &str) -> bool {
    let slot = slot_path(store, canonical);
    if fs::symlink_metadata(&slot).is_ok_and(|metadata| metadata.is_dir()) {
        return marker(store, canonical).as_deref() == Some(identity);
    }
    fs::read_link(slot).ok() == Some(link_target(identity))
}

fn marker_path(store: &Path, canonical: &str) -> PathBuf {
    store.join("direct").join("slots").join(slug(canonical))
}

/// The identity a complete mutable slot of `canonical` was made for (its
/// marker), if the marker exists and is well formed. An ordinary
/// repository's slot has a marker only once another repository's tool wrote
/// into it ([`written_into`]): its slot became a mutable copy of that tree.
pub fn marker(store: &Path, canonical: &str) -> Option<String> {
    let text = fs::read_to_string(marker_path(store, canonical)).ok()?;
    let mut lines = text.lines();
    if lines.next()? != MARKER_MAGIC {
        return None;
    }
    let identity = lines.next()?;
    let named = lines.next()?;
    (named == canonical
        && identity.len() == 64
        && identity
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then(|| identity.to_owned())
}

/// Write the marker of `canonical`'s mutable slot (atomically: a temporary
/// file renamed into place). A marker is a small fixed-format text record
/// (magic line, identity, canonical name), not JSON.
fn write_marker(store: &Path, canonical: &str, identity: &str) -> Result<(), String> {
    let path = marker_path(store, canonical);
    let directory = path.parent().expect("a marker has a parent");
    match fs::create_dir(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("cannot create {}: {error}", directory.display())),
    }
    let mut stage = tempfile::NamedTempFile::new_in(directory)
        .map_err(|e| format!("cannot stage a repository directory marker: {e}"))?;
    stage
        .write_all(format!("{MARKER_MAGIC}\n{identity}\n{canonical}\n").as_bytes())
        .and_then(|()| stage.as_file().sync_all())
        .map_err(|e| e.to_string())?;
    stage
        .persist(&path)
        .map_err(|e| format!("cannot write the repository directory marker: {e}"))?;
    Ok(())
}

fn remove_marker(store: &Path, canonical: &str) -> Result<(), String> {
    match fs::remove_file(marker_path(store, canonical)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "cannot remove the repository directory marker of @@{canonical}: {error}"
        )),
    }
}

/// The process-wide record of the slots this process materialized: slot
/// path → (publication identity, class).
fn registry() -> &'static Mutex<HashMap<PathBuf, (String, SlotClass)>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, (String, SlotClass)>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Record that this process materialized `canonical` as publication
/// `identity` of `class`.
pub fn note(store: &Path, canonical: &str, identity: &str, class: SlotClass) {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(slot_path(store, canonical), (identity.to_owned(), class));
}

/// What this process materialized at `slot` ([`note`]).
pub fn noted(slot: &Path) -> Option<(String, SlotClass)> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(slot)
        .cloned()
}

/// Whether `canonical`'s slot is a complete mutable directory: a real
/// directory with its marker.
pub fn complete(store: &Path, canonical: &str) -> bool {
    complete_for(store, canonical, None)
}

/// [`complete`], made for publication `identity` when given.
pub fn complete_for(store: &Path, canonical: &str, identity: Option<&str>) -> bool {
    fs::symlink_metadata(slot_path(store, canonical)).is_ok_and(|metadata| metadata.is_dir())
        && marker(store, canonical)
            .is_some_and(|marked| identity.is_none_or(|identity| marked == identity))
}

/// A byte that ends a path inside an argument or environment value: a
/// list separator, a quote, an assignment, white space.
fn path_end(byte: u8) -> bool {
    matches!(byte, b':' | b';' | b',' | b'\'' | b'"' | b'=' | 0) || byte.is_ascii_whitespace()
}

/// Where `root` occurs in `text` as a whole path or a path's prefix
/// (followed by `/`, a [`path_end`] byte or the end).
fn occurrences(text: &[u8], root: &[u8]) -> Vec<usize> {
    if root.is_empty() {
        return Vec::new();
    }
    memchr::memmem::find_iter(text, root)
        .filter(|&at| {
            text.get(at + root.len())
                .is_none_or(|&next| next == b'/' || path_end(next))
        })
        .collect()
}

/// Whether one of `texts` names the directory `root` or a path below it.
pub fn names_path<'a>(root: &[u8], mut texts: impl Iterator<Item = &'a [u8]>) -> bool {
    texts.any(|text| !occurrences(text, root).is_empty())
}

/// Whether one of `values` (an `execute`'s arguments and environment)
/// names a path below the published tree `root` that the tree does not
/// have: where the tool will write (gazelle's go_repository_tools runs
/// `env -i GOCACHE=<go_repository_cache>/gocache ... go build`, the
/// directory derived from a label into the cache repository,
/// internal/env.bzl resolve_go_env, internal/common.bzl env_execute). Genuine's repository is a
/// plain directory under `external/` that the tool writes into; a published
/// tree must never change, so that repository's slot becomes a mutable copy
/// of the tree for the tool to use (crate::repository_ctx).
pub fn written_into<'a>(root: &Path, values: impl Iterator<Item = &'a [u8]>) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let bytes = root.as_os_str().as_bytes();
    for value in values {
        for at in occurrences(value, bytes) {
            let rest = &value[at + bytes.len()..];
            let length = rest.iter().take_while(|&&byte| !path_end(byte)).count();
            let rest = &rest[..length];
            if rest.len() > 1
                && fs::symlink_metadata(Path::new(std::ffi::OsStr::from_bytes(
                    &value[at..at + bytes.len() + length],
                )))
                .is_err_and(|error| error.kind() == ErrorKind::NotFound)
            {
                return true;
            }
        }
    }
    false
}

/// `text` with every occurrence of the path `from` ([`names_path`])
/// replaced by `to`; `None` when there is none.
pub fn redirected(text: &[u8], from: &[u8], to: &[u8]) -> Option<Vec<u8>> {
    let starts = occurrences(text, from);
    if starts.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() + starts.len() * to.len());
    let mut done = 0;
    for at in starts {
        out.extend_from_slice(&text[done..at]);
        out.extend_from_slice(to);
        done = at + from.len();
    }
    out.extend_from_slice(&text[done..]);
    Some(out)
}

/// Make `canonical`'s mutable slot again from its published `tree`
/// (publication `identity`), under the writer protocol — unless another
/// writer completed it meanwhile: a self-referencing repository reused
/// while its slot is missing, a link, or an interrupted writer's leftover
/// (no marker). The slot gets fresh copies of the tree's entries, then the
/// marker. What else was in the slot is gone, as after a re-execution.
/// `exact`: a complete slot made for another publication is made again too
/// (an ordinary repository written into: its slot is a copy of the tree
/// the consumer resolved, never another version's); a self-referencing
/// one accepts any complete slot (no ping-pong between worktrees).
pub fn restore_mutable(
    store: &Path,
    canonical: &str,
    identity: &str,
    tree: &Path,
    exact: bool,
) -> Result<(), String> {
    let writer = SlotWriter::acquire(store, canonical)?;
    if complete_for(store, canonical, exact.then_some(identity)) {
        return Ok(());
    }
    writer.unmark()?;
    let slot = writer.fresh()?;
    let scratch = SlotScratch::new(&writer);
    for entry in fs::read_dir(tree).map_err(|e| format!("cannot read {}: {e}", tree.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        copy_tree(&entry.path(), &slot.join(entry.file_name()))?;
    }
    fs::set_permissions(
        &slot,
        fs::metadata(tree).map_err(|e| e.to_string())?.permissions(),
    )
    .map_err(|e| e.to_string())?;
    writer.mark(identity)?;
    scratch.keep();
    Ok(())
}

/// The slot names (canonical names) that occur as `<store>/external/<name>`
/// in `texts` (an `execute`'s arguments, environment values and working
/// directory): a command may name a mutable slot it did not reach through
/// a label. `#long-` names are never matched (a label reaches those).
pub fn named_slots<'a>(
    store: &Path,
    texts: impl IntoIterator<Item = &'a [u8]>,
) -> BTreeSet<String> {
    use std::os::unix::ffi::OsStrExt;
    let mut needle = store.join(SLOTS).as_os_str().as_bytes().to_vec();
    needle.push(b'/');
    let finder = memchr::memmem::Finder::new(&needle);
    let mut names = BTreeSet::new();
    for text in texts {
        for at in finder.find_iter(text) {
            let rest = &text[at + needle.len()..];
            let length = rest
                .iter()
                .take_while(|byte| name_byte(**byte) && **byte != b'#')
                .count();
            if length > 0
                && let Ok(name) = std::str::from_utf8(&rest[..length])
                && checked_name(name).is_ok()
            {
                names.insert(name.to_owned());
            }
        }
    }
    names
}

/// Copy the tree `from` to the new directory `to` as fresh files (single
/// links, the source modes, link targets verbatim; never following links):
/// the published copy of a self-referencing repository, whose slot stays
/// mutable. Directories get their modes after their entries, so read-only
/// ones copy too.
pub fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::symlink_metadata(from).map_err(|e| e.to_string())?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(from).map_err(|e| e.to_string())?;
        return std::os::unix::fs::symlink(target, to)
            .map_err(|e| format!("cannot copy {}: {e}", from.display()));
    }
    if metadata.is_dir() {
        fs::create_dir(to).map_err(|e| format!("cannot copy {}: {e}", from.display()))?;
        for entry in fs::read_dir(from).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        return fs::set_permissions(to, metadata.permissions())
            .map_err(|e| format!("cannot copy {}: {e}", from.display()));
    }
    if !metadata.is_file() {
        return Err(format!(
            "unsupported file type in repository directory: {}",
            from.display()
        ));
    }
    let mut source =
        File::open(from).map_err(|e| format!("cannot copy {}: {e}", from.display()))?;
    let mut copy = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
        .map_err(|e| format!("cannot copy {}: {e}", from.display()))?;
    std::io::copy(&mut source, &mut copy)
        .map_err(|e| format!("cannot copy {}: {e}", from.display()))?;
    copy.set_permissions(fs::Permissions::from_mode(
        metadata.permissions().mode() & 0o7777,
    ))
    .map_err(|e| format!("cannot copy {}: {e}", from.display()))
}

/// What a published tree's bytes say about paths into the store
/// ([`EmbeddedPaths`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Embedded {
    /// A file or link target contains the repository's own slot path.
    pub self_referencing: bool,
    /// The other store trees named (`direct/trees/<id>`, `trees/<id>`,
    /// `git/trees/<id>`, `builtins/trees/<id>`), for a future collector.
    pub refs: BTreeSet<String>,
}

/// The store-path scan of one tree's bytes, riding on the hashing reads of
/// a snapshot: occurrences of `<store>/` followed by `external/<own name>`
/// (a self-reference: the next byte is no repository-name byte) or by a
/// tree area and a 64-digit identity (a reference).
pub struct EmbeddedPaths {
    needle: Vec<u8>,
    own: Vec<u8>,
    found: Embedded,
}

/// The most bytes a classification reads after `<store>/`: `external/` and
/// a 255-byte name plus the byte after it, or an area and an identity.
const TAIL: usize = SLOTS.len() + 1 + 255 + 1;

const AREAS: [&str; 4] = ["direct/trees/", "trees/", "git/trees/", "builtins/trees/"];

/// A byte of a slot name ([`slot_name`]).
fn name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+' | b'~' | b'#')
}

impl EmbeddedPaths {
    pub fn new(store: &Path, canonical: &str) -> Self {
        use std::os::unix::ffi::OsStrExt;
        let mut needle = store.as_os_str().as_bytes().to_vec();
        needle.push(b'/');
        Self {
            needle,
            own: slot_name(canonical).as_bytes().to_vec(),
            found: Embedded::default(),
        }
    }

    /// A streaming scan of one file's bytes (write them, then
    /// [`FileScan::finish`]).
    pub fn file(&mut self) -> FileScan<'_> {
        FileScan {
            paths: self,
            window: Vec::new(),
        }
    }

    /// Scan one link target.
    pub fn link(&mut self, target: &[u8]) {
        let mut scan = self.file();
        scan.feed(target);
        scan.finish();
    }

    pub fn finish(self) -> Embedded {
        self.found
    }

    fn classify(&mut self, tail: &[u8]) {
        if let Some(rest) = tail
            .strip_prefix(SLOTS.as_bytes())
            .and_then(|rest| rest.strip_prefix(b"/"))
        {
            let length = rest.iter().take_while(|byte| name_byte(**byte)).count();
            if rest[..length] == self.own[..] {
                self.found.self_referencing = true;
            }
            return;
        }
        for area in AREAS {
            if let Some(rest) = tail.strip_prefix(area.as_bytes())
                && rest.len() >= 64
                && rest[..64]
                    .iter()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
                && rest.get(64).is_none_or(|b| !b.is_ascii_hexdigit())
            {
                self.found
                    .refs
                    .insert(format!("{area}{}", String::from_utf8_lossy(&rest[..64])));
                return;
            }
        }
    }
}

/// The scan of one file in progress ([`EmbeddedPaths::file`]).
pub struct FileScan<'a> {
    paths: &'a mut EmbeddedPaths,
    window: Vec<u8>,
}

impl FileScan<'_> {
    fn feed(&mut self, bytes: &[u8]) {
        self.window.extend_from_slice(bytes);
        let needle = self.paths.needle.len();
        let mut start = 0;
        while let Some(found) = memchr::memmem::find(&self.window[start..], &self.paths.needle) {
            let at = start + found;
            if at + needle + TAIL > self.window.len() {
                // Its tail is not complete yet: keep the match.
                self.window.drain(..at);
                return;
            }
            self.paths
                .classify(&self.window[at + needle..at + needle + TAIL]);
            start = at + 1;
        }
        // A match may still straddle the next bytes.
        let keep = self.window.len().saturating_sub(needle - 1);
        self.window.drain(..keep.max(start).min(self.window.len()));
    }

    /// The end of the file: what is left is classified with the tails it has.
    pub fn finish(mut self) {
        let needle = self.paths.needle.len();
        let mut start = 0;
        let window = std::mem::take(&mut self.window);
        while let Some(found) = memchr::memmem::find(&window[start..], &self.paths.needle) {
            let at = start + found;
            let end = (at + needle + TAIL).min(window.len());
            self.paths.classify(&window[at + needle..end]);
            start = at + 1;
        }
    }
}

impl Write for FileScan<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.feed(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("repository-slots-test-")
            .tempdir_in(crate::test_temp::root())
            .unwrap()
    }

    fn scan(store: &Path, own: &str, chunks: &[&[u8]]) -> Embedded {
        let mut paths = EmbeddedPaths::new(store, own);
        let mut file = paths.file();
        for chunk in chunks {
            file.write_all(chunk).unwrap();
        }
        file.finish();
        paths.finish()
    }

    /// The scan finds the repository's own slot path and other trees'
    /// paths, across chunk boundaries, and never a longer name or a
    /// non-identity.
    #[test]
    fn embedded_store_paths_are_found_across_chunks() {
        let store = Path::new("/c/repositories");
        let id = "a".repeat(64);
        let text = format!(
            "GOPATH='/c/repositories/external/gocache'\nGOROOT='/c/repositories/direct/trees/{id}'\n"
        );
        for split in 0..text.len() {
            let (a, b) = text.as_bytes().split_at(split);
            let found = scan(store, "gocache", &[a, b]);
            assert!(found.self_referencing, "split at {split}");
            assert_eq!(
                found.refs.iter().collect::<Vec<_>>(),
                [&format!("direct/trees/{id}")],
                "split at {split}"
            );
        }
        // Another (longer) name, another store, a short identity.
        for other in [
            "x /c/repositories/external/gocache2/x".to_owned(),
            "x /c/repositories/external/gocach".to_owned(),
            "x /d/repositories/external/gocache".to_owned(),
            format!("x /c/repositories/direct/trees/{}", "a".repeat(63)),
            format!("x /c/repositories/direct/trees/{}", "a".repeat(65)),
        ] {
            let found = scan(store, "gocache", &[other.as_bytes()]);
            assert_eq!(found, Embedded::default(), "{other}");
        }
        // The name at the very end, and as a directory prefix.
        assert!(scan(store, "r", &[b"/c/repositories/external/r"]).self_referencing);
        assert!(scan(store, "r", &[b"\"/c/repositories/external/r/pkg/mod\""]).self_referencing);
        let mut paths = EmbeddedPaths::new(store, "r");
        paths.link(format!("/c/repositories/trees/{id}/bin").as_bytes());
        assert_eq!(paths.finish().refs.len(), 1);
    }

    /// The writer protocol excludes readers and writers in both directions;
    /// a waiting writer's gate holds new readers off (no starvation); a
    /// reader takes several slots all or nothing.
    #[test]
    fn writers_and_readers_exclude_each_other() {
        let store = temp();
        fs::create_dir_all(store.path().join("direct/locks")).unwrap();
        let store = store.path().to_path_buf();
        let readers = SlotReaders::acquire(&store, &["a".to_owned()]).unwrap();
        assert_eq!(readers.len(), 1);
        assert!(SlotWriter::try_acquire(&store, "a").unwrap().is_none());
        // Readers share.
        let second = SlotReaders::acquire(&store, &["a".to_owned()]).unwrap();
        drop(second);
        // A writer waits for the reader.
        let (sender, receiver) = std::sync::mpsc::channel();
        let writer = {
            let store = store.clone();
            std::thread::spawn(move || {
                let writer = SlotWriter::acquire(&store, "a").unwrap();
                sender.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(200));
                drop(writer);
            })
        };
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err()
        );
        // While the writer waits it holds the gate: a new reader cannot
        // start (it would starve the writer).
        let mut gate_held = false;
        for _ in 0..100 {
            let gate = lock_file(&locks(&store), &format!("slot-{}.gate", slug("a"))).unwrap();
            if FileExt::try_lock_shared(&gate).is_err() {
                gate_held = true;
                break;
            }
            let _ = FileExt::unlock(&gate);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(gate_held, "the waiting writer holds the gate");
        drop(readers);
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the writer runs once the reader is gone");
        // A reader waits for the writer.
        let started = std::time::Instant::now();
        let after = SlotReaders::acquire(&store, &["a".to_owned(), "b".to_owned()]).unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(100));
        assert_eq!(after.len(), 2);
        writer.join().unwrap();
        // All or nothing: with "b" held by a writer, a reader of a and b
        // holds neither while it waits.
        drop(after);
        let b = SlotWriter::acquire(&store, "b").unwrap();
        let waiting = {
            let store = store.clone();
            std::thread::spawn(move || {
                SlotReaders::acquire(&store, &["b".to_owned(), "a".to_owned()])
                    .unwrap()
                    .len()
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(100));
        let a = SlotWriter::try_acquire(&store, "a").unwrap();
        assert!(a.is_some(), "a waiting reader holds no other slot");
        drop(a);
        drop(b);
        assert_eq!(waiting.join().unwrap(), 2);
    }

    /// The reuse binding replaces links but never a real directory with
    /// a marker or one a writer holds; a marker-less leftover with free
    /// locks is replaced.
    #[test]
    fn reuse_binding_never_replaces_a_working_real_directory() {
        let store = temp();
        let store = store.path().to_path_buf();
        fs::create_dir_all(store.join("direct/locks")).unwrap();
        let one = "1".repeat(64);
        let two = "2".repeat(64);
        bind(&store, "r", &one).unwrap();
        let slot = slot_path(&store, "r");
        assert_eq!(fs::read_link(&slot).unwrap(), link_target(&one));
        bind(&store, "r", &two).unwrap();
        assert_eq!(fs::read_link(&slot).unwrap(), link_target(&two));
        // A writer also excludes link-to-link binders. The ordinary binder
        // returns without waiting while it holds another dependency's lock.
        let held = SlotWriter::acquire(&store, "r").unwrap();
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _dependent = SlotWriter::acquire(&store, "dependent").unwrap();
                    bind(&store, "r", &one).unwrap();
                })
                .join()
                .unwrap();
        });
        assert_eq!(fs::read_link(&slot).unwrap(), link_target(&two));
        drop(held);
        let readers = SlotReaders::acquire(&store, &["r".to_owned()]).unwrap();
        bind(&store, "r", &one).unwrap();
        assert_eq!(fs::read_link(&slot).unwrap(), link_target(&two));
        drop(readers);
        // A writer's directory in progress.
        let writer = SlotWriter::acquire(&store, "r").unwrap();
        writer.fresh().unwrap();
        fs::write(slot.join("partial"), b"x").unwrap();
        let other = std::thread::spawn({
            let store = store.clone();
            let one = one.clone();
            move || bind(&store, "r", &one)
        });
        other.join().unwrap().unwrap();
        assert!(slot.join("partial").is_file());
        // A complete mutable slot.
        writer.mark(&two).unwrap();
        drop(writer);
        bind(&store, "r", &one).unwrap();
        assert!(slot.join("partial").is_file());
        assert_eq!(marker(&store, "r"), Some(two.clone()));
        // An interrupted writer's leftover (no marker, free locks).
        remove_marker(&store, "r").unwrap();
        bind(&store, "r", &one).unwrap();
        assert_eq!(fs::read_link(&slot).unwrap(), link_target(&one));
        // The writer's own binding replaces its finished directory.
        let writer = SlotWriter::acquire(&store, "r").unwrap();
        writer.fresh().unwrap();
        fs::write(slot.join("done"), b"x").unwrap();
        writer.bind(&two).unwrap();
        assert_eq!(fs::read_link(&slot).unwrap(), link_target(&two));
        assert!(fs::read_dir(store.join(SLOTS)).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('.')
        }));
        assert!(bind(&store, "../escape", &one).is_err());
    }

    /// Slots an `execute` names in its arguments, environment or working
    /// directory: real names only, never scratch, `..` or another store.
    #[test]
    fn named_slots_are_found_in_arguments_and_environment() {
        let store = Path::new("/c/repositories");
        let texts: [&[u8]; 4] = [
            b"GOPATH=/c/repositories/external/gocache:/c/repositories/external/other/bin",
            b"/c/repositories/external/.bind-1-2 /c/repositories/external/../x",
            b"/d/repositories/external/elsewhere",
            b"/c/repositories/external/#long-0123",
        ];
        assert_eq!(
            named_slots(store, texts).into_iter().collect::<Vec<_>>(),
            ["gocache", "other"]
        );
    }

    /// A self-referencing repository's slot that is missing, a link or an
    /// interrupted writer's is made again from its tree, under the writer
    /// lock; a complete one is left alone.
    #[test]
    fn incomplete_mutable_slots_are_restored_from_their_trees() {
        let temp = temp();
        let store = temp.path().to_path_buf();
        fs::create_dir_all(store.join("direct/locks")).unwrap();
        let tree = store.join("direct/trees").join("a".repeat(64));
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("go.env"), b"GOPATH=x\n").unwrap();
        let identity = "a".repeat(64);
        let slot = slot_path(&store, "gocache");
        for disturb in [
            |slot: &Path| {
                let _ = crate::repository_ctx::remove_tree(slot);
            },
            |slot: &Path| {
                let _ = crate::repository_ctx::remove_tree(slot);
                std::os::unix::fs::symlink("../direct/trees/x", slot).unwrap();
            },
            |slot: &Path| {
                let _ = crate::repository_ctx::remove_tree(slot);
                fs::create_dir_all(slot.join("pkg")).unwrap();
            },
        ] {
            disturb(&slot);
            let _ = remove_marker(&store, "gocache");
            assert!(!complete(&store, "gocache"));
            restore_mutable(&store, "gocache", &identity, &tree, false).unwrap();
            assert!(complete(&store, "gocache"));
            assert_eq!(fs::read(slot.join("go.env")).unwrap(), b"GOPATH=x\n");
            assert!(!slot.join("pkg").exists());
        }
        // Complete: what consumers wrote stays.
        fs::create_dir_all(slot.join("pkg/mod")).unwrap();
        restore_mutable(&store, "gocache", &identity, &tree, false).unwrap();
        assert!(slot.join("pkg/mod").is_dir());
    }

    #[test]
    fn copies_are_fresh_single_link_files_with_modes() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let temp = temp();
        let from = temp.path().join("from");
        fs::create_dir_all(from.join("ro")).unwrap();
        fs::write(from.join("ro/f"), b"f").unwrap();
        fs::write(from.join("x"), b"x").unwrap();
        fs::set_permissions(from.join("x"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("../x", from.join("ro/link")).unwrap();
        fs::set_permissions(from.join("ro"), fs::Permissions::from_mode(0o555)).unwrap();
        let to = temp.path().join("to");
        copy_tree(&from, &to).unwrap();
        assert_eq!(fs::read(to.join("ro/f")).unwrap(), b"f");
        assert_eq!(
            fs::read_link(to.join("ro/link")).unwrap(),
            Path::new("../x")
        );
        assert_eq!(
            fs::metadata(to.join("x")).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(to.join("ro")).unwrap().permissions().mode() & 0o777,
            0o555
        );
        assert_eq!(fs::metadata(to.join("x")).unwrap().nlink(), 1);
        assert_ne!(
            fs::metadata(to.join("x")).unwrap().ino(),
            fs::metadata(from.join("x")).unwrap().ino()
        );
        crate::repository_ctx::remove_tree(temp.path()).unwrap();
    }

    #[test]
    fn paths_into_a_tree_are_found_whole_and_redirected() {
        let temp = tempfile::tempdir().unwrap();
        let tree = temp.path().join("trees/abc");
        fs::create_dir_all(tree.join("pkg")).unwrap();
        let root = tree.display().to_string();
        let texts = [format!("GOPATH={root}"), format!("x:{root}/pkg:y")];
        assert!(names_path(
            root.as_bytes(),
            texts.iter().map(|text| text.as_bytes())
        ));
        // A longer name is another path.
        assert!(!names_path(
            root.as_bytes(),
            std::iter::once(format!("{root}d/x").as_bytes())
        ));
        // Existing paths are no write target; a missing one below it is.
        assert!(!written_into(
            &tree,
            [root.clone(), format!("{root}/pkg"), format!("{root}/")]
                .iter()
                .map(|text| text.as_bytes())
        ));
        assert!(written_into(
            &tree,
            std::iter::once(format!("a:{root}/gocache:b").as_bytes())
        ));
        assert!(!written_into(
            &tree,
            std::iter::once(format!("{root}x/gocache").as_bytes())
        ));
        assert_eq!(
            redirected(
                format!("{root}/gocache:{root}:{root}x").as_bytes(),
                root.as_bytes(),
                b"/slot"
            )
            .unwrap(),
            format!("/slot/gocache:/slot:{root}x").into_bytes()
        );
        assert_eq!(redirected(b"/elsewhere", root.as_bytes(), b"/slot"), None);
    }
}
