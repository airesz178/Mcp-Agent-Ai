use serde_json::{json, Value};
use tauri::AppHandle;

const OPTIONAL_PERMISSION_KEYS: [&str; 3] = ["camera", "microphone", "location"];

fn valid_optional_permission(permission: &str) -> bool {
    OPTIONAL_PERMISSION_KEYS.contains(&permission)
}

fn validate_optional_status(value: Value) -> Result<Value, String> {
    let obj = value
        .as_object()
        .ok_or("Invalid Android optional permission status")?;
    if obj.len() != OPTIONAL_PERMISSION_KEYS.len()
        || OPTIONAL_PERMISSION_KEYS.iter().any(|key| {
            let Some(status) = obj.get(*key).and_then(Value::as_object) else {
                return true;
            };
            status.len() != 2
                || !status.get("supported").is_some_and(Value::is_boolean)
                || !status.get("granted").is_some_and(Value::is_boolean)
                || (status.get("supported") == Some(&Value::Bool(false))
                    && status.get("granted") == Some(&Value::Bool(true)))
        })
    {
        return Err("Invalid Android optional permission status".into());
    }
    Ok(value)
}

fn validate_optional_request(value: Value) -> Result<Value, String> {
    let obj = value
        .as_object()
        .ok_or("Invalid Android optional permission prompt result")?;
    // `opened` only says the OS request was issued. It cannot confer consent.
    if obj.len() != 1 || !obj.get("opened").is_some_and(Value::is_boolean) {
        return Err("Invalid Android optional permission prompt result".into());
    }
    Ok(value)
}

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

#[cfg(target_os = "android")]
struct AndroidStorage<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("storage-access")
        .setup(|app, api| {
            let plugin = api.register_android_plugin("ai.mcpagent.app", "StorageAccessPlugin")?;
            app.manage(AndroidStorage(plugin));
            Ok(())
        })
        .build()
}

pub fn query(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        let plugin = app.state::<AndroidStorage<tauri::Wry>>();
        return plugin
            .0
            .run_mobile_plugin("status", json!({}))
            .map_err(|_| "Android storage permission status unavailable".to_string());
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(json!({ "supported": false, "granted": false }))
    }
}

pub fn request(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        let plugin = app.state::<AndroidStorage<tauri::Wry>>();
        return plugin
            .0
            .run_mobile_plugin("request", json!({}))
            .map_err(|_| "Could not open Android file access settings".to_string());
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err("All files access is available on Android only".to_string())
    }
}

pub fn query_permissions(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        let plugin = app.state::<AndroidStorage<tauri::Wry>>();
        return plugin
            .0
            .run_mobile_plugin("permissionsStatus", json!({}))
            .map_err(|_| "Android permission status unavailable".to_string());
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(json!({
            "files": {"supported":false,"granted":false},
            "overlay": {"supported":false,"granted":false},
            "notifications": {"supported":false,"granted":false},
            "battery": {"supported":false,"granted":false}
        }))
    }
}

pub fn request_permission(app: AppHandle, permission: String) -> Result<Value, String> {
    if !matches!(
        permission.as_str(),
        "files" | "overlay" | "notifications" | "battery"
    ) {
        return Err("Unsupported Android permission request".into());
    }
    #[cfg(target_os = "android")]
    {
        let plugin = app.state::<AndroidStorage<tauri::Wry>>();
        return plugin
            .0
            .run_mobile_plugin("permissionsRequest", json!({"permission": permission}))
            .map_err(|_| "Could not open Android permission settings".to_string());
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, permission);
        Err("Android permissions are available on Android only".into())
    }
}

/// A separate optional contract keeps the four original required OS statuses
/// unchanged. An unknown or malformed native response is never a grant.
pub fn query_optional_permissions(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        let plugin = app.state::<AndroidStorage<tauri::Wry>>();
        let result = plugin
            .0
            .run_mobile_plugin("optionalPermissionsStatus", json!({}))
            .map_err(|_| "Android optional permission status unavailable".to_string())?;
        return validate_optional_status(result);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        validate_optional_status(json!({
            "camera": {"supported":false,"granted":false},
            "microphone": {"supported":false,"granted":false},
            "location": {"supported":false,"granted":false}
        }))
    }
}

/// Only a direct Allow interaction may launch Android's runtime prompt.
/// Checking the boolean `opened` is never sufficient to mark a grant.
pub fn request_optional_permission(
    app: AppHandle,
    permission: String,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated {
        return Err("Explicit user action required for optional permission".into());
    }
    if !valid_optional_permission(&permission) {
        return Err("Unsupported optional Android permission".into());
    }
    #[cfg(target_os = "android")]
    {
        let plugin = app.state::<AndroidStorage<tauri::Wry>>();
        let result = plugin
            .0
            .run_mobile_plugin(
                "optionalPermissionsRequest",
                json!({"permission":permission,"userInitiated":user_initiated}),
            )
            .map_err(|_| "Android optional permission prompt unavailable".to_string())?;
        return validate_optional_request(result);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, permission);
        Err("Optional runtime permissions are available on Android only".into())
    }
}

#[cfg(test)]
mod optional_permissions_tests {
    use super::*;

    #[test]
    fn only_three_optional_permissions_are_allowlisted() {
        for permission in OPTIONAL_PERMISSION_KEYS {
            assert!(valid_optional_permission(permission));
        }
        for permission in [
            "",
            "Camera",
            "CAMERA",
            "microPhone",
            "location_fine",
            "fine_location",
            "ACCESS_FINE_LOCATION",
            "files",
            "battery",
            "overlay",
            "notifications",
            "android.permission.CAMERA",
            "camera/../files",
            "camera\nlocation",
        ] {
            assert!(
                !valid_optional_permission(permission),
                "unexpected {permission}"
            );
        }
    }

    #[test]
    fn optional_status_is_exactly_three_independent_os_booleans() {
        let baseline = json!({
            "camera": {"supported":true,"granted":false},
            "microphone": {"supported":true,"granted":true},
            "location": {"supported":false,"granted":false}
        });
        assert_eq!(
            validate_optional_status(baseline.clone()).unwrap(),
            baseline
        );
        for corrupted in [
            json!({}),
            json!({"camera":{"supported":true,"granted":false}}),
            json!({"camera":{"supported":true,"granted":false},
                "microphone":{"supported":true,"granted":false},
                "location":{"supported":true,"granted":false},"files":{"granted":true}}),
            json!({"camera":{"supported":true,"granted":false},
                "microphone":{"supported":true,"granted":false},
                "location":{"supported":false,"granted":true}}),
            json!({"camera":{"supported":true,"granted":"true"},
                "microphone":{"supported":true,"granted":false},
                "location":{"supported":true,"granted":false}}),
            json!({"camera":{"supported":true,"granted":false,"opening":true},
                "microphone":{"supported":true,"granted":false},
                "location":{"supported":true,"granted":false}}),
            Value::Null,
        ] {
            assert!(validate_optional_status(corrupted).is_err());
        }
    }

    #[test]
    fn opening_the_system_prompt_never_claims_a_permission_grant() {
        for opened in [true, false] {
            let expected = json!({"opened":opened});
            assert_eq!(
                validate_optional_request(expected.clone()).unwrap(),
                expected
            );
        }
        for wrong in [
            json!({"opened":true,"granted":true}),
            json!({"opened":"true"}),
            json!({"granted":true}),
            json!({}),
            Value::Null,
        ] {
            assert!(validate_optional_request(wrong).is_err());
        }
    }
}
