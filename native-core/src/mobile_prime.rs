//! User-directed Android Prime + two *bounded* AI workers. This is durable
//! coordination, not a persistent background agent, shell, browser controller,
//! tool-execution engine, or a permission grant.
//!
//! The host explicitly invokes the already-approved `/api/mobile-ai-agents`
//! provider route once per spawn/message. This module never makes network
//! calls, schedules tasks, retries provider failures or creates tool authority.
//! A suspended/killed process recovers running workers as Interrupted.
use crate::mobile_ai_delegation::{
    ensure_private_home, replace_atomic, safe_regular, valid_prime_provider_model,
    valid_prime_text, writer_lock,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};
use uuid::Uuid;

const DATA_FILE: &str = "mobile-prime-v1.json";
const MAX_FILE: usize = 512 * 1024;
const MAX_GOAL: usize = 700;
const MAX_LABEL: usize = 80;
const MAX_ASSIGNMENT: usize = 650;
const MAX_MESSAGE: usize = 700;
const MAX_REPORT: usize = 20 * 1024;
const MAX_REPORTS: usize = 12;
const MAX_WORKERS: usize = 2;
const MAX_HISTORY: usize = 12;
const MAX_HISTORY_BYTES: usize = 64 * 1024;
const MAX_HISTORY_SUMMARY: usize = 512;
const CURRENT_VERSION: u8 = 2;
static LOCAL_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Active,
    Finished,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Waiting,
    Running,
    Finished,
    Cancelled,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerReport {
    pub source: String,
    pub text: String,
    /// Only the ordinary AI-delegation provider receipt may set this true.
    pub provider_verified: bool,
    pub delegate_job_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrimeWorker {
    pub id: String,
    pub label: String,
    pub provider: String,
    pub model: String,
    pub assignment: String,
    pub status: WorkerStatus,
    /// Ephemeral provider request UUID, never a token or long-lived authority.
    pub request_id: Option<String>,
    pub reports: Vec<WorkerReport>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrimeTask {
    pub id: String,
    pub goal: String,
    pub status: TaskStatus,
    pub workers: Vec<PrimeWorker>,
    /// Request UUIDs remain reserved after completion/cancellation to reject
    /// user-message replays, including after an app process restart.
    pub used_request_ids: Vec<String>,
    /// This is a *user-written* summary, not an automatically verified result.
    pub summary: Option<String>,
}

/// Past terminal tasks have a compact, deliberately non-executable record.
/// Provider text and request/job UUIDs are excluded: the verified counts are
/// derived only from validated, settled Prime reports at archival time.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrimeHistoryWorker {
    pub id: String,
    pub label: String,
    pub provider: String,
    pub model: String,
    pub assignment: String,
    pub status: WorkerStatus,
    pub report_count: usize,
    pub verified_report_count: usize,
    pub provider_verified: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrimeHistoryTask {
    pub id: String,
    pub goal: String,
    pub status: TaskStatus,
    /// User-written only; bounded excerpt from a finished task, never an AI result.
    pub summary: Option<String>,
    pub summary_user_written: bool,
    pub summary_truncated: bool,
    pub auto_verified: bool,
    pub workers: Vec<PrimeHistoryWorker>,
}

fn prefix_at_utf8_boundary(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) { end -= 1; }
    text[..end].to_owned()
}

impl PrimeHistoryTask {
    fn from_terminal(task: &PrimeTask) -> Result<Self, String> {
        if task.status == TaskStatus::Active {
            return Err("Cannot archive an active Prime task".into());
        }
        let summary = task.summary.as_ref()
            .map(|text|prefix_at_utf8_boundary(text,MAX_HISTORY_SUMMARY));
        Ok(Self {
            id:task.id.clone(),goal:task.goal.clone(),status:task.status,
            summary_truncated:task.summary.as_ref().is_some_and(|text|text.len()>MAX_HISTORY_SUMMARY),
            summary_user_written:task.summary.is_some(),summary,auto_verified:false,
            workers:task.workers.iter().map(|worker| {
                let verified = worker.reports.iter().filter(|report|report.source=="provider"
                    && report.provider_verified && report.delegate_job_id.is_some()).count();
                PrimeHistoryWorker { id:worker.id.clone(),label:worker.label.clone(),
                    provider:worker.provider.clone(),model:worker.model.clone(),
                    assignment:worker.assignment.clone(),status:worker.status,
                    report_count:worker.reports.len(),verified_report_count:verified,
                    provider_verified:verified>0,error:worker.error.clone() }
            }).collect(),
        })
    }

    fn validate(&self) -> bool {
        if self.status == TaskStatus::Active || !uuid_v4(&self.id)
            || !valid_prime_text(&self.goal,MAX_GOAL)
            || self.summary_user_written != self.summary.is_some()
            || self.auto_verified
            || self.summary_user_written != (self.status==TaskStatus::Finished)
            || (self.summary_truncated && self.summary.is_none())
            || self.summary.as_ref().is_some_and(|s|!valid_prime_text(s,MAX_HISTORY_SUMMARY))
            || self.workers.len()>MAX_WORKERS {
            return false;
        }
        let mut ids = std::collections::HashSet::new();
        for worker in &self.workers {
            if !uuid_v4(&worker.id) || !ids.insert(&worker.id)
                || !valid_prime_text(&worker.label,MAX_LABEL)
                || !valid_prime_provider_model(&worker.provider,&worker.model)
                || !valid_prime_text(&worker.assignment,MAX_ASSIGNMENT)
                || matches!(worker.status,WorkerStatus::Running|WorkerStatus::Waiting)
                || (worker.status==WorkerStatus::Finished && !worker.provider_verified)
                || worker.report_count>MAX_REPORTS
                || worker.verified_report_count>worker.report_count
                || worker.provider_verified!=(worker.verified_report_count>0)
                || worker.error.as_ref().is_some_and(|s|!valid_prime_text(s,200))
                || (worker.error.is_some() && !matches!(worker.status,
                    WorkerStatus::Failed|WorkerStatus::Interrupted|WorkerStatus::Cancelled)) {
                return false;
            }
        }
        true
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u8,
    revision: u64,
    task: Option<PrimeTask>,
    /// An absent field is a genuine v1 file; it adds no synthetic old jobs.
    #[serde(default)]
    history: Vec<PrimeHistoryTask>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            revision: 0,
            task: None,
            history: Vec::new(),
        }
    }
}

fn uuid_v4(value: &str) -> bool {
    Uuid::parse_str(value)
        .is_ok_and(|id| id.get_version_num() == 4 && id.hyphenated().to_string() == value)
}

impl State {
    fn validate(&self) -> Result<(), String> {
        let bad = || "Prime metadata corrupt or unsafe; nothing overwritten".to_string();
        if !matches!(self.version,1|CURRENT_VERSION) || self.revision == u64::MAX
            || self.history.len()>MAX_HISTORY
            || (self.version==1 && !self.history.is_empty())
            || serde_json::to_vec(&self.history)
                .map_or(true,|encoded|encoded.len()>MAX_HISTORY_BYTES) {
            return Err(bad());
        }
        let mut task_ids=std::collections::HashSet::new();
        if let Some(current) = &self.task { task_ids.insert(&current.id); }
        for archived in &self.history {
            if !archived.validate() || !task_ids.insert(&archived.id) {
                return Err(bad());
            }
        }
        if let Some(task) = &self.task {
            if !uuid_v4(&task.id)
                || !valid_prime_text(&task.goal, MAX_GOAL)
                || task.workers.len() > MAX_WORKERS
                || task.used_request_ids.len() > MAX_WORKERS * MAX_REPORTS
                || (task.status == TaskStatus::Active && task.summary.is_some())
                || (task.status == TaskStatus::Finished && task.summary.is_none())
                || (task.status == TaskStatus::Cancelled && task.summary.is_some())
                || task
                    .summary
                    .as_ref()
                    .is_some_and(|s| !valid_prime_text(s, MAX_REPORT))
            {
                return Err(bad());
            }
            let mut ids = std::collections::HashSet::new();
            let mut requests = std::collections::HashSet::new();
            if task
                .used_request_ids
                .iter()
                .any(|request| !uuid_v4(request) || !requests.insert(request))
            {
                return Err(bad());
            }
            for worker in &task.workers {
                if !uuid_v4(&worker.id)
                    || !ids.insert(&worker.id)
                    || !valid_prime_text(&worker.label, MAX_LABEL)
                    || !valid_prime_text(&worker.assignment, MAX_ASSIGNMENT)
                    || !valid_prime_provider_model(&worker.provider, &worker.model)
                    || worker.reports.len() > MAX_REPORTS
                    || (matches!(
                        worker.status,
                        WorkerStatus::Waiting | WorkerStatus::Finished
                    ) && !worker.reports.iter().any(|report| report.provider_verified))
                    || (worker.error.is_some()
                        && !matches!(
                            worker.status,
                            WorkerStatus::Failed
                                | WorkerStatus::Interrupted
                                | WorkerStatus::Cancelled
                        ))
                    || worker
                        .error
                        .as_ref()
                        .is_some_and(|s| !valid_prime_text(s, 200))
                {
                    return Err(bad());
                }
                let running = worker.status == WorkerStatus::Running;
                if running != worker.request_id.is_some() {
                    return Err(bad());
                }
                if let Some(req) = &worker.request_id {
                    if !uuid_v4(req) || !requests.contains(req) || task.status != TaskStatus::Active
                    {
                        return Err(bad());
                    }
                }
                for report in &worker.reports {
                    if !valid_prime_text(&report.text, MAX_REPORT)
                        || !matches!(report.source.as_str(), "user" | "provider")
                        || (report.source == "provider") != report.provider_verified
                        || report.provider_verified != report.delegate_job_id.is_some()
                        || report
                            .delegate_job_id
                            .as_deref()
                            .is_some_and(|id| !uuid_v4(id))
                    {
                        return Err(bad());
                    }
                }
            }
            if task.status != TaskStatus::Active
                && task
                    .workers
                    .iter()
                    .any(|w| w.status == WorkerStatus::Running)
            {
                return Err(bad());
            }
            if task.status == TaskStatus::Finished
                && task
                    .workers
                    .iter()
                    .any(|w| w.status == WorkerStatus::Waiting)
            {
                return Err(bad());
            }
        }
        Ok(())
    }
}

fn read_state(home: &Path) -> Result<Option<State>, String> {
    let path = home.join(DATA_FILE);
    let meta = safe_regular(&path)?;
    let Some(meta) = meta else {
        return Ok(None);
    };
    if meta.len() > MAX_FILE as u64 {
        return Err("Prime metadata exceeds limit".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "android", target_os = "linux"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0o400000); // O_NOFOLLOW
    }
    let mut bytes = Vec::new();
    options
        .open(path)
        .map_err(|_| "Cannot open private Prime metadata")?
        .take((MAX_FILE + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read private Prime metadata")?;
    if bytes.len() > MAX_FILE {
        return Err("Prime metadata exceeds limit".into());
    }
    let state: State =
        serde_json::from_slice(&bytes).map_err(|_| "Prime metadata is corrupt; no changes made")?;
    state.validate()?;
    Ok(Some(state))
}

struct Stage(PathBuf);
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn write_state(home: &Path, state: &State) -> Result<(), String> {
    state.validate()?;
    let contents = serde_json::to_vec(state).map_err(|_| "Cannot encode Prime metadata")?;
    if contents.len() > MAX_FILE {
        return Err("Prime metadata exceeds limit".into());
    }
    let target = home.join(DATA_FILE);
    safe_regular(&target)?;
    let stage = Stage(home.join(format!(".mobile-prime-{}.tmp", Uuid::new_v4())));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        #[cfg(any(target_os = "android", target_os = "linux"))]
        options.custom_flags(0o400000);
    }
    let mut file = options
        .open(&stage.0)
        .map_err(|_| "Cannot stage Prime metadata")?;
    file.write_all(&contents)
        .and_then(|_| file.sync_all())
        .map_err(|_| "Cannot persist Prime metadata")?;
    drop(file);
    replace_atomic(&stage.0, &target).map_err(|_| "Cannot commit Prime metadata")?;
    #[cfg(unix)]
    {
        let _ = fs::File::open(home).and_then(|dir| dir.sync_all());
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct PrimeStore {
    home: PathBuf,
}

#[derive(Clone, Debug)]
pub struct WorkerCall {
    pub task_id: String,
    pub worker_id: String,
    pub request_id: String,
    pub provider: String,
    pub model: String,
    pub prompt: String,
}

impl PrimeStore {
    /// Open exactly once per Engine lifetime. Running requests from a prior
    /// process are interrupted, never silently reissued or marked complete.
    pub fn open_or_create(home: &Path) -> Result<Self, String> {
        ensure_private_home(home, true)?;
        let _local = LOCAL_LOCK
            .lock()
            .map_err(|_| "Prime metadata lock poisoned")?;
        let _os = writer_lock(home)?;
        let mut state = read_state(home)?.unwrap_or_default();
        let mut changed = !home.join(DATA_FILE).exists();
        if state.version==1 {
            state.version=CURRENT_VERSION;
            changed=true;
        }
        if let Some(task) = state.task.as_mut() {
            for worker in &mut task.workers {
                if worker.status == WorkerStatus::Running {
                    worker.status = WorkerStatus::Interrupted;
                    worker.request_id = None;
                    worker.error = Some("Interrupted by restart; no automatic retry".into());
                    changed = true;
                }
            }
        }
        if changed {
            state.revision = state
                .revision
                .checked_add(1)
                .ok_or("Prime revision exhausted")?;
            write_state(home, &state)?;
        }
        Ok(Self {
            home: home.to_owned(),
        })
    }

    fn transact<T>(
        &self,
        update: impl FnOnce(&mut State) -> Result<(T, bool), String>,
    ) -> Result<T, String> {
        let _local = LOCAL_LOCK
            .lock()
            .map_err(|_| "Prime metadata lock poisoned")?;
        let _os = writer_lock(&self.home)?;
        let mut state = read_state(&self.home)?.ok_or("Prime state missing")?;
        let (value, changed) = update(&mut state)?;
        if changed {
            state.revision = state
                .revision
                .checked_add(1)
                .ok_or("Prime revision exhausted")?;
            write_state(&self.home, &state)?;
        }
        Ok(value)
    }

    pub fn status(&self) -> Result<Value, String> {
        self.transact(|state| {
            let active=state.task.as_ref().filter(|task|task.status==TaskStatus::Active)
                .map_or(0,|task|task.workers.iter()
                    .filter(|worker|worker.status==WorkerStatus::Running).count());
            let projection=json!({
                    "task":state.task,
                    "history":state.history,
                    "activeWorkerCount":active,
                    "maxHistoryTasks":MAX_HISTORY,
                    "maxConcurrentWorkers":2,
                    "modelToolsExecuted":false,
                    "automaticExecution":false,
                    "backgroundReplay":false
                });
            if serde_json::to_vec(&projection).map_or(true,|bytes|bytes.len()>MAX_FILE+2048) {
                return Err("Prime status exceeds bounded response size".into());
            }
            Ok((projection,false))
        })
    }

    /// Re-check immediately before an owned provider transport is polled.
    /// This closes the gap when Cancel happened before the delegate route had
    /// registered its cancellation channel; stale requests fail closed.
    pub fn still_running(
        &self,
        task_id: &str,
        worker_id: &str,
        request_id: &str,
    ) -> Result<bool, String> {
        self.transact(|state| {
            let active = state.task.as_ref().is_some_and(|task| {
                task.id == task_id
                    && task.status == TaskStatus::Active
                    && task.workers.iter().any(|worker| {
                        worker.id == worker_id
                            && worker.status == WorkerStatus::Running
                            && worker.request_id.as_deref() == Some(request_id)
                    })
            });
            Ok((active, false))
        })
    }

    pub fn start(&self, goal: &str, user_initiated: bool) -> Result<PrimeTask, String> {
        if !user_initiated || !valid_prime_text(goal, MAX_GOAL) {
            return Err("Explicit start and a safe bounded Prime goal required".into());
        }
        self.transact(|state| {
            if state
                .task
                .as_ref()
                .is_some_and(|t| t.status == TaskStatus::Active)
            {
                return Err("Finish or cancel the existing Prime task first".into());
            }
            if let Some(previous) = state.task.as_ref() {
                // History is newest-first. Explicit new starts retire only a
                // genuinely terminal task; never archive a running request.
                let archival=PrimeHistoryTask::from_terminal(previous)?;
                state.history.insert(0,archival);
                while state.history.len()>MAX_HISTORY
                    || serde_json::to_vec(&state.history)
                        .map_or(true,|bytes|bytes.len()>MAX_HISTORY_BYTES) {
                    state.history.pop();
                }
            }
            let task = PrimeTask {
                id: Uuid::new_v4().to_string(),
                goal: goal.to_string(),
                status: TaskStatus::Active,
                workers: Vec::new(),
                used_request_ids: Vec::new(),
                summary: None,
            };
            state.task = Some(task.clone());
            Ok((task, true))
        })
    }

    pub fn spawn(
        &self,
        task_id: &str,
        label: &str,
        provider: &str,
        model: &str,
        assignment: &str,
        request_id: &str,
        user_initiated: bool,
    ) -> Result<WorkerCall, String> {
        if !user_initiated
            || !uuid_v4(task_id)
            || !uuid_v4(request_id)
            || !valid_prime_text(label, MAX_LABEL)
            || !valid_prime_text(assignment, MAX_ASSIGNMENT)
            || !valid_prime_provider_model(provider, model)
        {
            return Err(
                "Explicit worker spawn, valid provider/model and bounded task required".into(),
            );
        }
        self.transact(|state| {
            let task = active_task(state, task_id)?;
            if task.workers.len() >= MAX_WORKERS {
                return Err("Maximum two workers per Prime task".into());
            }
            if task.used_request_ids.iter().any(|id| id == request_id) {
                return Err("Prime provider request UUID already used".into());
            }
            if task
                .workers
                .iter()
                .filter(|w| w.status == WorkerStatus::Running)
                .count()
                >= 2
            {
                return Err("Two Prime worker requests are already running".into());
            }
            let worker = PrimeWorker {
                id: Uuid::new_v4().to_string(),
                label: label.into(),
                provider: provider.into(),
                model: model.into(),
                assignment: assignment.into(),
                status: WorkerStatus::Running,
                request_id: Some(request_id.into()),
                reports: vec![WorkerReport {
                    source: "user".into(),
                    text: assignment.into(),
                    provider_verified: false,
                    delegate_job_id: None,
                }],
                error: None,
            };
            let call = worker_call(task, &worker, assignment, None, request_id)?;
            task.workers.push(worker);
            task.used_request_ids.push(request_id.into());
            Ok((call, true))
        })
    }

    pub fn message(
        &self,
        task_id: &str,
        worker_id: &str,
        message: &str,
        request_id: &str,
        user_initiated: bool,
    ) -> Result<WorkerCall, String> {
        if !user_initiated || !uuid_v4(request_id) || !valid_prime_text(message, MAX_MESSAGE) {
            return Err("Explicit safe worker message and fresh request UUID required".into());
        }
        self.transact(|state| {
            let task = active_task(state, task_id)?;
            if task.used_request_ids.iter().any(|id| id == request_id) {
                return Err("Prime provider request UUID already used".into());
            }
            let index = task
                .workers
                .iter()
                .position(|w| w.id == worker_id)
                .ok_or("Prime worker not found")?;
            if task.workers[index].status == WorkerStatus::Running {
                return Err("Worker already has a provider request in flight".into());
            }
            if !matches!(
                task.workers[index].status,
                WorkerStatus::Waiting | WorkerStatus::Interrupted | WorkerStatus::Failed
            ) {
                return Err(
                    "Worker finished or cancelled; a new user action must spawn a new task".into(),
                );
            }
            if task.workers[index].reports.len() + 2 > MAX_REPORTS {
                return Err("Worker report history full; finish the worker".into());
            }
            let previous = task.workers[index]
                .reports
                .iter()
                .rev()
                .find(|r| r.provider_verified)
                .map(|r| r.text.clone());
            let call = worker_call(
                task,
                &task.workers[index],
                message,
                previous.as_deref(),
                request_id,
            )?;
            let worker = &mut task.workers[index];
            worker.request_id = Some(request_id.into());
            worker.status = WorkerStatus::Running;
            worker.error = None;
            worker.reports.push(WorkerReport {
                source: "user".into(),
                text: message.into(),
                provider_verified: false,
                delegate_job_id: None,
            });
            task.used_request_ids.push(request_id.into());
            Ok((call, true))
        })
    }

    pub fn settle(
        &self,
        task_id: &str,
        worker_id: &str,
        request_id: &str,
        verified_answer: Option<(&str, &str)>,
    ) -> Result<PrimeWorker, String> {
        if !uuid_v4(request_id) {
            return Err("Invalid Prime provider request ID".into());
        }
        if let Some((delegate_job_id, answer)) = verified_answer {
            if !uuid_v4(delegate_job_id) || !valid_prime_text(answer, MAX_REPORT) {
                return Err("Unverified or sensitive provider report".into());
            }
        }
        self.transact(|state| {
            let task = active_task(state, task_id)?;
            let worker = task
                .workers
                .iter_mut()
                .find(|w| w.id == worker_id)
                .ok_or("Prime worker not found")?;
            if worker.status != WorkerStatus::Running
                || worker.request_id.as_deref() != Some(request_id)
            {
                return Err("Prime worker request was cancelled, interrupted or superseded".into());
            }
            worker.request_id = None;
            if let Some((job, answer)) = verified_answer {
                if worker.reports.len() >= MAX_REPORTS {
                    return Err("Worker report history full".into());
                }
                worker.status = WorkerStatus::Waiting;
                worker.reports.push(WorkerReport {
                    source: "provider".into(),
                    text: answer.into(),
                    provider_verified: true,
                    delegate_job_id: Some(job.into()),
                });
            } else {
                worker.status = WorkerStatus::Failed;
                worker.error =
                    Some("Provider call failed or was interrupted; no report verified".into());
            }
            Ok((worker.clone(), true))
        })
    }

    pub fn finish_worker(
        &self,
        task_id: &str,
        worker_id: &str,
        user_initiated: bool,
    ) -> Result<PrimeWorker, String> {
        if !user_initiated {
            return Err("Explicit worker finish required".into());
        }
        self.transact(|state| {
            let task = active_task(state, task_id)?;
            let worker = task
                .workers
                .iter_mut()
                .find(|w| w.id == worker_id)
                .ok_or("Prime worker not found")?;
            if worker.status != WorkerStatus::Waiting
                || !worker.reports.iter().any(|r| r.provider_verified)
            {
                return Err(
                    "Worker requires a verified provider report and no active request".into(),
                );
            }
            worker.status = WorkerStatus::Finished;
            Ok((worker.clone(), true))
        })
    }

    pub fn cancel_worker(
        &self,
        task_id: &str,
        worker_id: &str,
        user_initiated: bool,
    ) -> Result<(PrimeWorker, Option<String>), String> {
        if !user_initiated {
            return Err("Explicit worker cancellation required".into());
        }
        self.transact(|state| {
            let task = active_task(state, task_id)?;
            let worker = task
                .workers
                .iter_mut()
                .find(|w| w.id == worker_id)
                .ok_or("Prime worker not found")?;
            if matches!(
                worker.status,
                WorkerStatus::Cancelled | WorkerStatus::Finished
            ) {
                return Err("Worker is already terminal".into());
            }
            let cancel = worker.request_id.take();
            worker.status = WorkerStatus::Cancelled;
            Ok(((worker.clone(), cancel), true))
        })
    }

    pub fn finish_task(
        &self,
        task_id: &str,
        summary: &str,
        user_initiated: bool,
    ) -> Result<PrimeTask, String> {
        if !user_initiated || !valid_prime_text(summary, MAX_REPORT) {
            return Err("Explicit bounded user summary required".into());
        }
        self.transact(|state| {
            let task = active_task(state, task_id)?;
            if task
                .workers
                .iter()
                .any(|w| w.status == WorkerStatus::Running)
            {
                return Err("Cannot finish Prime while workers are running".into());
            }
            if task
                .workers
                .iter()
                .any(|w| w.status == WorkerStatus::Waiting)
            {
                return Err("Finish or cancel every available worker before closing Prime".into());
            }
            task.status = TaskStatus::Finished;
            task.summary = Some(summary.into());
            Ok((task.clone(), true))
        })
    }

    pub fn cancel_task(
        &self,
        task_id: &str,
        user_initiated: bool,
    ) -> Result<(PrimeTask, Vec<String>), String> {
        if !user_initiated {
            return Err("Explicit Prime task cancellation required".into());
        }
        self.transact(|state| {
            let task = active_task(state, task_id)?;
            let mut requests = Vec::new();
            for worker in &mut task.workers {
                if let Some(request) = worker.request_id.take() {
                    requests.push(request);
                }
                if !matches!(
                    worker.status,
                    WorkerStatus::Finished | WorkerStatus::Cancelled
                        | WorkerStatus::Failed | WorkerStatus::Interrupted
                ) {
                    worker.status = WorkerStatus::Cancelled;
                }
            }
            task.status = TaskStatus::Cancelled;
            Ok(((task.clone(), requests), true))
        })
    }
}

fn active_task<'a>(state: &'a mut State, id: &str) -> Result<&'a mut PrimeTask, String> {
    if !uuid_v4(id) {
        return Err("Invalid Prime task ID".into());
    }
    match state.task.as_mut() {
        Some(task) if task.id == id && task.status == TaskStatus::Active => Ok(task),
        _ => Err("No matching active Prime task".into()),
    }
}

fn worker_call(
    task: &PrimeTask,
    worker: &PrimeWorker,
    instruction: &str,
    previous: Option<&str>,
    request_id: &str,
) -> Result<WorkerCall, String> {
    let previous = previous
        .map(|text| {
            let mut end = text.len().min(500);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            &text[..end]
        })
        .unwrap_or("(none)");
    let prompt = format!(
        "Prime goal: {}\nWorker assignment: {}\nPrevious verified worker report excerpt: {}\nExplicit current request: {}\n\nProvide one bounded AI-only report. Do not claim to run commands, access files, operate browsers, delegate further, or execute tools. No autonomous follow-up.",
        task.goal,worker.assignment,previous,instruction
    );
    if prompt.len() > crate::mobile_ai_delegation::MAX_TASK_BYTES {
        return Err("Prime worker provider prompt exceeds bounded delegate limit".into());
    }
    Ok(WorkerCall {
        task_id: task.id.clone(),
        worker_id: worker.id.clone(),
        request_id: request_id.into(),
        provider: worker.provider.clone(),
        model: worker.model.clone(),
        prompt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case(PathBuf);
    impl Case {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("mcp-prime-{}", Uuid::new_v4())))
        }
        fn store(&self) -> PrimeStore {
            PrimeStore::open_or_create(&self.0).unwrap()
        }
    }
    impl Drop for Case {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn id() -> String {
        Uuid::new_v4().to_string()
    }

    #[test]
    fn history_archives_terminal_tasks_newest_first_rolls_over_and_censors_reports() {
        let case=Case::new();
        let store=case.store();
        let mut past=Vec::new();
        for index in 0..(MAX_HISTORY+5) {
            let task=store.start(&format!("Independent review {index}"),true).unwrap();
            past.push(task.id.clone());
            if index==0 {
                let request=id();
                let call=store.spawn(&task.id,"Analyst","openai","gpt-4o-mini",
                    "Review the requested analysis",&request,true).unwrap();
                store.settle(&task.id,&call.worker_id,&request,
                    Some((&id(),"SENSITIVE_REPORT_PAYLOAD_MUST_NOT_BE_IN_HISTORY"))).unwrap();
                store.finish_worker(&task.id,&call.worker_id,true).unwrap();
                store.finish_task(&task.id,&"Renée completed the review ".repeat(90),true).unwrap();
            } else if index%2==0 {
                store.finish_task(&task.id,"User completed work",true).unwrap();
            } else {
                store.cancel_task(&task.id,true).unwrap();
            }
            assert!(store.status().unwrap()["history"].as_array().unwrap().len()<=MAX_HISTORY);
        }
        let current=store.start("Most recent new task",true).unwrap();
        let status=store.status().unwrap();
        let history=status["history"].as_array().unwrap();
        assert_eq!(status["task"]["id"],current.id);
        assert_eq!(history.len(),MAX_HISTORY);
        assert_eq!(status["maxHistoryTasks"],MAX_HISTORY);
        assert_eq!(status["activeWorkerCount"],0);
        assert_eq!(history[0]["id"],past.last().unwrap().as_str());
        assert_eq!(history[MAX_HISTORY-1]["id"],past[past.len()-MAX_HISTORY].as_str());
        assert!(history.iter().all(|entry|entry["status"]!="active"));
        assert!(history.iter().all(|entry|entry["autoVerified"]==false));
        let history_text=serde_json::to_string(history).unwrap();
        assert!(!history_text.contains("SENSITIVE_REPORT_PAYLOAD_MUST_NOT_BE_IN_HISTORY"));
        assert!(!history_text.contains("requestId"));
        assert!(!history_text.contains("delegate_job_id"));
        assert!(!history_text.contains("used_request_ids"));
        let state=fs::read(case.0.join(DATA_FILE)).unwrap();
        assert!(state.len()<=MAX_FILE);
        assert!(!state.windows(b"SENSITIVE_REPORT_PAYLOAD_MUST_NOT_BE_IN_HISTORY".len())
            .any(|slice|slice==b"SENSITIVE_REPORT_PAYLOAD_MUST_NOT_BE_IN_HISTORY"));
        assert!(store.start("Cannot archive an active one",true).is_err());
        let after=store.status().unwrap();
        assert_eq!(after["history"],status["history"]);
    }

    #[test]
    fn history_preserves_verified_provenance_and_truncates_user_summary() {
        let case=Case::new();
        let store=case.store();
        let task=store.start("Review provider provenance",true).unwrap();
        let call=store.spawn(&task.id,"Verifier","openai","gpt-4o-mini",
            "Check bounded tasks",&id(),true).unwrap();
        store.settle(&task.id,&call.worker_id,&call.request_id,
            Some((&id(),"User-visible verified report which stays out of history"))).unwrap();
        store.finish_worker(&task.id,&call.worker_id,true).unwrap();
        let long_summary="résumé validé ".repeat(180);
        store.finish_task(&task.id,&long_summary,true).unwrap();
        store.start("Explicit next task",true).unwrap();
        let entry=store.status().unwrap()["history"][0].clone();
        assert_eq!(entry["status"],"finished");
        assert_eq!(entry["summaryUserWritten"],true);
        assert_eq!(entry["summaryTruncated"],true);
        assert_eq!(entry["autoVerified"],false);
        assert!(entry["summary"].as_str().unwrap().len()<=MAX_HISTORY_SUMMARY);
        assert_eq!(entry["workers"][0]["status"],"finished");
        assert_eq!(entry["workers"][0]["providerVerified"],true);
        assert_eq!(entry["workers"][0]["verifiedReportCount"],1);
        assert_eq!(entry["workers"][0]["reportCount"],2);
        assert!(entry["workers"][0].get("reports").is_none());
        assert!(entry["workers"][0].get("request_id").is_none());
        assert!(!entry.to_string().contains("User-visible verified report"));
        drop(store);
        let reopened=case.store();
        assert_eq!(reopened.status().unwrap()["history"][0],entry);
    }

    #[test]
    fn v1_migration_interrupts_running_once_and_does_not_create_history_jobs() {
        let case=Case::new();
        let store=case.store();
        let task=store.start("Real unfinished task",true).unwrap();
        let request=store.spawn(&task.id,"Worker","anthropic","claude-3-haiku",
            "Return one report",&id(),true).unwrap();
        let path=case.0.join(DATA_FILE);
        let mut old:Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        old["version"]=json!(1);
        old.as_object_mut().unwrap().remove("history");
        fs::write(&path,old.to_string()).unwrap();
        drop(store);

        let migrated=case.store();
        let result=migrated.status().unwrap();
        assert_eq!(result["task"]["id"],task.id);
        assert_eq!(result["task"]["status"],"active");
        assert_eq!(result["activeWorkerCount"],0);
        assert_eq!(result["history"],json!([]));
        assert_eq!(result["task"]["workers"][0]["status"],"interrupted");
        assert_eq!(result["task"]["workers"][0]["request_id"],Value::Null);
        assert_eq!(result["task"]["used_request_ids"].as_array().unwrap().len(),1);
        assert!(!migrated.still_running(&task.id,&request.worker_id,&request.request_id).unwrap());
        assert!(migrated.start("No active archiving",true).is_err());
        let saved:Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["version"],CURRENT_VERSION);
        assert_eq!(saved["history"],json!([]));
        let revision=saved["revision"].as_u64().unwrap();
        drop(migrated);
        let again=case.store();
        assert_eq!(again.status().unwrap()["activeWorkerCount"],0);
        let saved_again:Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved_again["revision"],revision);
        again.cancel_task(&task.id,true).unwrap();
        again.start("Explicit restart after cancellation",true).unwrap();
        assert_eq!(again.status().unwrap()["history"][0]["workers"][0]["status"],"interrupted");
        assert_eq!(again.status().unwrap()["history"][0]["workers"][0]["providerVerified"],false);
    }

    #[test]
    fn provider_failure_remains_failed_in_cancelled_history_without_false_receipt() {
        let case=Case::new();
        let store=case.store();
        let task=store.start("Review failed provider",true).unwrap();
        let req=store.spawn(&task.id,"Worker","openrouter","model-v1",
            "Attempt one provider answer",&id(),true).unwrap();
        let failed=store.settle(&task.id,&req.worker_id,&req.request_id,None).unwrap();
        assert_eq!(failed.status,WorkerStatus::Failed);
        assert!(store.start("Must not archive active task",true).is_err());
        assert_eq!(store.status().unwrap()["history"],json!([]));
        store.cancel_task(&task.id,true).unwrap();
        store.start("Next approved task",true).unwrap();
        let entry=&store.status().unwrap()["history"][0];
        assert_eq!(entry["status"],"cancelled");
        assert_eq!(entry["summary"],Value::Null);
        assert_eq!(entry["summaryUserWritten"],false);
        assert_eq!(entry["workers"][0]["status"],"failed");
        assert_eq!(entry["workers"][0]["providerVerified"],false);
        assert_eq!(entry["workers"][0]["verifiedReportCount"],0);
        assert_eq!(entry["workers"][0]["reportCount"],1);
        assert!(!store.still_running(&task.id,&req.worker_id,&req.request_id).unwrap());
        assert!(store.settle(&task.id,&req.worker_id,&req.request_id,
            Some((&id(),"Old call may not revive archived worker"))).is_err());
    }

    #[test]
    fn tampered_or_oversized_history_fails_closed_without_overwriting() {
        let case=Case::new();
        let store=case.store();
        let original=store.start("Review history integrity",true).unwrap();
        store.cancel_task(&original.id,true).unwrap();
        store.start("Current unaffected task",true).unwrap();
        let path=case.0.join(DATA_FILE);
        let before:Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for issue in ["active_history","duplicates","extra_privilege","false_verified",
            "large_history","malicious_report"] {
            let mut altered=before.clone();
            match issue {
                "active_history"=>altered["history"][0]["status"]=json!("active"),
                "duplicates"=>altered["history"][0]["id"]=before["task"]["id"].clone(),
                "extra_privilege"=>altered["history"][0]["canExecuteTools"]=json!(true),
                "false_verified"=>altered["history"][0]["autoVerified"]=json!(true),
                "large_history"=>altered["history"]=json!((0..MAX_HISTORY+1)
                    .map(|_|before["history"][0].clone()).collect::<Vec<_>>()),
                "malicious_report"=>altered["history"][0]["reports"]=json!([{"text":"private"}]),
                _=>unreachable!(),
            }
            let bytes=serde_json::to_vec(&altered).unwrap();
            fs::write(&path,&bytes).unwrap();
            assert!(store.status().is_err(),"{issue}: invalid archive accepted");
            assert_eq!(fs::read(&path).unwrap(),bytes,"{issue}: corrupted archive was overwritten");
        }
        let oversize=vec![b'x';MAX_FILE+1];
        fs::write(&path,&oversize).unwrap();
        assert!(store.status().is_err());
        assert_eq!(fs::read(&path).unwrap(),oversize);
        fs::write(&path,serde_json::to_vec(&before).unwrap()).unwrap();
        assert!(store.status().is_ok());
    }

    #[test]
    fn two_workers_real_provider_receipts_and_manual_finish() {
        let case = Case::new();
        let store = case.store();
        assert!(store.start("Review API safety", false).is_err());
        let task = store.start("Review API safety", true).unwrap();
        assert!(store.start("Second concurrent Prime", true).is_err());
        let a = store
            .spawn(
                &task.id,
                "Research",
                "openai",
                "gpt-4o",
                "Review request boundaries",
                &id(),
                true,
            )
            .unwrap();
        assert!(store
            .spawn(
                &task.id,
                "Replay",
                "openai",
                "gpt-4o",
                "Reuse request ID",
                &a.request_id,
                true
            )
            .is_err());
        let b = store
            .spawn(
                &task.id,
                "Tests",
                "anthropic",
                "claude-3",
                "Propose unit coverage",
                &id(),
                true,
            )
            .unwrap();
        assert_ne!(a.worker_id, b.worker_id);
        assert_ne!(a.provider, b.provider);
        assert!(store
            .spawn(
                &task.id,
                "Third",
                "openai",
                "gpt-4o",
                "Unauthorized third worker",
                &id(),
                true
            )
            .is_err());
        assert!(store.finish_worker(&task.id, &a.worker_id, true).is_err());
        assert!(store
            .settle(
                &task.id,
                &a.worker_id,
                &b.request_id,
                Some((&id(), "Wrong provider result"))
            )
            .is_err());
        assert!(store
            .settle(
                &task.id,
                &a.worker_id,
                &a.request_id,
                Some((&id(), "Checked boundaries"))
            )
            .is_ok());
        assert!(store
            .settle(
                &task.id,
                &b.worker_id,
                &b.request_id,
                Some((&id(), "Suggested tests"))
            )
            .is_ok());
        assert!(store.finish_worker(&task.id, &a.worker_id, true).is_ok());
        assert!(store.finish_task(&task.id, "User review", true).is_err());
        let followup = store
            .message(&task.id, &b.worker_id, "Check cancellation", &id(), true)
            .unwrap();
        assert!(store
            .message(&task.id, &b.worker_id, "Replay", &a.request_id, true)
            .is_err());
        assert_eq!(followup.provider, "anthropic");
        assert_eq!(followup.model, "claude-3");
        assert!(store
            .settle(
                &task.id,
                &b.worker_id,
                &followup.request_id,
                Some((&id(), "Cancellation requires gesture"))
            )
            .is_ok());
        store.finish_worker(&task.id, &b.worker_id, true).unwrap();
        let closed = store
            .finish_task(&task.id, "User reviewed both worker answers", true)
            .unwrap();
        assert_eq!(closed.status, TaskStatus::Finished);
        assert_eq!(closed.workers.len(), 2);
        assert!(closed
            .workers
            .iter()
            .all(|w| w.status == WorkerStatus::Finished));
        assert!(store.start("Next explicit Prime", true).is_ok());
    }

    #[test]
    fn cancellation_stale_results_and_restart_do_not_claim_success() {
        let case = Case::new();
        let store = case.store();
        let task = store.start("Check documentation", true).unwrap();
        let request = id();
        let call = store
            .spawn(
                &task.id,
                "Docs",
                "openrouter",
                "model-v1",
                "Summarize docs",
                &request,
                true,
            )
            .unwrap();
        assert!(store
            .still_running(&task.id, &call.worker_id, &request)
            .unwrap());
        let (cancelled, cancel_id) = store
            .cancel_worker(&task.id, &call.worker_id, true)
            .unwrap();
        assert_eq!(cancel_id.as_deref(), Some(request.as_str()));
        assert_eq!(cancelled.status, WorkerStatus::Cancelled);
        assert!(!store
            .still_running(&task.id, &call.worker_id, &request)
            .unwrap());
        assert!(store
            .settle(
                &task.id,
                &call.worker_id,
                &request,
                Some((&id(), "Late result"))
            )
            .is_err());
        let second = store
            .spawn(
                &task.id,
                "Tests",
                "openai",
                "gpt-4o",
                "Propose tests",
                &id(),
                true,
            )
            .unwrap();
        drop(store);
        let restarted = case.store();
        let status = restarted.status().unwrap();
        assert_eq!(
            status["task"]["used_request_ids"].as_array().unwrap().len(),
            2
        );
        assert_eq!(status["task"]["workers"][1]["status"], "interrupted");
        assert_eq!(status["task"]["workers"][1]["request_id"], Value::Null);
        assert!(!restarted
            .still_running(&task.id, &second.worker_id, &second.request_id)
            .unwrap());
        assert!(restarted
            .settle(
                &task.id,
                &second.worker_id,
                &second.request_id,
                Some((&id(), "Fake restart success"))
            )
            .is_err());
        assert!(restarted
            .message(&task.id, &second.worker_id, "Explicit retry", &id(), false)
            .is_err());
        let manual = restarted
            .message(&task.id, &second.worker_id, "Explicit retry", &id(), true)
            .unwrap();
        assert_eq!(manual.provider, "openai");
        restarted
            .settle(
                &task.id,
                &second.worker_id,
                &manual.request_id,
                Some((&id(), "Reviewed test plan")),
            )
            .unwrap();
        restarted
            .finish_worker(&task.id, &second.worker_id, true)
            .unwrap();
        assert!(restarted.finish_task(&task.id, "Reviewed", true).is_ok());
    }

    #[test]
    fn requests_reject_bad_providers_secrets_and_tampered_private_state() {
        let case = Case::new();
        let store = case.store();
        assert!(store.start("Bearer secret credential", true).is_err());
        let task = store.start("Draft product notes", true).unwrap();
        for provider in ["fake", "chatgpt-browser", "ollama"] {
            assert!(store
                .spawn(
                    &task.id,
                    "Research",
                    provider,
                    "any-model",
                    "Review task",
                    &id(),
                    true
                )
                .is_err());
        }
        assert!(store
            .spawn(
                &task.id,
                "Work",
                "openai",
                "gpt-4o",
                "Authorization: secret",
                &id(),
                true
            )
            .is_err());
        assert!(store
            .spawn(
                &task.id,
                "Work",
                "openai",
                "gpt-4o",
                "Review task",
                &id(),
                false
            )
            .is_err());
        let call = store
            .spawn(
                &task.id,
                "Work",
                "openai",
                "gpt-4o",
                "Review task",
                &id(),
                true,
            )
            .unwrap();
        assert!(store
            .settle(
                &task.id,
                &call.worker_id,
                &call.request_id,
                Some((&id(), "sk-proj-secret-key-should-not-persist"))
            )
            .is_err());
        assert_eq!(
            store.status().unwrap()["task"]["workers"][0]["status"],
            "running"
        );
        store.cancel_task(&task.id, true).unwrap();
        drop(store);
        fs::write(
            case.0.join(DATA_FILE),
            r#"{"version":3,"revision":1,"task":null}"#,
        )
        .unwrap();
        assert!(PrimeStore::open_or_create(&case.0).is_err());
    }

    #[test]
    fn persisted_prime_state_rejects_inconsistent_verified_or_finished_metadata_without_overwrite()
    {
        let case = Case::new();
        let store = case.store();
        let task = store.start("Audit metadata integrity", true).unwrap();
        let call = store
            .spawn(
                &task.id,
                "Worker",
                "openai",
                "gpt-4o-mini",
                "Return a bounded report",
                &id(),
                true,
            )
            .unwrap();
        assert!(store
            .still_running(&task.id, &call.worker_id, &call.request_id)
            .unwrap());
        let path = case.0.join(DATA_FILE);
        let original = fs::read(&path).unwrap();
        for kind in [
            "active_summary",
            "unverified_waiting",
            "finished_without_summary",
            "cancelled_with_summary",
            "running_with_error",
            "forged_report",
            "unknown_fields",
        ] {
            let mut data: Value = serde_json::from_slice(&original).unwrap();
            match kind {
                "active_summary" => data["task"]["summary"] = json!("Not user-confirmed"),
                "unverified_waiting" => {
                    data["task"]["workers"][0]["status"] = json!("waiting");
                    data["task"]["workers"][0]["request_id"] = Value::Null;
                }
                "finished_without_summary" => {
                    data["task"]["status"] = json!("finished");
                    data["task"]["workers"][0]["status"] = json!("cancelled");
                    data["task"]["workers"][0]["request_id"] = Value::Null;
                }
                "cancelled_with_summary" => {
                    data["task"]["status"] = json!("cancelled");
                    data["task"]["summary"] = json!("A summary cannot prove cancellation");
                    data["task"]["workers"][0]["status"] = json!("cancelled");
                    data["task"]["workers"][0]["request_id"] = Value::Null;
                }
                "running_with_error" => {
                    data["task"]["workers"][0]["error"] = json!("Contradictory error field");
                }
                "forged_report" => {
                    data["task"]["workers"][0]["reports"][0]["provider_verified"] = json!(true);
                }
                "unknown_fields" => data["task"]["workers"][0]["privileged"] = json!(true),
                _ => unreachable!(),
            }
            let corrupt = serde_json::to_vec(&data).unwrap();
            fs::write(&path, &corrupt).unwrap();
            assert!(
                store.status().is_err(),
                "Corrupt {kind} metadata was accepted"
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                corrupt,
                "Corrupt {kind} metadata was overwritten"
            );
            fs::write(&path, &original).unwrap();
        }
        assert!(store
            .still_running(&task.id, &call.worker_id, &call.request_id)
            .unwrap());
    }

    #[test]
    fn concurrent_transactions_never_admit_third_worker() {
        let case = Case::new();
        let store = std::sync::Arc::new(case.store());
        let task = store.start("Review safety tests", true).unwrap();
        let mut handles = Vec::new();
        for n in 0..6 {
            let store = store.clone();
            let task_id = task.id.clone();
            handles.push(std::thread::spawn(move || {
                store
                    .spawn(
                        &task_id,
                        &format!("Worker {n}"),
                        "openai",
                        "gpt-4o",
                        "Review bounded details",
                        &id(),
                        true,
                    )
                    .is_ok()
            }));
        }
        assert_eq!(
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|ok| *ok)
                .count(),
            2
        );
        let status = store.status().unwrap();
        assert_eq!(status["task"]["workers"].as_array().unwrap().len(), 2);
    }
}
