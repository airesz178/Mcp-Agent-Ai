//! Android-native foreground/unlocked proof and passive Plugins approval alert.
//! This bridge contains no authority to approve a call. It accepts only an
//! opaque UUID and absolute lease expiry, never args, origin or credentials.
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::AppHandle;
use uuid::Uuid;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

const MAX_EXPIRY_AHEAD_MS: u64 = 60_000;

#[cfg(target_os = "android")]
struct PluginsApprovalAlertHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub(crate) fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("plugins-approval-alert")
        .setup(|app, api| {
            let plugin =
                api.register_android_plugin("ai.mcpagent.app", "PluginsApprovalAlertPlugin")?;
            app.manage(PluginsApprovalAlertHandle(plugin));
            Ok(())
        })
        .build()
}

fn pending_payload(request_id: &str, expires_at_ms: u64, now_ms: u64) -> Option<Value> {
    Uuid::parse_str(request_id).ok()?;
    let remaining = expires_at_ms.checked_sub(now_ms)?;
    if remaining == 0 || remaining > MAX_EXPIRY_AHEAD_MS {
        return None;
    }
    Some(json!({"requestId":request_id,"expiresAtMs":expires_at_ms}))
}

fn valid_receipt(receipt: &Value, key: &str) -> bool {
    receipt
        .as_object()
        .is_some_and(|object| object.len() == 1 && object.get(key) == Some(&Value::Bool(true)))
}

fn now_millis() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

#[cfg(target_os = "android")]
fn run_native(app: &AppHandle, method: &str, payload: Value, expected: &str) -> bool {
    let Some(handle) = app.try_state::<PluginsApprovalAlertHandle<tauri::Wry>>() else {
        return false;
    };
    handle
        .0
        .run_mobile_plugin::<Value>(method, payload)
        .ok()
        .is_some_and(|receipt| valid_receipt(&receipt, expected))
}

#[cfg(not(target_os = "android"))]
fn run_native(_app: &AppHandle, _method: &str, _payload: Value, _expected: &str) -> bool {
    false
}

/// Non-authorizing and best effort. Only attempt a generic notification for a
/// live, bounded pending request; native Android independently checks whether
/// the Activity is paused/background and notification permission is enabled.
pub(crate) fn notify_pending(app: &AppHandle, request_id: &str, expires_at_ms: u64) -> bool {
    let Some(now) = now_millis() else {
        return false;
    };
    let Some(payload) = pending_payload(request_id, expires_at_ms, now) else {
        return false;
    };
    run_native(app, "notifyPending", payload, "posted")
}

/// Best-effort removal after decision, timeout or cancellation; never grant
/// permission, interact with a dialog, or reopen the Activity on its own.
pub(crate) fn dismiss_pending(app: &AppHandle, request_id: &str) {
    if Uuid::parse_str(request_id).is_err() {
        return;
    }
    let _ = run_native(
        app,
        "dismissPending",
        json!({"requestId":request_id}),
        "dismissed",
    );
}

/// Fail closed if no Android Activity is visibly resumed and focused, the
/// phone screen is inactive/locked, or the native bridge is unavailable.
/// This is an OS presence guard, NOT proof that a user granted a request.
pub(crate) fn foreground_unlocked(app: &AppHandle) -> bool {
    run_native(app, "foregroundUnlocked", json!({}), "foregroundUnlocked")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_notification_contains_only_opaque_identity_and_live_expiry() {
        let id = "4f456752-2fd4-47bc-becf-1132e63838ef";
        let payload = pending_payload(id, 10_022, 10_000).unwrap();
        assert_eq!(payload, json!({"requestId":id,"expiresAtMs":10_022}));
        assert!(pending_payload("https://example.org", 10_100, 10_000).is_none());
        assert!(pending_payload("", 10_100, 10_000).is_none());
        assert!(pending_payload(id, 10_000, 10_000).is_none());
        assert!(pending_payload(id, 9_999, 10_000).is_none());
        assert!(pending_payload(id, 70_001, 10_000).is_none());
        assert!(pending_payload(id, u64::MAX, 10_000).is_none());
    }

    #[test]
    fn native_receipts_must_be_single_true_boolean_or_fail_closed() {
        assert!(valid_receipt(
            &json!({"foregroundUnlocked":true}),
            "foregroundUnlocked"
        ));
        for invalid in [
            json!({"foregroundUnlocked":false}),
            json!({"foregroundUnlocked":"true"}),
            json!({"foregroundUnlocked":true,"extra":true}),
            json!({"posted":true}),
            json!(true),
            json!(null),
        ] {
            assert!(!valid_receipt(&invalid, "foregroundUnlocked"));
        }
    }
}
