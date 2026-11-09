//! Android Keystore-backed tunnel credentials, exposed only through narrow
//! Tauri commands. The only key-returning native method is private to Rust:
//! no command can return that secret to the renderer, status, or logs.
use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

#[cfg(target_os = "android")]
struct TunnelCredentialHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("tunnel-credentials")
        .setup(|app, api| {
            let plugin =
                api.register_android_plugin("ai.mcpagent.app", "TunnelCredentialPlugin")?;
            app.manage(TunnelCredentialHandle(plugin));
            Ok(())
        })
        .build()
}

const SECRET_MIN: usize = 8;
const SECRET_MAX: usize = 512;

fn valid_key(key: &str) -> bool {
    (SECRET_MIN..=SECRET_MAX).contains(&key.len())
        && key.bytes().all(|c| (0x21..=0x7e).contains(&c))
}

fn explicit(user_initiated: bool) -> Result<(), String> {
    if user_initiated {
        Ok(())
    } else {
        Err("Explicit user action required for tunnel credential change or use".into())
    }
}

fn public_status(receipt: Value, expected_stored: Option<bool>) -> Result<Value, String> {
    let object = receipt
        .as_object()
        .ok_or("Secure tunnel credential status unavailable")?;
    // Exact shape. A native plugin must never accidentally reveal key, nonce,
    // ciphertext, keystore alias, exception, or personal data through status.
    if object.len() != 2
        || object.get("secure") != Some(&Value::Bool(true))
        || !object.get("stored").is_some_and(Value::is_boolean)
    {
        return Err("Invalid Android secure tunnel credential receipt".into());
    }
    if expected_stored.is_some_and(|expected| receipt["stored"] != expected) {
        return Err("Android tunnel credential operation was not confirmed".into());
    }
    Ok(receipt)
}

fn private_key_receipt(receipt: Value) -> Result<String, String> {
    let fields = receipt
        .as_object()
        .ok_or("Android secure tunnel key unavailable")?;
    // This function is never called by an exposed Tauri command returning its
    // output. The secret only reaches the existing Rust tunnel connect() task.
    if fields.len() != 1 {
        return Err("Android secure tunnel key unavailable".into());
    }
    let key = fields
        .get("apiKey")
        .and_then(Value::as_str)
        .ok_or("Android secure tunnel key unavailable")?;
    if !valid_key(key) {
        return Err("Android secure tunnel key unavailable".into());
    }
    Ok(key.to_owned())
}

fn validate_destination(kind: &str) -> Result<(), String> {
    if matches!(kind, "tunnels" | "api_keys" | "chatgpt_plugins") {
        Ok(())
    } else {
        Err("Unsupported OpenAI Platform destination".into())
    }
}

fn platform_receipt(receipt: Value, expected_kind: &str) -> Result<Value, String> {
    let object = receipt
        .as_object()
        .ok_or("OpenAI Platform browser result unavailable")?;
    if object.len() != 2
        || object.get("opened") != Some(&Value::Bool(true))
        || object.get("kind").and_then(Value::as_str) != Some(expected_kind)
    {
        return Err("OpenAI Platform browser launch was not confirmed".into());
    }
    Ok(receipt)
}

#[cfg(target_os = "android")]
fn native_call(app: AppHandle, method: &str, args: Value) -> Result<Value, String> {
    let handle = app
        .try_state::<TunnelCredentialHandle<tauri::Wry>>()
        .ok_or("Android secure tunnel credential service unavailable")?;
    // Do not propagate Java/Kotlin exception messages, which might embed a
    // credential. Static errors only; never log the serialized request.
    handle
        .0
        .run_mobile_plugin(method, args)
        .map_err(|_| "Android secure tunnel credential service unavailable".to_owned())
}

#[cfg(not(target_os = "android"))]
fn native_call(_app: AppHandle, _method: &str, _args: Value) -> Result<Value, String> {
    Err("Secure tunnel credential storage is Android-only".into())
}

fn status_sync(app: AppHandle) -> Result<Value, String> {
    public_status(native_call(app, "status", json!({}))?, None)
}

fn store_sync(app: AppHandle, api_key: String) -> Result<Value, String> {
    if !valid_key(&api_key) {
        return Err("Invalid secure tunnel key".into());
    }
    public_status(
        native_call(app, "store", json!({"apiKey":api_key,"userInitiated":true}))?,
        Some(true),
    )
}

fn remove_sync(app: AppHandle) -> Result<Value, String> {
    public_status(
        native_call(app, "remove", json!({"userInitiated":true}))?,
        Some(false),
    )
}

/// Private ONLY: Rust's connect_stored uses this; a Tauri command never
/// publishes either this result or its raw Android plugin receipt.
pub(crate) fn load_for_connect(app: AppHandle) -> Result<String, String> {
    private_key_receipt(native_call(
        app,
        "readForConnect",
        json!({"userInitiated":true}),
    )?)
}

fn open_platform_sync(app: AppHandle, kind: String) -> Result<Value, String> {
    validate_destination(&kind)?;
    platform_receipt(
        native_call(
            app,
            "openPlatformPage",
            json!({"kind":kind,"userInitiated":true}),
        )?,
        &kind,
    )
}

#[tauri::command]
pub async fn android_tunnel_credentials_status(app: AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || status_sync(app))
        .await
        .map_err(|_| "Android tunnel credential status interrupted".to_owned())?
}

#[tauri::command]
pub async fn android_tunnel_credentials_store(
    app: AppHandle,
    api_key: String,
    user_initiated: bool,
) -> Result<Value, String> {
    explicit(user_initiated)?;
    if !valid_key(&api_key) {
        return Err("Invalid secure tunnel key".into());
    }
    tauri::async_runtime::spawn_blocking(move || store_sync(app, api_key))
        .await
        .map_err(|_| "Android tunnel credential storage interrupted".to_owned())?
}

#[tauri::command]
pub async fn android_tunnel_credentials_remove(
    app: AppHandle,
    tunnel: tauri::State<'_, crate::android_openai_tunnel::AndroidOpenAiTunnel>,
    user_initiated: bool,
) -> Result<Value, String> {
    explicit(user_initiated)?;
    let receipt = tauri::async_runtime::spawn_blocking(move || remove_sync(app))
        .await
        .map_err(|_| "Android tunnel credential removal interrupted".to_owned())??;
    // A running tunnel may still be using the now-deleted key from its
    // cancellable task. Explicit removal also stops that task. Removing the
    // local key never revokes the key at the OpenAI Platform.
    tunnel.disconnect()?;
    Ok(receipt)
}

#[tauri::command]
pub async fn android_tunnel_open_platform_page(
    app: AppHandle,
    kind: String,
    user_initiated: bool,
) -> Result<Value, String> {
    explicit(user_initiated)?;
    validate_destination(&kind)?;
    tauri::async_runtime::spawn_blocking(move || open_platform_sync(app, kind))
        .await
        .map_err(|_| "OpenAI Platform browser request interrupted".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_status_only_accepts_exact_secure_boolean_receipt() {
        assert_eq!(
            public_status(json!({"stored":true,"secure":true}), Some(true)).unwrap(),
            json!({"stored":true,"secure":true})
        );
        assert_eq!(
            public_status(json!({"stored":false,"secure":true}), Some(false)).unwrap(),
            json!({"stored":false,"secure":true})
        );
        for receipt in [
            json!({"stored":true}),
            json!({"stored":true,"secure":false}),
            json!({"stored":"true","secure":true}),
            json!({"stored":true,"secure":true,"apiKey":"sk-DO-NOT-EXPOSE"}),
            json!({"stored":true,"secure":true,"ciphertext":"secret"}),
            json!({}),
            Value::Null,
        ] {
            assert!(public_status(receipt, None).is_err());
        }
        assert!(public_status(json!({"stored":false,"secure":true}), Some(true)).is_err());
        assert!(public_status(json!({"stored":true,"secure":true}), Some(false)).is_err());
    }

    #[test]
    fn private_decrypt_receipt_never_accepts_unbounded_or_extra_fields() {
        assert_eq!(
            private_key_receipt(json!({"apiKey":"sk-test-key-1234"})).unwrap(),
            "sk-test-key-1234"
        );
        for receipt in [
            json!({}),
            json!({"apiKey":"sk-abc\r\nInjected: true"}),
            json!({"apiKey":"sk-abc with spaces"}),
            json!({"apiKey":"short"}),
            json!({"apiKey":"sk-valid-key","encrypted":"leak"}),
            json!({"apiKey":"x".repeat(513)}),
            Value::Null,
        ] {
            let error = private_key_receipt(receipt).unwrap_err();
            assert!(!error.contains("sk-"));
            assert!(!error.contains("Injected"));
        }
        assert!(!valid_key("sk-key with spaces"));
        assert!(valid_key("sk-abcdefghijklmnop"));
    }

    #[test]
    fn consent_and_browser_destinations_are_strict() {
        assert!(explicit(false).is_err());
        assert!(explicit(true).is_ok());
        for bad in [
            "",
            "https://evil.test",
            "api_keys/../tunnels",
            "API_KEYS",
            "tunnels?x=1",
        ] {
            assert!(validate_destination(bad).is_err());
        }
        for good in ["tunnels", "api_keys", "chatgpt_plugins"] {
            assert!(validate_destination(good).is_ok());
            assert_eq!(
                platform_receipt(json!({"opened":true,"kind":good}), good).unwrap(),
                json!({"opened":true,"kind":good})
            );
        }
        assert!(platform_receipt(
            json!({"opened":true,"kind":"tunnels","url":"https://evil.test"}),
            "tunnels"
        )
        .is_err());
        assert!(platform_receipt(json!({"opened":false,"kind":"tunnels"}), "tunnels").is_err());
    }
}
