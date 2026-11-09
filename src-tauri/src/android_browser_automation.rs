//! App-owned offscreen HTTPS WebView automation. No Chrome process or shared
//! ChatGPT cookies. Actual rendering/progress are measured by native Android,
//! which may suspend background app processes at any time.
use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

#[cfg(target_os = "android")]
struct BackgroundBrowserHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("background-browser-automation")
        .setup(|app, api| {
            let plugin = api.register_android_plugin(
                "ai.mcpagent.app",
                "BackgroundBrowserAutomationPlugin",
            )?;
            app.manage(BackgroundBrowserHandle(plugin));
            Ok(())
        })
        .build()
}

fn check_https(url: &str) -> Result<(), String> {
    if url.len() < 9
        || url.len() > 2048
        || !url.to_ascii_lowercase().starts_with("https://")
        || url.chars().any(char::is_whitespace)
        || url.chars().any(char::is_control)
    {
        return Err("Background browser requires a secure HTTPS URL".into());
    }
    let host = url[8..].split(['/', '#', '?']).next().unwrap_or("");
    if host.is_empty() || host.contains('@') || host.starts_with(':') {
        return Err("Background browser requires a valid HTTPS hostname".into());
    }
    Ok(())
}

fn check_index(snapshot_id: i64, index: i32) -> Result<(), String> {
    if snapshot_id < 1 || !(1..=48).contains(&index) {
        Err("Use a current background browser DOM snapshot and element index".into())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "android")]
fn call(app: AppHandle, command: &str, args: Value) -> Result<Value, String> {
    let handle = app
        .try_state::<BackgroundBrowserHandle<tauri::Wry>>()
        .ok_or("Native background browser plugin unavailable")?;
    handle
        .0
        .run_mobile_plugin(command, args)
        .map_err(|_| "Native background browser action was not confirmed".into())
}

#[cfg(not(target_os = "android"))]
fn call(_app: AppHandle, _command: &str, _args: Value) -> Result<Value, String> {
    Err("Background browser requires Android".into())
}

fn check_status(status: Value) -> Result<Value, String> {
    if !status["supported"].is_boolean()
        || !status["running"].is_boolean()
        || !status["ready"].is_boolean()
        || !status["offscreen"].is_boolean()
        || !status["profileIsolated"].is_boolean()
        || !status["url"].is_string()
        || !status["title"].is_string()
    {
        return Err("Background browser status is incomplete".into());
    }
    if status["running"] == true && status["profileIsolated"] != true {
        return Err("Background browser profile isolation was not confirmed".into());
    }
    if status["mode"] != "headless_webview"
        || status["realtime"] != true
        || status["domControl"] != status["supported"]
        || status["chromeControlled"] != false
        || (status["running"] == true && status["supported"] != true)
        || (status["ready"] == true
            && (status["running"] != true
                || status["offscreen"] != true
                || check_https(status["url"].as_str().unwrap_or("")).is_err()))
        || (status["offscreen"] == true) != (status["running"] == true)
        || (status["running"] != true && status["profileIsolated"] == true)
        || (status["running"] != true && status["ready"] == true)
    {
        return Err("Background browser native capability was not verified".into());
    }
    Ok(status)
}

fn check_external_chrome_status(value: Value) -> Result<Value, String> {
    let bool_field = |key| value.get(key).and_then(Value::as_bool);
    if value["mode"] != "external_chrome"
        || value["packageName"] != "com.android.chrome"
        || bool_field("supported") != Some(true)
        || bool_field("available").is_none()
        || bool_field("foreground").is_none()
        || bool_field("canLaunch").is_none()
        || bool_field("launchOnly") != Some(true)
        || bool_field("domControl") != Some(false)
        || bool_field("automation") != Some(false)
        || bool_field("tabsReadable") != Some(false)
        || bool_field("pageReadyVerified") != Some(false)
        || value["canLaunch"] != (value["available"] == true && value["foreground"] == true)
        || value["reason"] != if value["available"] != true {
            "chrome_launcher_not_resolved"
        } else if value["foreground"] != true {
            "app_not_foreground"
        } else {
            "ready_for_user_initiated_launch"
        }
    {
        return Err("External Chrome launcher capability was not verified".into());
    }
    Ok(value)
}

fn check_external_chrome_dispatch(value: Value) -> Result<Value, String> {
    if value["dispatched"] != true
        || value["mode"] != "external_chrome"
        || value["packageName"] != "com.android.chrome"
        || value["domControl"] != false
        || value["automation"] != false
        || value["pageReadyVerified"] != false
        || value.as_object().is_none_or(|fields| fields.len() != 6)
    {
        return Err("Android did not confirm an explicit Chrome launcher dispatch".into());
    }
    Ok(value)
}

/// Read-only OS check: package-visible official Chrome launcher, not an
/// attached Chrome tab, accessibility session or DOM automation capability.
#[tauri::command]
pub async fn android_external_chrome_status(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        check_external_chrome_status(call(app, "externalChromeStatus", json!({}))?)
    })
    .await
    .map_err(|_| "Chrome launcher status interrupted".to_owned())?
}

/// User-directed MAIN/LAUNCHER dispatch to Chrome stable; no URL argument,
/// generic browser chooser, CDP attachment, page content or cookies.
#[tauri::command]
pub async fn android_external_chrome_open(
    app: AppHandle,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated {
        return Err("Opening Chrome requires an explicit user action".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        check_external_chrome_dispatch(call(app, "openExternalChrome",
            json!({"userInitiated":true}))?)
    })
    .await
    .map_err(|_| "Chrome launcher request interrupted".to_owned())?
}

/// Genuine native capability/readiness receipts for three independent modes.
/// Headless and the in-app browser are separate isolated WebViews. Only the
/// in-app browser is a full-screen native window; a right-side DOM sidebar is
/// NOT implemented by these native plugins. External Chrome is launch-only.
#[tauri::command]
pub async fn android_browser_modes_status(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let headless = check_status(call(app.clone(), "status", json!({}))?)?;
        let in_app = crate::android_private_browser::browser_mode_status(app.clone())?;
        let external_chrome = check_external_chrome_status(
            call(app, "externalChromeStatus", json!({}))?)?;
        Ok(json!({"headless":headless,"inApp":in_app,"externalChrome":external_chrome}))
    })
    .await
    .map_err(|_| "Native browser modes status interrupted".to_owned())?
}

fn check_snapshot(value: Value) -> Result<Value, String> {
    let elements = value["elements"]
        .as_array()
        .ok_or("Invalid background browser DOM elements")?;
    if elements.len() > 48
        || value["snapshotId"].as_i64().unwrap_or(0) < 1
        || value["text"].as_str().is_none_or(|s| s.len() > 12_000)
        || value["title"].as_str().is_none_or(|s| s.len() > 150)
        || value["url"].as_str().is_none_or(|s| check_https(s).is_err())
        || value.to_string().len() > 22_000
    {
        return Err("Background browser DOM snapshot exceeded safety limits".into());
    }
    for (n, item) in elements.iter().enumerate() {
        if item["index"].as_u64() != Some((n + 1) as u64)
            || item["label"].as_str().is_none_or(|s| s.len() > 400)
            || item["tag"].as_str().is_none_or(|s| s.len() > 20)
            || !item["fillable"].is_boolean()
        {
            return Err("Invalid background browser DOM element".into());
        }
    }
    Ok(value)
}

#[tauri::command]
pub async fn android_background_browser_status(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || check_status(call(app, "status", json!({}))?))
        .await
        .map_err(|_| "Background browser status interrupted".to_string())?
}

#[tauri::command]
pub async fn android_background_browser_start(
    app: AppHandle,
    url: String,
    user_initiated: bool,
    chat_authorized: Option<bool>,
) -> Result<Value, String> {
    if !user_initiated {
        return Err("Background browser start requires explicit user approval".into());
    }
    check_https(&url)?;
    tauri::async_runtime::spawn_blocking(move || {
        check_status(call(app, "start", json!({"url":url,"userInitiated":true,
            "chatAuthorized":chat_authorized.unwrap_or(false)}))?)
    })
    .await
    .map_err(|_| "Background browser start interrupted".to_string())?
}

#[tauri::command]
pub async fn android_background_browser_stop(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || check_status(call(app, "stop", json!({}))?))
        .await
        .map_err(|_| "Background browser Stop interrupted".to_string())?
}

#[tauri::command]
pub async fn android_background_browser_snapshot(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || check_snapshot(call(app, "snapshot", json!({}))?))
        .await
        .map_err(|_| "Background browser snapshot interrupted".to_string())?
}

async fn browser_action(app: AppHandle, mode: &'static str, args: Value) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let receipt = call(app, mode, args)?;
        if receipt["ok"] == true && receipt["snapshotConsumed"] == true {
            Ok(receipt)
        } else {
            Err("Background browser did not confirm indexed action".into())
        }
    })
    .await
    .map_err(|_| "Background browser action interrupted".to_string())?
}

#[tauri::command]
pub async fn android_background_browser_click(
    app: AppHandle,
    snapshot_id: i64,
    index: i32,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated { return Err("Indexed browser action requires approval".into()); }
    check_index(snapshot_id, index)?;
    browser_action(app, "click", json!({"snapshotId":snapshot_id,"index":index,"userInitiated":true})).await
}

#[tauri::command]
pub async fn android_background_browser_fill(
    app: AppHandle,
    snapshot_id: i64,
    index: i32,
    value: String,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated || value.len() > 2048 || value.chars().any(char::is_control) {
        return Err("Browser fill requires approved bounded input".into());
    }
    check_index(snapshot_id, index)?;
    browser_action(app, "fill", json!({"snapshotId":snapshot_id,"index":index,"value":value,"userInitiated":true})).await
}

#[tauri::command]
pub async fn android_background_browser_scroll(
    app: AppHandle,
    snapshot_id: i64,
    index: i32,
    delta_y: i32,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated || !(-2000..=2000).contains(&delta_y) {
        return Err("Browser scroll requires approved bounded input".into());
    }
    check_index(snapshot_id, index)?;
    browser_action(app, "scroll", json!({"snapshotId":snapshot_id,"index":index,"deltaY":delta_y,"userInitiated":true})).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chrome_status(available: bool, foreground: bool) -> Value {
        json!({"mode":"external_chrome","packageName":"com.android.chrome",
            "supported":true,"available":available,"foreground":foreground,
            "canLaunch":available && foreground,"launchOnly":true,
            "domControl":false,"automation":false,"tabsReadable":false,
            "pageReadyVerified":false,
            "reason":if !available {"chrome_launcher_not_resolved"}
                else if !foreground {"app_not_foreground"}
                else {"ready_for_user_initiated_launch"}})
    }

    #[test]
    fn chrome_mode_requires_real_explicit_launcher_and_never_advertises_dom() {
        for (available, foreground) in [(false,false),(false,true),(true,false),(true,true)] {
            let sample = chrome_status(available,foreground);
            assert!(check_external_chrome_status(sample.clone()).is_ok());
            assert_eq!(sample["canLaunch"], available && foreground);
            let mut forged = sample.clone();
            forged["domControl"] = json!(true);
            assert!(check_external_chrome_status(forged).is_err());
            let mut forged = sample.clone();
            forged["canLaunch"] = json!(!(available && foreground));
            assert!(check_external_chrome_status(forged).is_err());
            let mut forged = sample;
            forged["packageName"] = json!("com.example.lookalike");
            assert!(check_external_chrome_status(forged).is_err());
        }
        let valid = json!({"dispatched":true,"mode":"external_chrome",
            "packageName":"com.android.chrome","domControl":false,
            "automation":false,"pageReadyVerified":false});
        assert!(check_external_chrome_dispatch(valid.clone()).is_ok());
        for (key, wrong) in [("dispatched",json!(false)),
            ("domControl",json!(true)),("pageReadyVerified",json!(true))] {
            let mut forged = valid.clone();
            forged[key] = wrong;
            assert!(check_external_chrome_dispatch(forged).is_err());
        }
        let mut extra = valid;
        extra["url"] = json!("https://unapproved.example.org/");
        assert!(check_external_chrome_dispatch(extra).is_err(),
            "Launch-only receipt may not smuggle external URL or content");
    }

    #[test]
    fn headless_probe_requires_real_isolation_and_current_page_readiness() {
        let idle = json!({"supported":true,"running":false,"ready":false,
            "offscreen":false,"profileIsolated":false,"url":"","title":"",
            "mode":"headless_webview","realtime":true,"domControl":true,
            "chromeControlled":false});
        assert!(check_status(idle.clone()).is_ok());
        let mut running = idle.clone();
        running["running"] = json!(true);
        running["offscreen"] = json!(true);
        running["profileIsolated"] = json!(true);
        running["url"] = json!("https://example.org/");
        assert!(check_status(running.clone()).is_ok(),
            "Running and still loading must not be described as ready");
        running["ready"] = json!(true);
        assert!(check_status(running.clone()).is_ok());
        running["url"] = json!("file:///private");
        assert!(check_status(running).is_err(),
            "Ready cannot be asserted for an untrusted non-HTTPS page");
        let mut unsupported = idle;
        unsupported["supported"] = json!(false);
        unsupported["running"] = json!(true);
        unsupported["offscreen"] = json!(true);
        unsupported["profileIsolated"] = json!(true);
        assert!(check_status(unsupported).is_err(),
            "Cannot claim live automation on an unsupported WebView provider");
        let unsupported_idle = json!({"supported":false,"running":false,"ready":false,
            "offscreen":false,"profileIsolated":false,"url":"","title":"",
            "mode":"headless_webview","realtime":true,"domControl":false,
            "chromeControlled":false});
        assert!(check_status(unsupported_idle).is_ok(),
            "An unsupported Android WebView must report false capability, not fail or claim DOM");
    }
    #[test]
    fn background_browser_never_accepts_non_https_or_credential_urls() {
        for url in ["http://example.org/", "file:///data/data", "https://user:pw@example.org", "https://"] {
            assert!(check_https(url).is_err());
        }
        assert!(check_https("https://example.org/").is_ok());
    }
    #[test]
    fn snapshots_must_be_bounded_and_identified() {
        let sample = json!({"snapshotId":7,"url":"https://example.org/","title":"Example","text":"Hello","elements":[
            {"index":1,"tag":"a","label":"Link","fillable":false}
        ]});
        assert!(check_snapshot(sample.clone()).is_ok());
        let mut invalid = sample;
        invalid["snapshotId"] = json!(0);
        assert!(check_snapshot(invalid).is_err());
        assert!(check_index(7,1).is_ok());
        assert!(check_index(7,49).is_err());
    }
}
