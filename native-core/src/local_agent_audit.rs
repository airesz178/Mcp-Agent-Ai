//! A durable metadata-only checkpoint for approved or user-intent-authorized
//! on-device tool dispatch. These are different authority sources.
//! It is deliberately separate from the remote MCP server audit. A process
//! crash after the attempted native action cannot silently imply no effects.
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    sync::{Mutex, OnceLock},
};
use uuid::Uuid;

const FILE: &str = "android-local-tool-audit-v1.json";
const MAX_EVENTS: usize = 128;
const MAX_BYTES: u64 = 64 * 1024;
static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Event {
    review_id: String,
    request_id: String,
    name: String,
    status: String,
    approved_by_user: bool,
    #[serde(default)]
    authorized_by_user_intent: bool,
}

fn read(home: &Path) -> Result<Vec<Event>, String> {
    let path = home.join(FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("Local agent audit inaccessible".into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_BYTES {
        return Err("Local agent audit is unsafe; existing record preserved".into());
    }
    let bytes = fs::read(path).map_err(|_| "Local agent audit unreadable")?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("Local agent audit exceeds size cap".into());
    }
    let events: Vec<Event> = serde_json::from_slice(&bytes)
        .map_err(|_| "Local agent audit corrupt; existing record preserved")?;
    let mut unique = std::collections::HashSet::new();
    if events.len() > MAX_EVENTS
        || events.iter().any(|e| {
            !unique.insert(e.review_id.as_str())
                || Uuid::parse_str(&e.review_id).is_err()
                || Uuid::parse_str(&e.request_id).is_err()
                || e.name.is_empty()
                || e.name.len() > 64
                || !e
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || !matches!(
                    e.status.as_str(),
                    "approved_dispatched"
                        | "user_intent_dispatched"
                        | "native_confirmed"
                        | "outcome_uncertain"
                        | "answer_saved"
                )
                || !(e.approved_by_user ^ e.authorized_by_user_intent)
        })
    {
        return Err("Local agent audit entries invalid; existing record preserved".into());
    }
    Ok(events)
}

fn write(home: &Path, events: &[Event]) -> Result<(), String> {
    let bytes = serde_json::to_vec(events).map_err(|_| "Local agent audit encode failed")?;
    if events.len() > MAX_EVENTS || bytes.len() as u64 > MAX_BYTES {
        return Err("Local agent audit reached capacity; action refused".into());
    }
    fs::create_dir_all(home).map_err(|_| "Local agent audit folder unavailable")?;
    let temp = home.join(format!("android-local-agent-{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|_| "Local agent audit staging failed")?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "Local agent audit write failed")?;
    drop(file);
    let dest = home.join(FILE);
    if fs::rename(&temp, &dest).is_err() {
        // Windows rename cannot atomically overwrite; fail closed, retaining
        // the existing audit instead of deleting dispatch evidence.
        let _ = fs::remove_file(&temp);
        return Err("Local agent audit update unavailable; no action permitted".into());
    }
    Ok(())
}

pub fn before_dispatch(home: &Path, review: &str, request: &str, name: &str) -> Result<(), String> {
    let _guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "Local agent audit lock unavailable")?;
    let mut events = read(home)?;
    if Uuid::parse_str(review).is_err()
        || Uuid::parse_str(request).is_err()
        || name.len() > 64
        || name.is_empty()
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || events.iter().any(|e| e.review_id == review)
    {
        return Err("Invalid or repeated local agent tool approval").map_err(str::to_string);
    }
    events.push(Event {
        review_id: review.into(),
        request_id: request.into(),
        name: name.into(),
        status: "approved_dispatched".into(),
        approved_by_user: true,
        authorized_by_user_intent: false,
    });
    write(home, &events)
}

/// Direct chat commands and model tools can be authorized by an exact
/// current-user intent without an extra approval UI. Recording this *distinct*
/// form of authority must never masquerade as explicit click approval.
pub fn before_intent_dispatch(
    home: &Path,
    review: &str,
    request: &str,
    name: &str,
) -> Result<(), String> {
    let _guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "Local agent audit lock unavailable")?;
    let mut events = read(home)?;
    if Uuid::parse_str(review).is_err()
        || Uuid::parse_str(request).is_err()
        || name.is_empty()
        || name.len() > 64
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || events
            .iter()
            .any(|e| e.review_id == review || (e.request_id == request && e.name == name))
    {
        return Err("Invalid or repeated user-authored agent intent".into());
    }
    events.push(Event {
        review_id: review.into(),
        request_id: request.into(),
        name: name.into(),
        status: "user_intent_dispatched".into(),
        approved_by_user: false,
        authorized_by_user_intent: true,
    });
    write(home, &events)
}

pub fn advance(home: &Path, review: &str, next: &str) -> Result<(), String> {
    let _guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "Local agent audit lock unavailable")?;
    let mut events = read(home)?;
    let e = events
        .iter_mut()
        .find(|e| e.review_id == review)
        .ok_or("Local agent audit approval missing")?;
    if !matches!(
        (e.status.as_str(), next),
        ("approved_dispatched", "native_confirmed")
            | ("approved_dispatched", "outcome_uncertain")
            | ("user_intent_dispatched", "native_confirmed")
            | ("user_intent_dispatched", "outcome_uncertain")
            | ("native_confirmed", "answer_saved")
    ) {
        return Err("Local agent audit transition invalid".into());
    }
    e.status = next.into();
    write(home, &events)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_audit_requires_prior_dispatch_and_preserves_evidence() {
        let home = std::env::temp_dir().join(format!("local-audit-{}", Uuid::new_v4()));
        let review = Uuid::new_v4().to_string();
        let request = Uuid::new_v4().to_string();
        assert!(advance(&home, &review, "native_confirmed").is_err());
        before_dispatch(&home, &review, &request, "android_app_start").unwrap();
        assert!(before_dispatch(&home, &review, &request, "android_app_start").is_err());
        assert_eq!(read(&home).unwrap()[0].status, "approved_dispatched");
        advance(&home, &review, "native_confirmed").unwrap();
        advance(&home, &review, "answer_saved").unwrap();
        assert!(advance(&home, &review, "answer_saved").is_err());
        let raw = fs::read_to_string(home.join(FILE)).unwrap();
        assert!(!raw.contains("arguments"));
        assert!(!raw.contains("result"));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn user_intent_is_not_falsely_recorded_as_individual_approval() {
        let home = std::env::temp_dir().join(format!("local-intent-audit-{}", Uuid::new_v4()));
        let review = Uuid::new_v4().to_string();
        let request = Uuid::new_v4().to_string();
        before_intent_dispatch(&home, &review, &request, "android_app_start").unwrap();
        assert!(before_intent_dispatch(
            &home,
            &Uuid::new_v4().to_string(),
            &request,
            "android_app_start"
        )
        .is_err());
        let events = read(&home).unwrap();
        assert!(!events[0].approved_by_user);
        assert!(events[0].authorized_by_user_intent);
        assert_eq!(events[0].status, "user_intent_dispatched");
        fs::remove_dir_all(home).unwrap();
    }
}
