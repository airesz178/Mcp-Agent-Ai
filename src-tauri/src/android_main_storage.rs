//! Bounded read-only listing of Android primary shared storage, separately
//! authorized by the platform's All files access grant. SAF remains separate.
use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

const MAX_DEPTH: usize = 16;
const MAX_CHILDREN: usize = 128;
const MAX_NAME_LENGTH: usize = 255;

#[cfg(target_os = "android")]
struct AndroidMainStorage<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("main-storage")
        .setup(|app, api| {
            let plugin = api.register_android_plugin("ai.mcpagent.app", "MainStoragePlugin")?;
            app.manage(AndroidMainStorage(plugin));
            Ok(())
        })
        .build()
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.encode_utf16().count() <= MAX_NAME_LENGTH
        && !matches!(segment, "." | "..")
        && !segment.contains('/')
        && !segment.contains('\\')
        && !segment.chars().any(char::is_control)
}

fn restricted(path: &[String]) -> bool {
    path.first()
        .is_some_and(|first| first.eq_ignore_ascii_case(".android_secure"))
        || (path.len() >= 2
            && path[0].eq_ignore_ascii_case("Android")
            && (path[1].eq_ignore_ascii_case("data") || path[1].eq_ignore_ascii_case("obb")))
}

fn check_path(path: &[String]) -> Result<(), String> {
    if path.len() > MAX_DEPTH || path.iter().any(|part| !valid_segment(part)) || restricted(path) {
        return Err("Invalid Android main-storage path".into());
    }
    Ok(())
}

fn check_root_path(root: &str) -> bool {
    root.len() <= 1024
        && root.starts_with('/')
        && !root.contains('\\')
        && !root.chars().any(char::is_control)
        && root.split('/').skip(1).all(|p| !matches!(p, "." | ".."))
}

fn validate_status(receipt: &Value) -> Result<(), String> {
    let obj = receipt
        .as_object()
        .ok_or("Invalid Android main-storage status")?;
    if obj.len() != 4
        || !obj.get("supported").is_some_and(Value::is_boolean)
        || !obj.get("granted").is_some_and(Value::is_boolean)
        || !obj.get("readable").is_some_and(Value::is_boolean)
    {
        return Err("Invalid Android main-storage status".into());
    }
    let supported = obj["supported"] == true;
    let granted = obj["granted"] == true;
    let readable = obj["readable"] == true;
    let root = obj["rootPath"]
        .as_str()
        .ok_or("Invalid Android main-storage root")?;
    if (!supported && (granted || readable || !root.is_empty()))
        || (!granted && readable)
        || (granted && root.is_empty())
        || (!root.is_empty() && !check_root_path(root))
    {
        return Err("Inconsistent Android main-storage status".into());
    }
    Ok(())
}

fn validate_listing(receipt: &Value, requested: &[String]) -> Result<(), String> {
    check_path(requested)?;
    let obj = receipt
        .as_object()
        .ok_or("Invalid Android main-storage listing")?;
    if obj.len() != 2 {
        return Err("Invalid Android main-storage listing".into());
    }
    let path = obj["path"]
        .as_array()
        .ok_or("Invalid Android main-storage listed path")?;
    if path.len() != requested.len()
        || path
            .iter()
            .zip(requested.iter())
            .any(|(value, expected)| value.as_str() != Some(expected))
    {
        return Err("Android main-storage response belongs to another folder".into());
    }
    let children = obj["entries"]
        .as_array()
        .ok_or("Invalid Android main-storage children")?;
    if children.len() > MAX_CHILDREN {
        return Err("Android main-storage listing exceeds limit".into());
    }
    let mut names = std::collections::HashSet::new();
    for child in children {
        let entry = child
            .as_object()
            .ok_or("Invalid Android main-storage child")?;
        if entry.len() != 3
            || !entry.get("isDirectory").is_some_and(Value::is_boolean)
            || !entry.get("readable").is_some_and(Value::is_boolean)
        {
            return Err("Invalid Android main-storage child".into());
        }
        let name = entry["name"]
            .as_str()
            .ok_or("Invalid Android main-storage child name")?;
        if !valid_segment(name) || !names.insert(name) {
            return Err("Invalid or duplicate Android main-storage child".into());
        }
        let mut child_path = requested.to_vec();
        child_path.push(name.to_owned());
        if restricted(&child_path) && entry["readable"] != false {
            return Err("Protected Android directory cannot be marked readable".into());
        }
    }
    Ok(())
}

/// Current selection, successful explicit selection and reset share one exact
/// receipt contract. Root [] is always unselected; an affirmative receipt
/// must point to a nonempty, validated child path.
fn validate_selection(receipt: &Value, expected: Option<(&[String], bool)>) -> Result<(), String> {
    let obj = receipt
        .as_object()
        .ok_or("Invalid Android main-storage selection")?;
    if obj.len() != 2 || !obj.get("selected").is_some_and(Value::is_boolean) {
        return Err("Invalid Android main-storage selection".into());
    }
    let selected = obj["selected"] == true;
    let values = obj["path"]
        .as_array()
        .ok_or("Invalid Android main-storage selection path")?;
    let path: Vec<String> = values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or("Invalid Android main-storage selection segment")
        })
        .collect::<Result<_, _>>()?;
    check_path(&path)?;
    if selected == path.is_empty() {
        return Err("Android storage selection and path disagree".into());
    }
    if let Some((expected_path, expected_selected)) = expected {
        if selected != expected_selected || path != expected_path {
            return Err("Android main-storage selection did not match the request".into());
        }
    }
    Ok(())
}

pub fn status(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        let plugin = app
            .try_state::<AndroidMainStorage<tauri::Wry>>()
            .ok_or("Android main-storage plugin is not registered")?;
        let receipt: Value = plugin
            .0
            .run_mobile_plugin("status", json!({}))
            .map_err(|_| "Android main-storage status unavailable")?;
        validate_status(&receipt)?;
        return Ok(receipt);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        let receipt = json!({"supported":false,"granted":false,"readable":false,"rootPath":""});
        validate_status(&receipt)?;
        Ok(receipt)
    }
}

pub fn list(app: AppHandle, path: Vec<String>) -> Result<Value, String> {
    check_path(&path)?;
    #[cfg(target_os = "android")]
    {
        let plugin = app
            .try_state::<AndroidMainStorage<tauri::Wry>>()
            .ok_or("Android main-storage plugin is not registered")?;
        let receipt: Value = plugin
            .0
            .run_mobile_plugin("list", json!({"path": &path}))
            .map_err(|_| "Android main-storage listing unavailable")?;
        validate_listing(&receipt, &path)?;
        return Ok(receipt);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, path);
        Err("Android main storage is available only on Android".into())
    }
}

pub fn selection_status(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        let plugin = app
            .try_state::<AndroidMainStorage<tauri::Wry>>()
            .ok_or("Android main-storage plugin is not registered")?;
        let receipt: Value = plugin
            .0
            .run_mobile_plugin("selection", json!({}))
            .map_err(|_| "Android main-storage selection unavailable")?;
        validate_selection(&receipt, None)?;
        return Ok(receipt);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        let receipt = json!({"path":[],"selected":false});
        validate_selection(&receipt, None)?;
        Ok(receipt)
    }
}

pub fn select(app: AppHandle, path: Vec<String>) -> Result<Value, String> {
    check_path(&path)?;
    if path.is_empty() {
        return Err("Primary storage root is the default; use reset".into());
    }
    #[cfg(target_os = "android")]
    {
        let plugin = app
            .try_state::<AndroidMainStorage<tauri::Wry>>()
            .ok_or("Android main-storage plugin is not registered")?;
        let receipt: Value = plugin
            .0
            .run_mobile_plugin("select", json!({"path": &path}))
            .map_err(|_| "Android main-storage folder selection failed")?;
        validate_selection(&receipt, Some((&path, true)))?;
        return Ok(receipt);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, path);
        Err("Android main storage is available only on Android".into())
    }
}

pub fn reset(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        let plugin = app
            .try_state::<AndroidMainStorage<tauri::Wry>>()
            .ok_or("Android main-storage plugin is not registered")?;
        let receipt: Value = plugin
            .0
            .run_mobile_plugin("reset", json!({}))
            .map_err(|_| "Android main-storage folder reset failed")?;
        validate_selection(&receipt, Some((&[], false)))?;
        return Ok(receipt);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err("Android main storage is available only on Android".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn paths_are_bounded_and_cannot_enter_protected_directories() {
        assert!(check_path(&[]).is_ok());
        assert!(check_path(&parts(&["Download", "Notes"])).is_ok());
        for path in [
            parts(&[".."]),
            parts(&["Android", "data"]),
            parts(&["android", "OBB", "other"]),
            parts(&[".android_secure"]),
            parts(&["a/b"]),
            parts(&["x\\y"]),
            parts(&["\u{0000}"]),
            vec!["folder".into(); 17],
            vec!["x".repeat(256)],
        ] {
            assert!(check_path(&path).is_err(), "accepted: {path:?}");
        }
    }

    #[test]
    fn status_rejects_forged_grants_and_bad_roots() {
        for receipt in [
            json!({"supported":false,"granted":false,"readable":false,"rootPath":""}),
            json!({"supported":true,"granted":true,"readable":true,"rootPath":"/storage/emulated/0"}),
            json!({"supported":true,"granted":false,"readable":false,"rootPath":"/storage/emulated/0"}),
        ] {
            assert!(validate_status(&receipt).is_ok());
        }
        for receipt in [
            json!({"supported":false,"granted":true,"readable":false,"rootPath":""}),
            json!({"supported":true,"granted":false,"readable":true,"rootPath":"/storage/emulated/0"}),
            json!({"supported":true,"granted":true,"readable":true,"rootPath":""}),
            json!({"supported":true,"granted":true,"readable":true,"rootPath":"/storage/../data"}),
            json!({"supported":true,"granted":true,"readable":true,"rootPath":"relative"}),
            json!({"supported":true,"granted":true,"readable":true,"rootPath":"/storage","extra":1}),
        ] {
            assert!(validate_status(&receipt).is_err(), "accepted: {receipt}");
        }
    }

    #[test]
    fn listings_validate_path_children_and_protected_read_flags() {
        let path = parts(&["Android"]);
        let valid = json!({"path":path,"entries":[
            {"name":"data","isDirectory":true,"readable":false},
            {"name":"media","isDirectory":true,"readable":true}
        ]});
        assert!(validate_listing(&valid, &path).is_ok());
        for receipt in [
            json!({"path":[],"entries":[]}),
            json!({"path":path,"entries":[{"name":"data","isDirectory":true,"readable":true}]}),
            json!({"path":path,"entries":[{"name":"../data","isDirectory":true,"readable":false}]}),
            json!({"path":path,"entries":[{"name":"media","isDirectory":true,"readable":true},{"name":"media","isDirectory":false,"readable":true}]}),
            json!({"path":path,"entries":[],"truncated":"false"}),
            json!({"path":path,"entries":[],"extra":1}),
            json!({"path":path,"entries":(0..129).map(|n| json!({"name":format!("{n}"),"isDirectory":false,"readable":true})).collect::<Vec<_>>() }),
        ] {
            assert!(
                validate_listing(&receipt, &path).is_err(),
                "accepted: {receipt}"
            );
        }
    }

    #[test]
    fn selection_root_is_unselected_and_child_paths_are_selected() {
        let empty = json!({"path":[],"selected":false});
        let nested = json!({"path":["Download","Projects"],"selected":true});
        let folder = parts(&["Download", "Projects"]);
        assert!(validate_selection(&empty, None).is_ok());
        assert!(validate_selection(&nested, None).is_ok());
        assert!(validate_selection(&nested, Some((&folder, true))).is_ok());
        assert!(validate_selection(&empty, Some((&[], false))).is_ok());
        assert!(validate_selection(&empty, Some((&[], true))).is_err());
        assert!(validate_selection(&nested, Some((&parts(&["Downloads"]), true))).is_err());
    }

    #[test]
    fn selection_receipts_reject_protected_paths_and_forged_fields() {
        for bad in [
            json!({"path":[],"selected":true}),
            json!({"path":["Pictures"],"selected":false}),
            json!({"path":["Android","data"],"selected":true}),
            json!({"path":["ANDROID","ObB"],"selected":true}),
            json!({"path":[".android_secure"],"selected":true}),
            json!({"path":["A",".."],"selected":true}),
            json!({"path":["A/B"],"selected":true}),
            json!({"path":[14],"selected":true}),
            json!({"path":[],"selected":"true"}),
            json!({"path":[],"selected":true,"readable":true}),
            json!({"selected":false}),
            json!({"path":null,"selected":false}),
            json!({"path":vec!["a";17],"selected":true}),
            json!({"path":["😀".repeat(128)],"selected":true}),
        ] {
            assert!(validate_selection(&bad, None).is_err(), "accepted: {bad}");
        }
    }
}
