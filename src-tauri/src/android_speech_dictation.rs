//! Explicit, foreground Android speech-to-text bridge. Transcriptions are
//! returned only to the requesting UI action; never automatically sent,
//! persisted, logged or uploaded by this bridge.
//!
//! Prime integration (not part of this file's ownership):
//! - `mod android_speech_dictation;`
//! - On Android `.plugin(android_speech_dictation::init())`
//! - `tauri::generate_handler![...,
//!      android_speech_dictation::speech_dictation_status,
//!      android_speech_dictation::speech_dictation_recognize]`
//! - Stage `android/SpeechDictationPlugin.kt` in package `ai.mcpagent.app`.

use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

const SOURCE: &str = "Android speech recognizer";
const MAX_TRANSCRIPT_BYTES: usize = 2_000;

#[cfg(target_os = "android")]
struct SpeechDictationHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("speech-dictation")
        .setup(|app, api| {
            let plugin = api.register_android_plugin("ai.mcpagent.app", "SpeechDictationPlugin")?;
            app.manage(SpeechDictationHandle(plugin));
            Ok(())
        })
        .build()
}

/// Demand a *canonical* lowercase hyphenated RFC 4122 version-4 UUID. A
/// provider receipt must echo this exact caller-generated ID to avoid a stale
/// earlier recognition response satisfying a different request.
fn validate_request_id(request_id: &str) -> Result<(), String> {
    let bytes = request_id.as_bytes();
    if bytes.len() != 36
        || [8, 13, 18, 23].iter().any(|&index| bytes[index] != b'-')
        || bytes.iter().enumerate().any(|(index, &byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                false
            } else {
                !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase()
            }
        })
        || bytes[14] != b'4'
        || !matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
    {
        return Err("Speech dictation requires a canonical lowercase UUID v4 request ID".into());
    }
    Ok(())
}

fn require_recognition_action(request_id: &str, user_initiated: bool) -> Result<(), String> {
    if !user_initiated {
        return Err("Speech recognition requires an explicit user action".into());
    }
    validate_request_id(request_id)
}

/// Reject invisibles, noncharacters, private-use codepoints and bidi overrides
/// while preserving ordinary visible UTF-8 text (including accents and emoji).
/// No sanitizing/trimming takes place: a problematic provider payload fails.
fn forbidden_transcript_char(ch: char) -> bool {
    let code = ch as u32;
    ch.is_control()
        || ch.is_whitespace() && ch != ' '
        || (code & 0xFFFE) == 0xFFFE
        || matches!(code,
            0x00AD | 0x034F | 0x061C | 0x115F..=0x1160 |
            0x17B4..=0x17B5 | 0x180B..=0x180F |
            0x200B..=0x200F | 0x202A..=0x202E |
            0x2060..=0x206F | 0x2800 | 0x3164 |
            0xE000..=0xF8FF | 0xFDD0..=0xFDEF |
            0xFE00..=0xFE0F | 0xFEFF | 0xFFA0 |
            0xFFF9..=0xFFFB | 0xF0000..=0xFFFFD |
            0x100000..=0x10FFFD | 0xE0000..=0xE0FFF)
}

/// Status is observational only. No implied microphone grant or recognition
/// success can be inferred from a host fallback or an unverified response.
/// Contract: exactly {supported,available,listening,source,autoSent}.
fn validate_status(receipt: &Value) -> Result<(), String> {
    let invalid = || "Android speech dictation returned invalid status".to_owned();
    let object = receipt.as_object().ok_or_else(invalid)?;
    if object.len() != 5
        || object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "supported" | "available" | "listening" | "source" | "autoSent"
            )
        })
        || receipt.get("source").and_then(Value::as_str) != Some(SOURCE)
        || receipt.get("autoSent") != Some(&Value::Bool(false))
    {
        return Err(invalid());
    }
    let supported = receipt
        .get("supported")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    let available = receipt
        .get("available")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    let listening = receipt
        .get("listening")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    if (available && !supported) || (listening && !available) {
        return Err(invalid());
    }
    Ok(())
}

/// Recognition receipt: exactly six keys. `recognized` and `cancelled` must
/// be true/false mutually exclusive; no third state is silently promoted to a
/// recognized transcript. Text is present but empty for cancellation.
fn validate_recognition(receipt: &Value, expected_request_id: &str) -> Result<(), String> {
    validate_request_id(expected_request_id)?;
    let invalid = || "Android speech dictation returned invalid or stale recognition".to_owned();
    let object = receipt.as_object().ok_or_else(invalid)?;
    if object.len() != 6
        || object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "requestId" | "recognized" | "cancelled" | "text" | "source" | "autoSent"
            )
        })
        || receipt.get("requestId").and_then(Value::as_str) != Some(expected_request_id)
        || receipt.get("source").and_then(Value::as_str) != Some(SOURCE)
        || receipt.get("autoSent") != Some(&Value::Bool(false))
    {
        return Err(invalid());
    }
    let recognized = receipt
        .get("recognized")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    let cancelled = receipt
        .get("cancelled")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    let text = receipt
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    if recognized == cancelled
        || text.len() > MAX_TRANSCRIPT_BYTES
        || (recognized && (text.is_empty() || text.trim().is_empty()))
        || (cancelled && !text.is_empty())
        || text.chars().any(forbidden_transcript_char)
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(target_os = "android")]
fn call(app: AppHandle, command: &str, args: Value) -> Result<Value, String> {
    let plugin = app
        .try_state::<SpeechDictationHandle<tauri::Wry>>()
        .ok_or_else(|| "Android speech dictation plugin is not registered".to_owned())?;
    plugin
        .0
        .run_mobile_plugin(command, args)
        .map_err(|_| "Android speech dictation unavailable, denied or cancelled".to_owned())
}

#[cfg(not(target_os = "android"))]
fn call(_app: AppHandle, _command: &str, _args: Value) -> Result<Value, String> {
    Err("Speech dictation is supported only by the Android application".into())
}

/// Read an actual bounded Android recognizer status; no synthesized success on
/// Windows or when the Android plugin cannot answer.
#[tauri::command]
pub async fn speech_dictation_status(app: AppHandle) -> Result<Value, String> {
    let receipt = tauri::async_runtime::spawn_blocking(move || call(app, "status", json!({})))
        .await
        .map_err(|_| "Android speech dictation status interrupted".to_owned())??;
    validate_status(&receipt)?;
    Ok(receipt)
}

/// A visible user gesture must supply a fresh canonical UUID v4. The Android
/// recognizer may show its own user-facing UI and must return text for review;
/// no recognition result is automatically sent as a chat message.
///
/// Call: invoke("speech_dictation_recognize",
///     {requestId:"...uuid-v4...",userInitiated:true})
/// Success: {requestId,recognized:true,cancelled:false,text,source,autoSent:false}
/// Cancel:  {requestId,recognized:false,cancelled:true,text:"",source,autoSent:false}
#[tauri::command]
pub async fn speech_dictation_recognize(
    app: AppHandle,
    request_id: String,
    user_initiated: bool,
) -> Result<Value, String> {
    require_recognition_action(&request_id, user_initiated)?;
    let expected_request_id = request_id.clone();
    let receipt = tauri::async_runtime::spawn_blocking(move || {
        call(
            app,
            "recognize",
            json!({"requestId": request_id,"userInitiated":true}),
        )
    })
    .await
    .map_err(|_| "Android speech recognition interrupted".to_owned())??;
    validate_recognition(&receipt, &expected_request_id)?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "94de94de-f21a-4ee0-9db2-8e4d42f2c904";
    const STALE: &str = "a1ee6b30-4895-4fe1-886d-5c1976cd6f8f";

    fn recognized(text: &str) -> Value {
        json!({"requestId":ID,"recognized":true,"cancelled":false,
            "text":text,"source":SOURCE,"autoSent":false})
    }

    fn cancelled() -> Value {
        json!({"requestId":ID,"recognized":false,"cancelled":true,
            "text":"","source":SOURCE,"autoSent":false})
    }

    #[test]
    fn only_exact_lowercase_canonical_v4_and_explicit_gesture() {
        assert!(require_recognition_action(ID, true).is_ok());
        assert!(require_recognition_action(ID, false).is_err());
        for invalid in [
            "",
            "94de94de-f21a-4ee0-9db2-8e4d42f2c90",
            "94DE94DE-F21A-4EE0-9DB2-8E4D42F2C904",
            "94de94de-f21a-3ee0-9db2-8e4d42f2c904",
            "94de94de-f21a-4ee0-7db2-8e4d42f2c904",
            "94de94de-f21a-4ee0-cdb2-8e4d42f2c904",
            "94de94de-f21a-4ee0-9db2-8e4d42f2c90g",
            "94de94def21a4ee09db28e4d42f2c904",
            " 94de94de-f21a-4ee0-9db2-8e4d42f2c904",
        ] {
            assert!(validate_request_id(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn strict_status_shape_and_state_consistency() {
        for valid in [
            json!({"supported":false,"available":false,"listening":false,"source":SOURCE,"autoSent":false}),
            json!({"supported":true,"available":false,"listening":false,"source":SOURCE,"autoSent":false}),
            json!({"supported":true,"available":true,"listening":true,"source":SOURCE,"autoSent":false}),
            json!({"supported":true,"available":true,"listening":false,"source":SOURCE,"autoSent":false}),
        ] {
            assert!(validate_status(&valid).is_ok(), "{valid}");
        }
        for invalid in [
            json!(null),
            json!([]),
            json!({}),
            json!({"supported":true,"available":false,"listening":true,"source":SOURCE,"autoSent":false}),
            json!({"supported":false,"available":true,"listening":false,"source":SOURCE,"autoSent":false}),
            json!({"supported":true,"available":true,"listening":false,"source":SOURCE,"autoSent":true}),
            json!({"supported":true,"available":true,"listening":false,"source":"fake","autoSent":false}),
            json!({"supported":true,"available":true,"listening":false,"source":SOURCE,"autoSent":false,"injected":true}),
            json!({"supported":1,"available":true,"listening":false,"source":SOURCE,"autoSent":false}),
        ] {
            assert!(validate_status(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn valid_recognition_and_native_cancel_have_exact_distinct_outcomes() {
        assert!(validate_recognition(&recognized("Hello, dunia 🌏"), ID).is_ok());
        assert!(validate_recognition(&recognized("École — 你好!"), ID).is_ok());
        assert!(validate_recognition(&recognized(&"é".repeat(1000)), ID).is_ok());
        assert!(validate_recognition(&cancelled(), ID).is_ok());
        let max = format!("{}😊", "x".repeat(MAX_TRANSCRIPT_BYTES - 4));
        assert_eq!(max.len(), MAX_TRANSCRIPT_BYTES);
        assert!(validate_recognition(&recognized(&max), ID).is_ok());
    }

    #[test]
    fn rejects_stale_missing_conflicting_or_injected_receipts() {
        let mut stale = recognized("hello");
        stale["requestId"] = json!(STALE);
        assert!(validate_recognition(&stale, ID).is_err());
        let mut wrong_version = recognized("hello");
        wrong_version["requestId"] = json!("94de94de-f21a-3ee0-9db2-8e4d42f2c904");
        assert!(validate_recognition(&wrong_version, ID).is_err());
        let mut injected = recognized("hello");
        injected["autoSend"] = json!(true);
        assert!(validate_recognition(&injected, ID).is_err());
        let mut missing = recognized("hello");
        missing.as_object_mut().unwrap().remove("cancelled");
        assert!(validate_recognition(&missing, ID).is_err());
        let mut fake_source = recognized("hello");
        fake_source["source"] = json!("Browser speech");
        assert!(validate_recognition(&fake_source, ID).is_err());
        let mut auto_sent = recognized("hello");
        auto_sent["autoSent"] = json!(true);
        assert!(validate_recognition(&auto_sent, ID).is_err());
        for flags in [(true, true), (false, false), (false, true)] {
            let mut contradictory = recognized("hello");
            contradictory["recognized"] = json!(flags.0);
            contradictory["cancelled"] = json!(flags.1);
            if flags == (false, true) {
                contradictory["text"] = json!("not empty");
            }
            assert!(validate_recognition(&contradictory, ID).is_err());
        }
        assert!(validate_recognition(&json!({}), ID).is_err());
        assert!(validate_recognition(&json!("hello"), ID).is_err());
    }

    #[test]
    fn rejects_oversized_empty_control_invisible_or_non_utf8_equivalent_text() {
        assert!(validate_recognition(&recognized(""), ID).is_err());
        assert!(validate_recognition(&recognized("    "), ID).is_err());
        assert!(
            validate_recognition(&recognized(&"a".repeat(MAX_TRANSCRIPT_BYTES + 1)), ID).is_err()
        );
        assert!(validate_recognition(&recognized(&"😊".repeat(501)), ID).is_err());
        for text in [
            "hi\nthere",
            "hi\tthere",
            "hi\rthere",
            "hi\0there",
            "hidden\u{200b}char",
            "bidi\u{202e}control",
            "join\u{200d}er",
            "override\u{2066}text",
            "private\u{e000}char",
            "nonchar\u{fdd0}",
            "variation\u{fe0f}selector",
            "space\u{00a0}hard",
        ] {
            assert!(
                validate_recognition(&recognized(text), ID).is_err(),
                "{text:?}"
            );
        }
        let mut cancelled_with_text = cancelled();
        cancelled_with_text["text"] = json!("bogus");
        assert!(validate_recognition(&cancelled_with_text, ID).is_err());
        let mut missing_text = cancelled();
        missing_text.as_object_mut().unwrap().remove("text");
        assert!(validate_recognition(&missing_text, ID).is_err());
    }
}
