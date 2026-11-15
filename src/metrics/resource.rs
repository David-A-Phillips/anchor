//! Process resource sampling: CPU time, memory and thread count of the daemon itself.
//!
//! Sampling is cheap by construction. On Linux one sample is a `getrusage` call plus one read of
//! `/proc/self/status` (a few microseconds, no allocation beyond the file contents); elsewhere
//! `sysinfo` refreshes this one process. Nothing here polls: the optional periodic sampler is a
//! coarse Tokio timer (never faster than once a second).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::base::{Error, Result};

/// Shortest interval the periodic sampler accepts; shorter requests are raised to this.
pub const MIN_SAMPLER_INTERVAL: Duration = Duration::from_secs(1);

/// Resource usage of this process at one instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessSample {
    /// When the sample was taken (monotonic clock).
    pub taken_at: Instant,
    /// CPU time consumed so far, user plus system, summed over all threads.
    pub cpu_time: Duration,
    /// Resident set size in bytes.
    pub resident_bytes: u64,
    /// Virtual memory size in bytes.
    pub virtual_bytes: u64,
    /// Number of threads; 0 when the platform does not report it.
    pub threads: u32,
}

impl ProcessSample {
    /// Samples the current process.
    pub fn now() -> Result<Self> {
        if cfg!(target_os = "linux") {
            sample_procfs()
        } else {
            sample_sysinfo()
        }
    }
}

/// CPU usage between two samples as a percentage of one core (a process keeping two cores
/// busy reports 200). Returns 0 when the samples are out of order or taken at the same instant.
pub fn cpu_percent(earlier: &ProcessSample, later: &ProcessSample) -> f64 {
    let wall = later.taken_at.saturating_duration_since(earlier.taken_at);
    if wall.is_zero() {
        return 0.0;
    }
    let cpu = later.cpu_time.saturating_sub(earlier.cpu_time);
    cpu.as_secs_f64() / wall.as_secs_f64() * 100.0
}

/// CPU time (user plus system) consumed by the **calling thread**. Lets a performance test
/// assert that one thread stayed idle regardless of what the rest of the process is doing.
/// Only available on Linux, FreeBSD and OpenBSD; an error elsewhere.
pub fn thread_cpu_time() -> Result<Duration> {
    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
    {
        rusage_cpu_time(nix::sys::resource::UsageWho::RUSAGE_THREAD)
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd")))]
    {
        Err(Error::other(
            "per-thread CPU time is not available on this platform",
        ))
    }
}

#[cfg(unix)]
fn rusage_cpu_time(who: nix::sys::resource::UsageWho) -> Result<Duration> {
    use nix::sys::time::TimeVal;

    fn duration(value: TimeVal) -> Duration {
        let seconds = u64::try_from(value.tv_sec()).unwrap_or(0);
        let micros = u64::try_from(value.tv_usec()).unwrap_or(0);
        Duration::from_secs(seconds) + Duration::from_micros(micros)
    }

    let usage = nix::sys::resource::getrusage(who)
        .map_err(|errno| Error::other(format!("getrusage failed: {errno}")))?;
    Ok(duration(usage.user_time()) + duration(usage.system_time()))
}

/// Memory figures of `/proc/<pid>/status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProcStatus {
    resident_bytes: u64,
    virtual_bytes: u64,
    threads: u32,
}

/// Parses the handful of `/proc/<pid>/status` fields we need. Unknown or malformed lines are
/// ignored; missing fields stay 0 (kernel threads have no `Vm*` lines).
fn parse_proc_status(text: &str) -> ProcStatus {
    fn kilobytes(value: &str) -> u64 {
        value
            .split_whitespace()
            .next()
            .and_then(|number| number.parse::<u64>().ok())
            .map_or(0, |kb| kb.saturating_mul(1024))
    }

    let mut status = ProcStatus::default();
    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name {
            "VmRSS" => status.resident_bytes = kilobytes(value),
            "VmSize" => status.virtual_bytes = kilobytes(value),
            "Threads" => status.threads = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    status
}

fn sample_procfs() -> Result<ProcessSample> {
    let text = std::fs::read_to_string("/proc/self/status")?;
    let status = parse_proc_status(&text);
    #[cfg(unix)]
    let cpu_time = rusage_cpu_time(nix::sys::resource::UsageWho::RUSAGE_SELF)?;
    #[cfg(not(unix))]
    let cpu_time = Duration::ZERO;
    Ok(ProcessSample {
        taken_at: Instant::now(),
        cpu_time,
        resident_bytes: status.resident_bytes,
        virtual_bytes: status.virtual_bytes,
        threads: status.threads,
    })
}

/// Portable fallback: asks `sysinfo` about this one process only.
fn sample_sysinfo() -> Result<ProcessSample> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    let pid = sysinfo::get_current_pid()
        .map_err(|err| Error::other(format!("cannot determine the process id: {err}")))?;
    let mut system = System::new()
        .map_err(|err| Error::other(format!("cannot query the operating system: {err}")))?;
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::nothing().with_cpu().with_memory(),
    );
    let process = system
        .process(pid)
        .ok_or_else(|| Error::other("the operating system does not report this process"))?;
    Ok(ProcessSample {
        taken_at: Instant::now(),
        cpu_time: Duration::from_millis(process.accumulated_cpu_time()),
        resident_bytes: process.memory(),
        virtual_bytes: process.virtual_memory(),
        threads: process
            .tasks()
            .map_or(0, |tasks| u32::try_from(tasks.len()).unwrap_or(u32::MAX)),
    })
}

/// A sample together with the derived figures the dashboard shows.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ResourceSnapshot {
    /// CPU usage since the previous sample of the same monitor, percent of one core
    /// (0 for the first sample).
    pub cpu_percent: f64,
    /// Total CPU time consumed by the process, milliseconds.
    pub cpu_time_ms: u64,
    /// Resident set size in bytes.
    pub resident_bytes: u64,
    /// Highest resident set size observed, bytes.
    pub peak_resident_bytes: u64,
    /// Virtual memory size in bytes.
    pub virtual_bytes: u64,
    /// Number of threads (0 when unknown).
    pub threads: u32,
}

#[derive(Default)]
struct MonitorState {
    previous: Option<ProcessSample>,
    latest: Option<ResourceSnapshot>,
}

/// Tracks process resource usage over time: CPU percentage between samples and peak resident
/// memory. Cheap to clone; clones share state.
#[derive(Clone, Default)]
pub struct ResourceMonitor {
    state: Arc<Mutex<MonitorState>>,
    peak_resident: Arc<AtomicU64>,
    samples: Arc<AtomicU64>,
}

impl std::fmt::Debug for ResourceMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceMonitor")
            .field("peak_resident_bytes", &self.peak_resident_bytes())
            .finish()
    }
}

impl ResourceMonitor {
    /// Creates a monitor with no samples.
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes a sample now, updates the peak and returns the snapshot. `cpu_percent` covers the
    /// time since the previous call on this monitor (or its sampler).
    pub fn sample(&self) -> Result<ResourceSnapshot> {
        Ok(self.observe(ProcessSample::now()?))
    }

    /// Folds an externally taken sample into the monitor.
    pub fn observe(&self, sample: ProcessSample) -> ResourceSnapshot {
        let peak = self
            .peak_resident
            .fetch_max(sample.resident_bytes, Ordering::Relaxed)
            .max(sample.resident_bytes);
        self.samples.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock();
        let snapshot = ResourceSnapshot {
            cpu_percent: state
                .previous
                .as_ref()
                .map_or(0.0, |previous| cpu_percent(previous, &sample)),
            cpu_time_ms: u64::try_from(sample.cpu_time.as_millis()).unwrap_or(u64::MAX),
            resident_bytes: sample.resident_bytes,
            peak_resident_bytes: peak,
            virtual_bytes: sample.virtual_bytes,
            threads: sample.threads,
        };
        state.previous = Some(sample);
        state.latest = Some(snapshot);
        snapshot
    }

    /// The most recent snapshot, without sampling.
    pub fn latest(&self) -> Option<ResourceSnapshot> {
        self.state.lock().latest
    }

    /// Highest resident set size seen by any sample of this monitor, bytes.
    pub fn peak_resident_bytes(&self) -> u64 {
        self.peak_resident.load(Ordering::Relaxed)
    }

    /// Number of samples folded into this monitor so far.
    pub fn samples_taken(&self) -> u64 {
        self.samples.load(Ordering::Relaxed)
    }

    /// Forgets the recorded peak (e.g. between phases of a performance test).
    pub fn reset_peak(&self) {
        self.peak_resident.store(0, Ordering::Relaxed);
    }

    /// Starts a background task sampling every `interval` (raised to
    /// [`MIN_SAMPLER_INTERVAL`] if shorter) until `cancel` fires. One sample per interval costs
    /// microseconds, so the sampler stays far inside the idle-CPU budget. Must be called from
    /// within a Tokio runtime.
    pub fn spawn_sampler(&self, interval: Duration, cancel: CancellationToken) -> JoinHandle<()> {
        let monitor = self.clone();
        let period = interval.max(MIN_SAMPLER_INTERVAL);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            // After a suspend or a stall, resume the cadence instead of firing a burst.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        if let Err(err) = monitor.sample() {
                            tracing::debug!(error = %err, "resource sample failed");
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at: Instant, cpu_ms: u64, resident: u64) -> ProcessSample {
        ProcessSample {
            taken_at: at,
            cpu_time: Duration::from_millis(cpu_ms),
            resident_bytes: resident,
            virtual_bytes: resident * 2,
            threads: 3,
        }
    }

    #[test]
    fn samples_of_this_process_are_sane() {
        let first = ProcessSample::now().expect("sample");
        assert!(first.resident_bytes > 1024 * 1024, "{first:?}");
        assert!(first.resident_bytes < 64 * 1024 * 1024 * 1024);
        assert!(first.virtual_bytes >= first.resident_bytes);
        if cfg!(target_os = "linux") {
            assert!(first.threads >= 1);
        }
        // Burn a little CPU: the CPU clock must not go backwards.
        let mut acc = 0u64;
        for value in 0..3_000_000u64 {
            acc = acc
                .wrapping_mul(31)
                .wrapping_add(std::hint::black_box(value));
        }
        std::hint::black_box(acc);
        let second = ProcessSample::now().expect("sample");
        assert!(second.cpu_time >= first.cpu_time);
        assert!(second.taken_at >= first.taken_at);
        assert!(cpu_percent(&first, &second) >= 0.0);
    }

    #[test]
    fn the_portable_sampler_agrees_on_the_order_of_magnitude() {
        let portable = sample_sysinfo().expect("sysinfo sample");
        let native = ProcessSample::now().expect("native sample");
        assert!(portable.resident_bytes > 1024 * 1024);
        let ratio = portable.resident_bytes as f64 / native.resident_bytes as f64;
        assert!((0.25..4.0).contains(&ratio), "{portable:?} vs {native:?}");
    }

    #[test]
    fn proc_status_parsing_tolerates_garbage() {
        let status = parse_proc_status(
            "Name:\tanchor\nVmPeak:\t  900 kB\nVmSize:\t  800 kB\nVmHWM:\t 300 kB\n\
             VmRSS:\t   250 kB\nThreads:\t17\nnonsense line\nVmSwap: x kB\n",
        );
        assert_eq!(
            status,
            ProcStatus {
                resident_bytes: 250 * 1024,
                virtual_bytes: 800 * 1024,
                threads: 17,
            }
        );
        assert_eq!(parse_proc_status(""), ProcStatus::default());
        assert_eq!(
            parse_proc_status("VmRSS: lots\nThreads: many"),
            ProcStatus::default()
        );
    }

    #[test]
    fn cpu_percentage_maths() {
        let start = Instant::now();
        let earlier = sample(start, 1_000, 10);
        let half = sample(start + Duration::from_secs(2), 2_000, 10);
        assert!((cpu_percent(&earlier, &half) - 50.0).abs() < 1e-9);
        let two_cores = sample(start + Duration::from_secs(1), 3_000, 10);
        assert!((cpu_percent(&earlier, &two_cores) - 200.0).abs() < 1e-9);
        // Degenerate inputs never divide by zero or go negative.
        assert_eq!(cpu_percent(&earlier, &earlier), 0.0);
        assert_eq!(cpu_percent(&half, &earlier), 0.0);
        let clock_went_back = sample(start + Duration::from_secs(1), 500, 10);
        assert_eq!(cpu_percent(&earlier, &clock_went_back), 0.0);
    }

    #[test]
    fn monitor_tracks_cpu_between_samples_and_peak_memory() {
        let monitor = ResourceMonitor::new();
        assert!(monitor.latest().is_none());
        let start = Instant::now();
        let first = monitor.observe(sample(start, 0, 500));
        assert_eq!(first.cpu_percent, 0.0);
        let second = monitor.observe(sample(start + Duration::from_secs(4), 1_000, 900));
        assert!((second.cpu_percent - 25.0).abs() < 1e-9);
        let third = monitor.observe(sample(start + Duration::from_secs(5), 1_000, 200));
        assert_eq!(third.cpu_percent, 0.0);
        assert_eq!(third.resident_bytes, 200);
        assert_eq!(third.peak_resident_bytes, 900);
        assert_eq!(monitor.peak_resident_bytes(), 900);
        assert_eq!(monitor.latest(), Some(third));
        monitor.reset_peak();
        assert_eq!(monitor.peak_resident_bytes(), 0);

        let real = monitor.sample().expect("real sample");
        assert_eq!(real.peak_resident_bytes, real.resident_bytes);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_idle_period_measures_near_zero() {
        // Per-thread CPU time is immune to the other tests running in this process.
        let before = thread_cpu_time().expect("thread cpu");
        let started = Instant::now();
        std::thread::sleep(Duration::from_millis(250));
        let used = thread_cpu_time().expect("thread cpu") - before;
        let percent = used.as_secs_f64() / started.elapsed().as_secs_f64() * 100.0;
        assert!(percent < 5.0, "idle thread used {percent:.2}% CPU");
    }

    #[tokio::test]
    async fn sampler_is_coarse_and_stops_on_cancellation() {
        let monitor = ResourceMonitor::new();
        let cancel = CancellationToken::new();
        // A 1 ms request is raised to one second: ticks at 0 s and 1 s only.
        let handle = monitor.spawn_sampler(Duration::from_millis(1), cancel.clone());
        tokio::time::sleep(Duration::from_millis(1_300)).await;
        let taken = monitor.samples_taken();
        assert!((1..=3).contains(&taken), "{taken} samples in 1.3 s");
        assert!(monitor.latest().is_some());
        assert!(monitor.peak_resident_bytes() > 0);
        cancel.cancel();
        handle.await.expect("sampler exits cleanly");
        assert_eq!(monitor.samples_taken(), taken);
    }
}
