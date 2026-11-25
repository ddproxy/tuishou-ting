//! Periodic garbage collection for Cargo target directories in the workspace.
//!
//! A single job can leave a large `target` tree behind when it is cancelled or
//! its harness exits unexpectedly. [`WorkspaceGc`] only sweeps when the
//! workspace filesystem is low on free space, and only removes directories it
//! can prove match a configured pattern, are not claimed by a running or
//! draining job, and have not been modified within a grace period.
//!
//! The module performs no scheduling of its own beyond a [`Sleeper`]. The
//! filesystem lister, remover, active-workspace state, clock and logger are
//! supplied by the caller so the safety decisions are testable with fakes. The
//! real adapters are intentionally narrow: they enumerate one directory at a
//! time and refuse symlinks at every step.

use crate::health::Statvfs;
use crate::orphan::{
    ConfigError, LogLevel, Logger, NodeMetrics, Sleeper, StderrLogger, ThreadSleeper,
};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

/// Default delay between sweeps (15 minutes).
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Default free-space threshold; a sweep runs only strictly below this value.
pub const DEFAULT_FREE_PCT_THRESHOLD: u8 = 15;
/// Default minimum idle time before a directory is removed (2 hours).
pub const DEFAULT_GRACE_PERIOD: Duration = Duration::from_secs(2 * 60 * 60);

const MAX_NAME_LEN: usize = 255;
const MAX_PATTERN_SEGMENTS: usize = 16;

/// The two roots the collector is allowed to enumerate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcRoot {
    /// Agent-managed per-job directories under the workspace.
    Workspace,
    /// Scratch directories under the workspace's tmp root.
    Tmp,
}

impl fmt::Display for GcRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace => f.write_str("workspace"),
            Self::Tmp => f.write_str("tmp"),
        }
    }
}

/// One relative glob anchored at a configured [`GcRoot`].
///
/// Patterns are `/`-separated and support `*` (any characters within one path
/// component) and `?` (one character). Matching is anchored at every segment:
/// a pattern cannot escape its root or match a directory at the wrong depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcPattern {
    root: GcRoot,
    glob: String,
}

impl GcPattern {
    /// Creates a pattern after rejecting escapes and unsupported glob syntax.
    pub fn new(root: GcRoot, glob: impl Into<String>) -> Result<Self, ConfigError> {
        let glob = glob.into();
        let segments: Vec<&str> = glob.split('/').collect();
        if segments.is_empty() || segments.len() > MAX_PATTERN_SEGMENTS {
            return Err(ConfigError::new("patterns", "has an invalid segment count"));
        }
        if !segments
            .iter()
            .all(|segment| valid_pattern_segment(segment))
        {
            return Err(ConfigError::new(
                "patterns",
                "allows only relative UTF-8 segments with '*' and '?' wildcards",
            ));
        }
        Ok(Self { root, glob })
    }

    /// Returns the root this pattern is anchored at.
    pub fn root(&self) -> GcRoot {
        self.root
    }

    /// Returns the relative glob text.
    pub fn glob(&self) -> &str {
        &self.glob
    }

    fn segments(&self) -> impl Iterator<Item = &str> {
        self.glob.split('/')
    }
}

/// Settings for a workspace garbage collector.
///
/// Defaults are a fifteen minute sweep interval, a strict 15 percent free-space
/// threshold, a two hour grace period, `target-*` and `*/target` under the
/// workspace root, and `*-target` and `target-*` under the tmp root. Set the
/// interval to `None` to disable sweeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcConfig {
    workspace_root: PathBuf,
    tmp_root: PathBuf,
    sweep_interval: Option<Duration>,
    free_pct_threshold: u8,
    grace_period: Duration,
    patterns: Vec<GcPattern>,
}

impl GcConfig {
    /// Creates a configuration with the documented defaults.
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        tmp_root: impl Into<PathBuf>,
    ) -> Result<Self, ConfigError> {
        let workspace_root = normalize_root(workspace_root.into(), "workspace_root")?;
        let tmp_root = normalize_root(tmp_root.into(), "tmp_root")?;
        let config = Self {
            workspace_root,
            tmp_root,
            sweep_interval: Some(DEFAULT_SWEEP_INTERVAL),
            free_pct_threshold: DEFAULT_FREE_PCT_THRESHOLD,
            grace_period: DEFAULT_GRACE_PERIOD,
            patterns: vec![
                GcPattern::new(GcRoot::Workspace, "target-*")?,
                GcPattern::new(GcRoot::Workspace, "*/target")?,
                GcPattern::new(GcRoot::Tmp, "*-target")?,
                GcPattern::new(GcRoot::Tmp, "target-*")?,
            ],
        };
        config.validate()?;
        Ok(config)
    }

    /// Sets the sweep interval; `None` disables the collector.
    pub fn with_sweep_interval(mut self, sweep_interval: Option<Duration>) -> Self {
        self.sweep_interval = sweep_interval;
        self
    }

    /// Sets the free-space percentage below which a sweep is allowed.
    pub fn with_free_pct_threshold(mut self, threshold: u8) -> Self {
        self.free_pct_threshold = threshold;
        self
    }

    /// Sets the minimum idle time before a matching directory is removed.
    pub fn with_grace_period(mut self, grace_period: Duration) -> Self {
        self.grace_period = grace_period;
        self
    }

    /// Adds one pattern to the configured set.
    pub fn with_pattern(mut self, pattern: GcPattern) -> Self {
        self.patterns.push(pattern);
        self
    }

    /// Replaces the configured patterns. An empty set disables removal.
    pub fn with_patterns(mut self, patterns: Vec<GcPattern>) -> Self {
        self.patterns = patterns;
        self
    }

    /// Returns the workspace root.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Returns the workspace tmp root.
    pub fn tmp_root(&self) -> &Path {
        &self.tmp_root
    }

    /// Returns the configured sweep interval, or `None` when disabled.
    pub fn sweep_interval(&self) -> Option<Duration> {
        self.sweep_interval
    }

    /// Returns the free-space threshold in percent.
    pub fn free_pct_threshold(&self) -> u8 {
        self.free_pct_threshold
    }

    /// Returns the idle-time grace period.
    pub fn grace_period(&self) -> Duration {
        self.grace_period
    }

    /// Returns the configured patterns.
    pub fn patterns(&self) -> &[GcPattern] {
        &self.patterns
    }

    fn root(&self, root: GcRoot) -> &Path {
        match root {
            GcRoot::Workspace => &self.workspace_root,
            GcRoot::Tmp => &self.tmp_root,
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        normalize_root(self.workspace_root.clone(), "workspace_root")?;
        normalize_root(self.tmp_root.clone(), "tmp_root")?;
        if !is_normalized_absolute(&self.workspace_root) {
            return Err(ConfigError::new(
                "workspace_root",
                "must be an absolute, normalized UTF-8 path",
            ));
        }
        if !is_normalized_absolute(&self.tmp_root) {
            return Err(ConfigError::new(
                "tmp_root",
                "must be an absolute, normalized UTF-8 path",
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
        if self.free_pct_threshold > 100 {
            return Err(ConfigError::new(
                "free_pct_threshold",
                "must be between 0 and 100",
            ));
        }
        for pattern in &self.patterns {
            GcPattern::new(pattern.root, pattern.glob.clone())?;
        }
        Ok(())
    }
}

/// A failed filesystem listing or removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsError {
    message: String,
    not_found: bool,
}

impl FsError {
    /// Creates an error with an operator-facing description.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            not_found: false,
        }
    }

    /// Creates an error for a path that no longer exists.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            not_found: true,
        }
    }

    fn from_io(context: &str, path: &Path, error: &std::io::Error) -> Self {
        Self {
            message: format!("{context} {}: {error}", path.display()),
            not_found: error.kind() == std::io::ErrorKind::NotFound,
        }
    }

    /// Returns the description.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns whether the path vanished, which is not an operational failure.
    pub fn is_not_found(&self) -> bool {
        self.not_found
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FsError {}

/// Type of one directory entry, without following symlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A real directory.
    Dir,
    /// A regular file.
    File,
    /// A symbolic link, including links to directories.
    Symlink,
    /// Anything else, such as a socket or device node.
    Other,
}

/// One entry observed in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEntry {
    /// File name, with no path separators.
    pub name: String,
    /// Type of the entry, never dereferenced.
    pub kind: EntryKind,
    /// Modification time of the entry itself.
    pub modified: SystemTime,
    /// Length in bytes; only meaningful for [`EntryKind::File`].
    pub size: u64,
}

/// Lists one directory without following symlinks.
pub trait DirLister: Send + Sync {
    /// Returns the immediate entries of `path`.
    ///
    /// Implementations must refuse to list a symlink and must report symlinks
    /// as [`EntryKind::Symlink`] rather than as their target.
    fn list_dir(&self, path: &Path) -> Result<Vec<FsEntry>, FsError>;
}

/// [`DirLister`] backed by `std::fs`.
#[derive(Debug, Default)]
pub struct SystemDirLister;

impl DirLister for SystemDirLister {
    fn list_dir(&self, path: &Path) -> Result<Vec<FsEntry>, FsError> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| FsError::from_io("metadata for", path, &error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(FsError::new(format!(
                "{} is not a real directory",
                path.display()
            )));
        }
        let read_dir =
            std::fs::read_dir(path).map_err(|error| FsError::from_io("listing", path, &error))?;
        let mut entries = Vec::new();
        for item in read_dir {
            let Ok(item) = item else { continue };
            let Some(name) = item.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if valid_name(&name).is_none() {
                continue;
            }
            let Ok(metadata) = std::fs::symlink_metadata(item.path()) else {
                continue;
            };
            let kind = if metadata.file_type().is_symlink() {
                EntryKind::Symlink
            } else if metadata.is_dir() {
                EntryKind::Dir
            } else if metadata.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            entries.push(FsEntry {
                name,
                kind,
                modified,
                size: if kind == EntryKind::File {
                    metadata.len()
                } else {
                    0
                },
            });
        }
        Ok(entries)
    }
}

/// Removes one directory tree.
pub trait DirRemover: Send + Sync {
    /// Removes `path` and its contents without following symlinks.
    ///
    /// `root` is the configured root `path` was found under. Implementations
    /// must refuse to remove anything that does not resolve to a strict
    /// descendant of `root`.
    fn remove_dir_all(&self, root: &Path, path: &Path) -> Result<(), FsError>;
}

/// [`DirRemover`] backed by `std::fs`.
#[derive(Debug, Default)]
pub struct SystemDirRemover;

impl DirRemover for SystemDirRemover {
    fn remove_dir_all(&self, root: &Path, path: &Path) -> Result<(), FsError> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| FsError::from_io("metadata for", path, &error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(FsError::new(format!(
                "{} is not a real directory",
                path.display()
            )));
        }
        let relative = path.strip_prefix(root).map_err(|_| {
            FsError::new(format!("{} is outside {}", path.display(), root.display()))
        })?;
        if relative.as_os_str().is_empty() {
            return Err(FsError::new(format!(
                "{} is the configured root",
                path.display()
            )));
        }
        let canonical_root = std::fs::canonicalize(root)
            .map_err(|error| FsError::from_io("resolving", root, &error))?;
        let canonical = std::fs::canonicalize(path)
            .map_err(|error| FsError::from_io("resolving", path, &error))?;
        if canonical != canonical_root.join(relative) {
            return Err(FsError::new(format!(
                "{} resolves to {}, outside its configured location",
                path.display(),
                canonical.display()
            )));
        }
        std::fs::remove_dir_all(&canonical)
            .map_err(|error| FsError::from_io("removing", &canonical, &error))
    }
}

/// Source of the current wall-clock time for grace-period checks.
pub trait MtimeClock: Send + Sync {
    /// Returns the current time.
    fn now(&self) -> SystemTime;
}

/// Wall-clock [`MtimeClock`].
#[derive(Debug, Default)]
pub struct SystemTimeClock;

impl MtimeClock for SystemTimeClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Paths occupied by jobs that are running or draining on this agent.
///
/// Returning `None` means state is unavailable; the collector then skips the
/// whole sweep instead of deleting a directory that may be in use.
pub trait ActiveWorkspaces: Send + Sync {
    /// Snapshot of active job workspace paths.
    fn active_workspaces(&self) -> Option<Vec<PathBuf>>;
}

/// In-memory active-workspace state.
#[derive(Debug, Default)]
pub struct ActiveJobWorkspaces {
    running: HashMap<String, PathBuf>,
    draining: HashMap<String, PathBuf>,
}

impl ActiveJobWorkspaces {
    /// Creates empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a running job workspace.
    pub fn with_running(
        mut self,
        job_id: impl Into<String>,
        workspace: impl Into<PathBuf>,
    ) -> Self {
        self.start(job_id, workspace);
        self
    }

    /// Adds a draining job workspace.
    pub fn with_draining(
        mut self,
        job_id: impl Into<String>,
        workspace: impl Into<PathBuf>,
    ) -> Self {
        let job_id = job_id.into();
        self.running.remove(&job_id);
        self.draining.insert(job_id, workspace.into());
        self
    }

    /// Records a running job workspace.
    pub fn start(&mut self, job_id: impl Into<String>, workspace: impl Into<PathBuf>) {
        let job_id = job_id.into();
        self.draining.remove(&job_id);
        self.running.insert(job_id, workspace.into());
    }

    /// Moves a job from running to draining, keeping its workspace protected.
    pub fn drain(&mut self, job_id: &str) {
        if let Some(workspace) = self.running.remove(job_id) {
            self.draining.insert(job_id.to_owned(), workspace);
        }
    }

    /// Removes a finished job.
    pub fn finish(&mut self, job_id: &str) {
        self.running.remove(job_id);
        self.draining.remove(job_id);
    }
}

impl ActiveWorkspaces for ActiveJobWorkspaces {
    fn active_workspaces(&self) -> Option<Vec<PathBuf>> {
        Some(
            self.running
                .values()
                .chain(self.draining.values())
                .cloned()
                .collect(),
        )
    }
}

/// One successful removal, with the bytes measured before deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcRemoval {
    /// Path that was removed.
    pub path: PathBuf,
    /// Bytes counted in its tree, excluding symlink targets.
    pub bytes: u64,
}

/// Outcome of one sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcSweepReport {
    /// Whether free space was below the configured threshold.
    pub below_threshold: bool,
    /// Free-space percentage read for this sweep, when available.
    pub free_pct: Option<u8>,
    /// Number of directory listings performed.
    pub listed: usize,
    /// Directories successfully removed.
    pub removed: Vec<GcRemoval>,
    /// Candidates protected by an active workspace.
    pub skipped_active: usize,
    /// Candidates younger than the grace period.
    pub skipped_young: usize,
    /// Candidates with a modification time in the future.
    pub skipped_clock_skew: usize,
    /// Symlinks whose name matched a pattern.
    pub skipped_symlink: usize,
    /// Entries or paths that could not be proven safe.
    pub skipped_invalid: usize,
    /// Candidates whose size could not be measured.
    pub skipped_unmeasurable: usize,
    /// Candidates that look like a repository checkout or contain a root.
    pub skipped_repo: usize,
    /// Candidates that vanished before or during removal.
    pub skipped_vanished: usize,
    /// Removals that failed.
    pub failures: usize,
    /// Free-space statistics were unavailable.
    pub stat_failed: bool,
    /// Active-workspace state was unavailable or malformed.
    pub workspace_state_failed: bool,
    /// A directory listing failed, so no removal was attempted.
    pub list_failed: bool,
}

struct Candidate {
    path: PathBuf,
    root: PathBuf,
    modified: SystemTime,
}

/// Sweeps stale, matching directories when the workspace is low on space.
pub struct WorkspaceGc {
    config: GcConfig,
    lister: Box<dyn DirLister>,
    remover: Box<dyn DirRemover>,
    statvfs: Box<dyn Statvfs>,
    workspaces: Box<dyn ActiveWorkspaces>,
    clock: Box<dyn MtimeClock>,
    sleeper: Box<dyn Sleeper>,
    logger: Box<dyn Logger>,
    metrics: Arc<NodeMetrics>,
}

impl WorkspaceGc {
    /// Creates a collector. Clock, sleeper and logger use real defaults.
    pub fn new(
        config: GcConfig,
        lister: Box<dyn DirLister>,
        remover: Box<dyn DirRemover>,
        statvfs: Box<dyn Statvfs>,
        workspaces: Box<dyn ActiveWorkspaces>,
        metrics: Arc<NodeMetrics>,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            lister,
            remover,
            statvfs,
            workspaces,
            metrics,
            clock: Box::new(SystemTimeClock),
            sleeper: Box::new(ThreadSleeper),
            logger: Box::new(StderrLogger),
        })
    }

    /// Replaces the clock, for example with a controllable test clock.
    pub fn with_clock(mut self, clock: Box<dyn MtimeClock>) -> Self {
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

    /// Returns the collector configuration.
    pub fn config(&self) -> &GcConfig {
        &self.config
    }

    /// Returns the shared metrics counters.
    pub fn metrics(&self) -> &Arc<NodeMetrics> {
        &self.metrics
    }

    /// Returns whether periodic sweeping is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.sweep_interval.is_some()
    }

    /// Performs one sweep. Also available for an on-demand sweep.
    pub fn sweep_once(&self) -> GcSweepReport {
        let mut report = GcSweepReport::default();
        if !self.is_enabled() {
            self.logger
                .log(LogLevel::Debug, "workspace gc: collector disabled");
            return report;
        }

        let Some(stats) = self.statvfs.statvfs(self.config.workspace_root()) else {
            report.stat_failed = true;
            self.logger.log(
                LogLevel::Warn,
                "workspace gc: skipping sweep because free space is unavailable",
            );
            return report;
        };
        let free_pct = stats.free_pct();
        report.free_pct = Some(free_pct);
        if free_pct >= self.config.free_pct_threshold {
            self.logger.log(
                LogLevel::Debug,
                &format!(
                    "workspace gc: skipping sweep at {free_pct}% free, threshold {}%",
                    self.config.free_pct_threshold
                ),
            );
            return report;
        }
        report.below_threshold = true;

        let Some(active) = self.workspaces.active_workspaces() else {
            report.workspace_state_failed = true;
            self.logger.log(
                LogLevel::Warn,
                "workspace gc: skipping sweep because active workspaces are unavailable",
            );
            return report;
        };
        let mut protected = Vec::with_capacity(active.len());
        for path in active {
            match normalize_workspace_path(&path) {
                Some(path) => protected.push(path),
                None => {
                    report.workspace_state_failed = true;
                    self.logger.log(
                        LogLevel::Warn,
                        "workspace gc: skipping sweep because an active workspace path is malformed",
                    );
                    return report;
                }
            }
        }

        let (candidates, listed) = match self.collect_candidates(&mut report) {
            Ok(result) => result,
            Err(error) => {
                report.list_failed = true;
                self.logger.log(
                    LogLevel::Error,
                    &format!("workspace gc: listing failed: {error}"),
                );
                return report;
            }
        };
        report.listed = listed;

        let now = self.clock.now();
        for candidate in candidates {
            if is_protected(&candidate.path, &protected) {
                report.skipped_active += 1;
                self.logger.log(
                    LogLevel::Debug,
                    &format!(
                        "workspace gc: skipping {} because a job workspace is active",
                        candidate.path.display()
                    ),
                );
                continue;
            }
            if self.contains_root(&candidate.path) {
                report.skipped_repo += 1;
                self.logger.log(
                    LogLevel::Warn,
                    &format!(
                        "workspace gc: refusing {} because it contains a configured root",
                        candidate.path.display()
                    ),
                );
                continue;
            }
            if !candidate.path.starts_with(&candidate.root) {
                report.skipped_invalid += 1;
                self.logger.log(
                    LogLevel::Warn,
                    &format!(
                        "workspace gc: refusing {} outside its configured root",
                        candidate.path.display()
                    ),
                );
                continue;
            }
            if now
                .duration_since(candidate.modified)
                .is_ok_and(|age| age < self.config.grace_period)
            {
                report.skipped_young += 1;
                continue;
            }
            let measured = match self.measure(&candidate.path, candidate.modified) {
                Ok(measured) => measured,
                Err(error) if error.is_not_found() => {
                    report.skipped_vanished += 1;
                    self.logger.log(
                        LogLevel::Debug,
                        &format!(
                            "workspace gc: {} vanished before removal",
                            candidate.path.display()
                        ),
                    );
                    continue;
                }
                Err(error) => {
                    report.skipped_unmeasurable += 1;
                    self.logger.log(
                        LogLevel::Warn,
                        &format!(
                            "workspace gc: skipping {} because its size is unavailable: {error}",
                            candidate.path.display()
                        ),
                    );
                    continue;
                }
            };
            if measured.repo_marker {
                report.skipped_repo += 1;
                self.logger.log(
                    LogLevel::Warn,
                    &format!(
                        "workspace gc: refusing {} because it looks like a repository checkout",
                        candidate.path.display()
                    ),
                );
                continue;
            }
            let Ok(age) = now.duration_since(measured.newest) else {
                report.skipped_clock_skew += 1;
                self.logger.log(
                    LogLevel::Warn,
                    &format!(
                        "workspace gc: skipping {} with a modification time in the future",
                        candidate.path.display()
                    ),
                );
                continue;
            };
            if age < self.config.grace_period {
                report.skipped_young += 1;
                self.logger.log(
                    LogLevel::Debug,
                    &format!(
                        "workspace gc: skipping {} younger than the grace period",
                        candidate.path.display()
                    ),
                );
                continue;
            }
            let bytes = measured.bytes;
            match self
                .remover
                .remove_dir_all(&candidate.root, &candidate.path)
            {
                Ok(()) => {
                    self.metrics.record_gc_removal(bytes);
                    report.removed.push(GcRemoval {
                        path: candidate.path.clone(),
                        bytes,
                    });
                    self.logger.log(
                        LogLevel::Info,
                        &format!(
                            "workspace gc: removed {} ({} bytes, idle {}s)",
                            candidate.path.display(),
                            bytes,
                            age.as_secs()
                        ),
                    );
                }
                Err(error) if error.is_not_found() => {
                    report.skipped_vanished += 1;
                    self.logger.log(
                        LogLevel::Debug,
                        &format!(
                            "workspace gc: {} vanished during removal",
                            candidate.path.display()
                        ),
                    );
                }
                Err(error) => {
                    report.failures += 1;
                    self.logger.log(
                        LogLevel::Error,
                        &format!(
                            "workspace gc: failed to remove {}: {error}",
                            candidate.path.display()
                        ),
                    );
                }
            }
        }

        self.logger.log(
            LogLevel::Debug,
            &format!(
                "workspace gc complete: free={}% listed={} removed={} active={} young={} skew={} symlink={} invalid={} unmeasurable={} failures={}",
                report.free_pct.unwrap_or(0),
                report.listed,
                report.removed.len(),
                report.skipped_active,
                report.skipped_young,
                report.skipped_clock_skew,
                report.skipped_symlink,
                report.skipped_invalid,
                report.skipped_unmeasurable,
                report.failures
            ),
        );
        report
    }

    /// Runs the periodic sweep loop until `shutdown` is set.
    ///
    /// The first sleep happens before the first sweep. Shutdown is checked
    /// before each sleep and again before each sweep.
    pub fn run(&self, shutdown: &Arc<AtomicBool>) {
        if !self.is_enabled() {
            self.logger.log(LogLevel::Info, "workspace gc disabled");
            return;
        }
        let Some(interval) = self.config.sweep_interval else {
            return;
        };
        self.logger.log(
            LogLevel::Info,
            &format!(
                "workspace gc started with a {}s interval",
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
        self.logger.log(LogLevel::Info, "workspace gc stopped");
    }

    fn collect_candidates(
        &self,
        report: &mut GcSweepReport,
    ) -> Result<(Vec<Candidate>, usize), FsError> {
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        let mut listed = 0;
        for pattern in &self.config.patterns {
            let root = self.config.root(pattern.root);
            let segments: Vec<&str> = pattern.segments().collect();
            let mut frontier = vec![root.to_path_buf()];
            for (index, segment) in segments.iter().enumerate() {
                let last = index + 1 == segments.len();
                let mut next = Vec::new();
                for directory in &frontier {
                    let entries = match self.lister.list_dir(directory) {
                        Ok(entries) => entries,
                        Err(error) if error.is_not_found() && directory != root => {
                            report.skipped_vanished += 1;
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    listed += 1;
                    for entry in entries {
                        let Some(name) = valid_name(&entry.name).map(str::to_owned) else {
                            report.skipped_invalid += 1;
                            continue;
                        };
                        if !match_segment(segment, &name) {
                            continue;
                        }
                        if entry.kind == EntryKind::Symlink {
                            report.skipped_symlink += 1;
                            self.logger.log(
                                LogLevel::Debug,
                                &format!(
                                    "workspace gc: refusing symlink {} matching {}",
                                    directory.join(&name).display(),
                                    pattern.glob()
                                ),
                            );
                            continue;
                        }
                        if entry.kind != EntryKind::Dir {
                            continue;
                        }
                        let path = directory.join(&name);
                        if !path.starts_with(root) {
                            report.skipped_invalid += 1;
                            continue;
                        }
                        if last {
                            if seen.insert(path.clone()) {
                                candidates.push(Candidate {
                                    path,
                                    root: root.to_path_buf(),
                                    modified: entry.modified,
                                });
                            }
                        } else {
                            next.push(path);
                        }
                    }
                }
                frontier = next;
            }
        }
        Ok((candidates, listed))
    }

    fn contains_root(&self, path: &Path) -> bool {
        self.config.workspace_root.starts_with(path) || self.config.tmp_root.starts_with(path)
    }

    fn measure(&self, root: &Path, modified: SystemTime) -> Result<Measured, FsError> {
        let mut measured = Measured {
            bytes: 0,
            newest: modified,
            repo_marker: false,
        };
        let mut stack = vec![root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            for entry in self.lister.list_dir(&directory)? {
                let Some(name) = valid_name(&entry.name) else {
                    continue;
                };
                if directory == root && REPO_MARKERS.contains(&name) {
                    measured.repo_marker = true;
                }
                measured.newest = measured.newest.max(entry.modified);
                match entry.kind {
                    EntryKind::File => {
                        measured.bytes =
                            measured.bytes.checked_add(entry.size).ok_or_else(|| {
                                FsError::new(format!("size overflow under {}", root.display()))
                            })?;
                    }
                    EntryKind::Dir => {
                        let child = directory.join(name);
                        if !child.starts_with(root) {
                            return Err(FsError::new(format!(
                                "entry {} escapes {}",
                                child.display(),
                                root.display()
                            )));
                        }
                        stack.push(child);
                    }
                    EntryKind::Symlink | EntryKind::Other => {}
                }
            }
        }
        Ok(measured)
    }
}

struct Measured {
    bytes: u64,
    newest: SystemTime,
    repo_marker: bool,
}

const REPO_MARKERS: [&str; 2] = [".git", "Cargo.toml"];

fn is_protected(path: &Path, workspaces: &[PathBuf]) -> bool {
    workspaces
        .iter()
        .any(|workspace| path.starts_with(workspace) || workspace.starts_with(path))
}

fn is_normalized_absolute(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    path.components().all(|component| match component {
        Component::RootDir | Component::Prefix(_) => true,
        Component::Normal(part) => part
            .to_str()
            .is_some_and(|text| !text.is_empty() && !text.chars().any(char::is_control)),
        Component::CurDir | Component::ParentDir => false,
    })
}

fn normalize_root(path: PathBuf, field: &'static str) -> Result<PathBuf, ConfigError> {
    if !is_normalized_absolute(&path) {
        return Err(ConfigError::new(
            field,
            "must be an absolute, normalized UTF-8 path",
        ));
    }
    if is_broad_root(&path) {
        return Err(ConfigError::new(
            field,
            "must not be a filesystem root, system directory or home directory",
        ));
    }
    Ok(path)
}

const BROAD_ROOTS: [&str; 20] = [
    "bin", "boot", "dev", "etc", "home", "lib", "lib64", "media", "mnt", "opt", "proc", "root",
    "run", "sbin", "srv", "sys", "tmp", "usr", "var", "Users",
];

/// Rejects `/`, a top-level system directory, or a single directory under
/// `/home` or `/Users` (a home directory).
fn is_broad_root(path: &Path) -> bool {
    let parts: Vec<&str> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect();
    match parts.as_slice() {
        [] => true,
        [only] => BROAD_ROOTS.contains(only),
        [first, _] => matches!(*first, "home" | "Users"),
        _ => false,
    }
}

fn normalize_workspace_path(path: &Path) -> Option<PathBuf> {
    if !is_normalized_absolute(path) {
        return None;
    }
    Some(path.to_path_buf())
}

fn valid_name(name: &str) -> Option<&str> {
    if name.is_empty()
        || name.len() > MAX_NAME_LEN
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\'))
    {
        return None;
    }
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return None;
    }
    Some(name)
}

fn valid_pattern_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_NAME_LEN
        && segment != "."
        && segment != ".."
        && !segment.chars().any(|character| {
            character.is_control() || matches!(character, '/' | '\\' | '[' | ']' | '{' | '}')
        })
}

fn match_segment(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let mut pattern_index = 0;
    let mut name_index = 0;
    let mut star = None;
    let mut star_match = 0;
    while name_index < name.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == name[name_index])
        {
            pattern_index += 1;
            name_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star = Some(pattern_index);
            pattern_index += 1;
            star_match = name_index;
        } else if let Some(index) = star {
            pattern_index = index + 1;
            star_match += 1;
            name_index = star_match;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::DiskStats;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::time::UNIX_EPOCH;

    #[derive(Clone)]
    struct FakeClock {
        now: Arc<Mutex<SystemTime>>,
    }

    impl FakeClock {
        fn new(now: SystemTime) -> Self {
            Self {
                now: Arc::new(Mutex::new(now)),
            }
        }

        fn advance(&self, by: Duration) {
            *self.now.lock().unwrap() += by;
        }
    }

    impl MtimeClock for FakeClock {
        fn now(&self) -> SystemTime {
            *self.now.lock().unwrap()
        }
    }

    type FakeDirs = Arc<Mutex<HashMap<PathBuf, Result<Vec<FsEntry>, String>>>>;

    #[derive(Clone, Default)]
    struct FakeDirLister {
        dirs: FakeDirs,
        calls: Arc<AtomicUsize>,
    }

    impl FakeDirLister {
        fn with_dir(self, path: impl Into<PathBuf>, entries: Vec<FsEntry>) -> Self {
            self.dirs.lock().unwrap().insert(path.into(), Ok(entries));
            self
        }

        fn fail_dir(self, path: impl Into<PathBuf>) -> Self {
            self.dirs
                .lock()
                .unwrap()
                .insert(path.into(), Err("listing failed".to_owned()));
            self
        }

        fn gone(self, path: impl Into<PathBuf>) -> Self {
            self.dirs
                .lock()
                .unwrap()
                .insert(path.into(), Err(GONE.to_owned()));
            self
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    const GONE: &str = "gone";

    impl DirLister for FakeDirLister {
        fn list_dir(&self, path: &Path) -> Result<Vec<FsEntry>, FsError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            match self.dirs.lock().unwrap().get(path) {
                Some(Ok(entries)) => Ok(entries.clone()),
                Some(Err(message)) if message == GONE => Err(FsError::not_found(GONE)),
                Some(Err(message)) => Err(FsError::new(message.clone())),
                None => Ok(Vec::new()),
            }
        }
    }

    #[derive(Clone, Default)]
    struct FakeRemover {
        removed: Arc<Mutex<Vec<PathBuf>>>,
        fail_for: Arc<Vec<PathBuf>>,
    }

    impl FakeRemover {
        fn failing_for(paths: &[&str]) -> Self {
            Self {
                fail_for: Arc::new(paths.iter().map(PathBuf::from).collect()),
                ..Self::default()
            }
        }

        fn removed(&self) -> Vec<PathBuf> {
            self.removed.lock().unwrap().clone()
        }
    }

    impl DirRemover for FakeRemover {
        fn remove_dir_all(&self, _root: &Path, path: &Path) -> Result<(), FsError> {
            if self.fail_for.contains(&path.to_path_buf()) {
                return Err(FsError::new("removal rejected"));
            }
            self.removed.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeStatvfs {
        stats: Option<DiskStats>,
        calls: Arc<AtomicUsize>,
    }

    impl FakeStatvfs {
        fn free(pct: u64) -> Self {
            Self {
                stats: Some(DiskStats {
                    total_bytes: 1000,
                    available_bytes: pct * 10,
                }),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn unavailable() -> Self {
            Self {
                stats: None,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl Statvfs for FakeStatvfs {
        fn statvfs(&self, _path: &Path) -> Option<DiskStats> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.stats
        }
    }

    #[derive(Clone)]
    struct FakeWorkspaces {
        paths: Arc<Mutex<Option<Vec<PathBuf>>>>,
    }

    impl Default for FakeWorkspaces {
        fn default() -> Self {
            Self {
                paths: Arc::new(Mutex::new(Some(Vec::new()))),
            }
        }
    }

    impl FakeWorkspaces {
        fn available(paths: &[&str]) -> Self {
            Self {
                paths: Arc::new(Mutex::new(Some(paths.iter().map(PathBuf::from).collect()))),
            }
        }

        fn unavailable() -> Self {
            Self {
                paths: Arc::new(Mutex::new(None)),
            }
        }
    }

    impl ActiveWorkspaces for FakeWorkspaces {
        fn active_workspaces(&self) -> Option<Vec<PathBuf>> {
            self.paths.lock().unwrap().clone()
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

    struct Harness {
        gc: WorkspaceGc,
        metrics: Arc<NodeMetrics>,
        lister: FakeDirLister,
        remover: FakeRemover,
        statvfs: FakeStatvfs,
        logger: RecordingLogger,
    }

    fn setup(
        config: GcConfig,
        lister: FakeDirLister,
        remover: FakeRemover,
        statvfs: FakeStatvfs,
        workspaces: Box<dyn ActiveWorkspaces>,
        clock: FakeClock,
        sleeper: Option<FakeSleeper>,
    ) -> Harness {
        let metrics = Arc::new(NodeMetrics::default());
        let logger = RecordingLogger::default();
        let mut gc = WorkspaceGc::new(
            config,
            Box::new(lister.clone()),
            Box::new(remover.clone()),
            Box::new(statvfs.clone()),
            workspaces,
            Arc::clone(&metrics),
        )
        .expect("valid config")
        .with_clock(Box::new(clock))
        .with_logger(Box::new(logger.clone()));
        if let Some(sleeper) = sleeper {
            gc = gc.with_sleeper(Box::new(sleeper));
        }
        Harness {
            gc,
            metrics,
            lister,
            remover,
            statvfs,
            logger,
        }
    }

    fn config() -> GcConfig {
        GcConfig::new("/srv/work", "/srv/work/tmp").expect("valid config")
    }

    fn when() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn ago(now: SystemTime, age: Duration) -> SystemTime {
        now.checked_sub(age).expect("age fits")
    }

    fn directory(name: &str, modified: SystemTime) -> FsEntry {
        FsEntry {
            name: name.to_owned(),
            kind: EntryKind::Dir,
            modified,
            size: 0,
        }
    }

    fn file(name: &str, size: u64) -> FsEntry {
        FsEntry {
            name: name.to_owned(),
            kind: EntryKind::File,
            modified: UNIX_EPOCH,
            size,
        }
    }

    fn symlink(name: &str, modified: SystemTime) -> FsEntry {
        FsEntry {
            name: name.to_owned(),
            kind: EntryKind::Symlink,
            modified,
            size: 0,
        }
    }

    #[test]
    fn config_defaults_match_documented_values() {
        let config = config();
        assert_eq!(config.sweep_interval(), Some(Duration::from_secs(900)));
        assert_eq!(config.free_pct_threshold(), 15);
        assert_eq!(config.grace_period(), Duration::from_secs(7200));
        assert_eq!(config.workspace_root(), Path::new("/srv/work"));
        assert_eq!(config.tmp_root(), Path::new("/srv/work/tmp"));
        assert_eq!(config.patterns().len(), 4);
        assert!(
            config
                .patterns()
                .iter()
                .any(|pattern| pattern.root() == GcRoot::Workspace && pattern.glob() == "target-*")
        );
        assert!(
            config
                .patterns()
                .iter()
                .any(|pattern| pattern.root() == GcRoot::Workspace && pattern.glob() == "*/target")
        );
        assert!(
            config
                .patterns()
                .iter()
                .any(|pattern| pattern.root() == GcRoot::Tmp && pattern.glob() == "*-target")
        );
        assert!(
            config
                .patterns()
                .iter()
                .any(|pattern| pattern.root() == GcRoot::Tmp && pattern.glob() == "target-*")
        );
        assert_eq!(DEFAULT_SWEEP_INTERVAL, Duration::from_secs(900));
        assert_eq!(DEFAULT_FREE_PCT_THRESHOLD, 15);
        assert_eq!(DEFAULT_GRACE_PERIOD, Duration::from_secs(7200));
    }

    #[test]
    fn config_rejects_invalid_values() {
        assert_eq!(
            GcConfig::new("relative", "/tmp").unwrap_err().field(),
            "workspace_root"
        );
        assert_eq!(
            GcConfig::new("/srv/work", "tmp").unwrap_err().field(),
            "tmp_root"
        );
        assert_eq!(
            config()
                .with_sweep_interval(Some(Duration::ZERO))
                .validate()
                .unwrap_err()
                .field(),
            "sweep_interval"
        );
        assert_eq!(
            config()
                .with_free_pct_threshold(101)
                .validate()
                .unwrap_err()
                .field(),
            "free_pct_threshold"
        );
        assert!(config().with_sweep_interval(None).validate().is_ok());
        for glob in ["", "../*", "/absolute", "a//b", "a/./b", "a/[b]", "a\\b"] {
            assert!(GcPattern::new(GcRoot::Tmp, glob).is_err(), "{glob}");
        }
        assert!(GcPattern::new(GcRoot::Tmp, "*-target").is_ok());
    }

    #[test]
    fn below_threshold_removes_stale_matches_and_records_metrics() {
        let now = when();
        let old = ago(now, Duration::from_secs(3 * 3600));
        let young = ago(now, Duration::from_secs(3600));
        let lister = FakeDirLister::default()
            .with_dir(
                "/srv/work",
                vec![
                    directory("target-old", old),
                    directory("target-young", young),
                    directory("work-a", old),
                ],
            )
            .with_dir("/srv/work/target-old", vec![file("payload.bin", 4096)])
            .with_dir("/srv/work/target-young", Vec::new())
            .with_dir(
                "/srv/work/work-a",
                vec![directory("target", old), symlink("target-link", old)],
            )
            .with_dir("/srv/work/work-a/target", vec![file("cache", 1024)])
            .with_dir(
                "/srv/work/tmp",
                vec![directory("app-target", old), symlink("link-target", old)],
            )
            .with_dir("/srv/work/tmp/app-target", vec![file("big", 8192)]);
        let harness = setup(
            config(),
            lister,
            FakeRemover::default(),
            FakeStatvfs::free(10),
            Box::new(FakeWorkspaces::default()),
            FakeClock::new(now),
            None,
        );

        let report = harness.gc.sweep_once();
        assert!(report.below_threshold);
        assert_eq!(report.free_pct, Some(10));
        assert_eq!(
            harness.remover.removed(),
            vec![
                PathBuf::from("/srv/work/target-old"),
                PathBuf::from("/srv/work/work-a/target"),
                PathBuf::from("/srv/work/tmp/app-target"),
            ]
        );
        assert_eq!(
            report.removed,
            vec![
                GcRemoval {
                    path: PathBuf::from("/srv/work/target-old"),
                    bytes: 4096
                },
                GcRemoval {
                    path: PathBuf::from("/srv/work/work-a/target"),
                    bytes: 1024
                },
                GcRemoval {
                    path: PathBuf::from("/srv/work/tmp/app-target"),
                    bytes: 8192
                },
            ]
        );
        assert_eq!(report.skipped_young, 1);
        assert_eq!(report.skipped_symlink, 1);
        assert_eq!(harness.metrics.gc_removed_total(), 3);
        assert_eq!(harness.metrics.gc_bytes_freed_total(), 13_312);
        assert_eq!(harness.statvfs.calls(), 1);
        assert!(
            harness
                .logger
                .has_line(LogLevel::Info, "removed /srv/work/target-old (4096 bytes")
        );
        let rendered = harness.metrics.render();
        assert!(rendered.contains("# TYPE gc_removed_total counter"));
        assert!(rendered.contains("# TYPE gc_bytes_freed_total counter"));
        assert!(rendered.contains("gc_removed_total 3\n"));
        assert!(rendered.contains("gc_bytes_freed_total 13312\n"));
    }

    #[test]
    fn above_threshold_does_not_list_or_remove() {
        let now = when();
        let lister = FakeDirLister::default().with_dir(
            "/srv/work",
            vec![directory(
                "target-old",
                ago(now, Duration::from_secs(3 * 3600)),
            )],
        );
        let harness = setup(
            config(),
            lister,
            FakeRemover::default(),
            FakeStatvfs::free(50),
            Box::new(FakeWorkspaces::default()),
            FakeClock::new(now),
            None,
        );

        let report = harness.gc.sweep_once();
        assert!(!report.below_threshold);
        assert_eq!(report.free_pct, Some(50));
        assert_eq!(harness.statvfs.calls(), 1);
        assert_eq!(harness.lister.calls(), 0);
        assert!(harness.remover.removed().is_empty());
        assert_eq!(harness.metrics.gc_removed_total(), 0);
    }

    #[test]
    fn active_workspaces_protect_running_and_draining_jobs() {
        let now = when();
        let old = ago(now, Duration::from_secs(3 * 3600));
        let lister = FakeDirLister::default()
            .with_dir(
                "/srv/work",
                vec![
                    directory("target-running", old),
                    directory("target-draining", old),
                    directory("work-parent", old),
                ],
            )
            .with_dir("/srv/work/target-running", Vec::new())
            .with_dir("/srv/work/target-draining", Vec::new())
            .with_dir("/srv/work/work-parent", vec![directory("target", old)])
            .with_dir("/srv/work/work-parent/target", Vec::new());
        let workspaces = ActiveJobWorkspaces::new()
            .with_running("r", "/srv/work/target-running")
            .with_draining("d", "/srv/work/target-draining")
            .with_running("p", "/srv/work/work-parent");
        let harness = setup(
            config(),
            lister,
            FakeRemover::default(),
            FakeStatvfs::free(10),
            Box::new(workspaces),
            FakeClock::new(now),
            None,
        );

        let report = harness.gc.sweep_once();
        assert_eq!(report.skipped_active, 3);
        assert!(harness.remover.removed().is_empty());
        assert_eq!(harness.metrics.gc_removed_total(), 0);
        assert!(harness.logger.has_line(
            LogLevel::Debug,
            "skipping /srv/work/target-running because a job workspace is active"
        ));
    }

    #[test]
    fn grace_period_boundary_is_inclusive() {
        let now = when();
        let grace = Duration::from_secs(7200);
        let lister = FakeDirLister::default()
            .with_dir(
                "/srv/work",
                vec![
                    directory("target-exact", ago(now, grace)),
                    directory("target-young", ago(now, grace - Duration::from_nanos(1))),
                ],
            )
            .with_dir("/srv/work/target-exact", Vec::new())
            .with_dir("/srv/work/target-young", Vec::new());
        let harness = setup(
            config(),
            lister,
            FakeRemover::default(),
            FakeStatvfs::free(10),
            Box::new(FakeWorkspaces::default()),
            FakeClock::new(now),
            None,
        );

        let report = harness.gc.sweep_once();
        assert_eq!(
            harness.remover.removed(),
            vec![PathBuf::from("/srv/work/target-exact")]
        );
        assert_eq!(report.skipped_young, 1);
        assert_eq!(harness.metrics.gc_removed_total(), 1);
    }

    #[test]
    fn future_modification_time_is_skipped() {
        let now = when();
        let lister = FakeDirLister::default()
            .with_dir(
                "/srv/work",
                vec![directory("target-future", now + Duration::from_secs(60))],
            )
            .with_dir("/srv/work/target-future", Vec::new());
        let harness = setup(
            config(),
            lister,
            FakeRemover::default(),
            FakeStatvfs::free(10),
            Box::new(FakeWorkspaces::default()),
            FakeClock::new(now),
            None,
        );

        let report = harness.gc.sweep_once();
        assert_eq!(report.skipped_clock_skew, 1);
        assert!(harness.remover.removed().is_empty());
    }

    #[test]
    fn symlink_escape_is_refused() {
        let now = when();
        let old = ago(now, Duration::from_secs(3 * 3600));
        let lister = FakeDirLister::default()
            .with_dir("/srv/work", Vec::new())
            .with_dir(
                "/srv/work/tmp",
                vec![symlink("target-escape", old), directory("target-safe", old)],
            )
            .with_dir("/srv/work/tmp/target-safe", vec![file("small", 8)]);
        let harness = setup(
            config(),
            lister,
            FakeRemover::default(),
            FakeStatvfs::free(10),
            Box::new(FakeWorkspaces::default()),
            FakeClock::new(now),
            None,
        );

        let report = harness.gc.sweep_once();
        assert_eq!(report.skipped_symlink, 1);
        assert_eq!(
            harness.remover.removed(),
            vec![PathBuf::from("/srv/work/tmp/target-safe")]
        );
        assert!(
            !harness
                .remover
                .removed()
                .contains(&PathBuf::from("/srv/work/tmp/target-escape"))
        );
        assert_eq!(harness.metrics.gc_bytes_freed_total(), 8);
    }

    #[test]
    fn malformed_entry_names_are_refused() {
        let now = when();
        let old = ago(now, Duration::from_secs(3 * 3600));
        let lister = FakeDirLister::default().with_dir(
            "/srv/work/tmp",
            vec![
                directory("../escape", old),
                directory("target/escape", old),
                directory("target-safe", old),
            ],
        );
        let harness = setup(
            config(),
            lister,
            FakeRemover::default(),
            FakeStatvfs::free(10),
            Box::new(FakeWorkspaces::default()),
            FakeClock::new(now),
            None,
        );

        let report = harness.gc.sweep_once();
        assert!(report.skipped_invalid >= 2);
        assert_eq!(
            harness.remover.removed(),
            vec![PathBuf::from("/srv/work/tmp/target-safe")]
        );
    }

    #[test]
    fn disabled_collector_never_samples_or_lists() {
        let now = when();
        let lister = FakeDirLister::default().with_dir(
            "/srv/work",
            vec![directory(
                "target-old",
                ago(now, Duration::from_secs(3 * 3600)),
            )],
        );
        let shutdown = Arc::new(AtomicBool::new(false));
        let harness = setup(
            config().with_sweep_interval(None),
            lister,
            FakeRemover::default(),
            FakeStatvfs::free(1),
            Box::new(FakeWorkspaces::default()),
            FakeClock::new(now),
            None,
        );

        harness.gc.run(&shutdown);
        let report = harness.gc.sweep_once();
        assert_eq!(report, GcSweepReport::default());
        assert!(!harness.gc.is_enabled());
        assert_eq!(harness.statvfs.calls(), 0);
        assert_eq!(harness.lister.calls(), 0);
        assert!(harness.remover.removed().is_empty());
        assert_eq!(harness.metrics.gc_removed_total(), 0);
        assert!(
            harness
                .logger
                .has_line(LogLevel::Info, "workspace gc disabled")
        );
    }

    #[test]
    fn unavailable_inputs_fail_closed() {
        let now = when();
        let missing_stats = setup(
            config(),
            FakeDirLister::default(),
            FakeRemover::default(),
            FakeStatvfs::unavailable(),
            Box::new(FakeWorkspaces::default()),
            FakeClock::new(now),
            None,
        );
        let report = missing_stats.gc.sweep_once();
        assert!(report.stat_failed);
        assert_eq!(missing_stats.lister.calls(), 0);

        let missing_state = setup(
            config(),
            FakeDirLister::default(),
            FakeRemover::default(),
            FakeStatvfs::free(10),
            Box::new(FakeWorkspaces::unavailable()),
            FakeClock::new(now),
            None,
        );
        let report = missing_state.gc.sweep_once();
        assert!(report.workspace_state_failed);
        assert_eq!(missing_state.lister.calls(), 0);
        assert!(missing_state.remover.removed().is_empty());

        let malformed_state = setup(
            config(),
            FakeDirLister::default(),
            FakeRemover::default(),
            FakeStatvfs::free(10),
            Box::new(FakeWorkspaces::available(&["relative/path"])),
            FakeClock::new(now),
            None,
        );
        assert!(malformed_state.gc.sweep_once().workspace_state_failed);
    }

    #[test]
    fn listing_failure_aborts_before_any_removal() {
        let now = when();
        let old = ago(now, Duration::from_secs(3 * 3600));
        let lister = FakeDirLister::default()
            .with_dir("/srv/work", vec![directory("target-old", old)])
            .with_dir("/srv/work/target-old", Vec::new())
            .fail_dir("/srv/work/tmp");
        let harness = setup(
            config(),
            lister,
            FakeRemover::default(),
            FakeStatvfs::free(10),
            Box::new(FakeWorkspaces::default()),
            FakeClock::new(now),
            None,
        );

        let report = harness.gc.sweep_once();
        assert!(report.list_failed);
        assert!(report.removed.is_empty());
        assert!(harness.remover.removed().is_empty());
        assert_eq!(harness.metrics.gc_removed_total(), 0);
    }

    #[test]
    fn removal_failure_does_not_increment_counters() {
        let now = when();
        let old = ago(now, Duration::from_secs(3 * 3600));
        let lister = FakeDirLister::default()
            .with_dir(
                "/srv/work",
                vec![
                    directory("target-stuck", old),
                    directory("target-good", old),
                ],
            )
            .with_dir("/srv/work/target-stuck", vec![file("a", 16)])
            .with_dir("/srv/work/target-good", vec![file("b", 32)]);
        let harness = setup(
            config(),
            lister,
            FakeRemover::failing_for(&["/srv/work/target-stuck"]),
            FakeStatvfs::free(10),
            Box::new(FakeWorkspaces::default()),
            FakeClock::new(now),
            None,
        );

        let report = harness.gc.sweep_once();
        assert_eq!(report.failures, 1);
        assert_eq!(report.removed.len(), 1);
        assert_eq!(harness.metrics.gc_removed_total(), 1);
        assert_eq!(harness.metrics.gc_bytes_freed_total(), 32);
        assert!(
            harness
                .logger
                .has_line(LogLevel::Error, "failed to remove /srv/work/target-stuck")
        );
    }

    #[test]
    fn run_sweeps_at_the_interval_until_shutdown() {
        let now = when();
        let shutdown = Arc::new(AtomicBool::new(false));
        let sleeper = FakeSleeper {
            durations: Arc::new(Mutex::new(Vec::new())),
            shutdown: Arc::clone(&shutdown),
            stop_after: 3,
        };
        let durations = Arc::clone(&sleeper.durations);
        let harness = setup(
            config().with_sweep_interval(Some(Duration::from_secs(30))),
            FakeDirLister::default(),
            FakeRemover::default(),
            FakeStatvfs::free(10),
            Box::new(FakeWorkspaces::default()),
            FakeClock::new(now),
            Some(sleeper),
        );

        harness.gc.run(&shutdown);
        assert_eq!(
            *durations.lock().unwrap(),
            vec![
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30)
            ]
        );
        assert_eq!(harness.statvfs.calls(), 2);
        assert!(shutdown.load(Ordering::Relaxed));
        assert!(
            harness
                .logger
                .has_line(LogLevel::Info, "workspace gc stopped")
        );
    }

    #[test]
    fn clock_can_advance_for_deterministic_grace_checks() {
        let clock = FakeClock::new(when());
        let before = clock.now();
        clock.advance(Duration::from_secs(60));
        assert_eq!(clock.now(), before + Duration::from_secs(60));
    }

    #[test]
    fn wildcard_matching_is_component_local() {
        assert!(match_segment("*-target", "app-target"));
        assert!(match_segment("target-*", "target-42"));
        assert!(match_segment("t?rget", "target"));
        assert!(!match_segment("*-target", "nested/target"));
        assert!(!match_segment("target", "target-old"));
        assert!(match_segment("*", "anything.else"));
    }

    fn sweep_with(lister: FakeDirLister, now: SystemTime) -> (Harness, GcSweepReport) {
        let harness = setup(
            config(),
            lister,
            FakeRemover::default(),
            FakeStatvfs::free(5),
            Box::new(FakeWorkspaces::default()),
            FakeClock::new(now),
            None,
        );
        let report = harness.gc.sweep_once();
        (harness, report)
    }

    #[test]
    fn recent_descendant_write_protects_an_old_directory() {
        let now = when();
        let old = ago(now, Duration::from_secs(10 * 3600));
        let fresh = ago(now, Duration::from_secs(60));
        let lister = FakeDirLister::default()
            .with_dir("/srv/work", vec![directory("target-busy", old)])
            .with_dir("/srv/work/target-busy", vec![directory("debug", old)])
            .with_dir(
                "/srv/work/target-busy/debug",
                vec![FsEntry {
                    modified: fresh,
                    ..file("deps.o", 10)
                }],
            );
        let (harness, report) = sweep_with(lister, now);
        assert!(harness.remover.removed().is_empty());
        assert_eq!(report.skipped_young, 1);
    }

    #[test]
    fn future_descendant_mtime_is_skipped() {
        let now = when();
        let old = ago(now, Duration::from_secs(10 * 3600));
        let lister = FakeDirLister::default()
            .with_dir("/srv/work", vec![directory("target-skew", old)])
            .with_dir(
                "/srv/work/target-skew",
                vec![FsEntry {
                    modified: now + Duration::from_secs(600),
                    ..file("late", 1)
                }],
            );
        let (harness, report) = sweep_with(lister, now);
        assert!(harness.remover.removed().is_empty());
        assert_eq!(report.skipped_clock_skew, 1);
    }

    #[test]
    fn repository_checkouts_are_never_removed() {
        let now = when();
        let old = ago(now, Duration::from_secs(10 * 3600));
        let lister = FakeDirLister::default()
            .with_dir(
                "/srv/work",
                vec![
                    directory("target-repo", old),
                    directory("target-crate", old),
                ],
            )
            .with_dir("/srv/work/target-repo", vec![directory(".git", old)])
            .with_dir("/srv/work/target-crate", vec![file("Cargo.toml", 10)]);
        let (harness, report) = sweep_with(lister, now);
        assert!(harness.remover.removed().is_empty());
        assert_eq!(report.skipped_repo, 2);
    }

    #[test]
    fn candidate_containing_a_root_is_never_removed() {
        let now = when();
        let old = ago(now, Duration::from_secs(10 * 3600));
        let config = GcConfig::new("/srv/work", "/srv/work/target-scratch/tmp").unwrap();
        let lister = FakeDirLister::default()
            .with_dir("/srv/work", vec![directory("target-scratch", old)])
            .with_dir("/srv/work/target-scratch", vec![directory("tmp", old)]);
        let harness = setup(
            config,
            lister,
            FakeRemover::default(),
            FakeStatvfs::free(5),
            Box::new(FakeWorkspaces::default()),
            FakeClock::new(now),
            None,
        );
        let report = harness.gc.sweep_once();
        assert!(harness.remover.removed().is_empty());
        assert_eq!(report.skipped_repo, 1);
    }

    #[test]
    fn broad_roots_are_rejected() {
        for bad in [
            "/",
            "/home",
            "/var",
            "/tmp",
            "/srv",
            "/usr",
            "/home/someone",
        ] {
            assert_eq!(
                GcConfig::new(bad, "/srv/work/tmp").unwrap_err().field(),
                "workspace_root",
                "{bad}"
            );
            assert_eq!(
                GcConfig::new("/srv/work", bad).unwrap_err().field(),
                "tmp_root",
                "{bad}"
            );
        }
        assert!(GcConfig::new("/srv/work", "/var/tmp/ting").is_ok());
        assert!(GcConfig::new("/srv/work/a/../b", "/srv/work/tmp").is_err());
    }

    #[test]
    fn patterns_cannot_escape_or_be_empty() {
        for glob in ["", "..", "../x", "/abs", "a//b", "a/.."] {
            assert!(GcPattern::new(GcRoot::Workspace, glob).is_err(), "{glob:?}");
        }
    }

    #[test]
    fn directory_vanishing_mid_sweep_is_not_a_failure() {
        let now = when();
        let old = ago(now, Duration::from_secs(10 * 3600));
        let lister = FakeDirLister::default()
            .with_dir(
                "/srv/work",
                vec![directory("job-a", old), directory("target-gone", old)],
            )
            .gone("/srv/work/job-a")
            .gone("/srv/work/target-gone");
        let (harness, report) = sweep_with(lister, now);
        assert!(harness.remover.removed().is_empty());
        assert_eq!(report.skipped_vanished, 3);
        assert_eq!(report.failures, 0);
        assert!(!report.list_failed);
    }

    #[test]
    fn removal_of_a_vanished_directory_is_not_a_failure() {
        struct Gone;
        impl DirRemover for Gone {
            fn remove_dir_all(&self, _root: &Path, path: &Path) -> Result<(), FsError> {
                Err(FsError::not_found(format!("{} is gone", path.display())))
            }
        }
        let now = when();
        let old = ago(now, Duration::from_secs(10 * 3600));
        let lister =
            FakeDirLister::default().with_dir("/srv/work", vec![directory("target-a", old)]);
        let gc = WorkspaceGc::new(
            config(),
            Box::new(lister),
            Box::new(Gone),
            Box::new(FakeStatvfs::free(5)),
            Box::new(FakeWorkspaces::default()),
            Arc::new(NodeMetrics::default()),
        )
        .unwrap()
        .with_clock(Box::new(FakeClock::new(now)))
        .with_logger(Box::new(RecordingLogger::default()));
        let report = gc.sweep_once();
        assert_eq!(report.skipped_vanished, 1);
        assert_eq!(report.failures, 0);
        assert_eq!(gc.metrics().gc_removed_total(), 0);
    }

    mod real_fs {
        use super::*;
        use std::os::unix::fs::symlink as os_symlink;

        struct Scratch(PathBuf);

        impl Scratch {
            fn new(tag: &str) -> Self {
                let path =
                    std::env::temp_dir().join(format!("ting-gc-test-{tag}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).unwrap();
                Self(path.canonicalize().unwrap())
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn remover_deletes_a_real_tree_and_never_follows_inner_symlinks() {
            let scratch = Scratch::new("tree");
            let root = scratch.0.join("root");
            let outside = scratch.0.join("outside");
            std::fs::create_dir_all(root.join("target-a/sub")).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("keep"), b"x").unwrap();
            os_symlink(&outside, root.join("target-a/sub/link")).unwrap();
            SystemDirRemover
                .remove_dir_all(&root, &root.join("target-a"))
                .unwrap();
            assert!(!root.join("target-a").exists());
            assert!(outside.join("keep").exists());
        }

        #[test]
        fn remover_refuses_a_symlink_and_leaves_its_target() {
            let scratch = Scratch::new("link");
            let root = scratch.0.join("root");
            let outside = scratch.0.join("outside");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("keep"), b"x").unwrap();
            os_symlink(&outside, root.join("target-link")).unwrap();
            assert!(
                SystemDirRemover
                    .remove_dir_all(&root, &root.join("target-link"))
                    .is_err()
            );
            assert!(outside.join("keep").exists());
            assert!(root.join("target-link").symlink_metadata().is_ok());
        }

        #[test]
        fn remover_refuses_a_path_through_a_symlinked_parent() {
            let scratch = Scratch::new("parent");
            let root = scratch.0.join("root");
            let outside = scratch.0.join("outside");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(outside.join("target")).unwrap();
            os_symlink(&outside, root.join("job")).unwrap();
            assert!(
                SystemDirRemover
                    .remove_dir_all(&root, &root.join("job/target"))
                    .is_err()
            );
            assert!(outside.join("target").exists());
        }

        #[test]
        fn remover_refuses_the_root_and_outside_paths() {
            let scratch = Scratch::new("root");
            let root = scratch.0.join("root");
            let other = scratch.0.join("other");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(&other).unwrap();
            assert!(SystemDirRemover.remove_dir_all(&root, &root).is_err());
            assert!(SystemDirRemover.remove_dir_all(&root, &other).is_err());
            assert!(root.exists() && other.exists());
        }

        #[test]
        fn remover_reports_a_missing_directory_as_not_found() {
            let scratch = Scratch::new("missing");
            let error = SystemDirRemover
                .remove_dir_all(&scratch.0, &scratch.0.join("target-x"))
                .unwrap_err();
            assert!(error.is_not_found());
        }

        #[test]
        fn lister_reports_symlinks_without_following_them() {
            let scratch = Scratch::new("list");
            std::fs::create_dir_all(scratch.0.join("real")).unwrap();
            os_symlink(scratch.0.join("real"), scratch.0.join("link")).unwrap();
            let entries = SystemDirLister.list_dir(&scratch.0).unwrap();
            let kind = |name: &str| entries.iter().find(|e| e.name == name).unwrap().kind;
            assert_eq!(kind("real"), EntryKind::Dir);
            assert_eq!(kind("link"), EntryKind::Symlink);
            assert!(SystemDirLister.list_dir(&scratch.0.join("link")).is_err());
        }
    }
}
