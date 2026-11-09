//! Explicit, bounded native goal state machine for a caller-controlled Android UI.
//!
//! No work is run by this module. The caller executes one real provider/tool turn,
//! then supplies its actual outcome together with the issued turn ID. Approvals
//! must originate from a user gesture. A recovered in-flight turn never resumes
//! automatically. Each goal owns one checkpoint in a dedicated app-private folder
//! provided by the caller; each folder is locked against concurrent writers.

use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
use uuid::Uuid;

const CHECKPOINT: &str = "goal-v1.json";
const MAX_CHECKPOINT_BYTES: u64 = 128 * 1024;
const MAX_STEPS: usize = 16;
const MAX_TURNS: u16 = 64;
const MAX_RETRIES: u8 = 5;
const MAX_APPROVALS: usize = MAX_TURNS as usize + MAX_STEPS;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GoalStep {
    pub description: String,
    pub requires_approval: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GoalSpec {
    pub title: String,
    pub steps: Vec<GoalStep>,
    pub max_turns: u16,
    pub max_retries_per_step: u8,
}

impl GoalSpec {
    fn validate(&self) -> Result<(), String> {
        validate_text(&self.title, 200, "Goal title")?;
        if self.steps.is_empty() || self.steps.len() > MAX_STEPS {
            return Err("Goal needs 1-16 fixed steps".into());
        }
        if self.max_turns < self.steps.len() as u16 || self.max_turns > MAX_TURNS {
            return Err("Goal turn budget must cover each step and stay within 64".into());
        }
        if self.max_retries_per_step > MAX_RETRIES {
            return Err("Goal allows at most five retries per step".into());
        }
        for step in &self.steps {
            validate_text(&step.description, 240, "Step description")?;
        }
        Ok(())
    }
}

fn validate_text(text: &str, max_bytes: usize, kind: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > max_bytes || text.chars().any(char::is_control) {
        return Err(format!("{kind} requires 1-{max_bytes} printable bytes"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Ready,
    AwaitingApproval,
    /// Explicitly parked before any provider turn, never runnable until resume().
    Paused,
    InTurn,
    Completed,
    Failed,
    Rejected,
    Cancelled,
}

impl GoalStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Rejected | Self::Cancelled
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "detail", rename_all = "snake_case")]
pub enum TurnOutcome {
    /// Caller attests this step actually completed; evidence is only caller-supplied.
    Completed(String),
    /// Bounded retry only when remaining turn and retry budgets permit.
    RetryableFailure(String),
    /// Permanent failure of this goal.
    TerminalFailure(String),
    /// A real turn requested explicit approval before proceeding.
    NeedsApproval(String),
}

impl TurnOutcome {
    fn validate(&self) -> Result<(), String> {
        let detail = match self {
            Self::Completed(value)
            | Self::RetryableFailure(value)
            | Self::TerminalFailure(value)
            | Self::NeedsApproval(value) => value,
        };
        validate_text(detail, 1024, "Turn outcome")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnTicket {
    pub id: String,
    pub step_index: usize,
    pub attempt: u8,
    pub turn_number: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnRecord {
    pub ticket: TurnTicket,
    pub outcome: TurnOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRecord {
    pub step_index: usize,
    pub approved: bool,
    pub user_note: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalSnapshot {
    pub version: u8,
    pub revision: u64,
    pub id: String,
    pub spec: GoalSpec,
    pub status: GoalStatus,
    pub current_step: usize,
    pub turns_used: u16,
    pub retries_used_on_step: u8,
    pub active_turn: Option<TurnTicket>,
    /// Identifies an unfinished caller-run turn discarded by cancellation.
    /// Optional for compatibility with checkpoints written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_turn: Option<TurnTicket>,
    pub approval_reason: Option<String>,
    pub approved_for_step: bool,
    /// The exact pre-pause gate. An absent field is the original v1 format.
    /// Only Ready or AwaitingApproval is accepted on a Paused checkpoint;
    /// no in-flight provider turn may be paused or resumed implicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_from: Option<GoalStatus>,
    pub history: Vec<TurnRecord>,
    pub approvals: Vec<ApprovalRecord>,
    pub stop_reason: Option<String>,
}

impl GoalSnapshot {
    pub fn completed_steps(&self) -> usize {
        self.current_step
    }

    /// Based strictly on acknowledged successful steps, never speculative work.
    pub fn progress_percent(&self) -> u8 {
        ((self.current_step * 100) / self.spec.steps.len()) as u8
    }

    pub fn turns_remaining(&self) -> u16 {
        self.spec.max_turns - self.turns_used
    }

    fn validate(&self) -> Result<(), String> {
        let corrupt = || String::from("Goal checkpoint is inconsistent; no changes were made");
        if self.version != 1 || Uuid::parse_str(&self.id).is_err() || self.revision == u64::MAX {
            return Err(corrupt());
        }
        // A genuine pause remembers its exact gate; all other statuses must
        // NOT have a spare resume target. No status rewrite can bypass a
        // pending manual approval, turn, or terminal outcome.
        let effective_status = match (self.status, self.paused_from) {
            (GoalStatus::Paused, Some(GoalStatus::Ready)) => GoalStatus::Ready,
            (GoalStatus::Paused, Some(GoalStatus::AwaitingApproval)) => {
                GoalStatus::AwaitingApproval
            }
            (GoalStatus::Paused, _) | (_, Some(_)) => return Err(corrupt()),
            (other, None) => other,
        };
        self.spec.validate().map_err(|_| corrupt())?;
        if self.current_step > self.spec.steps.len()
            || self.turns_used > self.spec.max_turns
            || self.retries_used_on_step > self.spec.max_retries_per_step.saturating_add(1)
            || self.history.len() > MAX_TURNS as usize
            || self.approvals.len() > MAX_APPROVALS
        {
            return Err(corrupt());
        }
        if let Some(reason) = &self.approval_reason {
            validate_text(reason, 1024, "Approval reason").map_err(|_| corrupt())?;
        }
        if let Some(reason) = &self.stop_reason {
            validate_text(reason, 1024, "Stop reason").map_err(|_| corrupt())?;
        }
        let mut successful_steps = 0;
        let mut retry_count = 0u8;
        let mut ids = HashSet::new();
        for (position, record) in self.history.iter().enumerate() {
            record.outcome.validate().map_err(|_| corrupt())?;
            if Uuid::parse_str(&record.ticket.id).is_err()
                || !ids.insert(record.ticket.id.as_str())
                || record.ticket.turn_number != position as u16 + 1
                || record.ticket.step_index != successful_steps
                || record.ticket.attempt != retry_count + 1
            {
                return Err(corrupt());
            }
            match &record.outcome {
                TurnOutcome::Completed(_) => {
                    successful_steps += 1;
                    retry_count = 0;
                }
                TurnOutcome::RetryableFailure(_) => {
                    retry_count = retry_count.checked_add(1).ok_or_else(corrupt)?;
                    if retry_count > self.spec.max_retries_per_step
                        && position + 1 < self.history.len()
                    {
                        return Err(corrupt());
                    }
                }
                TurnOutcome::TerminalFailure(_) => {
                    if position + 1 < self.history.len() {
                        return Err(corrupt());
                    }
                }
                TurnOutcome::NeedsApproval(_) => {}
            }
        }
        if successful_steps != self.current_step
            || self.retries_used_on_step != retry_count
            || self.history.len() > self.turns_used as usize
        {
            return Err(corrupt());
        }
        for (position, approval) in self.approvals.iter().enumerate() {
            if approval.step_index > self.current_step
                || approval.step_index >= self.spec.steps.len()
                || approval.user_note.len() > 256
                || approval.user_note.chars().any(char::is_control)
                || (position > 0 && self.approvals[position - 1].step_index > approval.step_index)
                || (!approval.approved && position + 1 != self.approvals.len())
            {
                return Err(corrupt());
            }
        }
        if let Some(last) = self.approvals.last() {
            if !last.approved
                && (self.status != GoalStatus::Rejected || last.step_index != self.current_step)
            {
                return Err(corrupt());
            }
        }
        if self.status == GoalStatus::Rejected
            && !self.approvals.last().is_some_and(|approval| {
                !approval.approved && approval.step_index == self.current_step
            })
        {
            return Err(corrupt());
        }
        if self.approved_for_step
            && !self.approvals.last().is_some_and(|approval| {
                approval.step_index == self.current_step && approval.approved
            })
        {
            return Err(corrupt());
        }
        if self.status != GoalStatus::Cancelled && self.cancelled_turn.is_some() {
            return Err(corrupt());
        }
        if self.status == GoalStatus::InTurn {
            let turn = self.active_turn.as_ref().ok_or_else(corrupt)?;
            if Uuid::parse_str(&turn.id).is_err()
                || ids.contains(turn.id.as_str())
                || turn.step_index != self.current_step
                || turn.turn_number != self.turns_used
                || turn.attempt != self.retries_used_on_step + 1
                || self.history.len() + 1 != self.turns_used as usize
                || self.approval_reason.is_some()
            {
                return Err(corrupt());
            }
        } else if self.active_turn.is_some() || self.history.len() != self.turns_used as usize {
            // Cancellation while a provider call was in flight intentionally has no
            // invented outcome, so it may leave exactly one unmatched used turn.
            if self.status != GoalStatus::Cancelled
                || self.active_turn.is_some()
                || self.history.len() + 1 != self.turns_used as usize
            {
                return Err(corrupt());
            }
        }
        if let Some(cancelled) = &self.cancelled_turn {
            if Uuid::parse_str(&cancelled.id).is_err()
                || ids.contains(cancelled.id.as_str())
                || self.history.len() + 1 != self.turns_used as usize
                || cancelled.turn_number != self.turns_used
                || cancelled.step_index != self.current_step
                || cancelled.attempt != self.retries_used_on_step + 1
            {
                return Err(corrupt());
            }
        }
        // A terminal provider outcome cannot later be converted into a runnable
        // checkpoint simply by editing its status field.
        if let Some(last) = self.history.last() {
            match &last.outcome {
                TurnOutcome::TerminalFailure(reason) => {
                    if self.status != GoalStatus::Failed
                        || self.stop_reason.as_deref() != Some(reason)
                    {
                        return Err(corrupt());
                    }
                }
                TurnOutcome::RetryableFailure(reason)
                    if self.retries_used_on_step > self.spec.max_retries_per_step
                        || (self.turns_used == self.spec.max_turns
                            && self.history.len() == self.turns_used as usize) =>
                {
                    if self.status != GoalStatus::Failed
                        || self.stop_reason.as_deref() != Some(reason)
                    {
                        return Err(corrupt());
                    }
                }
                TurnOutcome::NeedsApproval(_) | TurnOutcome::Completed(_)
                    if self.turns_used == self.spec.max_turns
                        && self.history.len() == self.turns_used as usize
                        && self.current_step < self.spec.steps.len() =>
                {
                    if self.status != GoalStatus::Failed
                        || self.stop_reason.as_deref() != Some("Turn budget exhausted")
                    {
                        return Err(corrupt());
                    }
                }
                _ => {
                    if self.status == GoalStatus::Failed {
                        return Err(corrupt());
                    }
                }
            }
            if matches!(last.outcome, TurnOutcome::NeedsApproval(_))
                && last.ticket.step_index == self.current_step
                && matches!(effective_status, GoalStatus::Ready | GoalStatus::InTurn)
                && !self.approved_for_step
            {
                return Err(corrupt());
            }
        } else if self.status == GoalStatus::Failed {
            return Err(corrupt());
        }
        if self.status == GoalStatus::Completed {
            if self.current_step != self.spec.steps.len()
                || self.approval_reason.is_some()
                || self.stop_reason.is_some()
            {
                return Err(corrupt());
            }
        } else if self.current_step >= self.spec.steps.len() {
            return Err(corrupt());
        }
        match effective_status {
            GoalStatus::AwaitingApproval => {
                if self.approval_reason.is_none()
                    || self.approved_for_step
                    || self.stop_reason.is_some()
                    || self.turns_used >= self.spec.max_turns
                {
                    return Err(corrupt());
                }
            }
            GoalStatus::Ready | GoalStatus::InTurn => {
                if self.approval_reason.is_some() || self.stop_reason.is_some() {
                    return Err(corrupt());
                }
                if self.spec.steps[self.current_step].requires_approval && !self.approved_for_step {
                    return Err(corrupt());
                }
                if self.status == GoalStatus::Ready && self.turns_used >= self.spec.max_turns {
                    return Err(corrupt());
                }
            }
            GoalStatus::Failed | GoalStatus::Rejected | GoalStatus::Cancelled => {
                if self.stop_reason.is_none() || self.approval_reason.is_some() {
                    return Err(corrupt());
                }
            }
            GoalStatus::Paused => return Err(corrupt()),
            GoalStatus::Completed => {}
        }
        Ok(())
    }
}

static ACTIVE_WRITERS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// Prevents concurrent writers in this process and cooperating OS processes.
struct FolderLease {
    path: PathBuf,
    lock_file: File,
}

impl FolderLease {
    fn acquire(path: &Path) -> Result<Self, String> {
        let guard = ACTIVE_WRITERS.get_or_init(|| Mutex::new(HashSet::new()));
        {
            let mut writers = guard
                .lock()
                .map_err(|_| "Goal writer registry unavailable")?;
            if !writers.insert(path.to_owned()) {
                return Err("Goal folder already has an active writer".into());
            }
        }
        let acquire = (|| -> Result<File, String> {
            let path = path.join(".goal-v1.lock");
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
                Ok(_) => return Err("Goal lock is an unsupported file type".into()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return Err("Cannot inspect goal lock".into()),
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&path)
                .map_err(|_| "Cannot open private goal lock")?;
            file.try_lock()
                .map_err(|_| "Goal folder is locked by another process")?;
            Ok(file)
        })();
        match acquire {
            Ok(lock_file) => Ok(Self {
                path: path.to_owned(),
                lock_file,
            }),
            Err(error) => {
                if let Ok(mut writers) = guard.lock() {
                    writers.remove(path);
                }
                Err(error)
            }
        }
    }
}

impl Drop for FolderLease {
    fn drop(&mut self) {
        let _ = self.lock_file.unlock();
        if let Some(registry) = ACTIVE_WRITERS.get() {
            if let Ok(mut writers) = registry.lock() {
                writers.remove(&self.path);
            }
        }
    }
}

fn private_folder(path: &Path) -> Result<PathBuf, String> {
    if fs::symlink_metadata(path)
        .map_err(|_| "Private goal folder must already exist")?
        .file_type()
        .is_symlink()
    {
        return Err("Goal folder must not be a symbolic link".into());
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| "Private goal folder unavailable")?;
    if !canonical.is_dir() {
        return Err("Goal folder is not a directory".into());
    }
    Ok(canonical)
}

fn existing_checkpoint(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => Ok(true),
        Ok(_) => Err("Goal checkpoint is an unsupported file type".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Cannot inspect goal checkpoint".into()),
    }
}

fn read_checkpoint(dir: &Path) -> Result<GoalSnapshot, String> {
    let path = dir.join(CHECKPOINT);
    if !existing_checkpoint(&path)? {
        return Err("Goal checkpoint does not exist".into());
    }
    let file = File::open(&path).map_err(|_| "Cannot open goal checkpoint")?;
    if file
        .metadata()
        .map_err(|_| "Cannot inspect goal checkpoint")?
        .len()
        > MAX_CHECKPOINT_BYTES
    {
        return Err("Goal checkpoint too large; no changes were made".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_CHECKPOINT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read goal checkpoint")?;
    if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
        return Err("Goal checkpoint too large; no changes were made".into());
    }
    let state: GoalSnapshot = serde_json::from_slice(&bytes)
        .map_err(|_| "Goal checkpoint is corrupt; no changes were made")?;
    state.validate()?;
    Ok(state)
}

#[cfg(windows)]
fn replace_atomically(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let src: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // Same-directory replacement (same volume), including existing targets.
    const REPLACE_EXISTING: u32 = 0x1;
    const WRITE_THROUGH: u32 = 0x8;
    if unsafe { MoveFileExW(src.as_ptr(), dst.as_ptr(), REPLACE_EXISTING | WRITE_THROUGH) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(windows))]
fn replace_atomically(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

fn persist(dir: &Path, state: &GoalSnapshot) -> Result<(), String> {
    state.validate()?;
    let bytes = serde_json::to_vec(state).map_err(|_| "Cannot encode goal checkpoint")?;
    if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
        return Err("Goal checkpoint exceeds 128 KiB limit".into());
    }
    let target = dir.join(CHECKPOINT);
    // If another actor replaced our checkpoint with a link, fail closed.
    let _ = existing_checkpoint(&target)?;
    let temporary = dir.join(format!("goal-{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<(), String> {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| "Cannot create temporary goal checkpoint")?;
        output
            .write_all(&bytes)
            .map_err(|_| "Cannot write goal checkpoint")?;
        output
            .sync_all()
            .map_err(|_| "Cannot sync goal checkpoint")?;
        drop(output);
        replace_atomically(&temporary, &target)
            .map_err(|_| "Cannot atomically replace goal checkpoint")?;
        #[cfg(unix)]
        File::open(dir)
            .and_then(|folder| folder.sync_all())
            .map_err(|_| "Cannot sync checkpoint directory")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub struct GoalDriver {
    dir: PathBuf,
    snapshot: GoalSnapshot,
    poisoned: bool,
    _lease: FolderLease,
}

impl GoalDriver {
    /// `app_private_goal_folder` must be a dedicated, existing private directory.
    /// Creating in an occupied folder fails, preserving any existing checkpoint.
    pub fn create(app_private_goal_folder: &Path, spec: GoalSpec) -> Result<Self, String> {
        spec.validate()?;
        let dir = private_folder(app_private_goal_folder)?;
        let lease = FolderLease::acquire(&dir)?;
        if existing_checkpoint(&dir.join(CHECKPOINT))? {
            return Err("Goal already exists in this private folder".into());
        }
        let initial_approval = spec.steps[0].requires_approval;
        let snapshot = GoalSnapshot {
            version: 1,
            revision: 0,
            id: Uuid::new_v4().to_string(),
            spec,
            status: if initial_approval {
                GoalStatus::AwaitingApproval
            } else {
                GoalStatus::Ready
            },
            current_step: 0,
            turns_used: 0,
            retries_used_on_step: 0,
            active_turn: None,
            cancelled_turn: None,
            approval_reason: initial_approval.then(|| "Step requires manual approval".into()),
            approved_for_step: false,
            paused_from: None,
            history: vec![],
            approvals: vec![],
            stop_reason: None,
        };
        persist(&dir, &snapshot)?;
        Ok(Self {
            dir,
            snapshot,
            poisoned: false,
            _lease: lease,
        })
    }

    /// Opening never runs or replays an in-flight turn.
    pub fn open(app_private_goal_folder: &Path) -> Result<Self, String> {
        let dir = private_folder(app_private_goal_folder)?;
        let lease = FolderLease::acquire(&dir)?;
        let snapshot = read_checkpoint(&dir)?;
        Ok(Self {
            dir,
            snapshot,
            poisoned: false,
            _lease: lease,
        })
    }

    pub fn snapshot(&self) -> GoalSnapshot {
        self.snapshot.clone()
    }

    /// Explicit, persisted pause of an idle gate. Never pause an active
    /// provider/tool call: the caller must cancel/complete that real request
    /// first. The checkpoint keeps the exact approval state, step and budgets.
    pub fn pause(&mut self) -> Result<GoalSnapshot, String> {
        self.change(|state| {
            if !matches!(state.status, GoalStatus::Ready | GoalStatus::AwaitingApproval) {
                return Err("Only a ready or awaiting-approval goal can be paused; stop any active provider turn first".into());
            }
            state.paused_from = Some(state.status);
            state.status = GoalStatus::Paused;
            Ok(())
        })
    }

    /// Explicit, persisted resume. Does not issue a ticket, run a tool,
    /// approve a step, or resume any previously cancelled provider request.
    pub fn resume(&mut self) -> Result<GoalSnapshot, String> {
        self.change(|state| {
            if state.status != GoalStatus::Paused {
                return Err("Goal is not paused".into());
            }
            let prior = state.paused_from.ok_or("Paused goal has no resume gate")?;
            if !matches!(prior, GoalStatus::Ready | GoalStatus::AwaitingApproval) {
                return Err("Paused goal has an invalid resume gate".into());
            }
            state.status = prior;
            state.paused_from = None;
            Ok(())
        })
    }

    /// Mutations persist before publishing a new in-memory snapshot. On I/O
    /// uncertainty the instance blocks all subsequent actions until reopened.
    fn change(
        &mut self,
        apply: impl FnOnce(&mut GoalSnapshot) -> Result<(), String>,
    ) -> Result<GoalSnapshot, String> {
        if self.poisoned {
            return Err("Goal storage error; reopen to recover".into());
        }
        // Detect unexpected external changes before attempting a replacement.
        let disk = read_checkpoint(&self.dir).map_err(|err| {
            self.poisoned = true;
            err
        })?;
        if disk != self.snapshot {
            self.poisoned = true;
            return Err("Goal checkpoint changed externally; reopen required".into());
        }
        let mut next = self.snapshot.clone();
        apply(&mut next)?;
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or("Goal revision limit reached")?;
        if let Err(err) = persist(&self.dir, &next) {
            self.poisoned = true;
            return Err(err);
        }
        self.snapshot = next;
        Ok(self.snapshot.clone())
    }

    /// The returned ticket authorizes exactly one caller-run turn. No run starts here.
    pub fn begin_turn(&mut self) -> Result<TurnTicket, String> {
        let updated = self.change(|state| {
            if state.status != GoalStatus::Ready {
                return Err("Goal is not ready for a turn".into());
            }
            if state.turns_used >= state.spec.max_turns {
                return Err("Goal turn budget exhausted".into());
            }
            state.turns_used += 1;
            let ticket = TurnTicket {
                id: Uuid::new_v4().to_string(),
                step_index: state.current_step,
                attempt: state.retries_used_on_step + 1,
                turn_number: state.turns_used,
            };
            state.active_turn = Some(ticket.clone());
            state.status = GoalStatus::InTurn;
            Ok(())
        })?;
        updated.active_turn.ok_or("Created turn missing".into())
    }

    /// Only the caller knows whether its real provider/tool action completed.
    /// Matching turn IDs prevent delayed/duplicate callbacks changing new steps.
    pub fn record_outcome(
        &mut self,
        turn_id: &str,
        outcome: TurnOutcome,
    ) -> Result<GoalSnapshot, String> {
        outcome.validate()?;
        self.change(|state| {
            if state.status != GoalStatus::InTurn {
                return Err("No active goal turn".into());
            }
            let active = state.active_turn.as_ref().ok_or("Active turn missing")?;
            if active.id != turn_id {
                return Err("Stale or incorrect turn ID".into());
            }
            state.history.push(TurnRecord {
                ticket: active.clone(),
                outcome: outcome.clone(),
            });
            state.active_turn = None;
            match outcome {
                TurnOutcome::Completed(_) => {
                    state.current_step += 1;
                    state.retries_used_on_step = 0;
                    state.approved_for_step = false;
                    if state.current_step == state.spec.steps.len() {
                        state.status = GoalStatus::Completed;
                    } else if state.turns_used == state.spec.max_turns {
                        state.status = GoalStatus::Failed;
                        state.stop_reason = Some("Turn budget exhausted".into());
                    } else if state.spec.steps[state.current_step].requires_approval {
                        state.status = GoalStatus::AwaitingApproval;
                        state.approval_reason = Some("Step requires manual approval".into());
                    } else {
                        state.status = GoalStatus::Ready;
                    }
                }
                TurnOutcome::RetryableFailure(reason) => {
                    state.retries_used_on_step += 1;
                    if state.retries_used_on_step > state.spec.max_retries_per_step
                        || state.turns_used == state.spec.max_turns
                    {
                        state.status = GoalStatus::Failed;
                        state.stop_reason = Some(reason);
                    } else {
                        state.status = GoalStatus::Ready;
                    }
                }
                TurnOutcome::TerminalFailure(reason) => {
                    state.status = GoalStatus::Failed;
                    state.stop_reason = Some(reason);
                }
                TurnOutcome::NeedsApproval(reason) => {
                    if state.turns_used == state.spec.max_turns {
                        state.status = GoalStatus::Failed;
                        state.stop_reason = Some("Turn budget exhausted".into());
                    } else {
                        state.status = GoalStatus::AwaitingApproval;
                        state.approved_for_step = false;
                        state.approval_reason = Some(reason);
                    }
                }
            }
            Ok(())
        })
    }

    /// Records an actually interrupted turn, never fabricates model/tool output.
    pub fn mark_turn_interrupted(
        &mut self,
        turn_id: &str,
        reason: &str,
    ) -> Result<GoalSnapshot, String> {
        self.record_outcome(turn_id, TurnOutcome::RetryableFailure(reason.to_owned()))
    }

    /// Caller may insert an approval gate before any ready step.
    pub fn request_approval(&mut self, reason: &str) -> Result<GoalSnapshot, String> {
        validate_text(reason, 1024, "Approval reason")?;
        self.change(|state| {
            if state.status != GoalStatus::Ready {
                return Err("Approval can only be requested before a turn".into());
            }
            state.status = GoalStatus::AwaitingApproval;
            state.approved_for_step = false;
            state.approval_reason = Some(reason.to_owned());
            Ok(())
        })
    }

    /// Invoke only after a real, explicit manual decision in the UI.
    pub fn resolve_approval(
        &mut self,
        approved: bool,
        user_note: &str,
    ) -> Result<GoalSnapshot, String> {
        if user_note.len() > 256 || user_note.chars().any(char::is_control) {
            return Err("Approval note exceeds 256 printable bytes".into());
        }
        self.change(|state| {
            if state.status != GoalStatus::AwaitingApproval {
                return Err("No pending manual approval".into());
            }
            if state.approvals.len() >= MAX_APPROVALS {
                return Err("Manual approval history limit reached".into());
            }
            state.approvals.push(ApprovalRecord {
                step_index: state.current_step,
                approved,
                user_note: user_note.to_owned(),
            });
            state.approval_reason = None;
            if approved {
                state.approved_for_step = true;
                state.status = GoalStatus::Ready;
            } else {
                state.status = GoalStatus::Rejected;
                state.stop_reason = Some("User declined approval".into());
            }
            Ok(())
        })
    }

    /// Cancellation is final. In-flight work must also be stopped by its caller.
    pub fn cancel(&mut self, reason: &str) -> Result<GoalSnapshot, String> {
        validate_text(reason, 1024, "Cancellation reason")?;
        self.change(|state| {
            if state.status.is_terminal() {
                return Err("Goal already stopped".into());
            }
            state.status = GoalStatus::Cancelled;
            state.paused_from = None;
            state.cancelled_turn = state.active_turn.take();
            state.approval_reason = None;
            state.stop_reason = Some(reason.to_owned());
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("synthetic-mobile-goal-{}", Uuid::new_v4()));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn spec(&self) -> GoalSpec {
            GoalSpec {
                title: "Synthetic private task".into(),
                max_turns: 4,
                max_retries_per_step: 1,
                steps: vec![
                    GoalStep {
                        description: "Read synthetic state".into(),
                        requires_approval: false,
                    },
                    GoalStep {
                        description: "User gated action".into(),
                        requires_approval: true,
                    },
                ],
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn explicit_turns_progress_and_approval_survive_reopen() {
        let f = Fixture::new();
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        assert_eq!(goal.snapshot().progress_percent(), 0);
        let first = goal.begin_turn().unwrap();
        assert_eq!(first.turn_number, 1);
        assert!(goal.begin_turn().is_err());
        assert!(goal
            .record_outcome("stale", TurnOutcome::Completed("fake".into()))
            .is_err());
        let progress = goal
            .record_outcome(
                &first.id,
                TurnOutcome::Completed("Real caller confirms step one".into()),
            )
            .unwrap();
        assert_eq!(progress.revision, 2);
        assert_eq!(progress.completed_steps(), 1);
        assert_eq!(progress.progress_percent(), 50);
        assert_eq!(progress.status, GoalStatus::AwaitingApproval);
        assert!(goal.begin_turn().is_err());
        assert!(GoalDriver::open(&f.0).is_err()); // same-process writer is exclusive
        drop(goal);
        let mut goal = GoalDriver::open(&f.0).unwrap();
        assert_eq!(goal.snapshot().progress_percent(), 50);
        assert_eq!(
            goal.resolve_approval(true, "User tapped Approve")
                .unwrap()
                .status,
            GoalStatus::Ready
        );
        let second = goal.begin_turn().unwrap();
        assert_eq!(second.step_index, 1);
        assert_eq!(
            goal.record_outcome(
                &second.id,
                TurnOutcome::Completed("Caller verified step two".into())
            )
            .unwrap()
            .status,
            GoalStatus::Completed
        );
        assert_eq!(goal.snapshot().progress_percent(), 100);
        assert_eq!(goal.snapshot().history.len(), 2);
        assert!(goal.begin_turn().is_err());
        drop(goal);
        let loaded = GoalDriver::open(&f.0).unwrap();
        assert_eq!(loaded.snapshot().status, GoalStatus::Completed);
    }

    #[test]
    fn retries_and_turn_limits_stop_bounded_attempts() {
        let f = Fixture::new();
        let mut spec = f.spec();
        spec.max_turns = 2;
        let mut goal = GoalDriver::create(&f.0, spec).unwrap();
        let first = goal.begin_turn().unwrap();
        assert_eq!(
            goal.mark_turn_interrupted(&first.id, "Connection lost")
                .unwrap()
                .status,
            GoalStatus::Ready
        );
        assert_eq!(goal.snapshot().retries_used_on_step, 1);
        let second = goal.begin_turn().unwrap();
        assert_eq!(second.attempt, 2);
        let state = goal
            .record_outcome(
                &second.id,
                TurnOutcome::RetryableFailure("Still offline".into()),
            )
            .unwrap();
        assert_eq!(state.status, GoalStatus::Failed);
        assert_eq!(state.turns_remaining(), 0);
        assert!(goal.begin_turn().is_err());
    }

    #[test]
    fn final_turn_completes_step_but_marks_remaining_goal_failed() {
        let f = Fixture::new();
        let mut spec = f.spec();
        spec.max_turns = 2;
        let mut goal = GoalDriver::create(&f.0, spec).unwrap();
        let t1 = goal.begin_turn().unwrap();
        goal.record_outcome(
            &t1.id,
            TurnOutcome::RetryableFailure("Temporary error".into()),
        )
        .unwrap();
        let t2 = goal.begin_turn().unwrap();
        let state = goal
            .record_outcome(&t2.id, TurnOutcome::Completed("Step confirmed".into()))
            .unwrap();
        assert_eq!(state.progress_percent(), 50);
        assert_eq!(state.status, GoalStatus::Failed);
        assert_eq!(state.turns_remaining(), 0);
        assert!(state.approval_reason.is_none());
    }

    #[test]
    fn in_flight_recovery_does_not_autorun_or_invent_outcomes() {
        let f = Fixture::new();
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        let ticket = goal.begin_turn().unwrap();
        drop(goal);
        let mut loaded = GoalDriver::open(&f.0).unwrap();
        assert_eq!(loaded.snapshot().status, GoalStatus::InTurn);
        assert_eq!(
            loaded.snapshot().active_turn.as_ref().unwrap().id,
            ticket.id
        );
        assert!(loaded.begin_turn().is_err());
        let state = loaded
            .mark_turn_interrupted(&ticket.id, "Interrupted after restart")
            .unwrap();
        assert_eq!(state.status, GoalStatus::Ready);
        assert_eq!(state.turns_used, 1);
        assert!(loaded
            .record_outcome(&ticket.id, TurnOutcome::Completed("Late".into()))
            .is_err());
    }

    #[test]
    fn manual_approval_gate_and_decline_stop_all_work() {
        let f = Fixture::new();
        let mut spec = f.spec();
        spec.steps[0].requires_approval = true;
        let mut goal = GoalDriver::create(&f.0, spec).unwrap();
        assert_eq!(goal.snapshot().status, GoalStatus::AwaitingApproval);
        assert!(goal.begin_turn().is_err());
        assert_eq!(
            goal.resolve_approval(false, "Declined explicitly")
                .unwrap()
                .status,
            GoalStatus::Rejected
        );
        assert!(goal.begin_turn().is_err());
        assert!(goal.resolve_approval(true, "Late approval").is_err());
    }

    #[test]
    fn dynamic_approval_cancel_and_duplicate_outcomes_fail_closed() {
        let f = Fixture::new();
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        let t = goal.begin_turn().unwrap();
        assert!(goal.request_approval("not during turn").is_err());
        let state = goal
            .record_outcome(
                &t.id,
                TurnOutcome::NeedsApproval("Confirm extra permission".into()),
            )
            .unwrap();
        assert_eq!(state.status, GoalStatus::AwaitingApproval);
        assert_eq!(state.history.len(), 1);
        assert!(goal
            .record_outcome(&t.id, TurnOutcome::Completed("Duplicate".into()))
            .is_err());
        assert_eq!(
            goal.resolve_approval(true, "Confirmed in UI")
                .unwrap()
                .status,
            GoalStatus::Ready
        );
        assert_eq!(
            goal.cancel("User stopped processing").unwrap().status,
            GoalStatus::Cancelled
        );
        assert!(goal.begin_turn().is_err());
    }

    #[test]
    fn corrupt_and_oversized_checkpoints_are_never_overwritten() {
        let f = Fixture::new();
        fs::write(f.0.join(CHECKPOINT), b"invalid_json").unwrap();
        assert!(GoalDriver::open(&f.0).is_err());
        assert!(GoalDriver::create(&f.0, f.spec()).is_err());
        assert_eq!(fs::read(f.0.join(CHECKPOINT)).unwrap(), b"invalid_json");
        fs::write(
            f.0.join(CHECKPOINT),
            vec![b'X'; MAX_CHECKPOINT_BYTES as usize + 1],
        )
        .unwrap();
        assert!(GoalDriver::open(&f.0).is_err());
    }

    #[test]
    fn invalid_specs_and_external_checkpoint_edits_are_rejected() {
        let f = Fixture::new();
        let mut spec = f.spec();
        spec.max_turns = 1;
        assert!(GoalDriver::create(&f.0, spec).is_err());
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        let before = fs::read(f.0.join(CHECKPOINT)).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&before).unwrap();
        value["current_step"] = serde_json::json!(99);
        let changed = serde_json::to_vec(&value).unwrap();
        fs::write(f.0.join(CHECKPOINT), &changed).unwrap();
        assert!(goal.begin_turn().is_err());
        assert_eq!(fs::read(f.0.join(CHECKPOINT)).unwrap(), changed);
        assert!(GoalDriver::open(&f.0).is_err());
    }

    #[test]
    fn maximum_turn_count_and_long_caller_outcomes_stay_within_disk_limit() {
        let f = Fixture::new();
        let spec = GoalSpec {
            title: "Synthetic bounded loop".into(),
            steps: vec![GoalStep {
                description: "Caller reports concrete result".into(),
                requires_approval: false,
            }],
            max_turns: MAX_TURNS,
            max_retries_per_step: 0,
        };
        let mut goal = GoalDriver::create(&f.0, spec).unwrap();
        for _ in 0..MAX_TURNS - 1 {
            let turn = goal.begin_turn().unwrap();
            let state = goal
                .record_outcome(&turn.id, TurnOutcome::NeedsApproval("A".repeat(1024)))
                .unwrap();
            assert_eq!(state.status, GoalStatus::AwaitingApproval);
            goal.resolve_approval(true, &"u".repeat(256)).unwrap();
        }
        let final_turn = goal.begin_turn().unwrap();
        assert_eq!(final_turn.turn_number, MAX_TURNS);
        let completed = goal
            .record_outcome(&final_turn.id, TurnOutcome::Completed("C".repeat(1024)))
            .unwrap();
        assert_eq!(completed.status, GoalStatus::Completed);
        assert_eq!(completed.history.len(), MAX_TURNS as usize);
        assert!((fs::metadata(f.0.join(CHECKPOINT)).unwrap().len()) < MAX_CHECKPOINT_BYTES);
        drop(goal);
        assert_eq!(
            GoalDriver::open(&f.0)
                .unwrap()
                .snapshot()
                .progress_percent(),
            100
        );
    }

    #[test]
    fn cancellation_during_active_turn_recovers_without_fabricated_completion() {
        let f = Fixture::new();
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        let ticket = goal.begin_turn().unwrap();
        let cancelled = goal
            .cancel("User cancelled while provider was running")
            .unwrap();
        assert_eq!(cancelled.status, GoalStatus::Cancelled);
        assert_eq!(cancelled.cancelled_turn, Some(ticket.clone()));
        assert_eq!(cancelled.turns_used, 1);
        assert!(cancelled.history.is_empty());
        assert_eq!(cancelled.progress_percent(), 0);
        drop(goal);

        let mut recovered = GoalDriver::open(&f.0).unwrap();
        assert_eq!(recovered.snapshot().cancelled_turn, Some(ticket.clone()));
        assert_eq!(recovered.snapshot().status, GoalStatus::Cancelled);
        assert!(recovered.begin_turn().is_err());
        assert!(recovered
            .record_outcome(&ticket.id, TurnOutcome::Completed("late output".into()))
            .is_err());
        assert!(recovered
            .mark_turn_interrupted(&ticket.id, "late callback")
            .is_err());
        assert!(recovered.cancel("duplicate cancel").is_err());
        drop(recovered);

        let mut corrupt: serde_json::Value =
            serde_json::from_slice(&fs::read(f.0.join(CHECKPOINT)).unwrap()).unwrap();
        corrupt["cancelled_turn"]["step_index"] = serde_json::json!(1);
        let bytes = serde_json::to_vec(&corrupt).unwrap();
        fs::write(f.0.join(CHECKPOINT), &bytes).unwrap();
        assert!(GoalDriver::open(&f.0).is_err());
        assert_eq!(fs::read(f.0.join(CHECKPOINT)).unwrap(), bytes);
    }

    #[test]
    fn older_cancelled_checkpoint_without_cancelled_ticket_remains_readable() {
        let f = Fixture::new();
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        let ticket = goal.begin_turn().unwrap();
        goal.cancel("Synthetic legacy checkpoint").unwrap();
        drop(goal);
        let mut old: serde_json::Value =
            serde_json::from_slice(&fs::read(f.0.join(CHECKPOINT)).unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("cancelled_turn");
        fs::write(f.0.join(CHECKPOINT), serde_json::to_vec(&old).unwrap()).unwrap();
        let mut recovered = GoalDriver::open(&f.0).unwrap();
        assert_eq!(recovered.snapshot().status, GoalStatus::Cancelled);
        assert_eq!(recovered.snapshot().cancelled_turn, None);
        assert_eq!(recovered.snapshot().turns_used, 1);
        assert!(recovered
            .record_outcome(&ticket.id, TurnOutcome::Completed("Late".into()))
            .is_err());
    }

    #[test]
    fn cancellation_before_turn_does_not_consume_budget_or_fake_a_ticket() {
        let f = Fixture::new();
        let mut spec = f.spec();
        spec.steps[0].requires_approval = true;
        let mut goal = GoalDriver::create(&f.0, spec).unwrap();
        let cancelled = goal
            .cancel("Explicit user cancellation at approval gate")
            .unwrap();
        assert_eq!(cancelled.status, GoalStatus::Cancelled);
        assert_eq!(cancelled.turns_used, 0);
        assert_eq!(cancelled.cancelled_turn, None);
        assert_eq!(cancelled.approval_reason, None);
        drop(goal);
        assert_eq!(
            GoalDriver::open(&f.0).unwrap().snapshot().status,
            GoalStatus::Cancelled
        );
    }

    #[test]
    fn rejected_approval_cannot_be_rewritten_to_allow_a_turn() {
        let f = Fixture::new();
        let mut spec = f.spec();
        spec.steps[0].requires_approval = true;
        let mut goal = GoalDriver::create(&f.0, spec).unwrap();
        goal.resolve_approval(false, "User declined explicitly")
            .unwrap();
        drop(goal);
        let baseline = fs::read(f.0.join(CHECKPOINT)).unwrap();

        let mut forged: serde_json::Value = serde_json::from_slice(&baseline).unwrap();
        forged["status"] = serde_json::json!("ready");
        forged["approved_for_step"] = serde_json::json!(true);
        forged["stop_reason"] = serde_json::Value::Null;
        assert_corrupt_preserved(&f.0, &forged);

        let mut forged: serde_json::Value = serde_json::from_slice(&baseline).unwrap();
        forged["approvals"][0]["step_index"] = serde_json::json!(1);
        assert_corrupt_preserved(&f.0, &forged);

        let mut forged: serde_json::Value = serde_json::from_slice(&baseline).unwrap();
        forged["approvals"][0]["approved"] = serde_json::json!(true);
        assert_corrupt_preserved(&f.0, &forged);

        fs::write(f.0.join(CHECKPOINT), baseline).unwrap();
        assert_eq!(
            GoalDriver::open(&f.0).unwrap().snapshot().status,
            GoalStatus::Rejected
        );
    }

    #[test]
    fn omitted_dynamic_approval_does_not_enable_ready_state_after_restart() {
        let f = Fixture::new();
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        let ticket = goal.begin_turn().unwrap();
        goal.record_outcome(
            &ticket.id,
            TurnOutcome::NeedsApproval("User confirmation required".into()),
        )
        .unwrap();
        goal.resolve_approval(true, "Explicit approval").unwrap();
        drop(goal);
        let baseline = fs::read(f.0.join(CHECKPOINT)).unwrap();
        let mut forged: serde_json::Value = serde_json::from_slice(&baseline).unwrap();
        forged["approved_for_step"] = serde_json::json!(false);
        assert_corrupt_preserved(&f.0, &forged);
        fs::write(f.0.join(CHECKPOINT), baseline).unwrap();
        let recovered = GoalDriver::open(&f.0).unwrap();
        assert_eq!(recovered.snapshot().status, GoalStatus::Ready);
    }

    #[test]
    fn inconsistent_retry_ticket_or_terminal_status_is_rejected_as_corruption() {
        let f = Fixture::new();
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        let first = goal.begin_turn().unwrap();
        goal.mark_turn_interrupted(&first.id, "Network interruption")
            .unwrap();
        let second = goal.begin_turn().unwrap();
        let next = goal
            .record_outcome(
                &second.id,
                TurnOutcome::TerminalFailure("Permanent error".into()),
            )
            .unwrap();
        assert_eq!(next.status, GoalStatus::Failed);
        drop(goal);
        let baseline = fs::read(f.0.join(CHECKPOINT)).unwrap();

        let mut forged: serde_json::Value = serde_json::from_slice(&baseline).unwrap();
        forged["history"][1]["ticket"]["attempt"] = serde_json::json!(1);
        assert_corrupt_preserved(&f.0, &forged);

        let mut forged: serde_json::Value = serde_json::from_slice(&baseline).unwrap();
        forged["retries_used_on_step"] = serde_json::json!(0);
        assert_corrupt_preserved(&f.0, &forged);

        let mut forged: serde_json::Value = serde_json::from_slice(&baseline).unwrap();
        forged["status"] = serde_json::json!("ready");
        forged["stop_reason"] = serde_json::Value::Null;
        assert_corrupt_preserved(&f.0, &forged);

        fs::write(f.0.join(CHECKPOINT), baseline).unwrap();
        assert_eq!(
            GoalDriver::open(&f.0).unwrap().snapshot().status,
            GoalStatus::Failed
        );
    }

    #[test]
    fn broken_checkpoint_and_orphaned_temp_file_do_not_trigger_replay() {
        let f = Fixture::new();
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        let ticket = goal.begin_turn().unwrap();
        drop(goal);
        let healthy = fs::read(f.0.join(CHECKPOINT)).unwrap();
        fs::write(f.0.join("goal-orphaned.tmp"), b"garbage partial checkpoint").unwrap();
        assert_eq!(
            GoalDriver::open(&f.0)
                .unwrap()
                .snapshot()
                .active_turn
                .unwrap()
                .id,
            ticket.id
        );
        let truncated = &healthy[..healthy.len() / 2];
        fs::write(f.0.join(CHECKPOINT), truncated).unwrap();
        assert!(GoalDriver::open(&f.0).is_err());
        assert_eq!(fs::read(f.0.join(CHECKPOINT)).unwrap(), truncated);
        assert!(GoalDriver::create(&f.0, f.spec()).is_err());
        assert_eq!(fs::read(f.0.join(CHECKPOINT)).unwrap(), truncated);
        fs::write(f.0.join(CHECKPOINT), &healthy).unwrap();
        let resumed = GoalDriver::open(&f.0).unwrap();
        assert_eq!(resumed.snapshot().active_turn.unwrap().id, ticket.id);
        assert_eq!(resumed.snapshot().history.len(), 0);
        assert!(f.0.join("goal-orphaned.tmp").exists());
    }

    #[test]
    fn explicit_pause_ready_survives_reopen_without_running_or_budget_loss() {
        let f = Fixture::new();
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        let original = goal.snapshot();
        assert_eq!(original.status, GoalStatus::Ready);
        let paused = goal.pause().unwrap();
        assert_eq!(paused.version, 1);
        assert_eq!(paused.revision, original.revision + 1);
        assert_eq!(paused.status, GoalStatus::Paused);
        assert_eq!(paused.paused_from, Some(GoalStatus::Ready));
        assert_eq!(paused.current_step, original.current_step);
        assert_eq!(paused.turns_used, original.turns_used);
        assert_eq!(paused.retries_used_on_step, original.retries_used_on_step);
        assert_eq!(paused.history, original.history);
        assert_eq!(paused.approvals, original.approvals);
        assert!(paused.active_turn.is_none());
        assert!(goal.begin_turn().is_err());
        assert!(goal.request_approval("Bypass pause").is_err());
        assert!(goal.resolve_approval(true, "Bypass pause").is_err());
        assert!(goal
            .record_outcome("any", TurnOutcome::Completed("fake".into()))
            .is_err());
        let bytes = fs::read(f.0.join(CHECKPOINT)).unwrap();
        assert!(goal.pause().is_err());
        assert_eq!(fs::read(f.0.join(CHECKPOINT)).unwrap(), bytes);
        drop(goal);

        let mut recovered = GoalDriver::open(&f.0).unwrap();
        assert_eq!(recovered.snapshot(), paused);
        assert!(recovered.begin_turn().is_err());
        assert!(recovered.pause().is_err());
        let resumed = recovered.resume().unwrap();
        assert_eq!(resumed.status, GoalStatus::Ready);
        assert_eq!(resumed.paused_from, None);
        assert_eq!(resumed.revision, paused.revision + 1);
        assert_eq!(resumed.current_step, original.current_step);
        assert_eq!(resumed.turns_used, original.turns_used);
        assert_eq!(resumed.history, original.history);
        assert!(recovered.resume().is_err());
        drop(recovered);

        let mut open_again = GoalDriver::open(&f.0).unwrap();
        assert_eq!(open_again.snapshot().status, GoalStatus::Ready);
        assert_eq!(open_again.snapshot().turns_used, 0);
        let ticket = open_again.begin_turn().unwrap();
        assert_eq!(ticket.turn_number, 1);
    }

    #[test]
    fn paused_initial_approval_gate_requires_real_manual_approval_after_reopen() {
        let f = Fixture::new();
        let mut spec = f.spec();
        spec.steps[0].requires_approval = true;
        let mut goal = GoalDriver::create(&f.0, spec).unwrap();
        let waiting = goal.snapshot();
        assert_eq!(waiting.status, GoalStatus::AwaitingApproval);
        let paused = goal.pause().unwrap();
        assert_eq!(paused.paused_from, Some(GoalStatus::AwaitingApproval));
        assert_eq!(paused.approval_reason, waiting.approval_reason);
        assert!(!paused.approved_for_step);
        assert!(goal
            .resolve_approval(true, "Cannot approve while paused")
            .is_err());
        assert!(goal.begin_turn().is_err());
        drop(goal);

        let mut recovered = GoalDriver::open(&f.0).unwrap();
        assert_eq!(recovered.snapshot(), paused);
        assert_eq!(
            recovered.resume().unwrap().status,
            GoalStatus::AwaitingApproval
        );
        assert!(recovered.begin_turn().is_err());
        assert_eq!(recovered.snapshot().approvals.len(), 0);
        assert_eq!(
            recovered
                .resolve_approval(true, "Explicit user approval")
                .unwrap()
                .status,
            GoalStatus::Ready
        );
        assert_eq!(recovered.snapshot().approvals.len(), 1);
        assert!(recovered.snapshot().approved_for_step);
        let authorized = recovered.begin_turn().unwrap();
        assert_eq!(authorized.step_index, 0);
        assert_eq!(authorized.turn_number, 1);
    }

    #[test]
    fn pause_after_manual_approval_preserves_gate_and_dynamic_approval_history() {
        let f = Fixture::new();
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        let first = goal.begin_turn().unwrap();
        goal.record_outcome(
            &first.id,
            TurnOutcome::NeedsApproval("Needs independent user check".into()),
        )
        .unwrap();
        let paused_gate = goal.pause().unwrap();
        assert_eq!(paused_gate.paused_from, Some(GoalStatus::AwaitingApproval));
        assert_eq!(paused_gate.history.len(), 1);
        assert_eq!(paused_gate.turns_used, 1);
        assert_eq!(
            paused_gate.approval_reason.as_deref(),
            Some("Needs independent user check")
        );
        drop(goal);

        let mut recovered = GoalDriver::open(&f.0).unwrap();
        recovered.resume().unwrap();
        assert!(recovered.begin_turn().is_err());
        recovered.resolve_approval(true, "User accepted").unwrap();
        let previously_approved = recovered.snapshot();
        assert_eq!(previously_approved.status, GoalStatus::Ready);
        assert!(previously_approved.approved_for_step);
        assert_eq!(previously_approved.approvals.len(), 1);
        let paused_ready = recovered.pause().unwrap();
        assert_eq!(paused_ready.paused_from, Some(GoalStatus::Ready));
        assert!(paused_ready.approved_for_step);
        assert_eq!(paused_ready.approvals, previously_approved.approvals);
        assert_eq!(paused_ready.history, previously_approved.history);
        assert_eq!(paused_ready.turns_used, 1);
        drop(recovered);

        let mut recovered = GoalDriver::open(&f.0).unwrap();
        assert!(recovered.begin_turn().is_err());
        let ready = recovered.resume().unwrap();
        assert!(ready.approved_for_step);
        assert_eq!(ready.approvals, previously_approved.approvals);
        assert_eq!(ready.turns_used, 1);
        let turn = recovered.begin_turn().unwrap();
        assert_eq!(turn.step_index, 0);
        assert_eq!(turn.turn_number, 2);
        assert!(recovered
            .record_outcome(&first.id, TurnOutcome::Completed("stale".into()))
            .is_err());
        assert_eq!(
            recovered
                .record_outcome(&turn.id, TurnOutcome::Completed("real".into()))
                .unwrap()
                .status,
            GoalStatus::AwaitingApproval
        );
        assert_eq!(recovered.snapshot().current_step, 1);
    }

    #[test]
    fn in_flight_and_terminal_goals_reject_pause_without_checkpoint_mutation() {
        let f = Fixture::new();
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        let ticket = goal.begin_turn().unwrap();
        let active_bytes = fs::read(f.0.join(CHECKPOINT)).unwrap();
        assert!(goal.pause().is_err());
        assert!(goal.resume().is_err());
        assert_eq!(fs::read(f.0.join(CHECKPOINT)).unwrap(), active_bytes);
        drop(goal);
        let mut reopened = GoalDriver::open(&f.0).unwrap();
        assert_eq!(reopened.snapshot().status, GoalStatus::InTurn);
        assert_eq!(reopened.snapshot().active_turn, Some(ticket.clone()));
        assert!(reopened.pause().is_err());
        assert!(reopened.begin_turn().is_err());
        reopened
            .mark_turn_interrupted(&ticket.id, "Real request cancelled first")
            .unwrap();
        assert_eq!(reopened.snapshot().status, GoalStatus::Ready);
        let paused = reopened.pause().unwrap();
        assert_eq!(paused.retries_used_on_step, 1);
        assert_eq!(paused.history.len(), 1);
        assert_eq!(paused.turns_used, 1);
        assert!(reopened
            .record_outcome(&ticket.id, TurnOutcome::Completed("late replay".into()))
            .is_err());
        reopened.resume().unwrap();
        let retry_ticket = reopened.begin_turn().unwrap();
        assert_eq!(retry_ticket.attempt, 2);
        assert_eq!(
            reopened
                .record_outcome(
                    &retry_ticket.id,
                    TurnOutcome::TerminalFailure("Stop".into())
                )
                .unwrap()
                .status,
            GoalStatus::Failed
        );
        let terminal_bytes = fs::read(f.0.join(CHECKPOINT)).unwrap();
        assert!(reopened.pause().is_err());
        assert!(reopened.resume().is_err());
        assert_eq!(fs::read(f.0.join(CHECKPOINT)).unwrap(), terminal_bytes);
    }

    #[test]
    fn cancel_while_paused_drops_resume_target_and_cannot_replay() {
        let f = Fixture::new();
        let mut spec = f.spec();
        spec.steps[0].requires_approval = true;
        let mut goal = GoalDriver::create(&f.0, spec).unwrap();
        let parked = goal.pause().unwrap();
        assert_eq!(parked.paused_from, Some(GoalStatus::AwaitingApproval));
        let stopped = goal.cancel("Explicit user cancel").unwrap();
        assert_eq!(stopped.status, GoalStatus::Cancelled);
        assert_eq!(stopped.paused_from, None);
        assert_eq!(stopped.cancelled_turn, None);
        assert_eq!(stopped.approval_reason, None);
        assert_eq!(stopped.turns_used, 0);
        assert_eq!(stopped.stop_reason.as_deref(), Some("Explicit user cancel"));
        drop(goal);
        let mut reopened = GoalDriver::open(&f.0).unwrap();
        assert_eq!(reopened.snapshot(), stopped);
        let bytes = fs::read(f.0.join(CHECKPOINT)).unwrap();
        assert!(reopened.resume().is_err());
        assert!(reopened.pause().is_err());
        assert!(reopened.resolve_approval(true, "Too late").is_err());
        assert!(reopened.cancel("Again").is_err());
        assert_eq!(fs::read(f.0.join(CHECKPOINT)).unwrap(), bytes);
    }

    #[test]
    fn legacy_v1_checkpoint_opens_without_pause_field_and_stays_compatible() {
        let f = Fixture::new();
        let goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        drop(goal);
        let old_bytes = fs::read(f.0.join(CHECKPOINT)).unwrap();
        let old: serde_json::Value = serde_json::from_slice(&old_bytes).unwrap();
        assert!(old.get("paused_from").is_none());
        let mut loaded = GoalDriver::open(&f.0).unwrap();
        assert_eq!(loaded.snapshot().version, 1);
        assert_eq!(loaded.snapshot().paused_from, None);
        assert_eq!(loaded.snapshot().status, GoalStatus::Ready);
        let ticket = loaded.begin_turn().unwrap();
        loaded
            .record_outcome(&ticket.id, TurnOutcome::Completed("Real first step".into()))
            .unwrap();
        drop(loaded);
        let reopened = GoalDriver::open(&f.0).unwrap();
        assert_eq!(reopened.snapshot().version, 1);
        assert_eq!(reopened.snapshot().paused_from, None);
        assert_eq!(reopened.snapshot().status, GoalStatus::AwaitingApproval);
    }

    #[test]
    fn invalid_paused_checkpoints_fail_closed_and_remain_byte_exact() {
        let f = Fixture::new();
        let mut spec = f.spec();
        spec.steps[0].requires_approval = true;
        let mut goal = GoalDriver::create(&f.0, spec).unwrap();
        goal.pause().unwrap();
        drop(goal);
        let baseline = fs::read(f.0.join(CHECKPOINT)).unwrap();
        let good: serde_json::Value = serde_json::from_slice(&baseline).unwrap();
        for forged in [
            {
                let mut value = good.clone();
                value.as_object_mut().unwrap().remove("paused_from");
                value
            },
            {
                let mut value = good.clone();
                value["paused_from"] = serde_json::json!("in_turn");
                value
            },
            {
                let mut value = good.clone();
                value["paused_from"] = serde_json::json!("paused");
                value
            },
            {
                let mut value = good.clone();
                value["paused_from"] = serde_json::json!("ready");
                value
            },
            {
                let mut value = good.clone();
                value["approval_reason"] = serde_json::Value::Null;
                value
            },
            {
                let mut value = good.clone();
                value["active_turn"] = serde_json::json!({
                    "id":Uuid::new_v4().to_string(),"step_index":0,"attempt":1,"turn_number":1
                });
                value
            },
            {
                let mut value = good.clone();
                value["stop_reason"] = serde_json::json!("Fake stop");
                value
            },
        ] {
            assert_corrupt_preserved(&f.0, &forged);
        }
        // A valid paused checkpoint is still recoverable after rejecting all
        // those tampered variants; recovery never repairs or starts work.
        fs::write(f.0.join(CHECKPOINT), &baseline).unwrap();
        let mut recovered = GoalDriver::open(&f.0).unwrap();
        assert_eq!(recovered.snapshot().status, GoalStatus::Paused);
        assert!(recovered.begin_turn().is_err());
        assert_eq!(
            recovered.resume().unwrap().status,
            GoalStatus::AwaitingApproval
        );
    }

    fn assert_corrupt_preserved(folder: &Path, corrupted: &serde_json::Value) {
        let bytes = serde_json::to_vec(corrupted).unwrap();
        fs::write(folder.join(CHECKPOINT), &bytes).unwrap();
        assert!(GoalDriver::open(folder).is_err());
        assert_eq!(fs::read(folder.join(CHECKPOINT)).unwrap(), bytes);
    }
}
