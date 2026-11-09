//! Opt-in GeckoView browser and extension management, independent of the
//! existing Android System WebView / Chrome / ChatGPT browser.
use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

#[cfg(target_os = "android")]
struct GeckoBrowserHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("custom-gecko-browser")
        .setup(|app, api| {
            let plugin =
                api.register_android_plugin("ai.mcpagent.app", "CustomGeckoBrowserPlugin")?;
            app.manage(GeckoBrowserHandle(plugin));
            Ok(())
        })
        .build()
}

#[cfg(target_os = "android")]
fn call(app: AppHandle, action: &str, payload: Value) -> Result<Value, String> {
    let handle = app
        .try_state::<GeckoBrowserHandle<tauri::Wry>>()
        .ok_or("GeckoView plugin not registered")?;
    let value: Value = handle
        .0
        .run_mobile_plugin(action, payload)
        .map_err(|_| "GeckoView operation failed or was declined")?;
    if !value.is_object() || value.to_string().len() > 32_768 {
        return Err("Invalid GeckoView result".into());
    }
    Ok(value)
}

#[cfg(not(target_os = "android"))]
fn call(_app: AppHandle, _action: &str, _payload: Value) -> Result<Value, String> {
    Err("GeckoView only exists on Android".into())
}

fn check_https(url: &str) -> Result<(), String> {
    let url = tauri::Url::parse(url).map_err(|_| "Invalid HTTPS URL")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.as_str().len() > 2048
        || url.port().is_some_and(|port| port != 443)
    {
        return Err("GeckoView requires a plain HTTPS URL".into());
    }
    Ok(())
}

pub fn status(app: AppHandle) -> Result<Value, String> {
    call(app, "status", json!({}))
}
pub fn open(app: AppHandle, url: String, user_initiated: bool) -> Result<Value, String> {
    if !user_initiated {
        return Err("Explicit GeckoView open required".into());
    }
    check_https(&url)?;
    call(app, "open", json!({"url":url,"userInitiated":true}))
}
pub fn show(app: AppHandle) -> Result<Value, String> {
    call(app, "show", json!({}))
}
pub fn close(app: AppHandle) -> Result<Value, String> {
    call(app, "close", json!({}))
}
pub fn list_extensions(app: AppHandle) -> Result<Value, String> {
    let value = call(app, "listExtensions", json!({}))?;
    if !value.get("extensions").is_some_and(|v| {
        v.as_array().is_some_and(|items| {
            items.len() <= 40
                && items.iter().all(|item| {
                    item.get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.len() <= 200)
                        && item.get("enabled").and_then(Value::as_bool).is_some()
                })
        })
    }) {
        return Err("GeckoView returned an invalid extension list".into());
    }
    Ok(value)
}
pub fn install_probe(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    if !user_initiated {
        return Err("Explicit bundled extension review required".into());
    }
    call(app, "installProbe", json!({}))
}
pub fn manage(
    app: AppHandle,
    id: String,
    action: &str,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated || !(3..=200).contains(&id.len()) || id.chars().any(char::is_control) {
        return Err("Invalid or unapproved WebExtension management request".into());
    }
    let method = match action {
        "enable" => "enableExtension",
        "disable" => "disableExtension",
        "remove" => "removeExtension",
        _ => return Err("Unknown WebExtension operation".into()),
    };
    call(app, method, json!({"id":id,"userInitiated":true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_https_browser_addresses() {
        for bad in [
            "http://example.com/",
            "file:///storage/index.html",
            "https://name:secret@example.com/",
            "https://example.com:8443/",
        ] {
            assert!(check_https(bad).is_err(), "{bad}");
        }
        assert!(check_https("https://example.org/").is_ok());
    }
}
