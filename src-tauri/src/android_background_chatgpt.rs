//! App-owned Android ChatGPT website session and explicitly authorized DOM bridge.
//! Kotlin retains the private WebView/profile, prompt lease and page evidence;
//! only the final owned assistant answer can cross to Tauri.
use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

#[cfg(target_os = "android")]
struct ChatGptBrowserHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("background-chatgpt")
        .setup(|app, api| {
            let plugin =
                api.register_android_plugin("ai.mcpagent.app", "BackgroundChatGptPlugin")?;
            app.manage(ChatGptBrowserHandle(plugin));
            Ok(())
        })
        .build()
}

fn validate_receipt(receipt: Value) -> Result<Value, String> {
    let object = receipt.as_object().ok_or("Invalid ChatGPT browser state")?;
    for field in [
        "supported",
        "running",
        "visible",
        "sessionLinked",
        "signInVerified",
    ] {
        if !object.get(field).is_some_and(Value::is_boolean) {
            return Err("Invalid ChatGPT browser state".into());
        }
    }
    if object.get("visible") == Some(&Value::Bool(true))
        && object.get("running") != Some(&Value::Bool(true))
    {
        return Err("Invalid visible ChatGPT browser state".into());
    }
    if object.get("sessionLinked") == Some(&Value::Bool(true))
        && (object.get("supported") != Some(&Value::Bool(true))
            || object.get("running") != Some(&Value::Bool(true)))
    {
        return Err("Invalid linked ChatGPT browser state".into());
    }
    if object.get("signInVerified") == Some(&Value::Bool(true))
        && (object.get("supported") != Some(&Value::Bool(true))
            || object.get("running") != Some(&Value::Bool(true))
            || object.get("visible") != Some(&Value::Bool(false)))
    {
        return Err("Invalid verified ChatGPT login state".into());
    }
    if !object
        .get("engine")
        .is_some_and(|v| v.as_str().is_some_and(|s| s.len() <= 120))
        || !object
            .get("message")
            .is_some_and(|v| v.as_str().is_some_and(|s| s.len() <= 500))
    {
        return Err("Invalid ChatGPT browser metadata".into());
    }
    Ok(receipt)
}

#[cfg(target_os = "android")]
fn call(app: AppHandle, method: &str, payload: Value) -> Result<Value, String> {
    let handle = app
        .try_state::<ChatGptBrowserHandle<tauri::Wry>>()
        .ok_or("Owned Android ChatGPT browser is not registered")?;
    let receipt: Value = handle
        .0
        .run_mobile_plugin(method, payload)
        .map_err(|_| "Owned Android ChatGPT browser operation failed")?;
    validate_receipt(receipt)
}

#[cfg(not(target_os = "android"))]
fn call(_app: AppHandle, _method: &str, _payload: Value) -> Result<Value, String> {
    Err("Owned Android ChatGPT browser unavailable on this platform".into())
}

pub fn status(app: AppHandle) -> Result<Value, String> {
    call(app, "status", json!({}))
}

/// One-shot read-only page capability receipt. No page text, credentials, or
/// conversation contents are returned to Tauri. Not evidence of login/AI reply.
pub fn probe(app: AppHandle) -> Result<Value, String> {
    let result = call_unchecked(app, "probe", json!({}))?;
    validate_probe_receipt(&result)?;
    Ok(result)
}

fn validate_session_receipt(value: &Value, brought: bool) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or("Invalid owned ChatGPT login receipt")?;
    if object.len() != 6
        || object.get("owned") != Some(&json!(true))
        || object.get("origin") != Some(&json!("chatgpt.com"))
        || object.get("readOnly") != Some(&json!(true))
        || !object
            .get("pageGeneration")
            .is_some_and(|v| v.as_u64().is_some())
        || !object.get("sessionLinked").is_some_and(Value::is_boolean)
        || !matches!(
            object.get("signInState").and_then(Value::as_str),
            Some("signed_in" | "signed_out" | "unknown")
        )
        || (object.get("sessionLinked") == Some(&json!(true))
            && object.get("signInState") != Some(&json!("signed_in")))
        || (brought
            && object.get("signInState") == Some(&json!("signed_in"))
            && object.get("sessionLinked") != Some(&json!(true)))
    {
        return Err("ChatGPT login handoff was not independently verified".into());
    }
    Ok(())
}

/// Read-only auth check using the owned page's authenticated session endpoint.
/// Neither session tokens nor account details cross this bridge.
pub fn check_session(app: AppHandle) -> Result<Value, String> {
    let receipt = call_unchecked(app, "checkSession", json!({}))?;
    validate_session_receipt(&receipt, false)?;
    Ok(receipt)
}

/// Explicit user action: recheck login and retain the SAME private browser
/// session hidden in the app, without importing cookies from another browser.
pub fn bring_session(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    if !user_initiated {
        return Err("Bringing a ChatGPT session requires a user action".into());
    }
    let receipt = call_unchecked(app, "bringSession", json!({"userInitiated":true}))?;
    validate_session_receipt(&receipt, true)?;
    Ok(receipt)
}

fn validate_probe_receipt(value: &Value) -> Result<(), String> {
    let object = value.as_object().ok_or("Invalid owned browser probe")?;
    if object.len() != 6
        || object.get("owned") != Some(&json!(true))
        || object.get("origin") != Some(&json!("chatgpt.com"))
        || object.get("readOnly") != Some(&json!(true))
        || !object.get("composerReady").is_some_and(Value::is_boolean)
        || !object.get("sendButtonReady").is_some_and(Value::is_boolean)
        || !object
            .get("pageGeneration")
            .is_some_and(|v| v.as_u64().is_some())
    {
        return Err("Invalid owned browser probe receipt".into());
    }
    Ok(())
}

#[cfg(target_os = "android")]
fn call_unchecked(app: AppHandle, method: &str, payload: Value) -> Result<Value, String> {
    let handle = app
        .try_state::<ChatGptBrowserHandle<tauri::Wry>>()
        .ok_or("Owned Android ChatGPT browser is not registered")?;
    handle
        .0
        .run_mobile_plugin(method, payload)
        .map_err(|_| "Owned Android ChatGPT probe was unsuccessful".into())
}
#[cfg(not(target_os = "android"))]
fn call_unchecked(_app: AppHandle, _method: &str, _payload: Value) -> Result<Value, String> {
    Err("Owned Android ChatGPT browser unavailable".into())
}

pub fn open(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    if !user_initiated {
        return Err("ChatGPT browser opening requires an explicit user action".into());
    }
    call(app, "open", json!({"userInitiated":true}))
}

pub fn show(app: AppHandle) -> Result<Value, String> {
    call(app, "show", json!({}))
}
pub fn hide(app: AppHandle) -> Result<Value, String> {
    call(app, "hide", json!({}))
}
pub fn close(app: AppHandle) -> Result<Value, String> {
    call(app, "close", json!({}))
}

fn valid_request_id(id: &str) -> bool {
    if id.len() != 36 {
        return false;
    }
    id.as_bytes().iter().enumerate().all(|(i, c)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            *c == b'-'
        } else {
            c.is_ascii_hexdigit()
        }
    })
}

fn valid_prompt(prompt: &str) -> bool {
    !prompt.trim().is_empty()
        && prompt.encode_utf16().count() <= 32_000
        && !prompt
            .chars()
            .any(|ch| ch == '\0' || (ch.is_control() && !matches!(ch, '\n' | '\t' | '\r')))
}

/// The mobile plugin must never return transcript, cookies, routes or browser credentials.
/// A completed answer is allowed only after one matching user turn and a stable assistant
/// candidate were observed in the same owned WebView lease.
fn validate_bridge_receipt(value: &Value, id: &str) -> Result<(), String> {
    let object = value.as_object().ok_or("Invalid ChatGPT bridge receipt")?;
    if !(5..=7).contains(&object.len())
        || object.get("requestId").and_then(Value::as_str) != Some(id)
        || !object
            .get("pageGeneration")
            .is_some_and(|v| v.as_u64().is_some())
        || object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "requestId"
                    | "state"
                    | "accepted"
                    | "completed"
                    | "pageGeneration"
                    | "conversationId"
                    | "answer"
            )
        })
    {
        return Err("Invalid ChatGPT bridge identity".into());
    }
    let state = object
        .get("state")
        .and_then(Value::as_str)
        .ok_or("Invalid ChatGPT bridge state")?;
    if !matches!(
        state,
        "prepared"
            | "submitted"
            | "answering"
            | "completed"
            | "uncertain"
            | "failed_before_send"
            | "cancelled"
    ) {
        return Err("Invalid ChatGPT bridge state".into());
    }
    let accepted = object
        .get("accepted")
        .and_then(Value::as_bool)
        .ok_or("Invalid ChatGPT acceptance receipt")?;
    let completed = object
        .get("completed")
        .and_then(Value::as_bool)
        .ok_or("Invalid ChatGPT completion receipt")?;
    if let Some(conversation_id) = object.get("conversationId") {
        let conversation_id = conversation_id
            .as_str()
            .ok_or("Invalid ChatGPT conversation ID")?;
        if !accepted
            || !(8..=64).contains(&conversation_id.len())
            || !conversation_id
                .bytes()
                .all(|ch| ch.is_ascii_hexdigit() || ch == b'-')
        {
            return Err("Invalid ChatGPT conversation binding".into());
        }
    }
    if completed != (state == "completed")
        || (state == "completed" && !accepted)
        || (matches!(
            state,
            "prepared" | "submitted" | "failed_before_send" | "cancelled"
        ) && (accepted || completed))
        || (state == "prepared" && object.contains_key("answer"))
        || (state != "completed" && object.contains_key("answer"))
    {
        return Err("Conflicting ChatGPT bridge receipt".into());
    }
    if completed {
        let answer = object
            .get("answer")
            .and_then(Value::as_str)
            .ok_or("ChatGPT completed without an owned answer")?;
        if answer.trim().is_empty()
            || answer.encode_utf16().count() > 100_000
            || answer.len() > 400_000
        {
            return Err("Invalid ChatGPT answer".into());
        }
    }
    Ok(())
}

fn bridge_call(app: AppHandle, method: &str, id: String, payload: Value) -> Result<Value, String> {
    if !valid_request_id(&id) {
        return Err("Invalid ChatGPT request ID".into());
    }
    let receipt = call_unchecked(app, method, payload)?;
    validate_bridge_receipt(&receipt, &id)?;
    Ok(receipt)
}

/// Reviewable draft preparation. Only an empty, unique ChatGPT editor may receive
/// these bytes. Never dispatches Send.
pub fn prepare(
    app: AppHandle,
    request_id: String,
    prompt: String,
    expected_conversation_id: Option<String>,
) -> Result<Value, String> {
    if !valid_request_id(&request_id) || !valid_prompt(&prompt) {
        return Err("Invalid ChatGPT prepare arguments".into());
    }
    if expected_conversation_id
        .as_deref()
        .is_some_and(|id| !valid_request_id(id))
    {
        return Err("Invalid ChatGPT conversation binding".into());
    }
    bridge_call(
        app,
        "prepare",
        request_id.clone(),
        json!({"requestId":request_id, "prompt":prompt, "expectedConversationId":expected_conversation_id}),
    )
}

/// Exactly one explicitly authorized Send attempt. Uncertain outcomes are terminal
/// and may only be observed/reconciled; they can never trigger a second click.
pub fn send(app: AppHandle, request_id: String, user_initiated: bool) -> Result<Value, String> {
    if !user_initiated {
        return Err("ChatGPT send requires explicit approval".into());
    }
    bridge_call(
        app,
        "send",
        request_id.clone(),
        json!({"requestId":request_id, "userInitiated":true}),
    )
}

/// Idempotently check the exact same request's accepted user row and final answer.
pub fn observe(app: AppHandle, request_id: String) -> Result<Value, String> {
    bridge_call(
        app,
        "observe",
        request_id.clone(),
        json!({"requestId":request_id}),
    )
}

/// Explicit cancellation/reconciliation. After a click it can only disown the
/// pending request; it is never evidence the remote send was undone.
pub fn cancel(app: AppHandle, request_id: String) -> Result<Value, String> {
    bridge_call(
        app,
        "cancel",
        request_id.clone(),
        json!({"requestId":request_id}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_missing_or_conflicting_browser_receipts() {
        assert!(validate_receipt(json!({"supported":true})).is_err());
        assert!(
            validate_receipt(json!({"supported":true,"running":false,"visible":true,
            "sessionLinked":false,"signInVerified":false,"engine":"Android","message":"Invalid"}))
            .is_err()
        );
        assert!(
            validate_receipt(json!({"supported":true,"running":false,"visible":false,
            "sessionLinked":false,"signInVerified":false,"engine":"Android","message":"Stopped"}))
            .is_ok()
        );
        assert!(validate_receipt(json!({"supported":true,"running":false,"visible":false,
            "sessionLinked":true,"signInVerified":false,"engine":"Android","message":"Forged linked"})).is_err());
        assert!(
            validate_receipt(json!({"supported":true,"running":true,"visible":false,
            "sessionLinked":false,"signInVerified":true,"engine":"Android","message":"Verified"}))
            .is_ok()
        );
        assert!(
            validate_receipt(json!({"supported":true,"running":true,"visible":true,
            "sessionLinked":false,"signInVerified":true,"engine":"Android","message":"Invalid"}))
            .is_err()
        );
        assert!(
            validate_receipt(json!({"supported":true,"running":false,"visible":false,
            "sessionLinked":false,"signInVerified":true,"engine":"Android","message":"Invalid"}))
            .is_err()
        );
    }
    #[test]
    fn verify_auth_handoff_does_not_accept_credentials_or_fake_login() {
        let login = json!({"owned":true,"origin":"chatgpt.com","readOnly":true,
            "pageGeneration":4,"signInState":"signed_in","sessionLinked":true});
        assert!(validate_session_receipt(&login, true).is_ok());
        assert!(validate_session_receipt(
            &json!({"owned":true,"origin":"chatgpt.com",
            "readOnly":true,"pageGeneration":4,"signInState":"signed_out",
            "sessionLinked":false}),
            false
        )
        .is_ok());
        for invalid in [
            json!({"owned":true,"origin":"chatgpt.com","readOnly":true,
                "pageGeneration":4,"signInState":"unknown","sessionLinked":true}),
            json!({"owned":true,"origin":"chatgpt.com","readOnly":true,
                "pageGeneration":4,"signInState":"signed_out","sessionLinked":true}),
            json!({"owned":true,"origin":"other.example","readOnly":true,
                "pageGeneration":4,"signInState":"signed_in","sessionLinked":true}),
            json!({"owned":true,"origin":"chatgpt.com","readOnly":true,
                "pageGeneration":4,"signInState":"signed_in","sessionLinked":true,
                "cookie":"DO_NOT_EXPOSE"}),
            json!({"owned":true,"origin":"chatgpt.com","readOnly":true,
                "pageGeneration":4,"signInState":"signed_in","sessionLinked":"yes"}),
        ] {
            assert!(validate_session_receipt(&invalid, true).is_err());
        }
    }
    #[test]
    fn reject_unsafe_browser_probe_fields() {
        let valid = json!({"owned":true,"origin":"chatgpt.com","readOnly":true,
            "pageGeneration": 6,"composerReady":true,"sendButtonReady":false});
        assert!(validate_probe_receipt(&valid).is_ok());
        for bad in [
            json!({"owned":false,"origin":"chatgpt.com","readOnly":true,
                "pageGeneration":6,"composerReady":true,"sendButtonReady":false}),
            json!({"owned":true,"origin":"evil.invalid","readOnly":true,
                "pageGeneration":6,"composerReady":true,"sendButtonReady":false}),
            json!({"owned":true,"origin":"chatgpt.com","readOnly":true,
                "pageGeneration":6,"composerReady":true,"sendButtonReady":false,
                "privateMessages":["not allowed"]}),
        ] {
            assert!(validate_probe_receipt(&bad).is_err());
        }
    }

    #[test]
    fn bridge_request_validation() {
        let id = "b60be7b9-edf2-4c4e-9afe-d0e671d92a0c";
        assert!(valid_request_id(id));
        assert!(!valid_request_id("123"));
        assert!(!valid_request_id("b60be7b9-edf2-4c4e-9afe-zzzzzzzzzzzz"));
        assert!(valid_prompt("What is a safe Rust API?\nAnother line."));
        assert!(!valid_prompt(" \t "));
        assert!(!valid_prompt("hi\0there"));
        assert!(!valid_prompt(&"x".repeat(32_001)));
        assert!(!valid_request_id("abc123ef"));
        assert!(valid_request_id("aaaabbbb-cccc-dddd-eeee-ffffffffffff"));
    }

    #[test]
    fn reject_forged_or_conflicting_bridge_receipts() {
        let id = "b60be7b9-edf2-4c4e-9afe-d0e671d92a0c";
        let base = json!({"requestId":id,"pageGeneration":7,"state":"prepared",
            "accepted":false,"completed":false});
        assert!(validate_bridge_receipt(&base, id).is_ok());
        let cancelled = json!({"requestId":id,"pageGeneration":7,"state":"cancelled",
            "accepted":false,"completed":false});
        assert!(validate_bridge_receipt(&cancelled, id).is_ok());
        let final_answer = json!({"requestId":id,"pageGeneration":7,"state":"completed",
            "accepted":true,"completed":true,"conversationId":"abc123ef-7890","answer":"An owned reply"});
        assert!(validate_bridge_receipt(&final_answer, id).is_ok());
        for forged in [
            json!({"requestId":"different","pageGeneration":7,"state":"completed",
                "accepted":true,"completed":true,"answer":"wrong chat"}),
            json!({"requestId":id,"pageGeneration":7,"state":"completed",
                "accepted":false,"completed":true,"answer":"unaccepted"}),
            json!({"requestId":id,"pageGeneration":7,"state":"answering",
                "accepted":true,"completed":false,"answer":"leaked partial"}),
            json!({"requestId":id,"pageGeneration":7,"state":"submitted",
                "accepted":true,"completed":false}),
            json!({"requestId":id,"pageGeneration":7,"state":"answering",
                "accepted":false,"completed":false,"conversationId":"abc123ef-7890"}),
            json!({"requestId":id,"pageGeneration":7,"state":"completed",
                "accepted":true,"completed":true,"answer":" "}),
            json!({"requestId":id,"pageGeneration":7,"state":"completed",
                "accepted":true,"completed":true,"answer":"final","cookies":"secret"}),
        ] {
            assert!(validate_bridge_receipt(&forged, id).is_err());
        }
    }
}
