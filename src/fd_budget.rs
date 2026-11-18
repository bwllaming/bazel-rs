//! Descriptor robustness of action execution.
//!
//! bazel-rs raises its soft `RLIMIT_NOFILE` to the hard limit at startup
//! like Bazel (crate::resource_limits), and a running local action holds two
//! descriptors of this process (its supervisor's control socket and its
//! action-cache lock; Bazel's server holds none). A hard limit that is small
//! for `--jobs` could still exhaust the table, so:
//!
//! - [`action_slot`] caps the actions between taking their action-cache lock
//!   and publishing their outputs to what the limit affords
//!   ([`FDS_PER_ACTION`] each, after the reader cap and [`RESERVED_FDS`]);
//!   with a raised limit (typically hundreds of thousands) the cap is far
//!   above any `--jobs` and never waits;
//! - [`retry`] retries an operation that failed with `EMFILE`/`ENFILE` a few
//!   times after short backoffs instead of failing the action at once
//!   (Bazel would fail it; the operations retried here have no effect when
//!   they fail: an open, a spawn whose command did not start);
//! - [`open_reader`] and [`read`] take a permit of a process-wide cap on
//!   concurrently open output-file readers (hashing, uploads), a quarter of
//!   the soft limit (at least [`MIN_READERS`]), so readers queue instead of
//!   competing with spawns for the last descriptors.

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

/// Backoffs between attempts of [`retry`] (about 3.9 s in total): long
/// enough for running actions to finish and release their descriptors.
pub const RETRY_DELAYS_MS: [u64; 8] = [10, 20, 50, 100, 200, 500, 1000, 2000];

/// The smallest reader cap, whatever the limit.
pub const MIN_READERS: usize = 8;

/// Descriptors of this process an executing action may hold at once: its
/// lock, its supervisor's control socket, and transiently its two logs (or
/// the spawn's internal pipe).
pub const FDS_PER_ACTION: u64 = 4;

/// Descriptors left to everything else (stdio, the progress display, the
/// remote cache's connections beyond the readers, the analysis snapshot).
pub const RESERVED_FDS: u64 = 64;

/// Whether `error` is a descriptor table exhaustion (the process's EMFILE
/// or the system's ENFILE).
pub fn exhausted(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(24 | 23))
}

/// `operation`, retried after each of [`RETRY_DELAYS_MS`] while it fails
/// with [`exhausted`]; any other result is returned at once.
pub fn retry<T>(operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    retry_with(operation, std::thread::sleep)
}

/// [`retry`] with an injected backoff (deterministic tests).
pub fn retry_with<T>(
    mut operation: impl FnMut() -> io::Result<T>,
    mut backoff: impl FnMut(Duration),
) -> io::Result<T> {
    let mut delays = RETRY_DELAYS_MS.into_iter();
    loop {
        match operation() {
            Err(error) if exhausted(&error) => {
                let Some(delay) = delays.next() else {
                    return Err(error);
                };
                backoff(Duration::from_millis(delay));
            }
            result => return result,
        }
    }
}

/// A counting semaphore (no permits needed when `max` is `None`).
struct Limit {
    max: Option<usize>,
    used: Mutex<usize>,
    freed: Condvar,
}

impl Limit {
    fn new(max: Option<usize>) -> Self {
        Self {
            max,
            used: Mutex::new(0),
            freed: Condvar::new(),
        }
    }

    fn acquire(&self) -> Permit<'_> {
        let Some(max) = self.max else {
            return Permit(None);
        };
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        while *used >= max {
            used = self.freed.wait(used).unwrap_or_else(|e| e.into_inner());
        }
        *used += 1;
        Permit(Some(self))
    }
}

struct Permit<'a>(Option<&'a Limit>);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if let Some(limit) = self.0 {
            let mut used = limit.used.lock().unwrap_or_else(|e| e.into_inner());
            *used -= 1;
            limit.freed.notify_one();
        }
    }
}

/// The reader cap for a soft limit (`None`: unlimited).
fn reader_cap(soft: Option<u64>) -> Option<usize> {
    let soft = soft?;
    let quarter = usize::try_from(soft / 4).unwrap_or(usize::MAX);
    Some(quarter.max(MIN_READERS))
}

/// The cap on directory descriptors staging holds ([`try_directory`]) for
/// a soft limit: an eighth of it (`None`: unlimited).
fn directory_cap(soft: Option<u64>) -> Option<usize> {
    soft.map(|soft| usize::try_from(soft / 8).unwrap_or(usize::MAX))
}

/// The action slot cap for a soft limit (`None`: unlimited).
fn action_cap(soft: Option<u64>) -> Option<usize> {
    let soft = soft?;
    let readers = reader_cap(Some(soft)).map_or(0, |cap| cap as u64);
    let directories = directory_cap(Some(soft)).map_or(0, |cap| cap as u64);
    let left = soft.saturating_sub(readers + directories + RESERVED_FDS);
    Some(
        usize::try_from(left / FDS_PER_ACTION)
            .unwrap_or(usize::MAX)
            .max(1),
    )
}

fn soft_limit() -> Option<u64> {
    rustix::process::getrlimit(rustix::process::Resource::Nofile).current
}

/// A slot of the executing actions' cap, held until dropped.
pub struct ActionSlot {
    _permit: Permit<'static>,
}

/// Waits for a slot of the executing actions' cap (see the module).
pub fn action_slot() -> ActionSlot {
    static SLOTS: OnceLock<Limit> = OnceLock::new();
    ActionSlot {
        _permit: SLOTS
            .get_or_init(|| Limit::new(action_cap(soft_limit())))
            .acquire(),
    }
}

fn readers() -> &'static Limit {
    static READERS: OnceLock<Limit> = OnceLock::new();
    READERS.get_or_init(|| Limit::new(reader_cap(soft_limit())))
}

/// An output file open for reading, holding a reader permit until dropped.
pub struct Reader {
    file: File,
    _permit: Permit<'static>,
}

impl std::ops::Deref for Reader {
    type Target = File;
    fn deref(&self) -> &File {
        &self.file
    }
}

impl std::ops::DerefMut for Reader {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

/// Opens `path` for reading under the reader cap, retried on exhaustion.
pub fn open_reader(path: &Path) -> io::Result<Reader> {
    let permit = readers().acquire();
    let file = retry(|| File::open(path))?;
    Ok(Reader {
        file,
        _permit: permit,
    })
}

/// `fs::read` under the reader cap, retried on exhaustion.
pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    let _permit = readers().acquire();
    retry(|| std::fs::read(path))
}

/// A descriptor of a directory that action staging keeps open while it
/// inspects the files in it (crate::action, `tree_file_metadata`): one of at
/// most [`directory_cap`], counted without a lock (thousands per action).
pub struct DirectoryPermit(());

static DIRECTORIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A directory descriptor permit, or `None` when the cap is reached (the
/// caller then works by path, holding no descriptor).
pub fn try_directory() -> Option<DirectoryPermit> {
    use std::sync::atomic::Ordering;
    static CAP: OnceLock<Option<usize>> = OnceLock::new();
    let cap = *CAP.get_or_init(|| directory_cap(soft_limit()));
    let before = DIRECTORIES.fetch_add(1, Ordering::Relaxed);
    if cap.is_some_and(|cap| before >= cap) {
        DIRECTORIES.fetch_sub(1, Ordering::Relaxed);
        return None;
    }
    Some(DirectoryPermit(()))
}

impl Drop for DirectoryPermit {
    fn drop(&mut self) {
        DIRECTORIES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn retry_backs_off_on_exhaustion_only_and_is_bounded() {
        for errno in [24, 23] {
            let mut calls = 0;
            let mut delays = Vec::new();
            let error = retry_with::<()>(
                || {
                    calls += 1;
                    Err(io::Error::from_raw_os_error(errno))
                },
                |delay| delays.push(delay.as_millis() as u64),
            )
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
            assert_eq!(calls, RETRY_DELAYS_MS.len() + 1);
            assert_eq!(delays, RETRY_DELAYS_MS);
        }
        let mut calls = 0;
        let value = retry_with(
            || {
                calls += 1;
                if calls < 3 {
                    Err(io::Error::from_raw_os_error(24))
                } else {
                    Ok(7)
                }
            },
            |_| {},
        )
        .unwrap();
        assert_eq!((value, calls), (7, 3));
        for errno in [2, 13, 26] {
            let error = retry_with::<()>(
                || Err(io::Error::from_raw_os_error(errno)),
                |_| panic!("no backoff for errno {errno}"),
            )
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
        }
    }

    #[test]
    fn reader_cap_is_a_quarter_of_the_soft_limit() {
        assert_eq!(reader_cap(None), None);
        assert_eq!(reader_cap(Some(1024)), Some(256));
        assert_eq!(reader_cap(Some(16)), Some(MIN_READERS));
    }

    #[test]
    fn action_cap_leaves_room_for_readers_and_the_rest() {
        assert_eq!(action_cap(None), None);
        // 256 - 64 readers - 32 directories - 64 reserved = 96 descriptors:
        // 24 actions.
        assert_eq!(action_cap(Some(256)), Some(24));
        // 1024 - 256 - 128 - 64 = 576: 144 actions.
        assert_eq!(action_cap(Some(1024)), Some(144));
        assert_eq!(action_cap(Some(524_288)), Some(81_904));
        assert_eq!(action_cap(Some(16)), Some(1));
    }

    #[test]
    fn directory_permits_stop_at_the_cap() {
        assert_eq!(directory_cap(None), None);
        assert_eq!(directory_cap(Some(1024)), Some(128));
    }

    #[test]
    fn limit_caps_concurrent_permits() {
        let limit = Limit::new(Some(2));
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let _permit = limit.acquire();
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(*limit.used.lock().unwrap(), 0);
    }
}
