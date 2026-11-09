//! Conservative authorization derived ONLY from the current user-authored
//! message. Assistant text, website DOM, system instructions and a renderer's
//! claimed action never grant permission. A provider tool call still needs a
//! matching strict native schema, a fresh verified receipt and a bounded run.
use super::{local_agent_tools, provider_tools::ModelToolCall};
use serde_json::{json, Value};
use std::net::IpAddr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intent {
    BrowserOpen { url: String },
    BrowserRead,
    BrowserStop,
    BrowserClick { label: String },
    BrowserFill { label: String, value: String },
    AppOpen { label: String },
    AppStop,
}

fn safe_prompt(prompt: &str) -> bool {
    !prompt.trim().is_empty() && prompt.len() <= 2048 && !prompt.chars().any(char::is_control)
}

/// URI schemes are ASCII case-insensitive. Normalize *only* the eight-byte
/// scheme prefix; keep the host, port, path, query and fragment byte-exact.
/// Never rebuild the dispatched URL from reqwest::Url, which may rewrite it.
pub fn canonical_https_scheme(text: &str) -> Option<String> {
    if !text.get(..8)?.eq_ignore_ascii_case("https://") {
        return None;
    }
    Some(format!("https://{}", &text[8..]))
}

pub fn validate_https_url(text: &str) -> bool {
    if text.len() > 2048
        || text.len() < 9
        || text
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return false;
    }
    let Some(canonical) = canonical_https_scheme(text) else {
        return false;
    };
    let Ok(url) = reqwest::Url::parse(&canonical) else {
        return false;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|p| p != 443)
    {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.parse::<IpAddr>().is_ok()
        || !host.contains('.')
        || host.ends_with('.')
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".localhost")
        || host == "localhost"
        || host.to_ascii_lowercase().contains("xn--") && host.len() > 240
    {
        return false;
    }
    true
}

fn strip_polite(text: &str) -> &str {
    for prefix in ["tolong ", "please ", "sila ", "boleh "] {
        if text
            .get(..prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
        {
            return text[prefix.len()..].trim();
        }
    }
    text
}

fn text_after_verb<'a>(text: &'a str, words: &[&str]) -> Option<&'a str> {
    for verb in words {
        if text
            .get(..verb.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(verb))
            && text[verb.len()..].starts_with(' ')
        {
            return Some(text[verb.len()..].trim());
        }
    }
    None
}

fn safe_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 80
        && label.split_whitespace().count() <= 5
        && label
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch == ' ' || ch == '.' || ch == '_')
        && !label.split_whitespace().any(|word| {
            [
                "and", "dan", "then", "kemudian", "after", "sebelum", "plus", "also",
            ]
            .iter()
            .any(|s| word.eq_ignore_ascii_case(s))
        })
}

fn is_sensitive_ui(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "pay",
        "buy",
        "purchase",
        "checkout",
        "order",
        "submit",
        "confirm",
        "delete",
        "remove",
        "send",
        "post",
        "publish",
        "transfer",
        "withdraw",
        "sign",
        "approve",
        "subscribe",
        "unsubscribe",
        "share",
        "password",
        "otp",
        "token",
        "login",
        "log in",
        "sign in",
        "beli",
        "bayar",
        "hantar",
        "padam",
        "hapus",
        "sah",
        "langgan",
        "pindah wang",
        "bank",
        "wallet",
        "pin",
        "2fa",
        "security",
        "save",
        "simpan",
    ]
    .iter()
    .any(|danger| {
        lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| w == *danger)
            || lower.contains(&format!(" {danger} "))
    })
}

/// `None` means this is an ordinary conversation, not an authorized device
/// command. No guessed action runs. A bare link is a direct open instruction.
pub fn parse(prompt: &str) -> Option<Intent> {
    if !safe_prompt(prompt) {
        return None;
    }
    // Collapse Unicode spaces (e.g. NBSP from pasted chat) while preserving
    // literal URL bytes and command intent. Never normalize assistant prose.
    let normalized = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = strip_polite(normalized.trim());
    if validate_https_url(text) {
        return Some(Intent::BrowserOpen {
            url: canonical_https_scheme(text)?,
        });
    }
    if let Some(target) = text_after_verb(
        text,
        &[
            "open", "buka", "bukak", "bukakan", "launch", "jalankan", "visit", "lawat",
        ],
    ) {
        if validate_https_url(target) {
            return Some(Intent::BrowserOpen {
                url: canonical_https_scheme(target)?,
            });
        }
        let target = target
            .strip_prefix("app ")
            .or_else(|| target.strip_prefix("aplikasi "))
            .or_else(|| target.strip_prefix("App "))
            .or_else(|| target.strip_prefix("Aplikasi "))
            .unwrap_or(target);
        if safe_label(target) && !is_sensitive_ui(target) {
            return Some(Intent::AppOpen {
                label: target.to_owned(),
            });
        }
    }
    if [
        "stop browser",
        "tutup browser",
        "close browser",
        "stop webview",
        "tutup pelayar",
    ]
    .iter()
    .any(|v| text.eq_ignore_ascii_case(v))
    {
        return Some(Intent::BrowserStop);
    }
    if [
        "stop app",
        "stop aplikasi",
        "tutup aplikasi",
        "stop automasi app",
        "stop app automation",
    ]
    .iter()
    .any(|v| text.eq_ignore_ascii_case(v))
    {
        return Some(Intent::AppStop);
    }
    if [
        "read page",
        "inspect page",
        "semak laman",
        "lihat laman",
        "baca laman",
        "check browser",
        "browser status",
    ]
    .iter()
    .any(|v| text.eq_ignore_ascii_case(v))
    {
        return Some(Intent::BrowserRead);
    }
    if let Some(label) = text_after_verb(text, &["click", "klik", "tap", "tekan"]) {
        if safe_label(label) && !is_sensitive_ui(label) {
            return Some(Intent::BrowserClick {
                label: label.to_owned(),
            });
        }
    }
    if let Some(body) = text_after_verb(text, &["fill", "isi", "type", "taip"]) {
        // Quoted value and quoted field are both required. The model cannot
        // invent arbitrary form values, unquoted messages or hidden targets.
        let parts = body.split('"').collect::<Vec<_>>();
        if parts.len() == 5
            && parts[0].trim().is_empty()
            && ["in", "into", "di", "pada", "dalam"]
                .contains(&parts[2].trim().to_ascii_lowercase().as_str())
            && parts[4].trim().is_empty()
            && parts[1].len() <= 500
            && !parts[1].is_empty()
            && safe_label(parts[3])
            && !is_sensitive_ui(parts[3])
            && !is_sensitive_ui(parts[1])
        {
            return Some(Intent::BrowserFill {
                label: parts[3].to_owned(),
                value: parts[1].to_owned(),
            });
        }
    }
    None
}

pub fn definitions_for(scope: &Intent) -> Vec<Value> {
    let allow: &[&str] = match scope {
        Intent::BrowserOpen { .. } => &["android_browser_start"],
        Intent::BrowserRead => &["android_browser_status", "android_browser_snapshot"],
        Intent::BrowserStop => &["android_browser_stop"],
        Intent::BrowserClick { .. } => &["android_browser_snapshot", "android_browser_click"],
        Intent::BrowserFill { .. } => &["android_browser_snapshot", "android_browser_fill"],
        Intent::AppOpen { .. } => &["android_app_start"],
        Intent::AppStop => &["android_app_stop"],
    };
    local_agent_tools::definitions()
        .into_iter()
        .filter(|tool| {
            tool["name"]
                .as_str()
                .is_some_and(|name| allow.contains(&name))
        })
        .collect()
}

/// This is *not* NLP authority: model proposals must be a member of the
/// original user's narrow scope, and no page text can enlarge that scope.
#[derive(Default)]
pub struct Progress {
    pub tool_count: usize,
    pub primary_done: bool,
    pub status_done: bool,
    pub snapshot: Option<Value>,
    pub snapshot_consumed: bool,
    pub seen_names: Vec<String>,
}

pub fn authorize(
    scope: &Intent,
    expected_package: Option<&str>,
    proposal: &ModelToolCall,
    state: &Progress,
) -> Result<(), String> {
    local_agent_tools::validate(&proposal.name, &proposal.arguments)?;
    let name = proposal.name.as_str();
    let arg = &proposal.arguments;
    if state.tool_count >= 3 {
        return Err("Android agent step budget exhausted".into());
    }
    if state.seen_names.iter().any(|seen| seen == name) {
        return Err("Duplicate Android agent action is not authorized".into());
    }
    let accepted = match scope {
        Intent::BrowserOpen { url } => match name {
            "android_browser_start" => {
                !state.primary_done
                    && arg["url"]
                        .as_str()
                        .and_then(canonical_https_scheme)
                        .as_deref()
                        == Some(url)
            }
            "android_browser_status" | "android_browser_snapshot" => {
                state.primary_done && !state.snapshot_consumed
            }
            _ => false,
        },
        Intent::BrowserRead => {
            matches!(name, "android_browser_status" | "android_browser_snapshot")
        }
        Intent::BrowserStop => {
            matches!(name, "android_browser_stop" | "android_browser_status")
                && (name != "android_browser_stop" || !state.primary_done)
        }
        Intent::BrowserClick { label } => match name {
            "android_browser_snapshot" => state.snapshot.is_none(),
            "android_browser_click" => {
                !state.primary_done && eligible_control(state, arg, label, false)
            }
            _ => false,
        },
        Intent::BrowserFill { label, value } => match name {
            "android_browser_snapshot" => state.snapshot.is_none(),
            "android_browser_fill" => {
                !state.primary_done
                    && arg["value"].as_str() == Some(value)
                    && eligible_control(state, arg, label, true)
            }
            _ => false,
        },
        Intent::AppOpen { .. } => match name {
            "android_app_start" => {
                !state.primary_done
                    && expected_package.is_some()
                    && arg["targetPackage"].as_str() == expected_package
            }
            "android_app_status" => state.primary_done,
            _ => false,
        },
        Intent::AppStop => {
            matches!(name, "android_app_stop" | "android_app_status")
                && (name != "android_app_stop" || !state.primary_done)
        }
    };
    if accepted {
        Ok(())
    } else {
        Err("Proposed Android action is unrelated to current user's specific instruction".into())
    }
}

fn eligible_control(state: &Progress, arg: &Value, user_label: &str, needs_fill: bool) -> bool {
    let Some(snapshot) = state.snapshot.as_ref() else {
        return false;
    };
    let Some(id) = arg["snapshotId"].as_i64() else {
        return false;
    };
    if id < 1 || snapshot["snapshotId"].as_i64() != Some(id) || state.snapshot_consumed {
        return false;
    }
    let Some(index) = arg["index"].as_u64() else {
        return false;
    };
    let Some(controls) = snapshot["elements"].as_array() else {
        return false;
    };
    let Some(control) = controls.iter().find(|v| v["index"].as_u64() == Some(index)) else {
        return false;
    };
    let label = control["label"].as_str().unwrap_or("");
    let tag = control["tag"].as_str().unwrap_or("");
    let authored = user_label.to_lowercase();
    let actual = label.to_lowercase();
    !actual.is_empty()
        && actual.len() >= 2
        && (actual == authored || actual.contains(&authored) && authored.len() >= 3)
        && !is_sensitive_ui(&actual)
        && !is_sensitive_ui(tag)
        && (!needs_fill || control["fillable"] == true)
}

pub fn record(scope: &Intent, name: &str, receipt: &Value, state: &mut Progress) {
    state.tool_count += 1;
    state.seen_names.push(name.to_owned());
    if name == "android_browser_snapshot" {
        state.snapshot = Some(receipt.clone());
    }
    if matches!(name, "android_browser_click" | "android_browser_fill") {
        state.snapshot_consumed = true;
    }
    if name.ends_with("_start")
        || name.ends_with("_stop")
        || name.ends_with("_click")
        || name.ends_with("_fill")
    {
        state.primary_done = true;
    }
    if name.ends_with("_status") {
        state.status_done = true;
    }
    // An ordinary browser read only requires at most one snapshot; no followup
    // page-supplied instructions ever become additional user authorization.
    if matches!(scope, Intent::BrowserRead) && name == "android_browser_snapshot" {
        state.primary_done = true;
    }
}

pub fn direct_arguments(
    scope: &Intent,
    action: &str,
    args: &Value,
    package: Option<&str>,
) -> Result<(String, Value), String> {
    match (scope, action) {
        (Intent::BrowserOpen { url }, "android_browser_start")
            if args.as_object().is_some_and(|o| o.len() == 1)
                && args["url"]
                    .as_str()
                    .and_then(canonical_https_scheme)
                    .as_deref()
                    == Some(url) =>
        {
            Ok((action.into(), json!({"url":url})))
        }
        (Intent::AppOpen { label }, "android_app_start")
            if args.as_object().is_some_and(|o| o.len() == 1)
                && args["query"]
                    .as_str()
                    .is_some_and(|q| q.eq_ignore_ascii_case(label)) =>
        {
            let target = package.ok_or("Trusted Android app package not found")?;
            Ok((action.into(), json!({"targetPackage":target})))
        }
        (Intent::BrowserStop, "android_browser_stop") | (Intent::AppStop, "android_app_stop")
            if args.as_object().is_some_and(|o| o.is_empty()) =>
        {
            Ok((action.into(), json!({})))
        }
        _ => Err("Unapproved direct app automation command".into()),
    }
}

pub fn native_message(action: &str, receipt: &Value) -> Result<String, String> {
    Ok(match action {
        "android_browser_start" => format!("Android browser launch confirmed: isolated WebView running={}, page ready={}. No further page actions were performed.",
            receipt["running"]==true,receipt["ready"]==true),
        "android_app_start" => format!("Android confirmed launching app package {}. Blue Accessibility indicator session active={}, visible={}. No tap or typing was performed.",
            receipt["targetPackage"].as_str().ok_or("Native app receipt lacked package")?,
            receipt["active"]==true,receipt["visible"]==true),
        "android_browser_stop" => "Android browser stop confirmed. No page is running.".to_owned(),
        "android_app_stop" => "Android automation indicator stopped. Third-party application was not force-stopped.".to_owned(),
        _ => return Err("Unsupported direct native action reply".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uppercase_https_scheme_keeps_exact_authority_path_query_and_fragment() {
        for (original, expected) in [
            ("Https://example.org/", "https://example.org/"),
            ("HTTPS://EXAMPLE.ORG/", "https://EXAMPLE.ORG/"),
            (
                "HtTpS://EXAMPLE.ORG/CaseSensitive?ID=XyZ#Fragment",
                "https://EXAMPLE.ORG/CaseSensitive?ID=XyZ#Fragment",
            ),
        ] {
            assert!(validate_https_url(original), "{original}");
            assert_eq!(canonical_https_scheme(original).as_deref(), Some(expected));
            assert_eq!(
                parse(original),
                Some(Intent::BrowserOpen {
                    url: expected.to_owned()
                })
            );
            assert_eq!(
                parse(&format!("buka {original}")),
                Some(Intent::BrowserOpen {
                    url: expected.to_owned()
                })
            );
            let scope = parse(original).unwrap();
            // The UI may send the original text or normalize only its scheme.
            for argument_url in [original, expected] {
                let (_, args) = direct_arguments(
                    &scope,
                    "android_browser_start",
                    &json!({"url":argument_url}),
                    None,
                )
                .unwrap();
                assert_eq!(args["url"], expected);
                let model_call = ModelToolCall {
                    id: "call_1".into(),
                    name: "android_browser_start".into(),
                    arguments: json!({"url":argument_url}),
                };
                assert!(authorize(&scope, None, &model_call, &Progress::default()).is_ok());
            }
        }
        for invalid in [
            "HTTPS://127.0.0.1/",
            "Https://localhost/",
            "HTTPS://service.local/",
            "Https://user:pass@example.org/",
            "Https://example.org/another",
        ] {
            if invalid.ends_with("another") {
                let scope = parse("Https://example.org/").unwrap();
                assert!(direct_arguments(
                    &scope,
                    "android_browser_start",
                    &json!({"url":invalid}),
                    None
                )
                .is_err());
            } else {
                assert!(!validate_https_url(invalid), "{invalid}");
            }
        }
    }

    #[test]
    fn direct_intent_is_tight_and_untrusted_prose_cannot_launch() {
        assert_eq!(
            parse("https://example.org/a"),
            Some(Intent::BrowserOpen {
                url: "https://example.org/a".into()
            })
        );
        assert_eq!(
            parse("buka YouTube"),
            Some(Intent::AppOpen {
                label: "YouTube".into()
            })
        );
        assert_eq!(
            parse("bukak\u{00a0}YouTube"),
            Some(Intent::AppOpen {
                label: "YouTube".into()
            })
        );
        assert_eq!(
            parse("jalankan aplikasi YouTube"),
            Some(Intent::AppOpen {
                label: "YouTube".into()
            })
        );
        for bad in [
            "send this message and open YouTube",
            "https://a.com https://b.com",
            "open https://localhost/",
            "open https://127.0.0.1/",
            "buka com.android.settings",
            "buka YouTube dan padam semua",
        ] {
            if bad != "buka com.android.settings" {
                assert!(parse(bad).is_none(), "{bad}");
            }
        }
        assert!(direct_arguments(
            &parse("buka YouTube").unwrap(),
            "android_app_start",
            &json!({"query":"Other"}),
            Some("com.google.android.youtube")
        )
        .is_err());
        assert!(direct_arguments(
            &parse("buka YouTube").unwrap(),
            "android_app_start",
            &json!({"query":"YouTube"}),
            Some("com.google.android.youtube")
        )
        .is_ok());
    }

    #[test]
    fn proposal_must_match_exact_user_link_or_trusted_app_target() {
        let scope = parse("https://example.org/article").unwrap();
        let p = ModelToolCall {
            id: "call_1".into(),
            name: "android_browser_start".into(),
            arguments: json!({"url":"https://evil.example/"}),
        };
        assert!(authorize(&scope, None, &p, &Progress::default()).is_err());
        let mut yes = p.clone();
        yes.arguments = json!({"url":"https://example.org/article"});
        assert!(authorize(&scope, None, &yes, &Progress::default()).is_ok());
        let scope = parse("buka YouTube").unwrap();
        let app = ModelToolCall {
            id: "call_2".into(),
            name: "android_app_start".into(),
            arguments: json!({"targetPackage":"com.notyoutube.app"}),
        };
        assert!(authorize(
            &scope,
            Some("com.google.android.youtube"),
            &app,
            &Progress::default()
        )
        .is_err());
    }

    #[test]
    fn index_click_needs_matching_fresh_snapshot_and_non_sensitive_label() {
        let scope = parse("klik Search").unwrap();
        let call = ModelToolCall {
            id: "call_3".into(),
            name: "android_browser_click".into(),
            arguments: json!({"snapshotId":4,"index":1}),
        };
        assert!(authorize(&scope, None, &call, &Progress::default()).is_err());
        let mut state = Progress {
            snapshot: Some(json!({"snapshotId":4,"elements":[
            {"index":1,"label":"Search","tag":"button","fillable":false},
            {"index":2,"label":"Pay now","tag":"button","fillable":false}]})),
            ..Progress::default()
        };
        assert!(authorize(&scope, None, &call, &state).is_ok());
        state.snapshot_consumed = true;
        assert!(authorize(&scope, None, &call, &state).is_err());
        state.snapshot_consumed = false;
        let dangerous = ModelToolCall {
            arguments: json!({"snapshotId":4,"index":2}),
            ..call
        };
        assert!(authorize(&scope, None, &dangerous, &state).is_err());
    }
}
