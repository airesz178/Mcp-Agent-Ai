//! User-created, app-private instruction skills. Explicitly enabled by user.
//! No command execution, remote downloads or auto-import.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

const MAX_SKILLS: usize = 16;
const MAX_FILE: u64 = 48 * 1024;
const MAX_INSTRUCTIONS: usize = 2_000;
const SKILLS_FILE: &str = "skills-v1.json";
const LOCK_FILE: &str = "skills-v1.lock";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub instructions: String,
    pub enabled: bool,
}

fn validate(skills: &[Skill]) -> Result<(), String> {
    if skills.len() > MAX_SKILLS {
        return Err("Skill library exceeds limit".into());
    }
    let mut ids = HashSet::with_capacity(skills.len());
    for skill in skills {
        // Compare parsed UUIDs so uppercase or alternate valid representations
        // cannot silently duplicate the same ID. Retain original ID strings.
        let id = Uuid::parse_str(&skill.id)
            .map_err(|_| "Existing skill library has an invalid ID; nothing was changed")?;
        if !ids.insert(id) {
            return Err("Existing skill library has duplicate IDs; nothing was changed".into());
        }
        if skill.name.is_empty()
            || skill.name.trim() != skill.name
            || skill.name.chars().count() > 55
            || skill.name.chars().any(char::is_control)
            || skill.instructions.is_empty()
            || skill.instructions.trim() != skill.instructions
            || skill.instructions.len() > MAX_INSTRUCTIONS
            || skill.instructions.contains('\0')
        {
            return Err("Existing skill library has invalid records; nothing was changed".into());
        }
    }
    Ok(())
}

fn lock_file(home: &Path, exclusive: bool) -> Result<File, String> {
    // Persistent lock inode: do NOT remove it after use, because unlinking a
    // locked inode would let another process lock a different replacement.
    fs::create_dir_all(home).map_err(|_| "Cannot access app skills directory")?;
    let home_meta =
        fs::symlink_metadata(home).map_err(|_| "Cannot inspect app skills directory")?;
    if !home_meta.is_dir() || home_meta.file_type().is_symlink() {
        return Err("Unsafe app skills directory".into());
    }
    let lock_path = home.join(LOCK_FILE);
    match fs::symlink_metadata(&lock_path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
        Ok(_) => return Err("Unsafe app skill lock file".into()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err("Cannot inspect app skill lock file".into()),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        // Android/Linux O_NOFOLLOW rejects swapped symlinks at open time.
        #[cfg(any(target_os = "android", target_os = "linux"))]
        options.custom_flags(0o400000);
    }
    let lock = options
        .open(&lock_path)
        .map_err(|_| "Cannot lock app skill library")?;
    let current =
        fs::symlink_metadata(&lock_path).map_err(|_| "Cannot inspect app skill lock file")?;
    if !current.is_file() || current.file_type().is_symlink() {
        return Err("Unsafe app skill lock file".into());
    }
    if exclusive {
        lock.lock()
    } else {
        lock.lock_shared()
    }
    .map_err(|_| "Cannot lock app skill library")?;
    // OS locks are released with the handle, including after process crashes.
    Ok(lock)
}

/// Caller must hold the shared or exclusive OS lock from `lock_file`.
fn load_locked(home: &Path) -> Result<Vec<Skill>, String> {
    let path = home.join(SKILLS_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(_) => return Err("Cannot read app skills".into()),
    };
    if !metadata.file_type().is_file() {
        return Err("Existing skill library is not a regular file; nothing was changed".into());
    }
    if metadata.len() > MAX_FILE {
        return Err("Skill library exceeds limit".into());
    }
    let bytes = fs::read(path).map_err(|_| "Cannot read app skills")?;
    if bytes.len() > MAX_FILE as usize {
        return Err("Skill library exceeds limit".into());
    }
    let skills: Vec<Skill> = serde_json::from_slice(&bytes)
        .map_err(|_| "Existing skill library is invalid; nothing was changed")?;
    validate(&skills)?;
    Ok(skills)
}

pub fn load(home: &Path) -> Result<Vec<Skill>, String> {
    let _lock = lock_file(home, false)?;
    load_locked(home)
}

// This runs only for the already validated app-private, same-directory
// temporary. MOVEFILE_REPLACE_EXISTING replaces the destination in one rename
// operation rather than fs::copy truncating it (the old Windows behavior).
#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    // If the OS cannot atomically replace (e.g. another process denies DELETE
    // sharing) return an error without touching the previous library.
    if unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        // Only a UUID-named temporary created by this call can be removed.
        let _ = fs::remove_file(&self.0);
    }
}

fn write_atomic<F>(home: &Path, skills: &[Skill], before_replace: F) -> Result<(), String>
where
    F: FnOnce(&Path) -> io::Result<()>,
{
    // Caller holds exclusive lock and has already loaded/validated existing
    // library. A corrupt file must never be replaced by an empty library.
    validate(skills)?;
    let content = serde_json::to_vec(skills).map_err(|_| "Cannot encode skills")?;
    if content.len() > MAX_FILE as usize {
        return Err("Skill library exceeds limit".into());
    }
    let temp_path = home.join(format!("skills-{}.tmp", Uuid::new_v4()));
    let guard = TemporaryFile(temp_path.clone());
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|_| "Cannot save skill library")?;
    file.write_all(&content)
        .and_then(|()| file.sync_all())
        .map_err(|_| "Cannot save skill library")?;
    drop(file);
    before_replace(&guard.0).map_err(|_| "Could not replace app skill library")?;
    atomic_replace(&guard.0, &home.join(SKILLS_FILE))
        .map_err(|_| "Could not replace app skill library")?;

    // POSIX directory sync helps persist the rename itself after power loss.
    // On providers/filesystems that prohibit directory fsync, atomic rename
    // still provides all-or-nothing visibility; do not rewrite as fallback.
    #[cfg(unix)]
    {
        if let Ok(directory) = File::open(home) {
            let _ = directory.sync_all();
        }
    }
    Ok(())
}

pub fn active_instructions(skills: &[Skill]) -> Vec<String> {
    skills
        .iter()
        .filter(|skill| skill.enabled)
        .map(|skill| format!("Skill: {}\n{}", skill.name, skill.instructions))
        .collect()
}

pub fn dispatch(home: &Path, body: &Value) -> Result<Value, String> {
    let action = body["action"].as_str().unwrap_or("");
    if action == "list" {
        return Ok(json!({"skills":load(home)?}));
    }
    if !matches!(action, "add" | "toggle" | "delete") {
        return Err("Unsupported skill action".into());
    }
    // One OS-level transaction spans the load, validation, modification and
    // atomic replacement. Works across processes, not just Rust threads.
    let _lock = lock_file(home, true)?;
    let mut skills = load_locked(home)?;
    match action {
        "add" => {
            if skills.len() >= MAX_SKILLS {
                return Err("Maximum 16 local skills".into());
            }
            let name = body["name"].as_str().unwrap_or("").trim();
            let instructions = body["instructions"].as_str().unwrap_or("").trim();
            if name.is_empty()
                || name.chars().count() > 55
                || name.chars().any(char::is_control)
                || instructions.is_empty()
                || instructions.len() > MAX_INSTRUCTIONS
                || instructions.contains('\0')
            {
                return Err("Skill name or instructions invalid (2,000 bytes max)".into());
            }
            skills.push(Skill {
                id: Uuid::new_v4().to_string(),
                name: name.into(),
                instructions: instructions.into(),
                enabled: false,
            });
        }
        "toggle" | "delete" => {
            let id = body["id"].as_str().ok_or("Skill ID required")?;
            Uuid::parse_str(id).map_err(|_| "Invalid skill ID")?;
            let index = skills
                .iter()
                .position(|s| s.id == id)
                .ok_or("Skill not found")?;
            if action == "delete" {
                skills.remove(index);
            } else {
                skills[index].enabled = body["enabled"].as_bool().ok_or("Skill state required")?;
            }
        }
        _ => unreachable!(),
    }
    write_atomic(home, &skills, |_| Ok(()))?;
    Ok(json!({"skills":skills}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::Command,
        sync::{Arc, Barrier},
        thread,
    };

    struct TestHome(PathBuf);

    impl TestHome {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("mcp-skill-test-{}", Uuid::new_v4())))
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn skills_are_disabled_by_default_and_persist() {
        let dir = TestHome::new();
        let added = dispatch(
            &dir.0,
            &json!({"action":"add","name":"Review code", "instructions":"Explain complexity and edge cases"}),
        )
        .unwrap();
        let id = added["skills"][0]["id"].as_str().unwrap();
        assert_eq!(added["skills"][0]["enabled"], false);
        assert!(active_instructions(&load(&dir.0).unwrap()).is_empty());
        dispatch(&dir.0, &json!({"action":"toggle","id":id,"enabled":true})).unwrap();
        let active = active_instructions(&load(&dir.0).unwrap());
        assert_eq!(active.len(), 1);
        assert!(active[0].contains("Explain complexity"));
        assert!(dispatch(
            &dir.0,
            &json!({"action":"add","name":"x","instructions":"x".repeat(2100)})
        )
        .is_err());
        assert!(dispatch(&dir.0, &json!({"action":"delete","id":"../"})).is_err());
        dispatch(&dir.0, &json!({"action":"delete","id":id})).unwrap();
        assert!(load(&dir.0).unwrap().is_empty());
    }

    #[test]
    fn rejects_corruption_duplicates_and_bad_ids_without_mutation() {
        let dir = TestHome::new();
        fs::create_dir_all(&dir.0).unwrap();
        let id = Uuid::new_v4().to_string();
        let good =
            json!({"id":id,"name":"Safe","instructions":"Keep original content","enabled":true});
        let samples = vec![
            b"[{partial".to_vec(),
            serde_json::to_vec(&json!([good.clone(), good.clone()])).unwrap(),
            serde_json::to_vec(&json!([good.clone(), {"id":id.to_uppercase(), "name":"Different", "instructions":"duplicate", "enabled":false}])).unwrap(),
            serde_json::to_vec(&json!([{"id":"invalid", "name":"Safe", "instructions":"hello", "enabled":false}])).unwrap(),
            serde_json::to_vec(&json!([{"id":Uuid::new_v4().to_string(),"name":"n".repeat(56), "instructions":"hello","enabled":false}])).unwrap(),
            serde_json::to_vec(&json!([{"id":Uuid::new_v4().to_string(),"name":"Safe", "instructions":"a".repeat(2001),"enabled":false}])).unwrap(),
            serde_json::to_vec(&json!([{"id":Uuid::new_v4().to_string(),"name":"Safe", "instructions":"hello","enabled":false,"unexpected":"field"}])).unwrap(),
        ];
        for corrupt in samples {
            fs::write(dir.0.join(SKILLS_FILE), &corrupt).unwrap();
            assert!(load(&dir.0).is_err());
            assert!(dispatch(
                &dir.0,
                &json!({"action":"add","name":"x","instructions":"x"})
            )
            .is_err());
            assert_eq!(fs::read(dir.0.join(SKILLS_FILE)).unwrap(), corrupt);
        }
    }

    #[test]
    fn refuses_unsafe_lock_path_without_touching_existing_skills() {
        let dir = TestHome::new();
        fs::create_dir_all(&dir.0).unwrap();
        let existing = b"[]";
        fs::write(dir.0.join(SKILLS_FILE), existing).unwrap();
        fs::create_dir(dir.0.join(LOCK_FILE)).unwrap();
        assert!(load(&dir.0).is_err());
        assert!(dispatch(
            &dir.0,
            &json!({"action":"add","name":"x","instructions":"x"})
        )
        .is_err());
        assert_eq!(fs::read(dir.0.join(SKILLS_FILE)).unwrap(), existing);
    }

    #[test]
    fn failed_replacement_keeps_complete_original_and_removes_temp() {
        let dir = TestHome::new();
        let result = dispatch(
            &dir.0,
            &json!({"action":"add","name":"Original","instructions":"keep me"}),
        )
        .unwrap();
        let original = fs::read(dir.0.join(SKILLS_FILE)).unwrap();
        let mut modified: Vec<Skill> = serde_json::from_value(result["skills"].clone()).unwrap();
        modified[0].name = "Replacement".into();
        let _lock = lock_file(&dir.0, true).unwrap();
        assert!(write_atomic(&dir.0, &modified, |_| Err(io::Error::other(
            "synthetic rename failure"
        )))
        .is_err());
        assert_eq!(fs::read(dir.0.join(SKILLS_FILE)).unwrap(), original);
        let temp_count = fs::read_dir(&dir.0)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(temp_count, 0);
    }

    #[test]
    fn preserves_valid_legacy_ids_and_schema_when_toggling() {
        let dir = TestHome::new();
        fs::create_dir_all(&dir.0).unwrap();
        let legacy_id = Uuid::new_v4().to_string().to_uppercase();
        let other_id = Uuid::new_v4().to_string();
        let original = vec![
            Skill {
                id: legacy_id.clone(),
                name: "Existing skill".into(),
                instructions: "Keep these exact instructions".into(),
                enabled: false,
            },
            Skill {
                id: other_id.clone(),
                name: "Second skill".into(),
                instructions: "Original preserved".into(),
                enabled: true,
            },
        ];
        fs::write(
            dir.0.join(SKILLS_FILE),
            serde_json::to_vec(&original).unwrap(),
        )
        .unwrap();
        let returned = dispatch(
            &dir.0,
            &json!({"action":"toggle","id":legacy_id,"enabled":true}),
        )
        .unwrap();
        assert_eq!(returned["skills"][0]["id"], legacy_id);
        let saved = load(&dir.0).unwrap();
        assert_eq!(saved.len(), original.len());
        assert_eq!(saved[0].id, original[0].id);
        assert_eq!(saved[0].name, original[0].name);
        assert_eq!(saved[0].instructions, original[0].instructions);
        assert!(saved[0].enabled);
        assert_eq!(saved[1].id, other_id);
        assert_eq!(saved[1].instructions, original[1].instructions);
        assert!(saved[1].enabled);
    }

    // An explicit, synthetic test-harness subprocess entry point. Normal
    // `cargo test` runs this without writing anything (no environment var).
    #[test]
    fn cross_process_add_worker() {
        let Some(home) = std::env::var_os("MCP_SYNTHETIC_SKILLS_CHILD_HOME") else {
            return;
        };
        let prefix = std::env::var("MCP_SYNTHETIC_SKILLS_CHILD_ID").unwrap();
        let path = PathBuf::from(home);
        for index in 0..4 {
            dispatch(
                &path,
                &json!({"action":"add","name":format!("{prefix}-{index}"),"instructions":"safe synthetic content"}),
            )
            .unwrap();
        }
    }

    #[test]
    fn concurrent_processes_share_one_kernel_lock() {
        let dir = TestHome::new();
        let executable = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for index in 0..4 {
            let child = Command::new(&executable)
                .args(["--exact", "mobile_skills::tests::cross_process_add_worker"])
                .env("MCP_SYNTHETIC_SKILLS_CHILD_HOME", &dir.0)
                .env("MCP_SYNTHETIC_SKILLS_CHILD_ID", index.to_string())
                .spawn()
                .unwrap();
            children.push(child);
        }
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        let saved = load(&dir.0).unwrap();
        assert_eq!(saved.len(), MAX_SKILLS);
        assert!(saved.iter().all(|s| !s.enabled));
        let unique_ids: HashSet<_> = saved.iter().map(|s| s.id.as_str()).collect();
        let unique_names: HashSet<_> = saved.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(unique_ids.len(), MAX_SKILLS);
        assert_eq!(unique_names.len(), MAX_SKILLS);
    }

    #[test]
    fn concurrent_mutations_keep_every_unique_id_and_never_enable_adds() {
        let dir = TestHome::new();
        let threads = 16;
        let gate = Arc::new(Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|index| {
                let dir = dir.0.clone();
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    gate.wait();
                    dispatch(&dir, &json!({"action":"add","name":format!("Skill {index}"),"instructions":"synthetic"}))
                })
            })
            .collect();
        for handle in handles {
            assert!(handle.join().unwrap().is_ok());
        }
        let saved = load(&dir.0).unwrap();
        assert_eq!(saved.len(), MAX_SKILLS);
        assert!(saved.iter().all(|s| !s.enabled));
        let names: HashSet<_> = saved.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names.len(), MAX_SKILLS);
        assert!(dispatch(
            &dir.0,
            &json!({"action":"add","name":"extra","instructions":"x"})
        )
        .is_err());
    }
}
