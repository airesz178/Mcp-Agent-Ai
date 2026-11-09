//! A single, explicit Goal-turn MCP proposal/review/dispatch checkpoint.
//!
//! This module NEVER calls MCP, logs or persists tool arguments, provider
//! payloads, schema definitions, credentials, transcripts or tool results.
//! The caller passes a REAL persisted GoalDriver snapshot, then a SEPARATE
//! visible user approval. The one-shot dispatch permit is issued only AFTER
//! writing and syncing a durable `dispatch_uncertain` marker. A process crash
//! can therefore NEVER authorize an automatic replay: on reopen pending and
//! approved proposals become `interrupted`, while dispatched work remains
//! `dispatch_uncertain` unless the original caller reports a real outcome.
//!
//! Contract for prime:
//! `GoalMcpLease::propose(home, &goal_snapshot, request_id, generation,
//!                     &model_tool_call, verified_tools, ttl_ms)`
//! `GoalMcpLease::open(home, turn_id)` (recovery, no proposal arguments)
//! `lease.snapshot()` (redacted persisted metadata only)
//! `lease.approve(&goal_snapshot, review_id, user_initiated)`
//! `lease.reject(review_id, user_initiated)`
//! `lease.begin_dispatch(&goal_snapshot, review_id, generation, verified_tools)`
//! `permit.into_call()` -> (DispatchTicket, original ModelToolCall)
//! `lease.report_outcome(&ticket, RemoteOutcome::{Confirmed,ReportedError})`.
//!
//! All paths MUST be a dedicated, existing owner-private Goal folder within
//! Android Context.filesDir, never a caller-selected path. The host MUST
//! verify a real foreground gesture, ensure `goal_snapshot` is from the real
//! GoalDriver, and dispatch the returned arguments exactly ONCE, with no
//! automatic retry on network failure or interrupted process.

use crate::{
    mcp_client,
    mobile_goal::{GoalSnapshot, GoalStatus},
    provider_tools::ModelToolCall,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const PREFIX: &str = "goal-mcp-";
const SUFFIX: &str = ".json";
const LOCK: &str = ".goal-mcp-v1.lock";
const MAX_RECORD_BYTES: u64 = 8 * 1024;
const MIN_TTL_MS: u64 = 1_000;
const MAX_TTL_MS: u64 = 5 * 60 * 1000;
static PROCESS_WRITER: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Pending,
    Approved,
    DispatchUncertain,
    Confirmed,
    ReportedError,
    Rejected,
    Expired,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseSnapshot {
    pub version: u8,
    pub revision: u64,
    pub goal_id: String,
    pub goal_revision: u64,
    pub turn_id: String,
    pub request_id: String,
    pub review_id: String,
    pub connection_generation: u64,
    pub tool_name: String,
    pub status: ReviewStatus,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub approved_at_ms: Option<u64>,
    pub dispatch_id: Option<String>,
    pub closed_at_ms: Option<u64>,
}

impl LeaseSnapshot {
    fn validate(&self) -> Result<(), String> {
        let invalid = || "Goal MCP checkpoint is invalid; no work was authorized".to_owned();
        if self.version != 1
            || self.revision == 0
            || self.revision > 5
            || ![
                &self.goal_id,
                &self.turn_id,
                &self.request_id,
                &self.review_id,
            ]
            .iter()
            .all(|id| valid_id(id))
            || self.dispatch_id.as_deref().is_some_and(|id| !valid_id(id))
            || self.goal_id == self.turn_id
            || self.goal_id == self.request_id
            || self.goal_id == self.review_id
            || self.request_id == self.turn_id
            || self.review_id == self.turn_id
            || self.review_id == self.request_id
            || self.connection_generation == 0
            || self.connection_generation == u64::MAX
            || !valid_tool_name(&self.tool_name)
            || self.created_at_ms == 0
            || self.expires_at_ms <= self.created_at_ms
            || self.expires_at_ms - self.created_at_ms > MAX_TTL_MS
            || self.expires_at_ms - self.created_at_ms < MIN_TTL_MS
            || self
                .approved_at_ms
                .is_some_and(|ms| ms < self.created_at_ms || ms > self.expires_at_ms)
            || self.closed_at_ms.is_some_and(|ms| ms < self.created_at_ms)
        {
            return Err(invalid());
        }
        let approved = self.approved_at_ms.is_some();
        let dispatched = self.dispatch_id.is_some();
        let closed = self.closed_at_ms.is_some();
        let consistent = match self.status {
            ReviewStatus::Pending => !approved && !dispatched && !closed,
            ReviewStatus::Approved => approved && !dispatched && !closed,
            ReviewStatus::DispatchUncertain => approved && dispatched && !closed,
            ReviewStatus::Confirmed | ReviewStatus::ReportedError => {
                approved && dispatched && closed
            }
            ReviewStatus::Rejected | ReviewStatus::Expired | ReviewStatus::Interrupted => {
                !dispatched && closed
            }
        };
        if !consistent {
            return Err(invalid());
        }
        Ok(())
    }
}

/// Contains only volatile, never-persisted provider input. Cannot be cloned.
struct BoundProposal {
    provider_call_id: String,
    arguments: Value,
    tool_definition: Value,
}

/// No Debug/Clone/Serialize implementation: raw arguments stay in memory only.
pub struct GoalMcpLease {
    home: PathBuf,
    turn_id: String,
    expected: LeaseSnapshot,
    bound: Option<BoundProposal>,
    dispatched_id: Option<String>,
    poisoned: bool,
}

/// Single-use, non-clone dispatch permit. Issuing it does not execute MCP.
/// Do NOT serialize it or use the returned call more than once.
pub struct DispatchPermit {
    ticket: DispatchTicket,
    proposal: ModelToolCall,
}

pub struct DispatchTicket {
    review_id: String,
    dispatch_id: String,
    turn_id: String,
}

impl DispatchPermit {
    pub fn into_call(self) -> (DispatchTicket, ModelToolCall) {
        (self.ticket, self.proposal)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteOutcome {
    Confirmed,
    ReportedError,
}

fn valid_id(text: &str) -> bool {
    Uuid::parse_str(text)
        .is_ok_and(|id| id.get_version_num() == 4 && id.hyphenated().to_string() == text)
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

fn now_ms() -> Result<u64, String> {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System clock is unavailable")?
        .as_millis();
    u64::try_from(ms).map_err(|_| "System clock is out of range".into())
}

fn goal_is_current(snapshot: &GoalSnapshot, goal_id: &str, turn_id: &str, revision: u64) -> bool {
    snapshot.version == 1
        && snapshot.id == goal_id
        && snapshot.revision == revision
        && snapshot.status == GoalStatus::InTurn
        && snapshot.cancelled_turn.is_none()
        && snapshot
            .active_turn
            .as_ref()
            .is_some_and(|turn| turn.id == turn_id)
}

fn file_path(home: &Path, turn_id: &str) -> Result<PathBuf, String> {
    if !valid_id(turn_id) {
        return Err("Goal MCP turn must be a canonical lowercase UUID v4".into());
    }
    Ok(home.join(format!("{PREFIX}{turn_id}{SUFFIX}")))
}

fn is_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

fn safe_home(home: &Path) -> Result<PathBuf, String> {
    if !home.is_absolute() {
        return Err("Goal MCP home must be a dedicated absolute app-private directory".into());
    }
    let meta = fs::symlink_metadata(home).map_err(|_| "Goal MCP directory must already exist")?;
    if !meta.is_dir() || is_reparse(&meta) {
        return Err("Goal MCP directory is not a safe real directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err("Goal MCP directory must be owner-private".into());
        }
    }
    let canonical = home
        .canonicalize()
        .map_err(|_| "Cannot resolve private Goal MCP directory")?;
    if !canonical.is_dir() {
        return Err("Private Goal MCP directory is unavailable".into());
    }
    Ok(canonical)
}

fn safe_file(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !is_reparse(&meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if meta.nlink() != 1 || meta.permissions().mode() & 0o077 != 0 {
                    return Err("Goal MCP file is not owner-private or is hard-linked".into());
                }
            }
            Ok(true)
        }
        Ok(_) => Err("Unsafe Goal MCP checkpoint or lock file; no changes made".into()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Cannot inspect Goal MCP checkpoint or lock".into()),
    }
}

struct WriterLock(File);
impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn writer_lock(home: &Path) -> Result<WriterLock, String> {
    let path = home.join(LOCK);
    safe_file(&path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        #[cfg(any(target_os = "android", target_os = "linux"))]
        options.custom_flags(0o400000); // O_NOFOLLOW
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options
        .open(&path)
        .map_err(|_| "Cannot open Goal MCP lock")?;
    if !safe_file(&path)?
        || file
            .metadata()
            .map_err(|_| "Cannot inspect Goal MCP lock")?
            .len()
            != 0
    {
        return Err("Goal MCP lock file changed or is not empty".into());
    }
    file.try_lock()
        .map_err(|_| "Goal MCP state is locked by another writer".to_owned())?;
    Ok(WriterLock(file))
}

fn read_state(home: &Path, turn_id: &str) -> Result<LeaseSnapshot, String> {
    let path = file_path(home, turn_id)?;
    if !safe_file(&path)? {
        return Err("Goal MCP lease checkpoint does not exist".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "android", target_os = "linux"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0o400000); // O_NOFOLLOW
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let mut file = options
        .open(&path)
        .map_err(|_| "Cannot open Goal MCP checkpoint")?;
    if !file
        .metadata()
        .map_err(|_| "Cannot inspect Goal MCP file")?
        .is_file()
    {
        return Err("Goal MCP checkpoint is not a regular file".into());
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read Goal MCP checkpoint")?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err("Goal MCP checkpoint exceeds 8 KiB".into());
    }
    let state: LeaseSnapshot = serde_json::from_slice(&bytes)
        .map_err(|_| "Goal MCP checkpoint is corrupt; no changes made".to_owned())?;
    state.validate()?;
    if state.turn_id != turn_id {
        return Err("Goal MCP checkpoint is bound to a different turn".into());
    }
    Ok(state)
}

#[cfg(windows)]
fn replace_atomically(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let from: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0x1 | 0x8) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(not(windows))]
fn replace_atomically(src: &Path, dst: &Path) -> io::Result<()> {
    fs::rename(src, dst)
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Caller holds the OS lock. For first-write use hard_link(create-new) so a
/// malicious or racing target cannot be silently overwritten.
fn write_state(home: &Path, state: &LeaseSnapshot, is_new: bool) -> Result<(), String> {
    state.validate()?;
    let target = file_path(home, &state.turn_id)?;
    let exists = safe_file(&target)?;
    if exists == is_new {
        return Err("Goal MCP checkpoint existence changed; no work authorized".into());
    }
    let bytes = serde_json::to_vec(state).map_err(|_| "Cannot encode Goal MCP checkpoint")?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err("Goal MCP checkpoint is too large".into());
    }
    let temp = Temporary(home.join(format!(".goal-mcp-{}.tmp", Uuid::new_v4())));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp.0)
        .map_err(|_| "Cannot stage Goal MCP checkpoint")?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "Cannot sync Goal MCP checkpoint")?;
    drop(file);
    if is_new {
        fs::hard_link(&temp.0, &target)
            .map_err(|_| "Cannot safely create new Goal MCP checkpoint")?;
        // Temporary remains linked until dropped, but on successful return
        // the destination alone contains the fully fsynced checkpoint.
    } else {
        // No external writer is authorized to mutate the target; refuse
        // special targets and only replace under the held kernel writer lock.
        safe_file(&target)?;
        replace_atomically(&temp.0, &target)
            .map_err(|_| "Cannot atomically update Goal MCP checkpoint")?;
    }
    #[cfg(unix)]
    {
        File::open(home)
            .and_then(|f| f.sync_all())
            .map_err(|_| "Cannot sync private Goal MCP folder")?;
    }
    Ok(())
}

impl GoalMcpLease {
    fn transaction(
        &mut self,
        f: impl FnOnce(&mut LeaseSnapshot) -> Result<(), String>,
    ) -> Result<LeaseSnapshot, String> {
        if self.poisoned {
            return Err("Goal MCP lease storage is uncertain; reopen without dispatch".into());
        }
        let _in_process = PROCESS_WRITER
            .lock()
            .map_err(|_| "Goal MCP writer mutex unavailable")?;
        let _kernel = writer_lock(&self.home)?;
        let mut state = match read_state(&self.home, &self.turn_id) {
            Ok(value) => value,
            Err(err) => {
                self.poisoned = true;
                self.bound = None;
                return Err(err);
            }
        };
        if state != self.expected {
            self.poisoned = true;
            self.bound = None;
            return Err("Goal MCP checkpoint changed externally; no dispatch authorized".into());
        }
        f(&mut state)?;
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or("Goal MCP revision overflow")?;
        if let Err(err) = write_state(&self.home, &state, false) {
            // Could have failed after rename/fsync. No stale lease may retry.
            self.poisoned = true;
            self.bound = None;
            return Err(err);
        }
        self.expected = state.clone();
        Ok(state)
    }

    /// One proposal for this real in-turn Goal ticket. Its exact arguments
    /// and verified tool definition stay ephemeral; only safe metadata is
    /// synced to disk. No directory creation, network, approval or tool call.
    pub fn propose(
        home: &Path,
        goal: &GoalSnapshot,
        request_id: &str,
        connection_generation: u64,
        proposal: &ModelToolCall,
        verified_tools: &[Value],
        ttl_ms: u64,
    ) -> Result<Self, String> {
        let turn_id = goal
            .active_turn
            .as_ref()
            .ok_or("Goal has no active persisted turn")?
            .id
            .clone();
        if !valid_id(&goal.id)
            || !valid_id(&turn_id)
            || !valid_id(request_id)
            || goal.id == turn_id
            || goal.id == request_id
            || request_id == turn_id
            || !goal_is_current(goal, &goal.id, &turn_id, goal.revision)
            || connection_generation == 0
            || connection_generation == u64::MAX
            || !valid_tool_name(&proposal.name)
            || proposal.id.is_empty()
            || proposal.id.len() > 128
            || !proposal
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            || !(MIN_TTL_MS..=MAX_TTL_MS).contains(&ttl_ms)
        {
            return Err("Goal MCP proposal lacks an active canonical ticket or safe lease".into());
        }
        mcp_client::validate_model_arguments(verified_tools, &proposal.name, &proposal.arguments)?;
        let definition = verified_tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(&proposal.name))
            .ok_or("Goal MCP verified tool definition missing")?
            .clone();
        let now = now_ms()?;
        let expires = now
            .checked_add(ttl_ms)
            .ok_or("Goal MCP lease time overflow")?;
        let record = LeaseSnapshot {
            version: 1,
            revision: 1,
            goal_id: goal.id.clone(),
            goal_revision: goal.revision,
            turn_id: turn_id.clone(),
            request_id: request_id.to_owned(),
            review_id: Uuid::new_v4().to_string(),
            connection_generation,
            tool_name: proposal.name.clone(),
            status: ReviewStatus::Pending,
            created_at_ms: now,
            expires_at_ms: expires,
            approved_at_ms: None,
            dispatch_id: None,
            closed_at_ms: None,
        };
        record.validate()?;
        let canonical_home = safe_home(home)?;
        {
            let _local = PROCESS_WRITER
                .lock()
                .map_err(|_| "Goal MCP writer mutex unavailable")?;
            let _kernel = writer_lock(&canonical_home)?;
            if safe_file(&file_path(&canonical_home, &turn_id)?)? {
                return Err("A Goal MCP proposal already exists for this turn".into());
            }
            write_state(&canonical_home, &record, true)?;
        }
        Ok(Self {
            home: canonical_home,
            turn_id,
            expected: record,
            bound: Some(BoundProposal {
                provider_call_id: proposal.id.clone(),
                arguments: proposal.arguments.clone(),
                tool_definition: definition,
            }),
            dispatched_id: None,
            poisoned: false,
        })
    }

    /// Recover metadata only. Any proposal that had not been dispatched is
    /// terminally interrupted and must be explicitly reproposed in a NEW Goal
    /// turn. In-flight dispatch remains uncertain, never replayed.
    pub fn open(home: &Path, turn_id: &str) -> Result<Self, String> {
        let canonical_home = safe_home(home)?;
        let _local = PROCESS_WRITER
            .lock()
            .map_err(|_| "Goal MCP writer mutex unavailable")?;
        let _kernel = writer_lock(&canonical_home)?;
        let mut snapshot = read_state(&canonical_home, turn_id)?;
        if matches!(
            snapshot.status,
            ReviewStatus::Pending | ReviewStatus::Approved
        ) {
            snapshot.status = ReviewStatus::Interrupted;
            snapshot.closed_at_ms = Some(now_ms()?.max(snapshot.created_at_ms));
            snapshot.revision = snapshot
                .revision
                .checked_add(1)
                .ok_or("Goal MCP revision overflow")?;
            write_state(&canonical_home, &snapshot, false)?;
        }
        Ok(Self {
            home: canonical_home,
            turn_id: turn_id.to_owned(),
            expected: snapshot,
            bound: None,
            dispatched_id: None,
            poisoned: false,
        })
    }

    pub fn snapshot(&self) -> Result<LeaseSnapshot, String> {
        if self.poisoned {
            return Err("Goal MCP state is uncertain and must be recovered".into());
        }
        let _local = PROCESS_WRITER
            .lock()
            .map_err(|_| "Goal MCP mutex unavailable")?;
        let _kernel = writer_lock(&self.home)?;
        let state = read_state(&self.home, &self.turn_id)?;
        if state != self.expected {
            return Err("Goal MCP checkpoint changed externally; no dispatch authorized".into());
        }
        Ok(state)
    }

    pub fn approve(
        &mut self,
        goal: &GoalSnapshot,
        review_id: &str,
        user_initiated: bool,
    ) -> Result<LeaseSnapshot, String> {
        if !user_initiated || self.bound.is_none() {
            return Err("Goal MCP approval requires a fresh explicit user gesture".into());
        }
        self.transaction(|state| {
            if state.review_id != review_id
                || state.status != ReviewStatus::Pending
                || !goal_is_current(goal, &state.goal_id, &state.turn_id, state.goal_revision)
            {
                return Err("Goal MCP approval is stale or already consumed".into());
            }
            let now = now_ms()?;
            if now < state.created_at_ms || now >= state.expires_at_ms {
                return Err("Goal MCP proposal has expired".into());
            }
            state.status = ReviewStatus::Approved;
            state.approved_at_ms = Some(now);
            Ok(())
        })
    }

    pub fn reject(
        &mut self,
        review_id: &str,
        user_initiated: bool,
    ) -> Result<LeaseSnapshot, String> {
        if !user_initiated {
            return Err("Goal MCP rejection requires a visible user action".into());
        }
        let result = self.transaction(|state| {
            if state.review_id != review_id
                || !matches!(state.status, ReviewStatus::Pending | ReviewStatus::Approved)
            {
                return Err("Goal MCP proposal is not rejectable".into());
            }
            state.status = ReviewStatus::Rejected;
            state.closed_at_ms = Some(now_ms()?.max(state.created_at_ms));
            Ok(())
        })?;
        self.bound = None;
        Ok(result)
    }

    /// Persist expiry as a terminal, redacted state. No approval or dispatch
    /// is produced, and the original arguments are irreversibly discarded.
    /// A not-yet-expired proposal returns false without changing the file.
    pub fn expire_if_due(&mut self) -> Result<bool, String> {
        let observed = self.snapshot()?;
        if !matches!(
            observed.status,
            ReviewStatus::Pending | ReviewStatus::Approved
        ) {
            return Ok(false);
        }
        let now = now_ms()?;
        if now < observed.created_at_ms {
            return Err("Goal MCP clock moved backwards; dispatch is unsafe".into());
        }
        if now < observed.expires_at_ms {
            return Ok(false);
        }
        self.transaction(|state| {
            if state != &observed
                || !matches!(state.status, ReviewStatus::Pending | ReviewStatus::Approved)
            {
                return Err("Goal MCP expiry checkpoint changed".into());
            }
            state.status = ReviewStatus::Expired;
            state.closed_at_ms = Some(now_ms()?.max(state.created_at_ms));
            Ok(())
        })?;
        self.bound = None;
        Ok(true)
    }

    /// Never performs MCP execution. The caller MUST recheck current live
    /// connection generation and exact verified schema/Goal turn. On success,
    /// a durable uncertain marker has already been synced BEFORE any outbound
    /// execution is possible. Failures never produce a dispatch permit.
    pub fn begin_dispatch(
        &mut self,
        goal: &GoalSnapshot,
        review_id: &str,
        current_connection_generation: u64,
        current_verified_tools: &[Value],
    ) -> Result<DispatchPermit, String> {
        let bound = self
            .bound
            .as_ref()
            .ok_or("Goal MCP proposal arguments unavailable; restart cannot dispatch")?;
        let state = self.snapshot()?;
        if state.review_id != review_id
            || state.status != ReviewStatus::Approved
            || state.connection_generation != current_connection_generation
            || !goal_is_current(goal, &state.goal_id, &state.turn_id, state.goal_revision)
            || now_ms()? >= state.expires_at_ms
        {
            return Err("Goal MCP dispatch is stale, unapproved or expired".into());
        }
        mcp_client::validate_model_arguments(
            current_verified_tools,
            &state.tool_name,
            &bound.arguments,
        )?;
        let current_definition = current_verified_tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(&state.tool_name))
            .ok_or("Goal MCP tool disappeared from verified MCP generation")?;
        if current_definition != &bound.tool_definition {
            return Err("Goal MCP tool definition changed since review".into());
        }
        let dispatch_id = Uuid::new_v4().to_string();
        self.transaction(|record| {
            if record != &state || record.status != ReviewStatus::Approved {
                return Err("Goal MCP checkpoint changed before dispatch".into());
            }
            record.status = ReviewStatus::DispatchUncertain;
            record.dispatch_id = Some(dispatch_id.clone());
            Ok(())
        })?;
        let bound = self
            .bound
            .take()
            .ok_or("Goal MCP proposal already consumed")?;
        self.dispatched_id = Some(dispatch_id.clone());
        Ok(DispatchPermit {
            ticket: DispatchTicket {
                review_id: review_id.to_owned(),
                dispatch_id,
                turn_id: self.turn_id.clone(),
            },
            proposal: ModelToolCall {
                id: bound.provider_call_id,
                name: state.tool_name,
                arguments: bound.arguments,
            },
        })
    }

    /// This attests ONLY the caller-reported real MCP response/error. It does
    /// not invent evidence, save results, retry tools or automatically resume
    /// a Goal. On network uncertainty leave DispatchUncertain untouched.
    pub fn report_outcome(
        &mut self,
        ticket: &DispatchTicket,
        outcome: RemoteOutcome,
    ) -> Result<LeaseSnapshot, String> {
        if self.dispatched_id.as_deref() != Some(&ticket.dispatch_id)
            || ticket.turn_id != self.turn_id
        {
            return Err("Goal MCP dispatch ticket missing or stale".into());
        }
        let result = self.transaction(|state| {
            if state.status != ReviewStatus::DispatchUncertain
                || state.review_id != ticket.review_id
                || state.dispatch_id.as_deref() != Some(&ticket.dispatch_id)
            {
                return Err("Goal MCP dispatch already completed or cannot be verified".into());
            }
            state.status = match outcome {
                RemoteOutcome::Confirmed => ReviewStatus::Confirmed,
                RemoteOutcome::ReportedError => ReviewStatus::ReportedError,
            };
            state.closed_at_ms = Some(now_ms()?.max(state.created_at_ms));
            Ok(())
        })?;
        self.dispatched_id = None;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mobile_goal::{GoalDriver, GoalSpec, GoalStep};

    struct SyntheticHome(PathBuf);

    impl SyntheticHome {
        fn new() -> Self {
            let folder =
                std::env::temp_dir().join(format!("synthetic-goal-mcp-{}", Uuid::new_v4()));
            fs::create_dir(&folder).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self(folder)
        }
        fn goal(&self) -> (GoalDriver, GoalSnapshot) {
            let mut driver = GoalDriver::create(
                &self.0,
                GoalSpec {
                    title: "Synthetic Goal".into(),
                    steps: vec![GoalStep {
                        description: "Test synthetic user-owned provider tool".into(),
                        requires_approval: false,
                    }],
                    max_turns: 4,
                    max_retries_per_step: 1,
                },
            )
            .unwrap();
            driver.begin_turn().unwrap();
            let snapshot = driver.snapshot();
            (driver, snapshot)
        }
    }

    impl Drop for SyntheticHome {
        fn drop(&mut self) {
            // Created by this test, never a real home or user file path.
            if self
                .0
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("synthetic-goal-mcp-"))
            {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    fn tools() -> Vec<Value> {
        vec![serde_json::json!({
            "name":"look_up",
            "description":"Synthetic fixture",
            "inputSchema":{"type":"object","additionalProperties":false,
                "properties":{"query":{"type":"string","minLength":1,"maxLength":80},
                    "limit":{"type":"integer","minimum":0,"maximum":3}},
                "required":["query"]}
        })]
    }

    fn proposal(id: &str) -> ModelToolCall {
        ModelToolCall {
            id: id.to_owned(),
            name: "look_up".into(),
            arguments: serde_json::json!({
                "query":"Synthetic secret=NEVER_PERSIST_PRIVATE_PAYLOAD", "limit":2
            }),
        }
    }

    fn proposed(home: &Path, goal: &GoalSnapshot, provider_id: &str) -> GoalMcpLease {
        GoalMcpLease::propose(
            home,
            goal,
            &Uuid::new_v4().to_string(),
            7,
            &proposal(provider_id),
            &tools(),
            60_000,
        )
        .unwrap()
    }

    #[test]
    fn only_one_provider_proposal_per_real_goal_turn_without_raw_args_on_disk() {
        let folder = SyntheticHome::new();
        let (_driver, goal) = folder.goal();
        let mut lease = proposed(&folder.0, &goal, "call_openai_123");
        let review = lease.snapshot().unwrap();
        assert_eq!(review.status, ReviewStatus::Pending);
        assert_eq!(review.goal_id, goal.id);
        assert_eq!(review.turn_id, goal.active_turn.as_ref().unwrap().id);
        assert_eq!(review.connection_generation, 7);
        let path = file_path(&folder.0, &review.turn_id).unwrap();
        let disk = fs::read_to_string(path).unwrap();
        assert!(!disk.contains("arguments"));
        assert!(!disk.contains("schema"));
        assert!(!disk.contains("NEVER_PERSIST_PRIVATE_PAYLOAD"));
        assert!(!disk.contains("call_openai_123"));
        assert!(!disk.contains("result"));
        assert!(GoalMcpLease::propose(
            &folder.0,
            &goal,
            &Uuid::new_v4().to_string(),
            7,
            &proposal("toolu_anthropic_123"),
            &tools(),
            60_000,
        )
        .is_err());
        assert!(lease.approve(&goal, &review.review_id, false).is_err());
        // Failed consent must not poison or promote the pending lease.
        assert_eq!(lease.snapshot().unwrap().status, ReviewStatus::Pending);
        assert_eq!(
            lease
                .approve(&goal, &review.review_id, true)
                .unwrap()
                .status,
            ReviewStatus::Approved
        );
    }

    #[test]
    fn proposal_validates_current_schema_and_ids_before_creating_checkpoint() {
        let folder = SyntheticHome::new();
        let (_driver, goal) = folder.goal();
        let mut invalid = proposal("toolu_anthropic_123");
        invalid.arguments = serde_json::json!({"query":"ok","limit":4});
        assert!(GoalMcpLease::propose(
            &folder.0,
            &goal,
            &Uuid::new_v4().to_string(),
            7,
            &invalid,
            &tools(),
            60_000
        )
        .is_err());
        assert!(
            !file_path(&folder.0, &goal.active_turn.as_ref().unwrap().id)
                .unwrap()
                .exists()
        );
        invalid.arguments = serde_json::json!({"query":"ok","unknown":true});
        assert!(GoalMcpLease::propose(
            &folder.0,
            &goal,
            &Uuid::new_v4().to_string(),
            7,
            &invalid,
            &tools(),
            60_000
        )
        .is_err());
        invalid.arguments = serde_json::json!({"query":"ok"});
        for invalid_request in [
            "not-a-uuid",
            "94de94de-f21a-3ee0-9db2-8e4d42f2c904",
            "94DE94DE-F21A-4EE0-9DB2-8E4D42F2C904",
        ] {
            assert!(GoalMcpLease::propose(
                &folder.0,
                &goal,
                invalid_request,
                7,
                &invalid,
                &tools(),
                60_000
            )
            .is_err());
        }
        assert!(GoalMcpLease::propose(
            &folder.0,
            &goal,
            &Uuid::new_v4().to_string(),
            0,
            &invalid,
            &tools(),
            60_000
        )
        .is_err());
        assert!(GoalMcpLease::propose(
            &folder.0,
            &goal,
            &Uuid::new_v4().to_string(),
            7,
            &invalid,
            &tools(),
            999
        )
        .is_err());
        assert!(GoalMcpLease::propose(
            &folder.0,
            &goal,
            &Uuid::new_v4().to_string(),
            7,
            &invalid,
            &tools(),
            MAX_TTL_MS + 1
        )
        .is_err());
        assert!(
            !file_path(&folder.0, &goal.active_turn.as_ref().unwrap().id)
                .unwrap()
                .exists()
        );
        assert!(GoalMcpLease::propose(
            &folder.0,
            &goal,
            &Uuid::new_v4().to_string(),
            7,
            &invalid,
            &tools(),
            60_000
        )
        .is_ok());
    }

    #[test]
    fn explicit_approval_before_one_time_dispatch_and_actual_outcome() {
        let folder = SyntheticHome::new();
        let (_driver, goal) = folder.goal();
        let mut lease = proposed(&folder.0, &goal, "toolu_approved_456");
        let review = lease.snapshot().unwrap().review_id;
        assert!(lease.begin_dispatch(&goal, &review, 7, &tools()).is_err());
        lease.approve(&goal, &review, true).unwrap();
        assert!(lease.begin_dispatch(&goal, &review, 8, &tools()).is_err());
        let mut changed_tools = tools();
        changed_tools[0]["description"] = serde_json::json!("Changed after approval");
        assert!(lease
            .begin_dispatch(&goal, &review, 7, &changed_tools)
            .is_err());
        assert_eq!(lease.snapshot().unwrap().status, ReviewStatus::Approved);
        let permit = lease.begin_dispatch(&goal, &review, 7, &tools()).unwrap();
        assert_eq!(
            lease.snapshot().unwrap().status,
            ReviewStatus::DispatchUncertain
        );
        let disk = fs::read_to_string(
            file_path(&folder.0, &goal.active_turn.as_ref().unwrap().id).unwrap(),
        )
        .unwrap();
        assert!(disk.contains("dispatch_uncertain"));
        assert!(!disk.contains("NEVER_PERSIST_PRIVATE_PAYLOAD"));
        let (ticket, call) = permit.into_call();
        assert_eq!(call.id, "toolu_approved_456");
        assert_eq!(call.name, "look_up");
        assert_eq!(call.arguments["limit"], 2);
        assert!(lease.begin_dispatch(&goal, &review, 7, &tools()).is_err());
        assert_eq!(
            lease
                .report_outcome(&ticket, RemoteOutcome::Confirmed)
                .unwrap()
                .status,
            ReviewStatus::Confirmed
        );
        assert!(lease
            .report_outcome(&ticket, RemoteOutcome::ReportedError)
            .is_err());
        assert!(lease.reject(&review, true).is_err());
    }

    #[test]
    fn restart_interrupts_unissued_proposals_and_never_replays_uncertain_dispatch() {
        for dispatch in [false, true] {
            let folder = SyntheticHome::new();
            let (_driver, goal) = folder.goal();
            let mut lease = proposed(&folder.0, &goal, "call_synthetic");
            let review = lease.snapshot().unwrap().review_id;
            lease.approve(&goal, &review, true).unwrap();
            let permit = if dispatch {
                Some(lease.begin_dispatch(&goal, &review, 7, &tools()).unwrap())
            } else {
                None
            };
            drop(lease);
            let mut reopened =
                GoalMcpLease::open(&folder.0, &goal.active_turn.as_ref().unwrap().id).unwrap();
            assert_eq!(
                reopened.snapshot().unwrap().status,
                if dispatch {
                    ReviewStatus::DispatchUncertain
                } else {
                    ReviewStatus::Interrupted
                }
            );
            assert!(reopened.approve(&goal, &review, true).is_err());
            assert!(reopened
                .begin_dispatch(&goal, &review, 7, &tools())
                .is_err());
            if let Some(permit) = permit {
                let (ticket, _) = permit.into_call();
                assert!(reopened
                    .report_outcome(&ticket, RemoteOutcome::Confirmed)
                    .is_err());
            }
        }
    }

    #[test]
    fn rejection_and_stale_goal_revision_cannot_be_approvals() {
        let folder = SyntheticHome::new();
        let (_driver, goal) = folder.goal();
        let mut lease = proposed(&folder.0, &goal, "call_reject_me");
        let review = lease.snapshot().unwrap().review_id;
        let mut changed_goal = goal.clone();
        changed_goal.revision += 1;
        assert!(lease.approve(&changed_goal, &review, true).is_err());
        assert!(lease
            .approve(&goal, &Uuid::new_v4().to_string(), true)
            .is_err());
        assert_eq!(
            lease.reject(&review, true).unwrap().status,
            ReviewStatus::Rejected
        );
        assert!(lease.approve(&goal, &review, true).is_err());
        assert!(lease.reject(&review, true).is_err());
        assert!(lease.begin_dispatch(&goal, &review, 7, &tools()).is_err());
    }

    #[test]
    fn expiration_is_durable_terminal_and_cannot_be_approved_or_replayed() {
        let folder = SyntheticHome::new();
        let (_driver, goal) = folder.goal();
        let mut lease = GoalMcpLease::propose(
            &folder.0,
            &goal,
            &Uuid::new_v4().to_string(),
            7,
            &proposal("call_expires"),
            &tools(),
            MIN_TTL_MS,
        )
        .unwrap();
        assert!(!lease.expire_if_due().unwrap());
        let review = lease.snapshot().unwrap().review_id;
        std::thread::sleep(std::time::Duration::from_millis(MIN_TTL_MS + 30));
        assert!(lease.expire_if_due().unwrap());
        assert_eq!(lease.snapshot().unwrap().status, ReviewStatus::Expired);
        assert!(!lease.expire_if_due().unwrap());
        assert!(lease.approve(&goal, &review, true).is_err());
        assert!(lease.begin_dispatch(&goal, &review, 7, &tools()).is_err());
        let recovered =
            GoalMcpLease::open(&folder.0, &goal.active_turn.as_ref().unwrap().id).unwrap();
        assert_eq!(recovered.snapshot().unwrap().status, ReviewStatus::Expired);
    }

    #[test]
    fn remote_reported_error_is_terminal_without_persisting_result_or_retrying() {
        let folder = SyntheticHome::new();
        let (_driver, goal) = folder.goal();
        let mut lease = proposed(&folder.0, &goal, "call_remote_error");
        let review = lease.snapshot().unwrap().review_id;
        lease.approve(&goal, &review, true).unwrap();
        let permit = lease.begin_dispatch(&goal, &review, 7, &tools()).unwrap();
        let (ticket, _call) = permit.into_call();
        let receipt = lease
            .report_outcome(&ticket, RemoteOutcome::ReportedError)
            .unwrap();
        assert_eq!(receipt.status, ReviewStatus::ReportedError);
        assert!(receipt.closed_at_ms.is_some());
        assert!(lease
            .report_outcome(&ticket, RemoteOutcome::Confirmed)
            .is_err());
        let disk = fs::read_to_string(file_path(&folder.0, &receipt.turn_id).unwrap()).unwrap();
        assert!(!disk.contains("arguments"));
        assert!(!disk.contains("NEVER_PERSIST_PRIVATE_PAYLOAD"));
        assert!(!disk.contains("response"));
        let mut reopened = GoalMcpLease::open(&folder.0, &receipt.turn_id).unwrap();
        assert_eq!(
            reopened.snapshot().unwrap().status,
            ReviewStatus::ReportedError
        );
        assert!(reopened
            .begin_dispatch(&goal, &review, 7, &tools())
            .is_err());
    }

    #[test]
    fn pending_restart_and_changed_but_valid_checkpoint_fail_closed() {
        let folder = SyntheticHome::new();
        let (_driver, goal) = folder.goal();
        let mut lease = proposed(&folder.0, &goal, "toolu_pending_restart");
        let review = lease.snapshot().unwrap().review_id;
        let path = file_path(&folder.0, &goal.active_turn.as_ref().unwrap().id).unwrap();
        let mut forged: LeaseSnapshot = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        forged.connection_generation += 1; // valid shape, but changed since review
        fs::write(&path, serde_json::to_vec(&forged).unwrap()).unwrap();
        assert!(lease.approve(&goal, &review, true).is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            serde_json::to_vec(&forged).unwrap()
        );
        drop(lease);
        let mut recovered = GoalMcpLease::open(&folder.0, &forged.turn_id).unwrap();
        assert_eq!(
            recovered.snapshot().unwrap().status,
            ReviewStatus::Interrupted
        );
        assert!(recovered.approve(&goal, &review, true).is_err());
        assert!(recovered
            .begin_dispatch(&goal, &review, 7, &tools())
            .is_err());
    }

    #[test]
    fn corrupt_checkpoint_and_unsafe_target_fail_closed_without_overwrite() {
        let folder = SyntheticHome::new();
        let (_driver, goal) = folder.goal();
        let mut lease = proposed(&folder.0, &goal, "call_corrupt");
        let review = lease.snapshot().unwrap().review_id;
        let path = file_path(&folder.0, &goal.active_turn.as_ref().unwrap().id).unwrap();
        fs::write(&path, b"{invalid").unwrap();
        assert!(lease.approve(&goal, &review, true).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{invalid");
        assert!(GoalMcpLease::open(&folder.0, &goal.active_turn.as_ref().unwrap().id).is_err());
        // A new real turn's path cannot overwrite an already occupied target.
        let other = Uuid::new_v4().to_string();
        let occupied = file_path(&folder.0, &other).unwrap();
        fs::create_dir(&occupied).unwrap();
        assert!(safe_file(&occupied).is_err());
    }

    #[test]
    fn process_writer_lock_and_duplicate_goal_turn_are_serialized() {
        let folder = SyntheticHome::new();
        let (_driver, goal) = folder.goal();
        let home = folder.0.clone();
        let goal_two = goal.clone();
        let one = std::thread::spawn({
            let home = home.clone();
            move || {
                GoalMcpLease::propose(
                    &home,
                    &goal,
                    &Uuid::new_v4().to_string(),
                    7,
                    &proposal("call_one"),
                    &tools(),
                    60_000,
                )
                .is_ok()
            }
        });
        let two = std::thread::spawn(move || {
            GoalMcpLease::propose(
                &home,
                &goal_two,
                &Uuid::new_v4().to_string(),
                7,
                &proposal("toolu_two"),
                &tools(),
                60_000,
            )
            .is_ok()
        });
        let successes = (one.join().unwrap() as usize) + (two.join().unwrap() as usize);
        assert_eq!(successes, 1);
    }
}
