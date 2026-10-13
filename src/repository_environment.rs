//! Lazy, bounded first-observation environment snapshot, not an atomic process
//! entry snapshot. Only requested names are read. Readers must not reenter this
//! service (including through another callback). No process-wide mutation occurs.
//! Hashes identify inputs; they are neither encryption nor a zeroization promise.

use crate::repository_budget::HostBudget;
use allocative::Allocative;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) const MAX_NAME: usize = 4096;
const MAX_VALUE: usize = 1024 * 1024;
const MAX_NAMES: usize = 10_000;
const MAX_CALLS: usize = 10_000;
const PROFILE: &str = "repository-environment-posix-ascii-v1";

/// Owned declaration metadata; never contains captured values.
#[derive(Clone, Default, Allocative, Serialize)]
#[serde(transparent)]
pub struct DeclaredEnvironment(Vec<String>);

impl fmt::Debug for DeclaredEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeclaredEnvironment(<redacted>)")
    }
}

impl DeclaredEnvironment {
    pub(crate) fn from_names<'a>(
        names: impl IntoIterator<Item = Result<&'a str, String>>,
        budget: &mut HostBudget,
    ) -> Result<Self, String> {
        let mut owned = Vec::new();
        for name in names {
            let name = name?;
            // Charge every occurrence before scanning/copying, including duplicates.
            validate_length(name).map_err(|e| e.to_string())?;
            budget.string(name)?;
            validate_ascii(name).map_err(|e| e.to_string())?;
            owned.push(name.to_owned());
        }
        owned.sort_unstable();
        owned.dedup();
        Ok(Self(owned))
    }

    pub(crate) fn names(&self) -> &[String] {
        &self.0
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn charge(&self, budget: &mut HostBudget) -> Result<(), String> {
        for name in &self.0 {
            budget.string(name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EnvError {
    NameLimit,
    NameEncoding,
    ValueLimit,
    ValueEncoding,
    Budget,
    Calls,
    Read,
}

impl fmt::Display for EnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NameLimit => "repository environment name exceeds 4096 bytes",
            Self::NameEncoding => "repository environment names must be ASCII",
            Self::ValueLimit => "repository environment value exceeds 1 MiB",
            Self::ValueEncoding => "repository environment values must be ASCII",
            Self::Budget => "repository environment admission or allocation limit exceeded",
            Self::Calls => "repository environment exceeds 10000 observations per callback",
            Self::Read => "repository environment read failed",
        })
    }
}
impl std::error::Error for EnvError {}

/// A fixed value-free failure, never an OS error carrying a name or value.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadError;

pub(crate) trait EnvironmentReader: Send + Sync {
    fn read(&self, name: &str) -> Result<Option<OsString>, ReadError>;
}

struct ProcessReader;
impl EnvironmentReader for ProcessReader {
    fn read(&self, name: &str) -> Result<Option<OsString>, ReadError> {
        Ok(std::env::var_os(name))
    }
}

pub(crate) enum Captured {
    Absent,
    Value(Arc<str>),
}
impl fmt::Debug for Captured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Captured(<redacted>)")
    }
}

pub(crate) struct Entry {
    name: Arc<str>,
    captured: OnceLock<Result<Captured, EnvError>>,
}
impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EnvironmentEntry(<redacted>)")
    }
}
impl Entry {
    pub(crate) fn captured(&self) -> Result<&Captured, EnvError> {
        self.captured
            .get()
            .expect("observed before watch insertion")
            .as_ref()
            .map_err(|e| *e)
    }
}

#[derive(Default)]
struct Retained {
    entries: BTreeMap<Arc<str>, Arc<Entry>>,
    budget: HostBudget,
    closed: bool,
}

pub(crate) struct Snapshot {
    retained: Mutex<Retained>,
    reader: Arc<dyn EnvironmentReader>,
}
impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EnvironmentSnapshot(<redacted>)")
    }
}
impl Default for Snapshot {
    fn default() -> Self {
        Self::with_reader(Arc::new(ProcessReader))
    }
}

fn validate_length(name: &str) -> Result<(), EnvError> {
    if name.len() > MAX_NAME {
        Err(EnvError::NameLimit)
    } else {
        Ok(())
    }
}
fn validate_ascii(name: &str) -> Result<(), EnvError> {
    if name.is_ascii() {
        Ok(())
    } else {
        Err(EnvError::NameEncoding)
    }
}

impl Snapshot {
    pub(crate) fn with_reader(reader: Arc<dyn EnvironmentReader>) -> Self {
        Self {
            retained: Mutex::new(Retained::default()),
            reader,
        }
    }

    fn observe(&self, name: &str) -> Result<Arc<Entry>, EnvError> {
        validate_length(name)?;
        // Caller reserves borrowed-name work before reaching this bounded scan.
        validate_ascii(name)?;
        let entry = {
            let mut retained = self.retained.lock().map_err(|_| EnvError::Read)?;
            if let Some(entry) = retained.entries.get(name) {
                entry.clone()
            } else {
                if retained.closed {
                    return Err(EnvError::Budget);
                }
                if retained.entries.len() >= MAX_NAMES
                    || retained
                        .budget
                        .transaction(|budget| {
                            budget.string(name)?;
                            budget.node()?;
                            budget.node()
                        })
                        .is_err()
                {
                    retained.closed = true;
                    return Err(EnvError::Budget);
                }
                let name: Arc<str> = Arc::from(name);
                let entry = Arc::new(Entry {
                    name: name.clone(),
                    captured: OnceLock::new(),
                });
                retained.entries.insert(name, entry.clone());
                entry
            }
        };
        // Never hold the map mutex across the reader or any Starlark evaluation.
        entry
            .captured
            .get_or_init(|| self.capture(&entry.name))
            .as_ref()
            .map_err(|e| *e)?;
        Ok(entry)
    }

    fn capture(&self, name: &str) -> Result<Captured, EnvError> {
        // Verified POSIX odd-name contract: no splitting, truncation or OS access.
        if name.is_empty() || name.bytes().any(|b| b == b'=' || b == 0) {
            return Ok(Captured::Absent);
        }
        // var_os necessarily allocates its initial OsString before we know its
        // size. All subsequent scanning/conversion/cache allocation is precharged.
        let Some(raw) = self.reader.read(name).map_err(|_| EnvError::Read)? else {
            return Ok(Captured::Absent);
        };
        let bytes = raw.as_encoded_bytes();
        if bytes.len() > MAX_VALUE {
            return Err(EnvError::ValueLimit);
        }
        {
            let mut retained = self.retained.lock().map_err(|_| EnvError::Read)?;
            if retained
                .budget
                .transaction(|budget| {
                    budget.node()?;
                    budget.bytes(bytes.len())
                })
                .is_err()
            {
                retained.closed = true;
                return Err(EnvError::Budget);
            }
        }
        if !bytes.is_ascii() {
            return Err(EnvError::ValueEncoding);
        }
        let text = raw.to_str().ok_or(EnvError::ValueEncoding)?;
        Ok(Captured::Value(Arc::from(text)))
    }
}

/// No raw values or per-key hashes are serialized in execution inputs.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct WatchedEnvironment {
    profile: &'static str,
    sha256: String,
}

#[derive(Default)]
pub(crate) struct Watch {
    entries: BTreeMap<Arc<str>, Arc<Entry>>,
    budget: HostBudget,
    calls: usize,
}
impl fmt::Debug for Watch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EnvironmentWatch(<redacted>)")
    }
}
impl Watch {
    pub(crate) fn observe(
        &mut self,
        snapshot: &Snapshot,
        name: &str,
    ) -> Result<Arc<Entry>, EnvError> {
        validate_length(name)?;
        if self.calls >= MAX_CALLS {
            return Err(EnvError::Calls);
        }
        self.calls += 1;
        self.budget
            .transaction(|budget| {
                budget.string(name)?;
                budget.node()
            })
            .map_err(|_| EnvError::Budget)?;
        let entry = snapshot.observe(name)?;
        if let Captured::Value(value) = entry.captured()? {
            // Reserve callback-local digest work even for declared-but-unread
            // captures, and charge repeated cached observations as well.
            self.budget
                .transaction(|budget| budget.bytes(value.len()))
                .map_err(|_| EnvError::Budget)?;
        }
        self.entries.insert(entry.name.clone(), entry.clone());
        Ok(entry)
    }

    pub(crate) fn charge_return(&mut self, text: &str) -> Result<(), EnvError> {
        self.budget
            .transaction(|budget| budget.string(text))
            .map_err(|_| EnvError::Budget)
    }

    pub(crate) fn inputs(&self) -> Result<Option<WatchedEnvironment>, EnvError> {
        if self.entries.is_empty() {
            return Ok(None);
        }
        let mut hash = Sha256::new();
        hash.update(PROFILE.as_bytes());
        hash.update([0]);
        hash.update((self.entries.len() as u64).to_be_bytes());
        for (name, entry) in &self.entries {
            hash.update((name.len() as u64).to_be_bytes());
            hash.update(name.as_bytes());
            match entry.captured()? {
                Captured::Absent => hash.update([0]),
                Captured::Value(value) => {
                    hash.update([1]);
                    hash.update((value.len() as u64).to_be_bytes());
                    hash.update(value.as_bytes());
                }
            }
        }
        Ok(Some(WatchedEnvironment {
            profile: PROFILE,
            sha256: format!("{:x}", hash.finalize()),
        }))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(crate) struct Reader {
        pub calls: AtomicUsize,
        pub value: Result<Option<OsString>, ReadError>,
    }
    impl Reader {
        pub(crate) fn new(value: Result<Option<OsString>, ReadError>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                value,
            })
        }
    }
    impl EnvironmentReader for Reader {
        fn read(&self, _: &str) -> Result<Option<OsString>, ReadError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.value.clone()
        }
    }

    #[test]
    fn once_for_present_absent_and_errors_concurrent() {
        for value in [
            Ok(Some(OsString::from("SyntheticSecret"))),
            Ok(None),
            Err(ReadError),
        ] {
            let reader = Reader::new(value);
            let snapshot = Arc::new(Snapshot::with_reader(reader.clone()));
            let barrier = Arc::new(std::sync::Barrier::new(8));
            std::thread::scope(|scope| {
                for _ in 0..8 {
                    let snapshot = snapshot.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        let _ = Watch::default().observe(&snapshot, "SYNTHETIC");
                    });
                }
            });
            assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
            assert!(!format!("{snapshot:?}").contains("SyntheticSecret"));
        }
    }

    #[test]
    fn odd_names_and_profile_errors_are_bounded_and_private() {
        let reader = Reader::new(Ok(Some(OsString::from("SyntheticSecret"))));
        let snapshot = Snapshot::with_reader(reader.clone());
        let mut watch = Watch::default();
        for name in ["", "=", "A=B", "A\0B"] {
            assert!(matches!(
                watch.observe(&snapshot, name).unwrap().captured().unwrap(),
                Captured::Absent
            ));
        }
        assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
        assert!(watch.observe(&snapshot, "?").is_ok());
        assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            watch.observe(&snapshot, "é").unwrap_err(),
            EnvError::NameEncoding
        );
        assert_eq!(
            watch
                .observe(&snapshot, &"x".repeat(MAX_NAME + 1))
                .unwrap_err(),
            EnvError::NameLimit
        );
        let inputs = serde_json::to_string(&watch.inputs().unwrap()).unwrap();
        assert!(!inputs.contains("SyntheticSecret"));
        assert!(!format!("{watch:?}").contains("SyntheticSecret"));
        for value in [
            OsString::from("SyntheticSecreté"),
            OsString::from("x".repeat(MAX_VALUE + 1)),
        ] {
            let reader = Reader::new(Ok(Some(value)));
            let snapshot = Snapshot::with_reader(reader.clone());
            for _ in 0..2 {
                let error = Watch::default()
                    .observe(&snapshot, "SYNTHETIC")
                    .unwrap_err();
                assert!(!format!("{error:?}: {error}").contains("SyntheticSecret"));
            }
            assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_is_cached_error_not_absence() {
        use std::os::unix::ffi::OsStringExt;
        let reader = Reader::new(Ok(Some(OsString::from_vec(
            b"SyntheticSecret\xff".to_vec(),
        ))));
        let snapshot = Snapshot::with_reader(reader.clone());
        for _ in 0..2 {
            assert_eq!(
                Watch::default()
                    .observe(&snapshot, "SYNTHETIC")
                    .unwrap_err(),
                EnvError::ValueEncoding
            );
        }
        assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn admission_latch_preserves_admitted_results_and_transaction_counters() {
        let reader = Reader::new(Ok(None));
        let snapshot = Snapshot::with_reader(reader.clone());
        snapshot.observe("KEPT").unwrap();
        {
            let mut retained = snapshot.retained.lock().unwrap();
            retained.budget.bytes(8 * 1024 * 1024 - 64).unwrap();
            let before = format!("{:?}", retained.budget);
            assert!(
                retained
                    .budget
                    .transaction(|b| {
                        b.node()?;
                        b.bytes(1000)
                    })
                    .is_err()
            );
            assert_eq!(before, format!("{:?}", retained.budget));
        }
        assert_eq!(snapshot.observe("NEW").unwrap_err(), EnvError::Budget);
        assert!(snapshot.observe("KEPT").is_ok());
        for _ in 0..100 {
            assert_eq!(snapshot.observe("OTHER").unwrap_err(), EnvError::Budget);
        }
        assert_eq!(snapshot.retained.lock().unwrap().entries.len(), 1);
        assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fixed_digest_vectors_frame_count_names_values_and_absence() {
        struct MapReader(BTreeMap<String, Option<OsString>>);
        impl EnvironmentReader for MapReader {
            fn read(&self, name: &str) -> Result<Option<OsString>, ReadError> {
                Ok(self.0[name].clone())
            }
        }
        for (values, expected) in [
            (
                vec![("A", None)],
                "4480356f287ee7fc245f67fa2a837581af1faf2a3cc1c30704e9dc61b71ec7dc",
            ),
            (
                vec![("A", Some(""))],
                "2d55fcca2be69d3e766a3ce31d4ac45a246ffe8ebf310c2a25eb104818fc1fd3",
            ),
            (
                vec![("A", Some("BC"))],
                "4e860934dcd1bdf912f6561c20d72302897dc25226c935b01ac1b511874c022f",
            ),
            (
                vec![("AB", Some("C"))],
                "7da01ab0e946af582b2c3b9642fb43c9a01ff13053c42136d7057c2d5d3770d9",
            ),
            (
                vec![("A", Some("x")), ("B", None)],
                "b04d19dc882821ae2acb07e9c414aa5df1f89041ca5211f24dc2daf1ce84966e",
            ),
        ] {
            let reader = MapReader(
                values
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.map(OsString::from)))
                    .collect(),
            );
            let snapshot = Snapshot::with_reader(Arc::new(reader));
            let mut watch = Watch::default();
            for (name, _) in values.iter().rev().chain(values.iter()) {
                watch.observe(&snapshot, name).unwrap();
            }
            assert_eq!(watch.inputs().unwrap().unwrap().sha256, expected);
        }
    }

    #[test]
    fn callback_watch_is_local_and_returns_are_separately_bounded() {
        let snapshot = Snapshot::with_reader(Reader::new(Ok(Some("SyntheticSecret".into()))));
        let mut first = Watch::default();
        first.observe(&snapshot, "A").unwrap();
        let before = first.inputs().unwrap();
        let mut second = Watch::default();
        second.observe(&snapshot, "B").unwrap();
        assert_eq!(first.inputs().unwrap(), before);
        assert_ne!(second.inputs().unwrap(), before);
        let returned = "x".repeat(MAX_VALUE);
        for _ in 0..7 {
            first.charge_return(&returned).unwrap();
        }
        assert_eq!(
            first.charge_return(&returned).unwrap_err(),
            EnvError::Budget
        );
    }

    #[test]
    fn distinct_name_limit_latches_without_new_cells_and_keeps_cached_errors() {
        let reader = Reader::new(Err(ReadError));
        let snapshot = Snapshot::with_reader(reader.clone());
        for i in 0..MAX_NAMES {
            assert_eq!(
                snapshot.observe(&format!("SYNTHETIC_{i}")).unwrap_err(),
                EnvError::Read
            );
        }
        assert_eq!(snapshot.observe("NEW").unwrap_err(), EnvError::Budget);
        assert_eq!(snapshot.observe("SYNTHETIC_0").unwrap_err(), EnvError::Read);
        assert_eq!(snapshot.retained.lock().unwrap().entries.len(), MAX_NAMES);
        assert_eq!(reader.calls.load(Ordering::SeqCst), MAX_NAMES);
    }

    #[test]
    fn reader_runs_without_map_lock_and_oversize_values_are_not_converted_or_reserved() {
        struct LockCheckingReader(Mutex<std::sync::Weak<Snapshot>>);
        impl EnvironmentReader for LockCheckingReader {
            fn read(&self, _: &str) -> Result<Option<OsString>, ReadError> {
                let snapshot = self.0.lock().unwrap().upgrade().unwrap();
                assert!(snapshot.retained.try_lock().is_ok());
                Ok(Some(OsString::from("x".repeat(MAX_VALUE + 1))))
            }
        }
        let reader = Arc::new(LockCheckingReader(Mutex::new(std::sync::Weak::new())));
        let snapshot = Arc::new(Snapshot::with_reader(reader.clone()));
        *reader.0.lock().unwrap() = Arc::downgrade(&snapshot);
        assert_eq!(
            snapshot.observe("SYNTHETIC").unwrap_err(),
            EnvError::ValueLimit
        );
        let retained = snapshot.retained.lock().unwrap();
        let mut expected = HostBudget::default();
        expected.string("SYNTHETIC").unwrap();
        expected.node().unwrap();
        expected.node().unwrap();
        assert_eq!(format!("{:?}", retained.budget), format!("{expected:?}"));
    }

    #[test]
    fn repeated_cached_reads_are_callback_bounded() {
        let reader = Reader::new(Ok(None));
        let snapshot = Snapshot::with_reader(reader.clone());
        let mut watch = Watch::default();
        for _ in 0..MAX_CALLS {
            watch.observe(&snapshot, "SYNTHETIC").unwrap();
        }
        assert_eq!(
            watch.observe(&snapshot, "SYNTHETIC").unwrap_err(),
            EnvError::Calls
        );
        assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
    }
}
