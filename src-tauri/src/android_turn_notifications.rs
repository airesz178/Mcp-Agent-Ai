//! Optional one-shot Android notification, posted only after the UI has
//! confirmed a completed reply under an explicitly saved General preference.
//! The user_initiated argument is a *caller assertion* of that authorization;
//! the native bridge independently validates all payload bounds and current
//! Android notification permission before posting. No notification service or
//! background runtime is started.

use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

const MAX_MESSAGE_CHARS: usize = 2048;
const MAX_INPUT_BYTES: usize = 65_536;

#[cfg(target_os = "android")]
struct TurnCompletionHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("turn-completion")
        .setup(|app, api| {
            let plugin = api.register_android_plugin("ai.mcpagent.app", "TurnCompletionPlugin")?;
            app.manage(TurnCompletionHandle(plugin));
            Ok(())
        })
        .build()
}

fn failure(reason: &'static str) -> Value {
    json!({"posted": false, "reason": reason})
}

fn is_format_control(ch: char) -> bool {
    matches!(
        ch as u32,
        0x061C | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0xFEFF
    )
}

fn is_token_run(ch: u8) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-' | b'.' | b'+' | b'/' | b'=')
}

fn has_token_after(text: &str, prefix: &str, minimum: usize) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.match_indices(prefix).any(|(offset, _)| {
        if offset > 0
            && (lower.as_bytes()[offset - 1].is_ascii_alphanumeric()
                || lower.as_bytes()[offset - 1] == b'_')
        {
            return false;
        }
        lower.as_bytes()[offset + prefix.len()..]
            .iter()
            .take_while(|ch| is_token_run(**ch))
            .count()
            >= minimum
    })
}

/// Do not put common credentials in a system notification, even on a locked
/// phone. Reject the complete message if it resembles secret-bearing text.
fn contains_credential(text: &str) -> bool {
    for (prefix, minimum) in [
        ("sk-", 16),
        ("ghp_", 16),
        ("gho_", 16),
        ("ghu_", 16),
        ("ghs_", 16),
        ("ghr_", 16),
        ("github_pat_", 16),
        ("glpat-", 16),
        ("xoxb-", 12),
        ("xoxp-", 12),
        ("xoxa-", 12),
        ("xoxr-", 12),
        ("xoxs-", 12),
        ("aiza", 20),
    ] {
        if has_token_after(text, prefix, minimum) {
            return true;
        }
    }
    let lower = text.to_ascii_lowercase();
    if let Some(after) = lower.split("bearer ").nth(1) {
        if after.bytes().take_while(|ch| is_token_run(*ch)).count() >= 8 {
            return true;
        }
    }
    for key in [
        "api_key",
        "api-key",
        "apikey",
        "access_token",
        "access-token",
        "refresh_token",
        "refresh-token",
        "client_secret",
        "client-secret",
        "password",
        "authorization",
    ] {
        if let Some((_, value)) = lower.split_once(key) {
            let tail = value.trim_start_matches([' ', '\t', '\'', '"']);
            if let Some(rest) = tail.strip_prefix([':', '=']) {
                let secret = rest.trim_start_matches([' ', '\t', '\'', '"']);
                if secret
                    .bytes()
                    .take_while(|ch| {
                        !ch.is_ascii_whitespace()
                            && !matches!(ch, b'\'' | b'"' | b';' | b',' | b'`')
                    })
                    .count()
                    >= 6
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Strip harmless layout whitespace, reject other controls, and truncate
/// Unicode on char boundaries with a visible ellipsis. The Kotlin bridge
/// independently enforces the same 2,048-code-point display ceiling.
fn sanitize_message(input: &str) -> Result<String, &'static str> {
    if input.len() > MAX_INPUT_BYTES {
        return Err("unsafe_message");
    }
    let mut text = String::with_capacity(input.len().min(MAX_MESSAGE_CHARS * 4));
    let mut previous_space = false;
    for ch in input.chars() {
        if is_format_control(ch) || (ch.is_control() && !matches!(ch, '\r' | '\n' | '\t')) {
            return Err("unsafe_message");
        }
        if ch.is_whitespace() {
            if !previous_space && !text.is_empty() {
                text.push(' ');
                previous_space = true;
            }
        } else {
            text.push(ch);
            previous_space = false;
        }
    }
    let text = text.trim_end();
    if text.is_empty() || contains_credential(text) {
        return Err("unsafe_message");
    }
    let mut chars = text.chars();
    let mut bounded: String = chars.by_ref().take(MAX_MESSAGE_CHARS).collect();
    if chars.next().is_some() {
        bounded = bounded.chars().take(MAX_MESSAGE_CHARS - 1).collect();
        bounded = bounded.trim_end().to_string();
        bounded.push('…');
    }
    Ok(bounded)
}

fn make_payload(
    mode: &str,
    message: &str,
    notification_priority: &str,
    user_initiated: bool,
) -> Result<Value, &'static str> {
    if !user_initiated {
        return Err("not_user_initiated");
    }
    if !matches!(notification_priority, "foreground" | "background" | "both") {
        return Err("invalid_priority");
    }
    match mode {
        "full_message" => Ok(json!({
            "mode": "full_message",
            "message": sanitize_message(message)?,
            "notificationPriority": notification_priority,
            "userInitiated": true
        })),
        // Never forward a reply body in task_finished mode, even if supplied.
        "task_finished" => Ok(json!({
            "mode": "task_finished",
            "message": "",
            "notificationPriority": notification_priority,
            "userInitiated": true
        })),
        _ => Err("invalid_mode"),
    }
}

/// Never pass arbitrary native errors, payloads, credentials, or unexpected
/// fields back to the renderer as a successful completion notification.
fn validate_receipt(receipt: Value) -> Value {
    let Some(object) = receipt.as_object() else {
        return failure("invalid_native_receipt");
    };
    let Some(posted) = object.get("posted").and_then(Value::as_bool) else {
        return failure("invalid_native_receipt");
    };
    if posted {
        if object.len() == 1 {
            json!({"posted": true})
        } else {
            failure("invalid_native_receipt")
        }
    } else if object.len() == 2 {
        match object.get("reason").and_then(Value::as_str) {
            Some(reason)
                if matches!(
                    reason,
                    "invalid_request"
                        | "not_user_initiated"
                        | "invalid_mode"
                        | "invalid_priority"
                        | "priority_filtered"
                        | "unsafe_message"
                        | "activity_unavailable"
                        | "notification_unavailable"
                        | "permission_denied"
                        | "channel_disabled"
                ) =>
            {
                json!({"posted": false, "reason": reason})
            }
            _ => failure("invalid_native_receipt"),
        }
    } else {
        failure("invalid_native_receipt")
    }
}

#[cfg(target_os = "android")]
fn native_post(app: AppHandle, payload: Value) -> Value {
    let Some(handle) = app.try_state::<TurnCompletionHandle<tauri::Wry>>() else {
        return failure("native_unavailable");
    };
    match handle.0.run_mobile_plugin("notifyTurn", payload) {
        Ok(receipt) => validate_receipt(receipt),
        Err(_) => failure("native_unavailable"),
    }
}

#[cfg(not(target_os = "android"))]
fn native_post(_app: AppHandle, _payload: Value) -> Value {
    failure("unsupported_platform")
}

/// Call only after a confirmed reply under the user's saved General
/// preference. An unsolicited JS invocation with user_initiated=false is
/// denied. The native plugin cannot independently inspect the chat receipt.
#[tauri::command]
pub async fn android_turn_notification(
    app: AppHandle,
    mode: String,
    message: String,
    notification_priority: String,
    user_initiated: bool,
) -> Value {
    let payload = match make_payload(&mode, &message, &notification_priority, user_initiated) {
        Ok(payload) => payload,
        Err(reason) => return failure(reason),
    };
    match tauri::async_runtime::spawn_blocking(move || native_post(app, payload)).await {
        Ok(result) => result,
        Err(_) => failure("native_unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_requires_prior_user_authorization_and_known_mode() {
        assert_eq!(
            make_payload("full_message", "Answer", "both", false),
            Err("not_user_initiated")
        );
        assert_eq!(make_payload("off", "Answer", "both", true), Err("invalid_mode"));
        assert_eq!(
            make_payload("full_message", "", "both", true),
            Err("unsafe_message")
        );
        assert_eq!(
            make_payload("full_message", "Answer", "urgent", true),
            Err("invalid_priority")
        );
        for selected in ["foreground", "background", "both"] {
            let payload = make_payload("task_finished", "", selected, true).unwrap();
            assert_eq!(payload["notificationPriority"], selected);
        }
    }

    #[test]
    fn task_finished_never_forwards_reply_content() {
        let receipt = make_payload(
            "task_finished",
            "Bearer private_token_that_must_never_be_transmitted",
            "background",
            true,
        )
        .unwrap();
        assert_eq!(receipt["message"], "");
        assert_eq!(receipt["mode"], "task_finished");
        assert_eq!(receipt["notificationPriority"], "background");
    }

    #[test]
    fn full_message_normalizes_layout_and_respects_unicode_limit() {
        let receipt = make_payload("full_message", "Hi\r\n\tthere  🐱", "foreground", true).unwrap();
        assert_eq!(receipt["message"], "Hi there 🐱");
        let very_long = "🐱".repeat(2300);
        let bounded = sanitize_message(&very_long).unwrap();
        assert_eq!(bounded.chars().count(), MAX_MESSAGE_CHARS);
        assert!(bounded.ends_with('…'));
        assert_eq!(sanitize_message(&" ".repeat(100)), Err("unsafe_message"));
    }

    #[test]
    fn control_format_and_credential_like_strings_never_leave_bridge() {
        for input in [
            "OK\0not OK",
            "Hello\u{202E}evil",
            "Here is sk-proj-ABCDEFGHIJKLMNOPQRST",
            "Authorization: Bearer abcdefghijklmnopqrst",
            "api_key=abcdefghijklmnopqrstuvwxyz",
            "ghp_ABCDEFGHIJKLMNOPQRST",
        ] {
            assert_eq!(sanitize_message(input), Err("unsafe_message"));
        }
        assert!(sanitize_message(&"x".repeat(MAX_INPUT_BYTES + 1)).is_err());
    }

    #[test]
    fn native_receipt_is_bounded_to_known_status_only() {
        assert_eq!(
            validate_receipt(json!({"posted":true})),
            json!({"posted":true})
        );
        assert_eq!(
            validate_receipt(json!({"posted":false,"reason":"permission_denied"})),
            json!({"posted":false,"reason":"permission_denied"})
        );
        assert_eq!(
            validate_receipt(json!({"posted":false,"reason":"priority_filtered"})),
            json!({"posted":false,"reason":"priority_filtered"})
        );
        for invalid in [
            json!({"posted":true,"message":"leak"}),
            json!({"posted":false,"reason":"token abc"}),
            json!({"posted":true,"reason":"ok"}),
            json!({"posted":"true"}),
        ] {
            assert_eq!(validate_receipt(invalid), failure("invalid_native_receipt"));
        }
    }
}
