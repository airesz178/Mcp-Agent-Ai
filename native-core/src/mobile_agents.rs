//! Bounded background coordinator for explicitly permitted local workspace jobs.
//!
//! A coordinator is scoped to an existing, app-owned private workspace directory.
//! Submission returns immediately; at most `max_concurrent` fixed worker threads
//! execute read-only jobs. No providers, shell, network, or Android permissions
//! are used here. Running cancellation is cooperative: a bounded workspace call
//! already in progress finishes before its result is discarded.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Component, Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use uuid::Uuid;

use super::workspace_tools;

const MAX_WORKERS: usize = 4;
const MAX_QUEUE: usize = 32;
const MAX_HISTORY: usize = 128;

#[derive(Clone, Copy, Debug)]
pub struct CoordinatorConfig {
    pub max_concurrent: usize,
    pub max_queued: usize,
    pub max_history: usize,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 2,
            max_queued: 16,
            max_history: 64,
        }
    }
}

/// All supported operations are read-only and confined to the supplied workspace.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceJob {
    List { path: String },
    Read { path: String },
    Properties { path: String },
    SearchNames { query: String },
}

impl WorkspaceJob {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::List { path } | Self::Read { path } | Self::Properties { path } => {
                if path.len() > 256 || path.contains(['\0', '\r', '\n', '\\', ':']) {
                    return Err("Invalid private workspace path".into());
                }
                if !Path::new(path)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
                {
                    return Err("Path must remain within the private workspace".into());
                }
                if !matches!(self, Self::List { .. }) && path.is_empty() {
                    return Err("A relative file path is required".into());
                }
            }
            Self::SearchNames { query } => {
                if query.trim().is_empty()
                    || query.len() > 80
                    || query.chars().any(char::is_control)
                {
                    return Err("Search requires 1-80 printable characters".into());
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSnapshot {
    pub id: String,
    pub sequence: u64,
    pub operation: WorkspaceJob,
    pub status: JobStatus,
    pub progress: u8,
    pub cancel_requested: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoordinatorStats {
    pub running: usize,
    pub queued: usize,
    pub retained: usize,
    pub max_concurrent: usize,
    pub max_queued: usize,
    pub accepting_jobs: bool,
}

type JobExecutor = dyn Fn(&Path, &WorkspaceJob, &JobContext) -> Result<Value, String> + Send + Sync;

struct Scheduler {
    next_sequence: u64,
    jobs: HashMap<String, JobSnapshot>,
    order: VecDeque<String>,
    queue: VecDeque<String>,
    running: usize,
    stopping: bool,
}

struct Shared {
    home: PathBuf,
    config: CoordinatorConfig,
    executor: Arc<JobExecutor>,
    state: Mutex<Scheduler>,
    changed: Condvar,
}

fn lock_state(shared: &Shared) -> MutexGuard<'_, Scheduler> {
    shared
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct JobContext {
    shared: Arc<Shared>,
    id: String,
}

impl JobContext {
    fn cancelled(&self) -> bool {
        lock_state(&self.shared)
            .jobs
            .get(&self.id)
            .map_or(true, |job| job.cancel_requested)
    }

    fn report_progress(&self, progress: u8) {
        if !(11..=99).contains(&progress) {
            return;
        }
        let mut state = lock_state(&self.shared);
        if let Some(job) = state.jobs.get_mut(&self.id) {
            if job.status == JobStatus::Running && !job.cancel_requested && progress > job.progress
            {
                job.progress = progress;
                self.shared.changed.notify_all();
            }
        }
    }
}

fn workspace_executor(home: &Path, job: &WorkspaceJob, ctx: &JobContext) -> Result<Value, String> {
    ctx.report_progress(30);
    if ctx.cancelled() {
        return Err("Job cancelled".into());
    }
    let output = match job {
        WorkspaceJob::List { path } => workspace_tools::list(home, path),
        WorkspaceJob::Read { path } => workspace_tools::read(home, path),
        WorkspaceJob::Properties { path } => workspace_tools::properties(home, path),
        WorkspaceJob::SearchNames { query } => workspace_tools::search(home, query),
    }?;
    ctx.report_progress(90);
    Ok(output)
}

fn prune_history(state: &mut Scheduler, limit: usize) {
    let mut terminal = state
        .jobs
        .values()
        .filter(|job| job.status.is_terminal())
        .count();
    while terminal > limit {
        let Some(position) = state.order.iter().position(|id| {
            state
                .jobs
                .get(id)
                .is_some_and(|job| job.status.is_terminal())
        }) else {
            break;
        };
        if let Some(id) = state.order.remove(position) {
            state.jobs.remove(&id);
            terminal -= 1;
        }
    }
}

fn run_worker(shared: Arc<Shared>) {
    loop {
        let (id, operation) = {
            let mut state = lock_state(&shared);
            loop {
                if state.stopping {
                    return;
                }
                if let Some(id) = state.queue.pop_front() {
                    if let Some(job) = state.jobs.get_mut(&id) {
                        if job.status != JobStatus::Queued {
                            continue;
                        }
                        job.status = JobStatus::Running;
                        job.progress = 10;
                        let operation = job.operation.clone();
                        state.running += 1;
                        shared.changed.notify_all();
                        break (id, operation);
                    }
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };

        let ctx = JobContext {
            shared: Arc::clone(&shared),
            id: id.clone(),
        };
        let result = if ctx.cancelled() {
            Err("Job cancelled".into())
        } else {
            catch_unwind(AssertUnwindSafe(|| {
                (shared.executor)(&shared.home, &operation, &ctx)
            }))
            .unwrap_or_else(|_| Err("Local agent worker failed".into()))
        };
        let mut state = lock_state(&shared);
        state.running -= 1;
        if let Some(job) = state.jobs.get_mut(&id) {
            job.progress = 100;
            if job.cancel_requested {
                job.status = JobStatus::Cancelled;
            } else {
                match result {
                    Ok(value) => {
                        job.status = JobStatus::Completed;
                        job.result = Some(value);
                    }
                    Err(error) => {
                        job.status = JobStatus::Failed;
                        job.error = Some(error);
                    }
                }
            }
        }
        prune_history(&mut state, shared.config.max_history);
        shared.changed.notify_all();
    }
}

/// Owns a fixed-size background pool. Drop stops it and joins in-flight workers.
pub struct MobileAgentCoordinator {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl MobileAgentCoordinator {
    pub fn new(workspace_root: PathBuf, config: CoordinatorConfig) -> Result<Self, String> {
        Self::with_executor(workspace_root, config, Arc::new(workspace_executor))
    }

    fn with_executor(
        workspace_root: PathBuf,
        config: CoordinatorConfig,
        executor: Arc<JobExecutor>,
    ) -> Result<Self, String> {
        if !(1..=MAX_WORKERS).contains(&config.max_concurrent)
            || !(1..=MAX_QUEUE).contains(&config.max_queued)
            || !(1..=MAX_HISTORY).contains(&config.max_history)
        {
            return Err("Invalid coordinator limits".into());
        }
        // The app supplies its own private workspace root, not shared phone storage.
        // Canonicalization also rejects missing roots before worker threads start.
        let home = workspace_root
            .canonicalize()
            .map_err(|_| "Private workspace missing")?;
        if !home.is_dir() {
            return Err("Private workspace root is not a directory".into());
        }
        let shared = Arc::new(Shared {
            home,
            config,
            executor,
            state: Mutex::new(Scheduler {
                next_sequence: 0,
                jobs: HashMap::new(),
                order: VecDeque::new(),
                queue: VecDeque::new(),
                running: 0,
                stopping: false,
            }),
            changed: Condvar::new(),
        });
        let mut workers = Vec::with_capacity(config.max_concurrent);
        for index in 0..config.max_concurrent {
            let owned = Arc::clone(&shared);
            match thread::Builder::new()
                .name(format!("mobile-agent-{index}"))
                .spawn(move || run_worker(owned))
            {
                Ok(worker) => workers.push(worker),
                Err(_) => {
                    lock_state(&shared).stopping = true;
                    shared.changed.notify_all();
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err("Could not start local agent workers".into());
                }
            }
        }
        Ok(Self { shared, workers })
    }

    /// Nonblocking: the job is accepted into a bounded queue or rejected.
    pub fn submit(&self, operation: WorkspaceJob) -> Result<JobSnapshot, String> {
        operation.validate()?;
        let mut state = lock_state(&self.shared);
        if state.stopping {
            return Err("Coordinator stopped".into());
        }
        if state.queue.len() >= self.shared.config.max_queued {
            return Err("Local agent queue is full".into());
        }
        state.next_sequence = state
            .next_sequence
            .checked_add(1)
            .ok_or("Job sequence limit reached")?;
        let id = Uuid::new_v4().to_string();
        let job = JobSnapshot {
            id: id.clone(),
            sequence: state.next_sequence,
            operation,
            status: JobStatus::Queued,
            progress: 0,
            cancel_requested: false,
            result: None,
            error: None,
        };
        state.order.push_back(id.clone());
        state.queue.push_back(id.clone());
        state.jobs.insert(id, job.clone());
        self.shared.changed.notify_one();
        Ok(job)
    }

    /// Cancels queued jobs immediately; running jobs stop at the next safe point.
    pub fn cancel(&self, id: &str) -> Result<JobSnapshot, String> {
        let mut state = lock_state(&self.shared);
        let job = state.jobs.get_mut(id).ok_or("Unknown or expired job")?;
        if !job.status.is_terminal() {
            job.cancel_requested = true;
            if job.status == JobStatus::Queued {
                job.status = JobStatus::Cancelled;
                job.progress = 100;
                state.queue.retain(|queued| queued != id);
                prune_history(&mut state, self.shared.config.max_history);
            }
        }
        self.shared.changed.notify_all();
        state
            .jobs
            .get(id)
            .cloned()
            .ok_or("Job expired from history".into())
    }

    pub fn get(&self, id: &str) -> Result<JobSnapshot, String> {
        lock_state(&self.shared)
            .jobs
            .get(id)
            .cloned()
            .ok_or("Unknown or expired job".into())
    }

    /// Recent jobs in submission order, newest first; finite retained history.
    pub fn history(&self) -> Vec<JobSnapshot> {
        let state = lock_state(&self.shared);
        state
            .order
            .iter()
            .rev()
            .filter_map(|id| state.jobs.get(id).cloned())
            .collect()
    }

    pub fn stats(&self) -> CoordinatorStats {
        let state = lock_state(&self.shared);
        CoordinatorStats {
            running: state.running,
            queued: state.queue.len(),
            retained: state.jobs.len(),
            max_concurrent: self.shared.config.max_concurrent,
            max_queued: self.shared.config.max_queued,
            accepting_jobs: !state.stopping,
        }
    }

    /// Optional bounded wait for native callers; UI may poll `get` instead.
    pub fn wait(&self, id: &str, timeout: Duration) -> Result<JobSnapshot, String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or("Invalid wait timeout")?;
        let mut state = lock_state(&self.shared);
        loop {
            let job = state.jobs.get(id).ok_or("Unknown or expired job")?;
            if job.status.is_terminal() {
                return Ok(job.clone());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("Job wait timed out".into());
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }
}

impl Drop for MobileAgentCoordinator {
    fn drop(&mut self) {
        {
            let mut state = lock_state(&self.shared);
            state.stopping = true;
            state.queue.clear();
            for job in state.jobs.values_mut() {
                if !job.status.is_terminal() {
                    job.cancel_requested = true;
                    if job.status == JobStatus::Queued {
                        job.status = JobStatus::Cancelled;
                        job.progress = 100;
                    }
                }
            }
        }
        self.shared.changed.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs, sync::mpsc};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("mobile-agent-test-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn config(concurrent: usize, queue: usize, history: usize) -> CoordinatorConfig {
        CoordinatorConfig {
            max_concurrent: concurrent,
            max_queued: queue,
            max_history: history,
        }
    }
    fn list() -> WorkspaceJob {
        WorkspaceJob::List { path: "".into() }
    }
    fn finished(coordinator: &MobileAgentCoordinator, id: &str) -> JobSnapshot {
        coordinator.wait(id, Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn real_private_jobs_are_read_only_and_enforce_path_size_and_search_bounds() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("notes")).unwrap();
        fs::write(fixture.0.join("notes").join("hello.txt"), "synthetic only").unwrap();
        fs::write(fixture.0.join("large.txt"), vec![b'x'; 16385]).unwrap();
        let coord =
            MobileAgentCoordinator::new(fixture.0.clone(), CoordinatorConfig::default()).unwrap();
        let read = coord
            .submit(WorkspaceJob::Read {
                path: "notes/hello.txt".into(),
            })
            .unwrap();
        assert_eq!(
            finished(&coord, &read.id).result.unwrap()["content"],
            "synthetic only"
        );
        let listed = coord
            .submit(WorkspaceJob::List {
                path: "notes".into(),
            })
            .unwrap();
        assert_eq!(
            finished(&coord, &listed.id).result.unwrap()["entries"][0]["name"],
            "hello.txt"
        );
        let search = coord
            .submit(WorkspaceJob::SearchNames {
                query: "HELLO".into(),
            })
            .unwrap();
        assert_eq!(finished(&coord, &search.id).result.unwrap()["total"], 1);
        let props = coord
            .submit(WorkspaceJob::Properties {
                path: "notes/hello.txt".into(),
            })
            .unwrap();
        assert_eq!(finished(&coord, &props.id).result.unwrap()["bytes"], 14);
        let oversized = coord
            .submit(WorkspaceJob::Read {
                path: "large.txt".into(),
            })
            .unwrap();
        assert_eq!(finished(&coord, &oversized.id).status, JobStatus::Failed);
        assert!(coord
            .submit(WorkspaceJob::Read {
                path: "../private".into()
            })
            .is_err());
        assert!(coord
            .submit(WorkspaceJob::Read {
                path: "C:/Windows/win.ini".into()
            })
            .is_err());
        assert!(coord
            .submit(WorkspaceJob::SearchNames { query: "".into() })
            .is_err());
        assert_eq!(
            fs::read(fixture.0.join("notes/hello.txt")).unwrap(),
            b"synthetic only"
        );
    }

    #[test]
    fn queue_limit_and_queued_and_running_cancellation_are_deterministic() {
        let fixture = Fixture::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let executor: Arc<JobExecutor> = Arc::new(move |_, _, ctx| {
            ctx.report_progress(55);
            started_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            Ok(json!({"completed":true}))
        });
        let coord =
            MobileAgentCoordinator::with_executor(fixture.0.clone(), config(1, 1, 8), executor)
                .unwrap();
        let running = coord.submit(list()).unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(coord.get(&running.id).unwrap().progress, 55);
        let queued = coord.submit(list()).unwrap();
        assert!(coord.submit(list()).unwrap_err().contains("queue"));
        assert_eq!(
            coord.cancel(&queued.id).unwrap().status,
            JobStatus::Cancelled
        );
        let replacement = coord.submit(list()).unwrap();
        let cancellation = coord.cancel(&running.id).unwrap();
        assert_eq!(cancellation.status, JobStatus::Running);
        assert!(cancellation.cancel_requested);
        release_tx.send(()).unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(finished(&coord, &running.id).status, JobStatus::Cancelled);
        release_tx.send(()).unwrap();
        assert_eq!(
            finished(&coord, &replacement.id).status,
            JobStatus::Completed
        );
        assert_eq!(coord.stats().running, 0);
        assert_eq!(coord.stats().queued, 0);
    }

    #[test]
    fn bounded_history_discards_oldest_terminal_jobs_and_keeps_failures() {
        let fixture = Fixture::new();
        let coord = MobileAgentCoordinator::new(fixture.0.clone(), config(2, 3, 2)).unwrap();
        let first = coord.submit(list()).unwrap();
        assert_eq!(finished(&coord, &first.id).status, JobStatus::Completed);
        let failed = coord
            .submit(WorkspaceJob::Read {
                path: "missing.txt".into(),
            })
            .unwrap();
        assert_eq!(finished(&coord, &failed.id).status, JobStatus::Failed);
        let last = coord.submit(list()).unwrap();
        assert_eq!(finished(&coord, &last.id).status, JobStatus::Completed);
        assert!(coord.get(&first.id).is_err());
        assert_eq!(coord.history().len(), 2);
        assert_eq!(coord.history()[0].id, last.id);
        assert_eq!(coord.history()[1].id, failed.id);
        assert!(coord.history()[1].error.is_some());
        assert!(MobileAgentCoordinator::new(fixture.0.clone(), config(0, 3, 2)).is_err());
        assert!(MobileAgentCoordinator::new(fixture.0.clone(), config(5, 3, 2)).is_err());
    }

    #[test]
    fn two_workers_run_in_parallel_but_third_waits_and_panics_do_not_kill_pool() {
        let fixture = Fixture::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let executor: Arc<JobExecutor> = Arc::new(move |_, job, _| {
            started_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            if let WorkspaceJob::SearchNames { .. } = job {
                panic!("synthetic worker panic");
            }
            Ok(json!({"private":true}))
        });
        let coord =
            MobileAgentCoordinator::with_executor(fixture.0.clone(), config(2, 2, 4), executor)
                .unwrap();
        let first = coord.submit(list()).unwrap();
        let second = coord
            .submit(WorkspaceJob::SearchNames {
                query: "synthetic".into(),
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let third = coord.submit(list()).unwrap();
        assert_eq!(coord.stats().running, 2);
        assert_eq!(coord.stats().queued, 1);
        assert_eq!(
            coord.wait(&third.id, Duration::from_millis(1)).unwrap_err(),
            "Job wait timed out"
        );
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(finished(&coord, &first.id).status, JobStatus::Completed);
        let failed = finished(&coord, &second.id);
        assert_eq!(failed.status, JobStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("Local agent worker failed"));
        assert_eq!(finished(&coord, &third.id).status, JobStatus::Completed);
    }
}
