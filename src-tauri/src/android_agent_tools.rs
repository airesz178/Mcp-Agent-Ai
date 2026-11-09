//! AppHandle adapter for ONE explicitly approved local Android model tool.
//! Provider output is only an untrusted proposal. The native-core coordinator
//! enforces a separate user approval and this adapter dispatches exactly one
//! fixed native command, passing no Tauri handle or OAuth credential to AI.
use mcp_agent_core::{AndroidLocalToolHost, Engine};
use serde_json::Value;
use serde_json::json;
use std::{future::Future, pin::Pin, sync::Arc};
use tauri::{AppHandle, Manager};
use uuid::Uuid;
use crate::android_automation_preferences::{AndroidAutomationPreferences, AppMode, BrowserMode};

pub struct Host {
    app: AppHandle,
}

impl Host {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }

    fn selected_browser_mode(&self) -> Result<BrowserMode, String> {
        self.app.try_state::<AndroidAutomationPreferences>()
            .ok_or("Native automation preferences unavailable")?
            .current().map(|selection| selection.browser_mode)
    }

    fn selected_app_mode(&self) -> Result<String, String> {
        let mode = self.app.try_state::<AndroidAutomationPreferences>()
            .ok_or("Native automation preferences unavailable")?
            .current()?.android_mode;
        Ok(match mode {
            AppMode::Realtime => "realtime",
            AppMode::ScreenshotClick => "screenshot_click",
        }.to_owned())
    }

    async fn approved_app_context(&self) -> Result<(String, String), String> {
        let state = crate::android_accessibility::accessibility_automation_status(
            self.app.clone()).await?;
        if state["connected"] != true || state["active"] != true ||
            state["visible"] != true || state["stopRequested"] != false {
            return Err("No foreground, OS-connected, approved Android app session".into());
        }
        let session = state["sessionId"].as_str()
            .filter(|s| (8..=128).contains(&s.len()))
            .ok_or("Approved Android app session ID unavailable")?;
        let target = state["targetPackage"].as_str()
            .filter(|s| (3..=160).contains(&s.len()) && s.contains('.'))
            .ok_or("Approved app target unavailable")?;
        Ok((session.to_owned(),target.to_owned()))
    }

    async fn private_browser_status(&self) -> Result<Value, String> {
        let app = self.app.clone();
        let response = tauri::async_runtime::spawn_blocking(move ||
            crate::android_private_browser::browser_mode_status(app)
        ).await.map_err(|_| "Owned browser status was interrupted".to_string())??;
        normalized_private_status(&response)
    }

    async fn private_tab(&self) -> Result<i32, String> {
        let app = self.app.clone();
        let status = tauri::async_runtime::spawn_blocking(move ||
            crate::android_private_browser::browser_mode_status(app)
        ).await.map_err(|_| "Owned browser status was interrupted".to_string())??;
        if status["domControl"] != true {
            return Err("Owned browser DOM control requires visible Android session approval".into());
        }
        i32::try_from(status["activeTabId"].as_i64()
            .filter(|id| *id > 0).ok_or("No current owned browser tab")?)
            .map_err(|_| "Owned browser tab ID invalid".into())
    }
}

fn normalized_private_status(status:&Value) -> Result<Value,String> {
    let supported=status["profileIsolationSupported"].as_bool()
        .ok_or("Owned browser support unverified")?;
    let running=status["opened"].as_bool()
        .ok_or("Owned browser state unverified")?;
    let isolated=status["isolatedProfile"].as_bool()
        .ok_or("Owned browser isolation unverified")?;
    let ready=status["domControl"].as_bool()
        .ok_or("Owned browser DOM permission unverified")?;
    if running && (!supported || !isolated)
        || ready && (!running || !isolated) {
        return Err("Owned browser state is inconsistent".into());
    }
    let active_tab=status["activeTabId"].as_i64().unwrap_or(0);
    let title=status["tabs"].as_array().and_then(|tabs|tabs.iter().find(|tab|
        tab["tabId"].as_i64()==Some(active_tab))).and_then(|tab|tab["title"].as_str())
        .unwrap_or("").chars().take(100).collect::<String>();
    Ok(json!({"supported":supported,"running":running,
        "ready":ready,"profileIsolated":isolated,"title":title,
        "mode":"right_sidebar","presentation":"native_right_drawer"}))
}

/// Call once inside the existing Tauri `.setup` after `Engine::new` and before
/// `app.manage(Arc::new(engine))`:
/// android_agent_tools::register(&engine, app.handle().clone())?;
pub fn register(engine: &Engine, app: AppHandle) -> Result<(), String> {
    engine.install_android_local_tool_host(Arc::new(Host::new(app)))
}

fn string<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
    args[field]
        .as_str()
        .ok_or_else(|| "Approved Android tool text argument missing".into())
}

fn int64(args: &Value, field: &str) -> Result<i64, String> {
    args[field]
        .as_i64()
        .ok_or_else(|| "Approved Android tool integer argument missing".into())
}

fn int32(args: &Value, field: &str) -> Result<i32, String> {
    i32::try_from(int64(args, field)?)
        .map_err(|_| "Android tool control index exceeds range".into())
}

impl AndroidLocalToolHost for Host {
    fn resolve_app_label<'a>(
        &'a self,
        query: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(String, String)>, String>> + Send + 'a>> {
        Box::pin(async move {
            let receipt = crate::android_accessibility::accessibility_automation_resolve_target(
                self.app.clone(), query.to_owned(),
            ).await?;
            if receipt["resolved"] == false {
                return Ok(None);
            }
            if receipt["resolved"] != true {
                return Err("Native Android app lookup was not confirmed".into());
            }
            let package = receipt["targetPackage"]
                .as_str().filter(|s| !s.is_empty() && s.len() <= 160)
                .ok_or("Native app package missing")?;
            let label = receipt["displayName"]
                .as_str().filter(|s| !s.is_empty() && s.len() <= 80)
                .ok_or("Native app display name missing")?;
            Ok(Some((package.to_owned(), label.to_owned())))
        })
    }

    fn execute<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move {
            // These are the ONLY available Android operations. No dispatch by
            // URL, shell command, executable name or arbitrary model text.
            match name {
                "android_browser_status" => {
                    match self.selected_browser_mode()? {
                        BrowserMode::Headless =>
                            crate::android_browser_automation::android_background_browser_status(
                                self.app.clone()).await,
                        BrowserMode::RightSidebar => self.private_browser_status().await,
                        BrowserMode::OpenBrowserApp =>
                            Ok(json!({"supported":false,"running":false,"ready":false,
                                "profileIsolated":false,"title":"",
                                "mode":"open_browser_app","automation":false,
                                "reason":"Chrome is launch-only; not an automation target"})),
                    }
                }
                "android_browser_start" => {
                    let url=string(&arguments,"url")?.to_owned();
                    match self.selected_browser_mode()? {
                        BrowserMode::Headless =>
                            crate::android_browser_automation::android_background_browser_start(
                                self.app.clone(),url,true,Some(true)).await,
                        BrowserMode::RightSidebar => {
                            let app=self.app.clone();
                            let receipt=tauri::async_runtime::spawn_blocking(move ||
                                crate::android_private_browser::open(app,url,true)
                            ).await.map_err(|_|"Owned browser open was interrupted".to_string())??;
                            if receipt["opened"] != true {
                                return Err("Owned browser open was not confirmed".into());
                            }
                            self.private_browser_status().await
                        }
                        BrowserMode::OpenBrowserApp => Err(
                            "Chrome mode is launch-only and does not support automated navigation to a requested website. Choose Headless browser or Right sidebar for browser automation".into()),
                    }
                }
                "android_browser_snapshot" => {
                    match self.selected_browser_mode()? {
                        BrowserMode::Headless =>
                            crate::android_browser_automation::android_background_browser_snapshot(
                                self.app.clone()).await,
                        BrowserMode::RightSidebar => {
                            let tab=self.private_tab().await?;
                            let app=self.app.clone();
                            tauri::async_runtime::spawn_blocking(move ||
                                crate::android_private_browser::snapshot(app,tab)
                            ).await.map_err(|_|"Owned DOM snapshot interrupted".to_string())?
                        },
                        BrowserMode::OpenBrowserApp => Err(
                            "Chrome does not expose DOM snapshots to this Android plugin".into()),
                    }
                }
                "android_browser_click" => {
                    let snapshot=int64(&arguments,"snapshotId")?;
                    let index=int32(&arguments,"index")?;
                    match self.selected_browser_mode()? {
                        BrowserMode::Headless =>
                            crate::android_browser_automation::android_background_browser_click(
                                self.app.clone(),snapshot,index,true).await,
                        BrowserMode::RightSidebar => {
                            let tab=self.private_tab().await?;
                            let app=self.app.clone();
                            tauri::async_runtime::spawn_blocking(move ||
                                crate::android_private_browser::click(app,tab,snapshot,index)
                            ).await.map_err(|_|"Owned browser click interrupted".to_string())?
                        }
                        BrowserMode::OpenBrowserApp => Err(
                            "Chrome is launch-only; automated clicks are unavailable".into()),
                    }
                }
                "android_browser_fill" => {
                    let snapshot=int64(&arguments,"snapshotId")?;
                    let index=int32(&arguments,"index")?;
                    let value=string(&arguments,"value")?.to_owned();
                    match self.selected_browser_mode()? {
                        BrowserMode::Headless =>
                            crate::android_browser_automation::android_background_browser_fill(
                                self.app.clone(),snapshot,index,value,true).await,
                        BrowserMode::RightSidebar => {
                            let tab=self.private_tab().await?;
                            let app=self.app.clone();
                            tauri::async_runtime::spawn_blocking(move ||
                                crate::android_private_browser::fill(app,tab,snapshot,index,value)
                            ).await.map_err(|_|"Owned browser fill interrupted".to_string())?
                        }
                        BrowserMode::OpenBrowserApp => Err(
                            "Chrome is launch-only; automated input is unavailable".into()),
                    }
                }
                "android_browser_stop" => {
                    match self.selected_browser_mode()? {
                        BrowserMode::Headless =>
                            crate::android_browser_automation::android_background_browser_stop(
                                self.app.clone()).await,
                        BrowserMode::RightSidebar => {
                            let app=self.app.clone();
                            tauri::async_runtime::spawn_blocking(move ||
                                crate::android_private_browser::close_all(app)
                            ).await.map_err(|_|"Owned browser close interrupted".to_string())??;
                            self.private_browser_status().await
                        }
                        BrowserMode::OpenBrowserApp => Err(
                            "Cannot close personal Chrome sessions from this plugin".into()),
                    }
                }
                // Private broker-only continuation, never advertised to the
                // model. The user approved a specific action while our own
                // Activity was foreground; this restores only that already
                // approved OS-bound target, without creating a new session.
                "android_app_resume_approved" => {
                    crate::android_accessibility::accessibility_automation_resume_approved(
                        self.app.clone(),
                        string(&arguments, "sessionId")?.to_owned(),
                        string(&arguments, "targetPackage")?.to_owned(),
                    ).await
                }
                // The broker needs the OS-bound opaque session ID *before*
                // showing a one-time screen-action approval. Public
                // android_app_status is sanitized by local_agent_tools and
                // never exposes that ID to ChatGPT. The internal handoff
                // context is not part of the MCP tool catalog.
                "android_app_status" | "android_app_handoff_context" => {
                    let mut status = crate::android_accessibility::accessibility_automation_status(
                        self.app.clone()).await?;
                    let mode = self.selected_app_mode()?;
                    // A blue activity indicator is NOT evidence of screen
                    // capture, node actions or gesture authority. A distinct
                    // OS-owned capability probe gates all four action tools.
                    let probe = crate::android_accessibility::accessibility_automation_mode_status(
                        self.app.clone(), mode.clone()).await?;
                    let allowed = probe["selectedMode"] == mode &&
                        (probe["effectiveMode"] == mode || (
                            mode == "realtime" && probe["effectiveMode"] == "screenshot_click"
                            && probe["fallbackUsed"] == true)) &&
                        ((probe["effectiveMode"] == "realtime" &&
                            probe["realtimeAvailable"] == true) ||
                         (probe["effectiveMode"] == "screenshot_click" &&
                            probe["screenshotClickAvailable"] == true));
                    status["automationActionsAvailable"] = json!(allowed &&
                        status["connected"] == true && status["active"] == true &&
                        status["visible"] == true && status["stopRequested"] == false);
                    status["selectedAppMode"] = json!(mode);
                    Ok(status)
                }
                "android_app_start" => {
                    crate::android_accessibility::accessibility_automation_start(
                        self.app.clone(),
                        true,
                        Uuid::new_v4().to_string(),
                        string(&arguments, "targetPackage")?.to_owned(),
                        true,
                    )
                    .await
                }
                "android_app_stop" => {
                    crate::android_accessibility::accessibility_automation_stop(self.app.clone())
                        .await
                }
                "android_app_snapshot" => {
                    let mode = self.selected_app_mode()?;
                    let (session,target) = self.approved_app_context().await?;
                    crate::android_accessibility::android_automation_snapshot(
                        self.app.clone(),true,mode,session,target).await
                }
                "android_app_click" => {
                    let mode = self.selected_app_mode()?;
                    let (session,target) = self.approved_app_context().await?;
                    let snapshot = string(&arguments,"snapshotId")?.to_owned();
                    let index = arguments.get("index").and_then(Value::as_u64)
                        .and_then(|n|u32::try_from(n).ok());
                    let x = arguments.get("x").and_then(Value::as_u64)
                        .and_then(|n|u32::try_from(n).ok());
                    let y = arguments.get("y").and_then(Value::as_u64)
                        .and_then(|n|u32::try_from(n).ok());
                    crate::android_accessibility::android_automation_click(
                        self.app.clone(),true,mode,session,target,snapshot,index,x,y).await
                }
                "android_app_fill" => {
                    if self.selected_app_mode()? != "realtime" {
                        return Err("Text-field automation requires Realtime run mode".into());
                    }
                    let (session,target) = self.approved_app_context().await?;
                    let snapshot = string(&arguments,"snapshotId")?.to_owned();
                    let index = arguments["index"].as_u64()
                        .and_then(|n|u32::try_from(n).ok())
                        .ok_or("App control index missing")?;
                    let value = string(&arguments,"value")?.to_owned();
                    crate::android_accessibility::android_automation_fill(
                        self.app.clone(),true,session,target,snapshot,index,value).await
                }
                "android_app_gesture" => {
                    let mode = self.selected_app_mode()?;
                    let (session,target) = self.approved_app_context().await?;
                    let snapshot = string(&arguments,"snapshotId")?.to_owned();
                    let kind = string(&arguments,"kind")?.to_owned();
                    let coords = |key:&str| -> Result<u32,String> {
                        arguments[key].as_u64().and_then(|n|u32::try_from(n).ok())
                            .ok_or_else(||format!("App gesture {key} missing"))
                    };
                    let x=coords("x")?;
                    let y=coords("y")?;
                    let to_x=coords("toX")?;
                    let to_y=coords("toY")?;
                    let duration_ms=coords("durationMs")?;
                    crate::android_accessibility::android_automation_gesture(
                        self.app.clone(),true,mode,session,target,snapshot,kind,
                        x,y,to_x,to_y,duration_ms).await
                }
                _ => Err("Unknown or unauthorized Android local agent operation".into()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_adapter_extracts_bounded_typed_identifiers() {
        let args = serde_json::json!({"snapshotId":7,"index":48,"url":"https://example.org"});
        assert_eq!(int64(&args, "snapshotId").unwrap(), 7);
        assert_eq!(int32(&args, "index").unwrap(), 48);
        assert_eq!(string(&args, "url").unwrap(), "https://example.org");
        assert!(int32(&serde_json::json!({"index":2147483648_i64}), "index").is_err());
        assert!(string(&serde_json::json!({"url":2}), "url").is_err());
    }

    #[test]
    fn private_browser_projection_requires_verified_isolated_and_ready_state() {
        let valid=serde_json::json!({"profileIsolationSupported":true,"opened":true,
            "isolatedProfile":true,"domControl":true,"activeTabId":7,
            "tabs":[{"tabId":7,"title":"Example Domain","url":"https://example.org"}]});
        let receipt=normalized_private_status(&valid).unwrap();
        assert_eq!(receipt["mode"],"right_sidebar");
        assert_eq!(receipt["presentation"],"native_right_drawer");
        assert_eq!(receipt["running"],true);
        assert_eq!(receipt["ready"],true);
        assert_eq!(receipt["title"],"Example Domain");
        let mut no_profile=valid.clone();
        no_profile["isolatedProfile"]=serde_json::json!(false);
        assert!(normalized_private_status(&no_profile).is_err());
        let mut no_session=valid.clone();
        no_session["opened"]=serde_json::json!(false);
        assert!(normalized_private_status(&no_session).is_err());
        let mut missing=valid;
        missing.as_object_mut().unwrap().remove("profileIsolationSupported");
        assert!(normalized_private_status(&missing).is_err());
    }
}
