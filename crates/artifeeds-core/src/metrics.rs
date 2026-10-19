//! Lock-free counters plus a tiny latency histogram.
//!
//! All hot-path updates are relaxed atomics; sampling happens once per snapshot
//! build (a few times per second at most), so the ingestion path never waits on
//! the UI and the UI never waits on the ingestion path.

use crate::model::Stats;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const HIST_BUCKETS: usize = 24;
/// Upper bounds (ms) of the latency histogram buckets.
const HIST_BOUNDS: [u32; HIST_BUCKETS] = [
    5, 10, 20, 35, 50, 75, 100, 150, 200, 300, 400, 500, 700, 900, 1_200, 1_600, 2_000, 3_000,
    4_000, 6_000, 8_000, 12_000, 20_000, u32::MAX,
];

#[derive(Debug, Default)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub responses_304: AtomicU64,
    pub responses_200: AtomicU64,
    pub errors: AtomicU64,
    pub items_new: AtomicU64,
    pub items_seen: AtomicU64,
    pub dupes_dropped: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_saved_304: AtomicU64,
    pub in_flight: AtomicU64,
    pub config_reloads: AtomicU64,
    hist: [AtomicU32; HIST_BUCKETS],
    hist_total: AtomicU64,
    started: AtomicU64,
    last_batch_ms: AtomicU32,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let m = Arc::new(Self::default());
        m.started.store(crate::config::now_unix() as u64, Ordering::Relaxed);
        m
    }

    #[inline]
    pub fn record_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_304(&self, approx_bytes_saved: u64) {
        self.responses_304.fetch_add(1, Ordering::Relaxed);
        if approx_bytes_saved > 0 {
            self.bytes_saved_304.fetch_add(approx_bytes_saved, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn record_200(&self, bytes: u64) {
        self.responses_200.fetch_add(1, Ordering::Relaxed);
        self.bytes_in.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_items(&self, seen: u64, new: u64, dupes: u64) {
        self.items_seen.fetch_add(seen, Ordering::Relaxed);
        self.items_new.fetch_add(new, Ordering::Relaxed);
        self.dupes_dropped.fetch_add(dupes, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_latency(&self, ms: u32) {
        let idx = HIST_BOUNDS.partition_point(|b| *b < ms);
        self.hist[idx.min(HIST_BUCKETS - 1)].fetch_add(1, Ordering::Relaxed);
        self.hist_total.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn set_in_flight(&self, n: u64) {
        self.in_flight.store(n, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_batch(&self, ms: u32) {
        self.last_batch_ms.store(ms, Ordering::Relaxed);
    }

    /// Resident set size in KiB.
    ///
    /// On macOS `TASK_BASIC_INFO.resident_size` is unreliable on modern
    /// releases (it reports absurd values for 64-bit processes), so this reads
    /// `TASK_VM_INFO.phys_footprint`, which is the number the system itself
    /// uses for memory pressure. Linux reads `/proc/self/statm`.
    pub fn rss_kib(&self) -> u64 {
        #[cfg(target_os = "macos")]
        {
            let bytes = macos_footprint_bytes();
            if bytes > 0 {
                return bytes / 1024;
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Ok(s) = std::fs::read_to_string("/proc/self/statm")
                && let Some(pages) = s.split_whitespace().nth(1)
                && let Ok(pages) = pages.parse::<u64>()
            {
                return pages * 4;
            }
        }
        0
    }

    fn percentile(&self, want: f64) -> u32 {
        let total = self.hist_total.load(Ordering::Relaxed);
        if total == 0 {
            return 0;
        }
        let target = (total as f64 * want) as u64;
        let mut acc = 0u64;
        for (i, bucket) in self.hist.iter().enumerate() {
            acc += bucket.load(Ordering::Relaxed) as u64;
            if acc >= target {
                return HIST_BOUNDS[i].min(60_000);
            }
        }
        HIST_BOUNDS[HIST_BUCKETS - 1].min(60_000)
    }

    pub fn avg_latency(&self) -> u32 {
        let total = self.hist_total.load(Ordering::Relaxed);
        if total == 0 {
            return 0;
        }
        let mut sum = 0u64;
        for (i, bucket) in self.hist.iter().enumerate() {
            sum += bucket.load(Ordering::Relaxed) as u64 * HIST_BOUNDS[i].min(60_000) as u64;
        }
        (sum / total) as u32
    }

    /// Materialise the immutable subset the UI displays.
    pub fn snapshot_stats(&self) -> Stats {
        let started = self.started.load(Ordering::Relaxed);
        Stats {
            requests: self.requests.load(Ordering::Relaxed),
            responses_304: self.responses_304.load(Ordering::Relaxed),
            responses_200: self.responses_200.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            items_new: self.items_new.load(Ordering::Relaxed),
            items_seen: self.items_seen.load(Ordering::Relaxed),
            dupes_dropped: self.dupes_dropped.load(Ordering::Relaxed),
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            bytes_saved_304: self.bytes_saved_304.load(Ordering::Relaxed),
            in_flight: self.in_flight.load(Ordering::Relaxed),
            avg_latency_ms: self.avg_latency(),
            p95_latency_ms: self.percentile(0.95),
            last_batch_ms: self.last_batch_ms.load(Ordering::Relaxed),
            uptime_secs: (crate::config::now_unix() as u64).saturating_sub(started),
            rss_kib: self.rss_kib(),
            ..Default::default()
        }
    }
}

/// Byte offsets inside `task_vm_info_data_t` (see `<mach/task_info.h>`), in
/// units of `u64`.
#[cfg(target_os = "macos")]
const VM_INFO_RESIDENT_SIZE: usize = 2;
#[cfg(target_os = "macos")]
const VM_INFO_PHYS_FOOTPRINT: usize = 18;

/// Ask the kernel for this task's memory footprint.
///
/// `TASK_VM_INFO` is flavour 22 and needs a `count` of at least 38 `natural_t`
/// units to reach `phys_footprint`; the buffer is oversized and the count is
/// what the header's `TASK_VM_INFO_REV0_COUNT` implies.
#[cfg(target_os = "macos")]
fn macos_footprint_bytes() -> u64 {
    const TASK_VM_INFO: u32 = 22;
    const COUNT: u32 = 48;

    unsafe extern "C" {
        fn mach_task_self() -> u32;
        fn task_info(
            target_task: u32,
            flavor: u32,
            task_info_out: *mut i32,
            task_info_count: *mut u32,
        ) -> i32;
    }

    let mut buf = [0u64; 32];
    let mut count = COUNT;
    let kr = unsafe {
        task_info(
            mach_task_self(),
            TASK_VM_INFO,
            buf.as_mut_ptr().cast::<i32>(),
            &mut count,
        )
    };
    if kr != 0 || (count as usize) < VM_INFO_PHYS_FOOTPRINT {
        return 0;
    }
    let footprint = buf[VM_INFO_PHYS_FOOTPRINT];
    if footprint > 0 {
        footprint
    } else {
        buf[VM_INFO_RESIDENT_SIZE]
    }
}

pub fn num_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}
