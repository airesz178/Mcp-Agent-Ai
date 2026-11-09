use mcp_agent_core::{BrowserChatTurnReceipt, BrowserTurnReceiptStatus, Engine, OAuthChatTurnReceipt, Session};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tauri::Manager;

// A browser receipt is never a generic API-provider response. Only a native
// WebView observation for an explicitly prepared request may be saved.
struct BrowserChatLease {
    prompt: String,
    session_id: Option<String>,
    expected_session: Option<Session>,
    prepared: bool,
    send_attempted: bool,
    verified_pre_send_failure: bool,
}
#[derive(Default)]
struct BrowserChatLeases(Mutex<HashMap<String, BrowserChatLease>>);

fn browser_receipt(value: &Value, request_id: &str) -> Result<(), String> {
    if value.get("requestId").and_then(Value::as_str) != Some(request_id)
        || !value.get("accepted").is_some_and(Value::is_boolean)
        || !value.get("completed").is_some_and(Value::is_boolean)
        || !value.get("state").and_then(Value::as_str).is_some_and(|s| {
            matches!(
                s,
                "prepared"
                    | "ready"
                    | "submitted"
                    | "answering"
                    | "completed"
                    | "uncertain"
                    | "failed_before_send"
                    | "cancelled"
                    | "not_started"
            )
        })
    {
        return Err("Owned browser returned an invalid scoped request receipt".into());
    }
    let completed = value["completed"] == true;
    if completed != (value["state"] == "completed")
        || (completed && value["accepted"] != true)
        || (completed
            && !value
                .get("answer")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty() && text.len() <= 100_000))
        || (!completed && value.get("answer").is_some())
    {
        return Err("Owned browser completion is unverified".into());
    }
    Ok(())
}

fn native_browser_prepare(
    app: tauri::AppHandle,
    request_id: String,
    prompt: String,
    expected_conversation_id: Option<String>,
) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        android_background_chatgpt::prepare(app, request_id, prompt, expected_conversation_id)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, request_id, prompt, expected_conversation_id);
        Err("Only the Android app supports native ChatGPT browser requests".into())
    }
}
fn native_browser_running(app: tauri::AppHandle) -> Result<bool, String> {
    #[cfg(target_os = "android")]
    let status = android_background_chatgpt::status(app)?;
    #[cfg(not(target_os = "android"))]
    let status = {
        let _ = app;
        json!({"supported":false,"running":false})
    };
    Ok(status["supported"] == true && status["running"] == true)
}
fn native_browser_send(
    app: tauri::AppHandle,
    request_id: String,
    user_initiated: bool,
) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        android_background_chatgpt::send(app, request_id, user_initiated)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, request_id, user_initiated);
        Err("Only the Android app supports native ChatGPT browser requests".into())
    }
}
fn native_browser_observe(app: tauri::AppHandle, request_id: String) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        android_background_chatgpt::observe(app, request_id)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, request_id);
        Err("Only the Android app supports native ChatGPT browser requests".into())
    }
}
fn native_browser_cancel(app: tauri::AppHandle, request_id: String) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        android_background_chatgpt::cancel(app, request_id)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, request_id);
        Err("Only the Android app supports native ChatGPT browser requests".into())
    }
}
mod android_accessibility;
mod android_automation_preferences;
#[cfg(any(target_os = "android", test))]
mod android_agent_tools;
mod android_browser_automation;
mod android_background_chatgpt;
mod android_chatgpt_oauth;
mod android_documents;
mod android_foreground_runtime;
mod android_gecko_browser;
mod android_local_mcp;
mod android_main_storage;
mod android_mcp_files;
mod android_mcp_permissions;
mod android_openai_tunnel;
#[cfg(any(target_os = "android", test))]
mod android_plugins;
#[cfg(test)]
mod android_plugins_approvals;
mod android_pet_overlay;
mod android_private_browser;
mod android_project_runtime;
mod android_pty;
mod android_review_share;
mod android_speech_dictation;
mod android_terminal;
mod android_turn_notifications;
mod android_tunnel_credentials;
mod storage_access;

#[tauri::command]
async fn api_request(
    app: tauri::AppHandle,
    route: String,
    body: Option<Value>,
) -> Result<Value, String> {
    // Return errors in the same structured format as the previous UI API.
    let engine = app.state::<Arc<Engine>>().inner().clone();
    Ok(match engine.request(&route, body).await {
        Ok(value) => value,
        Err(message) => json!({"error": message}),
    })
}

#[tauri::command]
fn storage_access_status(app: tauri::AppHandle) -> Result<Value, String> {
    storage_access::query(app)
}

#[tauri::command]
fn storage_access_request(app: tauri::AppHandle) -> Result<Value, String> {
    storage_access::request(app)
}

#[tauri::command]
async fn android_main_storage_status(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_main_storage::status(app))
        .await
        .map_err(|_| "Android main-storage status interrupted".to_owned())?
}

#[tauri::command]
async fn android_main_storage_list(
    app: tauri::AppHandle,
    path: Vec<String>,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_main_storage::list(app, path))
        .await
        .map_err(|_| "Android main-storage listing interrupted".to_owned())?
}

#[tauri::command]
async fn android_main_storage_selection_status(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_main_storage::selection_status(app))
        .await
        .map_err(|_| "Android main-storage selection status interrupted".to_owned())?
}

#[tauri::command]
async fn android_main_storage_select(
    app: tauri::AppHandle,
    path: Vec<String>,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_main_storage::select(app, path))
        .await
        .map_err(|_| "Android main-storage selection interrupted".to_owned())?
}

#[tauri::command]
async fn android_main_storage_reset(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_main_storage::reset(app))
        .await
        .map_err(|_| "Android main-storage reset interrupted".to_owned())?
}

#[tauri::command]
fn android_permissions_status(app: tauri::AppHandle) -> Result<Value, String> {
    storage_access::query_permissions(app)
}

#[tauri::command]
fn android_permissions_request(app: tauri::AppHandle, permission: String) -> Result<Value, String> {
    storage_access::request_permission(app, permission)
}

#[tauri::command]
fn android_optional_permissions_status(app: tauri::AppHandle) -> Result<Value, String> {
    storage_access::query_optional_permissions(app)
}

#[tauri::command]
fn android_optional_permissions_request(
    app: tauri::AppHandle,
    permission: String,
    user_initiated: bool,
) -> Result<Value, String> {
    storage_access::request_optional_permission(app, permission, user_initiated)
}

#[tauri::command]
fn document_tree_status(app: tauri::AppHandle) -> Result<Value, String> {
    android_documents::status(app)
}

#[tauri::command]
async fn document_tree_request(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_documents::request(app))
        .await
        .map_err(|_| "Android document picker failed".to_owned())?
}

#[tauri::command]
async fn document_tree_list(
    app: tauri::AppHandle,
    tree_uri: String,
    path: Vec<String>,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_documents::list(app, tree_uri, path))
        .await
        .map_err(|_| "Android document listing failed".to_owned())?
}

#[tauri::command]
async fn document_tree_open(
    app: tauri::AppHandle,
    tree_uri: String,
    path: Vec<String>,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_documents::open(app, tree_uri, path))
        .await
        .map_err(|_| "Android document preview failed".to_owned())?
}
#[tauri::command]
async fn in_app_browser_status(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_private_browser::status(app))
        .await
        .map_err(|_| "In-app browser status unavailable".to_owned())?
}
#[tauri::command]
async fn android_chatgpt_status(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_background_chatgpt::status(app))
        .await
        .map_err(|_| "Android ChatGPT browser status interrupted".to_owned())?
}
#[tauri::command]
async fn android_chatgpt_probe(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_background_chatgpt::probe(app))
        .await
        .map_err(|_| "Android owned ChatGPT probe interrupted".to_owned())?
}
#[tauri::command]
async fn android_chatgpt_check_session(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_background_chatgpt::check_session(app))
        .await
        .map_err(|_| "Android ChatGPT session check interrupted".to_owned())?
}
#[tauri::command]
async fn android_chatgpt_bring_session(
    app: tauri::AppHandle,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        android_background_chatgpt::bring_session(app, user_initiated)
    })
    .await
    .map_err(|_| "Android ChatGPT session handoff interrupted".to_owned())?
}
#[tauri::command]
async fn android_chatgpt_open(
    app: tauri::AppHandle,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        android_background_chatgpt::open(app, user_initiated)
    })
    .await
    .map_err(|_| "Android ChatGPT browser open interrupted".to_owned())?
}
#[tauri::command]
async fn android_chatgpt_show(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_background_chatgpt::show(app))
        .await
        .map_err(|_| "Android ChatGPT browser show interrupted".to_owned())?
}
#[tauri::command]
async fn android_chatgpt_hide(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_background_chatgpt::hide(app))
        .await
        .map_err(|_| "Android ChatGPT browser hide interrupted".to_owned())?
}
#[tauri::command]
async fn android_chatgpt_close(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_background_chatgpt::close(app))
        .await
        .map_err(|_| "Android ChatGPT browser close interrupted".to_owned())?
}
#[tauri::command]
async fn android_chatgpt_oauth_status(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_chatgpt_oauth::status(app))
        .await
        .map_err(|_| "ChatGPT OAuth status interrupted".to_owned())?
}
#[tauri::command]
async fn android_chatgpt_oauth_start(
    app: tauri::AppHandle,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_chatgpt_oauth::start(app, user_initiated))
        .await
        .map_err(|_| "ChatGPT OAuth sign-in interrupted".to_owned())?
}
#[tauri::command]
async fn android_chatgpt_oauth_bring(
    app: tauri::AppHandle,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_chatgpt_oauth::bring(app, user_initiated))
        .await
        .map_err(|_| "ChatGPT OAuth plan verification interrupted".to_owned())?
}
#[tauri::command]
async fn android_chatgpt_oauth_signout(
    app: tauri::AppHandle,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        android_chatgpt_oauth::signout(app, user_initiated)
    })
    .await
    .map_err(|_| "ChatGPT OAuth disconnect interrupted".to_owned())?
}

#[tauri::command]
async fn chatgpt_oauth_chat(
    app: tauri::AppHandle,
    request_id: String,
    prompt: String,
    session_id: Option<String>,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated {
        return Err("ChatGPT OAuth Send requires explicit one-time approval".into());
    }
    if prompt.trim().is_empty() || prompt.len() > 20_000 {
        return Err("ChatGPT OAuth prompt must be 1-20,000 bytes".into());
    }
    let engine = app.state::<Arc<Engine>>().inner().clone();
    // This snapshot must be captured before the native HTTP request.
    let (expected_session, context) =
        engine.chatgpt_oauth_session_snapshot(session_id.as_deref())?;
    // One durable claim even for interrupted, uncertain, and failed sends.
    engine.claim_oauth_chat_request(&request_id)?;
    let id = request_id.clone();
    let native = tauri::async_runtime::spawn_blocking(move || {
        android_chatgpt_oauth::chat(app, id, prompt.clone(), context, true)
            .map(|receipt| (receipt, prompt))
    }).await;
    let (receipt, prompt) = match native {
        Ok(Ok(result)) => result,
        _ => return Ok(json!({"requestId":request_id,"state":"uncertain"})),
    };
    if receipt["state"] == "failed_before_dispatch" || receipt["state"] == "uncertain" {
        let mut failure = json!({"requestId":request_id,"state":receipt["state"]});
        if let Some(reason) = receipt.get("reason").and_then(Value::as_str) {
            // The native wrapper has already accepted only finite, safe reason codes.
            failure["reason"] = json!(reason);
        }
        return Ok(failure);
    }
    if receipt["state"] != "completed" {
        return Ok(json!({"requestId":request_id,"state":"uncertain"}));
    }
    let answer = receipt["answer"].as_str()
        .ok_or("Native OAuth completion did not include an answer")?.to_owned();
    // Only the native plugin is authorized to produce the answer/proof.
    // Never accept a renderer-supplied completion or reuse a website binding.
    let saved = engine.persist_oauth_chat_turn(OAuthChatTurnReceipt {
        request_id: request_id.clone(), session_id, expected_session,
        prompt, answer,
    })?;
    Ok(saved)
}
#[tauri::command]
async fn custom_gecko_status(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_gecko_browser::status(app))
        .await
        .map_err(|_| "GeckoView status interrupted".to_owned())?
}
#[tauri::command]
async fn custom_gecko_open(
    app: tauri::AppHandle,
    url: String,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        android_gecko_browser::open(app, url, user_initiated)
    })
    .await
    .map_err(|_| "GeckoView open interrupted".to_owned())?
}
#[tauri::command]
async fn custom_gecko_show(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_gecko_browser::show(app))
        .await
        .map_err(|_| "GeckoView show interrupted".to_owned())?
}
#[tauri::command]
async fn custom_gecko_close(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_gecko_browser::close(app))
        .await
        .map_err(|_| "GeckoView close interrupted".to_owned())?
}
#[tauri::command]
async fn custom_gecko_list_extensions(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_gecko_browser::list_extensions(app))
        .await
        .map_err(|_| "GeckoView list interrupted".to_owned())?
}
#[tauri::command]
async fn custom_gecko_install_probe(
    app: tauri::AppHandle,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        android_gecko_browser::install_probe(app, user_initiated)
    })
    .await
    .map_err(|_| "GeckoView probe interrupted".to_owned())?
}
#[tauri::command]
async fn custom_gecko_manage(
    app: tauri::AppHandle,
    id: String,
    action: String,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        android_gecko_browser::manage(app, id, &action, user_initiated)
    })
    .await
    .map_err(|_| "GeckoView management interrupted".to_owned())?
}

#[tauri::command]
async fn chatgpt_browser_prepare(
    app: tauri::AppHandle,
    request_id: String,
    prompt: String,
    session_id: Option<String>,
) -> Result<Value, String> {
    // We only create an app-local lease; browser credentials and website history
    // never enter this API. The selected app session is snapshotted before Send.
    let uuid_shape = request_id.len() == 36
        && request_id.chars().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    if !uuid_shape || prompt.trim().is_empty() || prompt.len() > 16_384 {
        return Err("Invalid browser prompt or request UUID".into());
    }
    let engine = app.state::<Arc<Engine>>().inner().clone();
    let expected_session = engine.chatgpt_browser_session_snapshot(session_id.as_deref())?;
    // A new local conversation may only use a fresh website chat. A follow-up
    // must use the exact website conversation already bound in trusted native
    // receipt metadata; never send into whichever account chat happens to be open.
    let expected_site_id = if let Some(ref previous) = expected_session {
        let mut bound: Option<&str> = None;
        for action in &previous.actions {
            if action["type"] != "chatgpt_browser_turn"
                || action["hostConfirmed"] != true
                || action["mcpToolsUsed"] != false
            {
                return Err("Existing app chat has no trusted ChatGPT browser binding".into());
            }
            let id = action["browserConversationId"]
                .as_str()
                .ok_or("ChatGPT website conversation is not bound; start a new app chat")?;
            if bound.is_some_and(|prior| prior != id) {
                return Err("Conflicting ChatGPT browser conversation bindings".into());
            }
            bound = Some(id);
        }
        Some(
            bound
                .ok_or("ChatGPT website conversation is not bound; start a new app chat")?
                .to_owned(),
        )
    } else {
        None
    };
    let status_app = app.clone();
    let running = tauri::async_runtime::spawn_blocking(move || native_browser_running(status_app))
        .await
        .map_err(|_| "Browser status unavailable")??;
    if !running {
        return Err(
            "Open the app-owned ChatGPT browser explicitly before preparing a message".into(),
        );
    }
    let leases = app.state::<BrowserChatLeases>();
    {
        let mut map = leases
            .0
            .lock()
            .map_err(|_| "Browser request coordinator unavailable")?;
        if !map.is_empty() {
            return Err(
                "A previous browser turn must be reconciled before preparing another".into(),
            );
        }
        map.insert(
            request_id.clone(),
            BrowserChatLease {
                prompt: prompt.clone(),
                session_id,
                expected_session,
                prepared: false,
                send_attempted: false,
                verified_pre_send_failure: false,
            },
        );
    }
    let id = request_id.clone();
    let native_app = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        native_browser_prepare(native_app, id, prompt, expected_site_id)
    })
    .await
    .map_err(|_| "Browser preparation was interrupted")??;
    browser_receipt(&result, &request_id)?;
    if result["state"] == "cancelled" && result["accepted"] == false {
        app.state::<BrowserChatLeases>()
            .0
            .lock()
            .map_err(|_| "Browser request coordinator unavailable")?
            .remove(&request_id);
        return Ok(result);
    }
    if result["state"] == "failed_before_send" && result["accepted"] == false {
        // Android may retain a draft lease after a pre-send refusal. Only an
        // independently verified cancel may remove that lease.
        return Ok(result);
    }
    if !matches!(result["state"].as_str(), Some("prepared" | "ready"))
        || result["accepted"] != false
    {
        return Err("Owned browser did not confirm an isolated prepared draft".into());
    }
    if let Some(lease) = leases
        .0
        .lock()
        .map_err(|_| "Browser request coordinator unavailable")?
        .get_mut(&request_id)
    {
        lease.prepared = true;
    }
    Ok(result)
}

#[tauri::command]
async fn chatgpt_browser_send(
    app: tauri::AppHandle,
    request_id: String,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated {
        return Err("An explicit one-time Send confirmation is required".into());
    }
    {
        let leases = app.state::<BrowserChatLeases>();
        let mut map = leases
            .0
            .lock()
            .map_err(|_| "Browser request coordinator unavailable")?;
        let lease = map
            .get_mut(&request_id)
            .ok_or("Unknown or expired browser request")?;
        if !lease.prepared || lease.send_attempted {
            return Err(
                "Browser Send is not prepared or was already attempted; never retry".into(),
            );
        }
        // This flag is set BEFORE crossing the WebView boundary: timeouts,
        // crashes and lost callbacks may already have caused a remote Send.
        lease.send_attempted = true;
    }
    let id = request_id.clone();
    let native_app = app.clone();
    let result =
        tauri::async_runtime::spawn_blocking(move || native_browser_send(native_app, id, true))
            .await
            .map_err(|_| "Browser Send outcome unknown; never retry automatically")??;
    browser_receipt(&result, &request_id)?;
    if result["state"] == "cancelled" && result["accepted"] == false {
        app.state::<BrowserChatLeases>()
            .0
            .lock()
            .map_err(|_| "Browser request coordinator unavailable")?
            .remove(&request_id);
        return Ok(result);
    }
    if result["state"] == "failed_before_send" && result["accepted"] == false {
        if let Some(lease) = app
            .state::<BrowserChatLeases>()
            .0
            .lock()
            .map_err(|_| "Browser request coordinator unavailable")?
            .get_mut(&request_id)
        {
            lease.verified_pre_send_failure = true;
        }
    }
    Ok(result)
}

#[tauri::command]
async fn chatgpt_browser_observe(
    app: tauri::AppHandle,
    request_id: String,
) -> Result<Value, String> {
    {
        let leases = app.state::<BrowserChatLeases>();
        if !leases
            .0
            .lock()
            .map_err(|_| "Browser request coordinator unavailable")?
            .contains_key(&request_id)
        {
            return Err("Unknown browser request; no history was read".into());
        }
    }
    let id = request_id.clone();
    let result = tauri::async_runtime::spawn_blocking(move || native_browser_observe(app, id))
        .await
        .map_err(|_| "Browser observation interrupted")??;
    browser_receipt(&result, &request_id)?;
    Ok(result)
}

#[tauri::command]
async fn chatgpt_browser_cancel(
    app: tauri::AppHandle,
    request_id: String,
) -> Result<Value, String> {
    {
        let leases = app.state::<BrowserChatLeases>();
        let map = leases
            .0
            .lock()
            .map_err(|_| "Browser request coordinator unavailable")?;
        let lease = map.get(&request_id).ok_or("Unknown browser request")?;
        if lease.send_attempted && !lease.verified_pre_send_failure {
            return Err("Send may have occurred; cancellation cannot prove nondelivery".into());
        }
    }
    let id = request_id.clone();
    let native_app = app.clone();
    let result =
        tauri::async_runtime::spawn_blocking(move || native_browser_cancel(native_app, id))
            .await
            .map_err(|_| "Browser draft cancellation unverified")??;
    browser_receipt(&result, &request_id)?;
    if result["state"] != "cancelled" || result["accepted"] != false {
        return Err("Browser draft was not confirmed cancelled".into());
    }
    app.state::<BrowserChatLeases>()
        .0
        .lock()
        .map_err(|_| "Browser request coordinator unavailable")?
        .remove(&request_id);
    Ok(result)
}

#[tauri::command]
async fn chatgpt_browser_finalize(
    app: tauri::AppHandle,
    request_id: String,
) -> Result<Value, String> {
    // The renderer supplies ONLY the opaque request ID. No renderer-supplied
    // answer, transcript, page URL or claimed completed status is trusted.
    let (prompt, session_id, expected_session) = {
        let leases = app.state::<BrowserChatLeases>();
        let map = leases
            .0
            .lock()
            .map_err(|_| "Browser request coordinator unavailable")?;
        let lease = map.get(&request_id).ok_or("Unknown browser turn")?;
        if !lease.prepared || !lease.send_attempted {
            return Err("Only a prepared and explicitly sent browser turn can be finalized".into());
        }
        (
            lease.prompt.clone(),
            lease.session_id.clone(),
            lease.expected_session.clone(),
        )
    };
    let id = request_id.clone();
    let native_app = app.clone();
    let native_result =
        tauri::async_runtime::spawn_blocking(move || native_browser_observe(native_app, id))
            .await
            .map_err(|_| "Browser final receipt was interrupted")??;
    browser_receipt(&native_result, &request_id)?;
    if native_result["state"] != "completed"
        || native_result["accepted"] != true
        || native_result["completed"] != true
    {
        return Err("The owned browser has not confirmed the exact completed turn".into());
    }
    let answer = native_result["answer"]
        .as_str()
        .ok_or("Owned ChatGPT reply is missing")?
        .to_owned();
    let browser_conversation_id = native_result
        .get("conversationId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if browser_conversation_id.is_none() {
        return Err("Owned ChatGPT answer has no exact conversation URL ID; not saved".into());
    }
    // Never save a website turn through the provider API endpoint, and never
    // authorize an MCP tool from a website transcript.
    let engine = app.state::<Arc<Engine>>().inner().clone();
    let saved = engine.persist_browser_chat_turn(BrowserChatTurnReceipt {
        request_id: request_id.clone(),
        session_id,
        browser_conversation_id,
        prompt,
        answer,
        expected_session,
        status: BrowserTurnReceiptStatus::Confirmed,
        used_mcp_tools: false,
    })?;
    app.state::<BrowserChatLeases>()
        .0
        .lock()
        .map_err(|_| "Browser request coordinator unavailable")?
        .remove(&request_id);
    Ok(
        json!({"sessionId":saved["sessionId"],"answer":saved["answer"],
        "actions":[],"saved":saved["saved"],"status":saved["status"]}),
    )
}
#[tauri::command]
async fn in_app_browser_open(
    app: tauri::AppHandle,
    url: String,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        android_private_browser::open(app, url, user_initiated)
    })
    .await
    .map_err(|_| "In-app browser operation interrupted".to_owned())?
}
#[tauri::command]
async fn in_app_browser_automation(
    app: tauri::AppHandle,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        android_private_browser::request_automation(app, user_initiated)
    })
    .await
    .map_err(|_| "In-app browser permission flow interrupted".to_owned())?
}
#[tauri::command]
async fn in_app_browser_list_tabs(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_private_browser::list_tabs(app))
        .await
        .map_err(|_| "In-app tabs unavailable".to_owned())?
}
#[tauri::command]
async fn in_app_browser_switch_tab(app: tauri::AppHandle, tab_id: i32) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_private_browser::switch_tab(app, tab_id))
        .await
        .map_err(|_| "In-app tab switch interrupted".to_owned())?
}
#[tauri::command]
async fn in_app_browser_close_tab(app: tauri::AppHandle, tab_id: i32) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_private_browser::close_tab(app, tab_id))
        .await
        .map_err(|_| "In-app tab close interrupted".to_owned())?
}
#[tauri::command]
async fn in_app_browser_close_all(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_private_browser::close_all(app))
        .await
        .map_err(|_| "In-app browser close interrupted".to_owned())?
}
#[tauri::command]
async fn in_app_browser_snapshot(app: tauri::AppHandle, tab_id: i32) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || android_private_browser::snapshot(app, tab_id))
        .await
        .map_err(|_| "In-app DOM snapshot interrupted".to_owned())?
}
#[tauri::command]
async fn in_app_browser_click(
    app: tauri::AppHandle,
    tab_id: i32,
    snapshot_id: i64,
    index: i32,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        android_private_browser::click(app, tab_id, snapshot_id, index)
    })
    .await
    .map_err(|_| "In-app DOM click interrupted".to_owned())?
}
#[tauri::command]
async fn in_app_browser_fill(
    app: tauri::AppHandle,
    tab_id: i32,
    snapshot_id: i64,
    index: i32,
    value: String,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        android_private_browser::fill(app, tab_id, snapshot_id, index, value)
    })
    .await
    .map_err(|_| "In-app DOM fill interrupted".to_owned())?
}
#[tauri::command]
async fn in_app_browser_scroll(
    app: tauri::AppHandle,
    tab_id: i32,
    snapshot_id: i64,
    index: i32,
    delta_y: i32,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        android_private_browser::scroll(app, tab_id, snapshot_id, index, delta_y)
    })
    .await
    .map_err(|_| "In-app DOM scroll interrupted".to_owned())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    #[cfg(target_os = "android")]
    let builder = builder.plugin(storage_access::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_main_storage::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_documents::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_private_browser::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_browser_automation::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_background_chatgpt::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_chatgpt_oauth::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_gecko_browser::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_terminal::init());
    #[cfg(target_os = "android")]
    let builder = builder.on_window_event(|window, event| match event {
        tauri::WindowEvent::Focused(focused) => {
            android_pty::set_foreground(window.app_handle(), *focused)
        }
        tauri::WindowEvent::Destroyed => android_pty::set_foreground(window.app_handle(), false),
        _ => {}
    });
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_review_share::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_accessibility::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_foreground_runtime::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_turn_notifications::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_pet_overlay::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_speech_dictation::init());
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android_tunnel_credentials::init());
    builder
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            app.manage(android_automation_preferences::AndroidAutomationPreferences::new(
                data_dir.clone(),
            ));
            let engine =
                Engine::new(data_dir.clone()).map_err(|message| std::io::Error::other(message))?;
            #[cfg(target_os = "android")]
            android_agent_tools::register(&engine, app.handle().clone())
                .map_err(std::io::Error::other)?;
            app.manage(Arc::new(engine));
            app.manage(BrowserChatLeases::default());
            let permissions = android_mcp_permissions::AndroidMcpPermissions::for_app(
                app.handle().clone(),
                &data_dir,
            );
            let tunnel = android_openai_tunnel::AndroidOpenAiTunnel::with_permissions_and_data_dir(
                permissions.clone(), data_dir.clone(),
            );
            app.manage(tunnel.clone());
            #[cfg(target_os = "android")]
            {
                let plugin_runtime = android_plugins::AndroidPluginsRuntime::new(
                    app.handle().clone(),tunnel.clone(),data_dir.clone());
                app.manage(plugin_runtime);
            }
            // Both Core and enabled Plugins use this one tunnel connection.
            // Install the registry dispatcher before startup resumes polling.
            #[cfg(target_os = "android")]
            android_openai_tunnel::restore_on_startup(app.handle().clone(), tunnel);
            app.manage(permissions);
            app.manage(android_local_mcp::AndroidLocalMcp::default());
            #[cfg(target_os = "android")]
            app.manage(android_pty::AndroidPty::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            api_request,
            storage_access_status,
            storage_access_request,
            android_main_storage_status,
            android_main_storage_list,
            android_main_storage_selection_status,
            android_main_storage_select,
            android_main_storage_reset,
            android_permissions_status,
            android_permissions_request,
            android_optional_permissions_status,
            android_optional_permissions_request,
            document_tree_status,
            document_tree_request,
            document_tree_list,
            document_tree_open,
            android_documents::document_tree_request_write,
            android_documents::document_tree_create_text,
            android_documents::document_tree_create_folder,
            android_documents::document_tree_rename_entry,
            android_documents::document_tree_properties,
            android_documents::document_tree_project_folder_request,
            android_documents::document_tree_project_folder_status,
            android_documents::document_tree_project_folder_release,
            android_pet_overlay::pet_overlay_status,
            android_pet_overlay::pet_overlay_show,
            android_pet_overlay::pet_overlay_hide,
            android_speech_dictation::speech_dictation_status,
            android_speech_dictation::speech_dictation_recognize,
            in_app_browser_status,
            android_browser_automation::android_background_browser_status,
            android_browser_automation::android_background_browser_start,
            android_browser_automation::android_background_browser_stop,
            android_browser_automation::android_background_browser_snapshot,
            android_browser_automation::android_background_browser_click,
            android_browser_automation::android_background_browser_fill,
            android_browser_automation::android_background_browser_scroll,
            in_app_browser_open,
            in_app_browser_automation,
            android_chatgpt_status,
            android_chatgpt_probe,
            android_chatgpt_check_session,
            android_chatgpt_bring_session,
            android_chatgpt_open,
            android_chatgpt_show,
            android_chatgpt_hide,
            android_chatgpt_close,
            android_chatgpt_oauth_status,
            android_chatgpt_oauth_start,
            android_chatgpt_oauth_bring,
            android_chatgpt_oauth_signout,
            chatgpt_oauth_chat,
            custom_gecko_status,
            custom_gecko_open,
            custom_gecko_show,
            custom_gecko_close,
            custom_gecko_list_extensions,
            custom_gecko_install_probe,
            custom_gecko_manage,
            chatgpt_browser_prepare,
            chatgpt_browser_send,
            chatgpt_browser_observe,
            chatgpt_browser_cancel,
            chatgpt_browser_finalize,
            in_app_browser_list_tabs,
            in_app_browser_switch_tab,
            in_app_browser_close_tab,
            in_app_browser_close_all,
            in_app_browser_snapshot,
            in_app_browser_click,
            in_app_browser_fill,
            in_app_browser_scroll,
            android_private_browser::in_app_browser_inspect,
            android_private_browser::in_app_browser_hover,
            android_private_browser::in_app_browser_prepare_preview,
            android_private_browser::in_app_browser_capture_preview,
            android_terminal::android_terminal_status,
            android_terminal::android_terminal_run,
            android_terminal::android_terminal_cancel,
            android_terminal::android_termux_status,
            android_terminal::android_termux_install,
            android_terminal::android_termux_open,
            android_terminal::android_termux_command_status,
            android_terminal::android_termux_command_run,
            android_project_runtime::android_project_runtime_status,
            android_project_runtime::android_project_node,
            android_project_runtime::android_project_npm,
            android_project_runtime::android_project_ssh_identity,
            android_project_runtime::android_project_ssh_pin,
            android_project_runtime::android_project_ssh_run,
            android_pty::android_pty_status,
            android_pty::android_pty_capabilities,
            android_pty::android_pty_create,
            android_pty::android_pty_write,
            android_pty::android_pty_resize,
            android_pty::android_pty_ack,
            android_pty::android_pty_close,
            android_review_share::review_share_markdown,
            android_accessibility::accessibility_status,
            android_accessibility::accessibility_request_settings,
            android_accessibility::accessibility_automation_status,
            android_accessibility::accessibility_automation_mode_status,
            android_accessibility::accessibility_automation_resolve_target,
            android_accessibility::accessibility_automation_start,
            android_accessibility::accessibility_automation_stop,
            android_automation_preferences::android_automation_preferences_status,
            android_automation_preferences::android_automation_preferences_set,
            android_browser_automation::android_external_chrome_status,
            android_browser_automation::android_external_chrome_open,
            android_browser_automation::android_browser_modes_status,
            android_foreground_runtime::android_foreground_runtime_status,
            android_foreground_runtime::android_foreground_runtime_start,
            android_foreground_runtime::android_foreground_runtime_stop,
            android_turn_notifications::android_turn_notification,
            android_openai_tunnel::android_openai_tunnel_status,
            android_openai_tunnel::android_openai_tunnel_connect,
            android_openai_tunnel::android_openai_tunnel_connect_stored,
            android_openai_tunnel::android_tunnel_check_id,
            android_openai_tunnel::android_tunnel_save_id,
            android_openai_tunnel::android_tunnel_verify_and_save,
            android_openai_tunnel::android_tunnel_verify_stored_access,
            android_openai_tunnel::android_tunnel_saved_id_status,
            android_openai_tunnel::android_openai_tunnel_disconnect,
            #[cfg(target_os = "android")]
            android_plugins::android_plugins_status,
            #[cfg(target_os = "android")]
            android_plugins::android_plugins_catalog,
            #[cfg(target_os = "android")]
            android_plugins::android_plugins_set_enabled,
            #[cfg(target_os = "android")]
            android_plugins::android_plugins_install_bundled,
            #[cfg(target_os = "android")]
            android_plugins::android_plugins_uninstall_bundled,
            #[cfg(target_os = "android")]
            android_plugins::android_plugins_check_update,
            android_tunnel_credentials::android_tunnel_credentials_status,
            android_tunnel_credentials::android_tunnel_credentials_store,
            android_tunnel_credentials::android_tunnel_credentials_remove,
            android_tunnel_credentials::android_tunnel_open_platform_page,
            android_mcp_permissions::android_mcp_permissions_status,
            android_mcp_permissions::android_mcp_permissions_configure,
            android_mcp_permissions::android_mcp_command_rules_status,
            android_mcp_permissions::android_mcp_command_rules_set,
            android_local_mcp::android_local_mcp_status,
            android_local_mcp::android_local_mcp_start,
            android_local_mcp::android_local_mcp_stop
        ])
        .run(tauri::generate_context!())
        .expect("could not start Mcp Agent Ai");
}
