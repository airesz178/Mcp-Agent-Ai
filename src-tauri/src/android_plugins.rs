//! Android Plugins are opt-in tools on the existing Core MCP tunnel.
//! The native registry has its own durable module toggles and aliases, but
//! shares Core's authenticated tunnel session and RPC dispatcher. No separate
//! credential, tunnel ID, polling worker or Android approval broker is used.

use crate::android_agent_tools::Host;
#[cfg(test)]
use crate::android_plugins_approvals::ApprovalIntent;
use crate::android_openai_tunnel::{AndroidOpenAiTunnel, PluginRpcHandler, RpcReply};
use mcp_agent_core::android_plugin_registry::{
    execute_curated_host_tool, AndroidLocalToolHost, AndroidPluginRegistry, PluginSource, PluginToolInfo,
};
use serde_json::{json, Value};
use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::AppHandle;

const CATALOG_FILE: &str = "mcp-android-plugins-v1.json";
const MAX_RPC_ARGS: usize = 8 * 1024;
const MAX_OUTPUT: usize = 16 * 1024;
// Session authorization is RAM-only, never restored on app/tunnel restart.
// An unattended session cannot silently retain screen control forever.
const APP_SESSION_MAX_AGE: Duration = Duration::from_secs(60 * 60);
const CONNECTOR_NAME: &str = "Mcp Agent Ai Core";
const CONNECTOR_DESCRIPTION: &str =
    "Optional verified Android plugin tools shared through the Mcp Agent Ai Core tunnel.";

/// Keeps Android browser/app behavior in the existing host and executes only
/// two fixed, side-effect-free bundled tools in native-core. No APK parsing,
/// arbitrary plugins, network call or filesystem read is involved.
struct MarketplaceHost(Arc<dyn AndroidLocalToolHost>);

impl AndroidLocalToolHost for MarketplaceHost {
    fn execute<'a>(&'a self, name:&'a str, arguments:Value)
        ->Pin<Box<dyn Future<Output=Result<Value,String>> + Send + 'a>> {
        if let Some(result)=execute_curated_host_tool(name,&arguments) {
            Box::pin(async move {result})
        } else {
            self.0.execute(name,arguments)
        }
    }

    fn resolve_app_label<'a>(&'a self, query:&'a str)
        ->Pin<Box<dyn Future<Output=Result<Option<(String,String)>,String>> + Send + 'a>> {
        self.0.resolve_app_label(query)
    }
}

#[derive(Clone)]
pub(crate) struct AndroidPluginsRuntime {
    tunnel: AndroidOpenAiTunnel,
    registry: Arc<Mutex<AndroidPluginRegistry>>,
    host: Arc<dyn AndroidLocalToolHost>,
    config_path: Arc<PathBuf>,
    app_session_grant: Arc<Mutex<Option<AppSessionGrant>>>,
    plugin_evidence: Arc<Mutex<PluginCallEvidence>>,
}

/// One *live* Core generation. A saved tunnel ID alone is never authority
/// to use a native Android device-control tool.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CoreTunnelIdentity {
    tunnel_id: String,
    generation: u64,
}

fn live_core_identity(tunnel: &AndroidOpenAiTunnel) -> Option<CoreTunnelIdentity> {
    let (tunnel_id,generation)=tunnel.current_live_identity().ok()??;
    // A saved ID may change while an older poll worker is winding down.
    // Only the active authenticated Core identity may authorize phone actions.
    (tunnel.confirmed_id().as_deref()==Some(tunnel_id.as_str()))
        .then_some(CoreTunnelIdentity {tunnel_id,generation})
}

/// Evidence of actual plugin tool executions, scoped to one live Core
/// connection generation. Core app_info/echo calls cannot increase it.
#[derive(Default)]
struct PluginCallEvidence {
    identity: Option<CoreTunnelIdentity>,
    successful_calls: u64,
}

fn plugin_call_count(
    evidence: &Arc<Mutex<PluginCallEvidence>>,
    live_identity: Option<CoreTunnelIdentity>,
) -> Result<u64, String> {
    let mut proof=evidence.lock().map_err(|_| "Plugins call evidence unavailable")?;
    if proof.identity!=live_identity {
        proof.identity=live_identity;
        proof.successful_calls=0;
    }
    Ok(proof.successful_calls)
}

fn record_successful_plugin_call(
    evidence: &Arc<Mutex<PluginCallEvidence>>,
    before: Option<CoreTunnelIdentity>,
    after: Option<CoreTunnelIdentity>,
) {
    // Neither a disconnected status call nor a request crossing reconnect
    // proves activity for the current Core generation.
    let Some(identity)=before.filter(|identity|Some(identity)==after.as_ref()) else {return};
    if let Ok(mut proof)=evidence.lock() {
        if proof.identity.as_ref()!=Some(&identity) {
            proof.identity=Some(identity);
            proof.successful_calls=0;
        }
        proof.successful_calls=proof.successful_calls.saturating_add(1);
    }
}

fn project_plugin_call_evidence(
    result: &mut Value,
    evidence: &Arc<Mutex<PluginCallEvidence>>,
    live_identity: Option<CoreTunnelIdentity>,
) -> Result<(), String> {
    let count=plugin_call_count(evidence,live_identity)?;
    result["toolCallsVerified"]=json!(count);
    if result["pollActive"]==true {
        result["state"]=json!(if count>0 {"tool_verified"} else {"polling"});
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct AppSessionGrant {
    tunnel_id: String,
    tunnel_generation: u64,
    session_id: String,
    target_package: String,
    expires_at: Instant,
}

impl AppSessionGrant {
    fn matches(&self, tunnel: &CoreTunnelIdentity, status: &Value) -> bool {
        self.tunnel_id == tunnel.tunnel_id
            && self.tunnel_generation == tunnel.generation
            && Instant::now() < self.expires_at
            && approved_app_handoff_context(status).is_ok_and(|(session,target)| {
                session == self.session_id && target == self.target_package
            })
    }
}

/// A trusted shared Core tunnel's Android app start establishes only the
/// OS-bound session it actually launched, not a package-wide permission.
fn record_app_session(
    grant: &Arc<Mutex<Option<AppSessionGrant>>>, tunnel: &CoreTunnelIdentity,
    requested_package: &str, status: &Value,
) -> Result<(),String> {
    let (session,target)=approved_app_handoff_context(status)?;
    if target != requested_package { return Err("Approved app target changed".into()); }
    let expires_at=Instant::now().checked_add(APP_SESSION_MAX_AGE)
        .ok_or("App session time limit unavailable")?;
    *grant.lock().map_err(|_|"App session authorization unavailable")? =
        Some(AppSessionGrant {tunnel_id:tunnel.tunnel_id.clone(),
            tunnel_generation:tunnel.generation,session_id:session,
            target_package:target,expires_at});
    Ok(())
}

fn authorize_app_session_action(
    grant: &Arc<Mutex<Option<AppSessionGrant>>>, tunnel: &CoreTunnelIdentity,
    current_status: &Value,
) -> Result<(),String> {
    let mut guard=grant.lock().map_err(|_|"App session authorization unavailable")?;
    if guard.as_ref().is_some_and(|saved|saved.matches(tunnel,current_status)){
        return Ok(());
    }
    // A revoked, expired, switched or locked session never falls back to a
    // surprise per-click popup or a saved Always allow rule.
    *guard=None;
    Err("App control requires a new live session from the connected Core tunnel".into())
}

fn clear_app_session(grant:&Arc<Mutex<Option<AppSessionGrant>>>) {
    if let Ok(mut guard)=grant.lock(){*guard=None;}
}

/// Retained only as a backwards-compatibility test of the legacy permission
/// descriptions. The Android runtime no longer invokes a local approval broker.
#[cfg(test)]
fn approval_intent(
    info: &PluginToolInfo,
    args: &Value,
    host: &Arc<dyn AndroidLocalToolHost>,
) -> Result<ApprovalIntent, String> {
    if matches!(info.source,PluginSource::RemoteMcp { .. }) {
        // A remote MCP tool may deliberately reuse a native tool name. Its
        // requested effects are unknown, so it can NEVER borrow session
        // authorization or a native phone-control permission description.
        return Ok(ApprovalIntent {
            plugin_id:info.plugin_id.clone(),plugin_name:info.plugin_name.clone(),
            tool_name:info.original_name.clone(),
            summary:"Run the requested verified remote MCP tool".into(),
            scope:format!("Plugin {}",info.plugin_id),
            can_always_allow:false,
        });
    }
    let tool = info.original_name.as_str();
    let (summary, scope, can_always_allow) = match tool {
        "android_browser_start" => {
            let url = args["url"].as_str().ok_or("Browser URL missing")?;
            let parsed = reqwest::Url::parse(url).map_err(|_| "Browser URL invalid")?;
            if parsed.scheme() != "https" || parsed.host_str().is_none()
                || !parsed.username().is_empty() || parsed.password().is_some()
            { return Err("Browser approval requires HTTPS origin".into()) }
            let origin = parsed.origin().ascii_serialization();
            (format!("Open a page in the isolated browser at {origin}"), origin, true)
        }
        "android_browser_stop" | "android_browser_snapshot" | "android_browser_click" | "android_browser_fill" => {
            let native = tauri::async_runtime::block_on(host.execute("android_browser_status",json!({})))?;
            let origin = native["url"].as_str()
                .and_then(|url| reqwest::Url::parse(url).ok())
                .filter(|url| url.scheme() == "https" && url.host_str().is_some()
                    && url.username().is_empty() && url.password().is_none())
                .map(|url| url.origin().ascii_serialization());
            let (scope, can_always_allow) = match origin {
                Some(origin) => (origin, true),
                None => ("Current browser session".to_owned(), false),
            };
            let summary = match tool {
                "android_browser_stop" => "Stop the isolated Android browser".to_owned(),
                "android_browser_snapshot" => "Read current browser page text and visible controls".to_owned(),
                "android_browser_click" => format!("Click control {} in the current browser snapshot",args["index"]),
                "android_browser_fill" => format!("Fill control {} with {} character(s)", args["index"],
                    args["value"].as_str().map(|s|s.chars().count()).unwrap_or(0)),
                _ => unreachable!(),
            };
            (summary, scope, can_always_allow)
        }
        "android_app_start" => {
            let target = args["targetPackage"].as_str().ok_or("Target app required")?;
            ("Allow ChatGPT screen reading, tapping, swiping and non-password input in the selected Android app for this session. Stop, lock or switch apps to end access.".into(),
                format!("Package {target} · this session only"),false)
        }
        "android_app_stop" => {
            let native = tauri::async_runtime::block_on(host.execute("android_app_status",json!({})))?;
            let target = native["targetPackage"].as_str().filter(|v| !v.is_empty() && v.len()<=160);
            let (scope, can_always_allow) = target.map(|value| (format!("Package {value}"),true))
                .unwrap_or(("Current app session".to_owned(),false));
            ("Stop the Android app automation session".to_owned(),scope,can_always_allow)
        }
        "android_app_snapshot" | "android_app_click" | "android_app_fill"
        | "android_app_gesture" => {
            let native = tauri::async_runtime::block_on(
                host.execute("android_app_status",json!({})))?;
            if native["active"] != true || native["visible"] != true ||
                native["connected"] != true || native["automationActionsAvailable"] != true {
                return Err("Android app screen actions need a live approved OS session".into());
            }
            let target = native["targetPackage"].as_str()
                .filter(|value| (3..=160).contains(&value.len()) &&
                    value.contains('.') && value.chars().all(|ch|
                        ch.is_ascii_alphanumeric() || matches!(ch,'.'|'_')))
                .ok_or("Android app target not verified")?;
            let summary = match tool {
                "android_app_snapshot" => format!("Read the permitted Android app screen for {target}"),
                "android_app_click" => {
                    if let Some(index)=args["index"].as_u64() {
                        format!("Click visible control {index} in {target}")
                    } else {
                        format!("Tap approved screen coordinate in {target}")
                    }
                }
                "android_app_fill" => format!("Fill a non-password control in {target}"),
                "android_app_gesture" => format!("Perform an approved {} gesture in {target}",
                    args["kind"].as_str().unwrap_or("")),
                _ => unreachable!(),
            };
            // These operations are authorized solely by the RAM-only session
            // grant minted by a genuine, user-approved android_app_start.
            // They must not acquire a separately persistent Always allow rule.
            (summary,format!("Package {target}"),false)
        }
        _ => {
            // A new remote MCP plugin has no proven effect classification.
            // It can be approved once, but cannot acquire a durable grant.
            ("Run the requested verified remote MCP tool".to_owned(),
                format!("Plugin {}",info.plugin_id),false)
        }
    };
    Ok(ApprovalIntent {plugin_id:info.plugin_id.clone(), plugin_name:info.plugin_name.clone(),
        tool_name:info.original_name.clone(), summary, scope, can_always_allow})
}

/// Screen actions can only use a verified live OS-bound session established
/// through the shared Core tunnel. They never launch another app themselves.
fn is_app_screen_action(name: &str) -> bool {
    matches!(name, "android_app_snapshot" | "android_app_click"
        | "android_app_fill" | "android_app_gesture")
}

fn approved_app_handoff_context(status: &Value) -> Result<(String, String), String> {
    if status["connected"] != true || status["active"] != true
        || status["visible"] != true || status["stopRequested"] != false
        || status["automationActionsAvailable"] != true
        || status["screenUnlocked"] != true || status["targetFocused"] != true
    {
        return Err("No approved foreground Android app session".into());
    }
    let session = status["sessionId"].as_str()
        .filter(|id| (8..=128).contains(&id.len())
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .ok_or("Foreground Android app session identity invalid")?;
    let target = status["targetPackage"].as_str()
        .filter(|pkg| (3..=160).contains(&pkg.len())
            && pkg.contains('.')
            && pkg.split('.').all(|part| {
                let mut bytes = part.bytes();
                matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
                    && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
            && *pkg != "ai.mcpagent.app" && !pkg.starts_with("com.android."))
        .ok_or("Foreground Android app package invalid")?;
    Ok((session.to_owned(), target.to_owned()))
}

fn approved_app_resume_matches(
    receipt: &Value, session: &str, target: &str,
) -> Result<(), String> {
    if receipt["resumed"] != true || receipt["active"] != true
        || receipt["connected"] != true || receipt["visible"] != true
        || receipt["stopRequested"] != false
        || receipt["sessionId"].as_str() != Some(session)
        || receipt["targetPackage"].as_str() != Some(target)
    {
        return Err("Previously approved Android app did not return to foreground".into());
    }
    Ok(())
}

fn read_catalog(path: &Path) -> AndroidPluginRegistry {
    match fs::read_to_string(path) {
        Ok(raw) => AndroidPluginRegistry::from_config(&raw)
            .unwrap_or_else(|_| AndroidPluginRegistry::with_builtins()),
        Err(_) => AndroidPluginRegistry::with_builtins(),
    }
}

fn persist_catalog(path: &Path, content: &str) -> Result<(), String> {
    if content.len() > 16 * 1024 {
        return Err("Plugin catalog exceeds permitted size".into());
    }
    // Passive status polls and cached tools/call may both re-probe. Do not
    // rewrite the same durable identity on every read (Android flash wear).
    if fs::read(path).is_ok_and(|saved|saved==content.as_bytes()) {
        return Ok(());
    }
    let parent = path.parent().ok_or("Plugin settings directory missing")?;
    fs::create_dir_all(parent).map_err(|_| "Plugin settings cannot be saved")?;
    let tmp = parent.join(format!("plugins-{}.tmp", uuid::Uuid::new_v4()));
    let outcome = (|| {
        use std::io::Write;
        let mut file = fs::OpenOptions::new().write(true).create_new(true)
            .open(&tmp).map_err(|_| "Plugin settings cannot be saved")?;
        file.write_all(content.as_bytes()).map_err(|_| "Plugin settings cannot be saved")?;
        file.sync_all().map_err(|_| "Plugin settings cannot be saved")?;
        fs::rename(&tmp,path).map_err(|_| "Plugin settings cannot be saved")
    })();
    if outcome.is_err() { let _ = fs::remove_file(&tmp); }
    outcome.map_err(str::to_owned)
}

fn rpc_error(id: Value,code:i64, message:&'static str)->RpcReply {
    RpcReply {payload:Some(json!({"jsonrpc":"2.0","id":id,
        "error":{"code":code,"message":message}})),successful_tool_call:false}
}

fn rpc_result(id: Value,result:Value,tool:bool)->RpcReply {
    RpcReply{payload:Some(json!({"jsonrpc":"2.0","id":id,"result":result})),
        successful_tool_call:tool}
}

fn safe_tool_content(mut output:Value,is_approved_native_app_snapshot:bool)->Result<Value,String>{
    if is_approved_native_app_snapshot && output["imageAvailable"]==true {
        // Only our native, separately approved screen-snapshot operation may
        // produce MCP image media. Never rewrite arbitrary remote MCP output
        // into an image or include personal pixels in a log/text block.
        if output["imageMimeType"]!="image/jpeg" ||
            output["imageBytes"].as_u64().is_none_or(|n|n==0 || n>128*1024) {
            return Err("Android screenshot media was not validated".into());
        }
        let image=output.get("imageBase64").and_then(Value::as_str)
            .filter(|data|data.len()<=175000 && data.len()%4==0 && data.starts_with("/9j/"))
            .ok_or("Android screenshot image missing")?.to_owned();
        let obj=output.as_object_mut().ok_or("Android screenshot receipt invalid")?;
        obj.remove("imageBase64");
        let description=output.to_string();
        if description.len()>MAX_OUTPUT{return Err("Android screenshot metadata too large".into())}
        return Ok(json!({"content":[
            {"type":"text","text":description},
            {"type":"image","data":image,"mimeType":"image/jpeg"}
        ],"isError":false}));
    }
    let text=output.to_string();
    if text.len()>MAX_OUTPUT{return Err("Plugin output too large".into())}
    Ok(json!({"content":[{"type":"text","text":text}],"isError":false}))
}

fn refresh_native(
    registry:&Arc<Mutex<AndroidPluginRegistry>>,
    host: Arc<dyn AndroidLocalToolHost>,
    config_path:&Path,
) -> Result<(),String> {
    let mut locked=registry.lock().map_err(|_|"Plugin catalog unavailable")?;
    // Migration from v1 MUST durably record newly generated identities before
    // any verification can publish an alias. Never persist OS readiness here.
    let before=locked.export_config()?;
    persist_catalog(config_path,&before)?;
    let ids=locked.list().into_iter().filter(|item|item.enabled)
        .filter(|item|matches!(item.source,PluginSource::NativeBrowser|PluginSource::NativeApp
            | PluginSource::NativeSecurityAudit | PluginSource::NativeReverseEngineering))
        .map(|item|item.id).collect::<Vec<_>>();
    for id in ids {
        // Failed probes clear runtime proof but preserve the durable alias.
        // Rebinding an existing native host may rotate its generation.
        let _=tauri::async_runtime::block_on(locked.verify_native(&id,host.clone()));
    }
    let after=locked.export_config()?;
    if after!=before {
        if let Err(error)=persist_catalog(config_path,&after) {
            // A changed identity may NEVER be advertised before it is durable.
            // Keep the rotated generation in RAM (instead of restoring the old
            // alias) but discard ALL runtime proof. A subsequent refresh must
            // persist this new identity before exposing any verified tools.
            *locked=AndroidPluginRegistry::from_config(&after)?;
            return Err(error);
        }
    }
    Ok(())
}

fn handle_plugin_rpc(
    rpc:&Value,
    registry:&Arc<Mutex<AndroidPluginRegistry>>,
    host:Arc<dyn AndroidLocalToolHost>,
    core_identity:&dyn Fn() -> Option<CoreTunnelIdentity>,
    app_session_grant:&Arc<Mutex<Option<AppSessionGrant>>>,
    deadline:Instant,
    config_path:&Path,
)->RpcReply {
    let Some(obj)=rpc.as_object() else {return rpc_error(Value::Null,-32600,"Invalid Request")};
    let id=obj.get("id").cloned().unwrap_or(Value::Null);
    if obj.get("jsonrpc").and_then(Value::as_str)!=Some("2.0")
        || obj.get("method").and_then(Value::as_str).is_none()
        || obj.get("id").is_some_and(|v| !(v.is_string()||v.is_number()))
        || id.as_str().is_some_and(|s| s.len()>128)
    {return rpc_error(Value::Null,-32600,"Invalid Request")}
    // Notifications cannot ever execute side effects.
    if !obj.contains_key("id") {return RpcReply{payload:None,successful_tool_call:false}}
    let params=obj.get("params").unwrap_or(&Value::Null);
    match obj["method"].as_str().unwrap_or("") {
        "initialize" => {
            if !params.is_object(){return rpc_error(id,-32602,"Invalid params")}
            let version=match params["protocolVersion"].as_str(){
                Some("2024-11-05")=>"2024-11-05",Some("2025-03-26")=>"2025-03-26",_=>"2025-06-18"};
            rpc_result(id,json!({"protocolVersion":version,
                "serverInfo":{"name":"mcp-agent-ai-android-plugins","version":env!("CARGO_PKG_VERSION")},
                "capabilities":{"tools":{"listChanged":false}}}),false)
        },
        "ping" => rpc_result(id,json!({}),false),
        "tools/list" => {
            if !params.is_null() && !params.as_object().is_some_and(|p|
                p.keys().all(|k|k=="_meta") && p.get("_meta").is_none_or(Value::is_object)) {
                return rpc_error(id,-32602,"Invalid params");
            }
            if refresh_native(registry,host,config_path).is_err(){
                return rpc_error(id,-32603,"Plugin catalog persistence unavailable")
            }
            let tools=match registry.lock(){Ok(locked)=>locked.tools_list(),
                Err(_)=>return rpc_error(id,-32603,"Plugin catalog unavailable")};
            rpc_result(id,json!({"tools":tools}),false)
        },
        "tools/call" => {
            // ChatGPT may call a cached name before requesting a fresh list.
            // Restore native runtime proof with the persisted generation first.
            // Never accept a call if generation migration cannot be saved.
            if refresh_native(registry,host.clone(),config_path).is_err() {
                return rpc_error(id,-32603,"Plugin catalog persistence unavailable")
            }
            let Some(p)=params.as_object() else {return rpc_error(id,-32602,"Invalid params")};
            if p.keys().any(|k|!matches!(k.as_str(),"name"|"arguments"|"_meta"))
                || !p.get("_meta").is_none_or(Value::is_object){return rpc_error(id,-32602,"Invalid params")}
            let Some(name)=p.get("name").and_then(Value::as_str) else{return rpc_error(id,-32602,"Invalid params")};
            if name.len()>100 || !name.is_ascii() {return rpc_error(id,-32602,"Invalid params")}
            let args=p.get("arguments").cloned().unwrap_or_else(||json!({}));
            if !args.is_object()||serde_json::to_vec(&args).map_or(true,|b|b.len()>MAX_RPC_ARGS){
                return rpc_error(id,-32602,"Invalid params");
            }
            let info = match registry.lock() {
                Ok(locked) => locked.inspect_tool(name,&args),
                Err(_) => Err("Plugin catalog unavailable".into()),
            };
            let native_image = info.as_ref().is_ok_and(|info|
                info.source==PluginSource::NativeApp &&
                info.original_name=="android_app_snapshot");
            let result = (|| -> Result<Value, String> {
                let info=info?;
                    let app_action=info.source==PluginSource::NativeApp &&
                        is_app_screen_action(&info.original_name);
                    let app_start=info.original_name=="android_app_start" &&
                        info.source==PluginSource::NativeApp;
                    let app_stop=info.original_name=="android_app_stop" &&
                        info.source==PluginSource::NativeApp;
                    let read_only_native_status=
                        (info.source==PluginSource::NativeBrowser &&
                            info.original_name=="android_browser_status") ||
                        (info.source==PluginSource::NativeApp &&
                            info.original_name=="android_app_status") ||
                        matches!(info.source,PluginSource::NativeSecurityAudit |
                            PluginSource::NativeReverseEngineering);
                    // The ChatGPT plugin permission setting is the approval
                    // surface. The phone no longer shows a duplicate dialog.
                    // Authenticated, opted-in and verified Core tunnel
                    // calls still pass all native OS and tool-schema checks.
                    let effectful_tool=!read_only_native_status;
                    let identity=core_identity();
                    // Read-only native status remains available even when the
                    // tunnel is disconnected; only actions need a live Core.
                    if effectful_tool {
                        identity.as_ref().ok_or("Core tunnel disconnected")?;
                    }
                    if app_start {
                        // A fresh start can never inherit a previous app session.
                        clear_app_session(app_session_grant);
                    }
                    let context = if app_action {
                        let bound=identity.as_ref().ok_or("Core tunnel disconnected")?;
                        let status=tauri::async_runtime::block_on(host.execute(
                            "android_app_handoff_context", json!({})))?;
                        authorize_app_session_action(app_session_grant,bound,&status)?;
                        Some(approved_app_handoff_context(&status)?)
                    }else{None};
                    if Instant::now() >= deadline {
                        return Err("Plugins request expired".into());
                    }
                    if let Some((session,target))=context {
                        // A session-authorized call leaves the target in the
                        // foreground; no per-call dialog or relaunch is needed.
                        // Recheck the exact OS-bound session just before dispatch.
                        let live=tauri::async_runtime::block_on(host.execute(
                            "android_app_handoff_context",json!({})))?;
                        authorize_app_session_action(app_session_grant,
                            identity.as_ref().ok_or("Core tunnel disconnected")?,&live)?;
                        if approved_app_handoff_context(&live)? != (session,target) {
                            clear_app_session(app_session_grant);
                            return Err("App session changed before action dispatch".into());
                        }
                        if Instant::now() >= deadline {
                            return Err("Android app session action expired".into());
                        }
                    }
                    // Disconnect/reconnect invalidates the entire Core
                    // generation, even when the user reconnects the same ID.
                    if effectful_tool && core_identity()!=identity {
                        clear_app_session(app_session_grant);
                        return Err("Core tunnel changed before tool dispatch".into());
                    }
                    let locked=registry.lock().map_err(|_|"Plugin catalog unavailable".to_owned())?;
                    let output=if effectful_tool {
                        tauri::async_runtime::block_on(locked.execute_approved_tool(name,args.clone()))
                    }else{
                        tauri::async_runtime::block_on(locked.execute_tool(name,args.clone()))
                    };
                    if app_stop {
                        clear_app_session(app_session_grant);
                    }
                    let output=output?;
                    if app_start {
                        let wanted=args["targetPackage"].as_str()
                            .ok_or("Approved app package absent")?;
                        let status=tauri::async_runtime::block_on(host.execute(
                            "android_app_handoff_context",json!({})))?;
                        // No automatic grant just because an overlay exists;
                        // the OS must confirm that exact app, unlocked and focused.
                        if core_identity()!=identity {
                            clear_app_session(app_session_grant);
                            return Err("Core tunnel changed during app start".into());
                        }
                        record_app_session(app_session_grant,
                            identity.as_ref().ok_or("Core tunnel disconnected")?,wanted,&status)?;
                    }
                    Ok(output)
            })();
            match result {
                Ok(output) => {
                    match safe_tool_content(output,native_image) {
                        Ok(content)=>rpc_result(id,content,true),
                        Err(_)=>rpc_error(id,-32603,"Plugin output too large"),
                    }
                },
                Err(_)=>rpc_result(id,json!({"content":[{"type":"text",
                    "text":"Plugin tool unavailable, not authorized, or failed; inspect its state before retrying."}],
                    "isError":true}),false)
            }
        },
        _=>rpc_error(id,-32601,"Method not found")
    }
}

fn handle_plugin_rpc_with_evidence(
    rpc:&Value,
    registry:&Arc<Mutex<AndroidPluginRegistry>>,
    host:Arc<dyn AndroidLocalToolHost>,
    core_identity:&dyn Fn() -> Option<CoreTunnelIdentity>,
    app_session_grant:&Arc<Mutex<Option<AppSessionGrant>>>,
    deadline:Instant,
    config_path:&Path,
    evidence:&Arc<Mutex<PluginCallEvidence>>,
)->RpcReply {
    let before=core_identity();
    let reply=handle_plugin_rpc(rpc,registry,host,core_identity,
        app_session_grant,deadline,config_path);
    if reply.successful_tool_call {
        record_successful_plugin_call(evidence,before,core_identity());
    }
    reply
}

impl AndroidPluginsRuntime {
    pub(crate) fn new(
        app:AppHandle,
        core_tunnel:AndroidOpenAiTunnel,
        data_dir:PathBuf,
    )->Self {
        let config_path=Arc::new(data_dir.join(CATALOG_FILE));
        let registry=Arc::new(Mutex::new(read_catalog(&config_path)));
        let android_host:Arc<dyn AndroidLocalToolHost>=Arc::new(Host::new(app));
        let host:Arc<dyn AndroidLocalToolHost>=Arc::new(MarketplaceHost(android_host));
        let app_session_grant=Arc::new(Mutex::new(None));
        let plugin_evidence=Arc::new(Mutex::new(PluginCallEvidence::default()));
        let registry_rpc=registry.clone();
        let host_rpc=host.clone();
        let core_rpc=core_tunnel.clone();
        let app_grant_rpc=app_session_grant.clone();
        let config_rpc=config_path.clone();
        let evidence_rpc=plugin_evidence.clone();
        let handler:PluginRpcHandler=Arc::new(move |rpc,deadline|
            handle_plugin_rpc_with_evidence(rpc,&registry_rpc,host_rpc.clone(),
                &|| live_core_identity(&core_rpc),&app_grant_rpc,deadline,
                &config_rpc,&evidence_rpc));
        core_tunnel.attach_plugins_handler(handler)
            .expect("Shared Core tunnel Plugins handler already registered");
        Self{tunnel:core_tunnel,registry,host,config_path,
            app_session_grant,plugin_evidence}
    }

    fn projection(&self)->Result<Value,String>{
        let mut result=self.tunnel.snapshot()?;
        // The Core snapshot includes core echo/file receipts. The Plugins
        // status is verified only by plugin tools/call on this live generation.
        project_plugin_call_evidence(&mut result,&self.plugin_evidence,
            live_core_identity(&self.tunnel))?;
        let catalog=self.registry.lock().map_err(|_|"Plugin catalog unavailable")?.catalog();
        let items=catalog["plugins"].as_array().ok_or("Plugin catalog unavailable")?;
        let installed=items.iter().map(|p|{
            // MCP publishes a fixed, verified native capability catalog so
            // ChatGPT aliases survive browser start/stop transitions. The UI
            // must report actual last-probed OS readiness, not all advertised
            // capabilities as if each action could execute right now.
            let count=p["readyToolCount"].as_u64().unwrap_or(0);
            let id=p["id"].as_str().unwrap_or("");
            json!({"id":id,"name":p["name"],"description":
                if id=="browser" {"App-owned offscreen browser tools"}
                else if id=="app_control" {"Android Accessibility tools"}
                else if id=="security_audit" {"Bounded static text security analysis"}
                else if id=="reverse_engineering" {"Android DEX header inspection"}
                else {"Verified remote MCP tools"},
                "enabled":p["enabled"]==true,"ready":count>0 && p["toolsAvailable"]==true,
                "available":matches!(id,"browser"|"app_control"|"security_audit"|"reverse_engineering"),
                "tools":count,"state":p["state"],"category":p["category"],
                "version":p["version"],"risk":p["risk"],"sourceLabel":p["sourceLabel"],
                "permissions":p["permissions"],"toolNames":p["toolNames"]})
        }).collect::<Vec<_>>();
        result["plugins"]=json!(installed);
        let ready_total=items.iter().map(|item|item["readyToolCount"].as_u64().unwrap_or(0))
            .sum::<u64>();
        result["availableTools"]=json!(ready_total);
        if let Some(id)=self.tunnel.confirmed_id() {
            result["tunnelId"]=json!(id);
        }
        result["connectorName"]=json!(CONNECTOR_NAME);
        result["description"]=json!(CONNECTOR_DESCRIPTION);
        Ok(result)
    }

    fn change_enabled(&self,id:&str,enabled:bool)->Result<(),String>{
        if !matches!(id,"browser"|"app_control"|"security_audit"|"reverse_engineering") {
            return Err("Plugin cannot be enabled from this build".into());
        }
        if !enabled {
            if id=="app_control" {clear_app_session(&self.app_session_grant);}
            // Disabling a native module cannot keep a live app-control grant.
        }
        let mut locked=self.registry.lock().map_err(|_|"Plugin catalog unavailable")?;
        locked.set_enabled(id,enabled)?;
        let after=locked.export_config()?;
        if let Err(error)=persist_catalog(&self.config_path,&after){
            // A failed write cannot resurrect a revoked native alias by
            // restoring the previous in-memory generation. Keep the new
            // disabled/enabled preference pending in RAM with runtime proof
            // cleared, and retry persistence before any future tools/list.
            *locked=AndroidPluginRegistry::from_config(&after)?;
            return Err(error);
        }
        Ok(())
    }
}

#[tauri::command]
pub async fn android_plugins_status(state:tauri::State<'_,AndroidPluginsRuntime>)->Result<Value,String>{
    // Enabled modules are durable configuration, but readiness is deliberately
    // cleared on process restart. A status read must repeat genuine read-only
    // OS probes so the Settings page reflects actual live tool availability
    // without requiring the user to toggle modules off and on again.
    let runtime=state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        refresh_native(&runtime.registry,runtime.host.clone(),&runtime.config_path)?;
        runtime.projection()
    }).await.map_err(|_|"Android Plugins status interrupted".to_owned())?
}

#[tauri::command]
pub async fn android_plugins_catalog(state:tauri::State<'_,AndroidPluginsRuntime>)->Result<Value,String>{
    state.projection()
}

#[tauri::command]
pub async fn android_plugins_set_enabled(
    state:tauri::State<'_,AndroidPluginsRuntime>,
    plugin_id:String,enabled:bool,user_initiated:bool,
)->Result<Value,String>{
    if !cfg!(target_os="android")||!user_initiated{return Err("Explicit Android Plugins action required".into())}
    let runtime=state.inner().clone();
    tauri::async_runtime::spawn_blocking(move ||{
        runtime.change_enabled(&plugin_id,enabled)?;
        if enabled {
            refresh_native(&runtime.registry,runtime.host.clone(),&runtime.config_path)?;
        }
        runtime.projection()
    }).await.map_err(|_|"Android plugin configuration interrupted".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize,Ordering};

    fn live_test_identity(generation:u64)->CoreTunnelIdentity {
        CoreTunnelIdentity {
            tunnel_id:"tunnel_0123456789abcdef0123456789abcdef".into(),generation,
        }
    }

    #[test]
    fn core_echo_never_verifies_plugins_but_plugin_call_does_for_one_generation() {
        let evidence=Arc::new(Mutex::new(PluginCallEvidence::default()));
        let live=Arc::new(Mutex::new(Some(live_test_identity(4))));
        let identity=||live.lock().unwrap().clone();
        // This is the Core status after its own echo was acknowledged.
        let mut core_snapshot=json!({"pollActive":true,"state":"tool_verified",
            "toolCallsVerified":12});
        project_plugin_call_evidence(&mut core_snapshot,&evidence,identity()).unwrap();
        assert_eq!(core_snapshot["toolCallsVerified"],0);
        assert_eq!(core_snapshot["state"],"polling",
            "Core tool receipts must not verify the Plugins surface");

        let host:Arc<dyn AndroidLocalToolHost>=Arc::new(AppStatusHost);
        let registry=Arc::new(Mutex::new(AndroidPluginRegistry::with_builtins()));
        registry.lock().unwrap().set_enabled("app_control",true).unwrap();
        let catalog=catalog_for_test();
        refresh_native(&registry,host.clone(),&catalog).unwrap();
        let alias=registry.lock().unwrap().tools_list().into_iter()
            .find_map(|tool|tool["name"].as_str()
                .filter(|name|name.ends_with("_a_status")).map(str::to_owned))
            .expect("Enabled app status alias is published");
        let grant=Arc::new(Mutex::new(None));
        let listed=handle_plugin_rpc_with_evidence(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            &registry,host.clone(),&identity,&grant,
            Instant::now()+Duration::from_secs(15),&catalog,&evidence);
        assert!(!listed.successful_tool_call);
        assert_eq!(plugin_call_count(&evidence,identity()).unwrap(),0);

        let called=handle_plugin_rpc_with_evidence(
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                "params":{"name":alias,"arguments":{}}}),
            &registry,host,&identity,&grant,
            Instant::now()+Duration::from_secs(15),&catalog,&evidence);
        assert!(called.successful_tool_call,"Real plugin call must execute");
        project_plugin_call_evidence(&mut core_snapshot,&evidence,identity()).unwrap();
        assert_eq!(core_snapshot["toolCallsVerified"],1);
        assert_eq!(core_snapshot["state"],"tool_verified");

        // Disconnect/reconnect with identical Tunnel ID starts a fresh proof.
        *live.lock().unwrap()=Some(live_test_identity(5));
        project_plugin_call_evidence(&mut core_snapshot,&evidence,identity()).unwrap();
        assert_eq!(core_snapshot["toolCallsVerified"],0);
        assert_eq!(core_snapshot["state"],"polling");
        let _=fs::remove_file(catalog);
    }

    fn live_android_session() -> Value {
        json!({"connected":true,"active":true,"visible":true,
            "stopRequested":false,"automationActionsAvailable":true,
            "screenUnlocked":true,"targetFocused":true,
            "sessionId":"qa_session_1234","targetPackage":"com.google.android.youtube"})
    }

    #[test]
    fn one_approval_mints_only_the_exact_live_tunnel_app_session() {
        let grant=Arc::new(Mutex::new(None));
        let live=live_android_session();
        let tunnel=live_test_identity(3);
        assert!(authorize_app_session_action(&grant,&tunnel,&live).is_err());
        assert!(record_app_session(&grant,&tunnel,"com.example.other",&live).is_err());
        assert!(grant.lock().unwrap().is_none());
        assert!(record_app_session(&grant,&tunnel,"com.google.android.youtube",&live).is_ok());
        assert!(authorize_app_session_action(&grant,&tunnel,&live).is_ok());

        let mut same_target_new_session=live.clone();
        same_target_new_session["sessionId"]=json!("another_session_5678");
        assert!(authorize_app_session_action(&grant,&tunnel,&same_target_new_session).is_err());
        assert!(grant.lock().unwrap().is_none(), "A changed session revokes the grant");
        record_app_session(&grant,&tunnel,"com.google.android.youtube",&live).unwrap();
        let switched=CoreTunnelIdentity {
            tunnel_id:"tunnel_different000000000000000000".into(),generation:3,
        };
        assert!(authorize_app_session_action(&grant,&switched,&live).is_err());
        assert!(grant.lock().unwrap().is_none(), "Other tunnel cannot inherit the grant");
        record_app_session(&grant,&tunnel,"com.google.android.youtube",&live).unwrap();
        let reconnected=live_test_identity(4);
        assert!(authorize_app_session_action(&grant,&reconnected,&live).is_err(),
            "Same Core tunnel ID must lose its grant after reconnect");
        assert!(grant.lock().unwrap().is_none());
    }

    #[test]
    fn screen_lock_switch_stop_expiry_and_foreground_loss_revoke_approval() {
        let tunnel=live_test_identity(8);
        for (field,replacement) in [
            ("connected",json!(false)),("active",json!(false)),
            ("visible",json!(false)),("stopRequested",json!(true)),
            ("screenUnlocked",json!(false)),("targetFocused",json!(false)),
            ("automationActionsAvailable",json!(false)),
            ("targetPackage",json!("com.android.settings")),
        ] {
            let grant=Arc::new(Mutex::new(None));
            let live=live_android_session();
            record_app_session(&grant,&tunnel,"com.google.android.youtube",&live).unwrap();
            let mut changed=live;
            changed[field]=replacement;
            assert!(authorize_app_session_action(&grant,&tunnel,&changed).is_err(),"{field}");
            assert!(grant.lock().unwrap().is_none(),"{field} must revoke in RAM");
        }
        let grant=Arc::new(Mutex::new(None));
        let live=live_android_session();
        record_app_session(&grant,&tunnel,"com.google.android.youtube",&live).unwrap();
        grant.lock().unwrap().as_mut().unwrap().expires_at=Instant::now()
            .checked_sub(Duration::from_secs(1)).unwrap();
        assert!(authorize_app_session_action(&grant,&tunnel,&live).is_err());
        assert!(grant.lock().unwrap().is_none());
        record_app_session(&grant,&tunnel,"com.google.android.youtube",&live).unwrap();
        clear_app_session(&grant);
        assert!(authorize_app_session_action(&grant,&tunnel,&live).is_err());
    }

    #[test]
    fn native_app_start_grants_session_only_and_remote_tools_cannot_impersonate() {
        let info=PluginToolInfo {
            plugin_id:"app_control".into(),plugin_name:"Android app control".into(),
            original_name:"android_app_start".into(),source:PluginSource::NativeApp,
        };
        let host:Arc<dyn AndroidLocalToolHost>=Arc::new(TestHost);
        let intent=approval_intent(&info,
            &json!({"targetPackage":"com.google.android.youtube"}),&host).unwrap();
        assert!(!intent.can_always_allow, "Start must not create persistent Always allow rules");
        assert!(intent.summary.contains("screen reading"));
        assert!(intent.scope.contains("this session only"));
        let remote=PluginToolInfo {
            source:PluginSource::RemoteMcp{tunnel_id:"tunnel_other".into()},
            original_name:"android_app_snapshot".into(),..info
        };
        assert_ne!(remote.source,PluginSource::NativeApp);
        // Remote MCP may reuse a native tool's original name, but its source
        // must NEVER inherit the Android app session's consent.
        assert!(!(remote.source==PluginSource::NativeApp &&
            is_app_screen_action(&remote.original_name)));
        let remote_intent=approval_intent(&remote,&json!({}),&host).unwrap();
        assert_eq!(remote_intent.summary,"Run the requested verified remote MCP tool");
        assert!(!remote_intent.can_always_allow);
        assert_eq!(remote_intent.scope,"Plugin app_control");
    }

    struct SessionTestHost {
        running: AtomicBool,
        snapshots: AtomicUsize,
    }

    impl AndroidLocalToolHost for SessionTestHost {
        fn execute<'a>(&'a self,name:&'a str,args:Value)
            ->std::pin::Pin<Box<dyn std::future::Future<Output=Result<Value,String>>+Send+'a>> {
            Box::pin(async move {
                let running=self.running.load(Ordering::SeqCst);
                match name {
                    "android_app_status"|"android_app_handoff_context" => Ok(json!({
                        "supported":true,"connected":true,"active":running,
                        "visible":running,"stopRequested":false,
                        "automationActionsAvailable":running,
                        "selectedAppMode":"realtime","screenUnlocked":true,
                        "targetFocused":running,"sessionId":if running {"test-session-1234"}else{""},
                        "targetPackage":if running {"com.google.android.youtube"}else{""},
                        "launched":false,
                    })),
                    "android_app_start" if args["targetPackage"]=="com.google.android.youtube" => {
                        self.running.store(true,Ordering::SeqCst);
                        Ok(json!({"supported":true,"connected":true,"active":true,
                            "visible":true,"stopRequested":false,"launched":true,
                            "targetPackage":"com.google.android.youtube",
                            "selectedAppMode":"realtime","automationActionsAvailable":true}))
                    },
                    "android_app_snapshot" if running => {
                        self.snapshots.fetch_add(1,Ordering::SeqCst);
                        Ok(json!({"ok":true,"selectedMode":"realtime","effectiveMode":"realtime",
                            "fallbackUsed":false,"snapshotId":"4f456752-2fd4-47bc-becf-1132e63838ef",
                            "mimeType":"none","screenWidth":720,"screenHeight":1600,
                            "elements":[{"index":1,"className":"android.widget.Button",
                                "text":"Continue","description":"","clickable":true,
                                "editable":false,"protected":false,
                                "left":30,"top":30,"right":120,"bottom":80}]}))
                    },
                    "android_app_stop" if running => {
                        self.running.store(false,Ordering::SeqCst);
                        Ok(json!({"supported":true,"connected":true,"active":false,
                            "visible":false,"stopRequested":false,"launched":false,
                            "targetPackage":"","selectedAppMode":"realtime",
                            "automationActionsAvailable":false}))
                    },
                    _=>Err("Mock refuses an unauthorized or unexpected Android operation".into()),
                }
            })
        }
    }

    #[test]
    fn chatgpt_permission_controls_tools_without_second_android_popup_and_stop_revokes() {
        let host_impl=Arc::new(SessionTestHost {
            running:AtomicBool::new(false),snapshots:AtomicUsize::new(0),
        });
        let host:Arc<dyn AndroidLocalToolHost>=host_impl.clone();
        let registry=Arc::new(Mutex::new(AndroidPluginRegistry::with_builtins()));
        registry.lock().unwrap().set_enabled("app_control",true).unwrap();
        let catalog=catalog_for_test();
        refresh_native(&registry,host.clone(),&catalog).unwrap();
        let tools=registry.lock().unwrap().tools_list();
        let alias=|suffix:&str|->String {tools.iter().find_map(|t|{
            let name=t["name"].as_str()?;
            name.ends_with(suffix).then(||name.to_owned())
        }).unwrap()};
        let start=alias("_a_start");
        let snapshot=alias("_a_snapshot");
        let stop=alias("_a_stop");
        let tunnel_id=Arc::new(Mutex::new(Some(live_test_identity(9))));
        let core_identity=||tunnel_id.lock().unwrap().clone();
        let grant=Arc::new(Mutex::new(None));
        let invoke=|alias:&str,params:Value|json!({"jsonrpc":"2.0","id":1,
            "method":"tools/call","params":{"name":alias,"arguments":params}});
        let listed=handle_plugin_rpc(&json!({"jsonrpc":"2.0","id":100,
            "method":"tools/list"}),&registry,host.clone(),&core_identity,
            &grant,Instant::now()+Duration::from_secs(10),&catalog);
        let published=listed.payload.unwrap()["result"]["tools"].as_array().unwrap().clone();
        assert!(published.iter().any(|tool|tool["name"]==start),
            "Enabled native aliases must publish on the shared Core RPC channel");
        let call_start=invoke(&start,json!({"targetPackage":"com.google.android.youtube"}));
        *tunnel_id.lock().unwrap()=None;
        let blocked=handle_plugin_rpc(&call_start,&registry,host.clone(),&core_identity,
            &grant,Instant::now()+Duration::from_secs(10),&catalog);
        assert!(!blocked.successful_tool_call,"Disconnected Core tunnel cannot open YouTube");
        assert!(!host_impl.running.load(Ordering::SeqCst));
        assert!(grant.lock().unwrap().is_none());
        *tunnel_id.lock().unwrap()=Some(live_test_identity(9));
        let result=handle_plugin_rpc(&call_start,&registry,host.clone(),&core_identity,
            &grant,Instant::now()+Duration::from_secs(20),&catalog);
        assert!(result.successful_tool_call,"ChatGPT-authorized start: {:?}",result.payload);
        assert!(grant.lock().unwrap().is_some(),"Valid start mints only RAM session grant");
        for _ in 0..2 {
            let output=handle_plugin_rpc(
                &invoke(&snapshot,json!({})),&registry,host.clone(),
                &core_identity,&grant,Instant::now()+Duration::from_secs(10),&catalog);
            assert!(output.successful_tool_call,"No Android approval popup: {:?}",output.payload);
            assert!(output.payload.unwrap()["result"]["content"][0]["text"]
                .as_str().unwrap().contains("snapshotId"));
        }
        assert_eq!(host_impl.snapshots.load(Ordering::SeqCst),2);
        // Reconnecting the *same* Core ID changes the generation and must
        // invalidate the previous OS grant before the next screen read.
        *tunnel_id.lock().unwrap()=Some(live_test_identity(10));
        let stale=handle_plugin_rpc(&invoke(&snapshot,json!({})),
            &registry,host.clone(),&core_identity,&grant,
            Instant::now()+Duration::from_secs(10),&catalog);
        assert!(!stale.successful_tool_call,"Stale grant survived Core reconnect");
        assert!(grant.lock().unwrap().is_none());
        assert_eq!(host_impl.snapshots.load(Ordering::SeqCst),2);
        let finished=handle_plugin_rpc(&invoke(&stop,json!({})),&registry,host.clone(),
            &core_identity,&grant,Instant::now()+Duration::from_secs(10),&catalog);
        assert!(finished.successful_tool_call);
        assert!(grant.lock().unwrap().is_none(),"Stop must revoke session permission");
        let denied=handle_plugin_rpc(&invoke(&snapshot,json!({})),&registry,host,
            &core_identity,&grant,Instant::now()+Duration::from_secs(10),&catalog);
        assert!(!denied.successful_tool_call,"Stopped session cannot read screen");
        let _=fs::remove_file(catalog);
    }

    #[test]
    fn app_screen_handoff_requires_exact_approved_session_before_and_after() {
        let pre=json!({"connected":true,"active":true,"visible":true,
            "stopRequested":false,"automationActionsAvailable":true,
            "screenUnlocked":true,"targetFocused":true,
            "sessionId":"qa_session_1234","targetPackage":"com.example.app"});
        assert!(is_app_screen_action("android_app_snapshot"));
        assert!(is_app_screen_action("android_app_click"));
        assert!(is_app_screen_action("android_app_fill"));
        assert!(is_app_screen_action("android_app_gesture"));
        assert!(!is_app_screen_action("android_app_status"));
        let (session,target)=approved_app_handoff_context(&pre).unwrap();
        assert_eq!(session,"qa_session_1234");
        assert_eq!(target,"com.example.app");
        let post=json!({"resumed":true,"connected":true,"active":true,
            "visible":true,"stopRequested":false,
            "sessionId":session,"targetPackage":target});
        assert!(approved_app_resume_matches(&post,&session,&target).is_ok());
        for (key,value) in [
            ("visible",json!(false)),("connected",json!(false)),
            ("active",json!(false)),("resumed",json!(false)),
            ("stopRequested",json!(true)),
            ("sessionId",json!("new_session_5678")),
            ("targetPackage",json!("com.other.app")),
        ] {
            let mut wrong=post.clone();
            wrong[key]=value;
            assert!(approved_app_resume_matches(&wrong,&session,&target).is_err());
        }
        for (key,value) in [
            ("visible",json!(false)),("connected",json!(false)),
            ("active",json!(false)),("stopRequested",json!(true)),
            ("automationActionsAvailable",json!(false)),
            ("screenUnlocked",json!(false)),("targetFocused",json!(false)),
            ("sessionId",json!("oops")),("targetPackage",json!("ai.mcpagent.app")),
        ] {
            let mut wrong=pre.clone();
            wrong[key]=value;
            assert!(approved_app_handoff_context(&wrong).is_err());
        }
    }

    #[test]
    fn native_approved_screen_image_is_mcp_media_never_plain_text() {
        let encoded=format!("/9j/{}", "A".repeat(168));
        let payload=json!({"imageAvailable":true,"imageMimeType":"image/jpeg",
            "imageBytes":129,"imageBase64":encoded,"snapshotId":"test-snapshot",
            "screenWidth":720,"screenHeight":1600,"effectiveMode":"screenshot_click"});
        let output=safe_tool_content(payload.clone(),true).unwrap();
        assert_eq!(output["content"][1]["type"],"image");
        assert_eq!(output["content"][1]["mimeType"],"image/jpeg");
        assert_eq!(output["content"][1]["data"],encoded);
        let description=output["content"][0]["text"].as_str().unwrap();
        assert!(description.contains("snapshotId"));
        assert!(!description.contains("/9j/"));
        // A remote MCP tool cannot impersonate a native image through
        // arbitrary JSON output. It is still displayed only as plain text.
        let remote=safe_tool_content(payload,false).unwrap();
        assert_eq!(remote["content"].as_array().unwrap().len(),1);
        assert_eq!(remote["content"][0]["type"],"text");
        let mut missing=json!({"imageAvailable":true,"imageMimeType":"image/jpeg",
            "imageBytes":129,"snapshotId":"test-snapshot"});
        assert!(safe_tool_content(missing.clone(),true).is_err());
        missing["imageBase64"]=json!("not an image");
        assert!(safe_tool_content(missing,true).is_err());
    }

    struct TestHost;
    impl AndroidLocalToolHost for TestHost {
        fn execute<'a>(&'a self,_name:&'a str,_args:Value)->std::pin::Pin<Box<dyn std::future::Future<Output=Result<Value,String>>+Send+'a>>{
            Box::pin(async {Err("not connected".into())})
        }
    }

    struct AppStatusHost;
    impl AndroidLocalToolHost for AppStatusHost {
        fn execute<'a>(&'a self,name:&'a str,_args:Value)
            ->std::pin::Pin<Box<dyn std::future::Future<Output=Result<Value,String>>+Send+'a>>{
            Box::pin(async move {
                match name {
                    "android_app_status" => Ok(json!({
                        "supported":true,"connected":true,"active":true,
                        "visible":true,"stopRequested":false,
                        "targetPackage":"com.google.android.youtube",
                        "automationActionsAvailable":true,"selectedAppMode":"realtime"
                    })),
                    _ => Err("Test refuses native action side effects".into()),
                }
            })
        }
    }

    fn catalog_for_test() -> PathBuf {
        std::env::temp_dir().join(format!("mcp-plugins-alias-test-{}.json",uuid::Uuid::new_v4()))
    }

    #[test]
    fn curated_marketplace_tools_are_routed_and_sanitized_through_shared_rpc() {
        // The wrapped Android host rejects every call. Success here therefore
        // proves the curated route is compiled in, independent of UI claims.
        let host:Arc<dyn AndroidLocalToolHost>=Arc::new(MarketplaceHost(Arc::new(TestHost)));
        let registry=Arc::new(Mutex::new(AndroidPluginRegistry::with_builtins()));
        let path=catalog_for_test();
        let identity=||None;
        let grant=Arc::new(Mutex::new(None));
        let status=|id:&str|registry.lock().unwrap().catalog()["plugins"].as_array()
            .unwrap().iter().find(|entry|entry["id"]==id).unwrap().clone();
        assert_eq!(status("security_audit")["enabled"],false);
        assert_eq!(status("security_audit")["category"],"security");
        assert_eq!(status("reverse_engineering")["category"],"development");
        assert_eq!(status("reverse_engineering")["risk"],"read_only");
        refresh_native(&registry,host.clone(),&path).unwrap();
        assert!(registry.lock().unwrap().tools_list().is_empty());
        registry.lock().unwrap().set_enabled("security_audit",true).unwrap();
        registry.lock().unwrap().set_enabled("reverse_engineering",true).unwrap();
        refresh_native(&registry,host.clone(),&path).unwrap();
        let listing=handle_plugin_rpc(&json!({"jsonrpc":"2.0","id":9,
            "method":"tools/list"}),&registry,host.clone(),&identity,&grant,
            Instant::now()+Duration::from_secs(10),&path);
        assert_eq!(listing.payload.unwrap()["result"]["tools"].as_array().unwrap().len(),2);
        let names=registry.lock().unwrap().tools_list().into_iter()
            .filter_map(|def|def["name"].as_str().map(str::to_owned)).collect::<Vec<_>>();
        assert_eq!(names.len(),2);
        let audit=names.iter().find(|name|name.ends_with("_s_analyze")).unwrap();
        let raw_secret="sk-DO_NOT_ECHO_PRIVATE_VALUE";
        let response=handle_plugin_rpc(&json!({"jsonrpc":"2.0","id":1,
            "method":"tools/call","params":{"name":audit,
                "arguments":{"text":format!("api_key = {raw_secret}\nhttp://bad.test")}}}),
            &registry,host.clone(),&identity,&grant,Instant::now()+Duration::from_secs(10),&path);
        assert!(response.successful_tool_call);
        let returned=response.payload.unwrap()["result"]["content"][0]["text"].as_str().unwrap().to_owned();
        assert!(returned.contains("possibleCredentials"));
        assert!(!returned.contains(raw_secret));

        // A direct call with an invalid DEX header must return an MCP error,
        // and the security tool is rejected after disable and alias rotation.
        let dex=names.iter().find(|name|name.ends_with("_r_dex_header")).unwrap();
        let mut header=[0u8;112];
        header[..8].copy_from_slice(b"dex\n039\0");
        header[32..36].copy_from_slice(&512u32.to_le_bytes());
        header[36..40].copy_from_slice(&112u32.to_le_bytes());
        header[40..44].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        header[52..56].copy_from_slice(&116u32.to_le_bytes());
        header[104..108].copy_from_slice(&400u32.to_le_bytes());
        header[108..112].copy_from_slice(&112u32.to_le_bytes());
        let header_hex:String=header.iter().map(|b|format!("{b:02x}")).collect();
        let good=handle_plugin_rpc(&json!({"jsonrpc":"2.0","id":10,
            "method":"tools/call","params":{"name":dex,
                "arguments":{"hexHeader":header_hex.clone()}}}),
            &registry,host.clone(),&identity,&grant,Instant::now()+Duration::from_secs(10),&path);
        assert!(good.successful_tool_call);
        let summary=good.payload.unwrap()["result"]["content"][0]["text"].as_str().unwrap().to_owned();
        assert!(summary.contains("dexVersion"));
        assert!(summary.contains("039"));
        assert!(!summary.contains(&header_hex));
        let bad=handle_plugin_rpc(&json!({"jsonrpc":"2.0","id":2,
            "method":"tools/call","params":{"name":dex,
                "arguments":{"hexHeader":"00".repeat(112)}}}),
            &registry,host.clone(),&identity,&grant,Instant::now()+Duration::from_secs(10),&path);
        assert!(!bad.successful_tool_call);
        assert_eq!(bad.payload.unwrap()["result"]["isError"],true);
        registry.lock().unwrap().set_enabled("security_audit",false).unwrap();
        persist_catalog(&path,&registry.lock().unwrap().export_config().unwrap()).unwrap();
        let old=handle_plugin_rpc(&json!({"jsonrpc":"2.0","id":3,
            "method":"tools/call","params":{"name":audit,"arguments":{"text":"safe"}}}),
            &registry,host,&identity,&grant,Instant::now()+Duration::from_secs(10),&path);
        assert!(!old.successful_tool_call);
        assert_eq!(old.payload.unwrap()["result"]["isError"],true);
        let saved=read_catalog(&path);
        assert_eq!(saved.list().iter().find(|s|s.id=="security_audit").unwrap().enabled,false);
        let _=fs::remove_file(path);
    }

    #[test]
    fn catalog_write_replaces_existing_v2_and_roundtrips_without_tool_grants() {
        let path=catalog_for_test();
        persist_catalog(&path,"{\"version\":1}").unwrap();
        persist_catalog(&path,"{\"version\":2}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(),"{\"version\":2}");
        let _=fs::remove_file(path);
    }

    #[test]
    fn cached_native_app_status_survives_real_persisted_restart_and_direct_rpc() {
        let path=catalog_for_test();
        let legacy=json!({"version":1,"plugins":[{
            "id":"app_control","name":"Android app control",
            "source":{"kind":"native_app"},"enabled":true
        }]});
        fs::write(&path,legacy.to_string()).unwrap();
        let host:Arc<dyn AndroidLocalToolHost>=Arc::new(AppStatusHost);
        let first=Arc::new(Mutex::new(read_catalog(&path)));
        assert!(first.lock().unwrap().tools_list().is_empty());
        refresh_native(&first,host.clone(),&path).unwrap();
        let old=first.lock().unwrap().tools_list().into_iter()
            .find_map(|tool| {
                let alias=tool["name"].as_str()?;
                alias.ends_with("_a_status").then(||alias.to_owned())
            }).expect("Verified native a_status must be advertised");
        let saved=fs::read_to_string(&path).unwrap();
        let encoded:Value=serde_json::from_str(&saved).unwrap();
        assert_eq!(encoded["version"],2);
        assert!(encoded["nativeGenerations"]["app_control"].is_string());
        assert_eq!(encoded["plugins"][0]["enabled"],true);
        assert!(encoded.get("runtime").is_none());

        // Simulated process restart constructs a fresh registry and native
        // host. It MUST re-probe the OS and then recognize the cached alias,
        // without any initial tools/list exchange.
        let second=Arc::new(Mutex::new(read_catalog(&path)));
        assert!(second.lock().unwrap().tools_list().is_empty());
        let identity=||None;
        let grant=Arc::new(Mutex::new(None));
        let rpc=json!({"jsonrpc":"2.0","id":17,"method":"tools/call",
            "params":{"name":old,"arguments":{}}});
        let reply=handle_plugin_rpc(&rpc,&second,host,&identity,&grant,
            Instant::now()+std::time::Duration::from_secs(30),&path);
        assert!(reply.successful_tool_call,"Direct cached a_status must work after restart");
        let response=reply.payload.unwrap();
        assert_eq!(response["result"]["isError"],false);
        assert!(response["result"]["content"][0]["text"].as_str()
            .unwrap().contains("com.google.android.youtube"));
        assert_eq!(fs::read_to_string(&path).unwrap(),saved,
            "Repeated passive status probes must not rotate durable generation");
        let _=fs::remove_file(path);
    }

    #[test]
    fn failed_catalog_write_never_publishes_unpersisted_native_aliases() {
        let path=std::env::temp_dir().join(format!("mcp-alias-missing-{}",uuid::Uuid::new_v4()))
            .join("catalog.json");
        // A file at the would-be parent path prevents creating a directory.
        let invalid_parent=path.parent().unwrap();
        fs::write(invalid_parent,b"not a directory").unwrap();
        let registry=Arc::new(Mutex::new(AndroidPluginRegistry::with_builtins()));
        registry.lock().unwrap().set_enabled("app_control",true).unwrap();
        let host:Arc<dyn AndroidLocalToolHost>=Arc::new(AppStatusHost);
        assert!(refresh_native(&registry,host,&path).is_err());
        assert!(registry.lock().unwrap().tools_list().is_empty());
        let _=fs::remove_file(invalid_parent);
    }

    #[test]
    fn initialize_does_not_advertise_unsent_list_changed_notifications(){
        let registry=Arc::new(Mutex::new(AndroidPluginRegistry::new()));
        let host:Arc<dyn AndroidLocalToolHost>=Arc::new(TestHost);
        let identity=||None;
        let grant=Arc::new(Mutex::new(None));
        let catalog=catalog_for_test();
        let initialize=json!({"jsonrpc":"2.0","id":7,"method":"initialize",
            "params":{"protocolVersion":"2025-06-18","capabilities":{},
                "clientInfo":{"name":"test-client","version":"1"}}});
        let init=handle_plugin_rpc(&initialize,&registry,host.clone(),&identity,&grant,
            Instant::now()+std::time::Duration::from_secs(30),&catalog);
        assert!(!init.successful_tool_call);
        let init_payload=init.payload.expect("initialize response required");
        assert_eq!(init_payload["id"],json!(7));
        assert_eq!(init_payload["result"]["protocolVersion"],json!("2025-06-18"));
        assert_eq!(init_payload["result"]["capabilities"]["tools"],
            json!({"listChanged":false}));

        let listed=handle_plugin_rpc(&json!({"jsonrpc":"2.0","id":8,
            "method":"tools/list"}),&registry,host,&identity,&grant,
            Instant::now()+std::time::Duration::from_secs(30),&catalog);
        assert!(!listed.successful_tool_call);
        assert_eq!(listed.payload.expect("tools/list response required")["result"]["tools"],
            json!([]));
        let _=fs::remove_file(catalog);
    }

    #[test]
    fn shared_plugin_rpc_never_publishes_an_unverified_tool(){
        let registry=Arc::new(Mutex::new(AndroidPluginRegistry::with_builtins()));
        let host:Arc<dyn AndroidLocalToolHost>=Arc::new(TestHost);
        let identity=||None;
        let grant=Arc::new(Mutex::new(None));
        let catalog=catalog_for_test();
        let rpc=json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        let result=handle_plugin_rpc(&rpc,&registry,host.clone(),&identity,&grant,
            Instant::now()+std::time::Duration::from_secs(30),&catalog);
        assert_eq!(result.payload.unwrap()["result"]["tools"],json!([]));
        let no_id=json!({"jsonrpc":"2.0","method":"tools/call",
            "params":{"name":"app_info"}});
        assert!(handle_plugin_rpc(&no_id,&registry,host,&identity,&grant,
            Instant::now()+std::time::Duration::from_secs(30),&catalog).payload.is_none());
        let _=fs::remove_file(catalog);
    }
}
