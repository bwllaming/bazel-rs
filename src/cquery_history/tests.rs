use super::*;
use crate::observed_fs::{Observation, Op, Recording};

fn key(n: u8) -> Key {
    Key {
        g: [n; 32],
        binary: [7; 32],
    }
}

fn inputs(n: u8, metadata: u64) -> Vec<u8> {
    format::encode_inputs(
        &Recording {
            window_start_ns: metadata as i64,
            observations: vec![(
                PathBuf::from("/fixture/.bazelignore"),
                Observation {
                    op: Op::Read,
                    outcome: Outcome::Read {
                        sha256: [n; 32],
                        key: StatKey {
                            dev: 1,
                            ino: metadata,
                            size: 1,
                            mtime_ns: metadata as i64,
                            ctime_ns: metadata as i64,
                        },
                        racy: metadata % 2 == 1,
                    },
                },
            )],
            ..Recording::default()
        },
        &[],
        None,
        false,
    )
}

fn put(
    cache: &Path,
    key: &Key,
    n: u8,
    metadata: u64,
    verified: Option<&store::Reader>,
) -> Result<(), String> {
    save(
        cache,
        key,
        inputs(n, metadata),
        format::encode_events(b"note"),
        format::compress_plan(&[n]),
        verified,
    )
}

fn values(cache: &Path, key: &Key) -> Vec<u8> {
    let mut values: Vec<_> = candidates(cache, key)
        .into_iter()
        .filter_map(Result::ok)
        .map(|r| section(&r, Section::Plan).unwrap()[0])
        .collect();
    values.sort();
    values
}

#[test]
fn two_versions_deduplicate_metadata_and_evict_lru() {
    let cache = tempfile::tempdir().unwrap();
    let c = cache.path();
    let k = key(1);
    put(c, &k, 1, 1, None).unwrap();
    put(c, &k, 2, 2, None).unwrap();
    // A third analysis of A has new checkout metadata, not a third version.
    put(c, &k, 1, 30, None).unwrap();
    assert_eq!(values(c, &k), [1, 2]);
    let b = candidates(c, &k)
        .into_iter()
        .filter_map(Result::ok)
        .find(|r| section(r, Section::Plan).unwrap() == [2])
        .unwrap();
    File::open(&b.path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
    put(c, &k, 3, 3, None).unwrap();
    assert_eq!(values(c, &k), [1, 3]);
    assert_eq!(
        candidates(c, &k)
            .into_iter()
            .next()
            .unwrap()
            .unwrap()
            .header
            .slot(Section::Plan)
            .unwrap()
            .checksum,
        format::checksum(&format::compress_plan(&[3]))
    );
}

#[test]
fn equal_graphs_with_different_observations_remain_distinct() {
    let cache = tempfile::tempdir().unwrap();
    for n in [1, 2, 1] {
        save(
            cache.path(),
            &key(1),
            inputs(n, 1),
            format::encode_events(b""),
            format::compress_plan(b"same graph"),
            None,
        )
        .unwrap();
    }
    let readers = candidates(cache.path(), &key(1));
    assert_eq!(readers.len(), 2);
    let versions: Vec<_> = readers
        .into_iter()
        .map(|r| section(&r.unwrap(), Section::Stats).unwrap())
        .collect();
    assert_ne!(versions[0], versions[1]);
}

#[test]
fn verify_with_equal_graph_and_changed_inputs_cannot_duplicate_the_other_version() {
    let cache = tempfile::tempdir().unwrap();
    let c = cache.path();
    let put = |n, verified| {
        save(
            c,
            &key(1),
            inputs(n, 1),
            format::encode_events(b""),
            format::compress_plan(b"same graph"),
            verified,
        )
    };
    put(1, None).unwrap();
    let selected = open(c, &key(1), 0).unwrap().unwrap();
    put(2, None).unwrap();
    let before: Vec<_> = (0..2)
        .map(|slot| section(&open(c, &key(1), slot).unwrap().unwrap(), Section::Stats).unwrap())
        .collect();
    put(2, Some(&selected)).unwrap();
    let after: Vec<_> = (0..2)
        .map(|slot| section(&open(c, &key(1), slot).unwrap().unwrap(), Section::Stats).unwrap())
        .collect();
    assert_eq!(before, after);
    assert_ne!(after[0], after[1]);
}

#[test]
fn pinned_reader_and_verification_cannot_replace_or_remove_a_new_generation() {
    let cache = tempfile::tempdir().unwrap();
    let c = cache.path();
    let k = key(1);
    put(c, &k, 1, 1, None).unwrap();
    let old = open(c, &k, 0).unwrap().unwrap();
    put(c, &k, 1, 2, None).unwrap();
    assert_eq!(section(&old, Section::Plan).unwrap(), [1]);
    assert_ne!(old.identity(), open(c, &k, 0).unwrap().unwrap().identity());
    assert!(
        put(c, &k, 9, 3, Some(&old))
            .unwrap_err()
            .contains("generation")
    );
    remove(c, &old).unwrap();
    assert_eq!(values(c, &k), [1]);
    let selected = open(c, &k, 0).unwrap().unwrap();
    put(c, &k, 1, 4, Some(&selected)).unwrap();
    put(c, &k, 2, 5, None).unwrap();
    assert_eq!(values(c, &k), [1, 2]);
    remove(c, &selected).unwrap(); // successful verify refreshed the generation
    assert_eq!(values(c, &k), [1, 2]);
    let selected = open(c, &k, 0).unwrap().unwrap();
    remove(c, &selected).unwrap();
    assert_eq!(section(&selected, Section::Plan).unwrap(), [1]);
    assert!(put(c, &k, 1, 6, Some(&selected)).is_err());
    assert_eq!(values(c, &k), [2]);
}

#[test]
fn concurrent_same_and_different_versions_keep_two_atomic_generations() {
    let cache = tempfile::tempdir().unwrap();
    std::thread::scope(|scope| {
        for i in 0..8 {
            let c = cache.path();
            scope.spawn(move || {
                for m in 0..3 {
                    put(c, &key(1), i % 2 + 1, u64::from(i) + m, None).unwrap();
                }
            });
        }
    });
    assert_eq!(values(cache.path(), &key(1)), [1, 2]);
    assert_eq!(
        fs::read_dir(store::directory(cache.path()))
            .unwrap()
            .count(),
        2
    );
}

#[test]
fn graph_slots_share_the_build_count_budget_and_keys_and_elf_stay_separate() {
    let cache = tempfile::tempdir().unwrap();
    let c = cache.path();
    put(c, &key(1), 1, 1, None).unwrap();
    put(c, &key(1), 2, 2, None).unwrap();
    assert!(candidates(c, &key(2)).is_empty());
    assert!(
        candidates(
            c,
            &Key {
                binary: [9; 32],
                ..key(1)
            }
        )
        .is_empty()
    );
    // A short filename collision never bypasses the full original G check.
    let mut collision = key(1);
    collision.g[31] = 2;
    assert!(candidates(c, &collision).iter().all(Result::is_err));
    // Do not depend on the filesystem clock's timestamp granularity when
    // asserting which entry the shared GC evicts.
    for slot in 0..2 {
        File::open(path(c, &key(1), slot))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(slot as u64 + 1))
            .unwrap();
    }
    for n in 2..=(store::KEEP as u8) {
        store::write_in_background(c, &key(n), vec![(Section::Plan, b"ordinary".to_vec())])
            .commit()
            .unwrap();
    }
    assert_eq!(
        fs::read_dir(store::directory(c)).unwrap().count(),
        store::KEEP
    );
    assert_eq!(candidates(c, &key(1)).len(), 1);
    assert!(store::collect_garbage(c).is_empty());
}

#[test]
fn malformed_frames_are_rejected_in_preflight_without_large_allocation() {
    let frame = |count: u32, raw: u32, len: u32| {
        let mut v = format::COMPRESSED_MAGIC.to_vec();
        v.extend(count.to_le_bytes());
        v.extend(raw.to_le_bytes());
        v.extend(len.to_le_bytes());
        v
    };
    for v in [
        vec![],
        frame(u32::MAX, 0, 0),
        frame(1, u32::MAX, 0),
        frame(1, 1, u32::MAX),
        frame(2, 4 << 20, 0),
        frame(1, 0, 0),
    ] {
        assert!(format::decompress_plan_bounded(v, MAX_RAW).is_err());
    }
    let mut aggregate = format::COMPRESSED_MAGIC.to_vec();
    aggregate.extend(2u32.to_le_bytes());
    for _ in 0..2 {
        aggregate.extend((4u32 << 20).to_le_bytes());
        aggregate.extend(0u32.to_le_bytes());
    }
    assert!(format::decompress_plan_bounded(aggregate, 5 << 20).is_err());
    let compressed = format::compress_plan(&vec![3; (4 << 20) + 7]);
    assert!(format::check_compressed_bounds(&compressed, 4 << 20).is_err());
    assert_eq!(
        format::decompress_plan_bounded(compressed, MAX_RAW).unwrap(),
        vec![3; (4 << 20) + 7]
    );
}

#[test]
fn oversized_sparse_entries_and_sections_truncation_and_failed_save_preserve_other_slot() {
    let cache = tempfile::tempdir().unwrap();
    let c = cache.path();
    let k = key(1);
    put(c, &k, 1, 1, None).unwrap();
    put(c, &k, 2, 2, None).unwrap();
    let reader = open(c, &k, 1).unwrap().unwrap();
    let file = File::options().write(true).open(&reader.path).unwrap();
    file.set_len(MAX_ENTRY + 1).unwrap();
    assert!(open(c, &k, 1).unwrap_err().contains("size limit"));
    let mut header = reader.header.clone();
    header.slots[0].len = MAX_INPUTS + 1;
    file.set_len(MAX_ENTRY).unwrap();
    use std::os::unix::fs::FileExt;
    file.write_all_at(&header.encode(), 0).unwrap();
    assert!(open(c, &k, 1).unwrap_err().contains("section"));
    file.set_len(10).unwrap();
    assert!(open(c, &k, 1).is_err());
    assert!(save(c, &k, inputs(3, 3), vec![], vec![], None).is_err());
    assert_eq!(
        section(&open(c, &k, 0).unwrap().unwrap(), Section::Plan).unwrap(),
        [1]
    );
    fs::remove_file(reader.path.clone()).unwrap();
    assert_eq!(values(c, &k), [1]);
    assert_eq!(fs::read_dir(store::directory(c)).unwrap().count(), 1);
}
