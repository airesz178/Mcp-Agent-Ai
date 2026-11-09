//! Durable, metadata-only audit of explicitly approved model MCP calls.
//!
//! The external request may already have side effects if the app loses focus
//! or the provider continuation fails. Recording only the final chat answer
//! would erase that fact. Here we write a bounded pre-dispatch receipt BEFORE
//! tools/call, then advance its state only after real remote evidence.
//! No tool arguments, result bodies, model content, tokens, or server auth
//! details are persisted. This is separate from completed chat history.

use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const FILE: &str = "mcp-tool-audit-v1.json";
const MAX_BYTES: u64 = 64 * 1024;
const MAX_EVENTS: usize = 128;
static GUARD: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Event {
    pub review_id: String,
    pub request_id: String,
    pub session_id: Option<String>,
    pub name: String,
    pub status: String,
    pub at_unix_ms: u64,
    pub approved_by_user: bool,
    pub result_stored: bool,
}

fn valid_event(event: &Event) -> bool {
    Uuid::parse_str(&event.review_id).is_ok()
        && Uuid::parse_str(&event.request_id).is_ok()
        && event
            .session_id
            .as_ref()
            .is_none_or(|id| Uuid::parse_str(id).is_ok())
        && !event.name.is_empty()
        && event.name.len() <= 64
        && event
            .name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
        && matches!(
            event.status.as_str(),
            "approved_dispatched" | "server_confirmed" | "server_reported_error" | "answer_saved"
        )
        && event.at_unix_ms > 0
        && event.approved_by_user
        && !event.result_stored
}

fn read_unlocked(home: &Path) -> Result<Vec<Event>, String> {
    let path = home.join(FILE);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("Cannot inspect MCP tool audit".into()),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_BYTES {
        return Err("MCP tool audit is unsafe or oversized; not modified".into());
    }
    let mut data = Vec::new();
    File::open(path)
        .map_err(|_| "Cannot open MCP audit")?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|_| "Cannot read MCP audit")?;
    if data.len() as u64 > MAX_BYTES {
        return Err("MCP audit exceeds limit".into());
    }
    let events: Vec<Event> = serde_json::from_slice(&data)
        .map_err(|_| "MCP audit is invalid; it was not overwritten")?;
    if events.len() > MAX_EVENTS || events.iter().any(|event| !valid_event(event)) {
        return Err("MCP audit has invalid events; it was not modified".into());
    }
    let mut ids = std::collections::HashSet::new();
    if events.iter().any(|event| !ids.insert(&event.review_id)) {
        return Err("MCP audit has duplicate reviews; it was not modified".into());
    }
    Ok(events)
}

fn write_unlocked(home: &Path, events: &[Event]) -> Result<(), String> {
    if events.len() > MAX_EVENTS {
        return Err("MCP audit is full; no tool was called".into());
    }
    let encoded = serde_json::to_vec(events).map_err(|_| "Cannot encode MCP tool audit")?;
    if encoded.len() as u64 > MAX_BYTES {
        return Err("MCP tool audit storage limit reached".into());
    }
    fs::create_dir_all(home).map_err(|_| "Cannot create private MCP audit folder")?;
    let temp = home.join(format!("mcp-tool-audit-{}.tmp", Uuid::new_v4()));
    {
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|_| "Cannot stage MCP tool audit")?;
        if out
            .write_all(&encoded)
            .and_then(|_| out.sync_all())
            .is_err()
        {
            let _ = fs::remove_file(&temp);
            return Err("Cannot write MCP tool audit".into());
        }
    }
    if fs::rename(&temp, home.join(FILE)).is_err() {
        let _ = fs::remove_file(&temp);
        return Err("Cannot commit MCP tool audit; no existing audit overwritten".into());
    }
    Ok(())
}

pub fn list(home: &Path) -> Result<Vec<Event>, String> {
    let _guard = GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "MCP audit lock unavailable")?;
    read_unlocked(home)
}

/// Persist the explicit approval before trying a potentially mutating call.
pub fn before_dispatch(
    home: &Path,
    review_id: &str,
    request_id: &str,
    session_id: Option<&str>,
    name: &str,
) -> Result<(), String> {
    let event = Event {
        review_id: review_id.to_owned(),
        request_id: request_id.to_owned(),
        session_id: session_id.map(str::to_owned),
        name: name.to_owned(),
        status: "approved_dispatched".into(),
        at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "System clock unavailable")?
            .as_millis() as u64,
        approved_by_user: true,
        result_stored: false,
    };
    if !valid_event(&event) {
        return Err("Unsafe MCP audit metadata".into());
    }
    let _guard = GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "MCP audit lock unavailable")?;
    let mut events = read_unlocked(home)?;
    if events.iter().any(|old| old.review_id == review_id) {
        return Err("MCP review already dispatched; refused to run twice".into());
    }
    events.push(event);
    write_unlocked(home, &events)
}

/// A state transition requires a prior persisted explicit approval; cannot
/// forge approval by writing a terminal status as a new event.
pub fn advance(home: &Path, review_id: &str, next: &str) -> Result<(), String> {
    let _guard = GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "MCP audit lock unavailable")?;
    let mut events = read_unlocked(home)?;
    let event = events
        .iter_mut()
        .find(|e| e.review_id == review_id)
        .ok_or("MCP audit approval receipt missing")?;
    let allowed = matches!(
        (event.status.as_str(), next),
        ("approved_dispatched", "server_confirmed")
            | ("approved_dispatched", "server_reported_error")
            | ("server_confirmed", "answer_saved")
    );
    if !allowed {
        return Err("Invalid or duplicate MCP audit state transition".into());
    }
    event.status = next.to_owned();
    write_unlocked(home, &events)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dir() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("synthetic-mcp-audit-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }
    #[test]
    fn approved_dispatch_is_atomic_bounded_and_does_not_store_sensitive_payloads() {
        let root = dir();
        let review = Uuid::new_v4().to_string();
        let request = Uuid::new_v4().to_string();
        before_dispatch(&root, &review, &request, None, "read_status").unwrap();
        assert!(before_dispatch(&root, &review, &request, None, "read_status").is_err());
        assert!(advance(&root, &review, "answer_saved").is_err());
        advance(&root, &review, "server_confirmed").unwrap();
        advance(&root, &review, "answer_saved").unwrap();
        assert!(advance(&root, &review, "answer_saved").is_err());
        let events = list(&root).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, "answer_saved");
        assert!(!events[0].result_stored);
        let content = fs::read_to_string(root.join(FILE)).unwrap();
        assert!(!content.contains("arguments"));
        assert!(!content.contains("resultBody"));
        assert!(!content.contains("secret_token"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn corrupt_or_symlinked_existing_audit_fails_closed_without_replacing_data() {
        let root = dir();
        let original = b"{corrupt";
        fs::write(root.join(FILE), original).unwrap();
        assert!(before_dispatch(
            &root,
            &Uuid::new_v4().to_string(),
            &Uuid::new_v4().to_string(),
            None,
            "test"
        )
        .is_err());
        assert_eq!(fs::read(root.join(FILE)).unwrap(), original);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn no_fake_approval_or_unsafe_tool_name_can_enter_audit() {
        let root = dir();
        let review = Uuid::new_v4().to_string();
        assert!(before_dispatch(
            &root,
            &review,
            &Uuid::new_v4().to_string(),
            None,
            "bad\nname"
        )
        .is_err());
        assert!(advance(&root, &review, "server_confirmed").is_err());
        assert!(list(&root).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }
}
