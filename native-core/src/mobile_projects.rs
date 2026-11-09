//! App-private Android project *labels*; this module never reads or moves chat files.
//!
//! Public API for prime: list(home), create(home,name), rename(home,id,name),
//! remove(home,id), assign_chat(home,chat_uuid,Some(project_id),known_session_ids).
//! Pass actual session UUIDs from the owning session engine, never from arbitrary WebView input.
//! Use None to clear a chat's assignment. All mutations return the updated catalog.
//!
//! Legacy migration contract: the current frontend stores
//! localStorage["mcp-agent-mobile-projects-v1"] = JSON.stringify({projects,assignments}).
//! Call preview_legacy_import(home,raw,known_sessions) for a read-only review, show the
//! original names and assignments to the user, and pass that exact approved preview to
//! import_legacy_after_review. Never auto-import, clear old localStorage, drop existing
//! labels or switch UI source of truth before successful reviewed import. Conflicts,
//! missing sessions and stale previews block import without changing either store.
//! At approval time the bridge must ALSO re-read localStorage and compare its contents with
//! the exact JSON presented for review: this module can only check the JSON it is passed,
//! and cannot observe changes in the WebView behind its back.

use serde::{
    de::{self, MapAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    sync::Mutex,
};
use uuid::Uuid;

const STORAGE_FILE: &str = "mobile-projects-v1.json";
const WRITER_LOCK_FILE: &str = ".mobile-projects-v1.lock";
pub const FRONTEND_LABELS_STORAGE_KEY: &str = "mcp-agent-mobile-projects-v1";
pub const MAX_PROJECTS: usize = 30;
pub const MAX_ASSIGNMENTS: usize = 256;
pub const MAX_NAME_BYTES: usize = 160;
pub const MAX_STORAGE_BYTES: usize = 64 * 1024;
static WRITER_LOCK: Mutex<()> = Mutex::new(());

// A process-local mutex does not protect against an old Activity/process and a
// newly started app process both updating the same catalog. Use a persistent
// *zero-content* lock file backed by OS kernel locks: no stale lock after crash.
// Never delete this file on release: unlinking lockfiles creates split-brain
// when another process still holds an open handle to the previous inode.
struct CatalogWriterLock {
    file: File,
}

impl Drop for CatalogWriterLock {
    fn drop(&mut self) {
        let _ = unlock_file(&self.file);
        // Close releases the lock as well if OS unlock fails.
    }
}

fn acquire_writer_lock(home: &Path) -> Result<CatalogWriterLock, String> {
    ensure_home(home, true)?;
    let path = home.join(WRITER_LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
        Ok(_) => return Err("Project catalog lock path is not a regular file".into()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err("Cannot inspect project catalog lock".into()),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        // Android is Linux-based. Do not follow a malicious pre-existing link.
        #[cfg(any(target_os = "android", target_os = "linux"))]
        options.custom_flags(0o400000); // Linux O_NOFOLLOW
    }
    let file = options
        .open(&path)
        .map_err(|_| "Cannot open project catalog lock")?;
    let meta =
        fs::symlink_metadata(&path).map_err(|_| "Cannot inspect opened project catalog lock")?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err("Project catalog lock was replaced by an unsafe entry".into());
    }
    lock_file(&file).map_err(|_| "Project catalog is being changed by another process")?;
    Ok(CatalogWriterLock { file })
}

#[cfg(unix)]
fn lock_file(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
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
    const LOCK_UN: i32 = 8;
    if unsafe { flock(file.as_raw_fd(), LOCK_UN) } == 0 {
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
            low: u32,
            high: u32,
            bytes_low: u32,
            bytes_high: u32,
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
            low: u32,
            high: u32,
            bytes_low: u32,
            bytes_high: u32,
        ) -> i32;
    }
    if unsafe { UnlockFile(file.as_raw_handle(), 0, 0, 1, 0) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCatalog {
    pub version: u32,
    pub projects: Vec<Project>,
    #[serde(deserialize_with = "unique_assignments")]
    pub assignments: BTreeMap<String, String>, // session UUID -> project ID
}

impl Default for ProjectCatalog {
    fn default() -> Self {
        Self {
            version: 1,
            projects: Vec::new(),
            assignments: BTreeMap::new(),
        }
    }
}

fn unique_assignments<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Unique;
    impl<'de> Visitor<'de> for Unique {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("unique session UUID keys")
        }
        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut entries = BTreeMap::new();
            while let Some((chat, project)) = map.next_entry::<String, String>()? {
                if entries.len() >= MAX_ASSIGNMENTS || entries.insert(chat, project).is_some() {
                    return Err(de::Error::custom(
                        "duplicate or excessive session assignments",
                    ));
                }
            }
            Ok(entries)
        }
    }
    deserializer.deserialize_map(Unique)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyLabels {
    #[serde(default)]
    projects: Vec<Project>,
    #[serde(default, deserialize_with = "unique_assignments")]
    assignments: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyImportPreview {
    pub incoming_projects: Vec<Project>,
    pub incoming_assignments: BTreeMap<String, String>,
    pub projects_to_add: usize,
    pub assignments_to_add: usize,
    pub conflicts: Vec<String>,
    pub unknown_chats: Vec<String>,
    #[serde(skip)]
    merged: ProjectCatalog,
}

fn valid_project_id(id: &str) -> bool {
    // Generated Rust UUID or the existing frontend's project-timestamp-random ID.
    if Uuid::parse_str(id).is_ok() && id.len() == 36 {
        return true;
    }
    id.starts_with("project-")
        && id.len() >= 9
        && id.len() <= 90
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

fn valid_chat_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok_and(|uuid| uuid.hyphenated().to_string() == id)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name == name.trim()
        && name.len() <= MAX_NAME_BYTES
        && name.encode_utf16().count() <= 40
        && !name.chars().any(|c| {
            c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
}

fn validate(c: &ProjectCatalog) -> Result<(), String> {
    if c.version != 1 {
        return Err("Unsupported project catalog version".into());
    }
    if c.projects.len() > MAX_PROJECTS || c.assignments.len() > MAX_ASSIGNMENTS {
        return Err("Project catalog exceeds record limits".into());
    }
    let mut ids = HashSet::new();
    for p in &c.projects {
        if !valid_project_id(&p.id) || !valid_name(&p.name) || !ids.insert(p.id.as_str()) {
            return Err("Invalid or duplicate project label".into());
        }
    }
    for (chat, project) in &c.assignments {
        if !valid_chat_id(chat) || !ids.contains(project.as_str()) {
            return Err("Invalid or orphaned session assignment".into());
        }
    }
    Ok(())
}

fn ensure_home(home: &Path, create: bool) -> Result<bool, String> {
    if !home.is_absolute() {
        return Err("An absolute private app-data directory is required".into());
    }
    if create {
        fs::create_dir_all(home).map_err(|_| "Cannot create project catalog directory")?;
    }
    let info = match fs::symlink_metadata(home) {
        Ok(info) => info,
        Err(e) if !create && e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err("Project catalog directory is inaccessible".into()),
    };
    if info.file_type().is_symlink() || !info.is_dir() {
        return Err("Project catalog directory is not a regular directory".into());
    }
    Ok(true)
}

fn file_exists_safely(home: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(home.join(STORAGE_FILE)) {
        Ok(info) if info.file_type().is_file() && !info.file_type().is_symlink() => Ok(true),
        Ok(_) => Err("Project catalog is not a regular file; existing labels preserved".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Cannot inspect existing project catalog".into()),
    }
}

/// Only reads the dedicated metadata file. Missing means empty; corruption never means empty.
pub fn list(home: &Path) -> Result<ProjectCatalog, String> {
    if !ensure_home(home, false)? || !file_exists_safely(home)? {
        return Ok(ProjectCatalog::default());
    }
    let file = File::open(home.join(STORAGE_FILE)).map_err(|_| "Cannot read project catalog")?;
    if file
        .metadata()
        .map_err(|_| "Cannot inspect project catalog")?
        .len()
        > MAX_STORAGE_BYTES as u64
    {
        return Err("Project catalog too large; existing labels preserved".into());
    }
    let mut data = Vec::new();
    file.take((MAX_STORAGE_BYTES + 1) as u64)
        .read_to_end(&mut data)
        .map_err(|_| "Cannot read project catalog")?;
    if data.len() > MAX_STORAGE_BYTES {
        return Err("Project catalog too large; existing labels preserved".into());
    }
    let parsed: ProjectCatalog = serde_json::from_slice(&data)
        .map_err(|_| "Corrupt project catalog; existing labels preserved".to_string())?;
    validate(&parsed)?;
    Ok(parsed)
}

fn persist(home: &Path, catalog: &ProjectCatalog) -> Result<(), String> {
    validate(catalog)?;
    let data = serde_json::to_vec(catalog).map_err(|_| "Cannot encode project catalog")?;
    if data.len() > MAX_STORAGE_BYTES {
        return Err("Project catalog byte limit exceeded".into());
    }
    ensure_home(home, true)?;
    file_exists_safely(home)?;
    let temp = home.join(format!(".mobile-projects-{}.tmp", Uuid::new_v4()));
    let dest = home.join(STORAGE_FILE);
    let result = (|| -> io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(&data)?;
        f.sync_all()?;
        drop(f);
        atomic_replace(&temp, &dest)?;
        #[cfg(unix)]
        {
            if let Ok(dir) = File::open(home) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
        return Err("Atomic project catalog replacement failed; existing labels preserved".into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    const REPLACE_EXISTING: u32 = 0x1;
    const WRITE_THROUGH: u32 = 0x8;
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), REPLACE_EXISTING | WRITE_THROUGH) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn mutate<F>(home: &Path, change: F) -> Result<ProjectCatalog, String>
where
    F: FnOnce(&mut ProjectCatalog) -> Result<(), String>,
{
    let _guard = WRITER_LOCK
        .lock()
        .map_err(|_| "Project catalog lock unavailable")?;
    let _cross_process_guard = acquire_writer_lock(home)?;
    let mut current = list(home)?;
    change(&mut current)?;
    persist(home, &current)?;
    Ok(current)
}

pub fn create(home: &Path, name: &str) -> Result<ProjectCatalog, String> {
    if !valid_name(name) {
        return Err("Invalid project name (max 40 UTF-16 units, 160 bytes)".into());
    }
    mutate(home, |c| {
        if c.projects.len() >= MAX_PROJECTS {
            return Err("Maximum 30 project labels".into());
        }
        c.projects.push(Project {
            id: Uuid::new_v4().to_string(),
            name: name.to_owned(),
        });
        Ok(())
    })
}

pub fn rename(home: &Path, id: &str, name: &str) -> Result<ProjectCatalog, String> {
    if !valid_project_id(id) || !valid_name(name) {
        return Err("Invalid project ID or name".into());
    }
    mutate(home, |c| {
        c.projects
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or("Project label not found")?
            .name = name.to_owned();
        Ok(())
    })
}

/// Deletes only a grouping label and its assignments, never underlying conversations.
pub fn remove(home: &Path, id: &str) -> Result<ProjectCatalog, String> {
    if !valid_project_id(id) {
        return Err("Invalid project ID".into());
    }
    mutate(home, |c| {
        let index = c
            .projects
            .iter()
            .position(|p| p.id == id)
            .ok_or("Project label not found")?;
        c.projects.remove(index);
        c.assignments.retain(|_, project| project != id);
        Ok(())
    })
}

fn is_known_chat(uuid: &str, known_sessions: &[String]) -> bool {
    valid_chat_id(uuid)
        && known_sessions.len() <= MAX_ASSIGNMENTS
        && known_sessions.iter().any(|known| known == uuid)
}

pub fn assign_chat(
    home: &Path,
    chat_uuid: &str,
    project_id: Option<&str>,
    known_sessions: &[String],
) -> Result<ProjectCatalog, String> {
    if !valid_chat_id(chat_uuid) {
        return Err("Invalid conversation UUID".into());
    }
    if let Some(id) = project_id {
        if !valid_project_id(id) || !is_known_chat(chat_uuid, known_sessions) {
            return Err(
                "Assignment requires an existing conversation and a valid project ID".into(),
            );
        }
    }
    mutate(home, |c| {
        match project_id {
            Some(id) => {
                if !c.projects.iter().any(|p| p.id == id) {
                    return Err("Project label not found".into());
                }
                if c.assignments.len() >= MAX_ASSIGNMENTS && !c.assignments.contains_key(chat_uuid)
                {
                    return Err("Maximum 256 assignments".into());
                }
                c.assignments.insert(chat_uuid.to_owned(), id.to_owned());
            }
            None => {
                c.assignments.remove(chat_uuid);
            }
        }
        Ok(())
    })
}

fn parse_legacy(raw: &str) -> Result<ProjectCatalog, String> {
    if raw.len() > MAX_STORAGE_BYTES {
        return Err("Legacy labels exceed byte limit".into());
    }
    let labels: LegacyLabels = serde_json::from_str(raw)
        .map_err(|_| "Invalid legacy project labels; frontend labels unchanged".to_string())?;
    let catalog = ProjectCatalog {
        version: 1,
        projects: labels.projects,
        assignments: labels.assignments,
    };
    validate(&catalog)?;
    Ok(catalog)
}

fn build_preview(
    current: ProjectCatalog,
    legacy: ProjectCatalog,
    known_sessions: &[String],
) -> Result<LegacyImportPreview, String> {
    let mut merged = current;
    let mut conflicts = Vec::new();
    let mut unknown_chats = Vec::new();
    let mut projects_to_add = 0;
    let mut assignments_to_add = 0;
    for p in &legacy.projects {
        match merged.projects.iter().find(|old| old.id == p.id) {
            Some(old) if old.name != p.name => {
                conflicts.push(format!("Conflicting name for {}", p.id))
            }
            Some(_) => {}
            None => {
                merged.projects.push(p.clone());
                projects_to_add += 1;
            }
        }
    }
    for (chat, project) in &legacy.assignments {
        if !is_known_chat(chat, known_sessions) {
            unknown_chats.push(chat.to_owned());
        }
        match merged.assignments.get(chat) {
            Some(old) if old != project => {
                conflicts.push(format!("Conflicting assignment for {chat}"))
            }
            Some(_) => {}
            None => {
                merged.assignments.insert(chat.clone(), project.clone());
                assignments_to_add += 1;
            }
        }
    }
    validate(&merged)?;
    if serde_json::to_vec(&merged)
        .map_err(|_| "Cannot encode merged labels")?
        .len()
        > MAX_STORAGE_BYTES
    {
        return Err("Merged labels exceed byte limit".into());
    }
    Ok(LegacyImportPreview {
        incoming_projects: legacy.projects,
        incoming_assignments: legacy.assignments,
        projects_to_add,
        assignments_to_add,
        conflicts,
        unknown_chats,
        merged,
    })
}

/// Read-only review; the caller supplies the WebView's old localStorage JSON explicitly.
pub fn preview_legacy_import(
    home: &Path,
    local_storage_json: &str,
    known_sessions: &[String],
) -> Result<LegacyImportPreview, String> {
    build_preview(
        list(home)?,
        parse_legacy(local_storage_json)?,
        known_sessions,
    )
}

/// Requires caller-confirmed review; a stale/conflicting preview can never overwrite labels.
/// This function does not read, erase or modify localStorage or the chat history file.
pub fn import_legacy_after_review(
    home: &Path,
    local_storage_json: &str,
    known_sessions: &[String],
    approved_preview: &LegacyImportPreview,
) -> Result<ProjectCatalog, String> {
    let _guard = WRITER_LOCK
        .lock()
        .map_err(|_| "Project catalog lock unavailable")?;
    let _cross_process_guard = acquire_writer_lock(home)?;
    let current = build_preview(
        list(home)?,
        parse_legacy(local_storage_json)?,
        known_sessions,
    )?;
    if &current != approved_preview {
        return Err("Labels changed after review; preview again".into());
    }
    if !current.conflicts.is_empty() || !current.unknown_chats.is_empty() {
        return Err("Project labels conflict or reference unknown chats; review required".into());
    }
    persist(home, &current.merged)?;
    Ok(current.merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("cos-projects-synthetic-{}", Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn catalog_path(&self) -> PathBuf {
            self.0.join(STORAGE_FILE)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn crud_and_assignment_preserve_unrelated_session_file() {
        let t = Fixture::new();
        assert_eq!(list(t.path()).unwrap(), ProjectCatalog::default());
        let chat = Uuid::new_v4().to_string();
        let other = Uuid::new_v4().to_string();
        fs::write(t.path().join("sessions.json"), b"synthetic sessions").unwrap();
        let first = create(t.path(), "Idea \u{1f4c1}").unwrap();
        let id = first.projects[0].id.clone();
        assert_eq!(
            rename(t.path(), &id, "Renamed").unwrap().projects[0].name,
            "Renamed"
        );
        assert!(assign_chat(t.path(), &other, Some(&id), &[chat.clone()]).is_err());
        let assigned = assign_chat(t.path(), &chat, Some(&id), &[chat.clone()]).unwrap();
        assert_eq!(assigned.assignments[&chat], id);
        assert_eq!(list(t.path()).unwrap(), assigned);
        assert!(assign_chat(t.path(), &chat, None, &[])
            .unwrap()
            .assignments
            .is_empty());
        assign_chat(t.path(), &chat, Some(&id), &[chat.clone()]).unwrap();
        assert!(remove(t.path(), &id).unwrap().assignments.is_empty());
        assert!(list(t.path()).unwrap().projects.is_empty());
        assert_eq!(
            fs::read(t.path().join("sessions.json")).unwrap(),
            b"synthetic sessions"
        );
    }

    #[test]
    fn rejects_bad_names_ids_and_max_records_without_writes() {
        let t = Fixture::new();
        for bad in ["", " name ", "one\nline", "unsafe\u{202e}name"] {
            assert!(create(t.path(), bad).is_err());
        }
        assert!(create(t.path(), &"x".repeat(41)).is_err());
        for i in 0..MAX_PROJECTS {
            create(t.path(), &format!("P{i}")).unwrap();
        }
        let before = fs::read(t.catalog_path()).unwrap();
        assert!(create(t.path(), "P31").is_err());
        assert!(rename(t.path(), "../../sessions.json", "bad").is_err());
        assert!(remove(t.path(), "/root").is_err());
        assert!(assign_chat(t.path(), "../bad", None, &[]).is_err());
        assert_eq!(fs::read(t.catalog_path()).unwrap(), before);
    }

    #[test]
    fn corrupt_duplicate_unsupported_and_oversized_catalogs_fail_closed() {
        let t = Fixture::new();
        let invalid: Vec<Vec<u8>> = vec![
            br#"{"version":1,"projects":[{"id":"project-a","name":"A"},{"id":"project-a","name":"B"}],"assignments":{}}"#.to_vec(),
            br#"{"version":1,"projects":[],"assignments":{"00000000-0000-0000-0000-000000000001":"a","00000000-0000-0000-0000-000000000001":"b"}}"#.to_vec(),
            br#"{"version":12,"projects":[],"assignments":{}}"#.to_vec(),
            br#"{"version":1,"projects":[],"assignments":{},"extra":true}"#.to_vec(),
            b"corrupt".to_vec(),
            vec![b'x'; MAX_STORAGE_BYTES + 1],
        ];
        for bytes in invalid {
            fs::write(t.catalog_path(), &bytes).unwrap();
            assert!(list(t.path()).is_err());
            assert!(create(t.path(), "must fail").is_err());
            assert_eq!(fs::read(t.catalog_path()).unwrap(), bytes);
        }
    }

    #[test]
    fn rejects_directory_and_symlinked_catalog_paths() {
        let t = Fixture::new();
        fs::create_dir(t.catalog_path()).unwrap();
        assert!(list(t.path()).is_err());
        assert!(create(t.path(), "unsafe").is_err());
        fs::remove_dir(t.catalog_path()).unwrap();
        #[cfg(windows)]
        let link =
            std::os::windows::fs::symlink_file(t.path().join("synthetic-target"), t.catalog_path());
        #[cfg(unix)]
        let link = std::os::unix::fs::symlink(t.path().join("synthetic-target"), t.catalog_path());
        if link.is_ok() {
            assert!(list(t.path()).is_err());
            assert!(create(t.path(), "unsafe").is_err());
        }
    }

    #[test]
    fn reviewed_legacy_import_preserves_ids_names_assignments_and_is_idempotent() {
        let t = Fixture::new();
        let chat = Uuid::new_v4().to_string();
        let raw = format!(
            r#"{{"projects":[{{"id":"project-1700000000000-abc123","name":"Old Label"}}],"assignments":{{"{chat}":"project-1700000000000-abc123"}}}}"#
        );
        let review = preview_legacy_import(t.path(), &raw, &[chat.clone()]).unwrap();
        assert_eq!(review.projects_to_add, 1);
        assert_eq!(review.assignments_to_add, 1);
        assert!(!t.catalog_path().exists());
        let imported =
            import_legacy_after_review(t.path(), &raw, &[chat.clone()], &review).unwrap();
        assert_eq!(imported.projects[0].id, "project-1700000000000-abc123");
        assert_eq!(imported.assignments[&chat], imported.projects[0].id);
        let second = preview_legacy_import(t.path(), &raw, &[chat.clone()]).unwrap();
        assert_eq!((second.projects_to_add, second.assignments_to_add), (0, 0));
        assert_eq!(
            import_legacy_after_review(t.path(), &raw, &[chat], &second).unwrap(),
            imported
        );
    }

    #[test]
    fn legacy_conflicts_unknown_chats_and_stale_reviews_do_not_replace_disk() {
        let t = Fixture::new();
        let chat = Uuid::new_v4().to_string();
        let saved = create(t.path(), "Current").unwrap();
        let id = &saved.projects[0].id;
        let raw = format!(
            r#"{{"projects":[{{"id":"{id}","name":"Changed"}}],"assignments":{{"{chat}":"{id}"}}}}"#
        );
        let review = preview_legacy_import(t.path(), &raw, &[]).unwrap();
        assert_eq!(review.conflicts.len(), 1);
        assert_eq!(review.unknown_chats, vec![chat]);
        let before = fs::read(t.catalog_path()).unwrap();
        assert!(import_legacy_after_review(t.path(), &raw, &[], &review).is_err());
        assert_eq!(fs::read(t.catalog_path()).unwrap(), before);
        let valid = r#"{"projects":[{"id":"project-old","name":"Old"}],"assignments":{}}"#;
        let old_preview = preview_legacy_import(t.path(), valid, &[]).unwrap();
        rename(t.path(), id, "Other").unwrap();
        let changed = fs::read(t.catalog_path()).unwrap();
        assert!(import_legacy_after_review(t.path(), valid, &[], &old_preview).is_err());
        assert_eq!(fs::read(t.catalog_path()).unwrap(), changed);
    }

    #[test]
    fn malformed_legacy_import_is_never_truncated_or_persisted() {
        let t = Fixture::new();
        let duplicate = r#"{"projects":[{"id":"project-z","name":"Z"},{"id":"project-z","name":"Y"}],"assignments":{}}"#;
        assert!(preview_legacy_import(t.path(), duplicate, &[]).is_err());
        let oversized = serde_json::json!({
            "projects": (0..31).map(|i| serde_json::json!({"id":format!("project-{i}"),"name":"X"})).collect::<Vec<_>>(),
            "assignments":{}
        }).to_string();
        assert!(preview_legacy_import(t.path(), &oversized, &[]).is_err());
        assert!(!t.catalog_path().exists());
    }

    #[test]
    fn concurrent_create_operations_do_not_lose_labels() {
        let t = Fixture::new();
        std::thread::scope(|scope| {
            for i in 0..16 {
                let home = t.path().to_owned();
                scope.spawn(move || {
                    create(&home, &format!("Synthetic {i}")).unwrap();
                });
            }
        });
        let catalog = list(t.path()).unwrap();
        assert_eq!(catalog.projects.len(), 16);
        let ids: HashSet<_> = catalog.projects.iter().map(|p| &p.id).collect();
        assert_eq!(ids.len(), 16);
        for i in 0..16 {
            assert!(catalog
                .projects
                .iter()
                .any(|p| p.name == format!("Synthetic {i}")));
        }
    }

    #[test]
    fn changed_legacy_snapshot_or_chat_lifecycle_invalidates_review() {
        let t = Fixture::new();
        let chat = Uuid::new_v4().to_string();
        let original = format!(
            r#"{{"projects":[{{"id":"project-legacy","name":"Preserve me"}}],"assignments":{{"{chat}":"project-legacy"}}}}"#
        );
        let reviewed = preview_legacy_import(t.path(), &original, &[chat.clone()]).unwrap();
        let new_local_storage = format!(
            r#"{{"projects":[{{"id":"project-legacy","name":"Updated during review"}}],"assignments":{{"{chat}":"project-legacy"}}}}"#
        );
        assert!(import_legacy_after_review(
            t.path(),
            &new_local_storage,
            &[chat.clone()],
            &reviewed
        )
        .is_err());
        // The same snapshot is also invalid if the session disappeared after review.
        assert!(import_legacy_after_review(t.path(), &original, &[], &reviewed).is_err());
        assert!(!t.catalog_path().exists());
        // Old name and exact ID are retained after explicit re-review and approval.
        let rereview =
            preview_legacy_import(t.path(), &new_local_storage, &[chat.clone()]).unwrap();
        let imported =
            import_legacy_after_review(t.path(), &new_local_storage, &[chat], &rereview).unwrap();
        assert_eq!(imported.projects[0].id, "project-legacy");
        assert_eq!(imported.projects[0].name, "Updated during review");
    }

    #[test]
    fn partial_legacy_labels_are_not_discarded() {
        let t = Fixture::new();
        // The previous frontend tolerates a legacy object without assignments.
        let old_json = r#"{"projects":[{"id":"project-old","name":"Important label"}]}"#;
        let review = preview_legacy_import(t.path(), old_json, &[]).unwrap();
        assert_eq!(review.projects_to_add, 1);
        assert!(review.incoming_assignments.is_empty());
        let saved = import_legacy_after_review(t.path(), old_json, &[], &review).unwrap();
        assert_eq!(saved.projects[0].name, "Important label");
        assert!(saved.assignments.is_empty());
    }

    #[test]
    fn unsafe_cross_process_lock_file_fails_closed_without_touching_catalog() {
        let t = Fixture::new();
        let before = create(t.path(), "Keep").unwrap();
        let bytes = fs::read(t.catalog_path()).unwrap();
        let lock_path = t.path().join(WRITER_LOCK_FILE);
        assert!(lock_path.is_file());
        fs::remove_file(&lock_path).unwrap();
        fs::create_dir(&lock_path).unwrap();
        assert!(rename(t.path(), &before.projects[0].id, "Lost").is_err());
        assert_eq!(fs::read(t.catalog_path()).unwrap(), bytes);
        fs::remove_dir(lock_path).unwrap();
    }

    // Invoked only by the parent synthetic test as a separate OS process.
    #[test]
    fn writer_lock_child() {
        let Ok(dir) = std::env::var("COS_PROJECTS_TEST_CHILD_DIR") else {
            return;
        };
        let mode = std::env::var("COS_PROJECTS_TEST_CHILD_MODE").unwrap();
        let home = Path::new(&dir);
        match mode.as_str() {
            "expect_busy" => {
                let error = create(home, "Should not be saved").unwrap_err();
                assert!(error.contains("another process"), "{error}");
            }
            "write" => {
                let result = create(home, "Created by child").unwrap();
                assert!(result.projects.iter().any(|p| p.name == "Created by child"));
            }
            "crash_owner" => {
                let _guard = acquire_writer_lock(home).unwrap();
                fs::write(home.join(".synthetic-child-held-lock"), b"lock acquired").unwrap();
                // Simulate abrupt app-process termination; Drop will not execute.
                std::process::exit(0);
            }
            _ => panic!("Unknown synthetic child-test mode"),
        }
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn kernel_writer_lock_blocks_other_process_and_recovers_after_crash() {
        use std::process::Command;

        let t = Fixture::new();
        let run_child = |mode: &str| -> std::process::ExitStatus {
            let output = Command::new(std::env::current_exe().unwrap())
                // Substring filter: module names vary between standalone rustc --test
                // and Cargo's nested mobile_projects::tests::... test harness.
                // An exact tests::... filter silently ran zero tests under Cargo.
                .arg("writer_lock_child")
                .arg("--nocapture")
                .env("COS_PROJECTS_TEST_CHILD_DIR", t.path())
                .env("COS_PROJECTS_TEST_CHILD_MODE", mode)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if mode == "crash_owner" {
                // Calling process::exit bypasses libtest's final summary.
                // A marker proves the child test acquired its lock before exiting.
                assert!(t.path().join(".synthetic-child-held-lock").is_file());
                assert!(stdout.contains("running 1 test"));
            } else {
                assert!(
                    stdout.contains("1 passed;") && stdout.contains("writer_lock_child"),
                    "Nested synthetic child test did not run: status={:?}; stdout={stdout}; stderr={stderr}",
                    output.status
                );
            }
            output.status
        };
        // One process holds the lock: another must fail closed, not silently
        // overwrite the original catalog after a stale read.
        let guard = acquire_writer_lock(t.path()).unwrap();
        assert!(run_child("expect_busy").success());
        assert_eq!(list(t.path()).unwrap(), ProjectCatalog::default());
        drop(guard);
        assert!(run_child("write").success());
        assert_eq!(list(t.path()).unwrap().projects.len(), 1);

        // Even termination without RAII Drop releases the OS-level lock.
        assert!(run_child("crash_owner").success());
        create(t.path(), "Created after child terminated").unwrap();
        let catalog = list(t.path()).unwrap();
        assert_eq!(catalog.projects.len(), 2);
        assert!(catalog
            .projects
            .iter()
            .any(|p| p.name == "Created by child"));
        assert!(catalog
            .projects
            .iter()
            .any(|p| p.name == "Created after child terminated"));
    }
}
