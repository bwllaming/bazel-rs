//! Allocation accounting is isolated to this test executable, never production.

use std::io::{self, Write};
use std::time::Instant;

use bazel_rs::cquery_proto::*;

// Safety: this allocator transparently forwards the identical Layout/pointer to
// System. TLS counters neither allocate nor inspect allocated memory. Restrict
// the unsafe allowance to the allocator shim required by std::alloc's API.
#[allow(unsafe_code)]
mod allocations {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static ENABLED: Cell<bool> = const { Cell::new(false) };
        static COUNT: Cell<usize> = const { Cell::new(0) };
    }

    struct CountingAllocator;

    fn increment() {
        let _ = ENABLED.try_with(|enabled| {
            if enabled.get() {
                let _ = COUNT.try_with(|count| count.set(count.get() + 1));
            }
        });
    }

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            increment();
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            increment();
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            increment();
            unsafe { System.realloc(ptr, layout, size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    pub fn begin() {
        COUNT.set(0);
        ENABLED.set(true);
    }

    pub fn end() -> usize {
        ENABLED.set(false);
        COUNT.get()
    }
}

#[derive(Default)]
struct CountingSink(u64);

impl Write for CountingSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn serialization_allocates_zero_times_after_largest_record_and_is_result_count_bounded() {
    let count = std::env::var("CQUERY_STREAMING_RECORDS")
        .ok()
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(2_000);
    let target = Target::Rule(Rule {
        name: "//synthetic:large".into(),
        rule_class: "synthetic".into(),
        attributes: vec![
            Attribute::new("text", AttributeValue::String("x".repeat(4096)), true),
            Attribute::new(
                "labels",
                AttributeValue::LabelList(
                    (0..128)
                        .map(|i| format!("//synthetic/package:dependency_{i}"))
                        .collect(),
                ),
                true,
            ),
        ],
        ..Rule::default()
    });
    let config = Configuration {
        id: 1,
        checksum: "a".repeat(64),
        ..Configuration::default()
    };
    for format in [OutputFormat::Proto, OutputFormat::StreamedProto] {
        let mut writer =
            CqueryWriter::new(CountingSink::default(), format, WriterOptions::default());
        writer.write_target(&target, Some(&config)).unwrap();
        let capacity = writer.scratch_capacity();
        let start = Instant::now();
        allocations::begin();
        for _ in 0..count {
            writer.write_target(&target, Some(&config)).unwrap();
        }
        writer.write_configuration(&config).unwrap();
        let retained_capacity = writer.scratch_capacity();
        let sink = writer.finish().unwrap();
        let allocation_count = allocations::end();
        assert_eq!(allocation_count, 0, "unexpected per-record allocation");
        assert_eq!(retained_capacity, capacity);
        assert!(capacity < 32 * 1024);
        assert!(sink.0 > count as u64 * 8000);
        eprintln!(
            "{format:?}: records={}, bytes={}, scratch_capacity={}, fixed_io_buffer=8192, allocations={}, wall={:.3}s",
            count + 1,
            sink.0,
            capacity,
            allocation_count,
            start.elapsed().as_secs_f64(),
        );
    }
}
