//! Optional health fields for the node heartbeat.
//!
//! A controller can route jobs away from a node whose harness endpoint no
//! longer answers, or whose workspace filesystem is nearly full. This module
//! samples those two signals behind traits so the agent can supply its real
//! adapters while tests use fakes. Nothing here opens a listener or makes a
//! scheduling decision: [`HeartbeatHealth`] is the additive payload the agent
//! merges into its existing heartbeat, and [`HeartbeatSampler::health`]
//! produces it from a short-lived cache.
//!
//! The probe is the only I/O with a time budget. It shares one deadline across
//! connect, request and reply, so a heartbeat cannot wait longer than that
//! budget for a dead harness. A failed probe is a normal `harness_ok: false`
//! sample, not an error or a panic.

use crate::orphan::{Clock, NodeMetrics, SystemClock};
use rustix::fs::statvfs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default time budget for one harness probe.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(200);
/// Default lifetime of a cached health sample.
pub const DEFAULT_CACHE_INTERVAL: Duration = Duration::from_secs(5);

const HTTP_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
const HTTP_PREFIX: &[u8] = b"HTTP/";

/// Whether the harness endpoint produced an HTTP reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeResult {
    /// The endpoint replied within the time budget.
    Answered,
    /// No HTTP reply arrived within the time budget.
    Unanswered,
}

/// Probes the local harness endpoint used by jobs.
///
/// Implementations must honour the supplied timeout and must report failure
/// as [`ProbeResult::Unanswered`] rather than escaping as an error or panic.
pub trait HarnessProbe: Send + Sync {
    /// Probes the endpoint, taking no longer than `timeout`.
    fn probe(&self, timeout: Duration) -> ProbeResult;
}

/// Probes a harness HTTP server over loopback TCP.
///
/// The connection, request and reply share a single deadline. Any HTTP reply
/// counts as an answer, including an error status: the signal is liveness of
/// the harness, not the health of a particular route. No credentials are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpHarnessProbe {
    address: SocketAddr,
}

impl TcpHarnessProbe {
    /// Creates a probe for the given local socket address.
    pub fn new(address: SocketAddr) -> Self {
        Self { address }
    }

    /// Returns the address being probed.
    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl HarnessProbe for TcpHarnessProbe {
    fn probe(&self, timeout: Duration) -> ProbeResult {
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return ProbeResult::Unanswered;
        };
        let Ok(mut stream) = TcpStream::connect_timeout(&self.address, remaining(deadline)) else {
            return ProbeResult::Unanswered;
        };
        if stream.set_write_timeout(Some(remaining(deadline))).is_err()
            || stream.set_read_timeout(Some(remaining(deadline))).is_err()
        {
            return ProbeResult::Unanswered;
        }
        if stream.write_all(HTTP_REQUEST).is_err() {
            return ProbeResult::Unanswered;
        }
        let mut prefix = [0u8; HTTP_PREFIX.len()];
        if stream.read_exact(&mut prefix).is_err() {
            return ProbeResult::Unanswered;
        }
        if prefix == *HTTP_PREFIX {
            ProbeResult::Answered
        } else {
            ProbeResult::Unanswered
        }
    }
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Filesystem capacity reported for the workspace mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskStats {
    /// Total data blocks in the filesystem, in bytes.
    pub total_bytes: u64,
    /// Blocks available to unprivileged users, in bytes.
    pub available_bytes: u64,
}

impl DiskStats {
    /// Percentage of the filesystem available to unprivileged users.
    ///
    /// The result is clamped to `0..=100`, including when a filesystem reports
    /// more available blocks than total blocks. A zero-sized filesystem reads
    /// as fully occupied.
    pub fn free_pct(&self) -> u8 {
        if self.total_bytes == 0 {
            return 0;
        }
        let ratio = u128::from(self.available_bytes) * 100 / u128::from(self.total_bytes);
        u8::try_from(ratio.min(100)).unwrap_or(100)
    }
}

/// Reads filesystem statistics for a path.
pub trait Statvfs: Send + Sync {
    /// Returns the statistics for the filesystem containing `path`, or `None`
    /// when the path cannot be inspected.
    fn statvfs(&self, path: &Path) -> Option<DiskStats>;
}

/// [`Statvfs`] backed by a single safe `statvfs` syscall.
#[derive(Debug, Default)]
pub struct SystemStatvfs;

impl Statvfs for SystemStatvfs {
    fn statvfs(&self, path: &Path) -> Option<DiskStats> {
        let info = statvfs(path).ok()?;
        let total_bytes = info.f_blocks.checked_mul(info.f_frsize)?;
        let available_bytes = info.f_bavail.checked_mul(info.f_frsize)?.min(total_bytes);
        Some(DiskStats {
            total_bytes,
            available_bytes,
        })
    }
}

/// Health fields attached to one heartbeat.
///
/// Every field is optional on the wire. A field is omitted when it could not
/// be sampled; the default value has no fields and serializes to `{}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeartbeatHealth {
    /// Whether the harness endpoint answered the last probe.
    pub harness_ok: Option<bool>,
    /// Percentage of workspace filesystem space available, from 0 to 100.
    pub disk_free_pct: Option<u8>,
    /// Workspace filesystem bytes available to unprivileged users.
    pub disk_free_bytes: Option<u64>,
}

impl HeartbeatHealth {
    /// Renders the known fields as a JSON object containing no other keys.
    ///
    /// The agent merges this object into its heartbeat payload. Adding these
    /// keys is wire-compatible with controllers that ignore unknown fields;
    /// no existing heartbeat key is changed or removed.
    pub fn to_json(&self) -> String {
        let mut fields = Vec::with_capacity(3);
        if let Some(ok) = self.harness_ok {
            fields.push(format!("\"harness_ok\":{ok}"));
        }
        if let Some(pct) = self.disk_free_pct {
            fields.push(format!("\"disk_free_pct\":{pct}"));
        }
        if let Some(bytes) = self.disk_free_bytes {
            fields.push(format!("\"disk_free_bytes\":{bytes}"));
        }
        format!("{{{}}}", fields.join(","))
    }
}

/// Probe timeout and sample lifetime for a heartbeat sampler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthConfig {
    probe_timeout: Duration,
    cache_interval: Duration,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            cache_interval: DEFAULT_CACHE_INTERVAL,
        }
    }
}

impl HealthConfig {
    /// Creates a configuration with the documented defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the shared deadline for one harness probe.
    pub fn with_probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }

    /// Sets how long a sample is reused before it is taken again.
    pub fn with_cache_interval(mut self, interval: Duration) -> Self {
        self.cache_interval = interval;
        self
    }

    /// Returns the probe deadline.
    pub fn probe_timeout(&self) -> Duration {
        self.probe_timeout
    }

    /// Returns the sample lifetime.
    pub fn cache_interval(&self) -> Duration {
        self.cache_interval
    }
}

/// Samples health fields for successive heartbeats.
///
/// A sample is reused until the cache interval expires. Sampling happens
/// outside the cache lock so one slow probe cannot delay another caller past
/// its own deadline. Each fresh sample is published to the shared
/// [`NodeMetrics`] used by the node metrics endpoint.
pub struct HeartbeatSampler {
    config: HealthConfig,
    probe: Box<dyn HarnessProbe>,
    statvfs: Box<dyn Statvfs>,
    workspace: PathBuf,
    clock: Box<dyn Clock>,
    metrics: Arc<NodeMetrics>,
    cache: Mutex<HealthCache>,
}

#[derive(Debug, Clone, Copy, Default)]
struct HealthCache {
    sample: HeartbeatHealth,
    sampled_at: Option<Instant>,
}

impl HeartbeatSampler {
    /// Creates a sampler for `workspace`.
    pub fn new(
        config: HealthConfig,
        workspace: impl Into<PathBuf>,
        probe: Box<dyn HarnessProbe>,
        statvfs: Box<dyn Statvfs>,
        metrics: Arc<NodeMetrics>,
    ) -> Self {
        Self {
            config,
            probe,
            statvfs,
            workspace: workspace.into(),
            clock: Box::new(SystemClock),
            metrics,
            cache: Mutex::new(HealthCache::default()),
        }
    }

    /// Replaces the clock, for example with a controllable test clock.
    pub fn with_clock(mut self, clock: Box<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Returns the sampler configuration.
    pub fn config(&self) -> &HealthConfig {
        &self.config
    }

    /// Returns the shared metrics counters.
    pub fn metrics(&self) -> &Arc<NodeMetrics> {
        &self.metrics
    }

    /// Returns the health fields to attach to this heartbeat.
    pub fn health(&self) -> HeartbeatHealth {
        let now = self.clock.now();
        if let Some(cached) = self.cached(now) {
            return cached;
        }
        let disk = self.statvfs.statvfs(&self.workspace);
        let sample = HeartbeatHealth {
            harness_ok: Some(self.probe.probe(self.config.probe_timeout) == ProbeResult::Answered),
            disk_free_pct: disk.map(|stats| stats.free_pct()),
            disk_free_bytes: disk.map(|stats| stats.available_bytes),
        };
        self.metrics.record_health(
            sample.harness_ok,
            sample.disk_free_pct,
            sample.disk_free_bytes,
        );
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        *cache = HealthCache {
            sample,
            sampled_at: Some(now),
        };
        sample
    }

    fn cached(&self, now: Instant) -> Option<HeartbeatHealth> {
        let cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        let sampled_at = cache.sampled_at?;
        let age = now.checked_duration_since(sampled_at)?;
        (age < self.config.cache_interval).then_some(cache.sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct FakeClock {
        now: Arc<Mutex<Instant>>,
    }

    impl FakeClock {
        fn new(now: Instant) -> Self {
            Self {
                now: Arc::new(Mutex::new(now)),
            }
        }

        fn advance(&self, by: Duration) {
            *self.now.lock().unwrap() += by;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }
    }

    #[derive(Default)]
    struct ProbeState {
        results: Mutex<VecDeque<ProbeResult>>,
        calls: AtomicUsize,
        timeouts: Mutex<Vec<Duration>>,
    }

    #[derive(Clone, Default)]
    struct FakeProbe {
        state: Arc<ProbeState>,
    }

    impl FakeProbe {
        fn new(results: &[ProbeResult]) -> Self {
            Self {
                state: Arc::new(ProbeState {
                    results: Mutex::new(results.iter().copied().collect()),
                    ..ProbeState::default()
                }),
            }
        }

        fn calls(&self) -> usize {
            self.state.calls.load(Ordering::Relaxed)
        }

        fn timeouts(&self) -> Vec<Duration> {
            self.state.timeouts.lock().unwrap().clone()
        }
    }

    impl HarnessProbe for FakeProbe {
        fn probe(&self, timeout: Duration) -> ProbeResult {
            self.state.calls.fetch_add(1, Ordering::Relaxed);
            self.state.timeouts.lock().unwrap().push(timeout);
            self.state
                .results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ProbeResult::Answered)
        }
    }

    #[derive(Clone, Default)]
    struct FakeStatvfs {
        result: Option<DiskStats>,
        calls: Arc<AtomicUsize>,
        paths: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl FakeStatvfs {
        fn new(result: Option<DiskStats>) -> Self {
            Self {
                result,
                ..Self::default()
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }

        fn paths(&self) -> Vec<PathBuf> {
            self.paths.lock().unwrap().clone()
        }
    }

    impl Statvfs for FakeStatvfs {
        fn statvfs(&self, path: &Path) -> Option<DiskStats> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.paths.lock().unwrap().push(path.to_owned());
            self.result
        }
    }

    fn sampler(
        probe: FakeProbe,
        statvfs: FakeStatvfs,
        config: HealthConfig,
        clock: FakeClock,
    ) -> (HeartbeatSampler, Arc<NodeMetrics>) {
        let metrics = Arc::new(NodeMetrics::default());
        let instance = HeartbeatSampler::new(
            config,
            "/srv/workspace",
            Box::new(probe),
            Box::new(statvfs),
            Arc::clone(&metrics),
        )
        .with_clock(Box::new(clock));
        (instance, metrics)
    }

    #[test]
    fn fresh_sample_reports_probe_disk_and_metrics() {
        let clock = FakeClock::new(Instant::now());
        let probe = FakeProbe::new(&[ProbeResult::Answered]);
        let statvfs = FakeStatvfs::new(Some(DiskStats {
            total_bytes: 1000,
            available_bytes: 250,
        }));
        let config = HealthConfig::new().with_probe_timeout(Duration::from_millis(150));
        let (instance, metrics) = sampler(probe.clone(), statvfs.clone(), config, clock);

        let health = instance.health();
        assert_eq!(
            health,
            HeartbeatHealth {
                harness_ok: Some(true),
                disk_free_pct: Some(25),
                disk_free_bytes: Some(250),
            }
        );
        assert_eq!(
            health.to_json(),
            "{\"harness_ok\":true,\"disk_free_pct\":25,\"disk_free_bytes\":250}"
        );
        assert_eq!(probe.timeouts(), vec![Duration::from_millis(150)]);
        assert_eq!(statvfs.paths(), vec![PathBuf::from("/srv/workspace")]);
        assert_eq!(metrics.harness_ok(), Some(true));
        assert_eq!(metrics.disk_free_pct(), Some(25));
        assert_eq!(metrics.disk_free_bytes(), Some(250));
        let rendered = metrics.render();
        assert!(rendered.contains("# TYPE harness_ok gauge\n"));
        assert!(rendered.contains("harness_ok 1\n"));
        assert!(rendered.contains("disk_free_pct 25\n"));
        assert!(rendered.contains("disk_free_bytes 250\n"));
    }

    #[test]
    fn failed_probe_is_a_sample_not_a_failure() {
        let probe = FakeProbe::new(&[ProbeResult::Unanswered]);
        let statvfs = FakeStatvfs::new(Some(DiskStats {
            total_bytes: 100,
            available_bytes: 0,
        }));
        let (instance, metrics) = sampler(
            probe,
            statvfs,
            HealthConfig::new(),
            FakeClock::new(Instant::now()),
        );

        let health = instance.health();
        assert_eq!(
            health,
            HeartbeatHealth {
                harness_ok: Some(false),
                disk_free_pct: Some(0),
                disk_free_bytes: Some(0),
            }
        );
        assert!(health.to_json().contains("\"harness_ok\":false"));
        assert_eq!(metrics.harness_ok(), Some(false));
        assert!(metrics.render().contains("harness_ok 0\n"));
    }

    #[test]
    fn statvfs_failure_omits_disk_fields() {
        let probe = FakeProbe::new(&[ProbeResult::Answered]);
        let (instance, metrics) = sampler(
            probe,
            FakeStatvfs::new(None),
            HealthConfig::new(),
            FakeClock::new(Instant::now()),
        );

        let health = instance.health();
        assert_eq!(health.harness_ok, Some(true));
        assert_eq!(health.disk_free_pct, None);
        assert_eq!(health.disk_free_bytes, None);
        assert_eq!(health.to_json(), "{\"harness_ok\":true}");
        assert_eq!(metrics.disk_free_pct(), None);
        assert_eq!(metrics.disk_free_bytes(), None);
        let rendered = metrics.render();
        assert!(!rendered.contains("disk_free_pct"));
        assert!(!rendered.contains("disk_free_bytes"));
    }

    #[test]
    fn empty_health_serializes_to_an_empty_object() {
        assert_eq!(HeartbeatHealth::default().to_json(), "{}");
    }

    #[test]
    fn disk_percent_stays_in_range() {
        let full = DiskStats {
            total_bytes: 100,
            available_bytes: 100,
        };
        let empty = DiskStats {
            total_bytes: 100,
            available_bytes: 0,
        };
        let over = DiskStats {
            total_bytes: 100,
            available_bytes: 150,
        };
        let zero = DiskStats {
            total_bytes: 0,
            available_bytes: 10,
        };
        assert_eq!(full.free_pct(), 100);
        assert_eq!(empty.free_pct(), 0);
        assert_eq!(over.free_pct(), 100);
        assert_eq!(zero.free_pct(), 0);
    }

    #[test]
    fn recent_samples_are_reused_until_the_interval_expires() {
        let clock = FakeClock::new(Instant::now());
        let probe = FakeProbe::new(&[ProbeResult::Answered, ProbeResult::Unanswered]);
        let statvfs = FakeStatvfs::new(Some(DiskStats {
            total_bytes: 100,
            available_bytes: 50,
        }));
        let interval = Duration::from_secs(5);
        let config = HealthConfig::new().with_cache_interval(interval);
        let (instance, metrics) = sampler(probe.clone(), statvfs.clone(), config, clock.clone());

        assert!(instance.health().harness_ok.unwrap());
        assert!(instance.health().harness_ok.unwrap());
        assert_eq!(probe.calls(), 1);
        assert_eq!(statvfs.calls(), 1);

        clock.advance(interval - Duration::from_nanos(1));
        assert!(instance.health().harness_ok.unwrap());
        assert_eq!(probe.calls(), 1);

        clock.advance(Duration::from_nanos(1));
        assert!(!instance.health().harness_ok.unwrap());
        assert_eq!(probe.calls(), 2);
        assert_eq!(statvfs.calls(), 2);
        assert_eq!(metrics.harness_ok(), Some(false));
    }

    #[test]
    fn tcp_probe_accepts_an_http_reply() {
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let address = listener.local_addr().expect("local address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accepted connection");
            let mut request = [0u8; 16];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        });
        let probe = TcpHarnessProbe::new(address);
        assert_eq!(probe.probe(Duration::from_secs(1)), ProbeResult::Answered);
        server.join().expect("server thread");
    }

    #[test]
    fn tcp_probe_times_out_without_a_reply() {
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let address = listener.local_addr().expect("local address");
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().expect("accepted connection");
            std::thread::sleep(Duration::from_millis(300));
        });
        let probe = TcpHarnessProbe::new(address);
        let started = Instant::now();
        assert_eq!(
            probe.probe(Duration::from_millis(50)),
            ProbeResult::Unanswered
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        server.join().expect("server thread");
    }
}
