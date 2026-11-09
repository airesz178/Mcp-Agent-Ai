//! Durable, user-initiated Android AI delegation records. NO provider calls or
//! background executor live here. The native host must own the real provider
//! invocation and maintain the start ticket until it reports a real outcome.
//!
//! API (all methods are synchronous, no replay/scheduler):
//! - DelegationStore::create(home: &Path) -> Result<Self, String>: new only.
//! - DelegationStore::open(home: &Path) -> Result<Self, String>: existing, marks
//!   unfinished Running jobs Interrupted (never resubmits them).
//! - DelegationStore::open_or_create(home: &Path) -> Result<Self, String>.
//! - store.submit(provider, model, task, user_initiated) -> DelegationJob (Queued).
//! - store.start(job_id, user_initiated) -> DelegationTicket (Running).
//! - store.submit_and_start(provider, model, task, user_initiated)
//!   -> (DelegationJob, DelegationTicket) (ONE atomic Running transition;
//!   no persisted Queued intermediate or orphan on capacity failure).
//! - store.complete(job_id, ticket_id, answer) -> DelegationJob.
//! - store.fail(job_id, ticket_id, reason) -> DelegationJob.
//! - store.cancel(job_id, ticket_id: Option<&str>, user_initiated) -> CancelReceipt.
//! - store.list() -> Vec<DelegationJob> (newest first); store.get(job_id).
//!
//! The supplied home MUST be a dedicated directory under Android Context.filesDir,
//! never a WebView-selected path. Caller must obtain a real visible user gesture;
//! a supplied boolean is not cryptographic proof. NEVER pass API keys, tool args
//! or secrets in task/answer text; common credential shapes are additionally
//! rejected but this module cannot identify arbitrary confidential prose.
//! Completing/failing requires the exact durable per-run ticket. After process
//! death, only explicit user resubmission can create a new attempt.

use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const DATA_FILE: &str = "mobile-ai-delegations-v1.json";
const LOCK_FILE: &str = ".mobile-ai-delegations-v1.lock";
pub const MAX_JOBS: usize = 16;
pub const MAX_RUNNING: usize = 2;
pub const MAX_QUEUED: usize = 12;
pub const MAX_TASK_BYTES: usize = 3_000;
pub const MAX_ANSWER_BYTES: usize = 20 * 1024;
const MAX_REASON_BYTES: usize = 1_024;
const MAX_MODEL_BYTES: usize = 100;
const MAX_CATALOG_BYTES: usize = 512 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(3);
static IN_PROCESS_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl DelegationStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DelegationJob {
    pub id: String,
    pub order: u64,
    pub provider: String,
    pub model: String,
    pub task: String,
    pub status: DelegationStatus,
    /// Only present during Running. A random per-run token, NOT an API key.
    pub ticket: Option<String>,
    /// Verified output from the real host provider call. Never fabricated.
    pub answer: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DelegationTicket {
    pub job_id: String,
    pub ticket_id: String,
    pub provider: String,
    pub model: String,
    pub task: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CancelReceipt {
    pub job: DelegationJob,
    /// For a formerly running job, identifies which in-flight call the host
    /// should abort. Side effects may already have happened remotely.
    pub cancelled_ticket: Option<String>,
    pub was_running: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Catalog {
    version: u8,
    revision: u64,
    next_order: u64,
    jobs: Vec<DelegationJob>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            version: 1,
            revision: 0,
            next_order: 1,
            jobs: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DelegationStore {
    home: PathBuf,
}

fn valid_uuid(id: &str) -> bool {
    Uuid::parse_str(id).is_ok_and(|u| u.hyphenated().to_string() == id)
}

fn valid_text(s: &str, max: usize, allow_newline: bool) -> bool {
    !s.trim().is_empty()
        && s.len() <= max
        && !s.chars().any(|ch| {
            (ch.is_control() && !(allow_newline && (ch == '\n' || ch == '\t')))
                || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
}

/// Best-effort anti-credential check; callers must still explicitly exclude
/// arbitrary private information from persisted task/answer.
fn obvious_credentials(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    if [
        "bearer ",
        "authorization:",
        "api_key",
        "apikey",
        "api-key",
        "sk-proj-",
        "secret=",
        "token=",
        "password=",
        "-----begin private key",
        "x-api-key",
    ]
    .iter()
    .any(|mark| lower.contains(mark))
    {
        return true;
    }
    // Check a long OpenAI-style token prefix without rejecting ordinary
    // words such as "risk-based" that happen to contain the letters "sk-".
    lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .any(|word| word.starts_with("sk-") && word.len() >= 16)
}

fn valid_provider(provider: &str) -> bool {
    matches!(provider, "openai" | "anthropic" | "openrouter")
}

fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= MAX_MODEL_BYTES
        && model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:".contains(&b))
        && !model.contains("..")
        && !model.contains("://")
        && !model.starts_with('/')
}

/// Prime delegates share these validation rules with ordinary AI delegates.
/// A caller-provided provider/model or a saved report is never accepted merely
/// because it parses as JSON; credentials, controls and oversized text fail.
pub(crate) fn valid_prime_provider_model(provider: &str, model: &str) -> bool {
    valid_provider(provider) && valid_model(model)
}

pub(crate) fn valid_prime_text(text: &str, max: usize) -> bool {
    valid_text(text, max, true) && !obvious_credentials(text)
}

impl Catalog {
    fn validate(&self) -> Result<(), String> {
        let corrupt = || String::from("Delegation state is invalid; existing records preserved");
        if self.version != 1
            || self.jobs.len() > MAX_JOBS
            || self.revision == u64::MAX
            || self.next_order == 0
            || self.next_order == u64::MAX
        {
            return Err(corrupt());
        }
        let mut ids = HashSet::new();
        let mut tickets = HashSet::new();
        let mut prior = 0u64;
        let mut running = 0;
        let mut queued = 0;
        for job in &self.jobs {
            if !valid_uuid(&job.id)
                || !ids.insert(&job.id)
                || job.order <= prior
                || job.order >= self.next_order
                || !valid_provider(&job.provider)
                || !valid_model(&job.model)
                || !valid_text(&job.task, MAX_TASK_BYTES, true)
                || obvious_credentials(&job.task)
            {
                return Err(corrupt());
            }
            prior = job.order;
            if job
                .answer
                .as_deref()
                .is_some_and(|s| !valid_text(s, MAX_ANSWER_BYTES, true) || obvious_credentials(s))
                || job.error.as_deref().is_some_and(|s| {
                    !valid_text(s, MAX_REASON_BYTES, true) || obvious_credentials(s)
                })
            {
                return Err(corrupt());
            }
            match job.status {
                DelegationStatus::Queued => {
                    queued += 1;
                    if job.ticket.is_some() || job.answer.is_some() || job.error.is_some() {
                        return Err(corrupt());
                    }
                }
                DelegationStatus::Running => {
                    running += 1;
                    let t = job.ticket.as_deref().ok_or_else(corrupt)?;
                    if !valid_uuid(t)
                        || !tickets.insert(t)
                        || job.answer.is_some()
                        || job.error.is_some()
                    {
                        return Err(corrupt());
                    }
                }
                DelegationStatus::Completed => {
                    if job.ticket.is_some() || job.answer.is_none() || job.error.is_some() {
                        return Err(corrupt());
                    }
                }
                DelegationStatus::Failed | DelegationStatus::Interrupted => {
                    if job.ticket.is_some() || job.answer.is_some() || job.error.is_none() {
                        return Err(corrupt());
                    }
                }
                DelegationStatus::Cancelled => {
                    if job.ticket.is_some() || job.answer.is_some() || job.error.is_some() {
                        return Err(corrupt());
                    }
                }
            }
        }
        if running > MAX_RUNNING || queued > MAX_QUEUED {
            return Err(corrupt());
        }
        Ok(())
    }
}

pub(crate) fn ensure_private_home(home: &Path, allow_create: bool) -> Result<(), String> {
    if !home.is_absolute() || home.as_os_str().is_empty() {
        return Err("Require an absolute, dedicated app-private delegation directory".into());
    }
    if allow_create {
        let already_existed = fs::symlink_metadata(home).is_ok();
        fs::create_dir_all(home).map_err(|_| "Cannot create private delegation directory")?;
        #[cfg(unix)]
        if !already_existed {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(home, fs::Permissions::from_mode(0o700))
                .map_err(|_| "Cannot restrict delegation directory permissions")?;
        }
        #[cfg(not(unix))]
        let _ = already_existed;
    }
    let meta = fs::symlink_metadata(home).map_err(|_| "Private delegation directory missing")?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("Unsafe delegation directory (symlink/reparse path)".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.mode() & 0o077 != 0 {
            // Do not silently modify permissions on a caller-selected directory.
            return Err("Delegation directory must be owner-private (0700)".into());
        }
    }
    Ok(())
}

pub(crate) fn safe_regular(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() != 1 || meta.mode() & 0o077 != 0 {
                    return Err("Unsafe delegation file permissions or hardlink".into());
                }
            }
            Ok(Some(meta))
        }
        Ok(_) => Err("Unsafe delegation file type; no changes made".into()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("Cannot inspect delegation state file".into()),
    }
}

pub(crate) struct WriterLock(File);
impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = unlock_file(&self.0);
    }
}

pub(crate) fn writer_lock(home: &Path) -> Result<WriterLock, String> {
    ensure_private_home(home, false)?;
    let path = home.join(LOCK_FILE);
    safe_regular(&path)?;
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
        #[cfg(any(target_os = "android", target_os = "linux"))]
        opts.custom_flags(0o400000); // O_NOFOLLOW
    }
    let file = opts
        .open(&path)
        .map_err(|_| "Cannot open delegation lock file")?;
    let meta = safe_regular(&path)?.ok_or("Delegation lock file disappeared")?;
    if meta.len() != 0
        || file
            .metadata()
            .map_err(|_| "Cannot inspect lock handle")?
            .len()
            != 0
    {
        return Err("Delegation lock file is modified or corrupt".into());
    }
    let start = Instant::now();
    loop {
        match lock_file(&file) {
            Ok(()) => return Ok(WriterLock(file)),
            Err(_) if start.elapsed() < LOCK_TIMEOUT => thread::sleep(Duration::from_millis(20)),
            Err(_) => return Err("Delegation store busy; no changes made".into()),
        }
    }
}

#[cfg(unix)]
fn lock_file(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    if unsafe { flock(file.as_raw_fd(), 2 | 4) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(unix)]
fn unlock_file(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    if unsafe { flock(file.as_raw_fd(), 8) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(windows)]
fn lock_file(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    unsafe extern "system" {
        fn LockFile(
            handle: *mut std::ffi::c_void,
            off_lo: u32,
            off_hi: u32,
            n_lo: u32,
            n_hi: u32,
        ) -> i32;
    }
    if unsafe { LockFile(file.as_raw_handle(), 0, 0, 1, 0) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(windows)]
fn unlock_file(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    unsafe extern "system" {
        fn UnlockFile(
            handle: *mut std::ffi::c_void,
            off_lo: u32,
            off_hi: u32,
            n_lo: u32,
            n_hi: u32,
        ) -> i32;
    }
    if unsafe { UnlockFile(file.as_raw_handle(), 0, 0, 1, 0) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn replace_atomic(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let src: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(src.as_ptr(), dst.as_ptr(), 0x1 | 0x8) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(not(windows))]
pub(crate) fn replace_atomic(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn read_catalog(home: &Path) -> Result<Catalog, String> {
    let target = home.join(DATA_FILE);
    let Some(meta) = safe_regular(&target)? else {
        return Err("Delegation store is missing; use create/open_or_create explicitly".into());
    };
    if meta.len() > MAX_CATALOG_BYTES as u64 {
        return Err("Delegation state exceeds byte limit; no changes made".into());
    }
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(any(target_os = "android", target_os = "linux"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(0o400000);
    }
    let file = opts
        .open(&target)
        .map_err(|_| "Cannot read delegation state")?;
    let mut data = Vec::new();
    file.take(MAX_CATALOG_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|_| "Cannot read delegation state")?;
    if data.len() > MAX_CATALOG_BYTES {
        return Err("Delegation state exceeds byte limit; no changes made".into());
    }
    let catalog: Catalog = serde_json::from_slice(&data)
        .map_err(|_| "Delegation state is corrupt; no changes made".to_owned())?;
    catalog.validate()?;
    Ok(catalog)
}

fn save_catalog(home: &Path, catalog: &Catalog) -> Result<(), String> {
    catalog.validate()?;
    let bytes = serde_json::to_vec(catalog).map_err(|_| "Cannot encode delegation state")?;
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err("Delegation state exceeds byte limit; no changes made".into());
    }
    // Caller holds both process mutex and kernel lock. Never overwrite a link,
    // directory, or unexpected existing file.
    safe_regular(&home.join(DATA_FILE))?;
    let guard = TempFile(home.join(format!(".mobile-ai-delegation-{}.tmp", Uuid::new_v4())));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&guard.0)
        .map_err(|_| "Cannot stage delegation state")?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| "Cannot sync delegation state")?;
    drop(file);
    replace_atomic(&guard.0, &home.join(DATA_FILE))
        .map_err(|_| "Cannot atomically replace delegation state")?;
    #[cfg(unix)]
    {
        // Persist the rename itself on POSIX when the filesystem permits it.
        if let Ok(directory) = File::open(home) {
            let _ = directory.sync_all();
        }
    }
    Ok(())
}

impl DelegationStore {
    fn transaction<R>(
        &self,
        f: impl FnOnce(&mut Catalog) -> Result<(R, bool), String>,
    ) -> Result<R, String> {
        let _local = IN_PROCESS_LOCK
            .lock()
            .map_err(|_| "Delegation mutex poisoned")?;
        let _os = writer_lock(&self.home)?;
        let mut state = read_catalog(&self.home)?;
        let (output, changed) = f(&mut state)?;
        if changed {
            state.revision = state
                .revision
                .checked_add(1)
                .ok_or("Delegation revision exhausted")?;
            save_catalog(&self.home, &state)?;
        }
        Ok(output)
    }

    fn initialize(home: &Path, mode: InitMode) -> Result<Self, String> {
        ensure_private_home(home, mode != InitMode::Open)?;
        let store = Self {
            home: home.to_path_buf(),
        };
        let _local = IN_PROCESS_LOCK
            .lock()
            .map_err(|_| "Delegation mutex poisoned")?;
        let _os = writer_lock(home)?;
        let exists = safe_regular(&home.join(DATA_FILE))?.is_some();
        match (mode, exists) {
            (InitMode::Create, true) => {
                return Err("Delegation state already exists; nothing overwritten".into())
            }
            (InitMode::Open, false) => return Err("Delegation state missing; create first".into()),
            (_, false) => save_catalog(home, &Catalog::default())?,
            (_, true) => {
                let mut state = read_catalog(home)?;
                let mut interrupted = false;
                for job in &mut state.jobs {
                    if job.status == DelegationStatus::Running {
                        job.status = DelegationStatus::Interrupted;
                        job.ticket = None;
                        job.error =
                            Some("Interrupted after process restart; never auto-replayed".into());
                        interrupted = true;
                    }
                }
                if interrupted {
                    state.revision = state
                        .revision
                        .checked_add(1)
                        .ok_or("Delegation revision exhausted")?;
                    save_catalog(home, &state)?;
                }
            }
        }
        Ok(store)
    }

    /// Initialize a NEW private catalog, refusing any existing state.
    pub fn create(home: &Path) -> Result<Self, String> {
        Self::initialize(home, InitMode::Create)
    }
    /// Open existing state. Any persisted Running jobs are interrupted; no replay.
    pub fn open(home: &Path) -> Result<Self, String> {
        Self::initialize(home, InitMode::Open)
    }
    /// Convenience bootstrap: create only if missing, otherwise recover.
    pub fn open_or_create(home: &Path) -> Result<Self, String> {
        Self::initialize(home, InitMode::OpenOrCreate)
    }

    pub fn submit(
        &self,
        provider: &str,
        model: &str,
        task: &str,
        user_initiated: bool,
    ) -> Result<DelegationJob, String> {
        if !user_initiated {
            return Err("A real user gesture is required to submit a delegation".into());
        }
        if !valid_provider(provider)
            || !valid_model(model)
            || !valid_text(task, MAX_TASK_BYTES, true)
            || obvious_credentials(task)
        {
            return Err("Invalid provider, model or task; do not include credentials".into());
        }
        self.transaction(|catalog| {
            if catalog
                .jobs
                .iter()
                .filter(|j| j.status == DelegationStatus::Queued)
                .count()
                >= MAX_QUEUED
            {
                return Err("Delegation queue already contains 12 waiting tasks".into());
            }
            if catalog.jobs.len() == MAX_JOBS {
                let oldest_terminal = catalog
                    .jobs
                    .iter()
                    .position(|j| j.status.is_terminal())
                    .ok_or("Delegation retention full; active jobs preserved")?;
                catalog.jobs.remove(oldest_terminal);
            }
            let order = catalog.next_order;
            catalog.next_order = catalog
                .next_order
                .checked_add(1)
                .filter(|v| *v < u64::MAX)
                .ok_or("Delegation job order exhausted")?;
            let job = DelegationJob {
                id: Uuid::new_v4().to_string(),
                order,
                provider: provider.to_owned(),
                model: model.to_owned(),
                task: task.to_owned(),
                status: DelegationStatus::Queued,
                ticket: None,
                answer: None,
                error: None,
            };
            catalog.jobs.push(job.clone());
            Ok((job, true))
        })
    }

    /// User-triggered immediate delegation: reserve a running slot and create
    /// the durable Running job in ONE transaction before the host invokes its
    /// actual provider request. Never call submit() followed by start() for an
    /// immediate run: an intervening capacity failure would leave an orphaned
    /// Queued job with no UI action to start it.
    ///
    /// Only a terminal record may be evicted when retention is full. A failed
    /// capacity, validation, corruption, or persistence check returns Err
    /// without leaving any new job/ticket in the durable catalog.
    pub fn submit_and_start(
        &self,
        provider: &str,
        model: &str,
        task: &str,
        user_initiated: bool,
    ) -> Result<(DelegationJob, DelegationTicket), String> {
        if !user_initiated {
            return Err("A real user gesture is required to run a delegation".into());
        }
        if !valid_provider(provider)
            || !valid_model(model)
            || !valid_text(task, MAX_TASK_BYTES, true)
            || obvious_credentials(task)
        {
            return Err("Invalid provider, model or task; do not include credentials".into());
        }
        self.transaction(|catalog| {
            if catalog
                .jobs
                .iter()
                .filter(|j| j.status == DelegationStatus::Running)
                .count()
                >= MAX_RUNNING
            {
                return Err("Maximum two concurrent delegated AI tasks".into());
            }
            // Resolve EVERY possible rejection before evicting a terminal
            // record, incrementing the sequence, or creating the run ticket.
            let next_order = catalog
                .next_order
                .checked_add(1)
                .filter(|v| *v < u64::MAX)
                .ok_or("Delegation job order exhausted")?;
            let eviction_index = if catalog.jobs.len() == MAX_JOBS {
                Some(
                    catalog
                        .jobs
                        .iter()
                        .position(|j| j.status.is_terminal())
                        .ok_or("Delegation retention full; active jobs preserved")?,
                )
            } else {
                None
            };
            let ticket_id = Uuid::new_v4().to_string();
            let job = DelegationJob {
                id: Uuid::new_v4().to_string(),
                order: catalog.next_order,
                provider: provider.to_owned(),
                model: model.to_owned(),
                task: task.to_owned(),
                status: DelegationStatus::Running,
                ticket: Some(ticket_id.clone()),
                answer: None,
                error: None,
            };
            let ticket = DelegationTicket {
                job_id: job.id.clone(),
                ticket_id,
                provider: job.provider.clone(),
                model: job.model.clone(),
                task: job.task.clone(),
            };
            if let Some(index) = eviction_index {
                catalog.jobs.remove(index);
            }
            catalog.next_order = next_order;
            catalog.jobs.push(job.clone());
            Ok(((job, ticket), true))
        })
    }

    pub fn start(&self, job_id: &str, user_initiated: bool) -> Result<DelegationTicket, String> {
        if !user_initiated || !valid_uuid(job_id) {
            return Err("Explicit user action and a valid delegation ID are required".into());
        }
        self.transaction(|catalog| {
            if catalog
                .jobs
                .iter()
                .filter(|j| j.status == DelegationStatus::Running)
                .count()
                >= MAX_RUNNING
            {
                return Err("Maximum two concurrent delegated AI tasks".into());
            }
            let job = catalog
                .jobs
                .iter_mut()
                .find(|j| j.id == job_id)
                .ok_or("Delegation job not found")?;
            if job.status != DelegationStatus::Queued {
                return Err("Only explicitly queued delegations may be started".into());
            }
            let ticket_id = Uuid::new_v4().to_string();
            job.status = DelegationStatus::Running;
            job.ticket = Some(ticket_id.clone());
            let ticket = DelegationTicket {
                job_id: job.id.clone(),
                ticket_id,
                provider: job.provider.clone(),
                model: job.model.clone(),
                task: job.task.clone(),
            };
            Ok((ticket, true))
        })
    }

    pub fn complete(
        &self,
        job_id: &str,
        ticket_id: &str,
        answer: &str,
    ) -> Result<DelegationJob, String> {
        if !valid_text(answer, MAX_ANSWER_BYTES, true) || obvious_credentials(answer) {
            return Err("Provider answer invalid, too large or contains credential markers".into());
        }
        self.finish(job_id, ticket_id, DelegationStatus::Completed, answer)
    }
    pub fn fail(
        &self,
        job_id: &str,
        ticket_id: &str,
        reason: &str,
    ) -> Result<DelegationJob, String> {
        if !valid_text(reason, MAX_REASON_BYTES, true) || obvious_credentials(reason) {
            return Err("Delegation failure reason invalid or sensitive".into());
        }
        self.finish(job_id, ticket_id, DelegationStatus::Failed, reason)
    }
    fn finish(
        &self,
        job_id: &str,
        ticket_id: &str,
        status: DelegationStatus,
        text: &str,
    ) -> Result<DelegationJob, String> {
        if !valid_uuid(job_id) || !valid_uuid(ticket_id) {
            return Err("Invalid delegation completion ID/ticket".into());
        }
        self.transaction(|catalog| {
            let job = catalog
                .jobs
                .iter_mut()
                .find(|j| j.id == job_id)
                .ok_or("Delegation job not found")?;
            if job.status != DelegationStatus::Running || job.ticket.as_deref() != Some(ticket_id) {
                return Err("Delegation completion ticket stale or no longer running".into());
            }
            job.ticket = None;
            job.status = status;
            if status == DelegationStatus::Completed {
                job.answer = Some(text.to_owned());
            } else {
                job.error = Some(text.to_owned());
            }
            Ok((job.clone(), true))
        })
    }

    pub fn cancel(
        &self,
        job_id: &str,
        ticket_id: Option<&str>,
        user_initiated: bool,
    ) -> Result<CancelReceipt, String> {
        if !user_initiated || !valid_uuid(job_id) {
            return Err("Explicit user cancellation and job ID required".into());
        }
        self.transaction(|catalog| {
            let job = catalog
                .jobs
                .iter_mut()
                .find(|j| j.id == job_id)
                .ok_or("Delegation job not found")?;
            let (was_running, cancelled_ticket) = match job.status {
                DelegationStatus::Queued if ticket_id.is_none() => (false, None),
                DelegationStatus::Running
                    if job.ticket.as_deref() == ticket_id && ticket_id.is_some_and(valid_uuid) =>
                {
                    (true, job.ticket.take())
                }
                _ => return Err("Delegation cancellation requires current state/ticket".into()),
            };
            job.status = DelegationStatus::Cancelled;
            let receipt = CancelReceipt {
                job: job.clone(),
                cancelled_ticket,
                was_running,
            };
            Ok((receipt, true))
        })
    }

    pub fn list(&self) -> Result<Vec<DelegationJob>, String> {
        self.transaction(|catalog| Ok((catalog.jobs.iter().rev().cloned().collect(), false)))
    }
    pub fn get(&self, job_id: &str) -> Result<DelegationJob, String> {
        if !valid_uuid(job_id) {
            return Err("Invalid delegation ID".into());
        }
        self.transaction(|catalog| {
            Ok((
                catalog
                    .jobs
                    .iter()
                    .find(|job| job.id == job_id)
                    .cloned()
                    .ok_or("Delegation not found")?,
                false,
            ))
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InitMode {
    Create,
    Open,
    OpenOrCreate,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::Command,
        sync::{Arc, Barrier},
    };

    struct TestHome(PathBuf);
    impl TestHome {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("cos-delegate-{}", Uuid::new_v4()));
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn explicit_provider_call_ticket_and_durable_completion() {
        let dir = TestHome::new();
        assert!(DelegationStore::open(dir.path()).is_err());
        let store = DelegationStore::create(dir.path()).unwrap();
        assert!(DelegationStore::create(dir.path()).is_err());
        assert!(store
            .submit("openai", "gpt-6", "Audit this synthetic plan", false)
            .is_err());
        let job = store
            .submit("openai", "gpt-6", "Audit this synthetic plan", true)
            .unwrap();
        assert_eq!(job.status, DelegationStatus::Queued);
        assert!(job.ticket.is_none());
        assert!(job.answer.is_none());
        assert!(store.start(&job.id, false).is_err());
        let ticket = store.start(&job.id, true).unwrap();
        assert_eq!(ticket.task, job.task);
        assert_eq!(ticket.provider, "openai");
        assert_eq!(ticket.job_id, job.id);
        assert!(valid_uuid(&ticket.ticket_id));
        assert!(store.start(&job.id, true).is_err());
        let done = store
            .complete(
                &job.id,
                &ticket.ticket_id,
                "Real synthetic provider reply 🌿",
            )
            .unwrap();
        assert_eq!(done.status, DelegationStatus::Completed);
        assert_eq!(
            done.answer.as_deref(),
            Some("Real synthetic provider reply 🌿")
        );
        assert!(done.ticket.is_none());
        assert!(store
            .complete(&job.id, &ticket.ticket_id, "replay")
            .is_err());
        assert!(store.fail(&job.id, &ticket.ticket_id, "replay").is_err());
        let reopened = DelegationStore::open(dir.path()).unwrap();
        assert_eq!(reopened.get(&job.id).unwrap(), done);
        assert_eq!(reopened.list().unwrap(), vec![done]);
        assert!(store.get("not-a-uuid").is_err());
    }

    #[test]
    fn atomic_submit_and_start_persists_only_running_and_exact_ticket() {
        let dir = TestHome::new();
        let store = DelegationStore::create(dir.path()).unwrap();
        let initial = read_catalog(dir.path()).unwrap();
        assert_eq!(initial.revision, 0);
        let (job, ticket) = store
            .submit_and_start("openai", "gpt-6", "Explicit synthetic delegate", true)
            .unwrap();
        assert_eq!(job.status, DelegationStatus::Running);
        assert_eq!(job.ticket.as_deref(), Some(ticket.ticket_id.as_str()));
        assert_eq!(job.id, ticket.job_id);
        assert_eq!(job.provider, ticket.provider);
        assert_eq!(job.model, ticket.model);
        assert_eq!(job.task, ticket.task);
        assert!(valid_uuid(&job.id) && valid_uuid(&ticket.ticket_id));
        assert_ne!(job.id, ticket.ticket_id);
        assert!(job.answer.is_none() && job.error.is_none());
        let persisted = read_catalog(dir.path()).unwrap();
        assert_eq!(persisted.revision, 1);
        assert_eq!(persisted.next_order, 2);
        assert_eq!(persisted.jobs, vec![job.clone()]);
        assert!(!persisted
            .jobs
            .iter()
            .any(|j| j.status == DelegationStatus::Queued));

        let finished = store
            .complete(
                &ticket.job_id,
                &ticket.ticket_id,
                "Verified synthetic reply",
            )
            .unwrap();
        assert_eq!(finished.status, DelegationStatus::Completed);
        assert!(store
            .complete(&ticket.job_id, &ticket.ticket_id, "stale completion")
            .is_err());
        assert_eq!(store.get(&job.id).unwrap(), finished);
    }

    #[test]
    fn atomic_submit_and_start_capacity_or_invalid_input_has_zero_disk_mutation() {
        let dir = TestHome::new();
        let store = DelegationStore::create(dir.path()).unwrap();
        let file = dir.path().join(DATA_FILE);
        let initial = fs::read(&file).unwrap();
        for (provider, model, task, initiated) in [
            ("openai", "gpt-6", "synthetic", false),
            ("fake", "gpt-6", "synthetic", true),
            ("openai", "https://unsafe.example", "synthetic", true),
            ("openai", "gpt-6", "", true),
            ("openai", "gpt-6", "Bearer synthetic-secret", true),
        ] {
            assert!(store
                .submit_and_start(provider, model, task, initiated)
                .is_err());
            assert_eq!(fs::read(&file).unwrap(), initial);
        }
        let (a, _) = store
            .submit_and_start("openai", "gpt-6", "First running job", true)
            .unwrap();
        let (b, _) = store
            .submit_and_start("anthropic", "claude-sonnet-4", "Second running job", true)
            .unwrap();
        let before = fs::read(&file).unwrap();
        let state_before = read_catalog(dir.path()).unwrap();
        for n in 0..5 {
            assert!(store
                .submit_and_start("openrouter", "qwen/qwen3", &format!("Denied {n}"), true)
                .is_err());
            assert_eq!(fs::read(&file).unwrap(), before);
        }
        assert_eq!(read_catalog(dir.path()).unwrap(), state_before);
        assert_eq!(state_before.revision, 2);
        assert_eq!(state_before.next_order, 3);
        assert_eq!(state_before.jobs.len(), 2);
        assert_eq!(state_before.jobs[0].id, a.id);
        assert_eq!(state_before.jobs[1].id, b.id);
        assert!(state_before
            .jobs
            .iter()
            .all(|j| j.status == DelegationStatus::Running));
    }

    #[test]
    fn atomic_run_at_retention_limit_evicts_oldest_terminal_not_queued_or_running() {
        let dir = TestHome::new();
        let store = DelegationStore::create(dir.path()).unwrap();
        let mut queued_ids = Vec::new();
        for n in 0..MAX_QUEUED {
            queued_ids.push(
                store
                    .submit("openai", "gpt-6", &format!("Keep queued {n}"), true)
                    .unwrap()
                    .id,
            );
        }
        let mut finished_ids = Vec::new();
        for n in 0..3 {
            let (job, ticket) = store
                .submit_and_start("openai", "gpt-6", &format!("Old terminal {n}"), true)
                .unwrap();
            store
                .complete(&job.id, &ticket.ticket_id, "Confirmed result")
                .unwrap();
            finished_ids.push(job.id);
        }
        let (running, _) = store
            .submit_and_start("anthropic", "claude-sonnet-4", "Keep running", true)
            .unwrap();
        let before = read_catalog(dir.path()).unwrap();
        assert_eq!(before.jobs.len(), MAX_JOBS);
        assert_eq!(
            before
                .jobs
                .iter()
                .filter(|job| job.status == DelegationStatus::Running)
                .count(),
            1
        );
        let (new_job, ticket) = store
            .submit_and_start("openrouter", "synthetic/test", "New immediate run", true)
            .unwrap();
        let after = read_catalog(dir.path()).unwrap();
        assert_eq!(after.jobs.len(), MAX_JOBS);
        assert_eq!(after.revision, before.revision + 1);
        assert_eq!(after.next_order, before.next_order + 1);
        assert!(!after.jobs.iter().any(|j| j.id == finished_ids[0]));
        for id in queued_ids
            .iter()
            .chain(finished_ids.iter().skip(1))
            .chain(std::iter::once(&running.id))
        {
            assert!(after.jobs.iter().any(|j| &j.id == id));
        }
        assert!(after.jobs.iter().any(|j| j.id == new_job.id));
        assert_eq!(new_job.ticket.as_deref(), Some(ticket.ticket_id.as_str()));
        let snapshot = fs::read(dir.path().join(DATA_FILE)).unwrap();
        assert!(store
            .submit_and_start("openai", "gpt-6", "Denied on full active slots", true)
            .is_err());
        assert_eq!(fs::read(dir.path().join(DATA_FILE)).unwrap(), snapshot);
    }

    #[test]
    fn atomic_run_invalid_full_nonterminal_catalog_fails_closed_without_replacement() {
        // This 16-record catalog cannot satisfy the valid 2-running/12-queued
        // invariant; it simulates tampered disk state, NOT a valid app state.
        // There must never be silent eviction or creation to repair it.
        let dir = TestHome::new();
        let store = DelegationStore::create(dir.path()).unwrap();
        let mut tampered = Catalog::default();
        for order in 1..=MAX_JOBS as u64 {
            tampered.jobs.push(DelegationJob {
                id: Uuid::new_v4().to_string(),
                order,
                provider: "openai".into(),
                model: "gpt-6".into(),
                task: format!("Tampered queued {order}"),
                status: DelegationStatus::Queued,
                ticket: None,
                answer: None,
                error: None,
            });
        }
        tampered.next_order = MAX_JOBS as u64 + 1;
        assert!(tampered.validate().is_err());
        let file = dir.path().join(DATA_FILE);
        let content = serde_json::to_vec(&tampered).unwrap();
        fs::write(&file, &content).unwrap();
        assert!(store
            .submit_and_start("openai", "gpt-6", "Must not repair corrupted state", true)
            .is_err());
        assert_eq!(fs::read(file).unwrap(), content);
    }

    #[test]
    fn concurrent_atomic_run_race_allows_exactly_two_without_queued_orphans() {
        let dir = TestHome::new();
        let store = Arc::new(DelegationStore::create(dir.path()).unwrap());
        let count = 12;
        let gate = Arc::new(Barrier::new(count));
        let mut threads = Vec::new();
        for i in 0..count {
            let store = store.clone();
            let gate = gate.clone();
            threads.push(thread::spawn(move || {
                gate.wait();
                store.submit_and_start("openai", "gpt-6", &format!("race-{i}"), true)
            }));
        }
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), MAX_RUNNING);
        assert_eq!(
            results.iter().filter(|r| r.is_err()).count(),
            count - MAX_RUNNING
        );
        let state = read_catalog(dir.path()).unwrap();
        assert_eq!(state.jobs.len(), MAX_RUNNING);
        assert_eq!(state.revision, 2);
        assert_eq!(state.next_order, 3);
        assert_eq!(
            state
                .jobs
                .iter()
                .filter(|j| j.status == DelegationStatus::Queued)
                .count(),
            0
        );
        let successful: Vec<_> = results.into_iter().filter_map(Result::ok).collect();
        for (job, ticket) in successful {
            assert!(state
                .jobs
                .iter()
                .any(|j| j.id == job.id && j.ticket.as_deref() == Some(ticket.ticket_id.as_str())));
        }
    }

    #[test]
    fn abandoned_running_is_interrupted_only_after_reopen_never_auto_replayed() {
        let dir = TestHome::new();
        let store = DelegationStore::open_or_create(dir.path()).unwrap();
        let j = store
            .submit(
                "anthropic",
                "claude-sonnet-4",
                "Describe a synthetic concept",
                true,
            )
            .unwrap();
        let ticket = store.start(&j.id, true).unwrap();
        let on_disk = read_catalog(dir.path()).unwrap();
        assert_eq!(on_disk.jobs[0].status, DelegationStatus::Running);
        let reopened = DelegationStore::open(dir.path()).unwrap();
        let interrupted = reopened.get(&j.id).unwrap();
        assert_eq!(interrupted.status, DelegationStatus::Interrupted);
        assert!(interrupted.ticket.is_none());
        assert!(interrupted.answer.is_none());
        assert!(interrupted
            .error
            .as_deref()
            .unwrap()
            .contains("never auto-replayed"));
        assert!(reopened
            .complete(&j.id, &ticket.ticket_id, "stale provider callback")
            .is_err());
        assert!(reopened
            .fail(&j.id, &ticket.ticket_id, "stale error callback")
            .is_err());
        assert!(reopened.start(&j.id, true).is_err());
        assert!(reopened.cancel(&j.id, None, true).is_err());
        assert_eq!(
            DelegationStore::open(dir.path())
                .unwrap()
                .get(&j.id)
                .unwrap(),
            interrupted
        );
        let fresh = reopened
            .submit(
                "anthropic",
                "claude-sonnet-4",
                "Explicitly new user task",
                true,
            )
            .unwrap();
        assert_ne!(fresh.id, j.id);
        assert_eq!(fresh.status, DelegationStatus::Queued);
    }

    #[test]
    fn cancelling_queued_or_running_requires_exact_proof_and_blocks_late_callbacks() {
        let dir = TestHome::new();
        let store = DelegationStore::create(dir.path()).unwrap();
        let queued = store
            .submit("openrouter", "qwen/qwen3-8b", "First synthetic task", true)
            .unwrap();
        assert!(store.cancel(&queued.id, None, false).is_err());
        assert!(store
            .cancel(&queued.id, Some(&Uuid::new_v4().to_string()), true)
            .is_err());
        let receipt = store.cancel(&queued.id, None, true).unwrap();
        assert!(!receipt.was_running);
        assert!(receipt.cancelled_ticket.is_none());
        assert_eq!(receipt.job.status, DelegationStatus::Cancelled);
        assert!(store.start(&queued.id, true).is_err());

        let active = store
            .submit("openai", "gpt-6", "Second synthetic task", true)
            .unwrap();
        let ticket = store.start(&active.id, true).unwrap();
        assert!(store.cancel(&active.id, None, true).is_err());
        assert!(store
            .cancel(&active.id, Some(&Uuid::new_v4().to_string()), true)
            .is_err());
        let cancelled = store
            .cancel(&active.id, Some(&ticket.ticket_id), true)
            .unwrap();
        assert!(cancelled.was_running);
        assert_eq!(
            cancelled.cancelled_ticket.as_deref(),
            Some(ticket.ticket_id.as_str())
        );
        assert!(cancelled.job.ticket.is_none());
        assert_eq!(cancelled.job.status, DelegationStatus::Cancelled);
        assert!(store
            .complete(&active.id, &ticket.ticket_id, "late reply")
            .is_err());
        assert!(store
            .fail(&active.id, &ticket.ticket_id, "late failure")
            .is_err());
        assert!(store
            .cancel(&active.id, Some(&ticket.ticket_id), true)
            .is_err());
    }

    #[test]
    fn concurrent_run_queue_and_retention_caps_apply_under_lock() {
        let dir = TestHome::new();
        let store = DelegationStore::create(dir.path()).unwrap();
        let a = store.submit("openai", "gpt-6", "A", true).unwrap();
        let b = store.submit("openai", "gpt-6", "B", true).unwrap();
        let c = store.submit("openai", "gpt-6", "C", true).unwrap();
        let t1 = store.start(&a.id, true).unwrap();
        let t2 = store.start(&b.id, true).unwrap();
        assert!(store.start(&c.id, true).is_err());
        for n in 0..11 {
            store
                .submit("openrouter", "synthetic/test", &format!("Queue {n}"), true)
                .unwrap();
        }
        assert_eq!(
            store
                .list()
                .unwrap()
                .iter()
                .filter(|j| j.status == DelegationStatus::Queued)
                .count(),
            MAX_QUEUED
        );
        assert!(store
            .submit("openai", "gpt-6", "Too many queued", true)
            .is_err());

        store.complete(&a.id, &t1.ticket_id, "answer").unwrap();
        store
            .fail(&b.id, &t2.ticket_id, "provider returned an error")
            .unwrap();
        let mut terminal = Vec::new();
        terminal.push(a.id.clone());
        terminal.push(b.id.clone());
        for n in 0..6 {
            let list = store.list().unwrap();
            let next = list
                .iter()
                .find(|j| j.status == DelegationStatus::Queued)
                .unwrap();
            let ticket = store.start(&next.id, true).unwrap();
            store
                .complete(
                    &next.id,
                    &ticket.ticket_id,
                    &format!("synthetic result {n}"),
                )
                .unwrap();
            terminal.push(next.id.clone());
        }
        assert!(store.list().unwrap().len() <= MAX_JOBS);
        // With terminal jobs available, submitting beyond 16 drops ONLY oldest
        // terminal, keeping every queued/running record intact.
        // Six more submissions bring the queued count back to twelve. Any
        // seventh must fail even if old terminal records could be evicted.
        for n in 0..6 {
            store
                .submit(
                    "openai",
                    "gpt-6",
                    &format!("new explicit request {n}"),
                    true,
                )
                .unwrap();
        }
        assert!(store
            .submit("openai", "gpt-6", "queue still full", true)
            .is_err());
        let list = store.list().unwrap();
        assert!(list.len() <= MAX_JOBS);
        assert!(list.iter().any(|j| j.id == c.id));
        assert!(list.iter().all(|j| j.order > 0));
        assert!(list.iter().any(|j| terminal.iter().all(|id| id != &j.id)));
    }

    #[test]
    fn task_result_provider_limits_and_secret_markers_rejected_without_mutation() {
        let dir = TestHome::new();
        let store = DelegationStore::create(dir.path()).unwrap();
        let before = fs::read(dir.path().join(DATA_FILE)).unwrap();
        for (p, m, t) in [
            ("unknown", "test", "valid prompt"),
            ("openai", "", "valid prompt"),
            ("openai", "http://bad host", "valid prompt"),
            ("openai", "https://attacker.example", "valid prompt"),
            ("openai", "gpt-6", ""),
            ("openai", "gpt-6", "x\n".repeat(2_000).as_str()),
            ("openai", "gpt-6", "Bearer secretxyz"),
            ("openai", "gpt-6", "my api_key is private"),
            ("openai", "gpt-6", "password=never-store"),
        ] {
            assert!(store.submit(p, m, t, true).is_err());
        }
        assert_eq!(fs::read(dir.path().join(DATA_FILE)).unwrap(), before);
        let valid = "🌿".repeat(750);
        assert_eq!(valid.len(), MAX_TASK_BYTES);
        let job = store.submit("openai", "gpt-6", &valid, true).unwrap();
        assert!(store
            .submit("openai", "gpt-6", &("🌿".repeat(751)), true)
            .is_err());
        let ticket = store.start(&job.id, true).unwrap();
        for invalid in [
            "x".repeat(MAX_ANSWER_BYTES + 1),
            "Bearer abcdefghijklmnopqrstuvwxyz".to_owned(),
            "sk-proj-should-not-store".to_owned(),
        ] {
            assert!(store
                .complete(&job.id, &ticket.ticket_id, &invalid)
                .is_err());
        }
        assert_eq!(
            store.get(&job.id).unwrap().status,
            DelegationStatus::Running
        );
        let valid_result = "🌿".repeat(MAX_ANSWER_BYTES / 4);
        assert_eq!(valid_result.len(), MAX_ANSWER_BYTES);
        assert!(store
            .complete(&job.id, &ticket.ticket_id, &valid_result)
            .is_ok());
    }

    #[test]
    fn corrupt_disk_state_fails_closed_without_replacement() {
        let dir = TestHome::new();
        let store = DelegationStore::create(dir.path()).unwrap();
        let first = store
            .submit("openai", "gpt-6", "Keep my valid record", true)
            .unwrap();
        let state = dir.path().join(DATA_FILE);
        for bytes in [
            b"{incomplete json".to_vec(),
            b"{\"version\":55,\"revision\":0,\"next_order\":1,\"jobs\":[]}".to_vec(),
            vec![b'x'; MAX_CATALOG_BYTES + 1],
            b"{\"version\":1,\"revision\":0,\"next_order\":1,\"jobs\":[],\"bogus\":true}".to_vec(),
        ] {
            fs::write(&state, &bytes).unwrap();
            assert!(store.list().is_err());
            assert!(store
                .submit("openai", "gpt-6", "Should not overwrite", true)
                .is_err());
            assert!(DelegationStore::open(dir.path()).is_err());
            assert_eq!(fs::read(&state).unwrap(), bytes);
        }
        let _ = first;
    }

    #[test]
    fn path_and_lockfile_replacements_are_never_overwritten() {
        let dir = TestHome::new();
        let store = DelegationStore::create(dir.path()).unwrap();
        let lock = dir.path().join(LOCK_FILE);
        fs::remove_file(&lock).unwrap();
        fs::create_dir(&lock).unwrap();
        assert!(store
            .submit("openai", "gpt-6", "fail closed", true)
            .is_err());
        assert!(lock.is_dir());
        fs::remove_dir(&lock).unwrap();
        let state = dir.path().join(DATA_FILE);
        fs::remove_file(&state).unwrap();
        fs::create_dir(&state).unwrap();
        assert!(DelegationStore::open(dir.path()).is_err());
        assert!(store
            .submit("openai", "gpt-6", "No mutation", true)
            .is_err());
        assert!(state.is_dir());
    }

    #[test]
    fn simultaneous_handles_preserve_each_unique_submission() {
        let dir = TestHome::new();
        let a = Arc::new(DelegationStore::create(dir.path()).unwrap());
        let b = Arc::new(DelegationStore::open(dir.path()).unwrap());
        let barrier = Arc::new(Barrier::new(10));
        let mut threads = Vec::new();
        for n in 0..10 {
            let a = if n % 2 == 0 { a.clone() } else { b.clone() };
            let gate = barrier.clone();
            threads.push(thread::spawn(move || {
                gate.wait();
                a.submit("openai", "gpt-6", &format!("concurrent request {n}"), true)
                    .unwrap()
                    .id
            }));
        }
        let ids: HashSet<_> = threads
            .into_iter()
            .map(|join| join.join().unwrap())
            .collect();
        assert_eq!(ids.len(), 10);
        let list = a.list().unwrap();
        assert_eq!(list.len(), 10);
        assert!(list.iter().all(|j| ids.contains(&j.id)));
        let mut ids = Vec::new();
        let mut tickets = Vec::new();
        for job in list {
            let store = if job.order % 2 == 0 {
                a.as_ref()
            } else {
                b.as_ref()
            };
            ids.push(job.id.clone());
            if tickets.len() < 2 {
                tickets.push(store.start(&job.id, true).unwrap());
            } else {
                assert!(store.start(&job.id, true).is_err());
            }
        }
        assert_eq!(tickets.len(), 2);
        assert!(ids.len() == 10);
    }

    #[test]
    fn process_writer() {
        let Ok(home) = std::env::var("COS_DELEGATION_TEST_CHILD") else {
            return;
        };
        let store = DelegationStore {
            home: PathBuf::from(home),
        };
        for n in 0..4 {
            store
                .submit(
                    "openai",
                    "gpt-6",
                    &format!("child process request {n}"),
                    true,
                )
                .unwrap();
        }
    }

    #[test]
    fn concurrent_processes_share_single_kernel_writer_lock() {
        if std::env::var_os("COS_DELEGATION_TEST_CHILD").is_some() {
            return;
        }
        let dir = TestHome::new();
        let store = DelegationStore::create(dir.path()).unwrap();
        let exe = std::env::current_exe().unwrap();
        let child = Command::new(&exe)
            .args([
                "--exact",
                "mobile_ai_delegation::tests::process_writer",
                "--nocapture",
            ])
            .env("COS_DELEGATION_TEST_CHILD", dir.path())
            .spawn()
            .unwrap();
        for n in 0..4 {
            store
                .submit(
                    "anthropic",
                    "claude-sonnet-4",
                    &format!("parent process request {n}"),
                    true,
                )
                .unwrap();
        }
        let result = child.wait_with_output().unwrap();
        assert!(result.status.success(), "child writer failed: {result:?}");
        let list = store.list().unwrap();
        assert_eq!(list.len(), 8);
        assert_eq!(list.iter().filter(|j| j.provider == "openai").count(), 4);
        assert_eq!(list.iter().filter(|j| j.provider == "anthropic").count(), 4);
    }
}
