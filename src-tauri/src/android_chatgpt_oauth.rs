//! Official Sign in with ChatGPT OAuth grant, separate from the website WebView.
//! All PKCE, callback, ID-token verification and protected credentials stay in
//! the Android native plugin. No tokens or account identifiers reach Tauri UI.
#[cfg(any(target_os = "android", test))]
use serde_json::json;
use serde_json::Value;
use mcp_agent_core::Message;
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

#[cfg(target_os = "android")]
struct OAuthHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("chatgpt-oauth")
        .setup(|app, api| {
            let plugin = api.register_android_plugin("ai.mcpagent.app", "ChatGptOAuthPlugin")?;
            app.manage(OAuthHandle(plugin));
            Ok(())
        })
        .build()
}

fn validate_status(value: Value) -> Result<Value, String> {
    let object = value
        .as_object()
        .ok_or("Invalid ChatGPT OAuth status response")?;
    if object.len() != 7
        || ![
            "supported",
            "pending",
            "signInVerified",
            "sessionLinked",
            "planEnabled",
        ]
        .iter()
        .all(|key| object.get(*key).is_some_and(Value::is_boolean))
        || !object
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|s| {
                matches!(
                    s,
                    "signed_out" | "authorizing" | "verified" | "linked" | "error"
                )
            })
        || !object
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|s| s.len() <= 500)
    {
        return Err("Invalid ChatGPT OAuth status response".into());
    }
    let authorized = value["signInVerified"] == true;
    let linked = value["sessionLinked"] == true;
    let plan = value["planEnabled"] == true;
    if (linked && (!authorized || !plan))
        || (plan && !authorized)
        || (value["state"] == "linked" && !linked)
        || (value["state"] == "verified" && (!authorized || linked))
        || (value["state"] == "signed_out" && authorized)
        || (value["state"] == "error" && linked)
    {
        return Err("Inconsistent ChatGPT OAuth authorization".into());
    }
    Ok(value)
}

#[cfg(target_os = "android")]
fn call(app: AppHandle, method: &str, user_initiated: bool) -> Result<Value, String> {
    let handle = app
        .try_state::<OAuthHandle<tauri::Wry>>()
        .ok_or("ChatGPT OAuth Android plugin unavailable")?;
    let result = handle
        .0
        .run_mobile_plugin(method, json!({"userInitiated":user_initiated}))
        .map_err(|_| "ChatGPT OAuth native request failed")?;
    validate_status(result)
}

#[cfg(not(target_os = "android"))]
fn call(_app: AppHandle, _method: &str, _user_initiated: bool) -> Result<Value, String> {
    Err("ChatGPT OAuth is available only in the Android app".into())
}

pub fn status(app: AppHandle) -> Result<Value, String> {
    call(app, "status", false)
}
pub fn start(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    if !user_initiated {
        return Err("ChatGPT OAuth requires an explicit user action".into());
    }
    call(app, "start", true)
}
pub fn bring(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    if !user_initiated {
        return Err("ChatGPT plan connection requires an explicit user action".into());
    }
    call(app, "bring", true)
}
pub fn signout(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    if !user_initiated {
        return Err("ChatGPT disconnect requires an explicit user action".into());
    }
    call(app, "signout", true)
}

/// The Android plugin owns tokens and network transport. The native result is
/// deliberately narrow: arbitrary JSON, cookies, and tokens are never passed
/// back to the renderer or persisted as chat history.
fn valid_chat_failure_reason(value: &Value) -> bool {
    value.as_str().is_some_and(|reason| {
        reason.len() <= 64
            && reason.is_ascii()
            && matches!(reason,
                "http_400" | "http_401" | "http_403" | "http_404"
                | "http_422" | "http_429" | "http_5xx" | "http_other"
                | "subscription_sharing_usage_limit_exceeded" | "usage_unavailable"
                | "model_unavailable" | "model_not_found" | "provider_failure"
                | "response_incomplete" | "response_cancelled"
                | "stream_incomplete" | "network_timeout" | "network_error"
                | "invalid_stream" | "invalid_stream_json" | "invalid_stream_html"
                | "invalid_stream_missing" | "invalid_stream_other"
                | "invalid_completion" | "empty_answer"
                | "invalid_request" | "busy" | "unknown_failure")
    })
}

fn validate_chat_result(result: Value, request_id: &str) -> Result<Value, String> {
    let object = result.as_object().ok_or("Invalid native OAuth chat receipt")?;
    if object.get("requestId").and_then(Value::as_str) != Some(request_id) {
        return Err("Native OAuth chat request identity mismatch".into());
    }
    match object.get("state").and_then(Value::as_str) {
        Some("completed") => {
            if object.keys().any(|key|
                !matches!(key.as_str(), "requestId" | "state" | "answer" | "model"))
                || !object.get("answer").and_then(Value::as_str)
                    .is_some_and(|answer| !answer.trim().is_empty() && answer.len() <= 20_000)
                || object.get("model").is_some_and(|model| !model.as_str()
                    .is_some_and(|text| !text.is_empty() && text.len() <= 120
                        && text.bytes().all(|byte| byte.is_ascii_alphanumeric()
                            || b"._:/-".contains(&byte))))
            {
                return Err("Native OAuth completion proof is invalid".into());
            }
        }
        Some("failed_before_dispatch" | "uncertain") => {
            if object.keys().any(|key|
                !matches!(key.as_str(), "requestId" | "state" | "reason"))
                || object.get("reason").is_some_and(|value| !valid_chat_failure_reason(value))
            {
                return Err("Native OAuth failure receipt contains unexpected data".into());
            }
        }
        _ => return Err("Native OAuth chat state is invalid".into()),
    }
    Ok(result)
}

pub fn chat(app: AppHandle, request_id: String, prompt: String,
    context: Vec<Message>, user_initiated: bool) -> Result<Value, String>
{
    if !user_initiated {
        return Err("OAuth Send requires explicit user approval".into());
    }
    if request_id.len() != 36 || prompt.trim().is_empty() || prompt.len() > 20_000
        || context.len() > 16 || context.iter().any(|message|
            !matches!(message.role.as_str(), "user" | "assistant")
                || message.text.trim().is_empty() || message.text.len() > 20_000)
    {
        return Err("Invalid native OAuth chat request".into());
    }
    #[cfg(target_os = "android")]
    {
        let native_context: Vec<Value> = context.iter().map(|message|
            json!({"role":message.role,"content":message.text})
        ).collect();
        let handle = app.try_state::<OAuthHandle<tauri::Wry>>()
            .ok_or("ChatGPT OAuth Android plugin unavailable")?;
        let result = handle.0.run_mobile_plugin("chat", json!({
            "requestId":request_id,"prompt":prompt,
            "context":native_context,"userInitiated":true
        })).map_err(|_| "Native OAuth chat outcome uncertain; do not retry")?;
        validate_chat_result(result, &request_id)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, request_id, prompt, context);
        Err("ChatGPT OAuth chat is available only on Android".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn receipt(state: &str, verified: bool, linked: bool, plan: bool) -> Value {
        json!({
            "supported":true,"state":state,"pending":false,
            "signInVerified":verified,"sessionLinked":linked,
            "planEnabled":plan,"message":"Test"
        })
    }
    #[test]
    fn grant_must_be_coherent_and_bounded() {
        assert!(validate_status(receipt("signed_out", false, false, false)).is_ok());
        assert!(validate_status(receipt("authorizing", false, false, false)).is_ok());
        assert!(validate_status(receipt("verified", true, false, true)).is_ok());
        assert!(validate_status(receipt("linked", true, true, true)).is_ok());
        assert!(validate_status(receipt("linked", false, true, true)).is_err());
        assert!(validate_status(receipt("linked", true, true, false)).is_err());
        assert!(validate_status(receipt("verified", true, true, true)).is_err());
        assert!(validate_status(receipt("error", true, true, true)).is_err());
        assert!(validate_status(receipt("signed_out", true, false, false)).is_err());
        let mut forged = receipt("linked", true, true, true);
        forged["access_token"] = json!("must-not-leave-Android");
        assert!(validate_status(forged).is_err());
    }
    #[test]
    fn native_chat_receipt_requires_scoped_completion_without_secrets() {
        let id = "123e4567-e89b-42d3-a456-426614174000";
        let valid = json!({"requestId":id,"state":"completed","answer":"Hello","model":"gpt-5"});
        assert!(validate_chat_result(valid.clone(), id).is_ok());
        let mut leaked = valid.clone();
        leaked["accessToken"] = json!("secret");
        assert!(validate_chat_result(leaked,id).is_err());
        let mut empty = valid;
        empty["answer"] = json!("  ");
        assert!(validate_chat_result(empty,id).is_err());
        assert!(validate_chat_result(json!({"requestId":id,"state":"uncertain"}),id).is_ok());
        for state in ["failed_before_dispatch", "uncertain"] {
            for reason in [
                "http_400", "http_401", "http_403", "http_404",
                "http_422", "http_429", "http_5xx", "http_other",
                "subscription_sharing_usage_limit_exceeded", "usage_unavailable",
                "model_unavailable", "model_not_found", "provider_failure",
                "response_incomplete", "response_cancelled", "stream_incomplete",
                "network_timeout", "network_error", "invalid_stream",
                "invalid_completion", "empty_answer", "invalid_request", "busy",
                "unknown_failure"
            ] {
                assert!(validate_chat_result(
                    json!({"requestId":id,"state":state,"reason":reason}), id
                ).is_ok(), "{state}: {reason}");
            }
            for malicious in [
                json!("HTTP 401 token abc"), json!("http_402"),
                json!("usage_limit"), json!("unknown"),
                json!("network_error\nsecret"), json!("SECRET"), json!(""),
                json!(null), json!(123), json!("a".repeat(65))
            ] {
                assert!(validate_chat_result(
                    json!({"requestId":id,"state":state,"reason":malicious}), id
                ).is_err(), "{state}: {malicious}");
            }
            assert!(validate_chat_result(json!({
                "requestId":id,"state":state,"reason":"unknown_failure","accessToken":"secret"
            }),id).is_err());
        }
        assert!(validate_chat_result(json!({"requestId":id,"state":"failed_before_dispatch",
            "answer":"fake"}),id).is_err());
        assert!(validate_chat_result(json!({
            "requestId":id,"state":"completed","answer":"Hello","reason":"unknown_failure"
        }),id).is_err());
        assert!(validate_chat_result(json!({"requestId":"wrong","state":"completed",
            "answer":"hello"}),id).is_err());
    }
}
