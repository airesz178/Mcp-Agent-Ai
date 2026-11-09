//! Small, non-secret preferences for the Android automation presentation.
//! Choices are intent, NOT proof that the OS has granted execution capability.
//! Actual tool dispatch must still consult the OS and tool permissions.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs, path::PathBuf, sync::Mutex};
use tauri::{AppHandle, State};

const FILE_NAME: &str = "mcp-android-automation-preferences-v1.json";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AppMode {
    Realtime,
    ScreenshotClick,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BrowserMode {
    Headless,
    RightSidebar,
    OpenBrowserApp,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AutomationSelection {
    version: u8,
    pub(crate) android_mode: AppMode,
    pub(crate) browser_mode: BrowserMode,
}

impl Default for AutomationSelection {
    fn default() -> Self {
        Self { version: 1, android_mode: AppMode::Realtime, browser_mode: BrowserMode::Headless }
    }
}

fn validate(saved: AutomationSelection) -> Option<AutomationSelection> {
    (saved.version == 1).then_some(saved)
}

fn parse_app_mode(value: &str) -> Result<AppMode, String> {
    match value {
        "realtime" => Ok(AppMode::Realtime),
        "screenshot_click" => Ok(AppMode::ScreenshotClick),
        _ => Err("Unknown Android automation mode".into()),
    }
}

fn parse_browser_mode(value: &str) -> Result<BrowserMode, String> {
    match value {
        "headless" => Ok(BrowserMode::Headless),
        "right_sidebar" => Ok(BrowserMode::RightSidebar),
        "open_browser_app" => Ok(BrowserMode::OpenBrowserApp),
        _ => Err("Unknown browser automation mode".into()),
    }
}

pub(crate) struct AndroidAutomationPreferences {
    path: PathBuf,
    selected: Mutex<AutomationSelection>,
}

impl AndroidAutomationPreferences {
    pub(crate) fn new(data_dir: PathBuf) -> Self {
        let path = data_dir.join(FILE_NAME);
        let saved = fs::read(&path).ok().filter(|bytes| bytes.len() <= 1024)
            .and_then(|bytes| serde_json::from_slice::<AutomationSelection>(&bytes).ok())
            .and_then(validate).unwrap_or_default();
        Self { path, selected: Mutex::new(saved) }
    }

    pub(crate) fn current(&self) -> Result<AutomationSelection, String> {
        self.selected.lock().map(|guard| guard.clone())
            .map_err(|_| "Automation preferences unavailable".to_string())
    }

    fn update(&self, value: AutomationSelection) -> Result<AutomationSelection, String> {
        let checked = validate(value).ok_or("Unsupported automation preferences version")?;
        let mut guard = self.selected.lock()
            .map_err(|_| "Automation preferences unavailable")?;
        let bytes = serde_json::to_vec(&checked)
            .map_err(|_| "Unable to encode automation preferences")?;
        if bytes.len() > 1024 { return Err("Automation preferences too large".into()); }
        fs::create_dir_all(self.path.parent().ok_or("Automation data directory invalid")?)
            .map_err(|_| "Automation data directory unavailable")?;
        let stage = self.path.with_extension("json.tmp");
        fs::write(&stage, &bytes).map_err(|_| "Unable to save automation preferences")?;
        if let Err(error) = fs::rename(&stage, &self.path) {
            #[cfg(windows)]
            {
                // Win32 rename cannot replace an existing destination. These
                // are non-secret local preferences, not approval decisions.
                fs::remove_file(&self.path).ok();
                fs::rename(&stage, &self.path)
                    .map_err(|_| format!("Unable to save automation preferences: {error}"))?;
            }
            #[cfg(not(windows))]
            {
                fs::remove_file(&stage).ok();
                return Err(format!("Unable to save automation preferences: {error}"));
            }
        }
        *guard = checked.clone();
        Ok(checked)
    }
}

fn as_receipt(value: AutomationSelection) -> Value {
    json!({"version":value.version,"androidMode":value.android_mode,
        "browserMode":value.browser_mode,"saved":true})
}

#[tauri::command]
pub(crate) async fn android_automation_preferences_status(
    prefs: State<'_, AndroidAutomationPreferences>,
) -> Result<Value, String> {
    prefs.current().map(as_receipt)
}

#[tauri::command]
pub(crate) async fn android_automation_preferences_set(
    app: AppHandle,
    prefs: State<'_, AndroidAutomationPreferences>,
    android_mode: String,
    browser_mode: String,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated { return Err("Explicit automation mode selection required".into()); }
    let selected = AutomationSelection {
        version: 1,
        android_mode: parse_app_mode(&android_mode)?,
        browser_mode: parse_browser_mode(&browser_mode)?,
    };
    // Never strand an active owned WebView under a different selected backend.
    // A new mode may be chosen only after the old session is stopped. Chrome
    // is an external app: it is never forcibly closed or inspected here.
    if prefs.current()?.browser_mode != selected.browser_mode {
        #[cfg(target_os = "android")]
        {
            let background = crate::android_browser_automation::android_background_browser_status(
                app.clone()).await?;
            let visible = tauri::async_runtime::spawn_blocking(move ||
                crate::android_private_browser::status(app)
            ).await.map_err(|_| "Owned browser state check interrupted".to_string())??;
            if background["running"] == true || visible["opened"] == true {
                return Err("Stop the current owned browser before changing browser mode".into());
            }
            if !background["running"].is_boolean() || !visible["opened"].is_boolean() {
                return Err("Cannot verify the current owned browser state".into());
            }
        }
        #[cfg(not(target_os = "android"))]
        let _ = &app;
    }
    prefs.update(selected).map(as_receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_values_default_and_restart_roundtrip() {
        let path = std::env::temp_dir().join(format!("mcp-automation-prefs-{}", uuid::Uuid::new_v4()));
        let prefs = AndroidAutomationPreferences::new(path.clone());
        assert_eq!(prefs.current().unwrap(), AutomationSelection::default());
        assert!(parse_app_mode("realtime").is_ok());
        assert!(parse_app_mode("screenshot_click").is_ok());
        assert!(parse_app_mode("camera").is_err());
        assert!(parse_browser_mode("chrome").is_err());
        assert!(parse_browser_mode("right_sidebar").is_ok());
        let selected = AutomationSelection {version:1, android_mode:AppMode::ScreenshotClick,
            browser_mode:BrowserMode::RightSidebar};
        assert_eq!(prefs.update(selected.clone()).unwrap(), selected);
        let restored = AndroidAutomationPreferences::new(path.clone());
        assert_eq!(restored.current().unwrap(), selected);
        fs::write(path.join(FILE_NAME), b"{\"version\":1,\"androidMode\":\"realtime\",\"browserMode\":\"headless\",\"secret\":true}").unwrap();
        assert_eq!(AndroidAutomationPreferences::new(path.clone()).current().unwrap(),
            AutomationSelection::default());
        fs::remove_dir_all(path).ok();
    }
}
