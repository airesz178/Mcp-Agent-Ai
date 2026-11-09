//! Read-only access plus explicitly user-approved, create-only UTF-8 text
//! files and new directories within a user-selected Android SAF tree.
//! Wire this module through `mod android_documents;`, register `init()` on
//! Android, and register the existing read commands plus the create/write
//! `document_tree_request_write`, `document_tree_create_text`, and
//! `document_tree_create_folder`, and `document_tree_rename_entry` wrappers.

use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

const MAX_DEPTH: usize = 16;
const MAX_NAME_LENGTH: usize = 255;
const MAX_TREE_URI_LENGTH: usize = 8192;
const MAX_CREATE_BYTES: usize = 65_536;
const MAX_OPEN_BYTES: usize = 1_048_576;

#[cfg(target_os = "android")]
struct AndroidDocumentTree<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("document-tree")
        .setup(|app, api| {
            let plugin = api.register_android_plugin("ai.mcpagent.app", "DocumentTreePlugin")?;
            app.manage(AndroidDocumentTree(plugin));
            Ok(())
        })
        .build()
}

fn check_tree_uri(tree_uri: &str) -> Result<(), String> {
    if tree_uri.len() > MAX_TREE_URI_LENGTH
        || !tree_uri.starts_with("content://")
        || tree_uri.chars().any(char::is_control)
    {
        return Err("Invalid Android document-tree URI".into());
    }
    // Android isTreeUri() also accepts nested /tree/ID/document/ID URIs;
    // only a canonical root tree URI may be selected and persisted. The Kotlin
    // layer independently checks buildTreeDocumentUri(authority, ID) equality.
    let (authority, encoded_id) = tree_uri["content://".len()..]
        .split_once("/tree/")
        .ok_or_else(|| "Invalid canonical SAF tree root URI".to_owned())?;
    if authority.is_empty()
        || authority.len() > 255
        || !authority
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        || encoded_id.is_empty()
        || encoded_id.len() > 4096 * 3
        || encoded_id.chars().any(|ch| matches!(ch, '/' | '?' | '#'))
        || encoded_id.chars().any(char::is_whitespace)
    {
        return Err("Invalid canonical SAF tree root URI".into());
    }
    let encoded = encoded_id.as_bytes();
    let mut i = 0;
    while i < encoded.len() {
        if encoded[i] == b'%' {
            if i + 2 >= encoded.len()
                || !encoded[i + 1].is_ascii_hexdigit()
                || !encoded[i + 2].is_ascii_hexdigit()
            {
                return Err("Malformed SAF tree document ID encoding".into());
            }
            i += 3;
        } else {
            // Uri.toString() of the Android document picker encodes reserved
            // and non-ASCII characters in this segment.
            if !encoded[i].is_ascii_alphanumeric()
                && !matches!(
                    encoded[i],
                    b'-' | b'_' | b'.' | b'~' | b'!' | b'*' | b'\'' | b'(' | b')'
                )
            {
                return Err("Invalid SAF tree document ID encoding".into());
            }
            i += 1;
        }
    }
    Ok(())
}

/// Project IDs come from the app-private catalog, not a filesystem path.
/// Never accept separators or an arbitrary preference key from the WebView.
fn check_project_id(id: &str) -> Result<(), String> {
    let valid_uuid = id.len() == 36
        && id.bytes().enumerate().all(|(index, ch)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                ch == b'-'
            } else {
                ch.is_ascii_digit() || (b'a'..=b'f').contains(&ch)
            }
        });
    let valid_legacy = id.starts_with("project-")
        && (9..=90).contains(&id.len())
        && id
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_'));
    if valid_uuid || valid_legacy {
        Ok(())
    } else {
        Err("Invalid Android Project ID".into())
    }
}

fn verify_project_folder_receipt(value: &Value, id: &str, is_request: bool) -> Result<(), String> {
    let invalid = || "Invalid Android Project folder receipt".to_owned();
    let fields = value.as_object().ok_or_else(invalid)?;
    if fields.get("projectId").and_then(Value::as_str) != Some(id) {
        return Err(invalid());
    }
    let allowed = [
        "projectId",
        "selected",
        "cancelled",
        "linked",
        "granted",
        "persisted",
        "treeUri",
        "displayName",
        "readOnly",
        "pickerOpen",
        "released",
        "filesDeleted",
    ];
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid());
    }
    if is_request && value.get("selected") == Some(&json!(false)) {
        if value.get("cancelled") != Some(&json!(true)) || fields.len() != 3 {
            return Err(invalid());
        }
        return Ok(());
    }
    if value.get("readOnly") != Some(&json!(true)) {
        return Err(invalid());
    }
    if is_request
        && (value.get("selected") != Some(&json!(true))
            || value.get("persisted") != Some(&json!(true)))
    {
        return Err(invalid());
    }
    let granted = value
        .get("granted")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    let uri = value
        .get("treeUri")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let name = value
        .get("displayName")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    if granted {
        check_tree_uri(uri).map_err(|_| invalid())?;
        check_properties_name(name).map_err(|_| invalid())?;
    } else if is_request || !uri.is_empty() || !name.is_empty() {
        return Err(invalid());
    }
    if !is_request {
        let linked = value
            .get("linked")
            .and_then(Value::as_bool)
            .ok_or_else(invalid)?;
        if granted && !linked {
            return Err(invalid());
        }
        if !value.get("pickerOpen").is_some_and(Value::is_boolean) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn check_path(path: &[String], require_file: bool) -> Result<(), String> {
    if path.len() > MAX_DEPTH || (require_file && path.is_empty()) {
        return Err("Invalid Android document-tree path".into());
    }
    for segment in path {
        if segment.is_empty()
            || segment.chars().count() > MAX_NAME_LENGTH
            || segment == "."
            || segment == ".."
            || segment.contains('/')
            || segment.contains('\\')
            || segment.chars().any(char::is_control)
        {
            return Err("Invalid Android document-tree path".into());
        }
    }
    Ok(())
}

fn check_text_name(name: &str) -> Result<(), String> {
    let lower = name.to_ascii_lowercase();
    if check_properties_name(name).is_err() || lower.len() <= 4 || !lower.ends_with(".txt") {
        return Err("Invalid new UTF-8 text filename; use a simple .txt name".into());
    }
    Ok(())
}

fn check_text_create(
    tree_uri: &str,
    path: &[String],
    name: &str,
    content: &str,
    user_initiated: bool,
) -> Result<(), String> {
    if !user_initiated {
        return Err("Text creation requires an explicit user action".into());
    }
    check_tree_uri(tree_uri)?;
    check_path(path, false)?;
    // The newly created file adds one additional path segment.
    if path.len() >= MAX_DEPTH {
        return Err("New text file would exceed maximum SAF path depth".into());
    }
    check_text_name(name)?;
    if content.len() > MAX_CREATE_BYTES {
        return Err("UTF-8 text exceeds 64 KiB".into());
    }
    Ok(())
}

fn check_folder_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.encode_utf16().count() > MAX_NAME_LENGTH
        || name.trim() != name
        || name.starts_with('.')
        || name.ends_with('.')
        || name.chars().any(|ch| {
            ch.is_control()
                || "/\\<>:\"|?*".contains(ch)
                || matches!(ch,
                    '\u{061c}' | '\u{200b}'..='\u{200f}' |
                    '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' |
                    '\u{feff}')
        })
    {
        return Err("Invalid new folder name: use a plain, non-hidden name".into());
    }
    Ok(())
}

fn check_folder_create(
    tree_uri: &str,
    path: &[String],
    name: &str,
    user_initiated: bool,
) -> Result<(), String> {
    if !user_initiated {
        return Err("Folder creation requires an explicit user action".into());
    }
    check_tree_uri(tree_uri)?;
    // New folder is a final path segment; keep combined depth <= 16.
    check_path(path, false)?;
    if path.len() >= MAX_DEPTH {
        return Err("Selected folder parent is at the maximum path depth".into());
    }
    check_folder_name(name)?;
    Ok(())
}

fn check_rename_request(
    tree_uri: &str,
    path: &[String],
    name: &str,
    user_initiated: bool,
) -> Result<(), String> {
    if !user_initiated {
        return Err("SAF rename requires an explicit user action".into());
    }
    check_tree_uri(tree_uri)?;
    // The entry to be renamed is the last path segment; the root is immutable.
    check_path(path, true)?;
    check_folder_name(name)?;
    // Android validates full Unicode categories and NFC again. Reject
    // noncharacters and private-use scalars before crossing the bridge.
    if name.chars().any(|ch| {
        let cp = ch as u32;
        (0xFDD0..=0xFDEF).contains(&cp)
            || cp & 0xFFFE == 0xFFFE
            || (0xE000..=0xF8FF).contains(&cp)
            || (0xF0000..=0xFFFFD).contains(&cp)
            || (0x100000..=0x10FFFD).contains(&cp)
    }) {
        return Err("New SAF rename name contains unsupported Unicode".into());
    }
    if path
        .last()
        .is_some_and(|current| current.eq_ignore_ascii_case(name))
    {
        return Err("Renamed entry must have a new sibling name".into());
    }
    Ok(())
}

const MAX_SAFE_NUMBER: u64 = 9_007_199_254_740_991;
const MAX_PROVIDER_TIMESTAMP_MS: u64 = 253_402_300_799_999;

fn check_properties_name(name: &str) -> Result<(), String> {
    check_folder_name(name)?;
    if name.chars().any(|ch| {
        let cp = ch as u32;
        (0xFDD0..=0xFDEF).contains(&cp)
            || cp & 0xFFFE == 0xFFFE
            || (0xE000..=0xF8FF).contains(&cp)
            || (0xF0000..=0xFFFFD).contains(&cp)
            || (0x100000..=0x10FFFD).contains(&cp)
    }) {
        return Err("Unsafe Unicode in SAF properties name".into());
    }
    Ok(())
}

fn check_properties_request(
    tree_uri: &str,
    path: &[String],
    user_initiated: bool,
) -> Result<(), String> {
    if !user_initiated {
        return Err("SAF properties require an explicit user action".into());
    }
    check_tree_uri(tree_uri)?;
    check_path(path, false)?; // [] addresses the selected tree root.
    for name in path {
        check_properties_name(name)?;
    }
    Ok(())
}

/// Checks all output fields before exposing a provider-generated properties
/// receipt to the UI. Capability booleans describe provider flags only.
fn verify_properties_receipt(
    receipt: &Value,
    tree_uri: &str,
    path: &[String],
) -> Result<(), String> {
    let invalid = || "Invalid Android SAF properties receipt".to_owned();
    let fields = receipt.as_object().ok_or_else(invalid)?;
    const ALLOWED: &[&str] = &[
        "treeUri",
        "path",
        "name",
        "isDirectory",
        "mimeType",
        "size",
        "lastModified",
        "canWrite",
        "canRename",
        "canDelete",
    ];
    if fields.len() < 8
        || fields.len() > ALLOWED.len()
        || fields.keys().any(|key| !ALLOWED.contains(&key.as_str()))
        || receipt.to_string().len() > 12_288
    {
        return Err(invalid());
    }
    if fields.get("treeUri").and_then(Value::as_str) != Some(tree_uri) {
        return Err(invalid());
    }
    let got_path = fields
        .get("path")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if got_path.len() != path.len()
        || !got_path
            .iter()
            .zip(path.iter())
            .all(|(got, expected)| got.as_str() == Some(expected))
    {
        return Err(invalid());
    }
    let name = fields
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    check_properties_name(name).map_err(|_| invalid())?;
    if !path.is_empty() && path.last().map(String::as_str) != Some(name) {
        return Err(invalid());
    }
    let mime = fields
        .get("mimeType")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    if mime.is_empty()
        || mime.len() > 256
        || mime.trim() != mime
        || !mime.contains('/')
        || mime.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    let is_directory = fields
        .get("isDirectory")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    if is_directory != (mime == "vnd.android.document/directory") {
        return Err(invalid());
    }
    for key in ["canWrite", "canRename", "canDelete"] {
        if fields.get(key).and_then(Value::as_bool).is_none() {
            return Err(invalid());
        }
    }
    if let Some(size) = fields.get("size") {
        if !size.as_u64().is_some_and(|n| n <= MAX_SAFE_NUMBER) {
            return Err(invalid());
        }
    }
    if let Some(modified) = fields.get("lastModified") {
        if !modified
            .as_u64()
            .is_some_and(|n| n > 0 && n <= MAX_PROVIDER_TIMESTAMP_MS)
        {
            return Err(invalid());
        }
    }
    Ok(())
}

/// RFC 4648 standard Base64: no whitespace, URL-safe alphabet, missing/extra
/// padding, or non-zero unused pad bits. Checking the exact encoded length
/// proves the decoded size without allocating a second 1 MiB byte buffer.
fn canonical_open_base64(data: &str, decoded_size: usize) -> bool {
    if decoded_size > MAX_OPEN_BYTES || data.len() != decoded_size.div_ceil(3) * 4 {
        return false;
    }
    if decoded_size == 0 {
        return data.is_empty();
    }
    let alphabet_value = |byte: u8| -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let bytes = data.as_bytes();
    let padding = (3 - decoded_size % 3) % 3;
    if !bytes[..bytes.len() - padding]
        .iter()
        .all(|&byte| alphabet_value(byte).is_some())
        || !bytes[bytes.len() - padding..]
            .iter()
            .all(|&byte| byte == b'=')
    {
        return false;
    }
    match padding {
        2 => alphabet_value(bytes[bytes.len() - 3]).is_some_and(|value| value & 0b1111 == 0),
        1 => alphabet_value(bytes[bytes.len() - 2]).is_some_and(|value| value & 0b11 == 0),
        _ => true,
    }
}

/// Only the six fields actually emitted by Kotlin openDocument may reach the
/// UI. The caller's selected tree and relative entry path must echo exactly.
/// The response contains file bytes encoded as bounded canonical Base64,
/// never a filesystem URI or additional provider-controlled metadata.
fn verify_open_receipt(
    receipt: &Value,
    expected_tree_uri: &str,
    expected_path: &[String],
) -> Result<(), String> {
    let invalid = || "Android returned an invalid SAF document-read receipt".to_owned();
    let object = receipt.as_object().ok_or_else(invalid)?;
    if object.len() != 6
        || object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "treeUri" | "path" | "mimeType" | "size" | "encoding" | "data"
            )
        })
        || receipt.get("treeUri").and_then(Value::as_str) != Some(expected_tree_uri)
        || receipt.get("encoding").and_then(Value::as_str) != Some("base64")
    {
        return Err(invalid());
    }
    let path = receipt
        .get("path")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if path.len() != expected_path.len()
        || !path
            .iter()
            .zip(expected_path)
            .all(|(entry, segment)| entry.as_str() == Some(segment.as_str()))
    {
        return Err(invalid());
    }
    let mime = receipt
        .get("mimeType")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    // MIME types are an ASCII type/subtype pair, not a source of UI markup or
    // a filename. Deny directories even if a broken provider claims to read.
    let mut mime_parts = mime.split('/');
    let valid_mime_part = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                    )
            })
    };
    if mime.len() > 256
        || !mime_parts.next().is_some_and(valid_mime_part)
        || !mime_parts.next().is_some_and(valid_mime_part)
        || mime_parts.next().is_some()
        || mime == "vnd.android.document/directory"
    {
        return Err(invalid());
    }
    let size = receipt
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(invalid)?;
    if size > MAX_OPEN_BYTES as u64 {
        return Err(invalid());
    }
    let data = receipt
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    if !canonical_open_base64(data, size as usize) {
        return Err(invalid());
    }
    Ok(())
}

/// Only a verified rename receipt, explicit cancellation or an explicitly
/// uncertain provider result can leave this bridge. No inferred success.
fn verify_rename_receipt(
    receipt: &Value,
    tree_uri: &str,
    old_path: &[String],
    new_name: &str,
) -> Result<(), String> {
    let invalid = || "Android returned an invalid SAF rename receipt".to_owned();
    if receipt.to_string().len() > 32_768 {
        return Err(invalid());
    }
    if receipt.get("renamed") == Some(&Value::Bool(false)) {
        if receipt.get("cancelled") == Some(&Value::Bool(true))
            && receipt.get("uncertain") == Some(&Value::Bool(false))
            && receipt.get("verified").and_then(Value::as_bool) != Some(true)
            && receipt.get("providerAttempted").and_then(Value::as_bool) != Some(true)
        {
            return Ok(());
        }
        if receipt.get("uncertain") == Some(&Value::Bool(true))
            && receipt.get("verified") == Some(&Value::Bool(false))
            && receipt.get("providerAttempted") == Some(&Value::Bool(true))
            && receipt.get("cancelled").and_then(Value::as_bool) != Some(true)
            && receipt
                .get("message")
                .and_then(Value::as_str)
                .is_some_and(|message| {
                    !message.is_empty()
                        && message.len() <= 256
                        && !message.chars().any(char::is_control)
                })
        {
            return Ok(());
        }
        return Err(invalid());
    }
    if receipt.get("renamed") != Some(&Value::Bool(true))
        || receipt.get("verified") != Some(&Value::Bool(true))
        || receipt.get("uncertain") != Some(&Value::Bool(false))
        || receipt.get("treeUri").and_then(Value::as_str) != Some(tree_uri)
        || receipt.get("oldName").and_then(Value::as_str) != old_path.last().map(String::as_str)
        || receipt.get("name").and_then(Value::as_str) != Some(new_name)
        || receipt
            .get("isDirectory")
            .and_then(Value::as_bool)
            .is_none()
        || receipt
            .get("documentIdChanged")
            .and_then(Value::as_bool)
            .is_none()
    {
        return Err(invalid());
    }
    let mime = receipt
        .get("mimeType")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    if mime.is_empty() || mime.len() > 256 || mime.chars().any(char::is_control) {
        return Err(invalid());
    }
    if (mime == "vnd.android.document/directory")
        != receipt
            .get("isDirectory")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return Err(invalid());
    }
    let array_to_path = |key: &str| -> Result<Vec<String>, String> {
        let arr = receipt
            .get(key)
            .and_then(Value::as_array)
            .ok_or_else(invalid)?;
        arr.iter()
            .map(|entry| entry.as_str().map(str::to_owned).ok_or_else(invalid))
            .collect()
    };
    let parent = array_to_path("parentPath")?;
    let old = array_to_path("oldPath")?;
    let new = array_to_path("newPath")?;
    if old != old_path
        || parent != old_path[..old_path.len() - 1]
        || new != [parent, vec![new_name.to_owned()]].concat()
    {
        return Err(invalid());
    }
    Ok(())
}

/// Return whether a readable SAF tree was explicitly chosen and persisted.
/// `granted` is false on desktop or after Android revokes the persisted grant.
pub fn status(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        return mobile_call(&app, "status", json!({}));
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(json!({
            "supported": false,
            "granted": false,
            "treeUri": "",
            "pickerOpen": false,
            "maxEntries": 128,
            "maxDepth": MAX_DEPTH,
            "maxReadBytes": 1_048_576,
            "writeGranted": false,
            "maxCreateBytes": MAX_CREATE_BYTES
        }))
    }
}

/// Launch ACTION_OPEN_DOCUMENT_TREE only when called by a user-facing action.
/// The Kotlin callback persists a READ grant; it never silently selects a tree.
pub fn request(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        return mobile_call(&app, "request", json!({}));
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err("Android document-tree picker is unavailable".into())
    }
}

/// User-initiated READ grant for one existing Android Project label. It is
/// independent from the Files tab's selected tree and cannot read file bytes.
pub fn project_folder_request(
    app: AppHandle,
    project_id: String,
    user_initiated: bool,
) -> Result<Value, String> {
    check_project_id(&project_id)?;
    if !user_initiated {
        return Err("Project folder picker requires explicit user action".into());
    }
    #[cfg(target_os = "android")]
    {
        let receipt = mobile_call(
            &app,
            "requestProjectFolder",
            json!({"projectId":project_id,"userInitiated":true}),
        )?;
        verify_project_folder_receipt(&receipt, &project_id, true)?;
        return Ok(receipt);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err("Project folder picker is available on Android only".into())
    }
}

pub fn project_folder_status(app: AppHandle, project_id: String) -> Result<Value, String> {
    check_project_id(&project_id)?;
    #[cfg(target_os = "android")]
    {
        let receipt = mobile_call(&app, "projectFolderStatus", json!({"projectId":project_id}))?;
        verify_project_folder_receipt(&receipt, &project_id, false)?;
        return Ok(receipt);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err("Project folder status is available on Android only".into())
    }
}

pub fn project_folder_release(
    app: AppHandle,
    project_id: String,
    user_initiated: bool,
) -> Result<Value, String> {
    check_project_id(&project_id)?;
    if !user_initiated {
        return Err("Project folder unlink requires explicit user action".into());
    }
    #[cfg(target_os = "android")]
    {
        let receipt = mobile_call(
            &app,
            "releaseProjectFolder",
            json!({"projectId":project_id,"userInitiated":true}),
        )?;
        let obj = receipt
            .as_object()
            .ok_or("Invalid Project unlink receipt")?;
        if obj.len() != 4
            || receipt.get("projectId").and_then(Value::as_str) != Some(project_id.as_str())
            || !receipt.get("released").is_some_and(Value::is_boolean)
            || receipt.get("linked") != Some(&json!(false))
            || receipt.get("filesDeleted") != Some(&json!(false))
        {
            return Err("Invalid Project unlink receipt".into());
        }
        return Ok(receipt);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err("Project folder unlink is available on Android only".into())
    }
}

#[tauri::command]
pub async fn document_tree_project_folder_request(
    app: AppHandle,
    project_id: String,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        project_folder_request(app, project_id, user_initiated)
    })
    .await
    .map_err(|_| "Project folder picker interrupted".to_owned())?
}

#[tauri::command]
pub async fn document_tree_project_folder_status(
    app: AppHandle,
    project_id: String,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || project_folder_status(app, project_id))
        .await
        .map_err(|_| "Project folder status interrupted".to_owned())?
}

#[tauri::command]
pub async fn document_tree_project_folder_release(
    app: AppHandle,
    project_id: String,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        project_folder_release(app, project_id, user_initiated)
    })
    .await
    .map_err(|_| "Project folder unlink interrupted".to_owned())?
}

/// List up to 128 direct children at a relative path under the selected tree.
/// The supplied tree URI must match the user's current persisted selection.
pub fn list(app: AppHandle, tree_uri: String, path: Vec<String>) -> Result<Value, String> {
    check_tree_uri(&tree_uri)?;
    check_path(&path, false)?;
    #[cfg(target_os = "android")]
    {
        return mobile_call(&app, "list", json!({"treeUri": tree_uri, "path": path}));
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, tree_uri, path);
        Err("Android document-tree listing is unavailable".into())
    }
}

/// Read at most 1 MiB from a file within the tree; returns base64, not a
/// filesystem path or an unrestricted URI. No modifications are possible.
pub fn open(app: AppHandle, tree_uri: String, path: Vec<String>) -> Result<Value, String> {
    check_tree_uri(&tree_uri)?;
    check_path(&path, true)?;
    #[cfg(target_os = "android")]
    {
        let receipt = mobile_call(
            &app,
            "openDocument",
            json!({"treeUri": tree_uri, "path": path}),
        )?;
        verify_open_receipt(&receipt, &tree_uri, &path)?;
        return Ok(receipt);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, tree_uri, path);
        Err("Android document-tree reading is unavailable".into())
    }
}

/// Read ONLY provider-supplied metadata about a selected tree entry (or root
/// for []), after an explicit UI gesture. Requires persisted READ, not WRITE.
/// No file content, private filesystem paths, write operations or permissions
/// are requested. canWrite/canRename/canDelete are provider capability flags
/// and do not guarantee this app is authorized to mutate the document.
pub fn properties(
    app: AppHandle,
    tree_uri: String,
    path: Vec<String>,
    user_initiated: bool,
) -> Result<Value, String> {
    check_properties_request(&tree_uri, &path, user_initiated)?;
    #[cfg(target_os = "android")]
    {
        let result = mobile_call(
            &app,
            "properties",
            json!({"treeUri":tree_uri,"path":path,"userInitiated":true}),
        )?;
        verify_properties_receipt(&result, &tree_uri, &path)?;
        return Ok(result);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, tree_uri, path);
        Err("SAF properties are available on Android only".into())
    }
}

/// User-initiated Android ACTION_OPEN_DOCUMENT_TREE picker for a separate
/// persisted WRITE grant on the same previously selected readable folder.
/// This function does not silently grant, create, read, rename or delete files.
pub fn request_write(app: AppHandle, user_initiated: bool) -> Result<Value, String> {
    if !user_initiated {
        return Err("Folder write access requires a user gesture".into());
    }
    #[cfg(target_os = "android")]
    {
        return mobile_call(&app, "requestWrite", json!({"userInitiated": true}));
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err("SAF folder write access is available on Android only".into())
    }
}

/// Creates one NEW text/plain .txt document; exactly 0..64KiB UTF-8 bytes.
/// Kotlin independently validates name/path/permission, requires a native
/// confirmation dialog and checks the provider returned a new child URI.
pub fn create_text(
    app: AppHandle,
    tree_uri: String,
    path: Vec<String>,
    name: String,
    content: String,
    user_initiated: bool,
) -> Result<Value, String> {
    check_text_create(&tree_uri, &path, &name, &content, user_initiated)?;
    #[cfg(target_os = "android")]
    {
        return mobile_call(
            &app,
            "createText",
            json!({
                "treeUri": tree_uri,
                "path": path,
                "name": name,
                "content": content,
                "userInitiated": true
            }),
        );
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, tree_uri, path, name, content);
        Err("SAF text creation is available on Android only".into())
    }
}

/// Create exactly one NEW folder within the currently selected SAF tree.
/// UI must call from a user click with user_initiated=true. The Android plugin
/// independently checks the persisted READ+separately selected WRITE grants,
/// shows a native confirmation dialog, rejects existing/colliding names and
/// verifies the new provider document ID, parent, MIME type and display name.
/// It never overwrites, renames or deletes an existing document. A provider
/// race/failure may leave a folder even when verification reports failure.
pub fn create_folder(
    app: AppHandle,
    tree_uri: String,
    path: Vec<String>,
    name: String,
    user_initiated: bool,
) -> Result<Value, String> {
    check_folder_create(&tree_uri, &path, &name, user_initiated)?;
    #[cfg(target_os = "android")]
    {
        return mobile_call(
            &app,
            "createFolder",
            json!({
                "treeUri": tree_uri,
                "path": path,
                "name": name,
                "userInitiated": true
            }),
        );
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, tree_uri, path, name);
        Err("SAF folder creation is available on Android only".into())
    }
}

/// One explicit SAF rename of an EXISTING file or directory within the same
/// user-selected tree and parent. Kotlin checks persisted READ+WRITE access,
/// exact entry IDs/metadata, provider rename support and sibling collisions,
/// then asks for native confirmation. Provider may return a new document ID.
///
/// Outcomes: verified receipt, cancelled receipt, or {renamed:false,
/// verified:false,uncertain:true,providerAttempted:true,message} if the provider
/// may have mutated an entry but the post-rename state cannot be proved.
pub fn rename_entry(
    app: AppHandle,
    tree_uri: String,
    path: Vec<String>,
    name: String,
    user_initiated: bool,
) -> Result<Value, String> {
    check_rename_request(&tree_uri, &path, &name, user_initiated)?;
    #[cfg(target_os = "android")]
    {
        let receipt = mobile_call(
            &app,
            "renameEntry",
            json!({"treeUri":tree_uri,"path":path,"name":name,"userInitiated":true}),
        )?;
        verify_rename_receipt(&receipt, &tree_uri, &path, &name)?;
        return Ok(receipt);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, tree_uri, path, name);
        Err("SAF rename is available on Android only".into())
    }
}

/// Host integration: register these functions in `tauri::generate_handler!`
/// as `android_documents::document_tree_request_write` and
/// `android_documents::document_tree_create_text`; no lib.rs edits required
/// within this worker's ownership.
#[tauri::command]
pub async fn document_tree_request_write(
    app: AppHandle,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || request_write(app, user_initiated))
        .await
        .map_err(|_| "Android write-access picker interrupted".to_owned())?
}

#[tauri::command]
pub async fn document_tree_create_text(
    app: AppHandle,
    tree_uri: String,
    path: Vec<String>,
    name: String,
    content: String,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        create_text(app, tree_uri, path, name, content, user_initiated)
    })
    .await
    .map_err(|_| "Android text-file creation interrupted".to_owned())?
}

/// Frontend invokes `document_tree_create_folder` with camelCase args:
/// `{treeUri, path: string[], name, userInitiated: true}`. `path` is the
/// parent's relative path (empty for root), not a filesystem path or ID.
/// Returns `{created:true,treeUri,path,parentPath,name,isDirectory,mimeType}`
/// after exact provider verification, or `{created:false,cancelled:true}`.
#[tauri::command]
pub async fn document_tree_create_folder(
    app: AppHandle,
    tree_uri: String,
    path: Vec<String>,
    name: String,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        create_folder(app, tree_uri, path, name, user_initiated)
    })
    .await
    .map_err(|_| "Android folder creation interrupted".to_owned())?
}

/// Integration: add `android_documents::document_tree_rename_entry` to the
/// Tauri generate_handler list. UI invokes `document_tree_rename_entry` with
/// {treeUri, path:[...existing entry], name, userInitiated:true}.
#[tauri::command]
pub async fn document_tree_rename_entry(
    app: AppHandle,
    tree_uri: String,
    path: Vec<String>,
    name: String,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        rename_entry(app, tree_uri, path, name, user_initiated)
    })
    .await
    .map_err(|_| "Android SAF rename interrupted".to_owned())?
}

/// Prime: register `android_documents::document_tree_properties` in the
/// Tauri generate_handler! list. Frontend calls `document_tree_properties`
/// with {treeUri,path:[...existing entry] or [],userInitiated:true}.
#[tauri::command]
pub async fn document_tree_properties(
    app: AppHandle,
    tree_uri: String,
    path: Vec<String>,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || properties(app, tree_uri, path, user_initiated))
        .await
        .map_err(|_| "Android SAF properties interrupted".to_owned())?
}

#[cfg(target_os = "android")]
fn mobile_call(app: &AppHandle, command: &str, payload: Value) -> Result<Value, String> {
    let plugin = app
        .try_state::<AndroidDocumentTree<tauri::Wry>>()
        .ok_or_else(|| "Android document-tree plugin is not registered".to_string())?;
    plugin
        .0
        .run_mobile_plugin(command, payload)
        .map_err(|_| "Android document-tree operation was unsuccessful".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_folder_ids_require_catalog_shapes() {
        for valid in [
            "9fa04626-20f5-4f98-a912-a413685c2aac",
            "project-1700000000000-abc123",
        ] {
            assert!(check_project_id(valid).is_ok());
        }
        for invalid in [
            "",
            "../shared",
            "../project-a",
            "PROJECT-xyz",
            "project-a/b",
            "project-a\\b",
            "project-a\n",
            "ffffffff-ffff-ffff-ffff-ffffffffffff/",
            "a".repeat(120).as_str(),
        ] {
            assert!(check_project_id(invalid).is_err());
        }
    }

    #[test]
    fn project_folder_receipts_fail_closed() {
        let id = "9fa04626-20f5-4f98-a912-a413685c2aac";
        let uri = "content://com.android.externalstorage.documents/tree/primary%3AMyFolder";
        let good = json!({"projectId":id,"selected":true,"granted":true,"persisted":true,
            "treeUri":uri,"displayName":"MyFolder","readOnly":true});
        assert!(verify_project_folder_receipt(&good, id, true).is_ok());
        assert!(verify_project_folder_receipt(
            &json!({"projectId":id,"selected":false,"cancelled":true}),
            id,
            true
        )
        .is_ok());
        let status = json!({"projectId":id,"linked":true,"granted":true,
            "treeUri":uri,"displayName":"MyFolder","readOnly":true,"pickerOpen":false});
        assert!(verify_project_folder_receipt(&status, id, false).is_ok());
        assert!(verify_project_folder_receipt(
            &json!({"projectId":id,"linked":true,"granted":false,
            "treeUri":"","displayName":"","readOnly":true,"pickerOpen":false}),
            id,
            false
        )
        .is_ok());
        for malformed in [
            json!({"projectId":id,"selected":true,"granted":true,"persisted":true,
                "treeUri":"file:///sdcard","displayName":"MyFolder","readOnly":true}),
            json!({"projectId":"project-other","selected":true,"granted":true,"persisted":true,
                "treeUri":uri,"displayName":"MyFolder","readOnly":true}),
            json!({"projectId":id,"selected":true,"granted":true,"persisted":true,
                "treeUri":uri,"displayName":"..","readOnly":true}),
            json!({"projectId":id,"selected":true,"granted":true,"persisted":false,
                "treeUri":uri,"displayName":"MyFolder","readOnly":true}),
            json!({"projectId":id,"selected":true,"granted":true,"persisted":true,
                "treeUri":uri,"displayName":"MyFolder","readOnly":false}),
            json!({"projectId":id,"selected":false,"cancelled":true,"treeUri":uri}),
        ] {
            assert!(
                verify_project_folder_receipt(&malformed, id, true).is_err(),
                "{malformed}"
            );
        }
    }

    #[test]
    fn valid_root_and_nested_paths() {
        assert!(check_path(&[], false).is_ok());
        assert!(check_path(&["notes".into(), "file.txt".into()], true).is_ok());
        assert!(check_tree_uri("content://example.documents/tree/primary%3ADocuments").is_ok());
    }

    #[test]
    fn reject_traversal_ambiguous_names_and_large_paths() {
        for segment in ["", ".", "..", "a/b", "a\\b", "a\nsecret", "\u{7f}"] {
            assert!(check_path(&[segment.into()], false).is_err());
        }
        assert!(check_path(&[], true).is_err());
        assert!(check_path(&vec!["d".into(); MAX_DEPTH + 1], false).is_err());
        assert!(check_path(&["a".repeat(MAX_NAME_LENGTH + 1)], true).is_err());
    }

    #[test]
    fn reject_non_content_and_oversized_uris() {
        assert!(check_tree_uri("file:///private/data").is_err());
        assert!(check_tree_uri("content://a\ninvalid").is_err());
        assert!(check_tree_uri(&format!("content://{}", "a".repeat(MAX_TREE_URI_LENGTH))).is_err());
    }

    #[test]
    fn canonical_tree_root_required_not_nested_document_or_malformed_escape() {
        let uri = "content://com.android.externalstorage.documents/tree/primary%3ADocuments";
        assert!(check_tree_uri(uri).is_ok());
        assert!(check_tree_uri("content://example.documents/tree/home%2FMy%20Docs").is_ok());
        for invalid in [
            "content://example.documents/tree/",
            "content://example.documents/document/primary%3ADocuments",
            "content://example.documents/tree/root/document/child",
            "content://example.documents/tree/root?x=1",
            "content://example.documents/tree/root#fragment",
            "content://example.documents/tree/root%ZZ",
            "content://example.documents/tree/root%",
            "content:///tree/root",
            "content://invalid authority/tree/root",
        ] {
            assert!(check_tree_uri(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn validate_new_text_name_and_explicit_consent() {
        let uri = "content://example.documents/tree/primary%3ADocuments";
        assert!(check_text_create(uri, &[], "notes.txt", "Hello, UTF-8", true).is_ok());
        assert!(check_text_create(uri, &["folder".into()], "report.TXT", "ok", true).is_ok());
        assert!(check_text_create(uri, &[], "notes.txt", "ok", false).is_err());
        for bad in [
            "",
            ".txt",
            "foo",
            "a/b.txt",
            "a\\b.txt",
            " x.txt",
            "x.txt ",
            "x\n.txt",
            "a?.txt",
            "a\u{202e}.txt",
            "a\u{e000}.txt",
            "a\u{fdd0}.txt",
        ] {
            assert!(
                check_text_create(uri, &[], bad, "ok", true).is_err(),
                "{bad:?}"
            );
        }
        assert!(
            check_text_create(uri, &[], &format!("{}.txt", "🌿".repeat(125)), "", true).is_ok()
        );
        assert!(
            check_text_create(uri, &[], &format!("{}.txt", "🌿".repeat(126)), "", true).is_err()
        );
    }

    #[test]
    fn checks_utf8_bytes_not_characters_at_64kib_limit() {
        let uri = "content://example.documents/tree/primary%3ADocuments";
        assert!(check_text_create(uri, &[], "new.txt", &"é".repeat(32_768), true).is_ok());
        assert!(check_text_create(uri, &[], "new.txt", &"é".repeat(32_769), true).is_err());
        assert!(check_text_create(uri, &[], "new.txt", &"🌿".repeat(16_384), true).is_ok());
        assert!(check_text_create(uri, &[], "new.txt", &"🌿".repeat(16_385), true).is_err());
    }

    #[test]
    fn new_text_file_depth_includes_final_filename() {
        let tree = "content://example.documents/tree/primary%3ADocuments";
        assert!(check_text_create(
            tree,
            &vec!["dir".into(); MAX_DEPTH - 1],
            "new.txt",
            "",
            true
        )
        .is_ok());
        assert!(
            check_text_create(tree, &vec!["dir".into(); MAX_DEPTH], "new.txt", "", true).is_err()
        );
        assert!(check_text_create(
            tree,
            &vec!["dir".into(); MAX_DEPTH + 1],
            "new.txt",
            "",
            true
        )
        .is_err());
    }

    #[test]
    fn folder_creation_requires_explicit_gesture_and_bounded_parent() {
        let uri = "content://example.documents/tree/primary%3ADocuments";
        assert!(check_folder_create(uri, &[], "New folder", true).is_ok());
        assert!(check_folder_create(uri, &["Existing".into()], "Plans 2026", true).is_ok());
        assert!(check_folder_create(uri, &[], "New folder", false).is_err());
        assert!(check_folder_create(uri, &vec!["a".into(); MAX_DEPTH - 1], "new", true).is_ok());
        assert!(check_folder_create(uri, &vec!["a".into(); MAX_DEPTH], "new", true).is_err());
        assert!(check_folder_create(uri, &["..".into()], "new", true).is_err());
        assert!(check_folder_create("file:///tmp", &[], "new", true).is_err());
    }

    #[test]
    fn folder_names_reject_unsafe_and_confusable_inputs() {
        let uri = "content://example.documents/tree/primary%3ADocuments";
        for bad in [
            "",
            ".hidden",
            "..",
            "trailing.",
            " trailing",
            "trailing ",
            "a/b",
            "a\\b",
            "a:b",
            "a*b",
            "a?b",
            "a|b",
            "a\nb",
            "a\0b",
            "a\u{200d}b",
            "a\u{202e}b",
            "a\u{feff}b",
        ] {
            assert!(check_folder_create(uri, &[], bad, true).is_err(), "{bad:?}");
        }
        // Kotlin uses UTF-16 String.length. Reject oversized supplementary
        // names before sending them to Android too.
        assert!(check_folder_create(uri, &[], &"🌿".repeat(127), true).is_ok());
        assert!(check_folder_create(uri, &[], &"🌿".repeat(128), true).is_err());
        assert!(check_folder_create(uri, &[], &"n".repeat(256), true).is_err());
        assert!(check_folder_create(uri, &[], "Résumé", true).is_ok());
    }

    #[test]
    fn rename_needs_existing_entry_and_explicit_user_gesture() {
        let tree = "content://example.documents/tree/primary%3ADocuments";
        let old = vec!["Reports".into(), "old.txt".into()];
        assert!(check_rename_request(tree, &old, "new.txt", true).is_ok());
        assert!(check_rename_request(tree, &old, "new.txt", false).is_err());
        assert!(check_rename_request(tree, &[], "new.txt", true).is_err());
        assert!(check_rename_request(tree, &old, "old.txt", true).is_err());
        assert!(check_rename_request(tree, &old, "OLD.TXT", true).is_err());
        assert!(check_rename_request("file:///tmp", &old, "new.txt", true).is_err());
        assert!(check_rename_request(tree, &vec!["x".into(); MAX_DEPTH + 1], "new", true).is_err());
        assert!(check_rename_request(tree, &["..".into()], "new", true).is_err());
    }

    #[test]
    fn rename_rejects_hidden_unsafe_and_oversized_new_names() {
        let tree = "content://example.documents/tree/primary%3ADocuments";
        let path = vec!["existing".into()];
        for name in [
            "",
            ".",
            "..",
            ".hidden",
            "suffix.",
            " prefix",
            "suffix ",
            "a/b",
            "a\\b",
            "a:b",
            "a?b",
            "a\n",
            "bad\u{202e}name",
            "bad\u{200d}name",
            "bad\u{feff}name",
            "bad\u{e000}name",
            "bad\u{fdd0}name",
        ] {
            assert!(
                check_rename_request(tree, &path, name, true).is_err(),
                "{name:?}"
            );
        }
        assert!(check_rename_request(tree, &path, &"🌿".repeat(128), true).is_err());
        assert!(check_rename_request(tree, &path, "Résumé.txt", true).is_ok());
    }

    #[test]
    fn rename_bridge_accepts_only_verified_exact_paths_or_uncertainty() {
        let tree = "content://example.documents/tree/primary%3ADocuments";
        let old = vec!["Reports".to_owned(), "old.txt".to_owned()];
        let verified = json!({
            "renamed":true,"verified":true,"uncertain":false,
            "treeUri":tree,"oldPath":["Reports","old.txt"],
            "newPath":["Reports","new.txt"],"parentPath":["Reports"],
            "oldName":"old.txt","name":"new.txt","mimeType":"text/plain",
            "isDirectory":false,"documentIdChanged":true
        });
        assert!(verify_rename_receipt(&verified, tree, &old, "new.txt").is_ok());
        let mut bad = verified.clone();
        bad["newPath"] = json!(["OtherTree", "new.txt"]);
        assert!(verify_rename_receipt(&bad, tree, &old, "new.txt").is_err());
        bad = verified.clone();
        bad["verified"] = json!(false);
        assert!(verify_rename_receipt(&bad, tree, &old, "new.txt").is_err());
        bad = verified.clone();
        bad["isDirectory"] = json!(true);
        assert!(verify_rename_receipt(&bad, tree, &old, "new.txt").is_err());
        let uncertain = json!({"renamed":false,"verified":false,
            "uncertain":true,"providerAttempted":true,
            "message":"Provider rename may have changed the entry"});
        assert!(verify_rename_receipt(&uncertain, tree, &old, "new.txt").is_ok());
        let mut contradictory = uncertain.clone();
        contradictory["cancelled"] = json!(true);
        assert!(verify_rename_receipt(&contradictory, tree, &old, "new.txt").is_err());
        assert!(verify_rename_receipt(&json!({"renamed":true}), tree, &old, "new.txt").is_err());
        assert!(verify_rename_receipt(
            &json!({"renamed":false,"cancelled":true,
            "uncertain":false}),
            tree,
            &old,
            "new.txt"
        )
        .is_ok());
    }

    #[test]
    fn properties_require_read_only_explicit_selection_and_safe_path() {
        let uri = "content://example.documents/tree/primary%3ADocuments";
        assert!(check_properties_request(uri, &[], true).is_ok()); // root allowed
        assert!(
            check_properties_request(uri, &["Folder".into(), "report.txt".into()], true).is_ok()
        );
        assert!(check_properties_request(uri, &[], false).is_err());
        assert!(check_properties_request("file:///data/private", &[], true).is_err());
        assert!(check_properties_request(uri, &vec!["a".into(); MAX_DEPTH], true).is_ok());
        assert!(check_properties_request(uri, &vec!["a".into(); MAX_DEPTH + 1], true).is_err());
        for unsafe_segment in [
            "",
            ".",
            "..",
            ".hidden",
            " trailing",
            "trailing.",
            "a/b",
            "bad\\name",
            "a\n",
            "bad\u{202e}name",
            "bad\u{e000}name",
            "bad\u{fdd0}name",
        ] {
            assert!(
                check_properties_request(uri, &[unsafe_segment.into()], true).is_err(),
                "{unsafe_segment:?}"
            );
        }
        assert!(check_properties_request(uri, &["🌿".repeat(128)], true).is_err());
    }

    #[test]
    fn properties_receipt_accepts_bounded_provider_metadata_and_optional_fields() {
        let uri = "content://example.documents/tree/primary%3ADocuments";
        let path = vec!["Folder".into(), "report.txt".into()];
        let receipt = json!({
            "treeUri":uri, "path":["Folder","report.txt"], "name":"report.txt",
            "isDirectory":false, "mimeType":"text/plain",
            "size":0, "lastModified":1_765_000_000_000_u64,
            "canWrite":true, "canRename":false, "canDelete":false
        });
        assert!(verify_properties_receipt(&receipt, uri, &path).is_ok());
        let mut no_optional = receipt.clone();
        no_optional.as_object_mut().unwrap().remove("size");
        no_optional.as_object_mut().unwrap().remove("lastModified");
        assert!(verify_properties_receipt(&no_optional, uri, &path).is_ok());
        let root = json!({
            "treeUri":uri,"path":[],"name":"Selected Documents",
            "isDirectory":true,"mimeType":"vnd.android.document/directory",
            "canWrite":false,"canRename":false,"canDelete":false
        });
        assert!(verify_properties_receipt(&root, uri, &[]).is_ok());
    }

    #[test]
    fn properties_receipt_rejects_malformed_and_overclaiming_values() {
        let uri = "content://example.documents/tree/primary%3ADocuments";
        let path = vec!["a.txt".into()];
        let valid = json!({
            "treeUri":uri, "path":["a.txt"], "name":"a.txt",
            "isDirectory":false,"mimeType":"text/plain","size":123,
            "canWrite":false,"canRename":false,"canDelete":false
        });
        assert!(verify_properties_receipt(&valid, uri, &path).is_ok());
        let mut bad = valid.clone();
        bad["treeUri"] = json!("content://other.documents/tree/root");
        assert!(verify_properties_receipt(&bad, uri, &path).is_err());
        bad = valid.clone();
        bad["path"] = json!(["other.txt"]);
        assert!(verify_properties_receipt(&bad, uri, &path).is_err());
        bad = valid.clone();
        bad["name"] = json!("other.txt");
        assert!(verify_properties_receipt(&bad, uri, &path).is_err());
        bad = valid.clone();
        bad["canWrite"] = json!("true");
        assert!(verify_properties_receipt(&bad, uri, &path).is_err());
        bad = valid.clone();
        bad["size"] = json!(-1);
        assert!(verify_properties_receipt(&bad, uri, &path).is_err());
        bad = valid.clone();
        bad["size"] = json!(MAX_SAFE_NUMBER + 1);
        assert!(verify_properties_receipt(&bad, uri, &path).is_err());
        bad = valid.clone();
        bad["lastModified"] = json!(-99);
        assert!(verify_properties_receipt(&bad, uri, &path).is_err());
        bad = valid.clone();
        bad["lastModified"] = json!(MAX_PROVIDER_TIMESTAMP_MS + 1);
        assert!(verify_properties_receipt(&bad, uri, &path).is_err());
        bad = valid.clone();
        bad["lastModified"] = json!("2026-11-01");
        assert!(verify_properties_receipt(&bad, uri, &path).is_err());
        bad = valid.clone();
        bad["isDirectory"] = json!(true);
        assert!(verify_properties_receipt(&bad, uri, &path).is_err());
        bad = valid.clone();
        bad["privatePath"] = json!("/storage/emulated/0/private");
        assert!(verify_properties_receipt(&bad, uri, &path).is_err());
        bad = valid.clone();
        bad.as_object_mut().unwrap().remove("canDelete");
        assert!(verify_properties_receipt(&bad, uri, &path).is_err());
    }

    fn valid_open_receipt(mime: &str, size: usize, data: &str) -> Value {
        json!({
            "treeUri":"content://example.documents/tree/primary%3ADocuments",
            "path":["Documents","sample.txt"],
            "mimeType":mime,
            "size":size,
            "encoding":"base64",
            "data":data
        })
    }

    fn check_open(receipt: &Value) -> Result<(), String> {
        verify_open_receipt(
            receipt,
            "content://example.documents/tree/primary%3ADocuments",
            &["Documents".to_owned(), "sample.txt".to_owned()],
        )
    }

    #[test]
    fn saf_open_accepts_exact_text_tiny_image_and_empty_file() {
        // "Hello" and the eight original PNG signature bytes.
        assert!(check_open(&valid_open_receipt("text/plain", 5, "SGVsbG8=")).is_ok());
        assert!(check_open(&valid_open_receipt("image/png", 8, "iVBORw0KGgo=")).is_ok());
        assert!(check_open(&valid_open_receipt("application/octet-stream", 2, "QUI=")).is_ok());
        assert!(check_open(&valid_open_receipt("text/plain", 0, "")).is_ok());
        assert!(check_open(&valid_open_receipt("text/plain", 1, "QQ==")).is_ok());
        // 1 MiB exactly, encoded as all NUL bytes. No content decoding needed.
        let exact = format!("{}AA==", "AAAA".repeat(MAX_OPEN_BYTES / 3));
        assert_eq!(exact.len(), MAX_OPEN_BYTES.div_ceil(3) * 4);
        assert!(check_open(&valid_open_receipt(
            "application/octet-stream",
            MAX_OPEN_BYTES,
            &exact
        ))
        .is_ok());
    }

    #[test]
    fn saf_open_refuses_stale_uri_or_entry_path_and_extra_fields() {
        let valid = valid_open_receipt("text/plain", 5, "SGVsbG8=");
        let mut bad = valid.clone();
        bad["treeUri"] = json!("content://example.documents/tree/other");
        assert!(check_open(&bad).is_err());
        bad = valid.clone();
        bad["path"] = json!(["Documents", "other.txt"]);
        assert!(check_open(&bad).is_err());
        bad = valid.clone();
        bad["path"] = json!(["Documents", "sample.txt", "extra"]);
        assert!(check_open(&bad).is_err());
        bad = valid.clone();
        bad["path"] = json!(["Documents", 7]);
        assert!(check_open(&bad).is_err());
        bad = valid.clone();
        bad["path"] = json!("Documents/sample.txt");
        assert!(check_open(&bad).is_err());
        bad = valid.clone();
        bad["privatePath"] = json!("/sdcard/secret.txt");
        assert!(check_open(&bad).is_err());
        bad = valid.clone();
        bad.as_object_mut().unwrap().remove("data");
        assert!(check_open(&bad).is_err());
        assert!(check_open(&json!({})).is_err());
        assert!(check_open(&json!(null)).is_err());
    }

    #[test]
    fn saf_open_rejects_noncanonical_and_wrongly_padded_base64() {
        for (size, data) in [
            (5, "SGVsbG8"),    // missing required padding
            (5, "SGVsbG8=="),  // extra '='
            (5, "SGVsbG8=\n"), // whitespace
            (5, "SGVs bG8="),  // internal whitespace
            (5, "SGVs-bG8="),  // URL-safe / foreign alphabet
            (5, "SGVs_bG8="),
            (5, "SGVsbG8!"),
            (5, "SGVsbG8é"), // non-ASCII
            (5, "SGVs=bG8"), // embedded '='
            (1, "QR=="),     // nonzero unused low four bits
            (2, "QUJ="),     // nonzero unused low two bits
            (1, "QQ=A"),     // nonterminal padding
            (0, "AA=="),     // nonempty data for zero bytes
            (1, ""),         // empty data with positive size
            (2, "QQ=="),     // decoded length one, declared two
            (3, "QQ=="),     // decoded length one, declared three
            (4, "QQ=="),     // encoded length mismatch
        ] {
            assert!(
                check_open(&valid_open_receipt("text/plain", size, data)).is_err(),
                "Accepted bad base64 {size} {data:?}"
            );
        }
        assert!(check_open(&valid_open_receipt("text/plain", 2, "QUI=")).is_ok());
        assert!(check_open(&valid_open_receipt("text/plain", 3, "QUJD")).is_ok());
    }

    #[test]
    fn saf_open_enforces_one_megabyte_and_strict_types() {
        let valid = valid_open_receipt("text/plain", 5, "SGVsbG8=");
        let mut bad = valid.clone();
        bad["size"] = json!(MAX_OPEN_BYTES + 1);
        assert!(check_open(&bad).is_err());
        bad = valid.clone();
        bad["size"] = json!(-1);
        assert!(check_open(&bad).is_err());
        bad = valid.clone();
        bad["size"] = json!(5.5);
        assert!(check_open(&bad).is_err());
        bad = valid.clone();
        bad["size"] = json!("5");
        assert!(check_open(&bad).is_err());
        bad = valid.clone();
        bad["encoding"] = json!("BASE64");
        assert!(check_open(&bad).is_err());
        bad = valid.clone();
        bad["data"] = json!(["SGVsbG8="]);
        assert!(check_open(&bad).is_err());
        bad = valid.clone();
        bad["mimeType"] = json!(42);
        assert!(check_open(&bad).is_err());
        for mime in [
            "",
            "text",
            "text/plain/extra",
            " text/plain",
            "text/plain ",
            "text/pla\nin",
            "text/pla\u{202e}in",
            "text/with space",
            "vnd.android.document/directory",
        ] {
            assert!(
                check_open(&valid_open_receipt(mime, 5, "SGVsbG8=")).is_err(),
                "{mime:?}"
            );
        }
        assert!(check_open(&valid_open_receipt(
            &format!("a/{}", "x".repeat(255)),
            5,
            "SGVsbG8="
        ))
        .is_err());
        // Even when size claims <=1 MiB, oversized Base64 is not accepted.
        let huge = "AAAA".repeat(MAX_OPEN_BYTES / 3 + 2);
        assert!(check_open(&valid_open_receipt("text/plain", 3, &huge)).is_err());
    }
}
