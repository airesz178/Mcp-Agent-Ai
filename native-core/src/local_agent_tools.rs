//! User-intent-authorized, app-owned Android tool descriptions and receipts.
//! No Android/Tauri API, network request or model-text command dispatch lives
//! here. The host alone performs one independently intent-validated operation.
use serde_json::{json, Value};
use std::{future::Future, pin::Pin};

pub trait AndroidLocalToolHost: Send + Sync {
    fn execute<'a>(
        &'a self,
        name: &'a str,
        arguments: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

    /// Resolve exactly one installed, launchable app using Android's trusted
    /// PackageManager. An untrusted renderer/model must never assert the
    /// package-label binding itself. Hosts without a resolver fail closed.
    fn resolve_app_label<'a>(
        &'a self,
        _query: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(String, String)>, String>> + Send + 'a>> {
        Box::pin(async { Err("Trusted Android app-label resolver is not installed".into()) })
    }
}

const NAMES: [&str; 15] = [
    "android_browser_status",
    "android_browser_start",
    "android_browser_snapshot",
    "android_browser_click",
    "android_browser_fill",
    "android_browser_stop",
    "android_app_status",
    "android_app_start",
    "android_app_stop",
    "android_app_snapshot",
    "android_app_click",
    "android_app_fill",
    "android_app_gesture",
    "security_audit_analyze",
    "reverse_engineering_dex_header",
];

const MAX_AUDIT_BYTES: usize = 4096;
const DEX_HEADER_BYTES: usize = 112;

/// These modules are compiled into native-core. Internal status probes are
/// deliberately not model-facing tools or installable executable packages.
pub(crate) fn curated_host_execute(name: &str, args: &Value) -> Option<Result<Value, String>> {
    Some(match name {
        "security_audit_internal_status" => Ok(json!({"supported":true,"ready":true})),
        "reverse_engineering_internal_status" => Ok(json!({"supported":true,"ready":true})),
        "security_audit_analyze" => audit_text(args),
        "reverse_engineering_dex_header" => inspect_dex_header(args),
        _ => return None,
    })
}

/// Returns only aggregate line counts; never includes the submitted text,
/// matched snippets, tokens, hashes of secrets, or candidate credentials.
fn audit_text(args: &Value) -> Result<Value, String> {
    validate("security_audit_analyze", args)?;
    let input = args["text"].as_str().ok_or("Audit text required")?;
    let mut counts = [0u64; 4];
    for line in input.lines() {
        let text = line.to_ascii_lowercase();
        let sensitive_key = ["api_key", "apikey", "password", "passwd", "secret", "access_token",
            "auth_token", "authorization", "bearer ", "private_key"];
        let private_key_block=text.contains("-----begin") && text.contains("private key-----");
        let token_literal=text.split("sk-").skip(1).any(|tail|tail.bytes()
            .take_while(|ch|ch.is_ascii_alphanumeric() || *ch==b'_' || *ch==b'-')
            .count()>=12);
        if (sensitive_key.iter().any(|key| text.contains(key))
            && (text.contains('=') || text.contains(':') || text.contains("bearer ")))
            || private_key_block || token_literal {
            counts[0] += 1;
        }
        if text.contains("http://") || text.contains("usescleartexttraffic=\"true\"") {
            counts[1] += 1;
        }
        if ["eval(", "runtime.getruntime().exec(", "processbuilder(", "os.system(",
            "subprocess.popen(", "subprocess.run(", "sh -c "]
            .iter().any(|pattern| text.contains(pattern)) {
            counts[2] += 1;
        }
        if ["md5(", "sha1(", "sha-1", "des/ecb", "rc4", "android:exported=\"true\""]
            .iter().any(|pattern| text.contains(pattern)) {
            counts[3] += 1;
        }
    }
    let level = if counts[0] > 0 { "high" }
        else if counts.iter().any(|n| *n > 0) { "review" } else { "none_detected" };
    Ok(json!({"scannedBytes":input.len(),"matchedLines":{
        "possibleCredentials":counts[0],"cleartextTransport":counts[1],
        "dynamicExecution":counts[2],"weakCryptoOrExposure":counts[3]},
        "riskLevel":level,"heuristicOnly":true}))
}

fn hex_nibble(ch: u8) -> Option<u8> {
    match ch { b'0'..=b'9' => Some(ch-b'0'), b'a'..=b'f'=>Some(ch-b'a'+10),
        b'A'..=b'F'=>Some(ch-b'A'+10), _=>None }
}

fn inspect_dex_header(args: &Value) -> Result<Value, String> {
    validate("reverse_engineering_dex_header", args)?;
    let hex = args["hexHeader"].as_str().ok_or("DEX header hex required")?;
    let mut bytes = [0u8; DEX_HEADER_BYTES];
    for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex_nibble(pair[0]).ok_or("DEX header is not hex")? << 4)
            | hex_nibble(pair[1]).ok_or("DEX header is not hex")?;
    }
    if &bytes[0..4] != b"dex\n" || bytes[7] != 0 {
        return Err("Not a standard Android DEX header".into());
    }
    let version = std::str::from_utf8(&bytes[4..7]).map_err(|_| "DEX version invalid")?;
    if !matches!(version, "035"|"036"|"037"|"038"|"039"|"040") {
        return Err("Unsupported DEX header version".into());
    }
    let read_u32 = |off: usize| u32::from_le_bytes(bytes[off..off+4].try_into().unwrap());
    let file_size = read_u32(32);
    let header_size = read_u32(36);
    let endian_tag = read_u32(40);
    if header_size != DEX_HEADER_BYTES as u32 || file_size < header_size
        || endian_tag != 0x1234_5678 {
        return Err("DEX header size, file size or endianness is invalid".into());
    }
    let data_size = read_u32(104);
    let data_off = read_u32(108);
    let map_off = read_u32(52);
    if data_off < header_size || data_off % 4 != 0
        || data_size == 0 || data_off.checked_add(data_size) != Some(file_size)
        || map_off < data_off || map_off % 4 != 0
        || map_off.checked_add(4).is_none_or(|end|end>file_size) {
        return Err("DEX map or data section is inconsistent".into());
    }
    let link_size = read_u32(44);
    let link_off = read_u32(48);
    if (link_size==0 && link_off!=0) || (link_size>0 &&
        (link_off < header_size || link_off % 4 != 0 ||
            link_off.checked_add(link_size).is_none_or(|n| n>file_size))) {
        return Err("DEX link section is inconsistent".into());
    }
    let mut section_counts = [0u32; 6];
    let mut previous_end = header_size;
    for (i, item_width) in [4u32,4,12,8,8,32].iter().enumerate() {
        let count = read_u32(56+i*8);
        let offset = read_u32(60+i*8);
        section_counts[i] = count;
        if count==0 && offset!=0 || count>0 && (
            offset < previous_end || offset % 4!=0 ||
            count.checked_mul(*item_width).and_then(|size|offset.checked_add(size))
                .is_none_or(|end|end>file_size || end>data_off)) {
            return Err("DEX ID section offsets or counts are inconsistent".into());
        }
        if count>0 { previous_end = offset+count*item_width; }
    }
    Ok(json!({"validHeader":true,"dexVersion":version,"fileSizeBytes":file_size,
        "headerSizeBytes":header_size,"endian":"little","mapOffset":map_off,
        "sectionCounts":{"strings":section_counts[0],"types":section_counts[1],
            "prototypes":section_counts[2],"fields":section_counts[3],
            "methods":section_counts[4],"classes":section_counts[5]},
        "checksumVerified":false,"headerOnly":true}))
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name":name,
        "description":description,
        "inputSchema":{
            "type":"object",
            "properties":properties,
            "required":required,
            "additionalProperties":false
        }
    })
}

/// These are *real* native functions, exposed only for the exact scope of the
/// currently user-authored chat instruction. Browser interactions use one
/// fresh, consumed DOM snapshot. A local
/// app session opens an app and shows the Accessibility indicator; it does not
/// implement taps, gestures, typing or third-party app content inspection.
pub fn definitions() -> Vec<Value> {
    let snapshot = json!({
        "snapshotId":{"type":"integer","minimum":1},
        "index":{"type":"integer","minimum":1,"maximum":48}
    });
    vec![
        tool(NAMES[0], "Check the state of the isolated app-owned Android browser; no page is opened.", json!({}), &[]),
        tool(NAMES[1], "Ask the user to approve opening ONE HTTPS page in an isolated Android WebView. This has a native consent dialog; it may be cancelled.",
            json!({"url":{"type":"string","minLength":9,"maxLength":2048}}), &["url"]),
        tool(NAMES[2], "Read a bounded current DOM snapshot of an already running app-owned browser; returns a snapshotId and indexed visible controls.",json!({}),&[]),
        tool(NAMES[3], "Click one numbered control from a fresh browser snapshot. A click can change a website. Requires individual approval; the snapshot is consumed.",
            snapshot.clone(), &["snapshotId","index"]),
        tool(NAMES[4], "Fill one permitted non-password control from a fresh browser snapshot with bounded text. Requires individual approval; the snapshot is consumed.",
            json!({"snapshotId":snapshot["snapshotId"],"index":snapshot["index"],
                "value":{"type":"string","maxLength":2048}}), &["snapshotId","index","value"]),
        tool(NAMES[5], "Stop and dispose of the app-owned Android browser. Does not close another app.", json!({}), &[]),
        tool(NAMES[6], "Check current accessibility automation session/blue indicator state. No app is opened.",json!({}),&[]),
        tool(NAMES[7], "After separate user approval, request Android to open one installed third-party app and show the blue 'Mcp Agent Ai is using this app' indicator. Does not tap or operate the app.",
            json!({"targetPackage":{"type":"string","minLength":3,"maxLength":160}}),&["targetPackage"]),
        tool(NAMES[8], "Stop the app automation indicator session. Does not force-stop the third-party app.",json!({}),&[]),
        tool(NAMES[9], "With separate on-device approval, obtain a bounded snapshot of the active, permitted Android app. Realtime mode may return safe Accessibility controls; screenshot mode needs a real screen capture grant. Never reads a locked screen.",json!({}),&[]),
        tool(NAMES[10], "With separate on-device approval, click ONE safe Accessibility control index or tap ONE screenshot coordinate. Requires a fresh snapshot from the same approved app session.",json!({
            "snapshotId":{"type":"string","minLength":8,"maxLength":128},
            "index":{"type":"integer","minimum":1,"maximum":48},
            "x":{"type":"integer","minimum":0,"maximum":8192},
            "y":{"type":"integer","minimum":0,"maximum":8192}
        }),&["snapshotId"]),
        tool(NAMES[11], "With separate on-device approval, fill ONE safe, non-password Accessibility text control from a fresh snapshot. Never fills sensitive inputs.",json!({
            "snapshotId":{"type":"string","minLength":8,"maxLength":128},
            "index":{"type":"integer","minimum":1,"maximum":48},
            "value":{"type":"string","maxLength":2048}
        }),&["snapshotId","index","value"]),
        tool(NAMES[12], "With separate on-device approval, perform ONE bounded tap or swipe on the permitted app screen. Requires a fresh approved screenshot snapshot and OS gesture permission.",json!({
            "snapshotId":{"type":"string","minLength":8,"maxLength":128},
            "kind":{"type":"string","enum":["tap","swipe"]},
            "x":{"type":"integer","minimum":0,"maximum":8192},
            "y":{"type":"integer","minimum":0,"maximum":8192},
            "toX":{"type":"integer","minimum":0,"maximum":8192},
            "toY":{"type":"integer","minimum":0,"maximum":8192},
            "durationMs":{"type":"integer","minimum":50,"maximum":1500}
        }),&["snapshotId","kind","x","y","toX","toY","durationMs"]),
        tool(NAMES[13], "Analyze at most 4096 bytes of supplied source or configuration text for static risk patterns; returns counts only, never the input or any matched secret.",
            json!({"text":{"type":"string","minLength":1,"maxLength":4096}}), &["text"]),
        tool(NAMES[14], "Inspect exactly 112 bytes of an Android standard DEX header encoded as 224 hex characters. Checks magic, supported version, endian tag, file size and section bounds; does not load an APK or verify a full file checksum.",
            json!({"hexHeader":{"type":"string","minLength":224,"maxLength":224}}), &["hexHeader"]),
    ]
}

pub fn validate(name: &str, args: &Value) -> Result<(), String> {
    if !NAMES.contains(&name) {
        return Err("Unknown Android local agent tool".into());
    }
    super::mcp_client::validate_model_arguments(&definitions(), name, args)?;
    match name {
        "android_browser_start" => {
            let url = args["url"].as_str().ok_or("Browser URL required")?;
            if !super::local_agent_intent::validate_https_url(url) {
                return Err("Only an HTTPS browser URL may be opened".into());
            }
        }
        "android_browser_fill" => {
            if args["value"]
                .as_str()
                .is_none_or(|s| s.chars().any(char::is_control))
            {
                return Err("Browser fill text contains unsupported control characters".into());
            }
        }
        "android_app_start" => {
            let name = args["targetPackage"].as_str().unwrap_or("");
            let segments: Vec<&str> = name.split('.').collect();
            if segments.len() < 2
                || segments.len() > 9
                || name.len() > 160
                || segments.iter().any(|part| {
                    part.is_empty()
                        || !part.as_bytes()[0].is_ascii_alphabetic() && part.as_bytes()[0] != b'_'
                        || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                })
                || name == "ai.mcpagent.app"
                || name == "android"
                || name.starts_with("com.android.")
                || name.starts_with("com.google.android.permissioncontroller")
            {
                return Err("App tool requires one permitted third-party package".into());
            }
        }
        "android_app_click" => {
            let index=args.get("index").and_then(Value::as_u64);
            let x=args.get("x").and_then(Value::as_u64);
            let y=args.get("y").and_then(Value::as_u64);
            if !((index.is_some() && x.is_none() && y.is_none()) ||
                 (index.is_none() && x.is_some() && y.is_some())) {
                return Err("Click requires exactly one control index OR one coordinate pair".into());
            }
        }
        "android_app_fill" => {
            if args["value"].as_str().is_none_or(|s|s.chars().any(char::is_control)) {
                return Err("App text contains unsupported control characters".into());
            }
        }
        "android_app_gesture" => {
            if args["kind"]=="tap" &&
               (args["x"]!=args["toX"]||args["y"]!=args["toY"]) {
                return Err("Tap gesture cannot include a swipe destination".into());
            }
        }
        "security_audit_analyze" => {
            let text = args["text"].as_str().ok_or("Audit text required")?;
            if text.is_empty() || text.len() > MAX_AUDIT_BYTES || text.contains('\0') {
                return Err("Audit text must be 1-4096 UTF-8 bytes without NUL".into());
            }
        }
        "reverse_engineering_dex_header" => {
            let hex = args["hexHeader"].as_str().ok_or("DEX header hex required")?;
            if hex.len() != DEX_HEADER_BYTES*2 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("DEX inspection requires exactly 112 hex-encoded bytes".into());
            }
        }
        _ => {}
    }
    Ok(())
}

/// Construct the payload for a verified native tool. Only the case of the
/// HTTPS scheme may change; never rewrite authority, host, path or query.
pub fn dispatch_arguments(name: &str, args: &Value) -> Result<Value, String> {
    validate(name, args)?;
    if name == "android_browser_start" {
        let raw = args["url"].as_str().ok_or("Browser URL required")?;
        let canonical = super::local_agent_intent::canonical_https_scheme(raw)
            .ok_or("Invalid browser URL scheme")?;
        Ok(json!({"url":canonical}))
    } else {
        Ok(args.clone())
    }
}

fn status_bool(result: &Value, key: &str) -> Result<bool, String> {
    result[key]
        .as_bool()
        .ok_or("Native Android tool receipt was incomplete".into())
}

fn short_text(result: &Value, key: &str, max: usize) -> String {
    result[key]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(max)
        .collect()
}

fn short_bytes(result: &Value, key: &str, max_bytes: usize) -> String {
    let mut text = String::new();
    for ch in result[key].as_str().unwrap_or("").chars() {
        if text.len().saturating_add(ch.len_utf8()) > max_bytes {
            break;
        }
        text.push(ch);
    }
    text
}

fn valid_app_mode(mode: &str) -> bool {
    matches!(mode, "realtime" | "screenshot_click")
}

fn redacted_app_text(value: &Value, key: &str) -> String {
    let text = short_bytes(value, key, 64);
    let lowered = text.to_ascii_lowercase();
    if text.contains('@') ||
        ["password", "passcode", "passphrase", "otp", "one-time", "secret",
         "token", "authorization", "cookie", "credit card", "cvv", "cvc",
         "api key", "verification code", "security code"]
            .iter().any(|term|lowered.contains(term)) ||
        text.split(|c:char|!c.is_ascii_digit()).any(|digits|digits.len()>=6) ||
        text.chars().any(char::is_control) {
        "[redacted]".to_owned()
    } else {
        text
    }
}

/// Return only a grounded, bounded subset of the native receipt to the model.
/// Never pass raw plugin responses, full page DOM or private status metadata.
/// A successful tool result is not evidence of a final AI answer.
pub fn checked_result(name: &str, receipt: Value) -> Result<Value, String> {
    if !(NAMES.contains(&name) || matches!(name,
        "security_audit_internal_status" | "reverse_engineering_internal_status"))
        || !receipt.is_object() {
        return Err("Native Android tool receipt was not valid".into());
    }
    let result = match name {
        "security_audit_internal_status" | "reverse_engineering_internal_status" => {
            if !status_bool(&receipt,"supported")? || !status_bool(&receipt,"ready")? {
                return Err("Bundled curated module is unavailable".into());
            }
            json!({"supported":true,"ready":true})
        }
        "security_audit_analyze" => {
            let scanned=receipt["scannedBytes"].as_u64()
                .filter(|n|*n>=1 && *n<=MAX_AUDIT_BYTES as u64)
                .ok_or("Audit receipt byte count invalid")?;
            let counts=&receipt["matchedLines"];
            let mut projected=serde_json::Map::new();
            for name in ["possibleCredentials","cleartextTransport","dynamicExecution","weakCryptoOrExposure"] {
                let n=counts[name].as_u64().filter(|n|*n<=MAX_AUDIT_BYTES as u64)
                    .ok_or("Audit receipt finding count invalid")?;
                projected.insert(name.to_owned(),json!(n));
            }
            let risk=receipt["riskLevel"].as_str()
                .filter(|s|matches!(*s,"high"|"review"|"none_detected"))
                .ok_or("Audit receipt risk summary invalid")?;
            if receipt["heuristicOnly"]!=true {return Err("Audit provenance missing".into());}
            json!({"scannedBytes":scanned,"matchedLines":projected,
                "riskLevel":risk,"heuristicOnly":true})
        }
        "reverse_engineering_dex_header" => {
            if receipt["validHeader"]!=true || receipt["headerOnly"]!=true
                || receipt["checksumVerified"]!=false || receipt["endian"]!="little"
                || receipt["headerSizeBytes"]!=DEX_HEADER_BYTES as u64 {
                return Err("DEX inspection did not establish valid header-only result".into());
            }
            let version=receipt["dexVersion"].as_str()
                .filter(|s|matches!(*s,"035"|"036"|"037"|"038"|"039"|"040"))
                .ok_or("DEX inspection version invalid")?;
            let size=receipt["fileSizeBytes"].as_u64().filter(|n|*n>=DEX_HEADER_BYTES as u64 && *n<=u32::MAX as u64)
                .ok_or("DEX inspection size invalid")?;
            let map=receipt["mapOffset"].as_u64().filter(|n|*n>=DEX_HEADER_BYTES as u64 && *n<size)
                .ok_or("DEX inspection map offset invalid")?;
            let mut counts=serde_json::Map::new();
            for key in ["strings","types","prototypes","fields","methods","classes"] {
                let n=receipt["sectionCounts"][key].as_u64().filter(|n|*n<=u32::MAX as u64)
                    .ok_or("DEX inspection section count invalid")?;
                counts.insert(key.to_owned(),json!(n));
            }
            json!({"validHeader":true,"dexVersion":version,"fileSizeBytes":size,
                "headerSizeBytes":DEX_HEADER_BYTES,"endian":"little","mapOffset":map,
                "sectionCounts":counts,"checksumVerified":false,"headerOnly":true})
        }
        "android_browser_status" | "android_browser_start" | "android_browser_stop" => {
            let running = status_bool(&receipt, "running")?;
            let supported = status_bool(&receipt, "supported")?;
            let ready = status_bool(&receipt, "ready")?;
            let isolated = status_bool(&receipt, "profileIsolated")?;
            if running && (!isolated || !supported)
                || name == "android_browser_start" && (!running || !isolated)
                || name == "android_browser_stop" && running
            {
                return Err("Native browser did not confirm requested state".into());
            }
            json!({"supported":supported,"running":running,"ready":ready,
                "profileIsolated":isolated,"title":short_text(&receipt,"title",100)})
        }
        "android_browser_snapshot" => {
            let id = receipt["snapshotId"]
                .as_i64()
                .ok_or("Browser snapshot ID missing")?;
            if id < 1 {
                return Err("Browser snapshot ID is invalid".into());
            }
            let elements = receipt["elements"]
                .as_array()
                .ok_or("Browser snapshot controls missing")?;
            if elements.len() > 48 {
                return Err("Browser snapshot exceeds control limit".into());
            }
            let mut projected = Vec::new();
            // Include every index that the native plugin can accept. Omitting
            // controls while leaving their indices callable would invite the
            // model to guess invisible elements. Bytes are capped per label.
            for (i, el) in elements.iter().enumerate() {
                if el["index"].as_u64() != Some(i as u64 + 1)
                    || !el["fillable"].is_boolean()
                    || !el["label"].is_string()
                    || !el["tag"].is_string()
                {
                    return Err("Browser control identity invalid".into());
                }
                projected.push(json!({"index":i+1,"fillable":el["fillable"],
                    "label":short_bytes(el,"label",48),"tag":short_bytes(el,"tag",16)}));
            }
            json!({"snapshotId":id,"text":short_bytes(&receipt,"text",1000),
                "title":short_bytes(&receipt,"title",80),
                "totalControls":elements.len(),"controlsTruncated":false,
                "elements":projected})
        }
        "android_browser_click" | "android_browser_fill" => {
            if !status_bool(&receipt, "ok")? || !status_bool(&receipt, "snapshotConsumed")? {
                return Err("Native browser did not acknowledge the indexed action".into());
            }
            json!({"ok":true,"snapshotConsumed":true})
        }
        "android_app_status" | "android_app_start" | "android_app_stop" => {
            let connected = status_bool(&receipt, "connected")?;
            let active = status_bool(&receipt, "active")?;
            let visible = status_bool(&receipt, "visible")?;
            let stopped = status_bool(&receipt, "stopRequested")?;
            if name == "android_app_start"
                && (!connected || !active || !visible || !status_bool(&receipt, "launched")?)
                || name == "android_app_stop" && active
            {
                return Err("Android did not confirm requested app automation state".into());
            }
            json!({"connected":connected,"active":active,"visible":visible,
                "stopRequested":stopped,"targetPackage":short_text(&receipt,"targetPackage",160),
                "automationActionsAvailable":receipt["automationActionsAvailable"]==true,
                "selectedAppMode":match receipt["selectedAppMode"].as_str() {
                    Some("screenshot_click")=>"screenshot_click", _=>"realtime",
                },
                "launched":name=="android_app_start"})
        }
        "android_app_snapshot" => {
            if !status_bool(&receipt,"ok")? { return Err("Android did not confirm screen snapshot".into()); }
            let selected=receipt["selectedMode"].as_str().filter(|m|valid_app_mode(m))
                .ok_or("App snapshot selection missing")?;
            let effective=receipt["effectiveMode"].as_str().filter(|m|valid_app_mode(m))
                .ok_or("App snapshot effective mode missing")?;
            let fallback=status_bool(&receipt,"fallbackUsed")?;
            if selected=="screenshot_click" && effective!="screenshot_click" ||
                selected=="realtime" && fallback != (effective=="screenshot_click") {
                return Err("Android snapshot changed selected mode or fabricated fallback".into());
            }
            let token=receipt["snapshotId"].as_str()
                .filter(|s|uuid::Uuid::parse_str(s).is_ok())
                .ok_or("Android screenshot identity missing")?;
            let width=receipt["screenWidth"].as_u64().filter(|n|*n>=1&&*n<=8192)
                .ok_or("Android screen width invalid")?;
            let height=receipt["screenHeight"].as_u64().filter(|n|*n>=1&&*n<=8192)
                .ok_or("Android screen height invalid")?;
            // A screenshot-only receipt deliberately carries no Accessibility
            // nodes. Requiring `elements` here rejects every valid fallback
            // image after the native Android bridge has already validated it.
            let elements=if effective=="realtime" {
                let nodes=receipt["elements"].as_array()
                    .ok_or("Android controls list missing")?;
                if nodes.len()>48 {
                    return Err("Android controls exceed bounded snapshot".into());
                }
                Some(nodes)
            } else {
                if receipt.get("elements").is_some() {
                    return Err("Screenshot mode must not leak Accessibility controls".into());
                }
                None
            };
            let mut projected=Vec::with_capacity(elements.map_or(0, Vec::len));
            for (n,entry) in elements.into_iter().flatten().enumerate(){
                if entry["index"].as_u64()!=Some(n as u64 +1) ||
                    !entry["clickable"].is_boolean() ||
                    !entry["editable"].is_boolean() ||
                    !entry["protected"].is_boolean() ||
                    entry["protected"]==true {
                    return Err("Android control identity or protection invalid".into());
                }
                let mut bounds=Vec::new();
                for key in ["left","top","right","bottom"] {
                    bounds.push(entry[key].as_u64().filter(|v|*v<=8192)
                        .ok_or("Android control bounds invalid")?);
                }
                if bounds[0]>bounds[2] ||bounds[1]>bounds[3] {
                    return Err("Android control bounds inverted".into());
                }
                projected.push(json!({"index":n+1,
                    "type":short_bytes(entry,"className",32),
                    "label":redacted_app_text(entry,"text"),
                    "description":redacted_app_text(entry,"description"),
                    "clickable":entry["clickable"],"editable":entry["editable"],
                    "bounds":bounds}));
            }
            let total_controls=projected.len();
            let mut output=json!({"snapshotId":token,"selectedMode":selected,
                "effectiveMode":effective,"fallbackUsed":fallback,
                "screenWidth":width,"screenHeight":height,
                "elements":projected,"totalControls":total_controls,
                "imageAvailable":false});
            match receipt["mimeType"].as_str() {
                Some("none") if effective=="realtime" && receipt.get("imageBase64").is_none()=>{},
                Some("image/jpeg") if effective=="screenshot_click" =>{
                    let bytes=receipt["imageBytes"].as_u64()
                        .filter(|n|*n>=128&&*n<=128*1024)
                        .ok_or("Android screenshot exceeds safe image size")?;
                    let image=receipt["imageBase64"].as_str()
                        .filter(|s|s.len()<=175000 && s.len()%4==0 && s.starts_with("/9j/") &&
                            s.bytes().all(|b|b.is_ascii_alphanumeric() ||
                                matches!(b,b'+'|b'/'|b'=')))
                        .ok_or("Android screenshot JPEG encoding invalid")?;
                    let padding=image.bytes().rev().take_while(|b|*b==b'=').count();
                    if padding>2 || image.len()/4*3-padding!=bytes as usize {
                        return Err("Android screenshot byte length inconsistent".into());
                    }
                    let image_width=receipt["imageWidth"].as_u64()
                        .filter(|n|*n>=1&&*n<=width)
                        .ok_or("Android preview width invalid")?;
                    let image_height=receipt["imageHeight"].as_u64()
                        .filter(|n|*n>=1&&*n<=height)
                        .ok_or("Android preview height invalid")?;
                    output["imageAvailable"]=json!(true);
                    output["imageMimeType"]=json!("image/jpeg");
                    output["imageBase64"]=json!(image);
                    output["imageBytes"]=json!(bytes);
                    output["imageWidth"]=json!(image_width);
                    output["imageHeight"]=json!(image_height);
                },
                _=>return Err("Unsupported or inconsistent Android screen mode".into())
            }
            output
        }
        "android_app_click" | "android_app_fill" | "android_app_gesture" => {
            if !status_bool(&receipt,"ok")? ||
                !status_bool(&receipt,"dispatched")? {
                return Err("Android did not acknowledge approved app action".into());
            }
            let completed=status_bool(&receipt,"completed")?;
            let mode=receipt["effectiveMode"].as_str()
                .filter(|m|valid_app_mode(m))
                .ok_or("App action mode missing")?;
            let fallback=status_bool(&receipt,"fallbackUsed")?;
            if fallback && mode!="screenshot_click" {
                return Err("Invalid Android app fallback receipt".into());
            }
            json!({"ok":true,"dispatched":true,"completed":completed,
                "effectiveMode":mode,"fallbackUsed":fallback})
        }
        _ => unreachable!(),
    };
    let max = if name=="android_app_snapshot" && result["imageAvailable"]==true {
        195*1024
    }else{8*1024};
    if result.to_string().len() > max {
        return Err("Android tool result exceeds model continuation limit".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_dex_header() -> String {
        let mut bytes=[0u8;DEX_HEADER_BYTES];
        bytes[..8].copy_from_slice(b"dex\n039\0");
        bytes[32..36].copy_from_slice(&512u32.to_le_bytes());
        bytes[36..40].copy_from_slice(&(DEX_HEADER_BYTES as u32).to_le_bytes());
        bytes[40..44].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        bytes[52..56].copy_from_slice(&116u32.to_le_bytes());
        bytes[104..108].copy_from_slice(&400u32.to_le_bytes());
        bytes[108..112].copy_from_slice(&112u32.to_le_bytes());
        bytes.iter().map(|b|format!("{b:02x}")).collect()
    }

    #[test]
    fn curated_security_audit_returns_only_aggregate_findings() {
        let secret="sk-SECRET_DO_NOT_ECHO_1234567890";
        let input=format!("api_key = {secret}\nfetch('http://example.org')\neval(payload)\nmd5(data)");
        let args=json!({"text":input});
        let raw=curated_host_execute("security_audit_analyze",&args).unwrap().unwrap();
        let receipt=checked_result("security_audit_analyze",raw.clone()).unwrap();
        assert_eq!(receipt["matchedLines"]["possibleCredentials"],1);
        assert_eq!(receipt["matchedLines"]["cleartextTransport"],1);
        assert_eq!(receipt["matchedLines"]["dynamicExecution"],1);
        assert_eq!(receipt["matchedLines"]["weakCryptoOrExposure"],1);
        assert_eq!(receipt["riskLevel"],"high");
        assert!(!receipt.to_string().contains(secret));
        assert!(!raw.to_string().contains(secret));
        let mut injected=raw;
        injected["candidateSecret"]=json!(secret);
        assert!(!checked_result("security_audit_analyze",injected).unwrap().to_string().contains(secret));
        assert!(curated_host_execute("security_audit_analyze",&json!({"text":"x".repeat(4097)}))
            .unwrap().is_err());
        assert!(curated_host_execute("security_audit_analyze",&json!({"text":"a\0b"}))
            .unwrap().is_err());
        assert!(curated_host_execute("security_audit_analyze",&json!({"text":"safe","leak":true}))
            .unwrap().is_err());
    }

    #[test]
    fn curated_dex_inspection_checks_magic_version_endian_and_bounds() {
        let hex=valid_dex_header();
        let result=curated_host_execute("reverse_engineering_dex_header",&json!({"hexHeader":hex}))
            .unwrap().unwrap();
        let output=checked_result("reverse_engineering_dex_header",result.clone()).unwrap();
        assert_eq!(output["validHeader"],true);
        assert_eq!(output["dexVersion"],"039");
        assert_eq!(output["fileSizeBytes"],512);
        assert_eq!(output["checksumVerified"],false);
        assert_eq!(output["headerOnly"],true);
        assert!(!output.to_string().contains(&hex));
        for (offset,value) in [(0,"00"),(4,"34"),(40,"00"),(52,"ff"),(108,"ff")] {
            let mut bad=hex.clone();
            bad.replace_range(offset*2..offset*2+2,value);
            assert!(curated_host_execute("reverse_engineering_dex_header",&json!({"hexHeader":bad}))
                .unwrap().is_err(),"byte {offset} must invalidate header");
        }
        let mut bad=hex.clone();
        bad.replace_range(0..2,"xz");
        assert!(curated_host_execute("reverse_engineering_dex_header",&json!({"hexHeader":bad}))
            .unwrap().is_err());
        assert!(curated_host_execute("reverse_engineering_dex_header",&json!({"hexHeader":"00".repeat(111)}))
            .unwrap().is_err());
        let mut injected=result;
        injected["rawHeader"]=json!(hex);
        assert!(checked_result("reverse_engineering_dex_header",injected).unwrap()
            .get("rawHeader").is_none());
    }

    #[test]
    fn model_native_dispatch_folds_only_https_scheme() {
        let source = json!({"url":"HTTPS://EXAMPLE.ORG/SomePath?ID=AaB#Part"});
        let canonical = dispatch_arguments("android_browser_start", &source).unwrap();
        assert_eq!(canonical["url"], "https://EXAMPLE.ORG/SomePath?ID=AaB#Part");
        assert_eq!(source["url"], "HTTPS://EXAMPLE.ORG/SomePath?ID=AaB#Part");
        assert!(dispatch_arguments(
            "android_browser_start",
            &json!({"url":"HTTPS://127.0.0.1/"})
        )
        .is_err());
        assert!(dispatch_arguments(
            "android_browser_start",
            &json!({"url":"https://other.org/"})
        )
        .is_ok()); // Authorization of this URL is checked against the user intent separately.
    }

    #[test]
    fn local_registry_is_finite_and_strict() {
        let tools = definitions();
        assert_eq!(tools.len(), 15);
        let mut names = std::collections::HashSet::new();
        for entry in &tools {
            let name = entry["name"].as_str().unwrap();
            assert!(names.insert(name));
            let mut args = serde_json::Map::new();
            if let Some(required) = entry["inputSchema"]["required"].as_array() {
                for req in required {
                    let key = req.as_str().unwrap();
                    args.insert(
                        key.to_string(),
                        match key {
                            "index" => json!(1),
                            "snapshotId" => if name.starts_with("android_browser_") {
                                json!(1)
                            } else {
                                json!("00000000-0000-4000-8000-000000000000")
                            },
                            "kind" => json!("tap"),
                            "x" | "y" | "toX" | "toY" => json!(0),
                            "durationMs" => json!(100),
                            "url" => json!("https://example.org"),
                            "targetPackage" => json!("com.example.app"),
                            "value" => json!("hello"),
                            "text" => json!("GET http://example.org"),
                            "hexHeader" => json!("00".repeat(DEX_HEADER_BYTES)),
                            _ => unreachable!(),
                        },
                    );
                }
            }
            if name == "android_app_click" {
                args.insert("index".into(), json!(1));
            }
            assert!(
                validate(name, &Value::Object(args.clone())).is_ok(),
                "{name}"
            );
            args.insert("undeclared".into(), json!(true));
            assert!(validate(name, &Value::Object(args)).is_err(), "{name}");
        }
        assert!(validate("android_browser_open_shell", &json!({})).is_err());
        assert!(validate(
            "android_browser_start",
            &json!({"url":"file:///etc/passwd"})
        )
        .is_err());
        assert!(validate(
            "android_browser_start",
            &json!({"url":"https://user:secret@example.org"})
        )
        .is_err());
        assert!(validate(
            "android_app_start",
            &json!({"targetPackage":"com.android.settings"})
        )
        .is_err());
    }

    #[test]
    fn browser_receipts_are_measured_and_bounded() {
        assert!(checked_result(
            "android_browser_click",
            json!({"ok":true,"snapshotConsumed":false})
        )
        .is_err());
        assert!(checked_result(
            "android_browser_start",
            json!({"supported":true,"running":false,"ready":false,"profileIsolated":true})
        )
        .is_err());
        let mut elements = Vec::new();
        for i in 1..=48 {
            elements
                .push(json!({"index":i,"fillable":false,"label":"a".repeat(200),"tag":"button"}));
        }
        let result = checked_result(
            "android_browser_snapshot",
            json!({
                "snapshotId":21,"title":"Example","text":"x".repeat(4000),"elements":elements
            }),
        )
        .unwrap();
        assert_eq!(result["elements"].as_array().unwrap().len(), 48);
        assert_eq!(result["totalControls"], 48);
        assert_eq!(result["controlsTruncated"], false);
        assert!(result.to_string().len() < 8 * 1024);
    }

    #[test]
    fn app_launch_requires_the_blue_indicator_to_be_visible() {
        let armed_not_visible = json!({"connected":true,"active":true,
            "visible":false,"stopRequested":false,"launched":true,
            "targetPackage":"com.google.android.youtube"});
        assert!(checked_result("android_app_start", armed_not_visible).is_err());
        let visible = json!({"connected":true,"active":true,
            "visible":true,"stopRequested":false,"launched":true,
            "targetPackage":"com.google.android.youtube"});
        let result = checked_result("android_app_start", visible).unwrap();
        assert_eq!(result["visible"], true);
        assert_eq!(result["launched"], true);
        assert!(result.get("sessionId").is_none(),
            "Public MCP status must not expose the opaque OS-bound app session ID");
    }

    #[test]
    fn app_snapshot_is_bounded_redacted_and_fallback_is_not_persisted() {
        let token="00000000-0000-4000-8000-000000000000";
        let safe=json!({"ok":true,"selectedMode":"realtime","effectiveMode":"realtime",
            "fallbackUsed":false,"snapshotId":token,"screenWidth":720,"screenHeight":1600,
            "mimeType":"none","elements":[
                {"index":1,"className":"Button","text":"Search",
                 "description":"Open search","clickable":true,"editable":false,
                 "protected":false,"left":10,"top":100,"right":200,"bottom":160},
                {"index":2,"className":"TextView","text":"123456",
                 "description":"security code","clickable":false,"editable":false,
                 "protected":false,"left":15,"top":200,"right":250,"bottom":260}
            ]});
        let projected=checked_result("android_app_snapshot",safe.clone()).unwrap();
        assert_eq!(projected["totalControls"],2);
        assert_eq!(projected["imageAvailable"],false);
        assert_eq!(projected["elements"][0]["label"],"Search");
        assert_eq!(projected["elements"][1]["label"],"[redacted]");
        assert_eq!(projected["elements"][1]["description"],"[redacted]");
        assert_eq!(projected["selectedMode"],"realtime");
        let mut fabricated=safe.clone();
        fabricated["fallbackUsed"]=json!(true);
        assert!(checked_result("android_app_snapshot",fabricated).is_err());
        let mut protected=safe.clone();
        protected["elements"][0]["protected"]=json!(true);
        assert!(checked_result("android_app_snapshot",protected).is_err());
        let mut wrong_index=safe;
        wrong_index["elements"][0]["index"]=json!(12);
        assert!(checked_result("android_app_snapshot",wrong_index).is_err());
        let dispatched=checked_result("android_app_click",json!({
            "ok":true,"dispatched":true,"completed":false,
            "effectiveMode":"screenshot_click","fallbackUsed":true
        })).unwrap();
        assert_eq!(dispatched["completed"],false,
            "Android dispatched is not proof the app applied the gesture");
        assert_eq!(dispatched["fallbackUsed"],true);
    }

    #[test]
    fn app_screenshot_fallback_accepts_image_without_semantic_elements() {
        let snapshot=json!({"ok":true,"selectedMode":"realtime",
            "effectiveMode":"screenshot_click","fallbackUsed":true,
            "snapshotId":"00000000-0000-4000-8000-000000000000",
            "screenWidth":720,"screenHeight":1600,"mimeType":"image/jpeg",
            "imageBytes":129,"imageWidth":360,"imageHeight":800,
            "imageBase64":format!("/9j/{}", "A".repeat(168))});
        let projected=checked_result("android_app_snapshot",snapshot.clone()).unwrap();
        assert_eq!(projected["imageAvailable"],true);
        assert_eq!(projected["imageMimeType"],"image/jpeg");
        assert_eq!(projected["imageBytes"],129);
        assert_eq!(projected["totalControls"],0);
        assert_eq!(projected["elements"],json!([]));
        assert_eq!(projected["fallbackUsed"],true);

        let mut leaked=snapshot.clone();
        leaked["elements"]=json!([]);
        assert!(checked_result("android_app_snapshot",leaked).is_err());
        let mut mismatched=snapshot;
        mismatched["fallbackUsed"]=json!(false);
        assert!(checked_result("android_app_snapshot",mismatched).is_err());

        // A real-time Accessibility snapshot can legitimately contain no
        // exposed controls. The field must exist, but it may be empty.
        let realtime=json!({"ok":true,"selectedMode":"realtime",
            "effectiveMode":"realtime","fallbackUsed":false,
            "snapshotId":"00000000-0000-4000-8000-000000000000",
            "screenWidth":720,"screenHeight":1600,
            "mimeType":"none","elements":[]});
        let blank=checked_result("android_app_snapshot",realtime).unwrap();
        assert_eq!(blank["totalControls"],0);
        assert_eq!(blank["imageAvailable"],false);
    }

    #[test]
    fn app_actions_reject_mixed_or_unscoped_coordinates() {
        let id="00000000-0000-4000-8000-000000000000";
        assert!(validate("android_app_click",&json!({"snapshotId":id})).is_err());
        assert!(validate("android_app_click",&json!({"snapshotId":id,"x":20})).is_err());
        assert!(validate("android_app_click",&json!({"snapshotId":id,
            "index":1,"x":20,"y":20})).is_err());
        assert!(validate("android_app_click",&json!({"snapshotId":id,
            "x":20,"y":20})).is_ok());
        assert!(validate("android_app_fill",&json!({"snapshotId":id,"index":1,
            "value":"secret\ninput"})).is_err());
        assert!(validate("android_app_gesture",&json!({"snapshotId":id,
            "kind":"tap","x":10,"y":15,"toX":10,"toY":60,"durationMs":100})).is_err());
    }

    #[test]
    fn real_provider_proposal_and_no_more_tools_continuation_share_local_schema() {
        let names = definitions()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let provider_turn = json!({"choices":[{"index":0,"finish_reason":"tool_calls",
            "message":{"role":"assistant","content":null,"tool_calls":[{
                "id":"call_local_001","type":"function","function":{
                    "name":"android_browser_snapshot","arguments":"{}"}}]}}]});
        let proposal = match super::super::provider_tools::parse_provider_turn(
            "openai",
            &provider_turn,
            &names,
        )
        .unwrap()
        {
            super::super::provider_tools::ProviderTurn::ToolProposal(call) => call,
            _ => panic!("An actual model call must remain an unexecuted proposal"),
        };
        validate(&proposal.name, &proposal.arguments).unwrap();
        let result = checked_result(
            "android_browser_snapshot",
            json!({
                "snapshotId":23,"text":"Example page","title":"Example","elements":[]
            }),
        )
        .unwrap();
        let tools = definitions()
            .iter()
            .map(|entry| {
                json!({"type":"function","function":{
                "name":entry["name"],"description":entry["description"],
                "parameters":entry["inputSchema"]}})
            })
            .collect::<Vec<_>>();
        let request = super::super::provider_tools::continuation_payload(
            "openai",
            "gpt-6",
            &[json!({"role":"user","content":"Read my page"})],
            &tools,
            &proposal,
            &result,
        )
        .unwrap();
        assert_eq!(request["tool_choice"], "none");
        assert_eq!(request["tools"].as_array().unwrap().len(), 1);
        assert_eq!(request["messages"][2]["tool_call_id"], "call_local_001");
        assert!(request["messages"][2]["content"]
            .as_str()
            .unwrap()
            .contains("snapshotId"));
        let invalid = json!({"choices":[{"index":0,"finish_reason":"tool_calls",
            "message":{"role":"assistant","content":null,"tool_calls":[{
                "id":"call_local_002","type":"function","function":{
                    "name":"android_shell_run","arguments":"{}"}}]}}]});
        assert!(
            super::super::provider_tools::parse_provider_turn("openai", &invalid, &names).is_err()
        );
    }
}
