//! Periodic reaper for harnesses left behind by a previous agent run.
//!
//! The agent starts harness work as containers or child processes. A restart
//! or a cancelled job can leave a harness running after its job is gone, which
//! consumes disk and IO on the node. [`OrphanWatcher`] sweeps the harnesses
//! reported by a [`HarnessLister`], and terminates only harnesses that carry
//! this agent's exact owner marker and whose job is no longer active.
//!
//! Nothing in this module performs process or container I/O directly: the
//! lister, job state, terminator, clock, sleeper and logger are supplied by
//! the caller. That keeps the safety decisions testable with fakes and lets
//! the agent wire the watcher into its existing runtime.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Default delay between sweeps (10 minutes).
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Default minimum age before an orphan is terminated (15 minutes).
pub const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(15 * 60);
/// Prefix of the marker the agent puts on every harness it starts.
pub const MARKER_PREFIX: &str = "tuishou-ting:";
/// Name of the node metric increased for each successful termination.
pub const ORPHANS_REAPED_TOTAL: &str = "orphans_reaped_total";

const MAX_ID_LEN: usize = 256;
const MAX_AGENT_ID_LEN: usize = 128;

/// Settings for an orphan watcher.
///
/// A new configuration uses the documented defaults: a ten minute sweep
/// interval and a fifteen minute grace period. Set the interval to `None` to
/// disable sweeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanReaperConfig {
    agent_id: String,
    sweep_interval: Option<Duration>,
    grace_period: Duration,
}

impl OrphanReaperConfig {
    /// Creates a configuration with the default interval and grace period.
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            sweep_interval: Some(DEFAULT_SWEEP_INTERVAL),
            grace_period: DEFAULT_GRACE_PERIOD,
        }
    }

    /// Sets the sweep interval; `None` disables the watcher.
    pub fn with_sweep_interval(mut self, sweep_interval: Option<Duration>) -> Self {
        self.sweep_interval = sweep_interval;
        self
    }

    /// Sets the minimum age an orphan must reach before termination.
    pub fn with_grace_period(mut self, grace_period: Duration) -> Self {
        self.grace_period = grace_period;
        self
    }

    /// Returns the agent identifier used to build the owner marker.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Returns the configured sweep interval, or `None` when disabled.
    pub fn sweep_interval(&self) -> Option<Duration> {
        self.sweep_interval
    }

    /// Returns the configured grace period.
    pub fn grace_period(&self) -> Duration {
        self.grace_period
    }

    /// Returns the exact marker a harness must carry to be considered ours.
    pub fn marker(&self) -> String {
        format!("{MARKER_PREFIX}{}", self.agent_id)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.agent_id.is_empty() {
            return Err(ConfigError::new("agent_id", "must not be empty"));
        }
        if self.agent_id.len() > MAX_AGENT_ID_LEN {
            return Err(ConfigError::new("agent_id", "is too long"));
        }
        if !self
            .agent_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(ConfigError::new(
                "agent_id",
                "allows only ASCII letters, digits, '-', '_' and '.'",
            ));
        }
        if self
            .sweep_interval
            .is_some_and(|interval| interval.is_zero())
        {
            return Err(ConfigError::new(
                "sweep_interval",
                "must be non-zero when enabled",
            ));
        }
        Ok(())
    }
}

/// A rejected watcher configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    field: &'static str,
    reason: String,
}

impl ConfigError {
    fn new(field: &'static str, reason: impl Into<String>) -> Self {
        Self {
            field,
            reason: reason.into(),
        }
    }

    /// Returns the name of the invalid setting.
    pub fn field(&self) -> &'static str {
        self.field
    }

    /// Returns the reason the setting was rejected.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {}", self.field, self.reason)
    }
}

impl std::error::Error for ConfigError {}

/// Whether a harness is a container or a child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessKind {
    /// A container started through the container runtime.
    Container,
    /// A process started as a child of the agent.
    Process,
}

impl fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Container => f.write_str("container"),
            Self::Process => f.write_str("process"),
        }
    }
}

/// One harness observed by a [`HarnessLister`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessHandle {
    /// Container identifier or process identifier, as reported by the lister.
    pub id: String,
    /// Job this harness was started for.
    pub job_id: String,
    /// Owner marker set when the harness was started, if any.
    pub owner_marker: Option<String>,
    /// Time the harness was started.
    pub started_at: Instant,
    /// Whether this is a container or a process.
    pub kind: HarnessKind,
}

/// A failed list or termination operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessError(String);

impl HarnessError {
    /// Creates an error with an operator-facing description.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// Returns the description.
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HarnessError {}

/// Lists harnesses that may be orphans.
pub trait HarnessLister: Send + Sync {
    /// Returns a snapshot of the harnesses visible to this agent.
    fn list(&self) -> Result<Vec<HarnessHandle>, HarnessError>;
}

/// State of a job known to the agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JobPhase {
    /// The job is executing; its harness must not be touched.
    Running,
    /// The job is being drained; its harness must not be touched.
    Draining,
    /// The agent has no active record of the job.
    Absent,
    /// The state could not be read; fail closed.
    #[default]
    Unknown,
}

impl fmt::Display for JobPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => f.write_str("running"),
            Self::Draining => f.write_str("draining"),
            Self::Absent => f.write_str("absent"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

/// Reports the phase of jobs known to the agent.
pub trait JobState: Send + Sync {
    /// Returns the phase of `job_id`, or [`JobPhase::Absent`] if there is no
    /// active record. Implementations that cannot read their state must
    /// return [`JobPhase::Unknown`] so the harness is left alone.
    fn phase(&self, job_id: &str) -> JobPhase;
}

/// In-memory job state covering running and draining jobs.
#[derive(Debug, Default)]
pub struct ActiveJobs {
    running: HashSet<String>,
    draining: HashSet<String>,
}

impl ActiveJobs {
    /// Creates an empty job state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a running job.
    pub fn with_running(mut self, job_id: impl Into<String>) -> Self {
        self.running.insert(job_id.into());
        self
    }

    /// Adds a draining job.
    pub fn with_draining(mut self, job_id: impl Into<String>) -> Self {
        self.draining.insert(job_id.into());
        self
    }

    /// Records a running job.
    pub fn start(&mut self, job_id: impl Into<String>) {
        let job_id = job_id.into();
        self.draining.remove(&job_id);
        self.running.insert(job_id);
    }

    /// Moves a job from running to draining.
    pub fn drain(&mut self, job_id: &str) {
        self.running.remove(job_id);
        self.draining.insert(job_id.to_owned());
    }

    /// Removes a finished job.
    pub fn finish(&mut self, job_id: &str) {
        self.running.remove(job_id);
        self.draining.remove(job_id);
    }
}

impl JobState for ActiveJobs {
    fn phase(&self, job_id: &str) -> JobPhase {
        if self.running.contains(job_id) {
            JobPhase::Running
        } else if self.draining.contains(job_id) {
            JobPhase::Draining
        } else {
            JobPhase::Absent
        }
    }
}

/// Stops a harness the reaper has proven is an orphan.
pub trait HarnessTerminator: Send + Sync {
    /// Terminates the harness, including any descendants it owns.
    fn terminate(&self, handle: &HarnessHandle) -> Result<(), HarnessError>;
}

/// Source of the current time.
pub trait Clock: Send + Sync {
    /// Returns the current instant.
    fn now(&self) -> Instant;
}

/// Wall-clock [`Clock`].
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Pauses between sweeps.
pub trait Sleeper: Send + Sync {
    /// Sleeps for `duration`.
    fn sleep(&self, duration: Duration);
}

/// Thread-based [`Sleeper`].
#[derive(Debug, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Severity of a watcher log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Per-harness decisions and sweep summaries.
    Debug,
    /// Lifecycle and successful terminations.
    Info,
    /// Recoverable operation failures.
    Warn,
    /// Failures that need operator attention.
    Error,
}

/// Sink for watcher log lines.
pub trait Logger: Send + Sync {
    /// Records one line.
    fn log(&self, level: LogLevel, message: &str);
}

/// Logger that writes to standard error.
#[derive(Debug, Default)]
pub struct StderrLogger;

impl Logger for StderrLogger {
    fn log(&self, level: LogLevel, message: &str) {
        eprintln!("[{level:?}] {message}");
    }
}

/// Counters exported for the node metrics endpoint.
#[derive(Debug, Default)]
pub struct NodeMetrics {
    orphans_reaped_total: AtomicU64,
}

impl NodeMetrics {
    /// Returns the number of harnesses terminated as orphans.
    pub fn orphans_reaped_total(&self) -> u64 {
        self.orphans_reaped_total.load(Ordering::Relaxed)
    }

    pub(crate) fn record_orphan_reaped(&self) {
        self.orphans_reaped_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Renders the counters in Prometheus text exposition format.
    pub fn render(&self) -> String {
        format!(
            "# HELP {name} Harnesses terminated by the orphan watcher.\n\
             # TYPE {name} counter\n\
             {name} {}\n",
            self.orphans_reaped_total(),
            name = ORPHANS_REAPED_TOTAL,
        )
    }
}

/// Outcome of one sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Harnesses returned by the lister.
    pub listed: usize,
    /// Identifiers of harnesses successfully terminated.
    pub reaped: Vec<String>,
    /// Harnesses without this agent's exact owner marker.
    pub skipped_unmatched: usize,
    /// Harnesses whose job is running or draining.
    pub skipped_active: usize,
    /// Harnesses whose job state could not be read.
    pub skipped_unknown_phase: usize,
    /// Harnesses younger than the grace period.
    pub skipped_young: usize,
    /// Harnesses with a start time in the future.
    pub skipped_clock_skew: usize,
    /// Harnesses with missing or malformed identifiers.
    pub skipped_invalid: usize,
    /// Terminations that failed.
    pub failures: usize,
    /// Whether the lister failed, in which case no harness was touched.
    pub list_failed: bool,
}

/// Sweeps for and terminates orphaned harnesses.
pub struct OrphanWatcher {
    config: OrphanReaperConfig,
    lister: Box<dyn HarnessLister>,
    state: Box<dyn JobState>,
    terminator: Box<dyn HarnessTerminator>,
    clock: Box<dyn Clock>,
    sleeper: Box<dyn Sleeper>,
    logger: Box<dyn Logger>,
    metrics: Arc<NodeMetrics>,
}

impl OrphanWatcher {
    /// Creates a watcher. The clock, sleeper and logger use real defaults
    /// and can be replaced with [`Self::with_clock`], [`Self::with_sleeper`]
    /// and [`Self::with_logger`].
    pub fn new(
        config: OrphanReaperConfig,
        lister: Box<dyn HarnessLister>,
        state: Box<dyn JobState>,
        terminator: Box<dyn HarnessTerminator>,
        metrics: Arc<NodeMetrics>,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            lister,
            state,
            terminator,
            metrics,
            clock: Box::new(SystemClock),
            sleeper: Box::new(ThreadSleeper),
            logger: Box::new(StderrLogger),
        })
    }

    /// Replaces the clock, for example with a controllable test clock.
    pub fn with_clock(mut self, clock: Box<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Replaces the sleeper, for example with one driven by a test.
    pub fn with_sleeper(mut self, sleeper: Box<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// Replaces the logger, for example with one that records lines.
    pub fn with_logger(mut self, logger: Box<dyn Logger>) -> Self {
        self.logger = logger;
        self
    }

    /// Returns the watcher configuration.
    pub fn config(&self) -> &OrphanReaperConfig {
        &self.config
    }

    /// Returns the shared metrics counters.
    pub fn metrics(&self) -> &Arc<NodeMetrics> {
        &self.metrics
    }

    /// Returns whether sweeping is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.sweep_interval.is_some()
    }

    /// Performs one sweep. This is also available when running the loop so
    /// an operator can trigger a sweep without waiting for the interval.
    pub fn sweep_once(&self) -> SweepReport {
        let mut report = SweepReport::default();
        if !self.is_enabled() {
            self.logger
                .log(LogLevel::Debug, "orphan sweep: watcher disabled");
            return report;
        }

        let handles = match self.lister.list() {
            Ok(handles) => handles,
            Err(error) => {
                self.logger.log(
                    LogLevel::Error,
                    &format!("orphan sweep: listing failed: {error}"),
                );
                report.list_failed = true;
                return report;
            }
        };

        report.listed = handles.len();
        self.logger.log(
            LogLevel::Debug,
            &format!("orphan sweep: listed {} harnesses", report.listed),
        );

        let now = self.clock.now();
        let marker = self.config.marker();
        let grace_period = self.config.grace_period;

        for handle in &handles {
            if !valid_harness_id(handle.kind, &handle.id) || !valid_field(&handle.job_id) {
                report.skipped_invalid += 1;
                self.logger.log(
                    LogLevel::Warn,
                    "orphan sweep: skipping harness with a missing or malformed identifier",
                );
                continue;
            }

            if handle.owner_marker.as_deref() != Some(marker.as_str()) {
                report.skipped_unmatched += 1;
                self.logger.log(
                    LogLevel::Debug,
                    &format!(
                        "orphan sweep: skipping {} {} without this agent's owner marker",
                        handle.kind, handle.id
                    ),
                );
                continue;
            }

            match self.state.phase(&handle.job_id) {
                JobPhase::Absent => {}
                JobPhase::Unknown => {
                    report.skipped_unknown_phase += 1;
                    self.logger.log(
                        LogLevel::Warn,
                        &format!(
                            "orphan sweep: skipping {} {} because job state is unavailable",
                            handle.kind, handle.id
                        ),
                    );
                    continue;
                }
                phase => {
                    report.skipped_active += 1;
                    self.logger.log(
                        LogLevel::Debug,
                        &format!(
                            "orphan sweep: skipping {} {} for job {} in phase {phase}",
                            handle.kind, handle.id, handle.job_id
                        ),
                    );
                    continue;
                }
            }

            let Some(age) = now.checked_duration_since(handle.started_at) else {
                report.skipped_clock_skew += 1;
                self.logger.log(
                    LogLevel::Warn,
                    &format!(
                        "orphan sweep: skipping {} {} with a start time in the future",
                        handle.kind, handle.id
                    ),
                );
                continue;
            };

            if age < grace_period {
                report.skipped_young += 1;
                self.logger.log(
                    LogLevel::Debug,
                    &format!(
                        "orphan sweep: skipping {} {} younger than the grace period",
                        handle.kind, handle.id
                    ),
                );
                continue;
            }

            match self.terminator.terminate(handle) {
                Ok(()) => {
                    self.metrics.record_orphan_reaped();
                    report.reaped.push(handle.id.clone());
                    self.logger.log(
                        LogLevel::Info,
                        &format!(
                            "orphan sweep: reaped {} {} for job {} after {}s",
                            handle.kind,
                            handle.id,
                            handle.job_id,
                            age.as_secs()
                        ),
                    );
                }
                Err(error) => {
                    report.failures += 1;
                    self.logger.log(
                        LogLevel::Error,
                        &format!(
                            "orphan sweep: failed to terminate {} {}: {error}",
                            handle.kind, handle.id
                        ),
                    );
                }
            }
        }

        self.logger.log(
            LogLevel::Debug,
            &format!(
                "orphan sweep complete: listed={} reaped={} unmatched={} active={} unknown={} young={} invalid={} failures={}",
                report.listed,
                report.reaped.len(),
                report.skipped_unmatched,
                report.skipped_active,
                report.skipped_unknown_phase,
                report.skipped_young,
                report.skipped_invalid,
                report.failures
            ),
        );
        report
    }

    /// Runs the periodic sweep loop until `shutdown` is set.
    ///
    /// The first sleep happens before the first sweep so job state has time
    /// to load after a restart. Shutdown is checked before each sleep and
    /// again before each sweep.
    pub fn run(&self, shutdown: &Arc<AtomicBool>) {
        if !self.is_enabled() {
            self.logger.log(LogLevel::Info, "orphan watcher disabled");
            return;
        }
        let Some(interval) = self.config.sweep_interval else {
            return;
        };

        self.logger.log(
            LogLevel::Info,
            &format!(
                "orphan watcher started with a {}s interval",
                interval.as_secs()
            ),
        );
        while !shutdown.load(Ordering::Acquire) {
            self.sleeper.sleep(interval);
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            self.sweep_once();
        }
        self.logger.log(LogLevel::Info, "orphan watcher stopped");
    }
}

/// Rejects identifiers that could be read as an option or a process group
/// by a terminator that shells out. Containers need a leading alphanumeric;
/// processes need a plain pid above 1 (0 and 1 are never agent harnesses).
fn valid_harness_id(kind: HarnessKind, id: &str) -> bool {
    if id.len() > MAX_ID_LEN {
        return false;
    }
    match kind {
        HarnessKind::Container => {
            id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        }
        HarnessKind::Process => {
            id.parse::<u32>().is_ok_and(|pid| pid > 1) && id.bytes().all(|b| b.is_ascii_digit())
        }
    }
}

fn valid_field(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_LEN
        && !value.chars().any(|character| character.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct FakeLister {
        result: Result<Vec<HarnessHandle>, HarnessError>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FakeLister {
        fn new(handles: Vec<HarnessHandle>) -> Self {
            Self {
                result: Ok(handles),
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        fn failing(message: &str) -> Self {
            Self {
                result: Err(HarnessError::new(message)),
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl HarnessLister for FakeLister {
        fn list(&self) -> Result<Vec<HarnessHandle>, HarnessError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.result.clone()
        }
    }

    #[derive(Clone)]
    struct FakeTerminator {
        attempts: Arc<Mutex<Vec<String>>>,
        fail_for: Arc<Vec<String>>,
    }

    impl FakeTerminator {
        fn new() -> Self {
            Self {
                attempts: Arc::new(Mutex::new(Vec::new())),
                fail_for: Arc::new(Vec::new()),
            }
        }

        fn failing_for(ids: &[&str]) -> Self {
            Self {
                fail_for: Arc::new(ids.iter().map(|id| (*id).to_owned()).collect()),
                ..Self::new()
            }
        }

        fn attempts(&self) -> Vec<String> {
            self.attempts.lock().unwrap().clone()
        }
    }

    impl HarnessTerminator for FakeTerminator {
        fn terminate(&self, handle: &HarnessHandle) -> Result<(), HarnessError> {
            self.attempts.lock().unwrap().push(handle.id.clone());
            if self.fail_for.contains(&handle.id) {
                Err(HarnessError::new("termination rejected"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone, Copy)]
    struct FakeClock(Instant);

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.0
        }
    }

    #[derive(Clone)]
    struct FakeSleeper {
        durations: Arc<Mutex<Vec<Duration>>>,
        shutdown: Arc<AtomicBool>,
        stop_after: usize,
    }

    impl Sleeper for FakeSleeper {
        fn sleep(&self, duration: Duration) {
            let count = {
                let mut durations = self.durations.lock().unwrap();
                durations.push(duration);
                durations.len()
            };
            if count >= self.stop_after {
                self.shutdown.store(true, Ordering::Release);
            }
        }
    }

    #[derive(Clone, Default)]
    struct RecordingLogger {
        lines: Arc<Mutex<Vec<(LogLevel, String)>>>,
    }

    impl RecordingLogger {
        fn has_line(&self, level: LogLevel, fragment: &str) -> bool {
            self.lines
                .lock()
                .unwrap()
                .iter()
                .any(|(line_level, line)| *line_level == level && line.contains(fragment))
        }
    }

    impl Logger for RecordingLogger {
        fn log(&self, level: LogLevel, message: &str) {
            self.lines.lock().unwrap().push((level, message.to_owned()));
        }
    }

    struct UnknownState;

    impl JobState for UnknownState {
        fn phase(&self, _job_id: &str) -> JobPhase {
            JobPhase::Unknown
        }
    }

    struct Harness {
        watcher: OrphanWatcher,
        metrics: Arc<NodeMetrics>,
        logger: RecordingLogger,
        terminator: FakeTerminator,
    }

    fn setup(
        config: OrphanReaperConfig,
        lister: FakeLister,
        state: Box<dyn JobState>,
        terminator: FakeTerminator,
        clock: FakeClock,
        sleeper: Option<FakeSleeper>,
    ) -> Harness {
        let metrics = Arc::new(NodeMetrics::default());
        let logger = RecordingLogger::default();
        let mut watcher = OrphanWatcher::new(
            config,
            Box::new(lister),
            state,
            Box::new(terminator.clone()),
            Arc::clone(&metrics),
        )
        .expect("valid config")
        .with_clock(Box::new(clock))
        .with_logger(Box::new(logger.clone()));
        if let Some(sleeper) = sleeper {
            watcher = watcher.with_sleeper(Box::new(sleeper));
        }
        Harness {
            watcher,
            metrics,
            logger,
            terminator,
        }
    }

    fn handle(
        id: &str,
        job_id: &str,
        marker: Option<&str>,
        age: Duration,
        kind: HarnessKind,
        now: Instant,
    ) -> HarnessHandle {
        HarnessHandle {
            id: id.to_owned(),
            job_id: job_id.to_owned(),
            owner_marker: marker.map(str::to_owned),
            started_at: now.checked_sub(age).expect("age fits"),
            kind,
        }
    }

    fn grace() -> Duration {
        Duration::from_secs(15 * 60)
    }

    #[test]
    fn config_defaults_match_documented_values() {
        let config = OrphanReaperConfig::new("node-a");
        assert_eq!(config.sweep_interval(), Some(DEFAULT_SWEEP_INTERVAL));
        assert_eq!(config.grace_period(), DEFAULT_GRACE_PERIOD);
        assert_eq!(config.marker(), "tuishou-ting:node-a");
        assert_eq!(DEFAULT_SWEEP_INTERVAL, Duration::from_secs(600));
        assert_eq!(DEFAULT_GRACE_PERIOD, Duration::from_secs(900));
    }

    #[test]
    fn config_rejects_invalid_values() {
        let long_id = "x".repeat(MAX_AGENT_ID_LEN + 1);
        for agent_id in ["", "bad id", long_id.as_str()] {
            let error = OrphanReaperConfig::new(agent_id).validate().unwrap_err();
            assert_eq!(error.field(), "agent_id");
        }

        let error = OrphanReaperConfig::new("node-a")
            .with_sweep_interval(Some(Duration::ZERO))
            .validate()
            .unwrap_err();
        assert_eq!(error.field(), "sweep_interval");
        assert!(
            OrphanReaperConfig::new("node-a")
                .with_sweep_interval(None)
                .validate()
                .is_ok()
        );
        assert!(
            OrphanReaperConfig::new("node-a")
                .with_grace_period(Duration::ZERO)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn disabled_watcher_never_lists_or_reaps() {
        let now = Instant::now();
        let lister = FakeLister::new(vec![handle(
            "h1",
            "j1",
            Some("tuishou-ting:node-a"),
            Duration::from_secs(3600),
            HarnessKind::Container,
            now,
        )]);
        let calls = lister.calls.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let harness = setup(
            OrphanReaperConfig::new("node-a").with_sweep_interval(None),
            lister,
            Box::new(ActiveJobs::new()),
            FakeTerminator::new(),
            FakeClock(now),
            None::<FakeSleeper>,
        );

        harness.watcher.run(&shutdown);
        let report = harness.watcher.sweep_once();
        assert_eq!(report, SweepReport::default());
        assert!(!harness.watcher.is_enabled());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(harness.terminator.attempts().is_empty());
        assert_eq!(harness.metrics.orphans_reaped_total(), 0);
        assert!(
            harness
                .logger
                .has_line(LogLevel::Info, "orphan watcher disabled")
        );
    }

    #[test]
    fn reaps_only_owned_orphans_older_than_grace() {
        let now = Instant::now();
        let marker = "tuishou-ting:node-a";
        let old = Duration::from_secs(3600);
        let young = Duration::from_secs(60);
        let lister = FakeLister::new(vec![
            handle(
                "owned-old",
                "j1",
                Some(marker),
                old,
                HarnessKind::Container,
                now,
            ),
            handle("unmarked", "j2", None, old, HarnessKind::Container, now),
            handle(
                "foreign",
                "j3",
                Some("tuishou-ting:node-b"),
                old,
                HarnessKind::Container,
                now,
            ),
            handle(
                "partial-marker",
                "j4",
                Some("tuishou-ting:node-a-extra"),
                old,
                HarnessKind::Container,
                now,
            ),
            handle(
                "owned-young",
                "j5",
                Some(marker),
                young,
                HarnessKind::Container,
                now,
            ),
        ]);
        let harness = setup(
            OrphanReaperConfig::new("node-a"),
            lister,
            Box::new(ActiveJobs::new()),
            FakeTerminator::new(),
            FakeClock(now),
            None::<FakeSleeper>,
        );

        let report = harness.watcher.sweep_once();
        assert_eq!(report.listed, 5);
        assert_eq!(report.reaped, vec!["owned-old"]);
        assert_eq!(report.skipped_unmatched, 3);
        assert_eq!(report.skipped_young, 1);
        assert_eq!(harness.terminator.attempts(), vec!["owned-old"]);
        assert_eq!(harness.metrics.orphans_reaped_total(), 1);
        assert!(
            harness
                .logger
                .has_line(LogLevel::Info, "reaped container owned-old")
        );
    }

    #[test]
    fn skips_running_draining_and_unknown_jobs() {
        let now = Instant::now();
        let marker = "tuishou-ting:node-a";
        let age = Duration::from_secs(3600);
        let lister = FakeLister::new(vec![
            handle(
                "running",
                "j1",
                Some(marker),
                age,
                HarnessKind::Container,
                now,
            ),
            handle(
                "draining",
                "j2",
                Some(marker),
                age,
                HarnessKind::Container,
                now,
            ),
            handle(
                "absent",
                "j3",
                Some(marker),
                age,
                HarnessKind::Container,
                now,
            ),
        ]);
        let state = ActiveJobs::new().with_running("j1").with_draining("j2");
        let harness = setup(
            OrphanReaperConfig::new("node-a"),
            lister,
            Box::new(state),
            FakeTerminator::new(),
            FakeClock(now),
            None::<FakeSleeper>,
        );

        let report = harness.watcher.sweep_once();
        assert_eq!(report.reaped, vec!["absent"]);
        assert_eq!(report.skipped_active, 2);
        assert!(
            harness
                .logger
                .has_line(LogLevel::Debug, "for job j1 in phase running")
        );
        assert!(
            harness
                .logger
                .has_line(LogLevel::Debug, "for job j2 in phase draining")
        );
    }

    #[test]
    fn unknown_job_state_fails_closed() {
        let now = Instant::now();
        let lister = FakeLister::new(vec![handle(
            "h1",
            "j1",
            Some("tuishou-ting:node-a"),
            Duration::from_secs(3600),
            HarnessKind::Container,
            now,
        )]);
        let harness = setup(
            OrphanReaperConfig::new("node-a"),
            lister,
            Box::new(UnknownState),
            FakeTerminator::new(),
            FakeClock(now),
            None::<FakeSleeper>,
        );

        let report = harness.watcher.sweep_once();
        assert_eq!(report.skipped_unknown_phase, 1);
        assert!(report.reaped.is_empty());
        assert!(harness.terminator.attempts().is_empty());
        assert_eq!(harness.metrics.orphans_reaped_total(), 0);
        assert!(
            harness
                .logger
                .has_line(LogLevel::Warn, "job state is unavailable")
        );
    }

    #[test]
    fn default_job_phase_fails_closed() {
        assert_eq!(JobPhase::default(), JobPhase::Unknown);
    }

    #[test]
    fn active_jobs_tracks_drain_and_finish() {
        let mut jobs = ActiveJobs::new();
        jobs.start("j1");
        assert_eq!(jobs.phase("j1"), JobPhase::Running);
        jobs.drain("j1");
        assert_eq!(jobs.phase("j1"), JobPhase::Draining);
        jobs.finish("j1");
        assert_eq!(jobs.phase("j1"), JobPhase::Absent);
        jobs.start("j2");
        jobs.start("j2");
        jobs.drain("j2");
        assert_eq!(jobs.phase("j2"), JobPhase::Draining);
    }

    #[test]
    fn grace_period_boundary_is_inclusive() {
        let now = Instant::now();
        let lister = FakeLister::new(vec![
            handle(
                "at-grace",
                "j1",
                Some("tuishou-ting:node-a"),
                grace(),
                HarnessKind::Container,
                now,
            ),
            handle(
                "under-grace",
                "j2",
                Some("tuishou-ting:node-a"),
                grace() - Duration::from_nanos(1),
                HarnessKind::Container,
                now,
            ),
        ]);
        let harness = setup(
            OrphanReaperConfig::new("node-a"),
            lister,
            Box::new(ActiveJobs::new()),
            FakeTerminator::new(),
            FakeClock(now),
            None::<FakeSleeper>,
        );

        let report = harness.watcher.sweep_once();
        assert_eq!(report.reaped, vec!["at-grace"]);
        assert_eq!(report.skipped_young, 1);
    }

    #[test]
    fn future_start_time_and_bad_identifiers_are_skipped() {
        let now = Instant::now();
        let lister = FakeLister::new(vec![
            HarnessHandle {
                id: "future".to_owned(),
                job_id: "j1".to_owned(),
                owner_marker: Some("tuishou-ting:node-a".to_owned()),
                started_at: now + Duration::from_secs(30),
                kind: HarnessKind::Container,
            },
            handle(
                "",
                "j2",
                Some("tuishou-ting:node-a"),
                Duration::from_secs(3600),
                HarnessKind::Container,
                now,
            ),
            handle(
                "bad-id",
                "j3\n",
                Some("tuishou-ting:node-a"),
                Duration::from_secs(3600),
                HarnessKind::Container,
                now,
            ),
        ]);
        let harness = setup(
            OrphanReaperConfig::new("node-a"),
            lister,
            Box::new(ActiveJobs::new()),
            FakeTerminator::new(),
            FakeClock(now),
            None::<FakeSleeper>,
        );

        let report = harness.watcher.sweep_once();
        assert_eq!(report.skipped_clock_skew, 1);
        assert_eq!(report.skipped_invalid, 2);
        assert!(harness.terminator.attempts().is_empty());
        assert_eq!(harness.metrics.orphans_reaped_total(), 0);
    }

    #[test]
    fn list_failure_touches_nothing() {
        let harness = setup(
            OrphanReaperConfig::new("node-a"),
            FakeLister::failing("lister unavailable"),
            Box::new(ActiveJobs::new()),
            FakeTerminator::new(),
            FakeClock(Instant::now()),
            None::<FakeSleeper>,
        );

        let report = harness.watcher.sweep_once();
        assert!(report.list_failed);
        assert_eq!(report.listed, 0);
        assert!(report.reaped.is_empty());
        assert!(harness.terminator.attempts().is_empty());
        assert_eq!(harness.metrics.orphans_reaped_total(), 0);
        assert!(harness.logger.has_line(LogLevel::Error, "listing failed"));
    }

    #[test]
    fn termination_failure_does_not_increment_success_counter() {
        let now = Instant::now();
        let lister = FakeLister::new(vec![handle(
            "stuck",
            "j1",
            Some("tuishou-ting:node-a"),
            Duration::from_secs(3600),
            HarnessKind::Container,
            now,
        )]);
        let harness = setup(
            OrphanReaperConfig::new("node-a"),
            lister,
            Box::new(ActiveJobs::new()),
            FakeTerminator::failing_for(&["stuck"]),
            FakeClock(now),
            None::<FakeSleeper>,
        );

        let report = harness.watcher.sweep_once();
        assert_eq!(report.failures, 1);
        assert!(report.reaped.is_empty());
        assert_eq!(harness.terminator.attempts(), vec!["stuck"]);
        assert_eq!(harness.metrics.orphans_reaped_total(), 0);
        assert!(
            harness
                .logger
                .has_line(LogLevel::Error, "failed to terminate container stuck")
        );
    }

    #[test]
    fn run_sweeps_at_the_interval_until_shutdown() {
        let now = Instant::now();
        let lister = FakeLister::new(Vec::new());
        let calls = lister.calls.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let sleeper = FakeSleeper {
            durations: Arc::new(Mutex::new(Vec::new())),
            shutdown: Arc::clone(&shutdown),
            stop_after: 3,
        };
        let durations = Arc::clone(&sleeper.durations);
        let harness = setup(
            OrphanReaperConfig::new("node-a").with_sweep_interval(Some(Duration::from_secs(30))),
            lister,
            Box::new(ActiveJobs::new()),
            FakeTerminator::new(),
            FakeClock(now),
            Some(sleeper),
        );

        harness.watcher.run(&shutdown);
        assert_eq!(
            *durations.lock().unwrap(),
            vec![
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30)
            ]
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(shutdown.load(Ordering::Relaxed));
        assert!(
            harness
                .logger
                .has_line(LogLevel::Info, "orphan watcher stopped")
        );
    }

    #[test]
    fn already_stopped_loop_does_not_sleep_or_sweep() {
        let lister = FakeLister::new(Vec::new());
        let calls = lister.calls.clone();
        let shutdown = Arc::new(AtomicBool::new(true));
        let sleeper = FakeSleeper {
            durations: Arc::new(Mutex::new(Vec::new())),
            shutdown: Arc::clone(&shutdown),
            stop_after: 1,
        };
        let durations = Arc::clone(&sleeper.durations);
        let harness = setup(
            OrphanReaperConfig::new("node-a"),
            lister,
            Box::new(ActiveJobs::new()),
            FakeTerminator::new(),
            FakeClock(Instant::now()),
            Some(sleeper),
        );

        harness.watcher.run(&shutdown);
        assert!(durations.lock().unwrap().is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn metrics_render_exposes_the_counter() {
        let now = Instant::now();
        let lister = FakeLister::new(vec![
            handle(
                "h1",
                "j1",
                Some("tuishou-ting:node-a"),
                Duration::from_secs(3600),
                HarnessKind::Container,
                now,
            ),
            handle(
                "h2",
                "j2",
                Some("tuishou-ting:node-a"),
                Duration::from_secs(3600),
                HarnessKind::Container,
                now,
            ),
        ]);
        let harness = setup(
            OrphanReaperConfig::new("node-a"),
            lister,
            Box::new(ActiveJobs::new()),
            FakeTerminator::new(),
            FakeClock(now),
            None::<FakeSleeper>,
        );

        harness.watcher.sweep_once();
        harness.watcher.sweep_once();
        assert_eq!(harness.metrics.orphans_reaped_total(), 4);
        let rendered = harness.metrics.render();
        assert!(rendered.contains("# TYPE orphans_reaped_total counter"));
        assert!(rendered.contains("orphans_reaped_total 4\n"));
    }

    #[test]
    fn unsafe_ids_are_never_terminated() {
        let now = Instant::now();
        let m = Some("tuishou-ting:node-a");
        let old = Duration::from_secs(3600);
        let lister = FakeLister::new(vec![
            handle("-f", "j", m, old, HarnessKind::Container, now),
            handle("--all", "j", m, old, HarnessKind::Container, now),
            handle("a b", "j", m, old, HarnessKind::Container, now),
            handle("a;rm", "j", m, old, HarnessKind::Container, now),
            handle("0", "j", m, old, HarnessKind::Process, now),
            handle("1", "j", m, old, HarnessKind::Process, now),
            handle("-1", "j", m, old, HarnessKind::Process, now),
            handle("12x", "j", m, old, HarnessKind::Process, now),
            handle("99999999999", "j", m, old, HarnessKind::Process, now),
        ]);
        let harness = setup(
            OrphanReaperConfig::new("node-a"),
            lister,
            Box::new(ActiveJobs::new()),
            FakeTerminator::new(),
            FakeClock(now),
            None::<FakeSleeper>,
        );
        let report = harness.watcher.sweep_once();
        assert_eq!(report.skipped_invalid, 9);
        assert!(harness.terminator.attempts().is_empty());
    }

    #[test]
    fn well_formed_ids_are_reaped() {
        let now = Instant::now();
        let m = Some("tuishou-ting:node-a");
        let old = Duration::from_secs(3600);
        let lister = FakeLister::new(vec![
            handle("4242", "j", m, old, HarnessKind::Process, now),
            handle("abc123_x.y-z", "j", m, old, HarnessKind::Container, now),
        ]);
        let harness = setup(
            OrphanReaperConfig::new("node-a"),
            lister,
            Box::new(ActiveJobs::new()),
            FakeTerminator::new(),
            FakeClock(now),
            None::<FakeSleeper>,
        );
        assert_eq!(harness.watcher.sweep_once().reaped.len(), 2);
    }
}
