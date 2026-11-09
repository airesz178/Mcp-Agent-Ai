//! Explicit user-saved persistent instructions for *native Android* AI turns.
//! Stored privately, fail-closed on corruption, no scripts/tool authority.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs::{self, File, OpenOptions}, io::{self, Write}, path::{Path, PathBuf}};
use uuid::Uuid;

const DATA: &str = "standing-rules-v1.json";
const LOCK: &str = "standing-rules-v1.lock";
const DATA_V2: &str = "standing-rules-v2.json";
const MAX_V2_FILE: u64 = 1_048_576;
const MAX_RULE_SESSIONS: usize = 128;
pub const MAX_BYTES: usize = 4_000;
const MAX_FILE: u64 = 8_192;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandingRules {
    pub enabled: bool,
    pub text: String,
}

fn validate(rules: &StandingRules) -> Result<(), String> {
    if rules.text.len() > MAX_BYTES || rules.text.contains('\0') ||
        rules.text.chars().any(|ch| (ch.is_control() && ch != '\n' && ch != '\t') ||
            matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')) ||
        rules.text.trim() != rules.text ||
        (rules.enabled && rules.text.is_empty()) {
        return Err("Standing rules must be printable, trimmed and no more than 4000 UTF-8 bytes; enabled rules require text".into());
    }
    Ok(())
}

fn check_regular_or_absent(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        _ => Err("Unsafe private standing rules file".into()),
    }
}

fn locked(home: &Path, exclusive: bool) -> Result<File, String> {
    fs::create_dir_all(home).map_err(|_| "Cannot prepare private standing rules directory")?;
    let meta = fs::symlink_metadata(home).map_err(|_| "Cannot inspect standing rules directory")?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("Unsafe standing rules directory".into());
    }
    let path = home.join(LOCK);
    check_regular_or_absent(&path)?;
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
        #[cfg(any(target_os = "android", target_os = "linux"))]
        opts.custom_flags(0o400000); // O_NOFOLLOW
    }
    let file = opts.open(&path).map_err(|_| "Cannot lock standing rules")?;
    check_regular_or_absent(&path)?;
    if exclusive { file.lock() } else { file.lock_shared() }
        .map_err(|_| "Cannot lock standing rules")?;
    Ok(file)
}

fn read_locked(home: &Path) -> Result<StandingRules, String> {
    let path = home.join(DATA);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(StandingRules::default()),
        Err(_) => return Err("Cannot inspect standing rules".into()),
    };
    if !meta.file_type().is_file() || meta.len() > MAX_FILE {
        return Err("Invalid standing rules file; no AI request sent".into());
    }
    let contents = fs::read(path).map_err(|_| "Cannot read standing rules")?;
    if contents.len() as u64 > MAX_FILE { return Err("Standing rules storage oversized".into()); }
    let rules: StandingRules = serde_json::from_slice(&contents)
        .map_err(|_| "Standing rules storage corrupted; no AI request sent")?;
    validate(&rules)?;
    Ok(rules)
}

pub fn load(home: &Path) -> Result<StandingRules, String> {
    let _lock = locked(home, false)?;
    read_locked(home)
}

#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let src: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(src.as_ptr(), dst.as_ptr(), 0x1 | 0x8) } == 0 {
        Err(io::Error::last_os_error())
    } else { Ok(()) }
}

#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

pub fn save(home: &Path, rules: StandingRules) -> Result<StandingRules, String> {
    validate(&rules)?;
    let _lock = locked(home, true)?;
    check_regular_or_absent(&home.join(DATA))?;
    // Existing corrupt data cannot be silently overwritten/cleared.
    read_locked(home)?;
    let temp: PathBuf = home.join(format!("{DATA}.{}.tmp", Uuid::new_v4()));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&temp).map_err(|_| "Cannot create standing rules update")?;
    let result = (|| -> Result<(), String> {
        let bytes = serde_json::to_vec(&rules).map_err(|_| "Cannot encode standing rules")?;
        file.write_all(&bytes).map_err(|_| "Cannot write standing rules")?;
        file.sync_all().map_err(|_| "Cannot sync standing rules")?;
        drop(file);
        atomic_replace(&temp, &home.join(DATA))
            .map_err(|_| "Cannot replace private standing rules")?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&temp); }
    result?;
    Ok(rules)
}

pub fn dispatch(home: &Path, body: &Value) -> Result<Value, String> {
    match body.get("action").and_then(Value::as_str) {
        Some("get") => {
            if body.as_object().is_none_or(|obj| obj.len() != 1) {
                return Err("Invalid standing rules get request".into());
            }
            let rules = load(home)?;
            Ok(json!({"enabled":rules.enabled,"text":rules.text}))
        },
        Some("save") => {
            if body.get("userInitiated") != Some(&Value::Bool(true)) ||
                body.as_object().is_none_or(|obj| obj.len() != 4) {
                return Err("Explicit standing rules save required".into());
            }
            let enabled = body.get("enabled").and_then(Value::as_bool)
                .ok_or("Standing rules enabled flag must be boolean")?;
            let text = body.get("text").and_then(Value::as_str)
                .ok_or("Standing rules text must be a string")?.to_owned();
            let saved = save(home, StandingRules{enabled,text})?;
            Ok(json!({"enabled":saved.enabled,"text":saved.text}))
        },
        _ => Err("Unsupported standing rules action".into()),
    }
}

pub fn instruction(home: &Path) -> Result<Option<String>, String> {
    let saved = load(home)?;
    if !saved.enabled { return Ok(None); }
    Ok(Some(format!(
        "Standing rules saved by the user for every native AI turn. These are instructions, not proof of actual tests or tool permissions. Never claim a security check was completed unless it was actually performed:\n{}",
        saved.text)))
}

// V1 remains untouched on disk. Until an explicit V2 save, even an enabled
// legacy global rule is archived, NOT silently opted into all new chats.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedRules {
    global: StandingRules,
    sessions: BTreeMap<String, StandingRules>,
}

fn canonical_session_id(id: &str) -> Result<(), String> {
    let parsed = Uuid::parse_str(id).map_err(|_| "Invalid standing rules chat session ID")?;
    if parsed.hyphenated().to_string() != id {
        return Err("Standing rules require a canonical chat session ID".into());
    }
    Ok(())
}

fn validate_scoped(saved: &ScopedRules) -> Result<(), String> {
    validate(&saved.global)?;
    if saved.sessions.len() > MAX_RULE_SESSIONS { return Err("Too many saved chat rules".into()); }
    for (id, rules) in &saved.sessions {
        canonical_session_id(id)?;
        validate(rules)?;
    }
    Ok(())
}

fn read_scoped_locked(home: &Path) -> Result<ScopedRules, String> {
    let path = home.join(DATA_V2);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // Read and validate V1 to detect broken legacy storage without
            // ever activating or rewriting its global enabled flag.
            read_locked(home)?;
            return Ok(ScopedRules::default());
        },
        Err(_) => return Err("Cannot inspect scoped standing rules".into()),
    };
    if !meta.file_type().is_file() || meta.len() > MAX_V2_FILE {
        return Err("Invalid scoped standing rules storage; no AI request sent".into());
    }
    let bytes = fs::read(path).map_err(|_| "Cannot read scoped standing rules")?;
    if bytes.len() as u64 > MAX_V2_FILE { return Err("Scoped standing rules storage oversized".into()); }
    let saved: ScopedRules = serde_json::from_slice(&bytes)
        .map_err(|_| "Scoped standing rules corrupted; no AI request sent")?;
    validate_scoped(&saved)?;
    Ok(saved)
}

fn write_scoped_locked(home: &Path, saved: &ScopedRules) -> Result<(), String> {
    validate_scoped(saved)?;
    check_regular_or_absent(&home.join(DATA_V2))?;
    let bytes = serde_json::to_vec(saved).map_err(|_| "Cannot encode scoped rules")?;
    if bytes.len() as u64 > MAX_V2_FILE { return Err("Scoped standing rules storage full".into()); }
    let temp = home.join(format!("{DATA_V2}.{}.tmp", Uuid::new_v4()));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&temp).map_err(|_| "Cannot create scoped rules update")?;
    let result = (|| -> Result<(), String> {
        file.write_all(&bytes).map_err(|_| "Cannot write scoped rules")?;
        file.sync_all().map_err(|_| "Cannot sync scoped rules")?;
        drop(file);
        atomic_replace(&temp, &home.join(DATA_V2))
            .map_err(|_| "Cannot atomically replace scoped standing rules".to_owned())
    })();
    if result.is_err() { let _ = fs::remove_file(temp); }
    result
}

/// The caller must verify session_id against its actual saved conversations.
/// None never reads a session-local instruction.
pub fn instructions_scoped(home: &Path, session_id: Option<&str>) -> Result<Vec<String>, String> {
    if let Some(id) = session_id { canonical_session_id(id)?; }
    let _guard = locked(home, false)?;
    let saved = read_scoped_locked(home)?;
    let mut result = Vec::new();
    // The simplified per-chat UI deliberately retires global rule injection.
    // Preserve old global data on disk for recovery, but NEVER apply it to
    // another chat without a future explicit user-approved migration.
    if let Some(id) = session_id {
        if let Some(local) = saved.sessions.get(id).filter(|rule| rule.enabled) {
            result.push(format!("User-saved standing rules ONLY for this chat session (not tool permission):\n{}", local.text));
        }
    }
    Ok(result)
}

/// A v2 get/save has an exact caller-validated native chat ID (or None).
/// A save updates only one scope; it cannot promote local text into global.
pub fn dispatch_scoped(home: &Path, body: &Value, session_id: Option<&str>) -> Result<Value, String> {
    if let Some(id) = session_id { canonical_session_id(id)?; }
    let map = body.as_object().ok_or("Standing rules request must be an object")?;
    let supplied_id = map.get("sessionId").ok_or("Standing rules sessionId is required")?;
    if !matches!(supplied_id, Value::Null) && supplied_id.as_str() != session_id ||
        (supplied_id.is_null() && session_id.is_some()) {
        return Err("Standing rules chat session binding mismatch".into());
    }
    match map.get("action").and_then(Value::as_str) {
        Some("get") if map.len() == 2 => {
            let _guard = locked(home, false)?;
            let saved = read_scoped_locked(home)?;
            let legacy = if home.join(DATA).exists() {
                let old = read_locked(home)?;
                Some(json!({"text":old.text,"wasEnabled":old.enabled,"inactive":true}))
            } else { None };
            Ok(json!({"sessionId":session_id,"global":saved.global,
                "session":session_id.and_then(|id| saved.sessions.get(id)).cloned().unwrap_or_default(),
                "legacy":legacy}))
        },
        Some("save") if map.len() == 6 && map.get("userInitiated") == Some(&Value::Bool(true)) => {
            let scope = map.get("scope").and_then(Value::as_str).ok_or("Rule scope required")?;
            let enabled = map.get("enabled").and_then(Value::as_bool).ok_or("Rule enabled must be boolean")?;
            let text = map.get("text").and_then(Value::as_str).ok_or("Rule text must be string")?.to_owned();
            let rules = StandingRules {enabled,text};
            validate(&rules)?;
            if scope == "session" && session_id.is_none() {
                return Err("Save a real chat before setting this-session rules".into());
            }
            if !matches!(scope,"session" | "global") { return Err("Invalid standing rules scope".into()); }
            let _guard = locked(home, true)?;
            let mut saved = read_scoped_locked(home)?;
            match scope {
                "global" => saved.global = rules.clone(),
                "session" => {
                    let id = session_id.ok_or("Missing saved chat session")?;
                    if rules == StandingRules::default() { saved.sessions.remove(id); }
                    else { saved.sessions.insert(id.to_owned(),rules.clone()); }
                },
                _ => unreachable!(),
            }
            write_scoped_locked(home, &saved)?;
            Ok(json!({"sessionId":session_id,"scope":scope,"enabled":rules.enabled,"text":rules.text}))
        },
        Some("reset") if map.len() == 3 && map.get("userInitiated") == Some(&Value::Bool(true)) => {
            let id = session_id.ok_or("Reset requires a saved chat session")?;
            let _guard = locked(home, true)?;
            let mut saved = read_scoped_locked(home)?;
            saved.sessions.remove(id);
            write_scoped_locked(home, &saved)?;
            Ok(json!({"sessionId":id,"scope":"session","enabled":false,"text":"","reset":true}))
        },
        _ => Err("Invalid or unauthorized scoped standing rules action".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sandbox() -> PathBuf { std::env::temp_dir().join(format!("mcp-standing-rules-{}",Uuid::new_v4())) }

    #[test]
    fn private_rules_default_disabled_and_persist_only_after_explicit_save() {
        let home=sandbox();
        assert_eq!(load(&home).unwrap(),StandingRules::default());
        assert!(instruction(&home).unwrap().is_none());
        assert!(dispatch(&home,&json!({"action":"save","enabled":true,"text":"Review XSS"})).is_err());
        let receipt=dispatch(&home,&json!({"action":"save","enabled":true,"text":"Check XSS and SQL injection; run tests","userInitiated":true})).unwrap();
        assert_eq!(receipt["enabled"],true);
        assert_eq!(load(&home).unwrap().text,"Check XSS and SQL injection; run tests");
        assert!(instruction(&home).unwrap().unwrap().contains("Check XSS"));
        dispatch(&home,&json!({"action":"save","enabled":false,"text":"Check XSS and SQL injection; run tests","userInitiated":true})).unwrap();
        assert!(instruction(&home).unwrap().is_none());
        let _=fs::remove_dir_all(home);
    }

    #[test]
    fn rejects_invalid_rules_without_overwriting_last_saved() {
        let home=sandbox();
        let original=StandingRules{enabled:true,text:"Verify output".into()};
        save(&home,original.clone()).unwrap();
        for rules in [
            StandingRules{enabled:true,text:String::new()},
            StandingRules{enabled:true,text:"x".repeat(MAX_BYTES+1)},
            StandingRules{enabled:false,text:" leading".into()},
            StandingRules{enabled:true,text:"bad\0rule".into()},
            StandingRules{enabled:true,text:"bad\u{202e}rule".into()},
        ] { assert!(save(&home,rules).is_err()); }
        assert_eq!(load(&home).unwrap(),original);
        let _=fs::remove_dir_all(home);
    }

    #[test]
    fn corruption_is_not_silently_ignored_or_overwritten() {
        let home=sandbox();
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(DATA),"{broken").unwrap();
        assert!(instruction(&home).is_err());
        assert!(save(&home,StandingRules::default()).is_err());
        assert_eq!(fs::read_to_string(home.join(DATA)).unwrap(),"{broken");
        let _=fs::remove_dir_all(home);
    }

    #[test]
    fn rejects_forged_unknown_fields_and_nonboolean_toggle() {
        let home=sandbox();
        for invalid in [
            json!({"action":"save","userInitiated":true,"enabled":"true","text":"a"}),
            json!({"action":"save","userInitiated":true,"enabled":true,"text":"a","extra":1}),
            json!({"action":"get","userInitiated":true}),
        ] { assert!(dispatch(&home,&invalid).is_err()); }
        let _=fs::remove_dir_all(home);
    }

    #[test]
    fn scoped_rules_isolate_chats_and_retire_global_inclusion() {
        let home = sandbox();
        let a=Uuid::new_v4().to_string(); let b=Uuid::new_v4().to_string();
        let get = |id: Option<&str>| json!({"action":"get","sessionId":id});
        let save = |scope:&str, id:Option<&str>, enabled:bool, text:&str| json!({
            "action":"save","sessionId":id,"scope":scope,"enabled":enabled,
            "text":text,"userInitiated":true});
        assert_eq!(dispatch_scoped(&home,&get(Some(&a)),Some(&a)).unwrap()["session"]["enabled"],false);
        dispatch_scoped(&home,&save("session",Some(&a),true,"A PRIVATE"),Some(&a)).unwrap();
        assert!(instructions_scoped(&home,Some(&a)).unwrap().join("\n").contains("A PRIVATE"));
        assert!(!instructions_scoped(&home,Some(&b)).unwrap().join("\n").contains("A PRIVATE"));
        assert!(instructions_scoped(&home,None).unwrap().is_empty());
        assert_eq!(dispatch_scoped(&home,&get(Some(&b)),Some(&b)).unwrap()["session"]["text"],"");
        dispatch_scoped(&home,&save("global",None,true,"FOR ALL"),None).unwrap();
        let a_context=instructions_scoped(&home,Some(&a)).unwrap().join("\n");
        assert!(a_context.contains("A PRIVATE") && !a_context.contains("FOR ALL"));
        let b_context=instructions_scoped(&home,Some(&b)).unwrap().join("\n");
        assert!(!b_context.contains("FOR ALL") && !b_context.contains("A PRIVATE"));
        dispatch_scoped(&home,&save("session",Some(&a),false,"A PRIVATE"),Some(&a)).unwrap();
        assert!(!instructions_scoped(&home,Some(&a)).unwrap().join("\n").contains("A PRIVATE"));
        dispatch_scoped(&home,&save("global",None,false,"FOR ALL"),None).unwrap();
        assert!(instructions_scoped(&home,Some(&a)).unwrap().is_empty());
        let reset = json!({"action":"reset","sessionId":a,"userInitiated":true});
        assert!(dispatch_scoped(&home,&json!({"action":"reset","sessionId":a}),Some(&a)).is_err());
        assert!(dispatch_scoped(&home,&reset,None).is_err());
        assert_eq!(dispatch_scoped(&home,&reset,Some(&a)).unwrap()["reset"],true);
        assert_eq!(dispatch_scoped(&home,&get(Some(&a)),Some(&a)).unwrap()["session"]["text"],"");
        let _=fs::remove_dir_all(home);
    }

    #[test]
    fn scoped_rules_require_exact_scope_and_legacy_remains_unpromoted() {
        let home=sandbox(); let id=Uuid::new_v4().to_string();
        save(&home,StandingRules{enabled:true,text:"LEGACY GLOBAL".into()}).unwrap();
        assert!(instructions_scoped(&home,Some(&id)).unwrap().is_empty());
        let result=dispatch_scoped(&home,&json!({"action":"get","sessionId":null}),None).unwrap();
        assert_eq!(result["legacy"]["text"],"LEGACY GLOBAL");
        assert_eq!(result["legacy"]["inactive"],true);
        assert_eq!(result["global"]["enabled"],false);
        for invalid in [
            json!({"action":"save","sessionId":null,"scope":"session","enabled":true,"text":"bad","userInitiated":true}),
            json!({"action":"save","sessionId":id,"scope":"session","enabled":true,"text":"bad"}),
            json!({"action":"save","sessionId":id,"scope":"global","enabled":true,"text":"bad","userInitiated":true,"extra":42}),
            json!({"action":"save","sessionId":id,"scope":"global","enabled":"on","text":"bad","userInitiated":true}),
            json!({"action":"get","sessionId":"not-a-uuid"}),
        ] { assert!(dispatch_scoped(&home,&invalid,None).is_err()); }
        assert!(dispatch_scoped(&home,&json!({"action":"get","sessionId":id}),Some(&Uuid::new_v4().to_string())).is_err());
        assert!(instructions_scoped(&home,None).unwrap().is_empty());
        assert_eq!(load(&home).unwrap().text,"LEGACY GLOBAL");
        let _=fs::remove_dir_all(home);
    }

    #[test]
    fn scoped_rules_corruption_and_invalid_utf8_are_fail_closed() {
        let home=sandbox();
        let id=Uuid::new_v4().to_string();
        dispatch_scoped(&home,&json!({"action":"save","sessionId":id,"scope":"session",
            "enabled":true,"text":"Keep private","userInitiated":true}),Some(&id)).unwrap();
        let before=fs::read(home.join(DATA_V2)).unwrap();
        assert!(dispatch_scoped(&home,&json!({"action":"save","sessionId":id,"scope":"session",
            "enabled":true,"text":"bad\u{202e}","userInitiated":true}),Some(&id)).is_err());
        assert_eq!(before,fs::read(home.join(DATA_V2)).unwrap());
        fs::write(home.join(DATA_V2),"{bad").unwrap();
        assert!(instructions_scoped(&home,Some(&id)).is_err());
        assert!(dispatch_scoped(&home,&json!({"action":"save","sessionId":null,"scope":"global",
            "enabled":true,"text":"replace","userInitiated":true}),None).is_err());
        assert_eq!(fs::read_to_string(home.join(DATA_V2)).unwrap(),"{bad");
        let _=fs::remove_dir_all(home);
    }
}
