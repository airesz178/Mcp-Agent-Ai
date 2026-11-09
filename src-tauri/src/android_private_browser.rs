//! Android-only, explicitly opened WebView browser with user-consented DOM
//! actions. This module is intentionally independent of desktop automation.
//! The Kotlin plugin owns all WebViews and validates navigation/action state.
//!
//! Prime wiring: `mod android_private_browser;`, register `init()` on Android,
//! and expose the public functions through Tauri `#[tauri::command]` wrappers.

use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

const MAX_URL: usize = 2048;
const MAX_INPUT: usize = 4096;
const MAX_ELEMENT_INDEX: u32 = 64;
const MAX_SCROLL: i32 = 2000;
const MAX_PREVIEW_PNG_BYTES: usize = 1024 * 1024;
const MAX_PREVIEW_BASE64_CHARS: usize = MAX_PREVIEW_PNG_BYTES.div_ceil(3) * 4;
const MAX_PREVIEW_DIMENSION: u64 = 1024;
const MAX_PREVIEW_PIXELS: u64 = 786_432;

#[cfg(target_os = "android")]
struct PrivateBrowserHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("private-browser")
        .setup(|app, api| {
            let plugin = api.register_android_plugin("ai.mcpagent.app", "PrivateBrowserPlugin")?;
            app.manage(PrivateBrowserHandle(plugin));
            Ok(())
        })
        .build()
}

fn check_https_url(url: &str) -> Result<(), String> {
    if url.len() < 9
        || url.len() > MAX_URL
        || !url.to_ascii_lowercase().starts_with("https://")
        || url.chars().any(char::is_whitespace)
        || url.chars().any(char::is_control)
    {
        return Err("Private browser requires an HTTPS URL".into());
    }
    let authority = url[8..].split(&['/', '?', '#'][..]).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') || authority.starts_with(':') {
        return Err("Private browser requires a valid HTTPS host".into());
    }
    Ok(())
}

fn check_tab(tab_id: i32) -> Result<(), String> {
    if tab_id < 1 {
        return Err("Invalid private browser tab".into());
    }
    Ok(())
}

fn check_action(tab_id: i32, snapshot_id: i64, index: i32, allow_page: bool) -> Result<(), String> {
    check_tab(tab_id)?;
    if snapshot_id < 1 || index < if allow_page { 0 } else { 1 } || index > MAX_ELEMENT_INDEX as i32
    {
        return Err("Invalid private browser snapshot action".into());
    }
    Ok(())
}

fn check_inspect_output(
    result: &Value,
    tab_id: i32,
    snapshot_id: i64,
    index: i32,
) -> Result<(), String> {
    const TAGS: &[&str] = &[
        "a", "button", "input", "textarea", "select", "div", "span", "label", "option",
    ];
    const ROLES: &[&str] = &[
        "",
        "button",
        "link",
        "textbox",
        "searchbox",
        "checkbox",
        "radio",
        "switch",
        "combobox",
        "listbox",
        "option",
        "menuitem",
        "tab",
        "slider",
        "spinbutton",
    ];
    const TYPES: &[&str] = &[
        "", "text", "search", "url", "email", "tel", "number", "checkbox", "radio", "button",
        "submit", "reset",
    ];
    if result.to_string().len() > 2048
        || result.get("ok") != Some(&Value::Bool(true))
        || result.get("tabId").and_then(Value::as_i64) != Some(i64::from(tab_id))
        || result.get("snapshotId").and_then(Value::as_i64) != Some(snapshot_id)
        || result.get("index").and_then(Value::as_i64) != Some(i64::from(index))
        || result.get("snapshotInvalidated") != Some(&Value::Bool(false))
        || result.as_object().is_none_or(|m| m.len() != 9)
    {
        return Err("Private browser inspection returned invalid identity or shape".into());
    }
    let tag = result.get("tag").and_then(Value::as_str).unwrap_or("");
    let role = result.get("role").and_then(Value::as_str).unwrap_or("");
    let label = result.get("label").and_then(Value::as_str).unwrap_or("");
    if !TAGS.contains(&tag)
        || !ROLES.contains(&role)
        || label.len() > 240
        || label.chars().count() > 120
        || label.chars().any(char::is_control)
    {
        return Err("Private browser inspection contained an invalid label or role".into());
    }
    let lowered = label.to_ascii_lowercase();
    if [
        "password",
        "passcode",
        "passphrase",
        "otp",
        "token",
        "secret",
        "bearer",
        "api key",
        "apikey",
        "authorization",
        "cookie",
        "session",
        "credential",
        "credit card",
        "cvv",
        "cvc",
    ]
    .iter()
    .any(|s| lowered.contains(s))
        || label
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '/' && c != '-')
            .any(|word| word.len() >= 18)
        || label
            .split(|c: char| !c.is_ascii_digit())
            .any(|number| number.len() >= 6)
        || label.contains('@')
    {
        return Err("Private browser inspection may contain sensitive text".into());
    }
    let attributes = result
        .get("attributes")
        .and_then(Value::as_object)
        .ok_or("Private browser inspection attributes invalid")?;
    if attributes.len() != 7
        || !TYPES.contains(
            &attributes
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("!"),
        )
        || [
            "disabled",
            "readOnly",
            "required",
            "ariaExpanded",
            "ariaPressed",
            "ariaChecked",
        ]
        .iter()
        .any(|k| attributes.get(*k).and_then(Value::as_bool).is_none())
    {
        return Err("Private browser inspection returned unrecognized attributes".into());
    }
    Ok(())
}

fn check_hover_output(result: &Value, tab_id: i32) -> Result<(), String> {
    if result.as_object().is_none_or(|m| m.len() != 5)
        || result.get("ok") != Some(&Value::Bool(true))
        || result.get("tabId").and_then(Value::as_i64) != Some(i64::from(tab_id))
        || result.get("snapshotInvalidated") != Some(&Value::Bool(true))
        || result.get("syntheticEvents") != Some(&Value::Bool(true))
        || result.get("physicalPointerMoved") != Some(&Value::Bool(false))
    {
        return Err("Private browser hover returned invalid event state".into());
    }
    Ok(())
}

/// An Android DOM mutation is acknowledged only by the exact, bounded native
/// tab receipt. An arbitrary object (or a stale response for another tab) is
/// not evidence that a requested action actually occurred.
fn check_mutation_output(result: &Value, tab_id: i32) -> Result<(), String> {
    if result.as_object().is_none_or(|fields| fields.len() != 3)
        || result.get("ok") != Some(&Value::Bool(true))
        || result.get("tabId").and_then(Value::as_i64) != Some(i64::from(tab_id))
        || result.get("snapshotInvalidated") != Some(&Value::Bool(true))
    {
        return Err("Private browser action receipt has invalid tab or state".into());
    }
    Ok(())
}

fn check_preview_request(tab_id: i32, user_initiated: bool) -> Result<(), String> {
    check_tab(tab_id)?;
    if !user_initiated {
        return Err("Private browser preview requires explicit user action".into());
    }
    Ok(())
}

fn check_preview_token(token: &str) -> Result<(), String> {
    let bytes = token.as_bytes();
    if bytes.len() != 36
        || bytes.iter().enumerate().any(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                *b != b'-'
            } else {
                !b.is_ascii_hexdigit() || b.is_ascii_uppercase()
            }
        })
    {
        return Err("Invalid private browser preview token".into());
    }
    Ok(())
}

fn decode_preview_base64(input: &str) -> Result<Vec<u8>, String> {
    if input.is_empty() || input.len() > MAX_PREVIEW_BASE64_CHARS || input.len() % 4 != 0 {
        return Err("Private browser PNG encoding is invalid".into());
    }
    let bytes = input.as_bytes();
    let mut result = Vec::with_capacity(input.len() / 4 * 3);
    for (chunk_index, quartet) in bytes.chunks_exact(4).enumerate() {
        let last = chunk_index == bytes.len() / 4 - 1;
        let decode = |b: u8| -> Option<u8> {
            match b {
                b'A'..=b'Z' => Some(b - b'A'),
                b'a'..=b'z' => Some(b - b'a' + 26),
                b'0'..=b'9' => Some(b - b'0' + 52),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        };
        let a = decode(quartet[0]).ok_or("Invalid private browser PNG base64")?;
        let b = decode(quartet[1]).ok_or("Invalid private browser PNG base64")?;
        let c = if quartet[2] == b'=' {
            if !last || quartet[3] != b'=' || (b & 0x0f) != 0 {
                return Err("Invalid private browser PNG base64 padding".into());
            }
            None
        } else {
            Some(decode(quartet[2]).ok_or("Invalid private browser PNG base64")?)
        };
        let d = if quartet[3] == b'=' {
            if !last || c.is_some_and(|n| n & 3 != 0) {
                return Err("Invalid private browser PNG base64 padding".into());
            }
            None
        } else {
            Some(decode(quartet[3]).ok_or("Invalid private browser PNG base64")?)
        };
        if d.is_some() && c.is_none() {
            return Err("Invalid private browser PNG base64 padding".into());
        }
        result.push((a << 2) | (b >> 4));
        if let Some(c) = c {
            result.push((b << 4) | (c >> 2));
            if let Some(d) = d {
                result.push((c << 6) | d);
            }
        }
    }
    if result.len() > MAX_PREVIEW_PNG_BYTES {
        return Err("Private browser preview exceeds PNG byte limit".into());
    }
    Ok(result)
}

fn png_crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb88320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn inspect_preview_png(png: &[u8], width: u64, height: u64) -> Result<(), String> {
    if png.len() < 57 || png.len() > MAX_PREVIEW_PNG_BYTES || !png.starts_with(b"\x89PNG\r\n\x1a\n")
    {
        return Err("Private browser preview is not a bounded PNG".into());
    }
    let mut pos = 8usize;
    let mut saw_header = false;
    let mut saw_pixels = false;
    let mut saw_end = false;
    let mut chunks = 0;
    while pos + 12 <= png.len() && chunks < 128 {
        let size = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        let chunk_end = pos
            .checked_add(12)
            .and_then(|n| n.checked_add(size))
            .filter(|n| *n <= png.len())
            .ok_or("Invalid private browser PNG chunk")?;
        let kind = &png[pos + 4..pos + 8];
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            return Err("Invalid private browser PNG chunk type".into());
        }
        let checksum = u32::from_be_bytes(png[chunk_end - 4..chunk_end].try_into().unwrap());
        if png_crc32(&png[pos + 4..chunk_end - 4]) != checksum {
            return Err("Invalid private browser PNG checksum".into());
        }
        if !saw_header {
            if kind != b"IHDR"
                || size != 13
                || u64::from(u32::from_be_bytes(
                    png[pos + 8..pos + 12].try_into().unwrap(),
                )) != width
                || u64::from(u32::from_be_bytes(
                    png[pos + 12..pos + 16].try_into().unwrap(),
                )) != height
                || png[pos + 16] != 8
                || !matches!(png[pos + 17], 2 | 6)
                || png[pos + 18..pos + 21] != [0, 0, 0]
            {
                return Err("Invalid private browser PNG dimensions or format".into());
            }
            saw_header = true;
        } else if kind == b"IHDR" {
            return Err("Duplicate private browser PNG header".into());
        } else if kind == b"IDAT" {
            saw_pixels = true;
        } else if kind == b"IEND" {
            if size != 0 || !saw_pixels || chunk_end != png.len() {
                return Err("Private browser PNG is incomplete".into());
            }
            saw_end = true;
            break;
        }
        pos = chunk_end;
        chunks += 1;
    }
    if !saw_header || !saw_pixels || !saw_end {
        return Err("Private browser PNG is incomplete".into());
    }
    Ok(())
}

fn validate_preview_response(result: &Value, tab_id: i32) -> Result<(), String> {
    let width = result.get("width").and_then(Value::as_u64).unwrap_or(0);
    let height = result.get("height").and_then(Value::as_u64).unwrap_or(0);
    let bytes = result.get("pngBytes").and_then(Value::as_u64).unwrap_or(0);
    if result.get("captured") != Some(&Value::Bool(true))
        || result.get("tabId").and_then(Value::as_i64) != Some(i64::from(tab_id))
        || result.get("previewTokenConsumed") != Some(&Value::Bool(true))
        || result.get("mimeType").and_then(Value::as_str) != Some("image/png")
        || result
            .get("pageGeneration")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            < 1
        || width == 0
        || height == 0
        || width > MAX_PREVIEW_DIMENSION
        || height > MAX_PREVIEW_DIMENSION
        || width * height > MAX_PREVIEW_PIXELS
        || bytes == 0
        || bytes > MAX_PREVIEW_PNG_BYTES as u64
    {
        return Err("Private browser preview metadata is invalid".into());
    }
    let base64 = result
        .get("base64")
        .and_then(Value::as_str)
        .ok_or("Private browser preview is missing PNG data")?;
    let decoded = decode_preview_base64(base64)?;
    if decoded.len() as u64 != bytes {
        return Err("Private browser preview byte count is inconsistent".into());
    }
    inspect_preview_png(&decoded, width, height)
}

/// Returns {opened, automationAllowed, activeTabId, maxTabs, tabs}.
pub fn status(app: AppHandle) -> Result<Value, String> {
    call(app, "status", json!({}))
}

fn check_browser_mode_status(value: Value) -> Result<Value, String> {
    let field = |key| value.get(key).and_then(Value::as_bool);
    let supported = field("supported").ok_or("In-app browser support missing")?;
    let opened = field("opened").ok_or("In-app browser open state missing")?;
    let isolated = field("isolatedProfile").ok_or("In-app browser isolation missing")?;
    let fullscreen = field("fullscreenVisible").ok_or("In-app browser display state missing")?;
    let visible = field("activeTabVisible").ok_or("In-app browser active tab state missing")?;
    let secure = field("activeTabHttps").ok_or("In-app browser active URL state missing")?;
    let consent = field("automationAllowed").ok_or("In-app browser DOM approval state missing")?;
    let dom = field("domControl").ok_or("In-app browser DOM capability missing")?;
    let sidebar = field("rightSidebarImplemented").ok_or("In-app browser drawer state missing")?;
    let handset = field("handsetOverlay").ok_or("In-app browser handset state missing")?;
    let tabs = value["tabs"].as_array().ok_or("In-app browser tabs missing")?;
    if value["mode"] != "in_app_webview"
        || value["presentation"]
            != if opened {
                "native_right_drawer"
            } else {
                "not_presented"
            }
        || value["realtime"] != true
        || value["profileIsolationSupported"] != supported
        || sidebar != opened
        || value["chromeControlled"] != false
        || value["maxTabs"] != 5
        || tabs.len() > 5
        || (opened && (!supported || !isolated || !visible || tabs.is_empty()))
        || (isolated && (!supported || tabs.is_empty()))
        || fullscreen != (opened && handset && isolated)
        || (visible && (!opened || !isolated))
        || (!opened && (visible || sidebar || dom))
        || dom != (consent && visible && secure)
        || (dom && !supported)
    {
        return Err("In-app browser live native capability was not verified".into());
    }
    Ok(value)
}

/// Read-only native probe for the actual END-anchored drawer. Handsets use a
/// full-width overlay; tablet widths preserve a visible part of the chat.
pub fn browser_mode_status(app: AppHandle) -> Result<Value, String> {
    check_browser_mode_status(status(app)?)
}

/// Open one visible HTTPS tab; caller must pass true from an explicit UI action.
/// Max 5 tabs. This does not use desktop Chrome or the Tauri app WebView.
pub fn open(app: AppHandle, url: String, user_initiated: bool) -> Result<Value, String> {
    if !user_initiated {
        return Err("Private browser requires explicit user initiation".into());
    }
    check_https_url(&url)?;
    call(app, "open", json!({"url":url, "userInitiated":true}))
}

/// Shows the native Android Allow/Deny dialog for snapshot and DOM actions.
pub fn request_automation(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    if !user_initiated {
        return Err("Private browser automation requires user initiation".into());
    }
    call(app, "requestAutomation", json!({"userInitiated":true}))
}

pub fn list_tabs(app: AppHandle) -> Result<Value, String> {
    call(app, "listTabs", json!({}))
}

pub fn switch_tab(app: AppHandle, tab_id: i32) -> Result<Value, String> {
    check_tab(tab_id)?;
    call(app, "switchTab", json!({"tabId":tab_id}))
}

pub fn close_tab(app: AppHandle, tab_id: i32) -> Result<Value, String> {
    check_tab(tab_id)?;
    call(app, "closeTab", json!({"tabId":tab_id}))
}

pub fn close_all(app: AppHandle) -> Result<Value, String> {
    call(app, "closeAll", json!({}))
}

/// Snapshot has a unique snapshotId and bounded elements with stable indexes.
/// Every action consumes its snapshot, requiring a new snapshot for the next.
pub fn snapshot(app: AppHandle, tab_id: i32) -> Result<Value, String> {
    check_tab(tab_id)?;
    call(app, "snapshot", json!({"tabId":tab_id}))
}

/// Explicitly select an already visible, plugin-owned HTTPS WebView/tab.
/// Returns {tabId,previewToken,pageGeneration,expiresInMs,maxDimension,maxPngBytes}.
/// The token is ephemeral, bound to the current session/URL/page generation,
/// and consumed by the next capture attempt or invalidated by tab changes.
pub fn prepare_preview(app: AppHandle, tab_id: i32, user_initiated: bool) -> Result<Value, String> {
    check_preview_request(tab_id, user_initiated)?;
    let reply = call(
        app,
        "preparePreview",
        json!({"tabId":tab_id,"userInitiated":true}),
    )?;
    if reply.get("tabId").and_then(Value::as_i64) != Some(i64::from(tab_id))
        || reply
            .get("pageGeneration")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            < 1
    {
        return Err("Private browser preview selection did not match the tab".into());
    }
    check_preview_token(
        reply
            .get("previewToken")
            .and_then(Value::as_str)
            .unwrap_or(""),
    )?;
    Ok(reply)
}

/// Requires another explicit user request plus the current preview token.
/// Android displays a native Allow-once/Deny dialog; only a single visible
/// owned WebView is drawn, then the bounded PNG is returned in-memory.
/// Denial is {captured:false,cancelled:true,previewTokenConsumed:true}.
pub fn capture_preview(
    app: AppHandle,
    tab_id: i32,
    preview_token: String,
    user_initiated: bool,
) -> Result<Value, String> {
    check_preview_request(tab_id, user_initiated)?;
    check_preview_token(&preview_token)?;
    let reply = call(
        app,
        "capturePreview",
        json!({
            "tabId":tab_id,"previewToken":preview_token,"userInitiated":true
        }),
    )?;
    if reply.get("captured") == Some(&Value::Bool(false))
        && reply.get("cancelled") == Some(&Value::Bool(true))
        && reply.get("previewTokenConsumed") == Some(&Value::Bool(true))
    {
        return Ok(reply);
    }
    validate_preview_response(&reply, tab_id)?;
    Ok(reply)
}

#[tauri::command]
pub async fn in_app_browser_prepare_preview(
    app: AppHandle,
    tab_id: i32,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || prepare_preview(app, tab_id, user_initiated))
        .await
        .map_err(|_| "Private browser preview preparation interrupted".to_string())?
}

#[tauri::command]
pub async fn in_app_browser_capture_preview(
    app: AppHandle,
    tab_id: i32,
    preview_token: String,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        capture_preview(app, tab_id, preview_token, user_initiated)
    })
    .await
    .map_err(|_| "Private browser preview capture interrupted".to_string())?
}

pub fn click(app: AppHandle, tab_id: i32, snapshot_id: i64, index: i32) -> Result<Value, String> {
    check_action(tab_id, snapshot_id, index, false)?;
    let receipt = call(
        app,
        "click",
        json!({"tabId":tab_id, "snapshotId":snapshot_id, "index":index}),
    )?;
    check_mutation_output(&receipt, tab_id)?;
    Ok(receipt)
}

/// Read-only, indexed summary for the currently consented, selected, visible
/// plugin-owned WebView. Does not consume the snapshot; discloses only an
/// allowlisted tag, role, safe label and fixed boolean/type attributes.
pub fn inspect(app: AppHandle, tab_id: i32, snapshot_id: i64, index: i32) -> Result<Value, String> {
    check_action(tab_id, snapshot_id, index, false)?;
    let result = call(
        app,
        "inspect",
        json!({"tabId":tab_id, "snapshotId":snapshot_id, "index":index}),
    )?;
    check_inspect_output(&result, tab_id, snapshot_id, index)?;
    Ok(result)
}

/// Dispatches synthetic DOM mouseover/mouseenter events on the same
/// snapshot-verified node. Does not emulate a physical pointer or CDP hover;
/// each attempt consumes the snapshot.
pub fn hover(app: AppHandle, tab_id: i32, snapshot_id: i64, index: i32) -> Result<Value, String> {
    check_action(tab_id, snapshot_id, index, false)?;
    let result = call(
        app,
        "hover",
        json!({"tabId":tab_id, "snapshotId":snapshot_id, "index":index}),
    )?;
    check_hover_output(&result, tab_id)?;
    Ok(result)
}

#[tauri::command]
pub async fn in_app_browser_inspect(
    app: AppHandle,
    tab_id: i32,
    snapshot_id: i64,
    index: i32,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || inspect(app, tab_id, snapshot_id, index))
        .await
        .map_err(|_| "Private browser inspection interrupted".to_string())?
}

#[tauri::command]
pub async fn in_app_browser_hover(
    app: AppHandle,
    tab_id: i32,
    snapshot_id: i64,
    index: i32,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || hover(app, tab_id, snapshot_id, index))
        .await
        .map_err(|_| "Private browser hover interrupted".to_string())?
}

pub fn fill(
    app: AppHandle,
    tab_id: i32,
    snapshot_id: i64,
    index: i32,
    value: String,
) -> Result<Value, String> {
    check_action(tab_id, snapshot_id, index, false)?;
    if value.len() > MAX_INPUT {
        return Err("Private browser text input is too long".into());
    }
    let receipt = call(
        app,
        "fill",
        json!({"tabId":tab_id, "snapshotId":snapshot_id, "index":index, "value":value}),
    )?;
    check_mutation_output(&receipt, tab_id)?;
    Ok(receipt)
}

/// index = 0 scrolls the page; 1..64 scroll a snapshot element.
pub fn scroll(
    app: AppHandle,
    tab_id: i32,
    snapshot_id: i64,
    index: i32,
    delta_y: i32,
) -> Result<Value, String> {
    check_action(tab_id, snapshot_id, index, true)?;
    if !(-MAX_SCROLL..=MAX_SCROLL).contains(&delta_y) {
        return Err("Private browser scroll distance is out of range".into());
    }
    let receipt = call(
        app,
        "scroll",
        json!({"tabId":tab_id, "snapshotId":snapshot_id, "index":index, "deltaY":delta_y}),
    )?;
    check_mutation_output(&receipt, tab_id)?;
    Ok(receipt)
}

#[cfg(target_os = "android")]
fn call(app: AppHandle, command: &str, payload: Value) -> Result<Value, String> {
    let plugin = app
        .try_state::<PrivateBrowserHandle<tauri::Wry>>()
        .ok_or_else(|| "Android private browser plugin is not registered".to_string())?;
    plugin
        .0
        .run_mobile_plugin(command, payload)
        .map_err(|_| "Android private browser operation was unsuccessful".to_string())
}

#[cfg(not(target_os = "android"))]
fn call(_app: AppHandle, _command: &str, _payload: Value) -> Result<Value, String> {
    Err("Private browser is available on Android only".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_right_drawer_probe_accepts_visible_and_hidden_session_without_chrome_control() {
        let idle = json!({"mode":"in_app_webview",
            "presentation":"not_presented","realtime":true,
            "supported":true,"profileIsolationSupported":true,
            "opened":false,"isolatedProfile":false,"fullscreenVisible":false,
            "activeTabVisible":false,"activeTabHttps":false,
            "automationAllowed":false,"domControl":false,
            "rightSidebarImplemented":false,"chromeControlled":false,
            "handsetOverlay":true,"maxTabs":5,"tabs":[]});
        assert!(check_browser_mode_status(idle.clone()).is_ok());
        let mut session = idle.clone();
        session["opened"] = json!(true);
        session["presentation"] = json!("native_right_drawer");
        session["isolatedProfile"] = json!(true);
        session["fullscreenVisible"] = json!(true);
        session["activeTabVisible"] = json!(true);
        session["activeTabHttps"] = json!(true);
        session["rightSidebarImplemented"] = json!(true);
        session["tabs"] = json!([{"tabId":1}]);
        assert!(check_browser_mode_status(session.clone()).is_ok(),
            "Visible owned tab alone does not mean DOM automation was approved");
        session["automationAllowed"] = json!(true);
        session["domControl"] = json!(true);
        assert!(check_browser_mode_status(session.clone()).is_ok());

        // Dialog.hide() intentionally retains the WebViews and cookies,
        // but no DOM actions may be reported available while hidden.
        let mut hidden = session.clone();
        hidden["opened"] = json!(false);
        hidden["presentation"] = json!("not_presented");
        hidden["rightSidebarImplemented"] = json!(false);
        hidden["fullscreenVisible"] = json!(false);
        hidden["activeTabVisible"] = json!(false);
        hidden["domControl"] = json!(false);
        assert!(check_browser_mode_status(hidden.clone()).is_ok());
        hidden["domControl"] = json!(true);
        assert!(check_browser_mode_status(hidden).is_err());

        // A tablet drawer may leave chat visible behind it.
        let mut tablet = session.clone();
        tablet["handsetOverlay"] = json!(false);
        tablet["fullscreenVisible"] = json!(false);
        assert!(check_browser_mode_status(tablet).is_ok());

        for (key, wrong) in [("rightSidebarImplemented",json!(false)),
            ("chromeControlled",json!(true)),("isolatedProfile",json!(false)),
            ("fullscreenVisible",json!(false)),("supported",json!(false)),
            ("domControl",json!(false))] {
            let mut claimed = session.clone();
            claimed[key] = wrong;
            assert!(check_browser_mode_status(claimed).is_err(),
                "Invalid native WebView claim must be rejected: {key}");
        }
        let mut unsupported = idle;
        unsupported["supported"] = json!(false);
        unsupported["profileIsolationSupported"] = json!(false);
        assert!(check_browser_mode_status(unsupported).is_ok(),
            "A genuinely unavailable profile feature must remain explicitly unavailable");
    }

    #[test]
    fn accepts_regular_https() {
        assert!(check_https_url("https://example.org/").is_ok());
        assert!(check_https_url("HTTPS://sub.example.org/path?q=1").is_ok());
    }

    #[test]
    fn rejects_non_https_and_ambiguous_host() {
        for bad in [
            "http://example.org",
            "file:///private",
            "javascript:alert(1)",
            "https://",
            "https://user:pass@example.org",
            "https://example.org/\r\nHost:bad",
        ] {
            assert!(check_https_url(bad).is_err(), "accepted {bad:?}");
        }
        assert!(check_https_url(&format!("https://{}", "a".repeat(MAX_URL))).is_err());
    }

    #[test]
    fn only_bounded_snapshot_actions() {
        assert!(check_action(1, 1, 1, false).is_ok());
        assert!(check_action(1, 1, 0, true).is_ok());
        assert!(check_action(0, 1, 1, false).is_err());
        assert!(check_action(1, 0, 1, false).is_err());
        assert!(check_action(1, 1, 0, false).is_err());
        assert!(check_action(1, 1, 65, true).is_err());
        assert!(!(-MAX_SCROLL..=MAX_SCROLL).contains(&2001));
    }

    #[test]
    fn indexed_dom_mutation_receipt_requires_exact_active_tab_and_no_extra_fields() {
        let valid = json!({"ok":true,"tabId":3,"snapshotInvalidated":true});
        assert!(check_mutation_output(&valid, 3).is_ok());
        assert!(check_mutation_output(&valid, 4).is_err());
        for bad in [
            json!({"ok":true,"tabId":3,"snapshotInvalidated":false}),
            json!({"ok":false,"tabId":3,"snapshotInvalidated":true}),
            json!({"ok":true,"tabId":3,"snapshotInvalidated":true,"secret":"unexpected"}),
            json!({"ok":true,"tabId":"3","snapshotInvalidated":true}),
            json!({"ok":true,"tabId":3.5,"snapshotInvalidated":true}),
            json!({"ok":true,"tabId":3}),
            json!([]),
        ] {
            assert!(check_mutation_output(&bad, 3).is_err(), "{bad}");
        }
    }

    fn safe_inspection() -> Value {
        json!({
            "ok":true,"tabId":7,"snapshotId":13,"index":2,
            "tag":"button","role":"button","label":"Open menu",
            "attributes":{
                "type":"button","disabled":false,"readOnly":false,"required":false,
                "ariaExpanded":false,"ariaPressed":false,"ariaChecked":false
            },
            "snapshotInvalidated":false
        })
    }

    #[test]
    fn inspect_rejects_wrong_index_and_private_fields() {
        assert!(check_action(7, 13, 2, false).is_ok());
        assert!(check_inspect_output(&safe_inspection(), 7, 13, 2).is_ok());
        assert!(check_inspect_output(&safe_inspection(), 7, 13, 3).is_err());
        assert!(check_inspect_output(&safe_inspection(), 8, 13, 2).is_err());
        let mut modified = safe_inspection();
        modified["value"] = json!("injected secret");
        assert!(check_inspect_output(&modified, 7, 13, 2).is_err());
        let mut modified = safe_inspection();
        modified["attributes"]["href"] = json!("https://example.org/?token=private");
        assert!(check_inspect_output(&modified, 7, 13, 2).is_err());
        let mut modified = safe_inspection();
        modified["attributes"]["disabled"] = json!("yes");
        assert!(check_inspect_output(&modified, 7, 13, 2).is_err());
    }

    #[test]
    fn inspect_rejects_sensitive_labels_and_bad_enums() {
        for label in [
            "Bearer secret",
            "API token",
            "password",
            "1234567890123456",
            "test@example.org",
            "ghp_abcdefghijklmnopqrstuvwxyz12345",
        ] {
            let mut modified = safe_inspection();
            modified["label"] = json!(label);
            assert!(
                check_inspect_output(&modified, 7, 13, 2).is_err(),
                "{label}"
            );
        }
        let mut modified = safe_inspection();
        modified["tag"] = json!("script");
        assert!(check_inspect_output(&modified, 7, 13, 2).is_err());
        let mut modified = safe_inspection();
        modified["role"] = json!("arbitrary");
        assert!(check_inspect_output(&modified, 7, 13, 2).is_err());
    }

    #[test]
    fn hover_reports_synthetic_event_without_physical_pointer() {
        let valid = json!({
            "ok":true,"tabId":7,"snapshotInvalidated":true,
            "syntheticEvents":true,"physicalPointerMoved":false
        });
        assert!(check_hover_output(&valid, 7).is_ok());
        assert!(check_hover_output(&valid, 8).is_err());
        let mut modified = valid.clone();
        modified["physicalPointerMoved"] = json!(true);
        assert!(check_hover_output(&modified, 7).is_err());
        modified = valid.clone();
        modified["snapshotInvalidated"] = json!(false);
        assert!(check_hover_output(&modified, 7).is_err());
        modified = valid.clone();
        modified["html"] = json!("<secret>");
        assert!(check_hover_output(&modified, 7).is_err());
    }

    /// A valid 1x1 red RGBA PNG with one uncompressed zlib scanline.
    fn sample_png() -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        fn chunk(png: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
            png.extend_from_slice(&(data.len() as u32).to_be_bytes());
            png.extend_from_slice(name);
            png.extend_from_slice(data);
            let start = png.len() - name.len() - data.len();
            let crc = png_crc32(&png[start..]);
            png.extend_from_slice(&crc.to_be_bytes());
        }
        chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        chunk(
            &mut png,
            b"IDAT",
            &[
                0x78, 0x01, 0x01, 0x05, 0x00, 0xfa, 0xff, 0x00, 0xff, 0x00, 0x00, 0xff, 0x05, 0x00,
                0x01, 0xff,
            ],
        );
        chunk(&mut png, b"IEND", &[]);
        png
    }

    fn encode_base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = String::new();
        for chunk in bytes.chunks(3) {
            encoded.push(ALPHABET[(chunk[0] >> 2) as usize] as char);
            encoded.push(
                ALPHABET
                    [(((chunk[0] & 3) << 4) | (chunk.get(1).copied().unwrap_or(0) >> 4)) as usize]
                    as char,
            );
            encoded.push(if chunk.len() >= 2 {
                ALPHABET
                    [(((chunk[1] & 15) << 2) | (chunk.get(2).copied().unwrap_or(0) >> 6)) as usize]
                    as char
            } else {
                '='
            });
            encoded.push(if chunk.len() >= 3 {
                ALPHABET[(chunk[2] & 63) as usize] as char
            } else {
                '='
            });
        }
        encoded
    }

    fn sample_frame() -> Value {
        let png = sample_png();
        json!({
            "captured":true,"tabId":7,"pageGeneration":3,"previewTokenConsumed":true,
            "mimeType":"image/png","width":1,"height":1,
            "pngBytes":png.len(),"base64":encode_base64(&png)
        })
    }

    #[test]
    fn preview_requires_fresh_explicit_user_request_and_uuid() {
        assert!(check_preview_request(7, true).is_ok());
        assert!(check_preview_request(7, false).is_err());
        assert!(check_preview_request(0, true).is_err());
        let token = "8a403aa0-e180-4247-96ce-8cbe21bfc687";
        assert!(check_preview_token(token).is_ok());
        assert!(check_preview_token(&token.to_uppercase()).is_err());
        assert!(check_preview_token("8a403aa0-e180-4247-96ce-8cbe21bfxx87").is_err());
        assert!(check_preview_token("").is_err());
    }

    #[test]
    fn preview_accepts_valid_png_and_rejects_tamper_or_misbound_tab() {
        let frame = sample_frame();
        assert!(validate_preview_response(&frame, 7).is_ok());
        assert!(validate_preview_response(&frame, 8).is_err());
        let mut frame = frame;
        frame["width"] = json!(1025);
        assert!(validate_preview_response(&frame, 7).is_err());
        frame["width"] = json!(1);
        frame["pngBytes"] = json!(MAX_PREVIEW_PNG_BYTES + 1);
        assert!(validate_preview_response(&frame, 7).is_err());
        frame["pngBytes"] = json!(sample_png().len());
        frame["mimeType"] = json!("image/jpeg");
        assert!(validate_preview_response(&frame, 7).is_err());
        frame["mimeType"] = json!("image/png");
        frame["base64"] = json!("iVBORw0K\n");
        assert!(validate_preview_response(&frame, 7).is_err());
    }

    #[test]
    fn preview_checks_png_crc_lengths_headers_and_padding() {
        let png = sample_png();
        assert!(inspect_preview_png(&png, 1, 1).is_ok());
        assert!(inspect_preview_png(&png, 2, 1).is_err());
        let mut corrupted = png.clone();
        corrupted[40] ^= 0x02;
        assert!(inspect_preview_png(&corrupted, 1, 1).is_err());
        assert!(inspect_preview_png(&png[..png.len() - 1], 1, 1).is_err());
        assert!(decode_preview_base64("AA==").is_ok());
        assert!(decode_preview_base64("AB==").is_err());
        assert!(decode_preview_base64("AAB=").is_err());
        assert!(decode_preview_base64("AA=A").is_err());
        assert!(decode_preview_base64("AA$=").is_err());
    }
}
