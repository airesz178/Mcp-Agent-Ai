//! Optional Android 8+ app-scoped Pocket Bot passive overlay.
//! Prime integration: add mod android_pet_overlay; register init() on Android
//! and three pet_overlay_* commands in generate_handler!. Stage the matching
//! android/PetOverlayPlugin.kt into generated Android app sources.
//! Manifest already declares SYSTEM_ALERT_WINDOW. No silent grant or service.

use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

#[cfg(target_os = "android")]
struct PetOverlayHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("pet-overlay")
        .setup(|app, api| {
            let plugin = api.register_android_plugin("ai.mcpagent.app", "PetOverlayPlugin")?;
            app.manage(PetOverlayHandle(plugin));
            Ok(())
        })
        .build()
}

fn explicit(user_initiated: bool) -> Result<(), String> {
    if user_initiated {
        Ok(())
    } else {
        Err("Pocket Bot overlay requires an explicit user action".into())
    }
}

fn verify_common(receipt: &Value) -> Result<(bool, bool), String> {
    let object = receipt
        .as_object()
        .ok_or("Pocket Bot status is not an object")?;
    if object.len() < 7
        || object.len() > 9
        || object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "supported"
                    | "permissionGranted"
                    | "visible"
                    | "foreground"
                    | "pendingConsent"
                    | "sizeDp"
                    | "mode"
                    | "shown"
                    | "cancelled"
                    | "hidden"
            )
        })
        || [
            "supported",
            "permissionGranted",
            "visible",
            "foreground",
            "pendingConsent",
        ]
        .iter()
        .any(|key| receipt.get(*key).and_then(Value::as_bool).is_none())
        || receipt.get("sizeDp").and_then(Value::as_u64) != Some(56)
        || receipt.get("mode").and_then(Value::as_str) != Some("app-scoped")
    {
        return Err("Invalid Pocket Bot state receipt".into());
    }
    let granted = receipt["permissionGranted"] == true;
    let visible = receipt["visible"] == true;
    if (granted && receipt["supported"] != true)
        || (receipt["pendingConsent"] == true && (receipt["supported"] != true || !granted))
        || visible
            && (!granted
                || receipt["supported"] != true
                || receipt["foreground"] != true
                || receipt["pendingConsent"] == true)
    {
        return Err("Pocket Bot cannot report an unauthorized visible overlay".into());
    }
    Ok((granted, visible))
}

fn verify_show(receipt: &Value) -> Result<(), String> {
    let (granted, visible) = verify_common(receipt)?;
    if receipt.as_object().is_none_or(|o| o.len() != 9)
        || receipt.get("shown").and_then(Value::as_bool).is_none()
        || receipt.get("cancelled").and_then(Value::as_bool).is_none()
    {
        return Err("Pocket Bot show receipt is incomplete".into());
    }
    let shown = receipt["shown"] == true;
    let cancelled = receipt["cancelled"] == true;
    if (shown && (!granted || !visible || cancelled)) || (!shown && (!cancelled || visible)) {
        return Err("Pocket Bot show receipt contradicts actual overlay state".into());
    }
    Ok(())
}

fn verify_hide(receipt: &Value) -> Result<(), String> {
    let (_, visible) = verify_common(receipt)?;
    if receipt.as_object().is_none_or(|o| o.len() != 8)
        || receipt.get("hidden") != Some(&Value::Bool(true))
        || visible
        || receipt["pendingConsent"] == true
    {
        return Err("Pocket Bot hide did not confirm removal".into());
    }
    Ok(())
}

#[cfg(target_os = "android")]
fn call(app: AppHandle, command: &str, payload: Value) -> Result<Value, String> {
    let state = app
        .try_state::<PetOverlayHandle<tauri::Wry>>()
        .ok_or_else(|| "Pocket Bot Android plugin not registered".to_string())?;
    state
        .0
        .run_mobile_plugin(command, payload)
        .map_err(|_| "Pocket Bot permission or overlay operation denied".into())
}

#[cfg(not(target_os = "android"))]
fn call(_app: AppHandle, _command: &str, _payload: Value) -> Result<Value, String> {
    Err("Pocket Bot system overlay is available only on Android".into())
}

/// Read-only, actual Android overlay grant/foreground/view attachment status.
/// {supported,permissionGranted,visible,foreground,pendingConsent,sizeDp:56,mode:"app-scoped"}
#[tauri::command]
pub async fn pet_overlay_status(app: AppHandle) -> Result<Value, String> {
    let receipt = tauri::async_runtime::spawn_blocking(move || call(app, "status", json!({})))
        .await
        .map_err(|_| "Pocket Bot status interrupted".to_string())??;
    verify_common(&receipt)?;
    if receipt.as_object().is_none_or(|object| object.len() != 7) {
        return Err("Pocket Bot status returned unexpected action fields".into());
    }
    Ok(receipt)
}

/// Explicit user action AND native Allow-once dialog for every new show.
/// Returns status fields plus shown/cancelled, only claiming shown when View attached.
#[tauri::command]
pub async fn pet_overlay_show(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    explicit(user_initiated)?;
    let receipt = tauri::async_runtime::spawn_blocking(move || {
        call(app, "show", json!({"userInitiated":true}))
    })
    .await
    .map_err(|_| "Pocket Bot show interrupted".to_string())??;
    verify_show(&receipt)?;
    Ok(receipt)
}

/// Explicit user action, remove only this plugin's passive view.
/// Returns status fields plus hidden:true, visible:false.
#[tauri::command]
pub async fn pet_overlay_hide(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    explicit(user_initiated)?;
    let receipt = tauri::async_runtime::spawn_blocking(move || {
        call(app, "hide", json!({"userInitiated":true}))
    })
    .await
    .map_err(|_| "Pocket Bot hide interrupted".to_string())??;
    verify_hide(&receipt)?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Value {
        json!({"supported":true,"permissionGranted":true,"visible":false,
            "foreground":true,"pendingConsent":false,"sizeDp":56,"mode":"app-scoped"})
    }

    #[test]
    fn explicit_actions_only() {
        assert!(explicit(false).is_err());
        assert!(explicit(true).is_ok());
    }

    #[test]
    fn status_receipt_rejects_background_or_ungranted_visible() {
        assert!(verify_common(&base()).is_ok());
        let mut status = base();
        status["visible"] = json!(true);
        assert!(verify_common(&status).is_ok());
        status["permissionGranted"] = json!(false);
        assert!(verify_common(&status).is_err());
        status["permissionGranted"] = json!(true);
        status["foreground"] = json!(false);
        assert!(verify_common(&status).is_err());
        status["foreground"] = json!(true);
        status["pendingConsent"] = json!(true);
        assert!(verify_common(&status).is_err());
        status["visible"] = json!(false);
        status["sizeDp"] = json!(64);
        assert!(verify_common(&status).is_err());
        status["sizeDp"] = json!(56);
        status["mode"] = json!("background");
        assert!(verify_common(&status).is_err());
        let mut impossible = base();
        impossible["supported"] = json!(false);
        assert!(verify_common(&impossible).is_err());
        impossible["permissionGranted"] = json!(false);
        assert!(verify_common(&impossible).is_ok());
        impossible["pendingConsent"] = json!(true);
        assert!(verify_common(&impossible).is_err());
    }

    #[test]
    fn show_receipt_requires_granted_attached_view_or_cancel() {
        let mut shown = base();
        shown["visible"] = json!(true);
        shown["shown"] = json!(true);
        shown["cancelled"] = json!(false);
        assert!(verify_show(&shown).is_ok());
        shown["visible"] = json!(false);
        assert!(verify_show(&shown).is_err());
        shown["visible"] = json!(true);
        shown["permissionGranted"] = json!(false);
        assert!(verify_show(&shown).is_err());
        let mut cancelled = base();
        cancelled["shown"] = json!(false);
        cancelled["cancelled"] = json!(true);
        assert!(verify_show(&cancelled).is_ok());
        cancelled["cancelled"] = json!(false);
        assert!(verify_show(&cancelled).is_err());
        // The old Kotlin completion path could have returned this after
        // attachment was lost: neither shown nor cancelled is a valid receipt.
        cancelled["cancelled"] = json!(false);
        cancelled["shown"] = json!(false);
        assert!(verify_show(&cancelled).is_err());
    }

    #[test]
    fn hide_receipt_requires_not_visible() {
        let mut hidden = base();
        hidden["hidden"] = json!(true);
        assert!(verify_hide(&hidden).is_ok());
        hidden["visible"] = json!(true);
        assert!(verify_hide(&hidden).is_err());
        hidden["visible"] = json!(false);
        hidden["hidden"] = json!(false);
        assert!(verify_hide(&hidden).is_err());
    }
}
