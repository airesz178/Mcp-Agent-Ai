//! Explicit Android share-sheet handoff for one bounded Markdown conversation.
//! Caller supplies the already exported Markdown and direct user-gesture flag.
//! Neither this wrapper nor the Android plugin saves, uploads or logs content.
//!
//! Prime integration: `mod android_review_share;`, register `init()` only on
//! Android, then register `android_review_share::review_share_markdown` in the
//! `tauri::generate_handler!` macro. The UI should pass `userInitiated: true`
//! only on an actual Share button click.

#[cfg(target_os = "android")]
use serde_json::json;
use serde_json::Value;
use tauri::AppHandle;

#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
#[cfg(target_os = "android")]
use tauri::{Manager, Runtime};

const MAX_MARKDOWN_BYTES: usize = 128 * 1024;
const SESSION_MARKER: &str = "# Mcp Agent Ai conversation\n\nProvider: ";

#[cfg(target_os = "android")]
struct ReviewShareHandle<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("review-share")
        .setup(|app, api| {
            let plugin = api.register_android_plugin("ai.mcpagent.app", "ReviewSharePlugin")?;
            app.manage(ReviewShareHandle(plugin));
            Ok(())
        })
        .build()
}

fn validate_share_markdown(markdown: &str, user_initiated: bool) -> Result<(), String> {
    if !user_initiated {
        return Err("Sharing requires an explicit user action".into());
    }
    if markdown.len() > MAX_MARKDOWN_BYTES
        || !markdown.starts_with(SESSION_MARKER)
        || !markdown.contains("\nModel: ")
        || !markdown.contains("\n\n## ")
        || markdown
            .chars()
            .any(|ch| ch == '\0' || (ch.is_control() && !matches!(ch, '\r' | '\n' | '\t')))
    {
        return Err("Invalid or oversized conversation Markdown (128 KiB max)".into());
    }
    Ok(())
}

/// Checks the exact user-intent flag before sending the payload to Kotlin.
/// Chooser opened means only that Android displayed the chooser: whether the
/// user selects another app or completes sharing is not observable here.
pub fn share_markdown(app: AppHandle, text: String, user_initiated: bool) -> Result<Value, String> {
    validate_share_markdown(&text, user_initiated)?;
    #[cfg(target_os = "android")]
    {
        let plugin = app
            .try_state::<ReviewShareHandle<tauri::Wry>>()
            .ok_or_else(|| "Android Markdown sharing plugin not registered".to_string())?;
        return plugin
            .0
            .run_mobile_plugin(
                "shareMarkdown",
                json!({
                    "text": text,
                    "userInitiated": true
                }),
            )
            .map_err(|_| "Android Markdown share chooser failed".into());
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, text);
        Err("Android Markdown sharing is available on Android only".into())
    }
}

#[tauri::command]
pub async fn review_share_markdown(
    app: AppHandle,
    text: String,
    user_initiated: bool,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || share_markdown(app, text, user_initiated))
        .await
        .map_err(|_| "Android Markdown sharing interrupted".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(body: &str) -> String {
        format!("# Mcp Agent Ai conversation\n\nProvider: openai\nModel: test-model\n\n## User\n\n{body}\n")
    }

    #[test]
    fn requires_explicit_user_intent_and_session_marker() {
        let text = sample("Synthetic message");
        assert!(validate_share_markdown(&text, true).is_ok());
        assert!(validate_share_markdown(&text, false).is_err());
        assert!(validate_share_markdown("## User\n\nAnything", true).is_err());
        assert!(
            validate_share_markdown("# Mcp Agent Ai conversation\n\nModel: fake", true).is_err()
        );
    }

    #[test]
    fn checks_utf8_bytes_and_rejects_control_characters() {
        let prefix = sample("");
        assert!(validate_share_markdown(&sample("🌿"), true).is_ok());
        assert!(validate_share_markdown(&sample("Secret\0text"), true).is_err());
        assert!(validate_share_markdown(&sample("bad\u{0001}control"), true).is_err());
        assert!(validate_share_markdown(
            &sample(
                "🦀"
                    .repeat((MAX_MARKDOWN_BYTES - prefix.len()) / 4)
                    .as_str()
            ),
            true
        )
        .is_ok());
        assert!(validate_share_markdown(
            &sample("🦀".repeat(MAX_MARKDOWN_BYTES / 4).as_str()),
            true
        )
        .is_err());
    }

    #[test]
    fn does_not_accept_unrelated_text_with_only_partial_markers() {
        for candidate in [
            "",
            "Provider: openai\nModel: x\n\n## User\n",
            "# Mcp Agent Ai conversation\n\nProvider: a\nModel: b\n\nNo messages",
        ] {
            assert!(validate_share_markdown(candidate, true).is_err());
        }
    }
}
