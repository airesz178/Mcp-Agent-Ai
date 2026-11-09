//! Explicit, outbound-only OpenAI Secure MCP Tunnel runtime for Android.
//! Wire contract: https://github.com/openai/tunnel-client/blob/master/docs/protocol.md
//!
//! The authenticated tunnel exposes the bounded MCP tools individually
//! approved through Android MCP permissions, with no HTTP listener or remote
//! shell/desktop access. Only a previously user-authorized, Keystore-backed
//! tunnel may reconnect when the Android process starts again.

use crate::android_mcp_permissions::AndroidMcpPermissions;
use reqwest::{header, Client, RequestBuilder, Response, StatusCode};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    fs,
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Manager};
use tokio::task::JoinHandle;

const BASE_URL: &str = "https://api.openai.com";
const SERVER_INFO: &str = r#"{"version":1,"channels":[{"name":"main","proc_affinity":true}]}"#;
const POLL_BODY_LIMIT: usize = 512 * 1024;
const COMMAND_LIMIT: usize = 64;
const RPC_BODY_LIMIT: usize = 16 * 1024;
const RESPONSE_BODY_LIMIT: usize = 4096;
const DEFAULT_COMMAND_LIFETIME: Duration = Duration::from_secs(30);
const MAX_COMMAND_LIFETIME: Duration = Duration::from_secs(90);
const MAX_IN_FLIGHT_RESPONSES: usize = 4;
const HEARTBEAT_LIMIT: Duration = Duration::from_secs(45);
const POLL_RETRY_INITIAL: Duration = Duration::from_millis(300);
const POLL_RETRY_MAX: Duration = Duration::from_secs(8);
const TUNNEL_METADATA_LIMIT: usize = 4096;
const CORE_TUNNEL_FILENAME: &str = "mcp-core-tunnel-id-v1.json";
const DESIRED_TUNNEL_FILENAME: &str = "mcp-openai-tunnel-desired-v1.json";
const CORE_PLUGIN_PAIRING_FILENAME: &str = "mcp-core-plugin-pairing-v1.json";
const MAX_TELEMETRY_EVENTS: usize = 64;
/// This in-process HTTPS client disables proxies and does not run the Windows
/// tunnel-client executable or a local HTTP server.
const ANDROID_TUNNEL_ROUTE: &str = "api.openai.com:443 · direct";

#[derive(Clone, Copy)]
struct TunnelEvent {
    timestamp_ms: u64,
    kind: &'static str,
    detail: &'static str,
}

/// Clock failures are unknown, never a fabricated Unix epoch timestamp.
fn wallclock_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
}

fn append_tunnel_event(session: &mut Session, kind: &'static str, detail: &'static str) {
    let Some(timestamp_ms) = wallclock_ms() else {
        return;
    };
    if session.events.len() == MAX_TELEMETRY_EVENTS {
        session.events.pop_front();
    }
    session.events.push_back(TunnelEvent {
        timestamp_ms,
        kind,
        detail,
    });
}

/// Files live under the app-controlled private data directory. Historical
/// proof persists just the public tunnel ID, never a key, RPC, or tool count.
#[derive(Clone)]
struct CorePluginProofPaths {
    saved_id: Arc<std::path::PathBuf>,
    desired: Arc<std::path::PathBuf>,
    pairing: Arc<std::path::PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Disconnected,
    Connecting,
    Polling,
    ToolVerified,
    Error,
}

#[derive(Clone, Copy, Debug)]
enum ConnectAuthority {
    /// A pasted key remains strictly process-scoped, even if another key is
    /// saved in Keystore; never restore with a potentially different secret.
    Ephemeral,
    /// The user tapped Connect and the exact Keystore key was loaded.
    StoredUser { generation: u64 },
    /// Android startup found a prior explicit persisted Connect decision.
    PersistedStartup { generation: u64 },
}

/// App-local Plugins RPC handler. It is invoked only for authenticated inbound
/// calls on the shared Core tunnel; native aliases and arguments are checked
/// independently by the plugin registry before execution.
pub(crate) type PluginRpcHandler = Arc<dyn Fn(&Value, Instant) -> RpcReply + Send + Sync>;

impl Default for Phase {
    fn default() -> Self {
        Self::Disconnected
    }
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Polling => "polling",
            Self::ToolVerified => "tool_verified",
            Self::Error => "error",
        }
    }
}

#[derive(Default)]
struct Session {
    generation: u64,
    phase: Phase,
    active_tunnel_id: Option<String>,
    last_poll: Option<Instant>,
    verified_calls: u64,
    last_error: Option<&'static str>,
    worker: Option<JoinHandle<()>>,
    restore_started: bool,
    started_at: Option<Instant>,
    verified_link_at_ms: Option<u64>,
    last_chatgpt_call_at_ms: Option<u64>,
    poll_errors: u64,
    /// Session-only operational metadata. Never persisted or populated from
    /// arbitrary RPC text, keys, paths, or user prompts.
    events: VecDeque<TunnelEvent>,
}

/// Register once with a shared process-memory AndroidMcpPermissions gate.
#[derive(Clone, Default)]
pub struct AndroidOpenAiTunnel {
    session: Arc<Mutex<Session>>,
    permissions: AndroidMcpPermissions,
    plugins_handler: Arc<Mutex<Option<PluginRpcHandler>>>,
    // Configured only by the app's private data directory on Android.
    // This stores an explicit opt-in ID, never a credential or verification.
    desired_path: Option<Arc<std::path::PathBuf>>,
    saved_id_path: Option<Arc<std::path::PathBuf>>,
    pairing_path: Option<Arc<std::path::PathBuf>>,
}

impl AndroidOpenAiTunnel {
    fn reset_session(session: &mut Session) {
        session.generation = session.generation.wrapping_add(1);
        if let Some(worker) = session.worker.take() {
            // Aborting also cancels an in-flight long poll or response POST.
            worker.abort();
        }
        session.phase = Phase::Disconnected;
        session.active_tunnel_id = None;
        session.last_poll = None;
        session.verified_calls = 0;
        session.last_error = None;
        session.started_at = None;
        session.verified_link_at_ms = None;
        session.last_chatgpt_call_at_ms = None;
        session.poll_errors = 0;
        // Preserve bounded process-local activity across a manual disconnect;
        // a new Android process starts with a fresh empty event list.
    }

    pub fn with_permissions(permissions: AndroidMcpPermissions) -> Self {
        Self {
            session: Arc::new(Mutex::new(Session::default())),
            permissions,
            plugins_handler: Arc::new(Mutex::new(None)),
            desired_path: None,
            saved_id_path: None,
            pairing_path: None,
        }
    }

    /// The calling Tauri setup owns `data_dir`; no JS input may choose this
    /// path. Tests and non-Android clients can still use `with_permissions`.
    pub fn with_permissions_and_data_dir(
        permissions: AndroidMcpPermissions,
        data_dir: std::path::PathBuf,
    ) -> Self {
        let mut result = Self::with_permissions(permissions);
        result.desired_path = Some(Arc::new(data_dir.join(DESIRED_TUNNEL_FILENAME)));
        result.saved_id_path = Some(Arc::new(data_dir.join(CORE_TUNNEL_FILENAME)));
        result.pairing_path = Some(Arc::new(data_dir.join(CORE_PLUGIN_PAIRING_FILENAME)));
        result
    }

    /// Register the native Plugins registry before restoring or connecting
    /// Core. There is no second polling worker, tunnel ID or credential.
    pub(crate) fn attach_plugins_handler(&self, handler: PluginRpcHandler) -> Result<(),String> {
        let mut current=self.plugins_handler.lock()
            .map_err(|_|"Shared Core Plugins dispatcher unavailable")?;
        if current.is_some() {return Err("Shared Core Plugins dispatcher already registered".into())}
        *current=Some(handler);
        Ok(())
    }

    fn proof_paths(&self) -> Option<CorePluginProofPaths> {
        Some(CorePluginProofPaths {
            saved_id: self.saved_id_path.clone()?,
            desired: self.desired_path.clone()?,
            pairing: self.pairing_path.clone()?,
        })
    }

    pub(crate) fn snapshot(&self) -> Result<Value, String> {
        let session = self
            .session
            .lock()
            .map_err(|_| "Tunnel state unavailable")?;
        let desired_connected = self
            .desired_path
            .as_deref()
            .zip(self.saved_id_path.as_deref())
            .and_then(|(desired, saved)| restorable_tunnel_id(desired, saved))
            .is_some();
        let recently_polled = session
            .last_poll
            .is_some_and(|time| time.elapsed() < HEARTBEAT_LIMIT);
        let healthy =
            recently_polled && matches!(session.phase, Phase::Polling | Phase::ToolVerified);
        let stale = matches!(session.phase, Phase::Polling | Phase::ToolVerified) && !healthy;
        let state = if stale {
            "error"
        } else {
            session.phase.label()
        };
        // Previous remote ACK is historical evidence only. It counts as a
        // linked Core Plugin only while THIS process polls a matching,
        // explicitly opted-in, still-valid Core ID. Neither a marker nor a
        // reconnect can fabricate ToolVerified or live tool-call counters.
        let core_plugin_linked = healthy
            && session.active_tunnel_id.as_deref().is_some_and(|active| {
                self.proof_paths().is_some_and(|paths| {
                    restorable_tunnel_id(&paths.desired, &paths.saved_id).as_deref() == Some(active)
                        && read_confirmed_core_id(&paths.pairing).as_deref() == Some(active)
                })
            });
        let runtime_live = session.phase != Phase::Disconnected
            && session
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished());
        let uptime_seconds = session
            .started_at
            .filter(|_| runtime_live)
            .map(|started| started.elapsed().as_secs());
        let activity: Vec<Value> = session
            .events
            .iter()
            .rev()
            .map(|event| {
                json!({
                    "timestampMs": event.timestamp_ms,
                    "kind": event.kind,
                    "detail": event.detail,
                })
            })
            .collect();
        let mut result = json!({
            "state": state,
            "pollActive": healthy,
            "toolCallsVerified": session.verified_calls,
            "corePluginLinked": core_plugin_linked,
            // This is ONLY the user's durable opt-in preference, never a
            // network health or historical verification claim. Expose it so
            // an Error state still offers explicit Disconnect/opt-out.
            "desiredConnected": desired_connected,
            "health": {
                "verifiedLinkAtMs": session.verified_link_at_ms,
                "lastChatgptCallAtMs": session.last_chatgpt_call_at_ms,
                "pollErrors": session.poll_errors,
                "uptimeSeconds": uptime_seconds,
                "route": if runtime_live { Some(ANDROID_TUNNEL_ROUTE) } else { None },
                "tunnelClient": env!("CARGO_PKG_VERSION"),
                "localServer": "in-process (Android)",
                // There is no separate Windows tunnel-client loopback probe;
                // a successful native poll is already represented by pollActive.
                "probe": null,
            },
            "events": activity,
        });
        if stale {
            result["lastError"] = json!("Poll heartbeat expired");
        } else if let Some(error) = session.last_error {
            result["lastError"] = json!(error);
        }
        Ok(result)
    }

    pub(crate) fn confirmed_id(&self) -> Option<String> {
        self.saved_id_path.as_deref().and_then(|p| read_confirmed_core_id(p))
    }

    pub(crate) fn disconnect(&self) -> Result<Value, String> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| "Tunnel state unavailable")?;
        // Persist explicit opt-out BEFORE worker cancellation. If the Android
        // process dies immediately after Disconnect, an older opt-in must not
        // revive. Even on a storage failure, invalidate this live generation.
        let durable_opt_out = self
            .desired_path
            .as_deref()
            .map(|path| store_desired_tunnel(path, None))
            .transpose();
        let proof_opt_out = self
            .pairing_path
            .as_deref()
            .map(|path| clear_core_plugin_pairing(path))
            .transpose();
        Self::reset_session(&mut session);
        append_tunnel_event(&mut session, "tunnel", "Tunnel disconnected");
        drop(session);
        durable_opt_out?;
        proof_opt_out?;
        self.snapshot()
    }

    pub(crate) fn connect(&self, tunnel_id: String, api_key: String) -> Result<Value, String> {
        self.connect_internal(tunnel_id, api_key, ConnectAuthority::Ephemeral)
    }

    fn connect_internal(
        &self,
        tunnel_id: String,
        api_key: String,
        authority: ConnectAuthority,
    ) -> Result<Value, String> {
        if !valid_tunnel_id(&tunnel_id) {
            return Err("Invalid OpenAI tunnel ID".into());
        }
        if !valid_api_key(&api_key) {
            return Err("Invalid tunnel runtime key".into());
        }
        let client = Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(22))
            .pool_max_idle_per_host(1)
            .build()
            .map_err(|_| "Tunnel HTTP client unavailable")?;
        let instance_id = uuid::Uuid::new_v4().to_string();
        let mut session = self
            .session
            .lock()
            .map_err(|_| "Tunnel state unavailable")?;
        let expected_generation = match authority {
            ConnectAuthority::Ephemeral => None,
            ConnectAuthority::StoredUser { generation }
            | ConnectAuthority::PersistedStartup { generation } => Some(generation),
        };
        if expected_generation.is_some_and(|g| g != session.generation) {
            return Err("Tunnel connection request was superseded".into());
        }
        if session.phase != Phase::Disconnected {
            return Err("Disconnect the current tunnel before connecting again".into());
        }
        if !matches!(authority, ConnectAuthority::Ephemeral) {
            let saved_id = self
                .saved_id_path
                .as_deref()
                .and_then(|path| read_confirmed_core_id(path));
            if saved_id.as_deref() != Some(&tunnel_id) {
                return Err("Save this Core tunnel ID before persistent connection".into());
            }
            let desired_path = self
                .desired_path
                .as_deref()
                .ok_or("Persistent Android tunnel storage unavailable")?;
            match authority {
                ConnectAuthority::StoredUser { .. } => {
                    // A new user connection after opt-out, an ID change or a
                    // one-off connection needs a NEW remote ACK; an old marker
                    // must not be resurrected by opting in again.
                    if read_desired_tunnel(desired_path).as_deref() != Some(&tunnel_id) {
                        if let Some(pairing) = self.pairing_path.as_deref() {
                            clear_core_plugin_pairing(pairing)?;
                        }
                    }
                    // User-confirmed secure credentials only; this does not
                    // claim a remote poll or an executed MCP tool.
                    store_desired_tunnel(desired_path, Some(&tunnel_id))?;
                }
                ConnectAuthority::PersistedStartup { .. } => {
                    // Restore never writes a new authorization record.
                    if read_desired_tunnel(desired_path).as_deref() != Some(&tunnel_id) {
                        return Err("Persistent tunnel connection was revoked".into());
                    }
                }
                ConnectAuthority::Ephemeral => unreachable!(),
            }
        } else if let Some(path) = self.desired_path.as_deref() {
            // A one-off plaintext-key connection must never accidentally
            // restore later with a DIFFERENT Keystore credential.
            store_desired_tunnel(path, None)?;
            if let Some(pairing) = self.pairing_path.as_deref() {
                clear_core_plugin_pairing(pairing)?;
            }
        }
        session.generation = session.generation.wrapping_add(1);
        let generation = session.generation;
        session.phase = Phase::Connecting;
        session.active_tunnel_id = Some(tunnel_id.clone());
        session.last_poll = None;
        session.verified_calls = 0;
        session.last_error = None;
        session.started_at = Some(Instant::now());
        session.verified_link_at_ms = None;
        session.last_chatgpt_call_at_ms = None;
        session.poll_errors = 0;
        append_tunnel_event(&mut session, "tunnel", "Tunnel connection requested");
        let state = self.session.clone();
        let permissions = self.permissions.clone();
        let plugins_handler = self.plugins_handler.lock()
            .map_err(|_|"Shared Core Plugins dispatcher unavailable")?.clone();
        let proof_paths = self.proof_paths();
        session.worker = Some(tokio::spawn(async move {
            run_poll_loop(
                state,
                permissions,
                plugins_handler,
                proof_paths,
                generation,
                client,
                tunnel_id,
                api_key,
                instance_id,
            )
            .await;
        }));
        drop(session);
        self.snapshot()
    }

    pub(crate) fn connect_stored_user(
        &self,
        tunnel_id: String,
        api_key: String,
        expected_generation: u64,
    ) -> Result<Value, String> {
        self.connect_internal(
            tunnel_id,
            api_key,
            ConnectAuthority::StoredUser {
                generation: expected_generation,
            },
        )
    }

    fn connect_restored(
        &self,
        tunnel_id: String,
        api_key: String,
        expected_generation: u64,
    ) -> Result<Value, String> {
        self.connect_internal(
            tunnel_id,
            api_key,
            ConnectAuthority::PersistedStartup {
                generation: expected_generation,
            },
        )
    }

    fn restoration_generation(&self) -> Option<u64> {
        let mut session = self.session.lock().ok()?;
        if session.restore_started {
            return None;
        }
        session.restore_started = true;
        Some(session.generation)
    }

    pub(crate) fn current_generation(&self) -> Result<u64, String> {
        self.session
            .lock()
            .map(|session| session.generation)
            .map_err(|_| "Tunnel state unavailable".to_owned())
    }

    /// Private identity of the *currently polling* shared Core tunnel, not
    /// merely a saved ID. Session grants bind both this ID and generation:
    /// reconnect, manual Disconnect and runtime restart invalidate them.
    pub(crate) fn current_live_identity(&self) -> Result<Option<(String,u64)>,String> {
        let session=self.session.lock().map_err(|_|"Tunnel state unavailable")?;
        if !matches!(session.phase,Phase::Polling|Phase::ToolVerified)
            || session.last_poll.is_none_or(|time|time.elapsed()>=HEARTBEAT_LIMIT)
            || session.worker.as_ref().is_none_or(|worker|worker.is_finished())
        {return Ok(None)}
        Ok(session.active_tunnel_id.as_ref().map(|id|(id.clone(),session.generation)))
    }
}

/// Called once from Tauri setup after the credential plugin and tunnel state
/// have been registered. A previous explicit Connect is the only authority to
/// reconnect; successful saved ID/key setup alone is not enough. Android may
/// suspend or kill the app at any time, so reconnect resumes on process start.
pub(crate) fn restore_on_startup(app: AppHandle, tunnel: AndroidOpenAiTunnel) {
    if !cfg!(target_os = "android") {
        return;
    }
    let Some(generation) = tunnel.restoration_generation() else {
        return;
    };
    let Some(id) = tunnel
        .desired_path
        .as_deref()
        .zip(tunnel.saved_id_path.as_deref())
        .and_then(|(desired, saved)| restorable_tunnel_id(desired, saved))
    else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        let key = tauri::async_runtime::spawn_blocking(move || {
            crate::android_tunnel_credentials::load_for_connect(app)
        })
        .await;
        if let Ok(Ok(key)) = key {
            // A simultaneous explicit Disconnect, ID change, or manual
            // Connect invalidates the captured generation and fails closed.
            // No credential, durable proof, or error is emitted to the UI.
            let _ = tunnel.connect_restored(id, key, generation);
        }
    });
}

/// This command returns only connection health; it never returns credentials.
#[tauri::command]
pub async fn android_openai_tunnel_status(
    state: tauri::State<'_, AndroidOpenAiTunnel>,
) -> Result<Value, String> {
    state.snapshot()
}

/// A user gesture is required. Credentials live only inside the cancellable task.
#[tauri::command]
pub async fn android_openai_tunnel_connect(
    state: tauri::State<'_, AndroidOpenAiTunnel>,
    tunnel_id: String,
    api_key: String,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated {
        return Err("Explicit user action required to connect tunnel".into());
    }
    if !cfg!(target_os = "android") {
        return Err("OpenAI Secure MCP Tunnel is available on Android only".into());
    }
    state.connect(tunnel_id, api_key)
}

/// Reuses a Keystore-decrypted key only inside Rust. No plaintext is returned
/// to the renderer, including when the Keystore credential is missing/invalid.
#[tauri::command]
pub async fn android_openai_tunnel_connect_stored(
    app: tauri::AppHandle,
    state: tauri::State<'_, AndroidOpenAiTunnel>,
    tunnel_id: String,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated {
        return Err("Explicit user action required to connect tunnel".into());
    }
    if !cfg!(target_os = "android") {
        return Err("OpenAI Secure MCP Tunnel is available on Android only".into());
    }
    if !valid_tunnel_id(&tunnel_id) {
        return Err("Invalid OpenAI tunnel ID".into());
    }
    // Protect an explicit Disconnect while the KeyStore read is in flight.
    let generation = state.current_generation()?;
    let api_key = tauri::async_runtime::spawn_blocking(move || {
        crate::android_tunnel_credentials::load_for_connect(app)
    })
    .await
    .map_err(|_| "Android tunnel key unavailable".to_owned())??;
    // The exact same heartbeat/real tool-call verification path as a pasted
    // ephemeral credential. The stored key cannot mark Verified on its own.
    state.connect_stored_user(tunnel_id, api_key, generation)
}

/// A syntactic check only, never ownership or permission verification.
#[tauri::command]
pub async fn android_tunnel_check_id(tunnel_id: String) -> Result<Value, String> {
    Ok(json!({"valid":valid_tunnel_id(&tunnel_id),"formatOnly":true}))
}

fn core_tunnel_id_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|_| "Private Android app storage unavailable")?;
    fs::create_dir_all(&dir).map_err(|_| "Private Android app storage unavailable")?;
    Ok(dir.join(CORE_TUNNEL_FILENAME))
}

pub(crate) fn store_confirmed_core_id(path: &Path, tunnel_id: &str) -> Result<(), String> {
    if !valid_tunnel_id(tunnel_id) {
        return Err("Invalid Core tunnel ID".into());
    }
    let data = serde_json::to_vec(&json!({"tunnelId":tunnel_id}))
        .map_err(|_| "Could not encode Core tunnel ID")?;
    // The saved identifier is a draft, never a persisted authentication proof.
    // A unique temporary file in the same app-private directory is renamed
    // over the previous ID atomically on Android; keys are stored separately.
    let tmp = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4()));
    let save = (|| -> Result<(), String> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|_| "Could not save Core tunnel ID")?;
        file.write_all(&data)
            .map_err(|_| "Could not save Core tunnel ID")?;
        file.sync_all()
            .map_err(|_| "Could not save Core tunnel ID")?;
        drop(file);
        if fs::rename(&tmp, path).is_err() {
            // Windows rename cannot replace an existing destination; Android can.
            #[cfg(windows)]
            {
                fs::remove_file(path).map_err(|_| "Could not save Core tunnel ID")?;
                fs::rename(&tmp, path).map_err(|_| "Could not save Core tunnel ID")?;
            }
            #[cfg(not(windows))]
            {
                return Err("Could not save Core tunnel ID".into());
            }
        }
        Ok(())
    })();
    if save.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    save
}

pub(crate) fn read_confirmed_core_id(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    if data.len() > 160 {
        return None;
    }
    let value: Value = serde_json::from_slice(&data).ok()?;
    if value.as_object()?.len() != 1 {
        return None;
    }
    let id = value.get("tunnelId")?.as_str()?;
    valid_tunnel_id(id).then(|| id.to_string())
}

/// A successful, explicitly permitted tools/call POST ACK may store only a
/// single validated nonsecret ID. No response payload, API key, session count,
/// or historical phase is ever serialized.
fn store_core_plugin_pairing(path: &Path, tunnel_id: &str) -> Result<(), String> {
    store_confirmed_core_id(path, tunnel_id)
}

fn clear_core_plugin_pairing(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => {
            #[cfg(target_os = "android")]
            if let Some(parent) = path.parent() {
                let _ = fs::File::open(parent).and_then(|directory| directory.sync_all());
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("Could not clear Core Plugin pairing history".into()),
    }
}

/// Crash-safe intent record in app-private storage. The only persisted data
/// are a validated public tunnel ID and the user's preference; neither an API
/// key nor a historical connected/tool-verification state is serialized.
fn store_desired_tunnel(path: &Path, tunnel_id: Option<&str>) -> Result<(), String> {
    if tunnel_id.is_some_and(|id| !valid_tunnel_id(id)) {
        return Err("Invalid persisted tunnel ID".into());
    }
    let record = match tunnel_id {
        Some(id) => json!({"version":1,"desiredConnected":true,"tunnelId":id}),
        None => json!({"version":1,"desiredConnected":false}),
    };
    let encoded =
        serde_json::to_vec(&record).map_err(|_| "Unable to encode tunnel connection preference")?;
    let tmp = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4()));
    let save = (|| -> Result<(), String> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|_| "Unable to save tunnel connection preference")?;
        file.write_all(&encoded)
            .map_err(|_| "Unable to save tunnel connection preference")?;
        file.sync_all()
            .map_err(|_| "Unable to save tunnel connection preference")?;
        drop(file);
        if fs::rename(&tmp, path).is_err() {
            #[cfg(windows)]
            {
                // Windows testing has no rename-replace. Android replaces
                // atomically without a gap between old and new preferences.
                if path.exists() {
                    fs::remove_file(path)
                        .map_err(|_| "Unable to save tunnel connection preference")?;
                }
                fs::rename(&tmp, path)
                    .map_err(|_| "Unable to save tunnel connection preference")?;
            }
            #[cfg(not(windows))]
            {
                return Err("Unable to save tunnel connection preference".into());
            }
        }
        #[cfg(target_os = "android")]
        if let Some(parent) = path.parent() {
            // Some filesystems reject directory fsync even after a successful
            // atomic rename. Do not report Connect as rejected *after* its
            // preference was committed; the file itself was already synced.
            let _ = fs::File::open(parent).and_then(|directory| directory.sync_all());
        }
        Ok(())
    })();
    if save.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    save
}

fn read_desired_tunnel(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    if data.len() > 160 {
        return None;
    }
    let value: Value = serde_json::from_slice(&data).ok()?;
    let fields = value.as_object()?;
    if fields.len() != 3
        || fields.get("version") != Some(&json!(1))
        || fields.get("desiredConnected") != Some(&Value::Bool(true))
    {
        return None;
    }
    let id = fields.get("tunnelId")?.as_str()?;
    valid_tunnel_id(id).then(|| id.to_owned())
}

/// Reads only explicit previously persisted Connect intent and a matching
/// validated saved Core ID. Mere ID/key storage never authorizes reconnect.
fn restorable_tunnel_id(desired_path: &Path, saved_id_path: &Path) -> Option<String> {
    let desired = read_desired_tunnel(desired_path)?;
    (read_confirmed_core_id(saved_id_path).as_deref() == Some(&desired)).then_some(desired)
}

fn saved_core_id_status(path: &Path) -> Value {
    json!({"tunnelId":read_confirmed_core_id(path),"verified":false})
}

pub(crate) fn save_draft_core_id(
    path: &Path,
    tunnel_id: &str,
    user_initiated: bool,
    state: &AndroidOpenAiTunnel,
) -> Result<Value, String> {
    if !user_initiated {
        return Err("Explicit Save action required".into());
    }
    if !valid_tunnel_id(tunnel_id) {
        return Err("Invalid Core tunnel ID".into());
    }
    // Serialize the save with connect/disconnect. If the newly saved ID
    // differs from the live tunnel, cancel its worker and invalidate proof.
    let mut session = state
        .session
        .lock()
        .map_err(|_| "Tunnel state unavailable")?;
    if read_confirmed_core_id(path).as_deref() != Some(tunnel_id) {
        // Invalidate historical ACK before committing a different Core ID.
        // The same mutex serializes this against any late response ACK.
        if let Some(pairing) = state.pairing_path.as_deref() {
            clear_core_plugin_pairing(pairing)?;
        }
    }
    if let Some(desired_path) = state.desired_path.as_deref() {
        if read_desired_tunnel(desired_path).is_some_and(|old| old != tunnel_id) {
            // Changing an ID is not permission to reconnect to its old tunnel.
            store_desired_tunnel(desired_path, None)?;
        }
    }
    store_confirmed_core_id(path, tunnel_id)?;
    if session.phase != Phase::Disconnected
        && session.active_tunnel_id.as_deref() != Some(tunnel_id)
    {
        AndroidOpenAiTunnel::reset_session(&mut session);
    }
    Ok(json!({"saved":true,"verified":false,"tunnelId":tunnel_id}))
}

/// Save a user-entered Core Tunnel ID as an unverified local draft. This
/// command does not require a runtime key or make any network request.
#[tauri::command]
pub async fn android_tunnel_save_id(
    app: AppHandle,
    state: tauri::State<'_, AndroidOpenAiTunnel>,
    tunnel_id: String,
    user_initiated: bool,
) -> Result<Value, String> {
    if !cfg!(target_os = "android") {
        return Err("Core tunnel ID storage is available on Android only".into());
    }
    let path = core_tunnel_id_path(&app)?;
    save_draft_core_id(&path, &tunnel_id, user_initiated, &state)
}

fn tunnel_metadata_result(status: StatusCode, body: &[u8], id: &str) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "unauthorized",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::OK => {
            if body.len() > TUNNEL_METADATA_LIMIT {
                return "invalid_metadata";
            }
            let Ok(value) = serde_json::from_slice::<Value>(body) else {
                return "invalid_metadata";
            };
            let Some(record) = value.as_object() else {
                return "invalid_metadata";
            };
            if record.get("id").and_then(Value::as_str) == Some(id) {
                "ok"
            } else {
                "invalid_metadata"
            }
        }
        StatusCode::TOO_MANY_REQUESTS => "network_error",
        code if code.is_server_error() => "network_error",
        _ => "service_error",
    }
}

pub(crate) async fn verify_tunnel_metadata(
    client: &Client,
    endpoint: &str,
    tunnel_id: &str,
    api_key: &str,
) -> &'static str {
    let result = async {
        let mut response = client
            .get(endpoint)
            .bearer_auth(api_key)
            .header("X-Tunnel-Client-Name", "mcp-agent-ai-android")
            .header("X-Tunnel-Client-Version", env!("CARGO_PKG_VERSION"))
            .send()
            .await
            .map_err(|_| "network_error")?;
        let status = response.status();
        if status != StatusCode::OK {
            return Ok::<&'static str, &'static str>(tunnel_metadata_result(
                status,
                &[],
                tunnel_id,
            ));
        }
        // Bounded to 4 KiB regardless of Content-Length. Never expose the
        // OpenAI raw response or any Keystore credential in a Tauri receipt.
        let mut body = Vec::with_capacity(512);
        while let Some(chunk) = response.chunk().await.map_err(|_| "network_error")? {
            if body.len().saturating_add(chunk.len()) > TUNNEL_METADATA_LIMIT {
                return Ok("invalid_metadata");
            }
            body.extend_from_slice(&chunk);
        }
        Ok(tunnel_metadata_result(status, &body, tunnel_id))
    }
    .await;
    result.unwrap_or("network_error")
}

pub(crate) fn tunnel_metadata_client() -> Result<Client, String> {
    Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|_| "Core tunnel HTTPS verification unavailable".to_owned())
}

fn stored_access_receipt(tunnel_id: Option<&str>, reason: &'static str) -> Value {
    json!({"verified":reason == "ok","tunnelId":tunnel_id,"reason":reason})
}

#[cfg(test)]
mod tunnel_id_verification_tests {
    use super::*;

    #[test]
    fn exact_id_and_authorized_metadata_are_required() {
        let id = "tunnel_0123456789abcdef0123456789abcdef";
        assert_eq!(
            tunnel_metadata_result(
                StatusCode::OK,
                br#"{"id":"tunnel_0123456789abcdef0123456789abcdef","name":"Core"}"#,
                id
            ),
            "ok"
        );
        for body in [
            br#"{"id":"tunnel_ffffffffffffffffffffffffffffffff"}"#.as_slice(),
            br#"{"tunnel":{"id":"tunnel_0123456789abcdef0123456789abcdef"}}"#.as_slice(),
            br#"{"name":"No id"}"#.as_slice(),
            br#"not JSON"#.as_slice(),
            b"".as_slice(),
        ] {
            assert_eq!(
                tunnel_metadata_result(StatusCode::OK, body, id),
                "invalid_metadata"
            );
        }
        assert_eq!(
            tunnel_metadata_result(StatusCode::OK, &vec![b'X'; TUNNEL_METADATA_LIMIT + 1], id),
            "invalid_metadata"
        );
    }

    #[test]
    fn authentication_and_service_errors_are_not_accepted_as_verified() {
        let id = "tunnel_0123456789abcdef0123456789abcdef";
        for (status, expected) in [
            (StatusCode::UNAUTHORIZED, "unauthorized"),
            (StatusCode::FORBIDDEN, "unauthorized"),
            (StatusCode::NOT_FOUND, "not_found"),
            (StatusCode::TOO_MANY_REQUESTS, "network_error"),
            (StatusCode::INTERNAL_SERVER_ERROR, "network_error"),
            (StatusCode::BAD_REQUEST, "service_error"),
        ] {
            assert_eq!(
                tunnel_metadata_result(
                    status,
                    br#"{"id":"tunnel_0123456789abcdef0123456789abcdef"}"#,
                    id
                ),
                expected
            );
        }
    }

    #[test]
    fn saved_core_id_is_only_an_untrusted_draft_after_restart() {
        let file = std::env::temp_dir().join(format!("tunnel-id-{}.json", uuid::Uuid::new_v4()));
        let id = "tunnel_0123456789abcdef0123456789abcdef";
        assert_eq!(
            saved_core_id_status(&file),
            json!({"tunnelId":null,"verified":false})
        );
        store_confirmed_core_id(&file, id).unwrap();
        assert_eq!(
            saved_core_id_status(&file),
            json!({"tunnelId":id,"verified":false})
        );
        assert!(store_confirmed_core_id(&file, "tunnel_not_valid").is_err());
        fs::write(
            &file,
            br#"{"tunnelId":"tunnel_0123456789abcdef0123456789abcdef","verified":true}"#,
        )
        .unwrap();
        assert_eq!(
            saved_core_id_status(&file),
            json!({"tunnelId":null,"verified":false})
        );
        fs::write(&file, b"{").unwrap();
        assert_eq!(
            saved_core_id_status(&file),
            json!({"tunnelId":null,"verified":false})
        );
        let _ = fs::remove_file(file);
    }

    #[test]
    fn saving_core_id_without_key_has_exact_unverified_schema() {
        let dir = std::env::temp_dir().join(format!("tunnel-save-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join(CORE_TUNNEL_FILENAME);
        let id = "tunnel_0123456789abcdef0123456789abcdef";
        let tunnel = AndroidOpenAiTunnel::default();
        // A fresh tunnel has neither a runtime connection nor any API key.
        assert_eq!(
            save_draft_core_id(&path, id, true, &tunnel).unwrap(),
            json!({"saved":true,"verified":false,"tunnelId":id})
        );
        assert_eq!(
            saved_core_id_status(&path),
            json!({"tunnelId":id,"verified":false})
        );
        assert_eq!(tunnel.snapshot().unwrap()["toolCallsVerified"], 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_rejects_missing_user_gesture_invalid_id_and_failed_persistence() {
        let dir = std::env::temp_dir().join(format!("tunnel-save-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join(CORE_TUNNEL_FILENAME);
        let id = "tunnel_0123456789abcdef0123456789abcdef";
        let tunnel = AndroidOpenAiTunnel::default();
        assert!(save_draft_core_id(&path, id, false, &tunnel).is_err());
        for invalid in [
            "",
            "tunnel_short",
            "tunnel_ABCDEF0123456789abcdef0123456789ab",
            "tunnel_0123456789abcdef0123456789abcdef/../",
            "tunnel_0123456789abcdef0123456789abcdef\r\nX:1",
        ] {
            assert!(save_draft_core_id(&path, invalid, true, &tunnel).is_err());
        }
        assert_eq!(
            saved_core_id_status(&path),
            json!({"tunnelId":null,"verified":false})
        );
        let missing_parent = dir.join("does_not_exist").join("id.json");
        assert!(save_draft_core_id(&missing_parent, id, true, &tunnel).is_err());
        assert!(!missing_parent.exists());
        assert_eq!(tunnel.snapshot().unwrap()["state"], "disconnected");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn changing_saved_id_invalidates_another_active_tunnels_tool_proof() {
        let dir = std::env::temp_dir().join(format!("tunnel-save-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join(CORE_TUNNEL_FILENAME);
        let first = "tunnel_0123456789abcdef0123456789abcdef";
        let second = "tunnel_ffffffffffffffffffffffffffffffff";
        let tunnel = AndroidOpenAiTunnel::default();
        {
            let mut session = tunnel.session.lock().unwrap();
            session.generation = 7;
            session.active_tunnel_id = Some(first.into());
            session.phase = Phase::ToolVerified;
            session.last_poll = Some(Instant::now());
            session.verified_calls = 3;
        }
        assert_eq!(tunnel.snapshot().unwrap()["state"], "tool_verified");
        assert_eq!(
            save_draft_core_id(&path, second, true, &tunnel).unwrap(),
            json!({"saved":true,"verified":false,"tunnelId":second})
        );
        let snapshot = tunnel.snapshot().unwrap();
        assert_eq!(snapshot["state"], "disconnected");
        assert_eq!(snapshot["toolCallsVerified"], 0);
        assert_eq!(tunnel.session.lock().unwrap().generation, 8);
        assert_eq!(
            saved_core_id_status(&path),
            json!({"tunnelId":second,"verified":false})
        );
        let _ = fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod persisted_connection_tests {
    use super::*;

    const FIRST: &str = "tunnel_0123456789abcdef0123456789abcdef";
    const SECOND: &str = "tunnel_ffffffffffffffffffffffffffffffff";
    const KEY: &str = "sk-in-memory-only-for-tests";

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mcp-desired-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        dir
    }

    fn with_directory(dir: &Path) -> AndroidOpenAiTunnel {
        AndroidOpenAiTunnel::with_permissions_and_data_dir(
            AndroidMcpPermissions::default(),
            dir.to_path_buf(),
        )
    }

    fn arm_healthy_poll(tunnel: &AndroidOpenAiTunnel, id: &str, generation: u64) {
        let mut current = tunnel.session.lock().unwrap();
        current.generation = generation;
        current.phase = Phase::Polling;
        current.active_tunnel_id = Some(id.to_owned());
        current.last_poll = Some(Instant::now());
        current.verified_calls = 0;
    }

    fn settle_ack(
        tunnel: &AndroidOpenAiTunnel,
        generation: u64,
        permitted: bool,
        result: Result<(), TunnelFailure>,
    ) {
        let mut failure = None;
        let paths = tunnel.proof_paths();
        settle_batch_result(
            &tunnel.session,
            generation,
            paths.as_ref(),
            Ok((permitted, result)),
            &mut failure,
        );
    }

    #[test]
    fn saved_id_and_keystore_alone_never_imply_prior_connection_consent() {
        let dir = temp_dir();
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let intent = dir.join(DESIRED_TUNNEL_FILENAME);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        assert_eq!(restorable_tunnel_id(&intent, &saved), None);
        assert_eq!(read_desired_tunnel(&intent), None);

        store_desired_tunnel(&intent, Some(FIRST)).unwrap();
        assert_eq!(restorable_tunnel_id(&intent, &saved), Some(FIRST.into()));
        store_desired_tunnel(&intent, None).unwrap();
        assert_eq!(restorable_tunnel_id(&intent, &saved), None);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&intent).unwrap()).unwrap(),
            json!({"version":1,"desiredConnected":false})
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn desired_connected_is_only_durable_user_intent_even_during_runtime_error() {
        let dir = temp_dir();
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let intent = dir.join(DESIRED_TUNNEL_FILENAME);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        let tunnel = with_directory(&dir);
        assert_eq!(tunnel.snapshot().unwrap()["desiredConnected"], false);
        store_desired_tunnel(&intent, Some(FIRST)).unwrap();
        let snapshot = tunnel.snapshot().unwrap();
        assert_eq!(snapshot["desiredConnected"], true);
        assert_eq!(snapshot["state"], "disconnected");
        assert_eq!(snapshot["pollActive"], false);
        assert_eq!(snapshot["toolCallsVerified"], 0);
        {
            let mut session = tunnel.session.lock().unwrap();
            session.phase = Phase::Error;
            session.last_error = Some("Tunnel network request failed");
        }
        let snapshot = tunnel.snapshot().unwrap();
        assert_eq!(snapshot["desiredConnected"], true);
        assert_eq!(snapshot["state"], "error");
        assert_eq!(snapshot["pollActive"], false);
        assert_eq!(snapshot["toolCallsVerified"], 0);
        tunnel.disconnect().unwrap();
        let snapshot = tunnel.snapshot().unwrap();
        assert_eq!(snapshot["desiredConnected"], false);
        assert_eq!(snapshot["state"], "disconnected");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_or_mismatched_persisted_intent_fails_closed_without_tool_proof() {
        let dir = temp_dir();
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let intent = dir.join(DESIRED_TUNNEL_FILENAME);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        for bogus in [
            json!({"version":1,"desiredConnected":true}),
            json!({"version":1,"desiredConnected":true,"tunnelId":SECOND}),
            json!({"version":1,"desiredConnected":true,"tunnelId":FIRST,
                "toolCallsVerified":1}),
            json!({"version":1,"desiredConnected":true,"tunnelId":FIRST,
                "apiKey":"sk-leak"}),
            json!({"version":1,"desiredConnected":"true","tunnelId":FIRST}),
            json!({"version":0,"desiredConnected":true,"tunnelId":FIRST}),
            json!({"version":1,"desiredConnected":true,"tunnelId":"../poll"}),
            json!([FIRST]),
        ] {
            fs::write(&intent, serde_json::to_vec(&bogus).unwrap()).unwrap();
            assert_eq!(restorable_tunnel_id(&intent, &saved), None, "{bogus}");
        }
        fs::write(&intent, b"{").unwrap();
        assert_eq!(restorable_tunnel_id(&intent, &saved), None);
        fs::write(&intent, vec![b'x'; 161]).unwrap();
        assert_eq!(restorable_tunnel_id(&intent, &saved), None);
        assert!(store_desired_tunnel(&intent, Some("evil-host/path")).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn explicit_stored_connect_survives_new_process_but_proof_never_does() {
        let dir = temp_dir();
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let intent = dir.join(DESIRED_TUNNEL_FILENAME);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        let first_process = with_directory(&dir);
        assert_eq!(first_process.snapshot().unwrap()["state"], "disconnected");
        assert_eq!(restorable_tunnel_id(&intent, &saved), None);
        let generation = first_process.current_generation().unwrap();
        let first = first_process
            .connect_stored_user(FIRST.into(), KEY.into(), generation)
            .unwrap();
        assert_eq!(first["state"], "connecting");
        assert_eq!(first["desiredConnected"], true);
        assert_eq!(first["pollActive"], false);
        assert_eq!(first["toolCallsVerified"], 0);
        let on_disk: Value = serde_json::from_slice(&fs::read(&intent).unwrap()).unwrap();
        assert_eq!(
            on_disk,
            json!({"version":1,"desiredConnected":true,"tunnelId":FIRST})
        );
        assert!(!fs::read_to_string(&intent).unwrap().contains("sk-"));
        assert!(!fs::read_to_string(&intent).unwrap().contains("verified"));
        assert!(
            first_process
                .connect_stored_user(FIRST.into(), KEY.into(), generation)
                .is_err(),
            "a second worker or opt-in must never be started for the same session"
        );

        // Simulate process termination WITHOUT a user Disconnect (which would
        // deliberately persist opt-out). Abort the worker before any HTTP IO.
        {
            let mut session = first_process.session.lock().unwrap();
            AndroidOpenAiTunnel::reset_session(&mut session);
        }
        drop(first_process);
        let reopened = with_directory(&dir);
        assert_eq!(reopened.snapshot().unwrap()["state"], "disconnected");
        assert_eq!(reopened.snapshot().unwrap()["toolCallsVerified"], 0);
        assert_eq!(restorable_tunnel_id(&intent, &saved), Some(FIRST.into()));
        let generation = reopened.restoration_generation().unwrap();
        assert_eq!(reopened.restoration_generation(), None);
        let resumed = reopened
            .connect_restored(FIRST.into(), KEY.into(), generation)
            .unwrap();
        assert_eq!(resumed["state"], "connecting");
        assert_eq!(resumed["desiredConnected"], true);
        assert_eq!(resumed["pollActive"], false);
        assert_eq!(resumed["toolCallsVerified"], 0);
        assert!(reopened
            .connect_restored(FIRST.into(), KEY.into(), generation)
            .is_err());

        reopened.disconnect().unwrap();
        assert_eq!(reopened.snapshot().unwrap()["desiredConnected"], false);
        assert_eq!(restorable_tunnel_id(&intent, &saved), None);
        let later = with_directory(&dir);
        assert_eq!(later.snapshot().unwrap()["state"], "disconnected");
        assert_eq!(restorable_tunnel_id(&intent, &saved), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn disconnect_during_keystore_load_defeats_stale_manual_or_startup_connect() {
        let dir = temp_dir();
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let desired = dir.join(DESIRED_TUNNEL_FILENAME);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        store_desired_tunnel(&desired, Some(FIRST)).unwrap();
        let tunnel = with_directory(&dir);
        let generation = tunnel.current_generation().unwrap();
        assert_eq!(tunnel.restoration_generation(), Some(generation));
        tunnel.disconnect().unwrap();
        assert!(tunnel
            .connect_restored(FIRST.into(), KEY.into(), generation)
            .is_err());
        assert!(tunnel
            .connect_stored_user(FIRST.into(), KEY.into(), generation)
            .is_err());
        assert_eq!(tunnel.snapshot().unwrap()["state"], "disconnected");
        assert_eq!(read_desired_tunnel(&desired), None);
        assert_eq!(tunnel.restoration_generation(), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn changing_saved_id_or_using_ephemeral_key_cancels_old_autoreconnect() {
        let dir = temp_dir();
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let desired = dir.join(DESIRED_TUNNEL_FILENAME);
        let tunnel = with_directory(&dir);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        store_desired_tunnel(&desired, Some(FIRST)).unwrap();
        save_draft_core_id(&saved, SECOND, true, &tunnel).unwrap();
        assert_eq!(read_desired_tunnel(&desired), None);
        assert_eq!(restorable_tunnel_id(&desired, &saved), None);
        assert_eq!(tunnel.snapshot().unwrap()["state"], "disconnected");

        save_draft_core_id(&saved, FIRST, true, &tunnel).unwrap();
        store_desired_tunnel(&desired, Some(FIRST)).unwrap();
        let receipt = tunnel.connect(FIRST.into(), KEY.into()).unwrap();
        assert_eq!(receipt["state"], "connecting");
        assert_eq!(read_desired_tunnel(&desired), None);
        tunnel.disconnect().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persistent_restore_requires_matching_saved_core_id_even_with_opt_in() {
        let dir = temp_dir();
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let desired = dir.join(DESIRED_TUNNEL_FILENAME);
        let tunnel = with_directory(&dir);
        store_confirmed_core_id(&saved, SECOND).unwrap();
        store_desired_tunnel(&desired, Some(FIRST)).unwrap();
        assert_eq!(restorable_tunnel_id(&desired, &saved), None);
        assert!(tunnel
            .connect_stored_user(FIRST.into(), KEY.into(), 0)
            .is_err());
        assert_eq!(tunnel.snapshot().unwrap()["state"], "disconnected");
        assert_eq!(read_desired_tunnel(&desired), Some(FIRST.into()));
        store_confirmed_core_id(&saved, FIRST).unwrap();
        assert_eq!(restorable_tunnel_id(&desired, &saved), Some(FIRST.into()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn transient_poll_reconnect_delays_are_bounded_and_auth_fails_terminally() {
        let mut delay = POLL_RETRY_INITIAL;
        assert_eq!(delay, Duration::from_millis(300));
        for _ in 0..30 {
            assert!(delay >= POLL_RETRY_INITIAL && delay <= POLL_RETRY_MAX);
            delay = next_poll_retry(delay);
        }
        assert_eq!(delay, Duration::from_secs(8));
        assert!(!TunnelFailure::Network.fatal());
        assert!(!TunnelFailure::InvalidPayload.fatal());
        assert!(TunnelFailure::Unauthorized.fatal());
        assert!(TunnelFailure::Rejected.fatal());
    }

    #[test]
    fn first_permitted_remote_ack_writes_only_validated_pairing_id() {
        let dir = temp_dir();
        let tunnel = with_directory(&dir);
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let desired = dir.join(DESIRED_TUNNEL_FILENAME);
        let pairing = dir.join(CORE_PLUGIN_PAIRING_FILENAME);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        store_desired_tunnel(&desired, Some(FIRST)).unwrap();
        arm_healthy_poll(&tunnel, FIRST, 3);
        assert!(!pairing.exists());
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], false);

        settle_ack(&tunnel, 3, true, Ok(()));
        let receipt = tunnel.snapshot().unwrap();
        assert_eq!(receipt["state"], "tool_verified");
        assert_eq!(receipt["pollActive"], true);
        assert_eq!(receipt["corePluginLinked"], true);
        assert_eq!(receipt["toolCallsVerified"], 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&pairing).unwrap()).unwrap(),
            json!({"tunnelId":FIRST})
        );
        assert!(!fs::read_to_string(&pairing).unwrap().contains("sk-"));
        assert!(!fs::read_to_string(&pairing).unwrap().contains("verified"));
        settle_ack(&tunnel, 3, true, Ok(()));
        assert_eq!(tunnel.snapshot().unwrap()["toolCallsVerified"], 2);
        assert_eq!(read_confirmed_core_id(&pairing), Some(FIRST.to_owned()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cold_restart_healthy_poll_uses_history_without_replaying_tool_calls() {
        let dir = temp_dir();
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let desired = dir.join(DESIRED_TUNNEL_FILENAME);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        store_desired_tunnel(&desired, Some(FIRST)).unwrap();
        let first = with_directory(&dir);
        arm_healthy_poll(&first, FIRST, 7);
        settle_ack(&first, 7, true, Ok(()));
        assert_eq!(first.snapshot().unwrap()["corePluginLinked"], true);
        drop(first);

        let new_process = with_directory(&dir);
        let before_poll = new_process.snapshot().unwrap();
        assert_eq!(before_poll["state"], "disconnected");
        assert_eq!(before_poll["corePluginLinked"], false);
        assert_eq!(before_poll["toolCallsVerified"], 0);
        arm_healthy_poll(&new_process, FIRST, 1);
        let after_poll = new_process.snapshot().unwrap();
        assert_eq!(after_poll["state"], "polling");
        assert_eq!(after_poll["pollActive"], true);
        assert_eq!(after_poll["corePluginLinked"], true);
        assert_eq!(after_poll["toolCallsVerified"], 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_corrupt_mismatched_pairing_or_stale_poll_never_links_core_plugin() {
        let dir = temp_dir();
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let desired = dir.join(DESIRED_TUNNEL_FILENAME);
        let pairing = dir.join(CORE_PLUGIN_PAIRING_FILENAME);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        store_desired_tunnel(&desired, Some(FIRST)).unwrap();
        let tunnel = with_directory(&dir);
        arm_healthy_poll(&tunnel, FIRST, 4);
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], false);
        for corrupt in [
            json!({"tunnelId":SECOND}),
            json!({"tunnelId":FIRST,"verified":true}),
            json!({"tunnelId":FIRST,"toolCallsVerified":9}),
            json!({"tunnelId":"../invalid"}),
            json!([FIRST]),
        ] {
            fs::write(&pairing, serde_json::to_vec(&corrupt).unwrap()).unwrap();
            assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], false);
        }
        fs::write(&pairing, b"not-json").unwrap();
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], false);
        fs::write(&pairing, vec![b'X'; 161]).unwrap();
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], false);
        store_core_plugin_pairing(&pairing, FIRST).unwrap();
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], true);

        store_desired_tunnel(&desired, None).unwrap();
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], false);
        store_desired_tunnel(&desired, Some(FIRST)).unwrap();
        store_confirmed_core_id(&saved, SECOND).unwrap();
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], false);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], true);
        {
            let mut current = tunnel.session.lock().unwrap();
            current.last_poll = Some(Instant::now() - HEARTBEAT_LIMIT);
        }
        let expired = tunnel.snapshot().unwrap();
        assert_eq!(expired["state"], "error");
        assert_eq!(expired["pollActive"], false);
        assert_eq!(expired["corePluginLinked"], false);
        {
            let mut current = tunnel.session.lock().unwrap();
            current.phase = Phase::Error;
            current.last_poll = Some(Instant::now());
        }
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], false);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn disconnect_clears_pairing_and_stale_generation_ack_cannot_recreate_it() {
        let dir = temp_dir();
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let desired = dir.join(DESIRED_TUNNEL_FILENAME);
        let pairing = dir.join(CORE_PLUGIN_PAIRING_FILENAME);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        store_desired_tunnel(&desired, Some(FIRST)).unwrap();
        let tunnel = with_directory(&dir);
        arm_healthy_poll(&tunnel, FIRST, 19);
        settle_ack(&tunnel, 19, true, Ok(()));
        assert!(pairing.exists());
        tunnel.disconnect().unwrap();
        assert!(!pairing.exists());
        assert_eq!(tunnel.snapshot().unwrap()["desiredConnected"], false);
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], false);

        // A POST may have received a successful ACK just as the user pressed
        // Disconnect. If its async completion races in afterward, the mutex
        // and generation fence must reject it, without resurrecting the file.
        settle_ack(&tunnel, 19, true, Ok(()));
        assert!(!pairing.exists());
        assert_eq!(tunnel.snapshot().unwrap()["toolCallsVerified"], 0);
        store_desired_tunnel(&desired, Some(FIRST)).unwrap();
        arm_healthy_poll(&tunnel, FIRST, 21);
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], false);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_response_or_non_tool_ack_cannot_persist_pairing() {
        let dir = temp_dir();
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let desired = dir.join(DESIRED_TUNNEL_FILENAME);
        let pairing = dir.join(CORE_PLUGIN_PAIRING_FILENAME);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        store_desired_tunnel(&desired, Some(FIRST)).unwrap();
        let tunnel = with_directory(&dir);
        arm_healthy_poll(&tunnel, FIRST, 31);
        for error in [
            TunnelFailure::Network,
            TunnelFailure::Unauthorized,
            TunnelFailure::Rejected,
            TunnelFailure::InvalidPayload,
        ] {
            settle_ack(&tunnel, 31, true, Err(error));
            assert!(!pairing.exists());
        }
        settle_ack(&tunnel, 31, false, Ok(()));
        assert!(!pairing.exists());
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], false);
        assert_eq!(tunnel.snapshot().unwrap()["toolCallsVerified"], 0);

        // No opt-in (or mismatched current saved ID) can ever write a marker.
        store_desired_tunnel(&desired, None).unwrap();
        settle_ack(&tunnel, 31, true, Ok(()));
        assert!(!pairing.exists());
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], false);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn changing_core_tunnel_id_invalidates_pairing_before_new_id_is_saved() {
        let dir = temp_dir();
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let desired = dir.join(DESIRED_TUNNEL_FILENAME);
        let pairing = dir.join(CORE_PLUGIN_PAIRING_FILENAME);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        store_desired_tunnel(&desired, Some(FIRST)).unwrap();
        let tunnel = with_directory(&dir);
        arm_healthy_poll(&tunnel, FIRST, 2);
        settle_ack(&tunnel, 2, true, Ok(()));
        assert!(pairing.exists());
        save_draft_core_id(&saved, FIRST, true, &tunnel).unwrap();
        assert!(
            pairing.exists(),
            "resaving the same ID must retain true history"
        );
        save_draft_core_id(&saved, SECOND, true, &tunnel).unwrap();
        assert!(!pairing.exists());
        assert_eq!(tunnel.snapshot().unwrap()["corePluginLinked"], false);
        assert_eq!(tunnel.snapshot().unwrap()["state"], "disconnected");
        settle_ack(&tunnel, 2, true, Ok(()));
        assert!(!pairing.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn health_clocks_have_distinct_real_proofs_and_runtime_uptime() {
        let tunnel = AndroidOpenAiTunnel::default();
        let initial = tunnel.snapshot().unwrap();
        assert_eq!(initial["health"]["verifiedLinkAtMs"], Value::Null);
        assert_eq!(initial["health"]["lastChatgptCallAtMs"], Value::Null);
        assert_eq!(initial["health"]["pollErrors"], 0);
        assert_eq!(initial["health"]["uptimeSeconds"], Value::Null);
        assert_eq!(initial["health"]["route"], Value::Null);
        assert_eq!(initial["health"]["probe"], Value::Null);
        assert_eq!(initial["health"]["localServer"], "in-process (Android)");
        assert_eq!(initial["health"]["tunnelClient"], env!("CARGO_PKG_VERSION"));
        assert_eq!(initial["events"], json!([]));

        let before_poll = wallclock_ms().unwrap();
        let worker = tokio::spawn(async { std::future::pending::<()>().await });
        {
            let mut session = tunnel.session.lock().unwrap();
            session.generation = 11;
            session.phase = Phase::Connecting;
            session.started_at = Some(Instant::now() - Duration::from_secs(74));
            session.worker = Some(worker);
        }
        assert_eq!(
            tunnel.snapshot().unwrap()["health"]["route"],
            ANDROID_TUNNEL_ROUTE
        );
        assert_eq!(
            tunnel.snapshot().unwrap()["health"]["verifiedLinkAtMs"],
            Value::Null
        );
        update_state(&tunnel.session, 11, Phase::Polling, None, true, false);
        let after_poll = wallclock_ms().unwrap();
        let polled = tunnel.snapshot().unwrap();
        let verified_at = polled["health"]["verifiedLinkAtMs"].as_u64().unwrap();
        assert!((before_poll..=after_poll).contains(&verified_at));
        assert_eq!(polled["health"]["lastChatgptCallAtMs"], Value::Null);
        assert_eq!(polled["health"]["pollErrors"], 0);
        assert!(polled["health"]["uptimeSeconds"].as_u64().unwrap() >= 74);
        assert_eq!(polled["health"]["route"], ANDROID_TUNNEL_ROUTE);
        assert_eq!(polled["events"].as_array().unwrap().len(), 1);
        assert_eq!(
            polled["events"][0]["detail"],
            "OpenAI tunnel polling healthy"
        );

        // An ordinary succeeding long-poll tick updates freshness, not the
        // timeline: only the transition into health earns a new event.
        update_state(&tunnel.session, 11, Phase::Polling, None, true, false);
        assert_eq!(
            tunnel.snapshot().unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        tunnel.disconnect().unwrap();
        let off = tunnel.snapshot().unwrap();
        assert_eq!(off["health"]["verifiedLinkAtMs"], Value::Null);
        assert_eq!(off["health"]["uptimeSeconds"], Value::Null);
        assert_eq!(off["health"]["route"], Value::Null);
        assert_eq!(off["events"][0]["detail"], "Tunnel disconnected");
    }

    #[test]
    fn real_failed_poll_and_response_count_without_repeating_error_events() {
        let tunnel = AndroidOpenAiTunnel::default();
        {
            let mut session = tunnel.session.lock().unwrap();
            session.phase = Phase::Connecting;
            session.generation = 5;
        }
        update_state(
            &tunnel.session,
            5,
            Phase::Error,
            Some(TunnelFailure::Network.message()),
            false,
            false,
        );
        update_state(
            &tunnel.session,
            5,
            Phase::Error,
            Some(TunnelFailure::Network.message()),
            false,
            false,
        );
        let failures = tunnel.snapshot().unwrap();
        assert_eq!(failures["health"]["pollErrors"], 2);
        assert_eq!(failures["health"]["verifiedLinkAtMs"], Value::Null);
        assert_eq!(failures["health"]["lastChatgptCallAtMs"], Value::Null);
        assert_eq!(failures["events"].as_array().unwrap().len(), 1);
        assert_eq!(failures["events"][0]["kind"], "error");
        assert_eq!(
            failures["events"][0]["detail"],
            "Tunnel network request failed"
        );

        update_state(&tunnel.session, 5, Phase::Polling, None, true, false);
        let recovered = tunnel.snapshot().unwrap();
        assert!(recovered["health"]["verifiedLinkAtMs"].is_number());
        assert_eq!(
            recovered["events"][0]["detail"],
            "OpenAI tunnel polling healthy"
        );
        update_state(
            &tunnel.session,
            5,
            Phase::Error,
            Some(TunnelFailure::Unauthorized.message()),
            false,
            false,
        );
        let refailed = tunnel.snapshot().unwrap();
        assert_eq!(refailed["health"]["pollErrors"], 3);
        assert_eq!(
            refailed["events"][0]["detail"],
            "OpenAI tunnel authentication denied"
        );
        assert_eq!(
            refailed["health"]["verifiedLinkAtMs"],
            recovered["health"]["verifiedLinkAtMs"]
        );
        // Late completions from the old generation cannot increment counters.
        update_state(
            &tunnel.session,
            4,
            Phase::Error,
            Some(TunnelFailure::Network.message()),
            false,
            false,
        );
        assert_eq!(tunnel.snapshot().unwrap()["health"]["pollErrors"], 3);
    }

    #[test]
    fn inbound_commands_are_not_a_confirmed_chatgpt_tool_call() {
        let tunnel = AndroidOpenAiTunnel::default();
        {
            let mut session = tunnel.session.lock().unwrap();
            session.phase = Phase::Polling;
            session.generation = 32;
            session.last_poll = Some(Instant::now());
        }
        note_received_tunnel_commands(&tunnel.session, 31);
        assert_eq!(tunnel.snapshot().unwrap()["events"], json!([]));
        note_received_tunnel_commands(&tunnel.session, 32);
        let received = tunnel.snapshot().unwrap();
        assert_eq!(received["events"].as_array().unwrap().len(), 1);
        assert_eq!(received["events"][0]["kind"], "request");
        assert_eq!(
            received["events"][0]["detail"],
            "MCP command batch received"
        );
        assert_eq!(received["health"]["lastChatgptCallAtMs"], Value::Null);
        assert_eq!(received["toolCallsVerified"], 0);
        tunnel.disconnect().unwrap();
        note_received_tunnel_commands(&tunnel.session, 32);
        assert_eq!(
            tunnel.snapshot().unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn only_successful_permitted_ack_sets_last_chatgpt_call_time() {
        let tunnel = AndroidOpenAiTunnel::default();
        {
            let mut session = tunnel.session.lock().unwrap();
            session.phase = Phase::Polling;
            session.generation = 27;
            session.last_poll = Some(Instant::now());
        }
        settle_ack(&tunnel, 27, false, Ok(()));
        settle_ack(&tunnel, 27, true, Err(TunnelFailure::Network));
        assert_eq!(
            tunnel.snapshot().unwrap()["health"]["lastChatgptCallAtMs"],
            Value::Null
        );
        assert_eq!(tunnel.snapshot().unwrap()["toolCallsVerified"], 0);
        let before = wallclock_ms().unwrap();
        settle_ack(&tunnel, 27, true, Ok(()));
        let after = wallclock_ms().unwrap();
        let acknowledged = tunnel.snapshot().unwrap();
        let at = acknowledged["health"]["lastChatgptCallAtMs"]
            .as_u64()
            .unwrap();
        assert!((before..=after).contains(&at));
        assert_eq!(acknowledged["toolCallsVerified"], 1);
        assert_eq!(acknowledged["events"][0]["kind"], "tool");
        assert_eq!(
            acknowledged["events"][0]["detail"],
            "MCP tool response acknowledged by OpenAI"
        );
        tunnel.disconnect().unwrap();
        settle_ack(&tunnel, 27, true, Ok(()));
        let after_disconnect = tunnel.snapshot().unwrap();
        assert_eq!(
            after_disconnect["health"]["lastChatgptCallAtMs"],
            Value::Null
        );
        assert_eq!(after_disconnect["toolCallsVerified"], 0);
        assert_eq!(
            after_disconnect["events"][0]["detail"],
            "Tunnel disconnected"
        );
    }

    #[test]
    fn activity_is_bounded_newest_first_and_never_contains_tunnel_identity_or_credentials() {
        let tunnel = AndroidOpenAiTunnel::default();
        {
            let mut session = tunnel.session.lock().unwrap();
            session.active_tunnel_id = Some(FIRST.into());
            for index in 0..(MAX_TELEMETRY_EVENTS + 25) {
                let (kind, detail) = if index % 2 == 0 {
                    ("error", "Tunnel network request failed")
                } else {
                    ("tunnel", "OpenAI tunnel polling healthy")
                };
                append_tunnel_event(&mut session, kind, detail);
            }
        }
        let snapshot = tunnel.snapshot().unwrap();
        let events = snapshot["events"].as_array().unwrap();
        assert_eq!(events.len(), MAX_TELEMETRY_EVENTS);
        assert_eq!(events[0]["kind"], "error");
        assert_eq!(events[MAX_TELEMETRY_EVENTS - 1]["kind"], "tunnel");
        assert!(events.iter().all(|event| {
            event.as_object().unwrap().len() == 3
                && event["timestampMs"].as_u64().is_some_and(|at| at > 0)
                && matches!(
                    event["kind"].as_str(),
                    Some("tunnel" | "request" | "tool" | "error")
                )
        }));
        assert!(events.windows(2).all(|pair| {
            pair[0]["timestampMs"].as_u64().unwrap() >= pair[1]["timestampMs"].as_u64().unwrap()
        }));
        let wire = snapshot.to_string();
        assert!(!wire.contains(FIRST));
        assert!(!wire.contains(KEY));
        assert!(!wire.contains("request_id"));
        assert!(!wire.contains("arguments"));
    }

    #[test]
    fn cold_start_never_restores_process_local_telemetry_even_with_pairing_history() {
        let dir = temp_dir();
        let saved = dir.join(CORE_TUNNEL_FILENAME);
        let desired = dir.join(DESIRED_TUNNEL_FILENAME);
        let pairing = dir.join(CORE_PLUGIN_PAIRING_FILENAME);
        store_confirmed_core_id(&saved, FIRST).unwrap();
        store_desired_tunnel(&desired, Some(FIRST)).unwrap();
        let first_process = with_directory(&dir);
        arm_healthy_poll(&first_process, FIRST, 1);
        update_state(&first_process.session, 1, Phase::Polling, None, true, false);
        settle_ack(&first_process, 1, true, Ok(()));
        assert!(pairing.exists());
        assert_eq!(first_process.snapshot().unwrap()["corePluginLinked"], true);
        assert!(first_process.snapshot().unwrap()["health"]["verifiedLinkAtMs"].is_number());
        assert!(first_process.snapshot().unwrap()["health"]["lastChatgptCallAtMs"].is_number());
        drop(first_process);

        let restarted = with_directory(&dir);
        let snapshot = restarted.snapshot().unwrap();
        assert_eq!(snapshot["health"]["verifiedLinkAtMs"], Value::Null);
        assert_eq!(snapshot["health"]["lastChatgptCallAtMs"], Value::Null);
        assert_eq!(snapshot["health"]["pollErrors"], 0);
        assert_eq!(snapshot["health"]["uptimeSeconds"], Value::Null);
        assert_eq!(snapshot["events"], json!([]));
        assert_eq!(snapshot["toolCallsVerified"], 0);
        assert_eq!(snapshot["corePluginLinked"], false);
        // A new live poll can revalidate historical pairing, without
        // importing an old tool event, count or ACK timestamp.
        arm_healthy_poll(&restarted, FIRST, 1);
        update_state(&restarted.session, 1, Phase::Polling, None, true, false);
        assert_eq!(restarted.snapshot().unwrap()["corePluginLinked"], true);
        assert_eq!(
            restarted.snapshot().unwrap()["health"]["lastChatgptCallAtMs"],
            Value::Null
        );
        fs::remove_dir_all(dir).unwrap();
    }
}

/// A verified tunnel ID is a tunnel record readable with the already-saved
/// restricted runtime key. It does NOT prove that the ChatGPT connector is
/// paired to a particular workspace or that a remote tool call has succeeded.
/// No key, raw API error, or account identifier is exposed to the WebView.
#[tauri::command]
pub async fn android_tunnel_verify_and_save(
    app: AppHandle,
    state: tauri::State<'_, AndroidOpenAiTunnel>,
    tunnel_id: String,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated {
        return Err("Explicit Save action required".into());
    }
    if !cfg!(target_os = "android") {
        return Err("Core tunnel verification is available on Android only".into());
    }
    if !valid_tunnel_id(&tunnel_id) {
        return Ok(json!({"verified":false,"saved":false,"reason":"invalid_id"}));
    }
    let app_for_key = app.clone();
    let key = match tauri::async_runtime::spawn_blocking(move || {
        crate::android_tunnel_credentials::load_for_connect(app_for_key)
    })
    .await
    {
        Ok(Ok(key)) => key,
        _ => return Ok(json!({"verified":false,"saved":false,"reason":"key_required"})),
    };
    let client = tunnel_metadata_client()?;
    let url = format!("{BASE_URL}/v1/tunnels/{tunnel_id}");
    let result = verify_tunnel_metadata(&client, &url, &tunnel_id, &key).await;
    if result != "ok" {
        return Ok(json!({"verified":false,"saved":false,"reason":result}));
    }
    let path = core_tunnel_id_path(&app)?;
    save_draft_core_id(&path, &tunnel_id, true, &state)?;
    Ok(json!({"verified":true,"saved":true,"reason":"ok"}))
}

/// Step 7 read-only status check. Unlike verify_and_save this is safe during
/// app startup: it never writes an ID, starts/stops a tunnel, or grants a tool
/// permission. It reads the current saved ID and Keystore key privately then
/// checks a bounded OpenAI HTTPS metadata response with matching exact ID.
#[tauri::command]
pub async fn android_tunnel_verify_stored_access(app: AppHandle) -> Result<Value, String> {
    if !cfg!(target_os = "android") {
        return Err("Core tunnel verification is available on Android only".into());
    }
    let path = core_tunnel_id_path(&app)?;
    let Some(tunnel_id) = read_confirmed_core_id(&path) else {
        return Ok(stored_access_receipt(None, "id_required"));
    };
    let for_key = app.clone();
    let key = match tauri::async_runtime::spawn_blocking(move || {
        crate::android_tunnel_credentials::load_for_connect(for_key)
    })
    .await
    {
        Ok(Ok(key)) => key,
        _ => return Ok(stored_access_receipt(Some(&tunnel_id), "key_required")),
    };
    let client = tunnel_metadata_client()?;
    let url = format!("{BASE_URL}/v1/tunnels/{tunnel_id}");
    let reason = verify_tunnel_metadata(&client, &url, &tunnel_id, &key).await;
    // Prevent a stale in-flight verification from authorizing an ID the user
    // changed while the HTTPS request was pending.
    if read_confirmed_core_id(&path).as_deref() != Some(&tunnel_id) {
        return Ok(stored_access_receipt(None, "id_changed"));
    }
    Ok(stored_access_receipt(Some(&tunnel_id), reason))
}

/// Read a non-secret ID from app-private storage without claiming that an old
/// authentication proof is still current. Verify with a runtime key in Step 7.
#[tauri::command]
pub async fn android_tunnel_saved_id_status(app: AppHandle) -> Result<Value, String> {
    if !cfg!(target_os = "android") {
        return Err("Core tunnel status is available on Android only".into());
    }
    let path = core_tunnel_id_path(&app)?;
    Ok(saved_core_id_status(&path))
}

#[tauri::command]
pub async fn android_openai_tunnel_disconnect(
    state: tauri::State<'_, AndroidOpenAiTunnel>,
    user_initiated: bool,
) -> Result<Value, String> {
    if !user_initiated {
        return Err("Explicit user action required to disconnect tunnel".into());
    }
    state.disconnect()
}

pub(crate) fn valid_tunnel_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("tunnel_") else {
        return false;
    };
    let valid = |s: &str, length: usize| {
        s.len() == length
            && s.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    };
    if valid(rest, 32) {
        return true;
    }
    match rest.split_once('_') {
        Some((namespace, suffix)) => valid(namespace, 4) && valid(suffix, 32),
        None => false,
    }
}

fn valid_api_key(key: &str) -> bool {
    (8..=512).contains(&key.len()) && key.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn update_state(
    session: &Arc<Mutex<Session>>,
    generation: u64,
    phase: Phase,
    error: Option<&'static str>,
    poll_success: bool,
    verified_tool: bool,
) {
    if let Ok(mut current) = session.lock() {
        if current.generation != generation {
            return;
        }
        let newly_healthy = poll_success
            && (current
                .last_poll
                .is_none_or(|when| when.elapsed() >= HEARTBEAT_LIMIT)
                || !matches!(current.phase, Phase::Polling | Phase::ToolVerified));
        let new_error =
            error.is_some() && (current.phase != Phase::Error || current.last_error != error);
        if error.is_some() {
            current.poll_errors = current.poll_errors.saturating_add(1);
            if new_error {
                // A fixed vocabulary, not the raw request/response or network
                // error, crosses into Activity. Repeated failures count, but
                // generate only one visible error until recovery/change.
                let detail = match error {
                    Some("Tunnel authentication denied (401/403)") => {
                        "OpenAI tunnel authentication denied"
                    }
                    Some("Tunnel control plane rejected request") => {
                        "OpenAI rejected a tunnel request"
                    }
                    Some("Tunnel network request failed") => "Tunnel network request failed",
                    Some("Tunnel payload exceeded safety limit") => {
                        "Tunnel payload exceeded its safety limit"
                    }
                    Some("Tunnel protocol returned invalid JSON") => {
                        "Tunnel response was malformed"
                    }
                    _ => "Tunnel request failed",
                };
                append_tunnel_event(&mut current, "error", detail);
            }
        }
        if newly_healthy {
            append_tunnel_event(&mut current, "tunnel", "OpenAI tunnel polling healthy");
        }
        current.phase = phase;
        current.last_error = error;
        if poll_success {
            current.last_poll = Some(Instant::now());
            current.verified_link_at_ms = wallclock_ms();
        } else if error.is_some() {
            current.last_poll = None;
        }
        if verified_tool {
            current.verified_calls = current.verified_calls.saturating_add(1);
        }
    }
}

fn note_received_tunnel_commands(session: &Arc<Mutex<Session>>, generation: u64) {
    if let Ok(mut current) = session.lock() {
        if current.generation == generation
            && matches!(current.phase, Phase::Polling | Phase::ToolVerified)
        {
            append_tunnel_event(&mut current, "request", "MCP command batch received");
        }
    }
}

/// Called exclusively after a permitted real tools/call response received an
/// explicit successful OpenAI response ACK. The generation check and marker
/// write occur under the SAME mutex as explicit Disconnect and Core ID edits:
/// a late ACK can neither revive an old user opt-in nor recreate its marker.
fn acknowledge_permitted_tool_response(
    session: &Arc<Mutex<Session>>,
    generation: u64,
    proof_paths: Option<&CorePluginProofPaths>,
) {
    let Ok(mut current) = session.lock() else {
        return;
    };
    if current.generation != generation || current.phase == Phase::Disconnected {
        return;
    }
    current.phase = Phase::ToolVerified;
    current.last_error = None;
    current.verified_calls = current.verified_calls.saturating_add(1);
    current.last_chatgpt_call_at_ms = wallclock_ms();
    append_tunnel_event(
        &mut current,
        "tool",
        "MCP tool response acknowledged by OpenAI",
    );

    let Some(paths) = proof_paths else {
        return;
    };
    let Some(active) = current.active_tunnel_id.as_deref() else {
        return;
    };
    if !valid_tunnel_id(active)
        || restorable_tunnel_id(&paths.desired, &paths.saved_id).as_deref() != Some(active)
    {
        return;
    }
    // If an earlier ACK already persisted the identical ID, avoid unnecessary
    // storage writes; a malformed/mismatched marker is replaced only NOW,
    // after a newly acknowledged, permitted real call.
    if read_confirmed_core_id(&paths.pairing).as_deref() != Some(active) {
        let _ = store_core_plugin_pairing(&paths.pairing, active);
    }
}

#[derive(Debug, Clone, Copy)]
enum TunnelFailure {
    Unauthorized,
    Rejected,
    Network,
    Oversized,
    InvalidPayload,
}

impl TunnelFailure {
    fn message(self) -> &'static str {
        match self {
            Self::Unauthorized => "Tunnel authentication denied (401/403)",
            Self::Rejected => "Tunnel control plane rejected request",
            Self::Network => "Tunnel network request failed",
            Self::Oversized => "Tunnel payload exceeded safety limit",
            Self::InvalidPayload => "Tunnel protocol returned invalid JSON",
        }
    }

    fn fatal(self) -> bool {
        matches!(self, Self::Unauthorized | Self::Rejected)
    }
}

fn next_poll_retry(backoff: Duration) -> Duration {
    (backoff * 2).min(POLL_RETRY_MAX)
}

async fn poll_retry_delay(backoff: Duration) {
    // Small jitter prevents synchronized tunnel clients after an outage; the
    // entire delay stays under nine seconds and never loops without sleeping.
    let jitter_ms = u64::from(uuid::Uuid::new_v4().as_bytes()[0]) % 151;
    tokio::time::sleep(backoff + Duration::from_millis(jitter_ms)).await;
}

async fn run_poll_loop(
    session: Arc<Mutex<Session>>,
    permissions: AndroidMcpPermissions,
    plugins_handler: Option<PluginRpcHandler>,
    proof_paths: Option<CorePluginProofPaths>,
    generation: u64,
    client: Client,
    tunnel_id: String,
    api_key: String,
    instance_id: String,
) {
    let url = format!("{BASE_URL}/v1/tunnels/{tunnel_id}");
    let mut backoff = POLL_RETRY_INITIAL;
    loop {
        match poll_once(&client, &url, &api_key, &instance_id).await {
            Ok((commands, received_at)) => {
                let already_verified = session
                    .lock()
                    .ok()
                    .is_some_and(|s| s.generation == generation && s.verified_calls > 0);
                update_state(
                    &session,
                    generation,
                    if already_verified {
                        Phase::ToolVerified
                    } else {
                        Phase::Polling
                    },
                    None,
                    true,
                    false,
                );
                if !commands.is_empty() {
                    note_received_tunnel_commands(&session, generation);
                }
                backoff = POLL_RETRY_INITIAL;
                let post_error = process_batch(
                    &session,
                    &permissions,
                    plugins_handler.as_ref(),
                    proof_paths.as_ref(),
                    generation,
                    &client,
                    &url,
                    &api_key,
                    &instance_id,
                    commands,
                    received_at,
                )
                .await;
                if let Some(failure) = post_error {
                    update_state(
                        &session,
                        generation,
                        Phase::Error,
                        Some(failure.message()),
                        false,
                        false,
                    );
                    if failure.fatal() {
                        return;
                    }
                    poll_retry_delay(backoff).await;
                    backoff = next_poll_retry(backoff);
                }
            }
            Err(failure) => {
                update_state(
                    &session,
                    generation,
                    Phase::Error,
                    Some(failure.message()),
                    false,
                    false,
                );
                if failure.fatal() {
                    return;
                }
                poll_retry_delay(backoff).await;
                backoff = next_poll_retry(backoff);
            }
        }
    }
}

/// Each command has its own response lease and retry budget. Awaiting a slow or
/// failed POST must not starve other commands in the same poll batch. JoinSet
/// aborts outstanding requests when the parent poll task is disconnected.
async fn process_batch(
    session: &Arc<Mutex<Session>>,
    permissions: &AndroidMcpPermissions,
    plugins_handler: Option<&PluginRpcHandler>,
    proof_paths: Option<&CorePluginProofPaths>,
    generation: u64,
    client: &Client,
    url: &str,
    key: &str,
    instance_id: &str,
    commands: Vec<Value>,
    received_at: Instant,
) -> Option<TunnelFailure> {
    let mut pending = tokio::task::JoinSet::new();
    let mut failure = None;
    for command in commands {
        let Some(command_expires) = command_deadline(&command, received_at) else {
            continue;
        };
        if pending.len() >= MAX_IN_FLIGHT_RESPONSES {
            if let Some(result) = pending.join_next().await {
                settle_batch_result(session, generation, proof_paths, result, &mut failure);
            }
        }
        let permissions = permissions.clone();
        let plugins_handler = plugins_handler.cloned();
        let (client, url, key, instance_id) = (
            client.clone(),
            url.to_owned(),
            key.to_owned(),
            instance_id.to_owned(),
        );
        pending.spawn(async move {
            // Native Step4 permission verification can cross Kotlin IPC and
            // file reads are blocking. Never stall Tokio's poll/response loop;
            // one bounded command runs on a blocking worker, then its exact
            // response is posted once/retried without re-executing the tool.
            // Native IPC is external to this process. Bound its wait by the
            // exact request lease; an OS hang cannot pin the tunnel poll
            // forever or produce a response after the original deadline.
            let prepared = tokio::time::timeout_at(
                tokio::time::Instant::from_std(command_expires),
                tokio::task::spawn_blocking(move || {
                    prepare_command_with_dispatcher(
                        &command, received_at, &permissions, plugins_handler.as_ref()
                    )
                }),
            )
            .await;
            let (body, shard_token, real_tool_call, deadline) = match prepared {
                Ok(Ok(Some(result))) => result,
                Ok(Ok(None)) => return (false, Ok(())),
                Ok(Err(_)) | Err(_) => return (false, Err(TunnelFailure::Network)),
            };
            (
                real_tool_call,
                post_response(
                    &client,
                    &url,
                    &key,
                    &instance_id,
                    &shard_token,
                    body,
                    deadline,
                )
                .await,
            )
        });
    }
    while let Some(result) = pending.join_next().await {
        settle_batch_result(session, generation, proof_paths, result, &mut failure);
    }
    failure
}

fn settle_batch_result(
    session: &Arc<Mutex<Session>>,
    generation: u64,
    proof_paths: Option<&CorePluginProofPaths>,
    result: Result<(bool, Result<(), TunnelFailure>), tokio::task::JoinError>,
    failure: &mut Option<TunnelFailure>,
) {
    match result {
        Ok((true, Ok(()))) => acknowledge_permitted_tool_response(session, generation, proof_paths),
        Ok((false, Ok(()))) => {}
        Ok((_, Err(error))) => {
            if failure.is_none_or(|current| !current.fatal() && error.fatal()) {
                *failure = Some(error);
            }
        }
        Err(_) => {
            if failure.is_none() {
                *failure = Some(TunnelFailure::Network);
            }
        }
    }
}

fn common_headers(request: RequestBuilder, key: &str, instance_id: &str) -> RequestBuilder {
    request
        .bearer_auth(key)
        .header("X-Tunnel-Client-Name", "mcp-agent-ai-android")
        .header("X-Tunnel-Client-Version", env!("CARGO_PKG_VERSION"))
        .header("X-Tunnel-Client-Wire-Protocol-Version", "2026-08-25")
        .header("X-Tunnel-Client-Instance-Id", instance_id)
        .header("X-Tunnel-MCP-Server-Info", SERVER_INFO)
}

async fn read_limited(mut response: Response, limit: usize) -> Result<Vec<u8>, TunnelFailure> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(TunnelFailure::Oversized);
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| TunnelFailure::Network)? {
        if chunk.len() > limit.saturating_sub(data.len()) {
            return Err(TunnelFailure::Oversized);
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

fn classify_response(status: StatusCode) -> Result<(), TunnelFailure> {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(TunnelFailure::Unauthorized);
    }
    if status == StatusCode::REQUEST_TIMEOUT || status == StatusCode::TOO_MANY_REQUESTS {
        return Err(TunnelFailure::Network);
    }
    if !status.is_success() {
        return Err(if status.is_client_error() {
            TunnelFailure::Rejected
        } else {
            TunnelFailure::Network
        });
    }
    Ok(())
}

async fn poll_once(
    client: &Client,
    url: &str,
    key: &str,
    instance_id: &str,
) -> Result<(Vec<Value>, Instant), TunnelFailure> {
    let request = common_headers(
        client
            .get(format!("{url}/poll"))
            .query(&[("limit", "4"), ("timeout_ms", "15000")]),
        key,
        instance_id,
    );
    let response = request.send().await.map_err(|_| TunnelFailure::Network)?;
    // The protocol's relative command timeout begins at header receipt.
    let received_at = Instant::now();
    classify_response(response.status())?;
    if response.status() == StatusCode::NO_CONTENT {
        return Ok((Vec::new(), received_at));
    }
    if response.status() != StatusCode::OK {
        return Err(TunnelFailure::InvalidPayload);
    }
    let bytes = read_limited(response, POLL_BODY_LIMIT).await?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| TunnelFailure::InvalidPayload)?;
    let commands = value
        .get("commands")
        .and_then(Value::as_array)
        .ok_or(TunnelFailure::InvalidPayload)?;
    if commands.len() > COMMAND_LIMIT {
        return Err(TunnelFailure::Oversized);
    }
    Ok((commands.clone(), received_at))
}

async fn post_response(
    client: &Client,
    url: &str,
    key: &str,
    instance_id: &str,
    shard_token: &str,
    payload: Value,
    deadline: Instant,
) -> Result<(), TunnelFailure> {
    let shard_header =
        header::HeaderValue::from_str(shard_token).map_err(|_| TunnelFailure::InvalidPayload)?;
    // Serialize ONCE: retrying replays this exact response envelope, including
    // the identical request_id/channel/shard_token, without re-running any MCP.
    let encoded = serde_json::to_vec(&payload).map_err(|_| TunnelFailure::InvalidPayload)?;
    if encoded.len() > POLL_BODY_LIMIT {
        return Err(TunnelFailure::Oversized);
    }
    let mut attempt = 0u32;
    loop {
        if Instant::now() >= deadline {
            return Err(TunnelFailure::Network);
        }
        let request = common_headers(client.post(format!("{url}/response")), key, instance_id)
            .header(header::CONTENT_TYPE, "application/json")
            .header("X-Tunnel-Shard-Token", shard_header.clone())
            .body(encoded.clone());
        let outcome = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
            let response = request
                .send()
                .await
                .map_err(|_| (TunnelFailure::Network, true))?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return Err((TunnelFailure::Unauthorized, false));
            }
            if !status.is_success() {
                return Err((
                    if status.is_client_error() {
                        TunnelFailure::Rejected
                    } else {
                        TunnelFailure::Network
                    },
                    retryable_post_status(status),
                ));
            }
            if status != StatusCode::OK {
                return Err((TunnelFailure::InvalidPayload, false));
            }
            let bytes = read_limited(response, RESPONSE_BODY_LIMIT)
                .await
                .map_err(|failure| (failure, matches!(failure, TunnelFailure::Network)))?;
            let ack: Value = serde_json::from_slice(&bytes)
                .map_err(|_| (TunnelFailure::InvalidPayload, false))?;
            if ack.get("status").and_then(Value::as_str) != Some("ok") {
                return Err((TunnelFailure::InvalidPayload, false));
            }
            Ok(())
        })
        .await;
        let failure = match outcome {
            Ok(Ok(())) => return Ok(()),
            Ok(Err((failure, false))) => return Err(failure),
            Ok(Err((failure, true))) => failure,
            Err(_) => return Err(TunnelFailure::Network),
        };
        // Retry only transient failures. Jitter prevents synchronized retry
        // storms if several shards fail together. Never sleep beyond deadline.
        attempt = attempt.saturating_add(1);
        let exp = (1_u64 << attempt.min(4)).saturating_mul(100);
        let jitter = u64::from(uuid::Uuid::new_v4().as_bytes()[0]) % 125;
        let wait = Duration::from_millis(exp + jitter);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(failure);
        }
        tokio::time::sleep(wait.min(remaining)).await;
    }
}

fn retryable_post_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 502 | 503 | 504)
}

/// Uses the receipt's monotonic clock. Missing timeout defaults to 30 seconds;
/// a supplied malformed timeout fails closed. Legitimate 30-90s deadlines are
/// honored, with longer remote timeouts conservatively capped at 90 seconds.
fn command_deadline(command: &Value, received_at: Instant) -> Option<Instant> {
    let duration = match command.get("response_timeout") {
        Some(raw) => parse_response_timeout(Some(raw))?,
        None => DEFAULT_COMMAND_LIFETIME,
    }
    .min(MAX_COMMAND_LIFETIME);
    received_at.checked_add(duration)
}

fn parse_response_timeout(value: Option<&Value>) -> Option<Duration> {
    let raw = value?.as_str()?;
    let split = raw.bytes().position(|b| !b.is_ascii_digit())?;
    if split == 0 {
        return None;
    }
    let number = raw[..split].parse::<u64>().ok()?;
    let multiplier: u64 = match &raw[split..] {
        "ns" => 1,
        "us" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60_000_000_000,
        "h" => 3_600_000_000_000,
        _ => return None,
    };
    Some(Duration::from_nanos(number.checked_mul(multiplier)?))
}

fn safe_token(value: Option<&Value>, maximum: usize) -> Option<&str> {
    let text = value?.as_str()?;
    if text.is_empty() || text.len() > maximum || !text.bytes().all(|b| (0x21..=0x7e).contains(&b))
    {
        return None;
    }
    Some(text)
}

/// Returns the full response JSON, opaque shard header, whether it was an
/// allowed successful tools/call, and the absolute timeout for the POST.
#[cfg(test)]
fn prepare_command(
    command: &Value,
    received_at: Instant,
) -> Option<(Value, String, bool, Instant)> {
    prepare_command_with_permissions(command, received_at, &AndroidMcpPermissions::default())
}

fn prepare_command_with_permissions(
    command: &Value,
    received_at: Instant,
    permissions: &AndroidMcpPermissions,
) -> Option<(Value, String, bool, Instant)> {
    prepare_command_with_dispatcher(command, received_at, permissions, None)
}

fn prepare_command_with_dispatcher(
    command: &Value,
    received_at: Instant,
    permissions: &AndroidMcpPermissions,
    plugins_handler: Option<&PluginRpcHandler>,
) -> Option<(Value, String, bool, Instant)> {
    let obj = command.as_object()?;
    let request_id = safe_token(obj.get("request_id"), 256)?;
    let shard_token = safe_token(obj.get("shard_token"), 2048)?;
    let channel = obj.get("channel").and_then(Value::as_str).unwrap_or("main");
    let deadline = command_deadline(command, received_at)?;
    if Instant::now() >= deadline {
        return None;
    }
    let command_type = obj.get("command_type").and_then(Value::as_str)?;
    match command_type {
        "session_termination" => Some((
            json!({"request_id":request_id,"channel":channel,"resp_code":204,
                "resp_type":"session_termination_response"}),
            shard_token.to_owned(),
            false,
            deadline,
        )),
        "jsonrpc" => {
            let rpc = obj.get("jsonrpc")?;
            if serde_json::to_vec(rpc).ok()?.len() > RPC_BODY_LIMIT {
                return None;
            }
            let reply = if channel == "main" {
                match plugins_handler {
                    Some(handler) => dispatch_core_and_plugins(rpc, deadline, permissions, handler),
                    None => handle_rpc_with_permissions(rpc, permissions),
                }
            } else {
                RpcReply {
                    payload: Some(rpc_error(
                        rpc.get("id").cloned().unwrap_or(Value::Null),
                        -32601,
                        "Unsupported MCP channel",
                    )),
                    successful_tool_call: false,
                }
            };
            let (body, real_call) = match reply.payload {
                Some(result) => (
                    json!({"request_id":request_id,"channel":channel,
                        "resp_json":result,"resp_headers":{"Content-Type":["application/json"]},
                        "resp_code":200,"resp_type":"jsonrpc_response"}),
                    reply.successful_tool_call,
                ),
                None => (
                    json!({"request_id":request_id,"channel":channel,
                        "resp_code":204,"resp_type":"notify_ack"}),
                    false,
                ),
            };
            Some((body, shard_token.to_owned(), real_call, deadline))
        }
        // Unknown discriminator must not be treated as JSON-RPC.
        _ => None,
    }
}

/// One public MCP endpoint, two internal tool registries. Core retains its
/// original fixed names and permission checks; plugin aliases are discovered
/// only from the enabled, verified registry. No plugin can shadow a Core tool.
fn dispatch_core_and_plugins(
    rpc: &Value,
    deadline: Instant,
    permissions: &AndroidMcpPermissions,
    plugins_handler: &PluginRpcHandler,
) -> RpcReply {
    match rpc.get("method").and_then(Value::as_str) {
        Some("tools/list") => {
            let mut core=handle_rpc_with_permissions(rpc,permissions);
            let Some(core_payload)=core.payload.as_mut() else {return core};
            let Some(core_list)=core_payload["result"]["tools"].as_array_mut() else {return core};
            let extra=plugins_handler(rpc,deadline);
            if let Some(additional)=extra.payload.as_ref()
                .and_then(|payload|payload["result"]["tools"].as_array())
            {
                // Keep a bounded, collision-free catalog. If plugins cannot
                // provide a valid list, Core keeps functioning independently.
                let mut names=core_list.iter()
                    .filter_map(|tool|tool["name"].as_str())
                    .collect::<std::collections::HashSet<_>>();
                let valid=additional.len()<=16 && additional.iter().all(|tool|{
                    let Some(name)=tool["name"].as_str() else{return false};
                    name.starts_with("p_") && name.len()<=64 &&
                        name.bytes().all(|b|b.is_ascii_alphanumeric() || b==b'_') &&
                        names.insert(name) && tool["inputSchema"].is_object()
                });
                if valid && core_list.len()+additional.len()<=32 {
                    core_list.extend(additional.iter().cloned());
                }
            }
            core
        }
        Some("tools/call") => {
            let name=rpc["params"]["name"].as_str().unwrap_or("");
            if matches!(name,"app_info"|"echo"|"files/browse"|"files/search"|
                "files/read"|"files/metadata"|"files/create"|"files/edit"|
                "files/apply_patch"|"files/move"|"files/delete"|"terminal/exec") {
                handle_rpc_with_permissions(rpc,permissions)
            }else{
                // Plugins independently validate exact aliases, schema, live
                // OS receipts, tunnel/session IDs and Android screen focus.
                plugins_handler(rpc,deadline)
            }
        }
        _=>handle_rpc_with_permissions(rpc,permissions),
    }
}

pub(crate) struct RpcReply {
    pub(crate) payload: Option<Value>,
    pub(crate) successful_tool_call: bool,
}

fn rpc_error(id: Value, code: i64, message: &'static str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn rpc_result(id: Value, result: Value) -> RpcReply {
    RpcReply {
        payload: Some(json!({"jsonrpc":"2.0","id":id,"result":result})),
        successful_tool_call: false,
    }
}

/// Localhost MCP uses this same restricted, unprivileged app_info/echo
/// dispatcher. It is deliberately supplied a fresh default-deny gate; the
/// separately configured outbound OpenAI tunnel passes its shared live gate.
pub(crate) fn handle_rpc(rpc: &Value) -> RpcReply {
    handle_rpc_with_permissions(rpc, &AndroidMcpPermissions::default())
}

fn handle_rpc_with_permissions(rpc: &Value, permissions: &AndroidMcpPermissions) -> RpcReply {
    let Some(obj) = rpc.as_object() else {
        return RpcReply {
            payload: Some(rpc_error(Value::Null, -32600, "Invalid Request")),
            successful_tool_call: false,
        };
    };
    let id = obj.get("id").cloned().unwrap_or(Value::Null);
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || !obj.get("method").is_some_and(Value::is_string)
        || obj
            .get("id")
            .is_some_and(|id| !id.is_string() && !id.is_number())
        || obj
            .get("id")
            .is_some_and(|id| id.is_number() && id.as_i64().is_none() && id.as_u64().is_none())
        || id.as_str().is_some_and(|id| id.len() > 128)
    {
        return RpcReply {
            payload: Some(rpc_error(Value::Null, -32600, "Invalid Request")),
            successful_tool_call: false,
        };
    }
    let method = obj["method"].as_str().unwrap_or_default();
    // Never execute tool-call notifications. A request with no id is only
    // acknowledged; side effects are never permitted through this path.
    if !obj.contains_key("id") {
        return RpcReply {
            payload: None,
            successful_tool_call: false,
        };
    }
    let params = obj.get("params").unwrap_or(&Value::Null);
    match method {
        "initialize" => {
            if !params.is_object() {
                return RpcReply {
                    payload: Some(rpc_error(id, -32602, "Invalid params")),
                    successful_tool_call: false,
                };
            }
            let requested = params.get("protocolVersion").and_then(Value::as_str);
            let version = match requested {
                Some("2024-11-05") => "2024-11-05",
                Some("2025-03-26") => "2025-03-26",
                Some("2025-06-18") => "2025-06-18",
                _ => "2025-06-18",
            };
            rpc_result(
                id,
                json!({
                    "protocolVersion":version,
                    "capabilities":{"tools":{"listChanged":false}},
                    "serverInfo":{"name":"mcp-agent-ai-android","version":env!("CARGO_PKG_VERSION")}
                }),
            )
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => {
            // MCP clients may attach per-request _meta (including ChatGPT
            // locale, session and protocol hints). This metadata is neither
            // a tool argument nor an authorization grant; ignore its content.
            if !params.is_null()
                && !params.as_object().is_some_and(|p| {
                    p.keys().all(|key| key == "_meta")
                        && p.get("_meta").is_none_or(Value::is_object)
                })
            {
                return RpcReply {
                    payload: Some(rpc_error(id, -32602, "Invalid params")),
                    successful_tool_call: false,
                };
            }
            // Never advertise an unavailable tool. This list is computed from
            // the same freshly verified OS root + fine-grained user policy
            // used by tools/call, not just the selected preset label.
            let mut tools = vec![
                json!({"name":"app_info","description":"Return a fixed description of the Android app and its read-only MCP capabilities.",
                    "inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
                json!({"name":"echo","description":"Echo at most 1024 characters of text; does not access files, shell, or the network.",
                    "inputSchema":{"type":"object","properties":{"text":{"type":"string","maxLength":1024}},
                        "required":["text"],"additionalProperties":false}}),
            ];
            let file_catalog = permissions.effective_file_catalog();
            if file_catalog[0] {
                tools.push(json!({"name":"files/browse",
                    "description":"List at most 96 entries inside the freshly authorized Android folder, or the app-private MCP share. Read-only; no symlinks or Android/data.",
                    "inputSchema":{"type":"object","properties":{"path":{"type":"string","maxLength":512}},
                        "additionalProperties":false}}));
            }
            if file_catalog[1] {
                tools.push(json!({"name":"files/search",
                    "description":"Find filenames inside the authorized folder. Bounded to 320 entries, four subdirectory levels, and 32 matches.",
                    "inputSchema":{"type":"object","properties":{"path":{"type":"string","maxLength":512},
                        "query":{"type":"string","minLength":2,"maxLength":64}},
                        "required":["query"],"additionalProperties":false}}));
            }
            if file_catalog[2] {
                tools.push(json!({"name":"files/read",
                    "description":"Read one UTF-8 text file up to 12 KiB inside the authorized Android or isolated app-private folder. Never binary, linked or protected paths.",
                    "inputSchema":{"type":"object","properties":{"path":{"type":"string","maxLength":512}},
                        "required":["path"],"additionalProperties":false}}));
            }
            if file_catalog[3] {
                tools.push(json!({"name":"files/metadata",
                    "description":"Read bounded file metadata within the verified folder without revealing host paths.",
                    "inputSchema":{"type":"object","properties":{"path":{"type":"string","maxLength":512}},
                        "additionalProperties":false}}));
            }
            let writes=permissions.effective_write_catalog();
            if writes[0] {
                tools.push(json!({"name":"files/create",
                    "description":"Atomically create one new UTF-8 file <=12 KiB inside the currently approved Android MCP folder. No overwrite, absolute paths, hidden files or symlinks.",
                    "inputSchema":{"type":"object","properties":{
                        "path":{"type":"string","maxLength":512},
                        "content":{"type":"string","maxLength":12288}},
                        "required":["path","content"],"additionalProperties":false}}));
            }
            if writes[1] {
                tools.push(json!({"name":"files/edit",
                    "description":"Atomically replace a UTF-8 file <=12 KiB only if its entire current contents equal expectedText; prevents blind overwrites.",
                    "inputSchema":{"type":"object","properties":{
                        "path":{"type":"string","maxLength":512},
                        "expectedText":{"type":"string","maxLength":12288},
                        "content":{"type":"string","maxLength":12288}},
                        "required":["path","expectedText","content"],"additionalProperties":false}}));
                tools.push(json!({"name":"files/apply_patch",
                    "description":"Apply exactly one unique literal text replacement to a UTF-8 file <=12 KiB in the approved folder; oldText must occur once. Not a shell command or multi-file unified diff.",
                    "inputSchema":{"type":"object","properties":{
                        "path":{"type":"string","maxLength":512},
                        "oldText":{"type":"string","minLength":1,"maxLength":12288},
                        "newText":{"type":"string","maxLength":12288}},
                        "required":["path","oldText","newText"],"additionalProperties":false}}));
            }
            if writes[2] {
                tools.push(json!({"name":"files/move",
                    "description":"Atomically move/rename a regular UTF-8 file <=12 KiB inside the same approved folder; never overwrite the destination.",
                    "inputSchema":{"type":"object","properties":{
                        "from":{"type":"string","maxLength":512},
                        "to":{"type":"string","maxLength":512}},
                        "required":["from","to"],"additionalProperties":false}}));
            }
            if writes[3] {
                tools.push(json!({"name":"files/delete",
                    "description":"Permanently remove one small regular UTF-8 file from the approved folder; irreversible and never touches directories or hidden files.",
                    "inputSchema":{"type":"object","properties":{
                        "path":{"type":"string","maxLength":512}},
                        "required":["path"],"additionalProperties":false}}));
            }
            if permissions.permits_remote_tool("terminal/exec") {
                tools.push(json!({"name":"terminal/exec",
                    "description":"Execute ONLY virtual read-only pwd/ls/cat/stat against the approved MCP folder, subject to user-saved Allowlist/Denylist. No shell, git, processes, Termux or arbitrary commands.",
                    "inputSchema":{"type":"object","properties":{
                        "command":{"type":"string","enum":["pwd","ls","cat","stat"]},
                        "path":{"type":"string","maxLength":512}},
                        "required":["command"],"additionalProperties":false}}));
            }
            rpc_result(id, json!({"tools":tools}))
        }
        "tools/call" => {
            let Some(p) = params.as_object() else {
                return RpcReply {
                    payload: Some(rpc_error(id, -32602, "Invalid params")),
                    successful_tool_call: false,
                };
            };
            let Some(name) = p.get("name").and_then(Value::as_str) else {
                return RpcReply {
                    payload: Some(rpc_error(id, -32602, "Invalid params")),
                    successful_tool_call: false,
                };
            };
            if p.keys()
                .any(|key| key != "name" && key != "arguments" && key != "_meta")
                || !p.get("_meta").is_none_or(Value::is_object)
            {
                return RpcReply {
                    payload: Some(rpc_error(id, -32602, "Invalid params")),
                    successful_tool_call: false,
                };
            }
            // CallToolRequest.arguments is optional in MCP. Missing arguments
            // mean an empty object for tools such as app_info. Supplied values
            // must still be objects; echo requires its validated text field.
            let empty_args = serde_json::Map::new();
            let args = match p.get("arguments") {
                Some(Value::Object(args)) => args,
                None => &empty_args,
                _ => {
                    return RpcReply {
                        payload: Some(rpc_error(id, -32602, "Invalid params")),
                        successful_tool_call: false,
                    }
                }
            };
            // Every privileged tool is gated at call-time by an OS-scoped
            // root and an explicitly saved fine-grained native permission.
            if !permissions.permits_remote_tool(name) {
                return RpcReply {
                    payload: Some(rpc_error(id, -32602, "Unknown or unavailable remote tool")),
                    successful_tool_call: false,
                };
            }
            if matches!(
                name,
                "files/browse" | "files/search" | "files/read" | "files/metadata"
            ) {
                return match permissions.execute_read_tool(name, args) {
                    Ok(output) => {
                        let mut reply = rpc_result(
                            id,
                            json!({
                                "content":[{"type":"text","text":output}],"isError":false
                            }),
                        );
                        // A read receipt is counted as real only when its
                        // bounded response is subsequently accepted by OpenAI.
                        reply.successful_tool_call = true;
                        reply
                    }
                    Err(reason) => RpcReply {
                        payload: Some(json!({"jsonrpc":"2.0","id":id,
                            "result":{"content":[{"type":"text","text":reason}],
                                "isError":true}})),
                        successful_tool_call: false,
                    },
                };
            }
            if matches!(name,"files/create"|"files/edit"|"files/apply_patch"|
                "files/move"|"files/delete"|"terminal/exec") {
                let receipt=if name=="terminal/exec" {
                    permissions.execute_terminal_tool(args)
                }else{
                    permissions.execute_write_tool(name,args)
                };
                return match receipt {
                    Ok(output)=>{
                        let mut reply=rpc_result(id,json!({"content":[{"type":"text",
                            "text":output}],"isError":false}));
                        reply.successful_tool_call=true;
                        reply
                    },
                    Err(reason)=>RpcReply{payload:Some(json!({"jsonrpc":"2.0","id":id,
                        "result":{"content":[{"type":"text","text":reason}],"isError":true}})),
                        successful_tool_call:false},
                };
            }
            let output = match name {
                "app_info" if args.is_empty() => json!({
                    "name":"Mcp Agent Ai","platform":"Android",
                    "capabilities":["app_info","echo"],"privilegedAccess":false
                })
                .to_string(),
                "echo" if args.len() == 1 => match args.get("text").and_then(Value::as_str) {
                    Some(text) if text.chars().count() <= 1024 => text.to_owned(),
                    _ => {
                        return RpcReply {
                            payload: Some(rpc_error(id, -32602, "Invalid params")),
                            successful_tool_call: false,
                        }
                    }
                },
                _ => {
                    return RpcReply {
                        payload: Some(rpc_error(id, -32602, "Unknown or invalid tool")),
                        successful_tool_call: false,
                    }
                }
            };
            let mut reply = rpc_result(
                id,
                json!({"content":[{"type":"text","text":output}],"isError":false}),
            );
            reply.successful_tool_call = true;
            reply
        }
        _ => RpcReply {
            payload: Some(rpc_error(id, -32601, "Method not found")),
            successful_tool_call: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_core_tunnel_exposes_separate_core_and_plugin_tools_without_shadowing() {
        let location = std::env::temp_dir().join(format!("mcp-tunnel-surface-{}", uuid::Uuid::new_v4()));
        let core = AndroidOpenAiTunnel::with_permissions_and_data_dir(
            AndroidMcpPermissions::default(), location.clone());
        let plugins_handler: PluginRpcHandler = Arc::new(|rpc, _deadline| {
            let id = rpc.get("id").cloned().unwrap_or(Value::Null);
            if rpc.get("method").and_then(Value::as_str) == Some("tools/list") {
                rpc_result(id, json!({"tools":[{"name":"p_test_b_readonly", "inputSchema":{"type":"object"}}]}))
            } else if rpc["params"]["name"]=="p_test_b_readonly" {
                let mut result=rpc_result(id,json!({"content":[{"type":"text","text":"plugin-ok"}],"isError":false}));
                result.successful_tool_call=true;
                result
            } else {
                RpcReply { payload: Some(rpc_error(id,-32601,"Plugin-only method")), successful_tool_call:false }
            }
        });
        core.attach_plugins_handler(plugins_handler.clone()).unwrap();
        assert!(core.attach_plugins_handler(plugins_handler.clone()).is_err(),
            "The same tunnel cannot install two competing plugin registries");
        assert!(core.saved_id_path.as_ref().unwrap().ends_with(CORE_TUNNEL_FILENAME));
        assert!(core.desired_path.as_ref().unwrap().ends_with(DESIRED_TUNNEL_FILENAME));
        let rpc = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        let core_reply = handle_rpc_with_permissions(&rpc, &AndroidMcpPermissions::default());
        let plugin_reply = plugins_handler(&rpc, Instant::now() + Duration::from_secs(30));
        assert_eq!(core_reply.payload.as_ref().unwrap()["result"]["tools"][0]["name"],"app_info");
        assert_eq!(plugin_reply.payload.as_ref().unwrap()["result"]["tools"][0]["name"],"p_test_b_readonly");
        let raw = json!({"request_id":"test","shard_token":"opaque123","channel":"main",
            "command_type":"jsonrpc","jsonrpc":rpc});
        let processed = prepare_command_with_dispatcher(
            &raw, Instant::now(), &AndroidMcpPermissions::default(), Some(&plugins_handler))
            .expect("valid unified Core/Plugins command");
        let tools=processed.0["resp_json"]["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|tool|tool["name"]=="app_info"));
        assert!(tools.iter().any(|tool|tool["name"]=="echo"));
        assert!(tools.iter().any(|tool|tool["name"]=="p_test_b_readonly"));
        assert!(!processed.2,"Listing is not a verified tool call");
        let native=cmd(json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"p_test_b_readonly","arguments":{}}}));
        let plugin_call=prepare_command_with_dispatcher(&native,Instant::now(),
            &AndroidMcpPermissions::default(),Some(&plugins_handler)).unwrap();
        assert!(plugin_call.2);
        assert_eq!(plugin_call.0["resp_json"]["result"]["content"][0]["text"],"plugin-ok");
        let core_call=cmd(json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"echo","arguments":{"text":"core-ok"}}}));
        let result=prepare_command_with_dispatcher(&core_call,Instant::now(),
            &AndroidMcpPermissions::default(),Some(&plugins_handler)).unwrap();
        assert!(result.2);
        assert_eq!(result.0["resp_json"]["result"]["content"][0]["text"],"core-ok");
        let collision:PluginRpcHandler=Arc::new(|rpc,_|{
            rpc_result(rpc["id"].clone(),json!({"tools":[{"name":"app_info","inputSchema":{"type":"object"}}]}))
        });
        let listing=dispatch_core_and_plugins(&rpc,Instant::now()+Duration::from_secs(10),
            &AndroidMcpPermissions::default(),&collision);
        assert_eq!(listing.payload.unwrap()["result"]["tools"].as_array().unwrap().len(),2,
            "A plugin must never shadow Core app_info");
    }

    fn cmd(rpc: Value) -> Value {
        json!({"request_id":"req_opaque","shard_token":"opaqueShard012",
            "command_type":"jsonrpc","channel":"main","jsonrpc":rpc})
    }

    fn read_mock_http(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        // Windows can inherit O_NONBLOCK from the nonblocking test listener.
        // The mock reads a complete HTTP request and uses a bounded socket
        // timeout instead; reset accepted sockets to blocking first.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let mut received = Vec::new();
        let mut part = [0u8; 2048];
        loop {
            let n = stream.read(&mut part).unwrap();
            assert!(n > 0);
            received.extend_from_slice(&part[..n]);
            let Some(end) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&received[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|val| val.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if received.len() >= end + 4 + length {
                return String::from_utf8(received).unwrap();
            }
        }
    }

    fn write_mock_http(stream: &mut std::net::TcpStream, status: u16, payload: &str) {
        use std::io::Write;
        write!(
            stream,
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        )
        .unwrap();
        stream.flush().unwrap();
    }

    fn mock_endpoint(listener: &std::net::TcpListener) -> String {
        format!(
            "http://{}/v1/tunnels/tunnel_012345678901234567890123456789ab",
            listener.local_addr().unwrap()
        )
    }

    fn mock_client() -> Client {
        Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(4))
            .build()
            .unwrap()
    }

    #[test]
    fn read_only_stored_access_receipt_never_returns_credentials_or_tool_proof() {
        let id = "tunnel_0123456789abcdef0123456789abcdef";
        assert_eq!(
            stored_access_receipt(Some(id), "ok"),
            json!({"verified":true,"tunnelId":id,"reason":"ok"})
        );
        for reason in [
            "id_required",
            "key_required",
            "id_changed",
            "unauthorized",
            "not_found",
            "network_error",
            "invalid_metadata",
            "service_error",
        ] {
            let receipt = stored_access_receipt(Some(id), reason);
            assert_eq!(receipt["verified"], false);
            assert_eq!(receipt["reason"], reason);
            assert_eq!(receipt.as_object().unwrap().len(), 3);
            assert!(receipt.get("toolCallsVerified").is_none());
            assert!(receipt.get("apiKey").is_none());
        }
        assert_eq!(
            stored_access_receipt(None, "id_required"),
            json!({"verified":false,"tunnelId":null,"reason":"id_required"})
        );
    }

    #[tokio::test]
    async fn stored_access_https_probe_requires_matching_live_metadata() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = mock_endpoint(&listener);
        let service = std::thread::spawn(move || {
            let mut observed = Vec::new();
            for (status, body) in [
                (200, r#"{"id":"tunnel_0123456789abcdef0123456789abcdef"}"#),
                (200, r#"{"id":"tunnel_ffffffffffffffffffffffffffffffff"}"#),
                (403, r#"{"error":{"message":"not authorized"}}"#),
            ] {
                let (mut connection, _) = listener.accept().unwrap();
                observed.push(read_mock_http(&mut connection));
                write_mock_http(&mut connection, status, body);
            }
            observed
        });
        let client = mock_client();
        let tunnel_id = "tunnel_0123456789abcdef0123456789abcdef";
        let key = "sk-loopback-test-secret";
        assert_eq!(
            verify_tunnel_metadata(&client, &endpoint, tunnel_id, key).await,
            "ok"
        );
        assert_eq!(
            verify_tunnel_metadata(&client, &endpoint, tunnel_id, key).await,
            "invalid_metadata"
        );
        assert_eq!(
            verify_tunnel_metadata(&client, &endpoint, tunnel_id, key).await,
            "unauthorized"
        );
        let observed = service.join().unwrap();
        assert_eq!(observed.len(), 3);
        for request in observed {
            assert!(request.starts_with("GET /v1/tunnels/tunnel_"));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-loopback-test-secret"));
            assert!(!request.contains("/response"));
            assert!(!request.contains("/poll"));
        }
    }

    #[test]
    fn tunnel_id_rejects_host_injection_and_accepts_legacy_alias() {
        assert!(valid_tunnel_id("tunnel_012345678901234567890123456789ab"));
        assert!(valid_tunnel_id(
            "tunnel_ab12_012345678901234567890123456789ab"
        ));
        for candidate in [
            "tunnel_ab",
            "tunnel_ABCDEF01234567890123456789012345",
            "tunnel_ab12_012345678901234567890123456789ab/../poll",
            "https://example.com",
            "tunnel_ab12_012345678901234567890123456789ab?x=1",
        ] {
            assert!(!valid_tunnel_id(candidate));
        }
    }

    #[tokio::test]
    async fn tunnel_id_check_never_claims_authorization_or_connection() {
        assert_eq!(
            android_tunnel_check_id("tunnel_012345678901234567890123456789ab".into())
                .await
                .unwrap(),
            json!({"valid":true,"formatOnly":true})
        );
        for invalid in [
            "",
            "tunnel_ab",
            "tunnel_a\r\nX:injected",
            "https://evil.test",
        ] {
            assert_eq!(
                android_tunnel_check_id(invalid.into()).await.unwrap(),
                json!({"valid":false,"formatOnly":true})
            );
        }
    }

    #[test]
    fn credentials_reject_injected_http_headers() {
        assert!(valid_api_key("sk-abcdefgh"));
        assert!(!valid_api_key("sk-abc\r\nX-Injected: 1"));
        assert!(!valid_api_key("sk-abc def"));
    }

    #[test]
    fn initialize_and_catalog_are_real_mcp_jsonrpc() {
        let init = handle_rpc(&json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-06-18","capabilities":{}}}));
        assert_eq!(
            init.payload.as_ref().unwrap()["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        assert_eq!(
            init.payload.as_ref().unwrap()["result"]["protocolVersion"],
            "2025-06-18"
        );
        let listing = handle_rpc(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
        let tools = listing.payload.unwrap()["result"]["tools"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "app_info");
        assert_eq!(tools[1]["name"], "echo");
    }

    #[test]
    fn full_permission_intent_never_exposes_privileged_remote_routes() {
        // This is only an explicit in-memory preference, not an implemented
        // file/PTY/browser MCP tool. The central gate and RPC dispatcher must
        // both fail closed, without creating a successful tools/call receipt.
        let permissions = AndroidMcpPermissions::default();
        let requested = json!({
            "browseFolders":true,"searchFiles":true,"readFiles":true,
            "fileMetadata":true,"createFiles":true,"editFiles":true,
            "moveFiles":true,"deleteFiles":true,"browserControl":true,
            "terminal":true
        });
        let status = permissions.configure("full", requested, true).unwrap();
        assert_eq!(status["preset"], "full");
        assert_eq!(status["effective"]["terminal"], false);
        let listing = handle_rpc_with_permissions(
            &json!({"jsonrpc":"2.0","id":40,"method":"tools/list"}),
            &permissions,
        );
        assert_eq!(
            listing.payload.unwrap()["result"]["tools"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        for name in [
            "files/read",
            "files/create",
            "files/delete",
            "browser/control",
            "terminal/exec",
            "terminal_run",
        ] {
            let request = json!({"jsonrpc":"2.0","id":41,"method":"tools/call",
                "params":{"name":name,"arguments":{"command":"whoami"}}});
            let reply = handle_rpc_with_permissions(&request, &permissions);
            assert!(!reply.successful_tool_call);
            assert_eq!(reply.payload.unwrap()["error"]["code"], -32602);
            let command = cmd(request);
            let (body, _, verified, _) =
                prepare_command_with_permissions(&command, Instant::now(), &permissions).unwrap();
            assert!(!verified);
            assert_eq!(body["resp_json"]["error"]["code"], -32602);
        }
        let safe = handle_rpc_with_permissions(
            &json!({"jsonrpc":"2.0","id":42,
            "method":"tools/call","params":{"name":"echo",
                "arguments":{"text":"still safe"}}}),
            &permissions,
        );
        assert!(safe.successful_tool_call);
        assert_eq!(
            safe.payload.unwrap()["result"]["content"][0]["text"],
            "still safe"
        );
    }

    #[test]
    fn real_file_tool_listing_and_call_require_live_root_and_permission() {
        let root =
            std::env::temp_dir().join(format!("mcp-tunnel-read-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("public.txt"), "actual Android MCP text").unwrap();
        let permissions = AndroidMcpPermissions::for_test_root(root.clone());

        let listing_request = json!({"jsonrpc":"2.0","id":61,"method":"tools/list"});
        let before = handle_rpc_with_permissions(&listing_request, &permissions);
        assert_eq!(
            before.payload.unwrap()["result"]["tools"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        let requested = json!({
            "browseFolders":true,"searchFiles":true,"readFiles":true,
            "fileMetadata":true,"createFiles":false,"editFiles":false,
            "moveFiles":false,"deleteFiles":false,"browserControl":false,
            "terminal":false
        });
        permissions.configure("read_only", requested, true).unwrap();
        let after = handle_rpc_with_permissions(&listing_request, &permissions);
        let tools = after.payload.unwrap()["result"]["tools"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(tools.len(), 6);
        assert_eq!(tools[2]["name"], "files/browse");
        assert_eq!(tools[3]["name"], "files/search");
        assert_eq!(tools[4]["name"], "files/read");
        assert_eq!(tools[5]["name"], "files/metadata");

        let read_request = json!({"jsonrpc":"2.0","id":62,"method":"tools/call",
            "params":{"name":"files/read","arguments":{"path":"public.txt"}}});
        let reply = handle_rpc_with_permissions(&read_request, &permissions);
        assert!(reply.successful_tool_call);
        let data: Value = serde_json::from_str(
            reply.payload.as_ref().unwrap()["result"]["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(data["text"], "actual Android MCP text");

        // The same native file call in the signed response envelope is
        // eligible for ToolVerified only on a successful OpenAI POST ACK.
        let (prepared, _, eligible, _) = prepare_command_with_permissions(
            &cmd(read_request.clone()),
            Instant::now(),
            &permissions,
        )
        .unwrap();
        assert!(eligible);
        assert_eq!(prepared["resp_json"]["result"]["isError"], false);

        let invalid = handle_rpc_with_permissions(
            &json!({"jsonrpc":"2.0","id":63,
            "method":"tools/call","params":{"name":"files/read",
                "arguments":{"path":"../private.json"}}}),
            &permissions,
        );
        assert!(!invalid.successful_tool_call);
        assert_eq!(invalid.payload.unwrap()["result"]["isError"], true);

        std::fs::remove_dir_all(root).unwrap();
        let revoked = handle_rpc_with_permissions(&read_request, &permissions);
        assert!(!revoked.successful_tool_call);
        assert_eq!(revoked.payload.unwrap()["error"]["code"], -32602);
        let list_after_revoke = handle_rpc_with_permissions(&listing_request, &permissions);
        assert_eq!(
            list_after_revoke.payload.unwrap()["result"]["tools"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn bounded_core_file_mutations_are_real_and_never_exposed_without_write_permission() {
        let root=std::env::temp_dir().join(format!("mcp-write-rpc-{}",uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let permissions=AndroidMcpPermissions::for_test_root(root.clone());
        let full=json!({
            "browseFolders":true,"searchFiles":true,"readFiles":true,
            "fileMetadata":true,"createFiles":true,"editFiles":true,
            "moveFiles":true,"deleteFiles":true,"browserControl":true,
            "terminal":true
        });
        let list_request=json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        let before=handle_rpc_with_permissions(&list_request,&permissions);
        let before_names=before.payload.unwrap()["result"]["tools"].as_array()
            .unwrap().iter().map(|v|v["name"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        assert!(!before_names.iter().any(|name|name=="files/apply_patch"));
        assert!(!before_names.iter().any(|name|name=="files/delete"));
        permissions.configure("full",full,true).unwrap();
        let listing=handle_rpc_with_permissions(&list_request,&permissions);
        let names=listing.payload.unwrap()["result"]["tools"].as_array()
            .unwrap().iter().map(|v|v["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        for name in ["files/create","files/edit","files/apply_patch",
            "files/move","files/delete"] {
            assert!(names.iter().any(|tool|tool==name),"{name} not advertised");
        }
        assert!(!names.iter().any(|tool|tool=="terminal/exec"),
            "Full MCP permission cannot bypass the separate command-rule configuration");
        let call=|name:&str,args:Value|{
            handle_rpc_with_permissions(&json!({"jsonrpc":"2.0","id":3,
                "method":"tools/call","params":{"name":name,"arguments":args}}),&permissions)
        };
        let created=call("files/create",json!({"path":"note.txt","content":"hello world"}));
        assert!(created.successful_tool_call,"{:?}",created.payload);
        assert_eq!(std::fs::read_to_string(root.join("note.txt")).unwrap(),"hello world");
        let incorrect=call("files/edit",json!({"path":"note.txt",
            "expectedText":"outdated","content":"unintended"}));
        assert!(!incorrect.successful_tool_call);
        assert_eq!(incorrect.payload.unwrap()["result"]["isError"],true);
        let edited=call("files/edit",json!({"path":"note.txt",
            "expectedText":"hello world","content":"hello Android"}));
        assert!(edited.successful_tool_call,"{:?}",edited.payload);
        let patched=call("files/apply_patch",json!({"path":"note.txt",
            "oldText":"Android","newText":"MCP"}));
        assert!(patched.successful_tool_call,"{:?}",patched.payload);
        let renamed=call("files/move",json!({"from":"note.txt","to":"renamed.txt"}));
        assert!(renamed.successful_tool_call,"{:?}",renamed.payload);
        assert_eq!(std::fs::read_to_string(root.join("renamed.txt")).unwrap(),"hello MCP");
        let deleted=call("files/delete",json!({"path":"renamed.txt"}));
        assert!(deleted.successful_tool_call,"{:?}",deleted.payload);
        assert!(!root.join("renamed.txt").exists());
        let traversal=call("files/create",json!({"path":"../secret.txt","content":"no"}));
        assert!(!traversal.successful_tool_call);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(!permissions.permits_remote_tool("files/create"),
            "A revoked or unavailable root fails closed at the next call");
    }

    #[test]
    fn echo_returns_exact_text_and_is_eligible_for_verification() {
        let echo = handle_rpc(&json!({"jsonrpc":"2.0","id":"rpc1",
            "method":"tools/call","params":{"name":"echo","arguments":{"text":"safe"}}}));
        assert!(echo.successful_tool_call);
        assert_eq!(
            echo.payload.unwrap()["result"]["content"][0]["text"],
            "safe"
        );
    }

    #[test]
    fn openai_tunnel_raw_jsonrpc_tool_calls_preserve_ids_and_only_count_valid_calls() {
        // The ChatGPT-visible mcp__Mcp_Agent_Ai_Core__* label is not part of
        // a standard MCP tools/call request. The tunnel wire envelope wraps
        // raw JSON-RPC; the inner name must match a tool in tools/list.
        for (rpc_id, tool, arguments, expected) in [
            (json!("rpc_echo"), "echo", json!({"text":"ping"}), "ping"),
            (json!(205), "app_info", json!({}), "Mcp Agent Ai"),
        ] {
            let mut command = cmd(json!({"jsonrpc":"2.0","id":rpc_id,
                "method":"tools/call","params":{"name":tool,"arguments":arguments}}));
            command["created_at"] = json!("2026-11-06T00:00:00Z");
            command["response_timeout"] = json!("30s");
            command["headers"] = json!({"Mcp-Session-Id":["session_123"]});
            let (response, shard, eligible, _) =
                prepare_command(&command, Instant::now()).expect("valid tunnel RPC");
            assert_eq!(shard, "opaqueShard012");
            assert_eq!(response["request_id"], "req_opaque");
            assert_eq!(response["channel"], "main");
            assert_eq!(response["resp_type"], "jsonrpc_response");
            assert_eq!(response["resp_json"]["jsonrpc"], "2.0");
            assert_eq!(response["resp_json"]["id"], rpc_id);
            assert_eq!(response["resp_json"]["result"]["isError"], false);
            assert!(response["resp_json"]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(expected));
            assert!(
                eligible,
                "{tool} must be eligible only after a successful response POST"
            );
        }
    }

    #[test]
    fn mcp_app_info_accepts_omitted_arguments_as_an_empty_input_object() {
        // MCP CallToolRequest.params.arguments is optional. ChatGPT may omit
        // it for a tool whose advertised inputSchema has no properties.
        // Regression: requiring a literal `arguments:{}` returns -32602 even
        // though app_info needs no input, leaving Step9 Pending.
        let response = handle_rpc(&json!({"jsonrpc":"2.0","id":"no_args",
            "method":"tools/call","params":{"name":"app_info"}}));
        assert!(
            response.successful_tool_call,
            "app_info with omitted optional arguments must succeed: {:?}",
            response.payload
        );
        let body = response.payload.unwrap();
        assert_eq!(body["id"], "no_args");
        assert_eq!(body["result"]["isError"], false);
        assert!(body["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Mcp Agent Ai"));
    }

    #[test]
    fn modern_mcp_request_metadata_does_not_invalidate_safe_tool_calls() {
        // Modern MCP transports may attach reserved `_meta` alongside the
        // advertised tool name and arguments. It is metadata, not a tool
        // argument; the permission gate and exact input schema remain intact.
        for (rpc_id, tool, args, expected) in [
            (json!("meta_echo"), "echo", json!({"text":"ping"}), "ping"),
            (json!("meta_info"), "app_info", json!({}), "Mcp Agent Ai"),
        ] {
            let command = cmd(json!({"jsonrpc":"2.0","id":rpc_id,
                "method":"tools/call","params":{"name":tool,"arguments":args,
                    "_meta":{"progressToken":"progress_123"}}}));
            let (response, shard, eligible, _) =
                prepare_command(&command, Instant::now()).expect("valid MCP envelope");
            assert_eq!(shard, "opaqueShard012");
            assert_eq!(response["resp_json"]["id"], rpc_id);
            assert_eq!(
                response["resp_json"]["result"]["isError"], false,
                "metadata-bearing {tool} was rejected: {response}"
            );
            assert!(response["resp_json"]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(expected));
            assert!(
                eligible,
                "validated {tool} must remain eligible for POST ACK proof"
            );
        }
    }

    #[test]
    fn modern_mcp_tools_list_metadata_does_not_hide_the_safe_catalog() {
        // Some MCP versions attach `_meta` to tools/list too; this must not
        // advertise or authorize privileged tools or create a tool-call proof.
        let command = cmd(json!({"jsonrpc":"2.0","id":"list_meta",
            "method":"tools/list","params":{"_meta":{"progressToken":7}}}));
        let (response, _, eligible, _) =
            prepare_command(&command, Instant::now()).expect("valid tools/list envelope");
        assert!(!eligible);
        let tools = response["resp_json"]["result"]["tools"]
            .as_array()
            .expect("metadata-bearing tools/list must return the actual catalog");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "app_info");
        assert_eq!(tools[1]["name"], "echo");
    }

    #[test]
    fn tool_names_must_be_exact_and_untrusted_aliases_never_bypass_permission_gate() {
        for alias in [
            "mcp__Mcp_Agent_Ai_Core__echo",
            "mcp__Mcp_Agent_Ai_Core__app_info",
            "Mcp_Agent_Ai_Core.echo",
            "tools/echo",
            "echo/../files/read",
        ] {
            let reply = handle_rpc(&json!({"jsonrpc":"2.0","id":"alias",
                "method":"tools/call","params":{"name":alias,
                    "arguments":{"text":"ping"}}}));
            assert!(
                !reply.successful_tool_call,
                "unadvertised alias {alias} executed"
            );
            assert_eq!(
                reply.payload.unwrap()["error"]["message"],
                "Unknown or unavailable remote tool"
            );
        }
    }

    #[test]
    fn metadata_cannot_substitute_for_arguments_or_authorize_unadvertised_tools() {
        // Accepting a benign object-valued `_meta` must not make malformed
        // metadata, hidden tool input, or a privileged route executable.
        for invalid_meta in [
            json!(null),
            json!(false),
            json!("progress_123"),
            json!([1, 2]),
        ] {
            let reply = handle_rpc(&json!({"jsonrpc":"2.0","id":70,
                "method":"tools/call","params":{"name":"echo",
                    "arguments":{"text":"ping"},"_meta":invalid_meta}}));
            assert!(
                !reply.successful_tool_call,
                "non-object MCP metadata must not execute a tool: {invalid_meta}"
            );
            assert_eq!(reply.payload.unwrap()["error"]["code"], -32602);
        }
        for (name, args) in [
            ("terminal/exec", json!({"command":"whoami"})),
            ("files/delete", json!({"path":"private.txt"})),
            ("echo", json!({"command":"whoami"})),
            ("app_info", json!({"text":"ping"})),
        ] {
            let reply = handle_rpc(&json!({"jsonrpc":"2.0","id":71,
                "method":"tools/call","params":{"name":name,"arguments":args,
                    "_meta":{"progressToken":77}}}));
            assert!(
                !reply.successful_tool_call,
                "metadata must not elevate {name} with {args}"
            );
            assert!(reply.payload.unwrap().get("error").is_some());
        }
    }

    #[test]
    fn malformed_optional_arguments_and_nonstandard_jsonrpc_envelopes_fail_closed() {
        for params in [
            json!({"name":"echo"}), // echo requires a text string
            json!({"name":"echo","arguments":{}}),
            json!({"name":"echo","arguments":null}),
            json!({"name":"app_info","arguments":null}),
            json!({"name":"app_info","arguments":{"text":"ping"}}),
            json!({"name":"echo","arguments":{"text":"ping","command":"whoami"}}),
            json!({"name":"echo","arguments":{"text":"ping"},"tool":"app_info"}),
        ] {
            let reply = handle_rpc(&json!({"jsonrpc":"2.0","id":8,
                "method":"tools/call","params":params}));
            assert!(
                !reply.successful_tool_call,
                "malformed params unexpectedly executed: {params}"
            );
            assert!(reply.payload.unwrap().get("error").is_some());
        }
        // The tunnel client's command_type discriminator and nested raw RPC
        // are required; do not treat arbitrary top-level objects as commands.
        let raw = json!({"jsonrpc":"2.0","id":9,"method":"tools/call",
            "params":{"name":"echo","arguments":{"text":"ping"}}});
        assert!(prepare_command(&raw, Instant::now()).is_none());
        let mut command = cmd(raw.clone());
        command["command_type"] = json!("tools/call");
        assert!(prepare_command(&command, Instant::now()).is_none());
        command["command_type"] = json!("jsonrpc");
        command["jsonrpc"] = json!({"request":raw});
        let (response, _, eligible, _) = prepare_command(&command, Instant::now()).unwrap();
        assert_eq!(response["resp_json"]["error"]["code"], -32600);
        assert!(!eligible);
    }

    #[test]
    fn unknown_and_privileged_methods_are_inert() {
        for method in [
            "terminal/run",
            "tools/execute",
            "shell",
            "files/delete",
            "resources/read",
        ] {
            let reply = handle_rpc(&json!({"jsonrpc":"2.0","id":17,"method":method,
                "params":{"command":"rm -rf /"}}));
            assert!(!reply.successful_tool_call);
            assert_eq!(reply.payload.unwrap()["error"]["code"], -32601);
        }
        let reply = handle_rpc(&json!({"jsonrpc":"2.0","id":18,"method":"tools/call",
            "params":{"name":"terminal_run","arguments":{"command":"whoami"}}}));
        assert_eq!(reply.payload.unwrap()["error"]["code"], -32602);
    }

    #[test]
    fn malformed_tool_input_cannot_trigger_verification() {
        for args in [
            json!({"text":"hi","command":"do something"}),
            json!({"text":42}),
            json!({"text":"a".repeat(1025)}),
            json!({}),
        ] {
            let reply = handle_rpc(&json!({"jsonrpc":"2.0","id":1,
                "method":"tools/call","params":{"name":"echo","arguments":args}}));
            assert!(!reply.successful_tool_call);
            assert_eq!(reply.payload.unwrap()["error"]["code"], -32602);
        }
    }

    #[test]
    fn no_id_tool_call_is_never_executed() {
        let reply = handle_rpc(&json!({"jsonrpc":"2.0","method":"tools/call",
            "params":{"name":"app_info","arguments":{}}}));
        assert!(reply.payload.is_none());
        assert!(!reply.successful_tool_call);
    }

    #[test]
    fn response_body_routes_opaque_id_and_shard_separately() {
        let (response, shard, verified, _) = prepare_command(
            &cmd(json!({
                "jsonrpc":"2.0","id":4,"method":"tools/call",
                "params":{"name":"app_info","arguments":{}}
            })),
            Instant::now(),
        )
        .unwrap();
        assert_eq!(response["request_id"], "req_opaque");
        assert_eq!(response["channel"], "main");
        assert_eq!(response["resp_json"]["id"], 4);
        assert_eq!(response["resp_type"], "jsonrpc_response");
        assert_eq!(shard, "opaqueShard012");
        assert!(response.get("shard_token").is_none());
        assert!(verified);
    }

    #[test]
    fn unknown_command_type_never_falls_through_to_jsonrpc() {
        let mut command = cmd(json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"app_info","arguments":{}}}));
        command["command_type"] = json!("execute_shell");
        assert!(prepare_command(&command, Instant::now()).is_none());
        command["command_type"] = json!("session_termination");
        let (ack, _, verified, _) = prepare_command(&command, Instant::now()).unwrap();
        assert_eq!(ack["resp_type"], "session_termination_response");
        assert!(!verified);
    }

    #[test]
    fn notifications_acknowledged_without_tool_run() {
        let (ack, _, verified, _) = prepare_command(
            &cmd(json!({
                "jsonrpc":"2.0","method":"notifications/initialized"
            })),
            Instant::now(),
        )
        .unwrap();
        assert_eq!(ack["resp_type"], "notify_ack");
        assert!(ack.get("resp_json").is_none());
        assert!(!verified);
    }

    #[test]
    fn channel_cannot_redirect_into_other_mcp() {
        let mut command = cmd(json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"echo","arguments":{"text":"hello"}}}));
        command["channel"] = json!("other");
        let (reply, _, verified, _) = prepare_command(&command, Instant::now()).unwrap();
        assert_eq!(reply["resp_json"]["error"]["code"], -32601);
        assert!(!verified);
    }

    #[test]
    fn timeout_parser_and_immediate_expiry_are_strict() {
        for (raw, expected) in [
            ("0s", Some(0)),
            ("4500ms", Some(4_500_000_000)),
            ("3us", Some(3000)),
            ("2m", Some(120_000_000_000)),
            ("4.5s", None),
            ("-1s", None),
            ("1s ", None),
            ("1d", None),
            ("999999999999999999999999h", None),
        ] {
            assert_eq!(
                parse_response_timeout(Some(&json!(raw))).map(|x| x.as_nanos()),
                expected
            );
        }
        let mut expired = cmd(json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
        expired["response_timeout"] = json!("0s");
        assert!(prepare_command(&expired, Instant::now()).is_none());
        let mut thirty = cmd(json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
        thirty["response_timeout"] = json!("30s");
        assert!(
            command_deadline(&thirty, Instant::now())
                .unwrap()
                .duration_since(Instant::now())
                > Duration::from_secs(29)
        );
        thirty["response_timeout"] = json!("120s");
        let remaining = command_deadline(&thirty, Instant::now())
            .unwrap()
            .duration_since(Instant::now());
        assert!(remaining > Duration::from_secs(89) && remaining <= MAX_COMMAND_LIFETIME);
        thirty["response_timeout"] = json!("bogus");
        assert!(prepare_command(&thirty, Instant::now()).is_none());
        thirty.as_object_mut().unwrap().remove("response_timeout");
        assert!(
            command_deadline(&thirty, Instant::now())
                .unwrap()
                .duration_since(Instant::now())
                > Duration::from_secs(29)
        );
    }

    #[test]
    fn status_only_marks_poll_and_remote_acknowledged_tool_calls() {
        let tunnel = AndroidOpenAiTunnel::default();
        let snapshot = tunnel.snapshot().unwrap();
        assert_eq!(snapshot["state"], "disconnected");
        assert_eq!(snapshot["pollActive"], false);
        let generation = 1;
        {
            let mut session = tunnel.session.lock().unwrap();
            session.generation = generation;
            session.phase = Phase::Connecting;
        }
        update_state(
            &tunnel.session,
            generation,
            Phase::Polling,
            None,
            true,
            false,
        );
        assert_eq!(tunnel.snapshot().unwrap()["state"], "polling");
        assert_eq!(tunnel.snapshot().unwrap()["toolCallsVerified"], 0);
        update_state(
            &tunnel.session,
            generation,
            Phase::ToolVerified,
            None,
            false,
            true,
        );
        assert_eq!(tunnel.snapshot().unwrap()["toolCallsVerified"], 1);
        assert_eq!(tunnel.snapshot().unwrap()["state"], "tool_verified");
        tunnel.disconnect().unwrap();
        update_state(
            &tunnel.session,
            generation,
            Phase::ToolVerified,
            None,
            true,
            true,
        );
        assert_eq!(tunnel.snapshot().unwrap()["state"], "disconnected");
        assert_eq!(tunnel.snapshot().unwrap()["toolCallsVerified"], 0);
    }

    #[test]
    fn unauthorized_and_expired_heartbeat_never_appear_healthy() {
        assert!(matches!(
            classify_response(StatusCode::UNAUTHORIZED),
            Err(TunnelFailure::Unauthorized)
        ));
        assert!(matches!(
            classify_response(StatusCode::FORBIDDEN),
            Err(TunnelFailure::Unauthorized)
        ));
        assert!(matches!(
            classify_response(StatusCode::BAD_REQUEST),
            Err(TunnelFailure::Rejected)
        ));
        assert!(matches!(
            classify_response(StatusCode::TOO_MANY_REQUESTS),
            Err(TunnelFailure::Network)
        ));
        let tunnel = AndroidOpenAiTunnel::default();
        {
            let mut session = tunnel.session.lock().unwrap();
            session.phase = Phase::ToolVerified;
            session.verified_calls = 1;
            session.last_poll = Some(Instant::now() - HEARTBEAT_LIMIT);
        }
        let status = tunnel.snapshot().unwrap();
        assert_eq!(status["state"], "error");
        assert_eq!(status["pollActive"], false);
        assert_eq!(status["toolCallsVerified"], 1);
        assert_eq!(status["lastError"], "Poll heartbeat expired");
    }

    #[test]
    fn oversized_or_malformed_rpc_and_shard_are_not_forwarded() {
        let big = cmd(json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"echo","arguments":{"text":"x".repeat(RPC_BODY_LIMIT)}}}));
        assert!(prepare_command(&big, Instant::now()).is_none());
        let mut forged = cmd(json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
        forged["shard_token"] = json!("bad\r\nAuthorization: other");
        assert!(prepare_command(&forged, Instant::now()).is_none());
        forged["shard_token"] = json!("opaqueShard012");
        forged["request_id"] = json!("../poll");
        // Opaque IDs are allowed; their value never determines a URL.
        let (response, _, _, _) = prepare_command(&forged, Instant::now()).unwrap();
        assert_eq!(response["request_id"], "../poll");
        let bad_id = handle_rpc(&json!({"jsonrpc":"2.0","id":1.5,"method":"ping"}));
        assert_eq!(bad_id.payload.unwrap()["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn loopback_poll_and_response_use_wire_contract_and_shard_header() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        // Test-only loopback server; production always sends to BASE_URL over HTTPS.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let service = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(4)))
                    .unwrap();
                let mut received = Vec::new();
                let mut part = [0u8; 2048];
                loop {
                    let count = stream.read(&mut part).unwrap();
                    assert!(count > 0);
                    received.extend_from_slice(&part[..count]);
                    let Some(header_end) = received.windows(4).position(|x| x == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&received[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if received.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
                requests.push(String::from_utf8(received).unwrap());
                let result = if index == 0 {
                    json!({"commands":[cmd(json!({"jsonrpc":"2.0","id":"rpc_1",
                        "method":"tools/call",
                        "params":{"name":"echo","arguments":{"text":"loopback"}}}))]})
                    .to_string()
                } else {
                    json!({"status":"ok"}).to_string()
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", result.len(), result).unwrap();
                stream.flush().unwrap();
            }
            requests
        });
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(4))
            .build()
            .unwrap();
        let endpoint =
            format!("http://{address}/v1/tunnels/tunnel_012345678901234567890123456789ab");
        let (commands, received_at) =
            poll_once(&client, &endpoint, "sk-local-test", "instance_test")
                .await
                .unwrap();
        assert_eq!(commands.len(), 1);
        let (response, shard, real_tool, deadline) =
            prepare_command(&commands[0], received_at).unwrap();
        assert!(real_tool);
        assert_eq!(
            response["resp_json"]["result"]["content"][0]["text"],
            "loopback"
        );
        post_response(
            &client,
            &endpoint,
            "sk-local-test",
            "instance_test",
            &shard,
            response,
            deadline,
        )
        .await
        .unwrap();
        let observed = service.join().unwrap();
        let poll = observed[0].to_ascii_lowercase();
        let posted = observed[1].to_ascii_lowercase();
        assert!(poll.starts_with("get /v1/tunnels/tunnel_"));
        assert!(poll.contains("/poll?limit=4&timeout_ms=15000"));
        assert!(poll.contains("authorization: bearer sk-local-test"));
        assert!(poll.contains("x-tunnel-mcp-server-info:"));
        assert!(posted.starts_with("post /v1/tunnels/tunnel_"));
        assert!(posted.contains("/response http/1.1"));
        assert!(posted.contains("x-tunnel-shard-token: opaqueshard012"));
        assert!(posted.contains("\"request_id\":\"req_opaque\""));
        assert!(!posted.contains("\"shard_token\""));
    }

    #[tokio::test]
    async fn post_retries_only_transient_statuses_with_exact_same_response() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = mock_endpoint(&listener);
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for status in [408, 429, 502, 503, 504, 200] {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_mock_http(&mut stream));
                write_mock_http(&mut stream, status, r#"{"status":"ok"}"#);
            }
            requests
        });
        let payload = json!({"request_id":"same-id","channel":"main","resp_type":"jsonrpc_response",
            "resp_code":200,"resp_json":{"jsonrpc":"2.0","id":1,"result":{"ok":true}}});
        post_response(
            &mock_client(),
            &endpoint,
            "sk-local-test",
            "instance_test",
            "same-shard",
            payload,
            Instant::now() + Duration::from_secs(15),
        )
        .await
        .unwrap();
        let observed = server.join().unwrap();
        assert_eq!(observed.len(), 6);
        let bodies: Vec<_> = observed
            .iter()
            .map(|raw| raw.split_once("\r\n\r\n").unwrap().1)
            .collect();
        assert!(bodies.windows(2).all(|window| window[0] == window[1]));
        assert!(observed.iter().all(|raw| {
            let lower = raw.to_ascii_lowercase();
            lower.contains("x-tunnel-shard-token: same-shard")
                && lower.contains("authorization: bearer sk-local-test")
        }));
    }

    #[tokio::test]
    async fn post_retries_transport_drop_without_recomputing_mcp_result() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = mock_endpoint(&listener);
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let original = read_mock_http(&mut first);
            drop(first); // close without a response, simulates a network interruption
            let (mut second, _) = listener.accept().unwrap();
            let retried = read_mock_http(&mut second);
            write_mock_http(&mut second, 200, r#"{"status":"ok"}"#);
            (original, retried)
        });
        let command = cmd(json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"echo","arguments":{"text":"calculate exactly once"}}}));
        let (body, shard, real_call, deadline) = prepare_command(&command, Instant::now()).unwrap();
        assert!(real_call);
        post_response(
            &mock_client(),
            &endpoint,
            "sk-local-test",
            "instance_test",
            &shard,
            body,
            deadline,
        )
        .await
        .unwrap();
        let (original, retried) = server.join().unwrap();
        assert_eq!(
            original.split_once("\r\n\r\n").unwrap().1,
            retried.split_once("\r\n\r\n").unwrap().1
        );
    }

    #[tokio::test]
    async fn terminal_401_403_404_do_not_retry() {
        use std::net::TcpListener;
        for status in [401, 403, 404] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = mock_endpoint(&listener);
            listener.set_nonblocking(true).unwrap();
            let server = std::thread::spawn(move || {
                let until = Instant::now() + Duration::from_millis(650);
                let mut seen = 0;
                while Instant::now() < until {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            seen += 1;
                            read_mock_http(&mut stream);
                            write_mock_http(&mut stream, status, "{}");
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("mock accept failed: {error}"),
                    }
                }
                seen
            });
            let result = post_response(
                &mock_client(),
                &endpoint,
                "sk-local-test",
                "instance_test",
                "same-shard",
                json!({"request_id":"one","channel":"main"}),
                Instant::now() + Duration::from_secs(2),
            )
            .await;
            assert!(
                result.is_err(),
                "terminal status {status} unexpectedly accepted"
            );
            assert_eq!(server.join().unwrap(), 1, "status {status} retried");
        }
    }

    #[tokio::test]
    async fn batch_failure_does_not_prevent_independent_tool_response() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = mock_endpoint(&listener);
        let server = std::thread::spawn(move || {
            let mut names = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_mock_http(&mut stream);
                let body = request.split_once("\r\n\r\n").unwrap().1;
                let id = serde_json::from_str::<Value>(body).unwrap()["request_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let status = if id == "first-fails" { 404 } else { 200 };
                names.push(id);
                write_mock_http(&mut stream, status, r#"{"status":"ok"}"#);
            }
            names
        });
        let mut first = cmd(json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
        first["request_id"] = json!("first-fails");
        let mut second = cmd(json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"app_info","arguments":{}}}));
        second["request_id"] = json!("second-works");
        let state = Arc::new(Mutex::new(Session::default()));
        {
            let mut session = state.lock().unwrap();
            session.generation = 1;
            session.phase = Phase::Polling;
            session.last_poll = Some(Instant::now());
        }
        let failure = process_batch(
            &state,
            &AndroidMcpPermissions::default(),
            None,
            None,
            1,
            &mock_client(),
            &endpoint,
            "sk-local-test",
            "instance_test",
            vec![first, second],
            Instant::now(),
        )
        .await;
        assert!(matches!(failure, Some(TunnelFailure::Rejected)));
        assert_eq!(state.lock().unwrap().verified_calls, 1);
        let observed = server.join().unwrap();
        assert!(observed.contains(&"first-fails".to_owned()));
        assert!(observed.contains(&"second-works".to_owned()));
    }
}
