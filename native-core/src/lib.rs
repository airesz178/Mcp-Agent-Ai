//! Shared Windows/Android Rust chat engine.
//! Only real provider endpoints are used. No mock responses or fake agents.
use reqwest::{header, Client};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs, io,
    fs::OpenOptions,
    path::PathBuf,
    sync::{atomic::{AtomicU64, Ordering}, Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;
mod workspace_tools;
mod mobile_skills;
mod standing_rules;
mod mcp_client;
mod mobile_agents;
mod mobile_ai_delegation;
mod mobile_prime;
mod mobile_goal;
mod goal_plan;
mod goal_review;
mod goal_mcp;
mod mobile_projects;
mod provider_stream;
mod provider_tools;
mod tool_audit;
mod local_agent_tools;
mod local_agent_audit;
mod local_agent_intent;
mod local_agent_runtime;
pub mod android_plugin_registry;
pub use local_agent_tools::AndroidLocalToolHost;

const MAX_RESPONSE: usize = 1024 * 1024;
const MAX_PROMPT: usize = 20_000;
const MAX_SESSIONS: usize = 25;
const CHATGPT_BROWSER_PROVIDER: &str = "chatgpt-browser";
const CHATGPT_BROWSER_MODEL: &str = "chatgpt.com";
// A browser answer is saved verbatim or rejected; it is never truncated.
const MAX_BROWSER_ANSWER: usize = 20_000;
// Keep every claimed request UUID for replay protection, including evicted
// sessions. On reaching this limit, fail closed instead of forgetting proofs.
const MAX_BROWSER_RECEIPTS: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub at: String,
    pub provider: String,
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub actions: Vec<Value>,
}

/// Only the native app-owned browser host may decide that a website receipt
/// is confirmed. Do not derive this state from a Tauri renderer/HTTP payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserTurnReceiptStatus {
    Confirmed,
    Pending,
    Failed,
}

/// Native-host verified user and assistant messages from one app-owned
/// chatgpt.com turn. Capture expected_session BEFORE dispatching the prompt;
/// never reconstruct it from the currently stored session after receiving a
/// website reply. The host must independently verify exact DOM receipt,
/// navigation and request identity before choosing Confirmed.
#[derive(Debug, Clone)]
pub struct BrowserChatTurnReceipt {
    pub request_id: String,
    pub session_id: Option<String>,
    /// Exact UUID from the native-owned ChatGPT conversation URL, if known.
    pub browser_conversation_id: Option<String>,
    pub prompt: String,
    pub answer: String,
    pub expected_session: Option<Session>,
    pub status: BrowserTurnReceiptStatus,
    pub used_mcp_tools: bool,
}

/// Only Tauri's authenticated Android native-plugin completion path may
/// construct this receipt. The JS renderer can provide a prompt/request UUID,
/// but never an answer, model proof, or claimed completion.
#[derive(Debug, Clone)]
pub struct OAuthChatTurnReceipt {
    pub request_id: String,
    pub session_id: Option<String>,
    pub prompt: String,
    pub answer: String,
    pub expected_session: Option<Session>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRequest {
    pub prompt: String,
    pub provider: String,
    pub model: String,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
    #[serde(default)]
    pub allow_mcp_tools: bool,
    #[serde(default)]
    pub allow_local_tools: bool,
    /// Ephemeral API-provider request preference. Never persisted as proof of
    /// actual model thinking, or accepted from a browser/OAuth receipt.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

fn apply_reasoning_effort(
    body: &mut Value,
    provider: &str,
    model: &str,
    selected: Option<&str>,
) -> Result<(), String> {
    let Some(effort) = selected else { return Ok(()) };
    if effort == "auto" { return Ok(()); }
    if !matches!(effort, "low" | "medium" | "high") {
        return Err("Unsupported reasoning effort; choose auto, low, medium or high".into());
    }
    match provider {
        "openai" if model.starts_with("gpt-5") || model.starts_with("gpt-6")
            || model.starts_with("o1") || model.starts_with("o3")
            || model.starts_with("o4") => {
            body["reasoning_effort"] = json!(effort);
        }
        "anthropic" if model.starts_with("claude-opus-5")
            || model.starts_with("claude-sonnet-5")
            || model.starts_with("claude-haiku-5")
            || model.starts_with("claude-opus-4-6")
            || model.starts_with("claude-sonnet-4-6") => {
            body["output_config"] = json!({"effort":effort});
        }
        _ => return Err(
            "Reasoning control is unavailable for this provider/model; select Auto".into(),
        ),
    }
    Ok(())
}

fn validate_tool_reasoning_compatibility(
    provider: &str,
    model: &str,
    selected: Option<&str>,
) -> Result<(), String> {
    if provider == "anthropic" && selected.is_some_and(|effort| effort != "auto") {
        // The current manual tool continuation reconstitutes assistant output
        // from a validated tool call only. Anthropic requires preserving
        // opaque signed thinking blocks when adaptive thinking was used.
        return Err("Anthropic reasoning control with tools is not supported; select Auto".into());
    }
    if provider == "openai" && [
        "gpt-6-astra",
        "gpt-6.1-sol",
        "gpt-6-sol",
        "gpt-6-luna",
    ].iter().any(|prefix| model == *prefix || model.starts_with(&format!("{prefix}-"))) {
        // These models' published Chat Completions endpoint does not support
        // function calling with reasoning enabled. The existing MCP path
        // requires function calls and does not implement Responses yet.
        return Err("This OpenAI model requires the Responses API for reasoning with tools".into());
    }
    Ok(())
}

/// Ephemeral unapproved AI-generated tool request. A model cannot call tools
/// until a separate direct user action consumes this exact review ID.
struct PendingMcpTurn {
    at: Instant,
    input: ChatRequest,
    previous: Option<Session>,
    context: Vec<Message>,
    instructions: Vec<String>,
    proposal: provider_tools::ModelToolCall,
    tools: Vec<Value>,
    connection: mcp_client::McpSession,
    connection_generation: u64,
}

/// Goal MCP proposals are scoped to ONE durable Goal turn, never a chat
/// session. Exact arguments are volatile and are dropped on restart; the
/// separate goal_mcp ledger persists only redacted dispatch uncertainty.
struct PendingGoalMcpTurn {
    lease: goal_mcp::GoalMcpLease,
    request_id: String,
    ticket_id: String,
    provider: String,
    model: String,
    instructions: Vec<String>,
    context: Vec<Message>,
    tools: Vec<Value>,
    connection: mcp_client::McpSession,
    connection_generation: u64,
}

pub struct Engine {
    home: PathBuf,
    keys: Mutex<HashMap<String, String>>,
    keys_generation: AtomicU64,
    mcp: Mutex<Option<mcp_client::McpSession>>,
    mcp_generation: AtomicU64,
    pending_tool_turns: Mutex<HashMap<String, PendingMcpTurn>>,
    local_tool_host: Mutex<Option<Arc<dyn AndroidLocalToolHost>>>,
    local_tool_generation: AtomicU64,
    pending_goal_tool: Mutex<Option<PendingGoalMcpTurn>>,
    agents: Mutex<Option<mobile_agents::MobileAgentCoordinator>>,
    ai_delegation: Mutex<Option<Arc<mobile_ai_delegation::DelegationStore>>>,
    prime: Mutex<Option<Arc<mobile_prime::PrimeStore>>>,
    /// job UUID -> (durable ticket UUID, active provider request UUID).
    ai_delegate_active: Mutex<HashMap<String, (String, String)>>,
    goal: Mutex<Option<mobile_goal::GoalDriver>>,
    project_import: Mutex<Option<(String, String, mobile_projects::LegacyImportPreview)>>,
    chat_cancellations: Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>,
    chat_progress: Mutex<HashMap<String, String>>,
    /// At most one explicitly started AI-backed Goal turn. There is no
    /// background runner and dropping the request releases this reservation.
    goal_ai_inflight: Mutex<Option<String>>,
    session_save_guard: Mutex<()>,
    client: Client,
}

// Dropping the Android/Tauri invocation (for example after Activity teardown)
// must release its cancellation slot and volatile partial text as well.
struct ChatCleanup<'a> { engine: &'a Engine, id: String }
impl Drop for ChatCleanup<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.engine.chat_cancellations.lock() { active.remove(&self.id); }
        if let Ok(mut progress) = self.engine.chat_progress.lock() { progress.remove(&self.id); }
    }
}

struct GoalAiCleanup<'a> {
    engine: &'a Engine,
    request_id: String,
    /// The durable Goal turn must never remain InTurn when the provider
    /// future is dropped by an Activity teardown or request cancellation.
    turn: Option<(String, String)>,
    settled: bool,
    /// Once tools/call MAY have been dispatched, an uncertain result cannot
    /// silently become an ordinary retryable step. Require manual review.
    external_effect_uncertain: bool,
}
impl Drop for GoalAiCleanup<'_> {
    fn drop(&mut self) {
        if !self.settled {
            if let Some((goal_id, ticket_id)) = &self.turn {
                if let Ok(mut guard) = self.engine.goal.lock() {
                    if let Some(driver) = guard.as_mut() {
                        let current = driver.snapshot();
                        if current.id == *goal_id
                            && current.status == mobile_goal::GoalStatus::InTurn
                            && current.active_turn.as_ref().is_some_and(|turn| &turn.id == ticket_id)
                        {
                            // No external action is retried. A dropped future
                            // has no confirmed provider answer or completion.
                            if self.external_effect_uncertain {
                                let _ = driver.record_outcome(ticket_id,
                                    mobile_goal::TurnOutcome::NeedsApproval(
                                        "An approved external MCP action may have changed remote state. Review it before authorizing another Goal turn".into()));
                            } else {
                                let _ = driver.mark_turn_interrupted(ticket_id,
                                    "Goal provider invocation interrupted before a verified response");
                            }
                        }
                    }
                }
            }
        }
        if let Ok(mut active) = self.engine.goal_ai_inflight.lock() {
            if active.as_deref() == Some(&self.request_id) { *active = None; }
        }
    }
}

/// A dropped Android UI command is NOT a completed provider response.
/// Persist interruption metadata best-effort; never synthesize an answer or
/// schedule a retry. Process death is independently recovered by Store::open.
struct DelegateRunCleanup<'a> {
    engine: &'a Engine,
    store: Arc<mobile_ai_delegation::DelegationStore>,
    job_id: String,
    ticket_id: String,
    settled: bool,
}

/// Dropping a Prime HTTP future must not leave a pretend live worker in the
/// same process. Persist a failed/interrupted metadata state; never resubmit.
struct PrimeRunCleanup {
    store: Arc<mobile_prime::PrimeStore>,
    task_id: String,
    worker_id: String,
    request_id: String,
    settled: bool,
}
impl Drop for PrimeRunCleanup {
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.store.settle(&self.task_id,&self.worker_id,&self.request_id,None);
        }
    }
}
impl Drop for DelegateRunCleanup<'_> {
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.store.fail(&self.job_id, &self.ticket_id,
                "Provider request interrupted; no result was verified");
        }
        if let Ok(mut active) = self.engine.ai_delegate_active.lock() {
            if active.get(&self.job_id).is_some_and(|(ticket, _)| ticket == &self.ticket_id) {
                active.remove(&self.job_id);
            }
        }
    }
}

fn valid_provider(provider: &str) -> bool {
    // User-selected API-key providers operate through the separate native
    // /api/chat pipeline. ChatGPT-plan OAuth uses its own narrower transport.
    matches!(provider, "openai" | "anthropic" | "openrouter")
        || (cfg!(target_os = "windows") && provider == "ollama")
}

fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 120
        && model.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
        && model.as_bytes()[0].is_ascii_alphanumeric()
}

/// This check is shared by the proposal and approval boundaries: parsing
/// syntactically valid model JSON is never equivalent to MCP schema approval.
fn validate_provider_mcp_proposal(
    verified_tools: &[Value],
    proposal: &provider_tools::ModelToolCall,
) -> Result<(), String> {
    mcp_client::validate_model_arguments(
        verified_tools,
        &proposal.name,
        &proposal.arguments,
    )
}

fn goal_ai_step_prompt(snapshot: &mobile_goal::GoalSnapshot) -> Result<String, String> {
    let step = snapshot.spec.steps.get(snapshot.current_step)
        .ok_or("Goal has no remaining step")?;
    let mut prompt = format!(
        "Goal: {}\nCurrent step {}/{}: {}\n\n",
        snapshot.spec.title, snapshot.current_step + 1, snapshot.spec.steps.len(),
        step.description,
    );
    if snapshot.current_step > 0 {
        prompt.push_str("Earlier steps marked complete by the user:\n");
        for (idx, step) in snapshot.spec.steps[..snapshot.current_step].iter().enumerate() {
            prompt.push_str(&format!("{}. {}\n", idx + 1, step.description));
        }
    }
    prompt.push_str("Provide a concrete answer for this ONE step. Do not claim you ran tools, accessed device files, or performed external actions. The user must independently review and record the outcome; do not assume completion.");
    if prompt.len() > MAX_PROMPT { return Err("Goal step exceeds provider prompt limit".into()); }
    Ok(prompt)
}

impl Engine {
    pub fn new(home: PathBuf) -> Result<Self, String> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(55))
            .build()
            .map_err(|_| "Could not initialize the HTTPS client")?;
        Ok(Self { home, keys: Mutex::new(HashMap::new()),
            keys_generation: AtomicU64::new(0),
            mcp: Mutex::new(None), mcp_generation: AtomicU64::new(0),
            pending_tool_turns: Mutex::new(HashMap::new()),
            local_tool_host: Mutex::new(None), local_tool_generation: AtomicU64::new(0),
            agents: Mutex::new(None),
            pending_goal_tool: Mutex::new(None),
            ai_delegation: Mutex::new(None), ai_delegate_active: Mutex::new(HashMap::new()),
            prime: Mutex::new(None),
            chat_cancellations: Mutex::new(HashMap::new()),
            chat_progress: Mutex::new(HashMap::new()),
            goal_ai_inflight: Mutex::new(None),
            session_save_guard: Mutex::new(()),
            goal: Mutex::new(None), project_import: Mutex::new(None), client })
    }

    /// Install the native Android AppHandle adapter once, during Tauri setup,
    /// before the engine is exposed to the renderer. No actions run on install.
    pub fn install_android_local_tool_host(
        &self, host: Arc<dyn AndroidLocalToolHost>,
    ) -> Result<(), String> {
        let mut guard = self.local_tool_host.lock()
            .map_err(|_| "Android tool host lock unavailable")?;
        if guard.is_some() { return Err("Android tool host already installed".into()); }
        *guard = Some(host);
        self.local_tool_generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn sessions_path(&self) -> PathBuf { self.home.join("sessions.json") }

    fn sessions(&self) -> Result<Vec<Session>, String> {
        let content = match fs::read(self.sessions_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err("Could not read stored conversations".into()),
        };
        if content.len() > 2 * 1024 * 1024 { return Err("Stored conversations exceed the safety limit".into()); }
        serde_json::from_slice(&content).map_err(|_| "Existing chat history is invalid; it was not changed".into())
    }

    fn save_sessions(&self, sessions: &[Session]) -> Result<(), String> {
        fs::create_dir_all(&self.home).map_err(|_| "Could not create app data directory")?;
        let encoded = serde_json::to_vec(sessions).map_err(|_| "Could not serialize sessions")?;
        if encoded.len() > 2 * 1024 * 1024 { return Err("Conversation storage limit reached".into()); }
        let temporary = self.home.join(format!("sessions-{}.tmp", Uuid::new_v4()));
        fs::write(&temporary, encoded).map_err(|_| "Could not write conversation")?;
        // Keep the previous file intact if replacement fails.
        let dest = self.sessions_path();
        if let Err(_) = fs::rename(&temporary, &dest) {
            let _ = fs::remove_file(&temporary);
            return Err("Could not replace session history safely".into());
        }
        Ok(())
    }

    /// A read-only snapshot to take BEFORE the website host dispatches a
    /// prompt. Its full contents, including at/actions, are compared under
    /// session_save_guard when the host later submits a confirmed receipt.
    /// No renderer-supplied transcript is accepted as an authoritative state.
    pub fn chatgpt_browser_session_snapshot(
        &self, session_id: Option<&str>,
    ) -> Result<Option<Session>, String> {
        let _guard = self.session_save_guard.lock().map_err(|_| "Chat storage unavailable")?;
        let Some(id) = session_id else { return Ok(None); };
        Uuid::parse_str(id).map_err(|_| "Invalid conversation ID")?;
        let sessions = self.sessions()?;
        let session = sessions.into_iter().find(|s| s.id == id)
            .ok_or("Conversation not found")?;
        if session.provider != CHATGPT_BROWSER_PROVIDER || session.model != CHATGPT_BROWSER_MODEL {
            return Err("The conversation belongs to a different provider or model".into());
        }
        Ok(Some(session))
    }

    /// Reject a website-based session before dispatching an OAuth turn.
    /// Context is taken exclusively from the stored conversation snapshot.
    pub fn chatgpt_oauth_session_snapshot(
        &self, session_id: Option<&str>,
    ) -> Result<(Option<Session>, Vec<Message>), String> {
        let snapshot = self.chatgpt_browser_session_snapshot(session_id)?;
        let mut context = Vec::new();
        if let Some(ref previous) = snapshot {
            if previous.actions.is_empty() {
                return Err("ChatGPT session has no authenticated OAuth turn history".into());
            }
            if previous.actions.iter().any(|action|
                action["type"] != "chatgpt_oauth_turn"
                    || action["nativeConfirmed"] != true
                    || action["mcpToolsUsed"] != false
            ) {
                return Err("Existing conversation uses another ChatGPT transport".into());
            }
            context = previous.messages.iter().rev().take(16).cloned().collect();
            context.reverse();
            if context.iter().any(|message|
                !matches!(message.role.as_str(), "user" | "assistant")
                    || message.text.trim().is_empty() || message.text.len() > MAX_BROWSER_ANSWER
            ) {
                return Err("Stored OAuth conversation context is invalid".into());
            }
        }
        Ok((snapshot, context))
    }

    /// Durable one-shot claim performed BEFORE invoking the Android plugin.
    /// A crash or uncertain response consumes the ID, preventing replay.
    pub fn claim_oauth_chat_request(&self, request_id: &str) -> Result<(), String> {
        let parsed = Uuid::parse_str(request_id).map_err(|_| "Invalid OAuth request UUID")?;
        if parsed.is_nil() || parsed.to_string() != request_id {
            return Err("OAuth request UUID must be canonical and nonzero".into());
        }
        let ledger = self.home.join("chatgpt-oauth-request-ids");
        fs::create_dir_all(&ledger).map_err(|_| "Could not initialize OAuth request ledger")?;
        let mut count = 0usize;
        for entry in fs::read_dir(&ledger).map_err(|_| "Could not read OAuth request ledger")? {
            entry.map_err(|_| "Could not inspect OAuth request ledger")?;
            count = count.saturating_add(1);
            if count >= MAX_BROWSER_RECEIPTS {
                return Err("OAuth request ledger is full".into());
            }
        }
        let marker = OpenOptions::new().write(true).create_new(true)
            .open(ledger.join(request_id)).map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    "OAuth request UUID has already been claimed".to_string()
                } else {
                    "Could not claim OAuth request UUID".to_string()
                }
            })?;
        marker.sync_all().map_err(|_| "Could not durably claim OAuth request UUID".to_string())
    }

    /// Persist only an answer returned by the native OAuth plugin for a
    /// previously claimed request. Website metadata and tool calls are rejected.
    pub fn persist_oauth_chat_turn(&self, receipt: OAuthChatTurnReceipt) -> Result<Value, String> {
        let parsed = Uuid::parse_str(&receipt.request_id)
            .map_err(|_| "Invalid OAuth request UUID")?;
        if parsed.is_nil() || parsed.to_string() != receipt.request_id {
            return Err("OAuth request UUID must be canonical and nonzero".into());
        }
        if receipt.prompt.trim().is_empty() || receipt.prompt.len() > MAX_PROMPT {
            return Err("OAuth prompt must be 1-20,000 bytes".into());
        }
        if receipt.answer.trim().is_empty() || receipt.answer.len() > MAX_BROWSER_ANSWER {
            return Err("Native OAuth answer must be 1-20,000 bytes".into());
        }
        if receipt.expected_session.as_ref().map(|s| &s.id) != receipt.session_id.as_ref() {
            return Err("OAuth session ID differs from the pre-send snapshot".into());
        }
        if receipt.expected_session.as_ref().is_some_and(|session|
            session.provider != CHATGPT_BROWSER_PROVIDER || session.model != CHATGPT_BROWSER_MODEL
                || session.actions.is_empty()
                || session.actions.iter().any(|action|
                    action["type"] != "chatgpt_oauth_turn"
                        || action["nativeConfirmed"] != true
                        || action["mcpToolsUsed"] != false)
        ) {
            return Err("Existing session is not an authenticated OAuth conversation".into());
        }
        if !self.home.join("chatgpt-oauth-request-ids").join(&receipt.request_id).is_file() {
            return Err("OAuth request was not claimed before native dispatch".into());
        }
        let _guard = self.session_save_guard.lock().map_err(|_| "Chat storage unavailable")?;
        let mut sessions = self.sessions()?;
        if sessions.iter().flat_map(|session| &session.actions).any(|action|
            action["type"] == "chatgpt_oauth_turn"
                && action["requestId"] == receipt.request_id
        ) {
            return Err("OAuth request has already been saved".into());
        }
        if let Some(ref expected) = receipt.expected_session {
            if sessions.iter().find(|s| s.id == expected.id) != Some(expected) {
                return Err("OAuth conversation changed while answering; reply was not saved".into());
            }
        }
        let request_id = receipt.request_id.clone();
        let input = ChatRequest {
            prompt: receipt.prompt,
            provider: CHATGPT_BROWSER_PROVIDER.into(),
            model: CHATGPT_BROWSER_MODEL.into(),
            session_id: receipt.session_id,
            request_id: Some(request_id.clone()),
            allow_mcp_tools: false,
            allow_local_tools: false,
            reasoning_effort: None,
        };
        let metadata = json!({
            "type":"chatgpt_oauth_turn", "requestId":request_id,
            "nativeConfirmed":true, "mcpToolsUsed":false
        });
        let saved = Self::compose_completed_turn(&mut sessions, input,
            receipt.expected_session, receipt.answer, vec![metadata])?;
        self.save_sessions(&sessions)
            .map_err(|_| "OAuth request consumed; conversation save failed and cannot be retried".to_string())?;
        Ok(json!({"requestId":request_id,"state":"completed",
            "sessionId":saved["sessionId"],"answer":saved["answer"],
            "actions":[],"saved":true,"status":"confirmed"}))
    }

    /// Claim the UUID before touching chat history. A claim is never pruned
    /// and remains present even if its conversation is evicted. Across app
    /// restarts and concurrent Engine instances, create_new is the atomic
    /// no-replay boundary. If a later save fails, the UUID stays consumed,
    /// so a failure cannot accidentally resend or duplicate a website turn.
    fn claim_chatgpt_browser_request(&self, request_id: &str) -> Result<(), String> {
        let ledger = self.home.join("chatgpt-browser-request-ids");
        fs::create_dir_all(&ledger).map_err(|_| "Could not create browser receipt ledger")?;
        let mut count = 0usize;
        for entry in fs::read_dir(&ledger).map_err(|_| "Could not read browser receipt ledger")? {
            entry.map_err(|_| "Could not inspect browser receipt ledger")?;
            count = count.saturating_add(1);
            if count >= MAX_BROWSER_RECEIPTS {
                return Err("Browser receipt ledger is full; no new request was stored".into());
            }
        }
        let proof = ledger.join(request_id);
        let marker = OpenOptions::new().write(true).create_new(true).open(&proof).map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                "ChatGPT browser request UUID has already been claimed".to_string()
            } else {
                "Could not claim browser receipt UUID safely".to_string()
            }
        })?;
        marker.sync_all().map_err(|_| "Could not durably claim browser receipt UUID".to_string())?;
        Ok(())
    }

    /// Persists an exact website turn only after the *native owned* WebView
    /// host has verified the user message, finished assistant message, origin
    /// and conversation identity. The host must NOT expose this method as a
    /// generic renderer-supplied/HTTP completion route; caller-provided
    /// Confirmed is not proof of WebView ownership by itself.
    pub fn persist_browser_chat_turn(
        &self, receipt: BrowserChatTurnReceipt,
    ) -> Result<Value, String> {
        if receipt.status != BrowserTurnReceiptStatus::Confirmed {
            return Err("ChatGPT website turn is not confirmed by the native host".into());
        }
        if receipt.used_mcp_tools {
            return Err("ChatGPT browser receipts cannot contain MCP tool calls".into());
        }
        let parsed = Uuid::parse_str(&receipt.request_id)
            .map_err(|_| "Invalid browser request UUID")?;
        if parsed.is_nil() || parsed.to_string() != receipt.request_id {
            return Err("Browser request UUID must be canonical and nonzero".into());
        }
        if receipt.prompt.trim().is_empty() || receipt.prompt.len() > MAX_PROMPT {
            return Err("Browser prompt must be 1-20,000 bytes".into());
        }
        if receipt.answer.trim().is_empty() || receipt.answer.len() > MAX_BROWSER_ANSWER {
            return Err("Confirmed browser answer must be 1-20,000 bytes".into());
        }
        if let Some(ref id) = receipt.session_id {
            Uuid::parse_str(id).map_err(|_| "Invalid conversation ID")?;
        }
        if receipt.expected_session.as_ref().map(|s| &s.id) != receipt.session_id.as_ref() {
            return Err("Browser session ID does not match the pre-send snapshot".into());
        }
        if receipt.expected_session.as_ref().is_some_and(|s|
            s.provider != CHATGPT_BROWSER_PROVIDER || s.model != CHATGPT_BROWSER_MODEL)
        {
            return Err("Browser snapshot belongs to another provider or model".into());
        }
        // The site may not expose a /c/<uuid> yet for a first turn. Such a
        // first turn is saved unbound but cannot be resumed as a prior website
        // conversation until a trusted binding exists.
        if let Some(ref id) = receipt.browser_conversation_id {
            let parsed_id = Uuid::parse_str(id).map_err(|_| "Invalid ChatGPT browser conversation UUID")?;
            if parsed_id.is_nil() || parsed_id.to_string() != *id {
                return Err("ChatGPT browser conversation UUID must be canonical".into());
            }
        }
        if let Some(ref prior) = receipt.expected_session {
            let mut known_id: Option<&str> = None;
            if prior.actions.is_empty() {
                return Err("Previous ChatGPT website conversation lacks a native browser binding".into());
            }
            for action in &prior.actions {
                if action["type"] != "chatgpt_browser_turn"
                    || action["hostConfirmed"] != true
                    || action["mcpToolsUsed"] != false
                {
                    return Err("Previous browser conversation has unverified actions".into());
                }
                let id = action["browserConversationId"].as_str()
                    .ok_or("Previous ChatGPT website conversation is not bound to a browser ID")?;
                if known_id.is_some_and(|previous| previous != id) {
                    return Err("Previous browser conversation contains conflicting IDs".into());
                }
                known_id = Some(id);
            }
            if known_id != receipt.browser_conversation_id.as_deref() {
                return Err("ChatGPT website conversation changed; start a new app chat".into());
            }
        }

        let _guard = self.session_save_guard.lock().map_err(|_| "Chat storage unavailable")?;
        let mut sessions = self.sessions()?;
        // This checks the complete snapshot under the same lock used for the
        // eventual write, so stale browser answers cannot replace later turns
        // or attach to a deleted/reused session UUID.
        if let Some(ref previous) = receipt.expected_session {
            let current = sessions.iter().find(|s| s.id == previous.id)
                .ok_or("Browser session changed while answering; no reply saved")?;
            if current != previous {
                return Err("Browser session changed while answering; no reply saved".into());
            }
        }
        let metadata = json!({
            "type": "chatgpt_browser_turn",
            "requestId": receipt.request_id,
            "browserConversationId": receipt.browser_conversation_id,
            "hostConfirmed": true,
            "mcpToolsUsed": false
        });
        let request_id = receipt.request_id.clone();
        let input = ChatRequest {
            prompt: receipt.prompt,
            provider: CHATGPT_BROWSER_PROVIDER.into(),
            model: CHATGPT_BROWSER_MODEL.into(),
            session_id: receipt.session_id,
            request_id: Some(request_id.clone()),
            allow_mcp_tools: false,
            allow_local_tools: false,
            reasoning_effort: None,
        };
        let saved = Self::compose_completed_turn(&mut sessions, input,
            receipt.expected_session, receipt.answer, vec![metadata])?;
        let serialized_size = serde_json::to_vec(&sessions)
            .map_err(|_| "Could not serialize ChatGPT browser conversation")?.len();
        if serialized_size > 2 * 1024 * 1024 {
            return Err("Conversation storage limit reached".into());
        }
        self.claim_chatgpt_browser_request(&request_id)?;
        self.save_sessions(&sessions)
            .map_err(|_| "Browser request UUID claimed; conversation save failed and cannot be retried".to_string())?;
        Ok(json!({
            "saved": true,
            "status": "confirmed",
            "sessionId": saved["sessionId"],
            "answer": saved["answer"],
            "provider": CHATGPT_BROWSER_PROVIDER,
            "model": CHATGPT_BROWSER_MODEL,
            "browserConversationId": saved["actions"][0]["browserConversationId"],
            "requestId": request_id
        }))
    }

    pub fn state(&self) -> Result<Value, String> {
        let providers = if cfg!(target_os = "android") {
            vec![CHATGPT_BROWSER_PROVIDER, "openai", "anthropic", "openrouter"]
        } else if cfg!(target_os = "windows") {
            vec!["openai", "anthropic", "openrouter", "ollama"]
        } else { vec!["openai", "anthropic", "openrouter"] };
        let keys = self.keys.lock().map_err(|_| "Key store unavailable")?;
        let mut names: Vec<&str> = keys.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        // A cancelled or unknown-outcome tools/call can revoke a shared MCP
        // transport without clearing the Option. Never report that stale
        // connection as healthy merely because its old handle still exists.
        let mcp_connected = self.mcp.lock().map_err(|_| "MCP connection unavailable")?
            .as_ref().is_some_and(|session| session.same_session(session));
        Ok(json!({
            "version": "0.2.2-preview",
            "platform": if cfg!(target_os="android") {"android"} else {"windows"},
            "modules": {},
            "availableModules": [],
            "skillsAvailable": cfg!(target_os = "android"),
            "skills": if cfg!(target_os="android") {
                mobile_skills::load(&self.home)?.iter().map(|skill| json!({
                    "id":skill.id,"name":skill.name,"enabled":skill.enabled
                })).collect::<Vec<_>>()
            } else { vec![] },
            "sessions": self.sessions()?,
            "providers": providers,
            "workspace": "Native app workspace",
            "hasKeys": names,
            "cloud": "not configured",
            "toolPanels": {"review": true, "terminal": true, "files": true, "subagents": cfg!(target_os="android")}
            ,"mcp": {"connected": mcp_connected, "transport": "HTTPS JSON or bounded SSE",
                "autoStart": false, "externalToolsAutoExecuted": false}
        }))
    }

    pub fn set_key(&self, provider: &str, value: &str) -> Result<Value, String> {
        if !valid_provider(provider) || provider == "ollama" { return Err("Unknown API provider".into()); }
        if value.len() > 4096 || value.contains(['\r', '\n']) { return Err("Invalid API key".into()); }
        let mut keys = self.keys.lock().map_err(|_| "Key store unavailable")?;
        if value.trim().is_empty() { keys.remove(provider); }
        else { keys.insert(provider.to_owned(), value.trim().to_owned()); }
        self.keys_generation.fetch_add(1, Ordering::SeqCst);
        Ok(json!({ "savedInMemory": keys.contains_key(provider) }))
    }

    /// Explicit user-facing conversation export. Returns text only; no files,
    /// cloud upload or external intents are triggered by this API.
    pub fn export_session_markdown(&self, id: &str) -> Result<Value, String> {
        Uuid::parse_str(id).map_err(|_| "Invalid conversation ID")?;
        let sessions = self.sessions()?;
        let session = sessions.iter().find(|s| s.id == id).ok_or("Conversation not found")?;
        let mut export = format!("# Mcp Agent Ai conversation\n\nProvider: {}\nModel: {}\n\n",
            session.provider, session.model);
        for message in &session.messages {
            let label = match message.role.as_str() {
                "user" => "User", "assistant" => "Assistant",
                _ => return Err("Unsupported message role in conversation".into()),
            };
            if message.text.len() > MAX_PROMPT { return Err("Conversation entry exceeds export limit".into()); }
            export.push_str(&format!("## {label}\n\n{}\n\n", message.text.trim_end()));
            if export.len() > 128 * 1024 { return Err("Conversation is too large to export".into()); }
        }
        Ok(json!({"filename": format!("conversation-{id}.md"), "content": export,
            "format": "markdown", "readOnly": true}))
    }

    // Bind chat-local standing rules to the verified persisted session, never
    // to the last selected renderer chat. Independent Goal/delegate requests
    // receive only explicitly enabled global rules and Skills.
    fn active_native_instructions(&self, session_id: Option<&str>) -> Result<Vec<String>, String> {
        if !cfg!(target_os = "android") { return Ok(Vec::new()); }
        let mut instructions = mobile_skills::active_instructions(&mobile_skills::load(&self.home)?);
        let scoped = standing_rules::instructions_scoped(&self.home, session_id)?;
        for rule in scoped.into_iter().rev() {
            instructions.insert(0, rule);
        }
        Ok(instructions)
    }

    async fn provider_answer(&self, provider: &str, model: &str, key: &str, history: &[Message], instructions: &[String], stream_id: Option<&str>, reasoning_effort: Option<&str>) -> Result<String, String> {
        let endpoint = match provider {
            "openai" => "https://api.openai.com/v1/chat/completions",
            "anthropic" => "https://api.anthropic.com/v1/messages",
            "openrouter" => "https://openrouter.ai/api/v1/chat/completions",
            "ollama" if cfg!(target_os = "windows") => "http://127.0.0.1:11434/v1/chat/completions",
            _ => return Err("Unsupported provider".into()),
        };
        let mut messages: Vec<Value> = history.iter().map(|message| json!({
            "role": message.role, "content": message.text
        })).collect();
        let system = if instructions.is_empty() { None } else {
            Some(format!("The user enabled these personal response instructions:\n\n{}",
                instructions.join("\n\n")))
        };
        if provider != "anthropic" {
            if let Some(system) = &system { messages.insert(0, json!({"role":"system","content":system})); }
        }
        let mut body = if provider == "anthropic" {
            let mut body = json!({"model": model, "max_tokens": 2048, "messages": messages});
            if let Some(system) = system { body["system"] = json!(system); }
            body
        } else { json!({"model": model, "messages": messages}) };
        // Stream only when the Android UI supplies an owned request UUID;
        // Windows and older callers continue to use the stable JSON endpoint.
        if stream_id.is_some() { body["stream"] = json!(true); }
        apply_reasoning_effort(&mut body, provider, model, reasoning_effort)?;
        if body.to_string().len() > 160_000 { return Err("Conversation request too large".into()); }
        let mut request = self.client.post(endpoint)
            .header(header::CONTENT_TYPE, "application/json")
            .json(&body);
        if provider == "anthropic" {
            request = request.header("x-api-key", key).header("anthropic-version", "2023-06-01");
        } else if provider != "ollama" {
            request = request.bearer_auth(key);
        }
        let mut response = request.send().await.map_err(|_| "Provider network request failed")?;
        let status = response.status();
        if status == 401 || status == 403 { return Err("Provider rejected the API key".into()); }
        if status == 429 { return Err("Provider rate limit reached".into()); }
        if !status.is_success() { return Err(format!("Provider HTTP {}", status.as_u16())); }
        if response.content_length().is_some_and(|size| size > MAX_RESPONSE as u64) {
            return Err("Provider response too large".into());
        }
        if let Some(id) = stream_id {
            let mut collector = provider_stream::Collector::new(provider);
            while let Some(chunk) = response.chunk().await.map_err(|_| "Provider stream interrupted")? {
                collector.feed(&chunk, |delta| {
                    if let Ok(mut progress) = self.chat_progress.lock() {
                        if let Some(text) = progress.get_mut(id) { text.push_str(delta); }
                    }
                })?;
            }
            return collector.finish();
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "Provider stream interrupted")? {
            if bytes.len() + chunk.len() > MAX_RESPONSE { return Err("Provider response too large".into()); }
            bytes.extend_from_slice(&chunk);
        }
        let payload: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid provider JSON response")?;
        let answer = if provider == "anthropic" {
            payload["content"].as_array()
                .ok_or("Missing Anthropic response content")?
                .iter().filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>().join("")
        } else {
            payload["choices"][0]["message"]["content"].as_str()
                .ok_or("Missing provider response text")?.to_owned()
        };
        if answer.trim().is_empty() || answer.len() > MAX_PROMPT {
            return Err("Provider returned no usable text".into());
        }
        Ok(answer)
    }

    /// A single real provider turn with MCP tools proposed but never executed.
    /// The caller must display the proposal and wait for a *separate* user
    /// approval before any MCP call or provider continuation occurs.
    async fn provider_tool_offer(&self, provider: &str, model: &str, key: &str,
        history: &[Message], instructions: &[String], tools: &[Value],
        reasoning_effort: Option<&str>)
        -> Result<provider_tools::ProviderTurn, String> {
        if !matches!(provider, "openai" | "openrouter" | "anthropic") || tools.is_empty() || tools.len() > 16 {
            return Err("Provider tool use requires connected, bounded MCP tools".into());
        }
        validate_tool_reasoning_compatibility(provider, model, reasoning_effort)?;
        let endpoint = match provider {
            "openai" => "https://api.openai.com/v1/chat/completions",
            "openrouter" => "https://openrouter.ai/api/v1/chat/completions",
            "anthropic" => "https://api.anthropic.com/v1/messages",
            _ => unreachable!(),
        };
        let mut messages: Vec<Value> = history.iter().map(|m| json!({"role":m.role,"content":m.text})).collect();
        let system = if instructions.is_empty() { None } else {
            Some(format!("The user explicitly enabled these response instructions:\n\n{}", instructions.join("\n\n")))
        };
        if let Some(text) = &system {
            if provider != "anthropic" { messages.insert(0,json!({"role":"system","content":text})); }
        }
        let native_tools = tools.iter().map(|tool| {
            let name = tool["name"].as_str().ok_or("Invalid MCP tool definition")?;
            let description = tool["description"].as_str().unwrap_or("");
            let schema = tool.get("inputSchema").ok_or("MCP input schema missing")?;
            Ok(if provider == "anthropic" {
                json!({"name":name,"description":description,"input_schema":schema})
            } else {
                json!({"type":"function","function":{"name":name,
                    "description":description,"parameters":schema}})
            })
        }).collect::<Result<Vec<Value>,String>>()?;
        let mut body = if provider == "anthropic" {
            json!({"model":model,"max_tokens":2048,"messages":messages,
                "tools":native_tools,"tool_choice":{"type":"auto"}})
        } else {
            json!({"model":model,"messages":messages,"tools":native_tools,"tool_choice":"auto"})
        };
        if provider == "anthropic" {
            if let Some(system) = system { body["system"] = json!(system); }
        }
        apply_reasoning_effort(&mut body, provider, model, reasoning_effort)?;
        if serde_json::to_vec(&body).map_err(|_| "Could not encode model tool request")?.len() > 160_000 {
            return Err("Model tool request exceeds safety limit".into());
        }
        let mut req = self.client.post(endpoint).header(header::CONTENT_TYPE,"application/json").json(&body);
        if provider == "anthropic" { req = req.header("x-api-key",key).header("anthropic-version","2023-06-01"); }
        else { req = req.bearer_auth(key); }
        let mut res = req.send().await.map_err(|_| "Provider tool proposal request failed")?;
        if res.status().as_u16() == 401 || res.status().as_u16() == 403 {
            return Err("Provider rejected API key".into());
        }
        if !res.status().is_success() { return Err(format!("Provider tool request HTTP {}",res.status().as_u16())); }
        let mut bytes = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|_| "Provider tool response interrupted")? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE { return Err("Provider tool response exceeds limit".into()); }
            bytes.extend_from_slice(&chunk);
        }
        let payload: Value = serde_json::from_slice(&bytes).map_err(|_| "Provider tool response is invalid JSON")?;
        let allowed = tools.iter().filter_map(|t| t["name"].as_str()).map(str::to_owned).collect::<Vec<_>>();
        provider_tools::parse_provider_turn(provider, &payload, &allowed)
    }

    async fn provider_tool_exchange(&self, provider: &str, key: &str, payload: &Value,
        allowed_names: &[String], reasoning_effort: Option<&str>)
        -> Result<provider_tools::ProviderTurn, String> {
        let endpoint = match provider {
            "openai" => "https://api.openai.com/v1/chat/completions",
            "openrouter" => "https://openrouter.ai/api/v1/chat/completions",
            "anthropic" => "https://api.anthropic.com/v1/messages",
            _ => return Err("Unsupported model tool provider".into()),
        };
        let mut body = payload.clone();
        let model = body["model"].as_str().ok_or("Tool continuation model missing")?.to_owned();
        apply_reasoning_effort(&mut body, provider, &model, reasoning_effort)?;
        if serde_json::to_vec(&body).map_err(|_| "Invalid tool continuation")?.len() > 160_000 {
            return Err("Tool continuation exceeds safety limit".into());
        }
        let mut req = self.client.post(endpoint).header(header::CONTENT_TYPE,"application/json").json(&body);
        if provider == "anthropic" { req = req.header("x-api-key",key).header("anthropic-version","2023-06-01"); }
        else { req = req.bearer_auth(key); }
        let mut res = req.send().await.map_err(|_| "Provider continuation network failed")?;
        if !res.status().is_success() { return Err(format!("Provider continuation HTTP {}",res.status().as_u16())); }
        let mut bytes = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|_| "Provider continuation interrupted")? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE { return Err("Provider continuation exceeds limit".into()); }
            bytes.extend_from_slice(&chunk);
        }
        let output: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid provider continuation JSON")?;
        provider_tools::parse_provider_turn(provider,&output,allowed_names)
    }

    async fn provider_tool_resume(&self, provider: &str, key: &str, payload: &Value,
        reasoning_effort: Option<&str>)
        -> Result<String, String> {
        match self.provider_tool_exchange(provider,key,payload,&[],reasoning_effort).await? {
            provider_tools::ProviderTurn::Text(answer) => Ok(answer),
            provider_tools::ProviderTurn::ToolProposal(_) => Err("Nested model tool calls are not permitted".into()),
        }
    }

    pub async fn chat(&self, input: ChatRequest) -> Result<Value, String> {
        if !valid_provider(&input.provider) { return Err("Unsupported provider".into()); }
        if !valid_model(&input.model) { return Err("Invalid model ID".into()); }
        apply_reasoning_effort(&mut json!({}), &input.provider, &input.model,
            input.reasoning_effort.as_deref())?;
        if input.prompt.trim().is_empty() || input.prompt.len() > MAX_PROMPT {
            return Err("Message must be 1-20,000 bytes".into());
        }
        if input.allow_local_tools && input.allow_mcp_tools {
            return Err("Select local Android tools OR remote MCP tools for one chat turn".into());
        }
        if input.allow_local_tools && !cfg!(target_os = "android") {
            return Err("App-owned Android tools require the Android host".into());
        }
        let key = if input.provider == "ollama" { String::new() } else {
            self.keys.lock().map_err(|_| "Key store unavailable")?
                .get(&input.provider).cloned().ok_or("Connect a provider API key first")?
        };
        let sessions = self.sessions()?;
        let previous = match &input.session_id {
            Some(id) => {
                Uuid::parse_str(id).map_err(|_| "Invalid conversation ID")?;
                let session = sessions.iter().find(|session| &session.id == id)
                    .ok_or("Conversation not found")?;
                if session.provider != input.provider || session.model != input.model {
                    return Err("Start a new chat to change provider or model".into());
                }
                Some(session.clone())
            },
            None => None,
        };
        let mut context = previous.as_ref().map(|s| s.messages.iter().rev().take(12).cloned()
            .collect::<Vec<_>>()).unwrap_or_default();
        context.reverse();
        context.push(Message { role: "user".into(), text: input.prompt.clone() });
        let enabled = self.active_native_instructions(previous.as_ref().map(|s| s.id.as_str()))?;
        let mcp_context = if input.allow_mcp_tools {
            if !cfg!(target_os = "android") { return Err("AI MCP tools require native Android".into()); }
            if input.request_id.is_none() { return Err("AI MCP tools require a request UUID".into()); }
            let guard = self.mcp.lock().map_err(|_| "MCP connection unavailable")?;
            let session = guard.clone().ok_or("Connect an MCP server before opting in to model tools")?;
            let tool_definitions = session.tool_definitions();
            if tool_definitions.is_empty() { return Err("MCP server has no validated tool schemas".into()) }
            Some((session, self.mcp_generation.load(Ordering::SeqCst), tool_definitions))
        } else { None };
        // The current user-authored command alone grants one bounded scope.
        // Ordinary chat stays text-only when no specific native command is
        // present, despite allowLocalTools=true by default in Android UI.
        if input.allow_local_tools {
            if let Some(scope) = local_agent_intent::parse(&input.prompt) {
                if !matches!(input.provider.as_str(),"openai"|"openrouter"|"anthropic") {
                    return Err("Android native tools require an API-key model".into());
                }
                let host = self.local_tool_host.lock()
                    .map_err(|_| "Android native host unavailable")?
                    .as_ref().cloned().ok_or("Android native tool host not installed")?;
                let generation=self.local_tool_generation.load(Ordering::SeqCst);
                return self.run_intent_agent_chat(input,previous,context,enabled,key,
                    host,generation,scope).await;
            }
        }
        let request_id = input.request_id.clone();
        let mut cancellation = None;
        let mut cleanup = None;
        if let Some(ref id) = request_id {
            Uuid::parse_str(id).map_err(|_| "Invalid request ID")?;
            let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
            let mut active = self.chat_cancellations.lock().map_err(|_| "Chat cancellation unavailable")?;
            if active.contains_key(id) { return Err("Request ID is already active".into()); }
            if active.len() >= 3 { return Err("Too many active chat requests".into()); }
            active.insert(id.clone(), sender);
            cancellation = Some(receiver);
            if cfg!(target_os = "android") {
                self.chat_progress.lock().map_err(|_| "Chat progress unavailable")?
                    .insert(id.clone(), String::new());
            }
            cleanup = Some(ChatCleanup { engine: self, id: id.clone() });
        }
        let stream_id = if cfg!(target_os = "android") { request_id.as_deref() } else { None };
        let provider_future = async {
            match &mcp_context {
                Some((_, _, tools)) => self.provider_tool_offer(
                    &input.provider,&input.model,&key,&context,&enabled,tools,
                    input.reasoning_effort.as_deref()).await,
                None => self.provider_answer(&input.provider,&input.model,&key,&context,&enabled,
                    stream_id,input.reasoning_effort.as_deref())
                    .await.map(provider_tools::ProviderTurn::Text),
            }
        };
        let response = if let Some(mut receiver) = cancellation {
            tokio::select! {
                result = provider_future => result,
                _ = &mut receiver => Err("Request cancelled".into())
            }
        } else {
            provider_future.await
        };
        if let Some(id) = &request_id {
            // Cancellation acknowledged before this boundary wins even if the
            // provider completed at the same instant. No answer is persisted.
            let still_active = self.chat_cancellations.lock()
                .map_err(|_| "Chat cancellation unavailable")?.remove(id).is_some();
            self.chat_progress.lock().map_err(|_| "Chat progress unavailable")?.remove(id);
            if !still_active { return Err("Request cancelled".into()); }
        }
        // Keep the registration alive across provider completion and the save;
        // normal, cancelled and dropped futures all clean up the same slots.
        let _cleanup = cleanup;
        match response? {
            provider_tools::ProviderTurn::Text(answer) => self.append_completed_turn(input, previous, answer),
            provider_tools::ProviderTurn::ToolProposal(proposal) => {
                let (connection, connection_generation, tools) = mcp_context
                    .ok_or("Provider remote MCP proposal was not opted in")?;
                validate_provider_mcp_proposal(&tools, &proposal)?;
                if self.mcp_generation.load(Ordering::SeqCst) != connection_generation ||
                    !self.mcp.lock().map_err(|_| "MCP connection unavailable")?
                        .as_ref().is_some_and(|current| current.same_session(&connection)) {
                    return Err("MCP connection changed before user review".into());
                }
                let review_id = Uuid::new_v4().to_string();
                let mut pending = self.pending_tool_turns.lock()
                    .map_err(|_| "AI tool approval unavailable")?;
                pending.retain(|_, turn| turn.at.elapsed() < Duration::from_secs(120));
                if pending.len() >= 2 { return Err("Too many pending AI tool reviews".into()); }
                pending.insert(review_id.clone(), PendingMcpTurn {
                    at: Instant::now(), input, previous, context, instructions:enabled,
                    proposal: proposal.clone(), tools, connection,
                    connection_generation,
                });
                Ok(json!({"pendingTool": {"reviewId":review_id,
                    "requestId":request_id,"name":proposal.name,"arguments":proposal.arguments,
                    "expiresInSeconds":120},"answer":null,"actions":[],"sessionId":null}))
            },
        }
    }

    fn append_completed_turn(&self, input: ChatRequest, previous: Option<Session>, answer: String) -> Result<Value, String> {
        self.append_completed_turn_with_actions(input, previous, answer, Vec::new())
    }

    fn append_completed_turn_with_actions(&self, input: ChatRequest, previous: Option<Session>, answer: String, actions: Vec<Value>) -> Result<Value, String> {
        // Never save from the snapshot obtained before an HTTP request: two
        // overlapping requests would silently drop one another's sessions.
        let _guard = self.session_save_guard.lock().map_err(|_| "Chat storage unavailable")?;
        let mut sessions = self.sessions()?;
        let saved = Self::compose_completed_turn(&mut sessions, input, previous, answer, actions)?;
        self.save_sessions(&sessions)?;
        Ok(saved)
    }

    /// Called with session_save_guard held. Compose the complete result before
    /// committing; the website bridge can then atomically claim the request
    /// UUID before writing that same, fully validated session image.
    fn compose_completed_turn(sessions: &mut Vec<Session>, input: ChatRequest,
        previous: Option<Session>, answer: String, actions: Vec<Value>)
        -> Result<Value, String>
    {
        let id = input.session_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        if let Some(old) = previous.as_ref() {
            let current = sessions.iter().find(|s| s.id == id)
                .ok_or("Conversation changed while answering; no reply was saved")?;
            if current != old || old.provider != input.provider || old.model != input.model {
                return Err("Conversation changed while answering; no reply was saved".into());
            }
        }
        let mut previous_actions = previous.as_ref().map(|s| s.actions.clone()).unwrap_or_default();
        if previous_actions.len().saturating_add(actions.len()) > 128 {
            return Err("Conversation tool-action history exceeds safety limit".into());
        }
        previous_actions.extend(actions.clone());
        let mut messages = previous.map(|s| s.messages).unwrap_or_default();
        messages.push(Message { role: "user".into(), text: input.prompt });
        messages.push(Message { role: "assistant".into(), text: answer.clone() });
        messages = messages.into_iter().rev().take(40).collect();
        messages.reverse();
        sessions.retain(|s| s.id != id);
        sessions.insert(0, Session {
            id: id.clone(),
            at: format!("{:?}", std::time::SystemTime::now()),
            provider: input.provider, model: input.model, messages,
            actions: previous_actions,
        });
        sessions.truncate(MAX_SESSIONS);
        Ok(json!({"sessionId": id, "answer": answer, "actions": actions}))
    }

    /// Completely separate from the initial model reply. Only direct approval
    /// consumes an exact in-memory pending turn, with a second user confirmation
    /// at the UI. No retry after a tool may have had side effects.
    async fn reviewed_model_tool(&self, body: &Value) -> Result<Value, String> {
        if !cfg!(target_os = "android") { return Err("Model tools are Android-only".into()); }
        if body["userInitiated"] != true { return Err("Explicit user approval is required".into()); }
        let action = body["action"].as_str().ok_or("Model tool review action missing")?;
        if !matches!(action,"approve" | "reject") { return Err("Invalid model tool review action".into()); }
        let id = body["reviewId"].as_str().ok_or("Model tool review ID missing")?;
        Uuid::parse_str(id).map_err(|_| "Invalid model tool review ID")?;
        if action == "reject" {
            let removed = self.pending_tool_turns.lock().map_err(|_| "Tool review unavailable")?
                .remove(id).is_some();
            return Ok(json!({"rejected":removed,"executed":false}));
        }
        let turn = self.pending_tool_turns.lock().map_err(|_| "Tool review unavailable")?
            .remove(id).ok_or("Model tool review expired or no longer available")?;
        if turn.at.elapsed() >= Duration::from_secs(120) { return Err("Model tool review has expired".into()); }
        let request_id = turn.input.request_id.as_deref().ok_or("Request UUID missing")?;
        if body["requestId"].as_str() != Some(request_id) {
            return Err("Model tool review request ID mismatch".into());
        }
        if self.mcp_generation.load(Ordering::SeqCst) != turn.connection_generation ||
            !self.mcp.lock().map_err(|_| "MCP connection unavailable")?.as_ref()
                .is_some_and(|current| current.same_session(&turn.connection)) {
            return Err("MCP server changed since the proposal; tool not called".into());
        }
        // Revalidate at the approval boundary as well. A tool name and
        // user approval do not override the verified MCP input schema.
        validate_provider_mcp_proposal(&turn.tools, &turn.proposal)?;
        let key = self.keys.lock().map_err(|_| "Key store unavailable")?
            .get(&turn.input.provider).cloned().ok_or("Provider key was removed before tool approval")?;
        let request_id = request_id.to_owned();
        let (sender, mut cancel) = tokio::sync::oneshot::channel();
        {
            let mut active = self.chat_cancellations.lock().map_err(|_| "Chat cancellation unavailable")?;
            if active.len() >= 3 || active.contains_key(&request_id) {
                return Err("Another request is already active; tool has not run".into());
            }
            active.insert(request_id.clone(), sender);
        }
        let _cleanup = ChatCleanup { engine:self, id:request_id.clone() };
        // A provider continuation can fail *after* the remote MCP call has
        // already caused side effects. Persist a metadata-only approval and
        // possible-dispatch receipt first; corruption/storage errors forbid
        // execution rather than losing an external action from Review.
        tool_audit::before_dispatch(&self.home, id, &request_id,
            turn.input.session_id.as_deref(), &turn.proposal.name)?;
        let execute = async {
            let mut session = turn.connection.clone();
            let tool_result = session.call_tool(&turn.proposal.name,
                turn.proposal.arguments.clone()).await?;
            if self.mcp_generation.load(Ordering::SeqCst) != turn.connection_generation ||
                !self.mcp.lock().map_err(|_| "MCP connection unavailable")?.as_ref()
                    .is_some_and(|current| current.same_session(&turn.connection)) {
                return Err("MCP server disconnected during approved tool call".into());
            }
            if tool_result["isError"] == true {
                tool_audit::advance(&self.home,id,"server_reported_error")?;
                return Err("MCP tool reported failure; no model resume".into());
            }
            let checked_tool_output = mcp_client::provider_tool_result(&tool_result["result"])?;
            let verified_result: Value = serde_json::from_str(&checked_tool_output)
                .map_err(|_| "MCP tool returned invalid bounded JSON")?;
            tool_audit::advance(&self.home,id,"server_confirmed")?;
            let mut history: Vec<Value> = turn.context.iter().map(|m| json!({"role":m.role,"content":m.text})).collect();
            if !turn.instructions.is_empty() {
                history.insert(0, json!({"role":"system","content":format!(
                    "User-enabled response instructions:\n\n{}",turn.instructions.join("\n\n"))}));
            }
            let payload = provider_tools::continuation_payload(
                &turn.input.provider, &turn.input.model, &history, &turn.tools,
                &turn.proposal, &verified_result)?;
            // This continuation is one model turn only and explicitly forbids
            // further tool calls, nested delegation and background loops.
            let answer = self.provider_tool_resume(&turn.input.provider, &key, &payload,
                turn.input.reasoning_effort.as_deref()).await?;
            if self.mcp_generation.load(Ordering::SeqCst) != turn.connection_generation {
                return Err("MCP server disconnected before final response save".into());
            }
            let metadata = json!({"type":"mcp_tool_call","name":turn.proposal.name,
                "approvedByUser":true,"resultIsError":false,"resultStored":false});
            Ok::<_, String>((turn.input, turn.previous, answer, metadata))
        };
        tokio::select! {
            result = execute => {
                let still_active = self.chat_cancellations.lock()
                    .map_err(|_| "Chat cancellation unavailable")?.remove(&request_id).is_some();
                if !still_active { return Err("Approved tool request cancelled; remote side effects may still have occurred".into()); }
                let (input, previous, answer, metadata) = result?;
                let saved = self.append_completed_turn_with_actions(input, previous, answer, vec![metadata])?;
                // Chat was durably saved already: do not fabricate a failed
                // provider turn if the separate audit status cannot advance.
                if tool_audit::advance(&self.home,id,"answer_saved").is_err() {
                    return Ok(json!({"sessionId":saved["sessionId"],"answer":saved["answer"],
                        "actions":saved["actions"],"auditWarning":"Tool audit state could not be finalized; inspect Review before another call"}));
                }
                Ok(saved)
            },
            _ = &mut cancel => Err("Approved tool request cancelled; remote side effects may still have occurred".into()),
        }
    }

    fn cancel_chat(&self, request_id: &str) -> Result<Value, String> {
        Uuid::parse_str(request_id).map_err(|_| "Invalid request ID")?;
        let sender = self.chat_cancellations.lock().map_err(|_| "Chat cancellation unavailable")?
            .remove(request_id).ok_or("Request is not active")?;
        sender.send(()).map_err(|_| "Request has already completed")?;
        Ok(json!({"cancelRequested":true}))
    }

    fn chat_stream_progress(&self, request_id: &str) -> Result<Value, String> {
        Uuid::parse_str(request_id).map_err(|_| "Invalid request ID")?;
        let active = self.chat_progress.lock().map_err(|_| "Chat progress unavailable")?;
        Ok(match active.get(request_id) {
            Some(text) => json!({"active":true,"text":text,"streaming":true}),
            None => json!({"active":false,"text":"","streaming":false}),
        })
    }

    async fn mcp_request(&self, action: &str, body: &Value) -> Result<Value, String> {
        if !cfg!(target_os = "android") {
            return Err("External MCP servers are available on Android only".into());
        }
        if action == "call" && body["userInitiated"] != true {
            return Err("Manual MCP tool execution requires explicit user consent".into());
        }
        match action {
            "status" => {
                Ok(json!({"connected":self.mcp.lock().map_err(|_| "MCP connection unavailable")?
                    .as_ref().is_some_and(|session| session.same_session(session)),
                    "transport":"HTTPS JSON or bounded SSE","autoStart":false}))
            },
            "disconnect" => {
                self.disconnect_mcp()
            },
            "connect" => {
                // Reserve a connection generation before DNS/network work.
                // A later Connect or Disconnect invalidates this in-flight
                // request, preventing a stale result from resurrecting MCP.
                let generation = self.mcp_generation.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
                let endpoint = body["endpoint"].as_str().ok_or("MCP endpoint missing")?;
                let token = body["token"].as_str().unwrap_or("");
                let mut session = mcp_client::McpSession::create(endpoint, Some(token))?;
                let server = session.initialize().await?;
                let tools = session.list_tools().await?;
                let mut guard = self.mcp.lock().map_err(|_| "MCP connection unavailable")?;
                if self.mcp_generation.load(Ordering::SeqCst) != generation {
                    return Err("MCP connection was superseded or disconnected".into());
                }
                if let Some(old) = guard.as_mut() { old.disconnect(); }
                *guard = Some(session);
                Ok(json!({"connected":true,"server":server,"tools":tools}))
            },
            "list" | "call" => {
                let mut session = self.mcp.lock().map_err(|_| "MCP connection unavailable")?
                    .clone().ok_or("Connect an MCP server first")?;
                let previous = session.clone();
                let result = if action == "list" {
                    session.list_tools().await?
                } else {
                    let name = body["name"].as_str().ok_or("MCP tool missing")?;
                    let args = body["arguments"].as_object().ok_or("MCP arguments must be an object")?;
                    session.call_tool(name, Value::Object(args.clone())).await?
                };
                // Keep the pinned transport session only when still connected.
                // A disconnect made while a request is in flight always wins.
                let mut guard = self.mcp.lock().map_err(|_| "MCP connection unavailable")?;
                if guard.as_ref().is_some_and(|current| current.same_session(&previous)) {
                    if action == "list" {
                        // A new tools/list can replace the model-facing
                        // schema/allowlist. Invalidate pending approvals rather
                        // than executing a proposal from stale server tools.
                        self.mcp_generation.fetch_add(1, Ordering::SeqCst);
                        session.rebind_verified_generation();
                        self.pending_tool_turns.lock()
                            .map_err(|_| "MCP tool approval unavailable")?.clear();
                        self.pending_goal_tool.lock()
                            .map_err(|_| "Goal MCP approval unavailable")?.take();
                    }
                    *guard = Some(session);
                }
                Ok(result)
            },
            _ => Err("Unsupported MCP action".into())
        }
    }

    fn disconnect_mcp(&self) -> Result<Value, String> {
        let mut guard = self.mcp.lock().map_err(|_| "MCP connection unavailable")?;
        self.mcp_generation.fetch_add(1, Ordering::SeqCst);
        if let Some(current) = guard.as_mut() { current.disconnect(); }
        *guard = None;
        self.pending_tool_turns.lock().map_err(|_| "MCP pending review unavailable")?.clear();
        self.pending_goal_tool.lock().map_err(|_| "Goal MCP pending review unavailable")?.take();
        Ok(json!({"connected":false}))
    }

    fn mobile_agent_request(&self, action: &str, body: &Value) -> Result<Value, String> {
        if !cfg!(target_os = "android") { return Err("Local Android workers only".into()); }
        let mut guard = self.agents.lock().map_err(|_| "Local worker coordinator unavailable")?;
        if action == "submit" && guard.is_none() {
            let directory = self.home.join("agent-workspace");
            fs::create_dir_all(&directory).map_err(|_| "Could not initialize private agent workspace")?;
            *guard = Some(mobile_agents::MobileAgentCoordinator::new(
                directory, mobile_agents::CoordinatorConfig::default())?);
        }
        let coordinator = guard.as_ref();
        match action {
            "status" => Ok(match coordinator {
                Some(agent) => json!({"stats":agent.stats(),"history":agent.history(),
                    "scope":"Private agent workspace", "mode":"Local read-only tasks"}),
                None => json!({"stats":{"running":0,"queued":0,"retained":0,
                    "maxConcurrent":2,"maxQueued":16,"acceptingJobs":true},
                    "history":[],"scope":"Private agent workspace","mode":"Local read-only tasks"}),
            }),
            "submit" => {
                let job: mobile_agents::WorkspaceJob =
                    serde_json::from_value(body["job"].clone())
                        .map_err(|_| "Invalid local worker job")?;
                Ok(json!({"job":coordinator.ok_or("Worker coordinator unavailable")?.submit(job)?}))
            },
            "get" => {
                let id = body["id"].as_str().ok_or("Worker job ID missing")?;
                Ok(json!({"job":coordinator.ok_or("No worker jobs yet")?.get(id)?}))
            },
            "cancel" => {
                let id = body["id"].as_str().ok_or("Worker job ID missing")?;
                Ok(json!({"job":coordinator.ok_or("No worker jobs yet")?.cancel(id)?}))
            },
            _ => Err("Unsupported worker action".into())
        }
    }

    fn delegation_store(&self) -> Result<Arc<mobile_ai_delegation::DelegationStore>, String> {
        let mut guard = self.ai_delegation.lock().map_err(|_| "AI delegation state unavailable")?;
        if guard.is_none() {
            // Lazily opened only by a user-invoked Android delegate UI route.
            // Never open a second time during this Engine lifetime, because
            // recovery intentionally marks older Running tickets interrupted.
            let root = self.home.join("ai-delegations");
            *guard = Some(Arc::new(mobile_ai_delegation::DelegationStore::open_or_create(&root)?));
        }
        guard.as_ref().cloned().ok_or("AI delegation state unavailable".into())
    }

    fn prime_store(&self) -> Result<Arc<mobile_prime::PrimeStore>, String> {
        let mut guard = self.prime.lock().map_err(|_| "Prime state unavailable")?;
        if guard.is_none() {
            // One initialization per Engine lifetime. Re-opening a running
            // store would incorrectly interrupt calls from this process.
            *guard = Some(Arc::new(mobile_prime::PrimeStore::open_or_create(
                &self.home.join("prime-state"))?));
        }
        guard.as_ref().cloned().ok_or("Prime state unavailable".into())
    }

    /// One explicitly requested provider call per worker spawn/message. Uses
    /// the existing real AI-delegate provider route, which reserves at most
    /// two global delegate slots, validates keys and never exposes OS/MCP tools.
    /// This does not start a scheduler, service or recursive autonomous agent.
    async fn run_prime_delegate(
        &self,
        store: Arc<mobile_prime::PrimeStore>,
        call: mobile_prime::WorkerCall,
    ) -> Result<Value, String> {
        self.run_prime_delegate_via(store,call,|request| async move {
            self.mobile_ai_delegation_request(&request).await
        }).await
    }

    /// Keep receipt validation, durable delegation lookup, Prime settlement,
    /// and drop/cancel cleanup in the SAME path for real provider requests and
    /// deterministic tests. Only the transport is substituted in unit tests;
    /// no test callback or synthetic report is exposed to Android callers.
    async fn run_prime_delegate_via<F, Fut>(
        &self,
        store: Arc<mobile_prime::PrimeStore>,
        call: mobile_prime::WorkerCall,
        transport: F,
    ) -> Result<Value, String>
    where
        F: FnOnce(Value) -> Fut,
        Fut: std::future::Future<Output=Result<Value,String>>,
    {
        let mut cleanup = PrimeRunCleanup {
            store:store.clone(),task_id:call.task_id.clone(),
            worker_id:call.worker_id.clone(),request_id:call.request_id.clone(),
            settled:false,
        };
        let request = json!({
            "action":"run", "userInitiated":true,
            "provider":call.provider, "model":call.model,
            "task":call.prompt, "requestId":call.request_id,
            "primeTaskId":call.task_id,"primeWorkerId":call.worker_id
        });
        // Cancellation can be recorded between reservation and provider
        // dispatch. Avoid sending another AI request if already invalidated.
        let active = store.still_running(&call.task_id,&call.worker_id,&call.request_id)?;
        if !active { return Err("Prime worker request already cancelled".into()); }
        let receipt = transport(request).await;
        match receipt {
            Ok(value) => {
                let job = self.verified_prime_delegate_job(&call,&value)?;
                let answer = job.answer.as_deref().ok_or("Provider report missing")?;
                let updated = store.settle(&call.task_id,&call.worker_id,&call.request_id,
                    Some((&job.id,answer)))?;
                cleanup.settled = true;
                Ok(json!({"worker":updated,"providerReceipt":true,
                    "modelToolsExecuted":false,"automaticExecution":false,
                    "backgroundReplay":false}))
            },
            Err(_) => {
                Err("Prime delegate provider failed or was cancelled; no answer verified".into())
            }
        }
    }

    /// The ordinary delegate route is the only provider transport allowed for
    /// Prime workers. A JSON receipt alone is not proof: it must match the
    /// *current, durable* completed job with this precise provider/model and
    /// exact bounded prompt. This rules out forged/mismatched/stale envelopes.
    fn verified_prime_delegate_job(
        &self,
        call: &mobile_prime::WorkerCall,
        value: &Value,
    ) -> Result<mobile_ai_delegation::DelegationJob, String> {
        let invalid = || "Delegate response did not prove the selected provider/model".to_string();
        if value["providerReceipt"] != true
            || value["modelToolsExecuted"] != false
            || value["backgroundReplay"] != false
            || value["requestId"] != call.request_id
        {
            return Err(invalid());
        }
        let job: mobile_ai_delegation::DelegationJob =
            serde_json::from_value(value["job"].clone())
                .map_err(|_| "Invalid delegate provider receipt")?;
        if job.provider != call.provider || job.model != call.model
            || job.task != call.prompt
            || job.status != mobile_ai_delegation::DelegationStatus::Completed
            || job.ticket.is_some() || job.error.is_some() || job.answer.is_none()
        {
            return Err(invalid());
        }
        let persisted = self.delegation_store()?.get(&job.id)?;
        if persisted != job { return Err(invalid()); }
        Ok(job)
    }

    async fn mobile_prime_agent_request(&self, body: &Value) -> Result<Value, String> {
        if !cfg!(target_os = "android") {
            return Err("Android Prime coordinator only".into());
        }
        let action = body["action"].as_str().ok_or("Prime action missing")?;
        if body["allowMcpTools"] == true || body["allowLocalTools"] == true
            || body["allowShell"] == true || body["automaticExecution"] == true {
            return Err("Prime workers do not receive browser, file, shell or MCP privileges".into());
        }
        let store = self.prime_store()?;
        if action == "status" { return store.status(); }
        if body["userInitiated"] != true {
            return Err("An explicit user gesture is required for Prime actions".into());
        }
        let task_id = || body["taskId"].as_str().ok_or("Prime task ID missing");
        match action {
            "start" => Ok(json!({"task":store.start(
                body["goal"].as_str().ok_or("Prime goal missing")?,true)?})),
            "spawn" => {
                let call = store.spawn(task_id()?,
                    body["label"].as_str().ok_or("Worker label missing")?,
                    body["provider"].as_str().ok_or("Worker provider missing")?,
                    body["model"].as_str().ok_or("Worker model missing")?,
                    body["task"].as_str().ok_or("Worker assignment missing")?,
                    body["requestId"].as_str().ok_or("Worker request UUID missing")?,true)?;
                self.run_prime_delegate(store,call).await
            },
            "message" => {
                let call = store.message(task_id()?,
                    body["workerId"].as_str().ok_or("Worker ID missing")?,
                    body["message"].as_str().ok_or("Worker follow-up missing")?,
                    body["requestId"].as_str().ok_or("Worker request UUID missing")?,true)?;
                self.run_prime_delegate(store,call).await
            },
            "finish" => {
                let task_id = task_id()?;
                match body["workerId"].as_str() {
                    Some(id) => Ok(json!({"worker":store.finish_worker(task_id,id,true)?,
                        "providerVerifiedByDelegate":true})),
                    None => Ok(json!({"task":store.finish_task(task_id,
                        body["summary"].as_str().ok_or("User-written Prime summary missing")?,true)?,
                        "summaryUserWritten":true,"autoVerified":false})),
                }
            },
            "cancel" => {
                let task_id = task_id()?;
                match body["workerId"].as_str() {
                    Some(id) => {
                        let (worker, request) = store.cancel_worker(task_id,id,true)?;
                        if let Some(request_id) = &request {
                            let _ = self.cancel_chat(request_id);
                        }
                        Ok(json!({"worker":worker,"cancelRequested":request.is_some(),
                            "remoteProviderMayStillComplete":request.is_some()}))
                    },
                    None => {
                        let (task, requests) = store.cancel_task(task_id,true)?;
                        for request_id in &requests { let _ = self.cancel_chat(request_id); }
                        Ok(json!({"task":task,"cancelRequests":requests.len(),
                            "remoteProviderMayStillComplete":!requests.is_empty()}))
                    }
                }
            },
            _ => Err("Unsupported Android Prime action".into())
        }
    }

    async fn mobile_ai_delegation_request(&self, body: &Value) -> Result<Value, String> {
        if !cfg!(target_os = "android") { return Err("AI delegates require Android".into()); }
        let action = body["action"].as_str().ok_or("AI delegate action missing")?;
        let store = self.delegation_store()?;
        match action {
            "status" => {
                let jobs = store.list()?;
                let running = jobs.iter().filter(|j| j.status ==
                    mobile_ai_delegation::DelegationStatus::Running).count();
                Ok(json!({"jobs":jobs,"running":running,"maxConcurrent":2,
                    "automaticExecution":false,"networkToolsEnabled":false}))
            },
            "cancel" => {
                if body["userInitiated"] != true { return Err("Explicit delegate cancellation required".into()); }
                let id = body["id"].as_str().ok_or("AI delegate ID missing")?;
                let current = self.ai_delegate_active.lock()
                    .map_err(|_| "AI delegate cancellation unavailable")?
                    .get(id).cloned();
                let receipt = store.cancel(id, current.as_ref().map(|(ticket,_)| ticket.as_str()), true)?;
                if let Some((_, request_id)) = current {
                    // No retry or provider continuation after cancellation.
                    let _ = self.cancel_chat(&request_id);
                }
                Ok(json!({"job":receipt.job,"cancelRequested":receipt.was_running,
                    "remoteSideEffectsMayAlreadyExist":receipt.was_running}))
            },
            "run" => {
                if body["userInitiated"] != true { return Err("Explicit delegate run confirmation required".into()); }
                let provider = body["provider"].as_str().ok_or("AI delegate provider missing")?;
                let model = body["model"].as_str().ok_or("AI delegate model missing")?;
                let task = body["task"].as_str().ok_or("AI delegate task missing")?;
                if !valid_provider(provider) || provider == "ollama" || !valid_model(model) ||
                    task.len() > mobile_ai_delegation::MAX_TASK_BYTES {
                    return Err("Invalid Android delegate provider/model/task".into());
                }
                let request_id = body["requestId"].as_str().ok_or("AI delegate request UUID missing")?;
                Uuid::parse_str(request_id).map_err(|_| "Invalid AI delegate request UUID")?;
                let key = self.keys.lock().map_err(|_| "Provider keys unavailable")?
                    .get(provider).cloned().ok_or("Connect the delegate provider in Settings first")?;
                let skills = self.active_native_instructions(None)?;
                let (sender, mut cancelled) = tokio::sync::oneshot::channel::<()>();
                {
                    let mut active = self.chat_cancellations.lock()
                        .map_err(|_| "AI delegate cancellation unavailable")?;
                    if active.contains_key(request_id) || active.len() >= 3 {
                        return Err("AI delegate request UUID already active or request capacity full".into());
                    }
                    active.insert(request_id.to_owned(),sender);
                }
                let _chat_cleanup = ChatCleanup { engine:self, id:request_id.to_owned() };
                // The running slot and durable job ticket are reserved in
                // ONE transaction: a third concurrent request must not leave
                // a queued orphan that cannot be started from this UI.
                let (job, ticket) = store.submit_and_start(provider,model,task,true)?;
                let mut cleanup = DelegateRunCleanup {
                    engine:self, store:store.clone(), job_id:job.id.clone(),
                    ticket_id:ticket.ticket_id.clone(), settled:false,
                };
                {
                    let mut active = self.ai_delegate_active.lock()
                        .map_err(|_| "AI delegate registry unavailable")?;
                    if active.len() >= 2 {
                        return Err("Two other AI delegate requests are already running".into());
                    }
                    active.insert(job.id.clone(),(ticket.ticket_id.clone(),request_id.to_owned()));
                }
                self.chat_progress.lock().map_err(|_| "AI delegate progress unavailable")?
                    .insert(request_id.to_owned(), String::new());
                // A Prime Cancel can happen after its reservation but before
                // this delegate route registers a cancellation channel. Once
                // registered, check the durable worker state one final time
                // before the HTTP provider future can be polled; later Cancel
                // uses the same channel and the biased cancellation branch.
                let prime_task_id = body.get("primeTaskId").and_then(Value::as_str);
                let prime_worker_id = body.get("primeWorkerId").and_then(Value::as_str);
                if prime_task_id.is_some() || prime_worker_id.is_some() {
                    let task_id = prime_task_id.ok_or("Prime task ID required for delegate")?;
                    let worker_id = prime_worker_id.ok_or("Prime worker ID required for delegate")?;
                    if !self.prime_store()?.still_running(task_id,worker_id,request_id)? {
                        return Err("Prime worker was cancelled before provider dispatch".into());
                    }
                }
                let instruction = format!("Complete the following one bounded delegated task as an AI assistant. Return a useful answer but never claim you accessed device files, executed tools, called external MCP, or modified the user's project. No further autonomous tasks or agent delegation are permitted.\n\nTask:\n{task}");
                let history = [Message { role:"user".into(), text:instruction }];
                let request = self.provider_answer(provider,model,&key,&history,&skills,Some(request_id),None);
                let outcome = tokio::select! {
                    biased;
                    _ = &mut cancelled => Err("AI delegate cancelled; no answer was persisted".into()),
                    response = request => response,
                };
                let was_registered = self.chat_cancellations.lock()
                    .map_err(|_| "AI delegate cancellation unavailable")?
                    .remove(request_id).is_some();
                self.chat_progress.lock().map_err(|_| "AI delegate progress unavailable")?
                    .remove(request_id);
                let outcome = if was_registered { outcome }
                    else { Err("AI delegate was cancelled; no answer was persisted".into()) };
                match outcome {
                    Ok(answer) => {
                        // The store requires this exact still-running ticket.
                        // Cancellation, stale callbacks and restart cannot turn
                        // an interrupted job into an invented success.
                        let saved = store.complete(&ticket.job_id,&ticket.ticket_id,&answer)?;
                        cleanup.settled = true;
                        Ok(json!({"job":saved,"requestId":request_id,"providerReceipt":true,
                            "modelToolsExecuted":false,"backgroundReplay":false}))
                    },
                    Err(error) => {
                        // No raw provider payload/result is used as an error;
                        // job status persists, with a bounded static reason.
                        if store.fail(&ticket.job_id,&ticket.ticket_id,
                            "Provider call failed or was cancelled; no answer verified").is_ok() {
                            cleanup.settled = true;
                        }
                        Err(error)
                    }
                }
            },
            _ => Err("Unsupported AI delegate action".into())
        }
    }

    /// An AI-backed Goal turn still requires an explicit user gesture and an
    /// existing, approved, ready checkpoint. The actual provider answer is
    /// returned as evidence for manual review; it NEVER marks the step done.
    /// One request cannot start another Goal turn, execute MCP, or continue
    /// automatically after the Android Activity is dropped/recreated.
    async fn run_mobile_goal_step(&self, body: &Value) -> Result<Value, String> {
        if !cfg!(target_os = "android") { return Err("AI Goal turns require Android".into()); }
        if body["userInitiated"] != true { return Err("Goal AI turn needs a user gesture".into()); }
        let provider = body["provider"].as_str().ok_or("Goal provider missing")?;
        let model = body["model"].as_str().ok_or("Goal model missing")?;
        if !valid_provider(provider) || provider == "ollama" || !valid_model(model) {
            return Err("Choose a supported configured Android provider/model".into());
        }
        let request_id = body["requestId"].as_str().ok_or("Goal request ID missing")?;
        let parsed_request = Uuid::parse_str(request_id).map_err(|_| "Invalid Goal request ID")?;
        if parsed_request.hyphenated().to_string() != request_id
            || parsed_request.get_version_num() != 4 {
            return Err("Goal request needs a canonical UUID v4".into());
        }
        let delegate_to_subagent = match body.get("delegateToSubagent") {
            None => false,
            Some(Value::Bool(value)) => *value,
            _ => return Err("Goal AI subagent selection must be boolean".into()),
        };
        let allow_mcp_tools = match body.get("allowMcpTools") {
            None => false,
            Some(Value::Bool(value)) => *value,
            _ => return Err("Goal MCP opt-in selection must be boolean".into()),
        };
        if delegate_to_subagent && allow_mcp_tools {
            return Err("Goal MCP tools and AI subagent delegation cannot be combined".into());
        }
        let key = self.keys.lock().map_err(|_| "Provider keys unavailable")?
            .get(provider).cloned().ok_or("Connect the selected provider first")?;
        let skills = self.active_native_instructions(None)?;
        let mcp_context = if allow_mcp_tools {
            let connected = self.mcp.lock().map_err(|_| "MCP connection unavailable")?
                .clone().ok_or("Connect a verified MCP server before enabling Goal tools")?;
            if !connected.same_session(&connected) {
                return Err("Goal MCP server session is no longer live".into());
            }
            let tools = connected.tool_definitions();
            if tools.is_empty() || tools.len() > 16 {
                return Err("Goal MCP requires 1–16 verified tool definitions".into());
            }
            let generation = self.mcp_generation.load(Ordering::SeqCst);
            if generation == 0 { return Err("Goal MCP generation is unavailable".into()); }
            Some((connected,generation,tools))
        } else { None };

        // Scope the synchronous MutexGuard so the Android Tauri command's
        // async future remains Send across the later provider await.
        {
            let mut claimed = self.goal_ai_inflight.lock().map_err(|_| "Goal runner unavailable")?;
            if claimed.is_some() { return Err("Another AI Goal turn is in progress".into()); }
            *claimed = Some(request_id.to_owned());
        }
        let mut goal_cleanup = GoalAiCleanup {
            engine:self, request_id:request_id.to_owned(), turn:None,
            settled:false,external_effect_uncertain:false,
        };
        let (sender, mut cancelled) = tokio::sync::oneshot::channel::<()>();
        {
            let mut active = self.chat_cancellations.lock()
                .map_err(|_| "Goal cancellation unavailable")?;
            if active.len() >= 3 || active.contains_key(request_id) {
                return Err("Goal request UUID is in use or request capacity is reached".into());
            }
            active.insert(request_id.to_owned(), sender);
        }
        let _chat_cleanup = ChatCleanup { engine:self, id:request_id.to_owned() };
        self.chat_progress.lock().map_err(|_| "Goal progress unavailable")?
            .insert(request_id.to_owned(), String::new());

        // An explicit Goal-to-AI-subagent selection reserves one *real*,
        // durable delegate before the Goal turn begins. Atomic admission
        // forbids the old queued-orphan race at capacity; failure leaves the
        // Goal Ready without consuming the retry budget or claiming work.
        let (ticket, original, mut delegate_cleanup) = {
            let mut guard = self.goal.lock().map_err(|_| "Goal coordinator unavailable")?;
            self.load_saved_goal(&mut guard)?;
            let driver = guard.as_mut().ok_or("Create a Goal before running an AI step")?;
            let snapshot = driver.snapshot();
            if snapshot.status != mobile_goal::GoalStatus::Ready {
                return Err("Approve the Goal step or finish the current turn first".into());
            }
            let instruction = goal_ai_step_prompt(&snapshot)?;
            let cleanup = if delegate_to_subagent {
                let mut active = self.ai_delegate_active.lock()
                    .map_err(|_| "Goal subagent capacity unavailable")?;
                if active.len() >= 2 { return Err("Two AI subagents are already running".into()); }
                let store = self.delegation_store()?;
                let (job, task_ticket) = store.submit_and_start(
                    provider, model, &instruction, true)?;
                active.insert(job.id.clone(),
                    (task_ticket.ticket_id.clone(), request_id.to_owned()));
                Some(DelegateRunCleanup {engine:self, store,
                    job_id:job.id, ticket_id:task_ticket.ticket_id, settled:false})
            } else { None };
            let ticket = driver.begin_turn()?;
            (ticket, snapshot, cleanup)
        };
        goal_cleanup.turn = Some((original.id.clone(),ticket.id.clone()));
        let prompt = goal_ai_step_prompt(&original)?;
        let messages = [Message { role:"user".into(), text:prompt }];
        let provider_turn = async {
            match &mcp_context {
                Some((_,_,tools)) => self.provider_tool_offer(
                    provider,model,&key,&messages,&skills,tools,None).await,
                None => self.provider_answer(provider, model, &key, &messages, &skills,
                    Some(request_id),None).await.map(provider_tools::ProviderTurn::Text),
            }
        };
        let result = tokio::select! {
            biased;
            _ = &mut cancelled => Err("Goal provider request cancelled".into()),
            answer = provider_turn => answer,
        };
        let registered = self.chat_cancellations.lock()
            .map_err(|_| "Goal cancellation unavailable")?
            .remove(request_id).is_some();
        self.chat_progress.lock().map_err(|_| "Goal progress unavailable")?.remove(request_id);
        let result = if registered { result }
            else { Err("Goal provider request cancelled".into()) };
        let mut guard = self.goal.lock().map_err(|_| "Goal coordinator unavailable")?;
        let driver = guard.as_mut().ok_or("Goal was archived during provider request")?;
        let latest = driver.snapshot();
        let still_current = latest.id == original.id
            && latest.status == mobile_goal::GoalStatus::InTurn
            && latest.active_turn.as_ref().is_some_and(|turn| turn.id == ticket.id);
        if !still_current {
            return Err("Goal changed during the AI request; no outcome was saved".into());
        }
        match result {
            Ok(provider_tools::ProviderTurn::ToolProposal(proposal)) => {
                let (connection,generation,tools) = mcp_context
                    .ok_or("Goal MCP proposal was not explicitly enabled")?;
                validate_provider_mcp_proposal(&tools,&proposal)?;
                if self.mcp_generation.load(Ordering::SeqCst) != generation
                    || !self.mcp.lock().map_err(|_| "MCP unavailable")?
                        .as_ref().is_some_and(|current| current.same_session(&connection)) {
                    return Err("Goal MCP server changed before user review".into());
                }
                let mut pending = self.pending_goal_tool.lock()
                    .map_err(|_| "Goal MCP approval state unavailable")?;
                if pending.is_some() {
                    return Err("Another Goal tool proposal already awaits user review".into());
                }
                let lease_dir = self.home.join("goal-mcp-state");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    let mut builder = fs::DirBuilder::new();
                    builder.recursive(true).mode(0o700);
                    builder.create(&lease_dir)
                        .map_err(|_| "Could not prepare owner-private Goal MCP ledger")?;
                }
                #[cfg(not(unix))]
                fs::create_dir_all(&lease_dir)
                    .map_err(|_| "Could not prepare private Goal MCP ledger")?;
                let lease = goal_mcp::GoalMcpLease::propose(
                    &lease_dir,&latest,request_id,generation,&proposal,&tools,120_000)?;
                let ledger = lease.snapshot()?;
                let review_id = ledger.review_id.clone();
                *pending = Some(PendingGoalMcpTurn {
                    lease,request_id:request_id.to_owned(),ticket_id:ticket.id.clone(),
                    provider:provider.to_owned(),model:model.to_owned(),instructions:skills,
                    context:messages.to_vec(),tools,
                    connection,connection_generation:generation,
                });
                goal_cleanup.settled = true;
                Ok(json!({"goal":latest,"ticket":ticket,"answer":null,
                    "requestId":request_id,"awaitingUserOutcome":false,
                    "pendingGoalTool":{"reviewId":review_id,"requestId":request_id,
                        "goalId":original.id,"turnId":ticket.id,"name":proposal.name,
                        "arguments":proposal.arguments,"expiresInSeconds":120},
                    "delegatedToSubagent":false,"delegateJobId":null,
                    "delegatePersisted":false,"modelToolsExecuted":false,
                    "savedAsComplete":false}))
            },
            Ok(provider_tools::ProviderTurn::Text(answer)) => {
                let delegated_job_id = if let Some(ref mut cleanup) = delegate_cleanup {
                    match cleanup.store.complete(&cleanup.job_id, &cleanup.ticket_id, &answer) {
                        Ok(saved) => {
                            cleanup.settled = true;
                            Some(saved.id)
                        },
                        Err(_) => {
                            driver.mark_turn_interrupted(&ticket.id,
                                "Subagent answer could not be verified and persisted")?;
                            return Err("Subagent answer not durably verified; Goal step interrupted".into());
                        },
                    }
                } else { None };
                goal_cleanup.settled = true;
                Ok(json!({"goal":latest,"ticket":ticket,"answer":answer,
                    "requestId":request_id,"awaitingUserOutcome":true,
                    "delegatedToSubagent":delegate_to_subagent,
                    "delegateJobId":delegated_job_id,
                    "delegatePersisted":delegated_job_id.is_some(),
                    "modelToolsExecuted":false,"savedAsComplete":false}))
            },
            Err(error) => {
                driver.mark_turn_interrupted(&ticket.id,
                    "Provider request failed or was cancelled; no completion was verified")?;
                goal_cleanup.settled = true;
                Err(error)
            }
        }
    }

    /// This is a NEW explicit user action after a bounded AI Goal proposal.
    /// Consent dispatches at most one verified remote MCP call, followed by
    /// one actual provider continuation; the Goal remains InTurn until the
    /// user records their own step outcome. Uncertain dispatches never replay.
    async fn reviewed_goal_tool(&self, body: &Value) -> Result<Value, String> {
        if !cfg!(target_os = "android") { return Err("Goal MCP review is Android-only".into()); }
        if body["userInitiated"] != true {
            return Err("Goal MCP needs a separate explicit user decision".into());
        }
        let action = body["action"].as_str().ok_or("Goal MCP decision missing")?;
        if !matches!(action,"approve" | "reject") {
            return Err("Unsupported Goal MCP decision".into());
        }
        let review_id = body["reviewId"].as_str().ok_or("Goal MCP review UUID missing")?;
        let request_id = body["requestId"].as_str().ok_or("Goal MCP request UUID missing")?;
        let goal_id = body["goalId"].as_str().ok_or("Goal MCP Goal UUID missing")?;
        let turn_id = body["turnId"].as_str().ok_or("Goal MCP turn UUID missing")?;
        for id in [review_id,request_id,goal_id,turn_id] {
            let uuid = Uuid::parse_str(id).map_err(|_| "Goal MCP identity must be UUID")?;
            if uuid.hyphenated().to_string() != id {
                return Err("Goal MCP identity must be canonical UUID".into());
            }
        }
        let mut turn = {
            let mut guard = self.pending_goal_tool.lock()
                .map_err(|_| "Goal MCP pending proposal unavailable")?;
            let pending = guard.as_ref().ok_or("No pending Goal MCP proposal")?;
            let snapshot = pending.lease.snapshot()?;
            if snapshot.review_id != review_id || snapshot.goal_id != goal_id
                || snapshot.turn_id != turn_id || pending.ticket_id != turn_id
                || pending.request_id != request_id {
                return Err("Goal MCP approval identity did not match pending ticket".into());
            }
            // Hold both locks until the approval/rejection has reserved the
            // Goal runner. A racing manual Finish must never see neither a
            // pending proposal nor an active Goal request.
            let mut active = self.goal_ai_inflight.lock()
                .map_err(|_| "Goal runner unavailable")?;
            if active.is_some() {
                return Err("Another Goal request is already in progress".into());
            }
            *active = Some(request_id.to_owned());
            guard.take().ok_or("Goal MCP proposal already consumed")?
        };
        // Taking the one-time proposal MUST NOT strand an InTurn Goal if a
        // key disappears, request slots fill, or pre-dispatch validation fails.
        // Until durable dispatch_uncertain is issued, the request remains
        // safely retryable only through a new explicit user action.
        let mut cleanup = GoalAiCleanup {engine:self,request_id:request_id.to_owned(),
            turn:Some((goal_id.to_owned(),turn_id.to_owned())),settled:false,
            external_effect_uncertain:false};
        if action == "reject" {
            turn.lease.reject(review_id,true)?;
            let mut guard = self.goal.lock().map_err(|_| "Goal coordinator unavailable")?;
            let driver = guard.as_mut().ok_or("Goal no longer exists")?;
            let current = driver.snapshot();
            if current.id != goal_id || current.status != mobile_goal::GoalStatus::InTurn
                || current.active_turn.as_ref().is_none_or(|ticket| ticket.id != turn_id) {
                return Err("Goal changed; proposal rejected without tool execution".into());
            }
            let goal = driver.mark_turn_interrupted(turn_id,
                "User rejected proposed Goal MCP tool; no tool was executed")?;
            cleanup.settled = true;
            return Ok(json!({"rejected":true,"executed":false,"goal":goal}));
        }
        let (key,key_generation) = {
            let keys = self.keys.lock().map_err(|_| "Provider keys unavailable")?;
            let secret = keys.get(&turn.provider).cloned()
                .ok_or("Provider key removed before Goal approval")?;
            (secret,self.keys_generation.load(Ordering::SeqCst))
        };
        let (sender,mut cancelled) = tokio::sync::oneshot::channel::<()>();
        {
            let mut active = self.chat_cancellations.lock()
                .map_err(|_| "Goal MCP cancellation unavailable")?;
            if active.len() >= 3 || active.contains_key(request_id) {
                return Err("Goal MCP request ID already active or capacity exceeded".into());
            }
            active.insert(request_id.to_owned(),sender);
        }
        let _chat_cleanup = ChatCleanup {engine:self,id:request_id.to_owned()};
        // The Goal lock remains held through approval and the persisted
        // possible-dispatch marker. Concurrent Goal cancellation cannot make
        // a stale snapshot appear current during this authorization window.
        let (dispatch_ticket,actual_call) = {
            let guard = self.goal.lock().map_err(|_| "Goal coordinator unavailable")?;
            let goal = guard.as_ref().ok_or("Goal no longer exists")?.snapshot();
            if goal.id != goal_id || goal.status != mobile_goal::GoalStatus::InTurn
                || goal.active_turn.as_ref().is_none_or(|ticket| ticket.id != turn_id) {
                return Err("Goal changed before tool execution; nothing dispatched".into());
            }
            if self.mcp_generation.load(Ordering::SeqCst) != turn.connection_generation
                || !self.mcp.lock().map_err(|_| "MCP unavailable")?.as_ref()
                    .is_some_and(|current| current.same_session(&turn.connection)) {
                return Err("MCP server changed; Goal tool not dispatched".into());
            }
            let live_tools = turn.connection.tool_definitions();
            turn.lease.approve(&goal,review_id,true)?;
            let permit = turn.lease.begin_dispatch(
                &goal,review_id,turn.connection_generation,&live_tools)?;
            // From this durable point onward the remote call may have side
            // effects. If the request drops/fails the Goal must enter an
            // explicit manual reconciliation gate, never auto retry.
            cleanup.external_effect_uncertain = true;
            permit.into_call()
        };
        // Both ledgers are metadata-only. If this audit cannot be committed,
        // NO remote call starts even though the Goal lease conservatively
        // records a dispatch-uncertain state that cannot be replayed.
        tool_audit::before_dispatch(&self.home,review_id,request_id,None,&actual_call.name)?;
        let execute = async {
            let mut connection = turn.connection.clone();
            let response = connection.call_tool(&actual_call.name,actual_call.arguments.clone()).await?;
            if self.mcp_generation.load(Ordering::SeqCst) != turn.connection_generation
                || !self.mcp.lock().map_err(|_| "MCP unavailable")?.as_ref()
                    .is_some_and(|current| current.same_session(&turn.connection)) {
                return Err("MCP disconnected during approved Goal tool; remote effects uncertain".into());
            }
            if response["isError"] == true {
                turn.lease.report_outcome(&dispatch_ticket,goal_mcp::RemoteOutcome::ReportedError)?;
                tool_audit::advance(&self.home,review_id,"server_reported_error")?;
                return Err("Goal MCP server reported tool error; no automatic retry".into());
            }
            let checked = mcp_client::provider_tool_result(&response["result"])?;
            let result: Value = serde_json::from_str(&checked)
                .map_err(|_| "Goal MCP tool result was not bounded JSON")?;
            turn.lease.report_outcome(&dispatch_ticket,goal_mcp::RemoteOutcome::Confirmed)?;
            tool_audit::advance(&self.home,review_id,"server_confirmed")?;
            let mut history = turn.context.iter().map(|msg|
                json!({"role":msg.role,"content":msg.text})).collect::<Vec<_>>();
            if !turn.instructions.is_empty() {
                history.insert(0,json!({"role":"system","content":format!(
                    "User-enabled response instructions:\n\n{}",turn.instructions.join("\n\n"))}));
            }
            let payload = provider_tools::continuation_payload(
                &turn.provider,&turn.model,&history,&turn.tools,&actual_call,&result)?;
            // Revoking or replacing the provider key while the MCP request
            // was in flight revokes authority to start a new model request.
            // A tool call already dispatched cannot be undone.
            {
                let keys = self.keys.lock().map_err(|_| "Provider keys unavailable")?;
                if self.keys_generation.load(Ordering::SeqCst) != key_generation
                    || keys.get(&turn.provider) != Some(&key) {
                    return Err("Provider key changed; Goal MCP provider continuation not started".into());
                }
            }
            self.provider_tool_resume(&turn.provider,&key,&payload,None).await
        };
        let result = tokio::select! {
            biased;
            _ = &mut cancelled => Err("Approved Goal MCP request cancelled; remote effects may have occurred".into()),
            response = execute => response,
        };
        let registered = self.chat_cancellations.lock()
            .map_err(|_| "Goal MCP cancellation state unavailable")?
            .remove(request_id).is_some();
        if !registered { return Err("Goal MCP request was cancelled; outcome unverified".into()); }
        let answer = result?;
        let goal = self.goal.lock().map_err(|_| "Goal coordinator unavailable")?
            .as_ref().ok_or("Goal removed before provider continuation")?.snapshot();
        if goal.id != goal_id || goal.status != mobile_goal::GoalStatus::InTurn
            || goal.active_turn.as_ref().is_none_or(|ticket| ticket.id != turn_id) {
            return Err("Goal changed during MCP request; no completion was saved".into());
        }
        cleanup.settled = true;
        Ok(json!({"goal":goal,"answer":answer,"requestId":request_id,
            "awaitingUserOutcome":true,"modelToolsExecuted":true,
            "toolExecutionConfirmed":true,"savedAsComplete":false,
            "reviewId":review_id,"ticket":goal.active_turn}))
    }

    /// AI-generated goals are only DRAFTS; they never alter goal-state,
    /// start a turn, or grant access to tools. User must review the returned
    /// specification and separately use the existing Create Goal action.
    async fn draft_mobile_goal_plan(&self, body: &Value) -> Result<Value, String> {
        if !cfg!(target_os = "android") {
            return Err("AI Goal planning requires Android".into());
        }
        if body["userInitiated"] != true {
            return Err("Explicit user request required to draft a Goal".into());
        }
        let provider = body["provider"].as_str().ok_or("Goal plan provider missing")?;
        let model = body["model"].as_str().ok_or("Goal plan model missing")?;
        if !valid_provider(provider) || provider == "ollama" || !valid_model(model) {
            return Err("Invalid Goal plan provider/model".into());
        }
        let objective = body["objective"].as_str().ok_or("Goal objective missing")?.trim();
        if objective.is_empty() || objective.len() > 1_000 ||
            objective.chars().any(|ch| ch.is_control() || matches!(ch,
                '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')) {
            return Err("Goal objective must contain 1-1000 printable UTF-8 bytes".into());
        }
        let request_id = body["requestId"].as_str().ok_or("Goal plan request UUID missing")?;
        let parsed_id = Uuid::parse_str(request_id)
            .map_err(|_| "Invalid Goal plan request UUID")?;
        if parsed_id.hyphenated().to_string() != request_id {
            return Err("Goal plan requires a canonical UUID".into());
        }
        let key = self.keys.lock().map_err(|_| "Provider keys unavailable")?
            .get(provider).cloned().ok_or("Connect the selected Goal provider first")?;
        // Only explicitly enabled, bounded Skills enter the provider context.
        // Drafting never executes a Skill script or automatically enables one.
        let skills = self.active_native_instructions(None)?;
        let (sender, mut cancelled) = tokio::sync::oneshot::channel::<()>();
        {
            let mut active = self.chat_cancellations.lock()
                .map_err(|_| "Goal plan cancellation unavailable")?;
            if active.len() >= 3 || active.contains_key(request_id) {
                return Err("Goal plan request is already active or capacity reached".into());
            }
            active.insert(request_id.to_owned(),sender);
        }
        let _cleanup = ChatCleanup { engine:self, id:request_id.to_owned() };
        self.chat_progress.lock().map_err(|_| "Goal plan progress unavailable")?
            .insert(request_id.to_owned(),String::new());
        let prompt = format!(
            concat!(
                "Suggest a small, reviewable plan for the user's goal. ",
                "Return ONLY one JSON object, no Markdown or commentary. ",
                "Fields: title (string up to 200 UTF-8 bytes), steps ",
                "(1 to 16 objects with description up to 240 bytes and ",
                "requires_approval boolean), max_turns (integer 1 to 64, ",
                "at least the number of steps), max_retries_per_step ",
                "(integer 0 to 5). Do not claim tools, device files, ",
                "network requests or other side effects were executed. ",
                "This is a future plan requiring the user's separate review.\n\n",
                "User goal:\n{}"
            ),
            objective
        );
        let history = [Message { role:"user".into(), text:prompt }];
        let request = self.provider_answer(provider,model,&key,&history,&skills,Some(request_id),None);
        let result = tokio::select! {
            biased;
            _ = &mut cancelled => Err("Goal draft request cancelled".into()),
            response = request => response,
        };
        let registered = self.chat_cancellations.lock()
            .map_err(|_| "Goal plan cancellation unavailable")?
            .remove(request_id).is_some();
        self.chat_progress.lock().map_err(|_| "Goal plan progress unavailable")?
            .remove(request_id);
        if !registered { return Err("Goal draft request no longer active".into()); }
        let raw = result?;
        let draft = goal_plan::parse_draft(&raw)?;
        Ok(json!({"draft":draft,"requestId":request_id,"requiresReview":true,
            "goalCreated":false,"turnStarted":false,"toolsExecuted":false}))
    }

    /// Called ONLY when a new Engine reacquires a checkpoint after process
    /// restart. A previous provider future is gone; it must not remain a
    /// falsely-running Goal turn. Remote MCP dispatch uncertainty is retained
    /// for manual inspection, never converted into a retryable auto-run.
    fn recover_goal_checkpoint_after_restart(
        &self, driver: &mut mobile_goal::GoalDriver,
    ) -> Result<(), String> {
        let goal = driver.snapshot();
        if goal.status != mobile_goal::GoalStatus::InTurn { return Ok(()); }
        let ticket = goal.active_turn.as_ref().ok_or("Persisted Goal turn missing")?;
        let ledger_dir = self.home.join("goal-mcp-state");
        let ledger_file = ledger_dir.join(format!("goal-mcp-{}.json",ticket.id));
        let checkpoint_exists = match fs::symlink_metadata(&ledger_file) {
            Ok(_) => true,
            Err(err) if err.kind() == io::ErrorKind::NotFound => false,
            Err(_) => return Err("Cannot inspect Goal MCP recovery ledger".into()),
        };
        if checkpoint_exists {
            let lease = goal_mcp::GoalMcpLease::open(&ledger_dir,&ticket.id)?;
            let recovered = lease.snapshot()?;
            if recovered.goal_id != goal.id || recovered.turn_id != ticket.id {
                return Err("Goal MCP recovery identity mismatch".into());
            }
            if matches!(recovered.status,goal_mcp::ReviewStatus::DispatchUncertain
                | goal_mcp::ReviewStatus::Confirmed | goal_mcp::ReviewStatus::ReportedError) {
                // External action may have occurred. A recovered step must
                // require a new explicit manual gate, not automatically
                // qualify for another provider/tool turn.
                driver.record_outcome(&ticket.id,mobile_goal::TurnOutcome::NeedsApproval(
                    "External MCP action may have occurred before restart. Reconcile its remote effects before authorizing further Goal execution".into()))?;
                return Ok(());
            }
            if !matches!(recovered.status, goal_mcp::ReviewStatus::Interrupted
                | goal_mcp::ReviewStatus::Rejected | goal_mcp::ReviewStatus::Expired) {
                return Err("Goal MCP recovery ledger is in an unexpected state".into());
            }
            driver.mark_turn_interrupted(&ticket.id,
                "Goal MCP proposal was interrupted after restart; no remote tool dispatched")?;
        } else {
            // Plain provider/delegate responses are not persisted as completed
            // user outcomes; no live request survived process death.
            driver.mark_turn_interrupted(&ticket.id,
                "Goal provider turn was interrupted by process restart; no completion verified")?;
        }
        Ok(())
    }

    /// All entry points must share this first-open reconciliation. Otherwise
    /// opening Review or pressing Run AI before Status could cache an orphaned
    /// InTurn checkpoint and permanently bypass restart recovery.
    fn load_saved_goal(&self, slot: &mut Option<mobile_goal::GoalDriver>)
        -> Result<(), String> {
        if slot.is_some() { return Ok(()); }
        let root = self.home.join("goal-state");
        if root.join("goal-v1.json").exists() {
            let mut driver = mobile_goal::GoalDriver::open(&root)?;
            self.recover_goal_checkpoint_after_restart(&mut driver)?;
            *slot = Some(driver);
        }
        Ok(())
    }

    fn mobile_goal_request(&self, action: &str, payload: &Value) -> Result<Value, String> {
        if !cfg!(target_os = "android") { return Err("Android goals only".into()); }
        // Reading a checkpoint is harmless. Creating, approving, advancing,
        // changing or archiving it must always originate from an explicit
        // Android user action; neither a provider nor a restored Activity
        // may silently mutate the user's Goal via this route.
        if action != "status" && payload["userInitiated"] != true {
            return Err("Goal state changes require an explicit user action".into());
        }
        let root = self.home.join("goal-state");
        let mut guard = self.goal.lock().map_err(|_| "Goal coordinator unavailable")?;
        if guard.is_none() && action != "create" {
            self.load_saved_goal(&mut guard)?;
        }
        if action == "create" {
            if guard.is_some() || root.join("goal-v1.json").exists() {
                return Err("A saved goal already exists. Keep or archive it before creating another".into());
            }
            let spec: mobile_goal::GoalSpec = serde_json::from_value(payload["spec"].clone())
                .map_err(|_| "Invalid goal specification")?;
            fs::create_dir_all(&root).map_err(|_| "Could not create private goal directory")?;
            let driver = mobile_goal::GoalDriver::create(&root, spec)?;
            let snapshot = driver.snapshot();
            *guard = Some(driver);
            return Ok(json!({"goal":snapshot, "mode":"Explicit manual checkpoints; no background AI loop"}));
        }
        if action == "archive" {
            let current = guard.as_ref().ok_or("No goal to archive")?.snapshot();
            if !current.status.is_terminal() {
                return Err("Cancel or complete the saved goal before archiving".into());
            }
            Uuid::parse_str(&current.id).map_err(|_| "Invalid goal ID")?;
            let archive_dir = self.home.join("goal-archive");
            fs::create_dir_all(&archive_dir).map_err(|_| "Could not prepare private goal archive")?;
            let count = fs::read_dir(&archive_dir).map_err(|_| "Goal archive unavailable")?.count();
            if count >= 25 { return Err("Goal archive is full; no history was deleted".into()); }
            let target = archive_dir.join(format!("{}.json", current.id));
            if target.exists() { return Err("Goal is already archived".into()); }
            drop(guard.take());
            fs::rename(root.join("goal-v1.json"), &target)
                .map_err(|_| "Goal could not be archived; the saved checkpoint was not deleted")?;
            return Ok(json!({"archived":true,"goalId":current.id,"newGoalAllowed":true}));
        }
        if action == "status" && guard.is_none() {
            return Ok(json!({"goal":null, "mode":"No active goal; no autonomous execution"}));
        }
        let driver = guard.as_mut().ok_or("No goal exists")?;
        if action == "finish" && self.pending_goal_tool.lock()
            .map_err(|_| "Goal MCP pending review unavailable")?.is_some() {
            return Err("Resolve or explicitly reject the pending Goal MCP proposal before recording a step outcome".into());
        }
        if action == "finish" && self.goal_ai_inflight.lock()
            .map_err(|_| "Goal runner unavailable")?.is_some() {
            return Err("Wait for the active provider request before recording its outcome".into());
        }
        if matches!(action, "cancel" | "interrupt") {
            if let Some(id) = self.goal_ai_inflight.lock()
                .map_err(|_| "Goal runner unavailable")?.clone() {
                let _ = self.cancel_chat(&id);
            }
        }
        let snapshot = match action {
            "status" => driver.snapshot(),
            "begin" => {
                let ticket = driver.begin_turn()?;
                return Ok(json!({"ticket":ticket,"goal":driver.snapshot()}));
            },
            "finish" => {
                let ticket_id = payload["turnId"].as_str().ok_or("Goal turn ID missing")?;
                let detail = payload["detail"].as_str().ok_or("Goal outcome detail missing")?.to_owned();
                let outcome = match payload["result"].as_str().ok_or("Goal outcome type missing")? {
                    "completed" => mobile_goal::TurnOutcome::Completed(detail),
                    "retryable" => mobile_goal::TurnOutcome::RetryableFailure(detail),
                    "terminal" => mobile_goal::TurnOutcome::TerminalFailure(detail),
                    "approval" => mobile_goal::TurnOutcome::NeedsApproval(detail),
                    _ => return Err("Invalid goal outcome".into())
                };
                driver.record_outcome(ticket_id, outcome)?
            },
            "interrupt" => driver.mark_turn_interrupted(
                payload["turnId"].as_str().ok_or("Goal turn ID missing")?,
                payload["reason"].as_str().ok_or("Reason missing")?)?,
            "pause" => {
                if payload["userInitiated"] != true {
                    return Err("Goal pause requires an explicit user action".into());
                }
                driver.pause()?
            },
            "resume" => {
                if payload["userInitiated"] != true {
                    return Err("Goal resume requires an explicit user action".into());
                }
                driver.resume()?
            },
            "approval" => driver.resolve_approval(
                payload["approved"].as_bool().ok_or("Approval decision missing")?,
                payload["note"].as_str().ok_or("Approval note missing")?)?,
            "cancel" => driver.cancel(payload["reason"].as_str().ok_or("Cancellation reason missing")?)?,
            _ => return Err("Unsupported goal action".into())
        };
        if matches!(action,"cancel" | "interrupt") {
            // A cancelled/interrupting Goal cannot retain a volatile proposal
            // that might later be accepted for a different checkpoint. The
            // metadata-only lease is rejected where possible; never claim a
            // remote call occurred or replay the unapproved arguments.
            let pending = self.pending_goal_tool.lock()
                .map_err(|_| "Goal MCP pending review unavailable")?.take();
            if let Some(mut pending) = pending {
                if let Ok(meta) = pending.lease.snapshot() {
                    if matches!(meta.status, goal_mcp::ReviewStatus::Pending
                        | goal_mcp::ReviewStatus::Approved) {
                        let _ = pending.lease.reject(&meta.review_id,true);
                    }
                }
            }
        }
        Ok(json!({"goal":snapshot, "progress":snapshot.progress_percent(),
            "mode":"Explicit manual checkpoints; no background AI loop"}))
    }

    /// Read-only metadata derived from the actual persisted GoalDriver.
    /// No step descriptions, approval notes, outcome details, or raw model/tool
    /// data may cross this Review route. Does not create or mutate a Goal.
    fn mobile_goal_review(&self) -> Result<Value, String> {
        if !cfg!(target_os = "android") { return Err("Android Goal Review only".into()); }
        let mut guard = self.goal.lock().map_err(|_| "Goal coordinator unavailable")?;
        self.load_saved_goal(&mut guard)?;
        let Some(driver) = guard.as_ref() else {
            return Ok(json!({"goal":null,"provenance":null}));
        };
        let snapshot = driver.snapshot();
        let provenance = goal_review::summarize(&snapshot)?;
        Ok(json!({"goalId":snapshot.id,"provenance":provenance}))
    }
    fn mobile_project_request(&self, action: &str, payload: &Value) -> Result<Value, String> {
        if !cfg!(target_os = "android") { return Err("Android project catalog only".into()); }
        let home = self.home.join("projects-state");
        let known_sessions: Vec<String> = self.sessions()?.iter().map(|s| s.id.clone()).collect();
        let catalog = match action {
            "status" => mobile_projects::list(&home)?,
            "create" => mobile_projects::create(&home,
                payload["name"].as_str().ok_or("Project name missing")?)?,
            "rename" => mobile_projects::rename(&home,
                payload["id"].as_str().ok_or("Project ID missing")?,
                payload["name"].as_str().ok_or("Project name missing")?)?,
            "remove" => mobile_projects::remove(&home,
                payload["id"].as_str().ok_or("Project ID missing")?)?,
            "assign" => mobile_projects::assign_chat(&home,
                payload["chatId"].as_str().ok_or("Chat ID missing")?,
                payload["projectId"].as_str(), &known_sessions)?,
            "review-import" => {
                let source = payload["legacy"].as_str().ok_or("Local project labels missing")?;
                let preview = mobile_projects::preview_legacy_import(&home, source, &known_sessions)?;
                let review_id = Uuid::new_v4().to_string();
                *self.project_import.lock().map_err(|_| "Project migration review unavailable")? =
                    Some((review_id.clone(), source.to_owned(), preview.clone()));
                return Ok(json!({"reviewId": review_id, "preview":preview,
                    "needsExplicitApproval":true, "legacyLabelsPreserved":true}));
            },
            "approve-import" => {
                let review_id = payload["reviewId"].as_str().ok_or("Project review ID missing")?;
                let (expected_id, raw, preview) = self.project_import.lock()
                    .map_err(|_| "Project migration review unavailable")?
                    .take().ok_or("Review project labels again")?;
                if review_id != expected_id { return Err("Project migration review expired; review again".into()); }
                mobile_projects::import_legacy_after_review(&home, &raw, &known_sessions, &preview)?
            },
            _ => return Err("Unsupported project catalog action".into())
        };
        Ok(json!({"catalog":catalog, "source":"Rust private project catalog",
            "sessionsMoved":false, "legacyLocalStoragePreserved":true}))
    }

    fn scoped_rules_request(&self, rules_body: Value) -> Result<Value, String> {
                let id = match rules_body.get("sessionId") {
                    Some(Value::Null) => None,
                    Some(Value::String(id)) => Some(id.as_str()),
                    _ => return Err("Standing rules require a real sessionId or null".into()),
                };
                if let Some(id) = id {
                    let parsed = Uuid::parse_str(id).map_err(|_| "Invalid chat session ID")?;
                    if parsed.hyphenated().to_string() != id ||
                        !self.sessions()?.iter().any(|session| session.id == id) {
                        return Err("Chat session no longer exists; rules not loaded or saved".into());
                    }
                }
                let _guard = if matches!(rules_body.get("action").and_then(Value::as_str), Some("save" | "reset")) {
                    Some(self.session_save_guard.lock().map_err(|_| "Chat storage unavailable")?)
                } else { None };
                // A session may be evicted between validation and acquiring
                // the save lock; revalidate while holding the same lock.
                if let Some(id) = id {
                    if !self.sessions()?.iter().any(|s| s.id == id) {
                        return Err("Chat session was removed; rules not saved".into());
                    }
                }
                standing_rules::dispatch_scoped(&self.home, &rules_body, id)
    }

    pub async fn request(&self, route: &str, body: Option<Value>) -> Result<Value, String> {
        match route {
            "/api/state" => self.state(),
            "/api/standing-rules" if cfg!(target_os = "android") =>
                self.scoped_rules_request(body.ok_or("Standing rules request missing")?),
            "/api/key" => {
                let b = body.ok_or("Key request missing")?;
                self.set_key(b["provider"].as_str().unwrap_or(""), b["key"].as_str().unwrap_or(""))
            },
            "/api/chat" => {
                let input: ChatRequest = serde_json::from_value(body.ok_or("Message missing")?)
                    .map_err(|_| "Invalid chat request")?;
                self.chat(input).await
            },
            "/api/chat-local-action" if cfg!(target_os="android") => {
                self.run_direct_local_chat_action(body.ok_or("Native chat action missing")?).await
            },
            "/api/chat-cancel" if cfg!(target_os = "android") => {
                let b = body.ok_or("Cancellation request missing")?;
                self.cancel_chat(b["requestId"].as_str().ok_or("Request ID missing")?)
            },
            "/api/chat-progress" if cfg!(target_os = "android") => {
                let b = body.ok_or("Chat progress request missing")?;
                self.chat_stream_progress(b["requestId"].as_str().ok_or("Request ID missing")?)
            },
            "/api/chat-tool-review" if cfg!(target_os = "android") => {
                let b = body.ok_or("Tool review action missing")?;
                self.reviewed_model_tool(&b).await
            },
            "/api/mcp-audit" if cfg!(target_os = "android") => {
                Ok(json!({"events":tool_audit::list(&self.home)?,
                    "meaning":"Approval and remote response metadata; no arguments or tool results stored"}))
            },
            "/api/workspace" => {
                let b = body.ok_or("Workspace request missing")?;
                let path = b["path"].as_str().ok_or("Workspace path missing")?;
                match b["action"].as_str().unwrap_or("") {
                    "list" => workspace_tools::list(&self.home, path),
                    "read" => workspace_tools::read(&self.home, path),
                    "properties" => workspace_tools::properties(&self.home, path),
                    "create_text" if cfg!(target_os = "android") => workspace_tools::create_text(
                        &self.home, path, b["content"].as_str().ok_or("Text missing")?,
                        b["allowWrite"] == true),
                    "replace_text" if cfg!(target_os = "android") => workspace_tools::replace_text(
                        &self.home, path, b["content"].as_str().ok_or("Text missing")?,
                        b["allowWrite"] == true),
                    _ => Err("Unsupported workspace action".into())
                }
            },
            "/api/workspace-search" => {
                let b = body.ok_or("Workspace search missing")?;
                let query = b["query"].as_str().ok_or("Search query missing")?;
                match b["mode"].as_str() {
                    None if b["mode"].is_null() => workspace_tools::search(&self.home, query),
                    Some("name") =>
                        workspace_tools::search(&self.home, query),
                    Some("content") if cfg!(target_os = "android") && b["userInitiated"] == true =>
                        workspace_tools::search_text(&self.home, query),
                    Some("content") => Err("Content search requires explicit Android user action".into()),
                    _ => Err("Unsupported workspace search mode".into()),
                }
            },
            "/api/terminal" => {
                let b = body.ok_or("Terminal command missing")?;
                let command = b["command"].as_str().ok_or("Invalid terminal command")?;
                workspace_tools::terminal(&self.home, command)
            },
            "/api/mobile-skills" if cfg!(target_os = "android") => {
                mobile_skills::dispatch(&self.home, &body.ok_or("Skill request missing")?)
            },
            "/api/mcp" if cfg!(target_os = "android") => {
                let payload = body.ok_or("MCP request missing")?;
                let action = payload["action"].as_str().ok_or("MCP action missing")?;
                self.mcp_request(action, &payload).await
            },
            "/api/mobile-agents" if cfg!(target_os = "android") => {
                let b = body.ok_or("Worker request missing")?;
                let action = b["action"].as_str().ok_or("Worker action missing")?;
                self.mobile_agent_request(action, &b)
            },
            "/api/mobile-ai-agents" if cfg!(target_os = "android") => {
                let b = body.ok_or("AI delegate request missing")?;
                self.mobile_ai_delegation_request(&b).await
            },
            "/api/mobile-prime" | "/api/mobile-prime-agent" if cfg!(target_os = "android") => {
                let b = body.ok_or("Prime coordinator request missing")?;
                self.mobile_prime_agent_request(&b).await
            },
            "/api/session-export" if cfg!(target_os = "android") => {
                let b = body.ok_or("Conversation export request missing")?;
                let id = b["id"].as_str().ok_or("Conversation ID missing")?;
                self.export_session_markdown(id)
            },
            "/api/mobile-goal" if cfg!(target_os = "android") => {
                let b = body.ok_or("Goal request missing")?;
                let action = b["action"].as_str().ok_or("Goal action missing")?;
                self.mobile_goal_request(action, &b)
            },
            "/api/goal-review" if cfg!(target_os = "android") => {
                if body.as_ref().is_some_and(|b| !b.is_null() && b != &json!({})) {
                    return Err("Goal Review accepts no query or sensitive input".into());
                }
                self.mobile_goal_review()
            },
            "/api/mobile-goal-run" if cfg!(target_os = "android") => {
                let b = body.ok_or("Goal AI request missing")?;
                self.run_mobile_goal_step(&b).await
            },
            "/api/goal-tool-review" if cfg!(target_os = "android") => {
                let b = body.ok_or("Goal MCP decision missing")?;
                self.reviewed_goal_tool(&b).await
            },
            "/api/mobile-goal-draft" if cfg!(target_os = "android") => {
                let b = body.ok_or("Goal draft request missing")?;
                self.draft_mobile_goal_plan(&b).await
            },
            "/api/mobile-projects" if cfg!(target_os = "android") => {
                let b = body.ok_or("Project request missing")?;
                let action = b["action"].as_str().ok_or("Project action missing")?;
                self.mobile_project_request(action, &b)
            },
            _ => Err("Feature not yet available in the Rust native app".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_effort_maps_only_to_supported_api_request_parameters() {
        let plain = json!({"model":"gpt-6","messages":[{"role":"user","content":"hello"}]});
        let mut default = plain.clone();
        apply_reasoning_effort(&mut default,"openai","gpt-6",None).unwrap();
        assert_eq!(default,plain);
        apply_reasoning_effort(&mut default,"openai","gpt-6",Some("auto")).unwrap();
        assert_eq!(default,plain);

        let mut openai = plain;
        apply_reasoning_effort(&mut openai,"openai","gpt-6",Some("high")).unwrap();
        assert_eq!(openai["reasoning_effort"],"high");
        assert!(openai.get("output_config").is_none());
        let mut anthropic = json!({"model":"claude-sonnet-5-5","max_tokens":2048});
        apply_reasoning_effort(&mut anthropic,"anthropic","claude-sonnet-5-5",Some("low")).unwrap();
        assert_eq!(anthropic["output_config"]["effort"],"low");
        assert!(anthropic.get("reasoning_effort").is_none());
    }

    #[test]
    fn reasoning_effort_rejects_unsupported_provider_model_values_without_mutation() {
        for (provider,model,effort) in [
            ("openrouter","openai/gpt-6","high"),
            ("ollama","gpt-6","medium"),
            ("chatgpt-browser","gpt-6","low"),
            ("openai","gpt-4o","high"),
            ("anthropic","claude-3-haiku","high"),
            ("openai","gpt-6","max"),
            ("openai","gpt-6","HIGH"),
        ] {
            let mut body = json!({"messages":[]});
            let before = body.clone();
            assert!(apply_reasoning_effort(&mut body,provider,model,Some(effort)).is_err());
            assert_eq!(body,before);
        }
        let implicit: ChatRequest = serde_json::from_value(json!({
            "prompt":"hello","provider":"openai","model":"gpt-6"
        })).unwrap();
        assert_eq!(implicit.reasoning_effort,None);
        let selected: ChatRequest = serde_json::from_value(json!({
            "prompt":"hello","provider":"openai","model":"gpt-6",
            "reasoningEffort":"medium"
        })).unwrap();
        assert_eq!(selected.reasoning_effort.as_deref(),Some("medium"));
        assert!(serde_json::from_value::<ChatRequest>(json!({
            "prompt":"hello","provider":"openai","model":"gpt-6",
            "reasoningEffort":{"untrusted":"high"}
        })).is_err());
    }

    #[test]
    fn tool_requests_block_incompatible_reasoning_models_before_provider_call() {
        for model in [
            "gpt-6-astra", "gpt-6.1-sol", "gpt-6-sol", "gpt-6-luna",
            "gpt-6-sol-2026-10-01"
        ] {
            assert!(validate_tool_reasoning_compatibility(
                "openai",model,Some("high")).is_err());
            assert!(validate_tool_reasoning_compatibility(
                "openai",model,None).is_err());
        }
        assert!(validate_tool_reasoning_compatibility(
            "anthropic","claude-opus-5-5",Some("medium")).is_err());
        assert!(validate_tool_reasoning_compatibility(
            "anthropic","claude-opus-5-5",Some("auto")).is_ok());
        assert!(validate_tool_reasoning_compatibility(
            "openai","gpt-5.5",Some("medium")).is_ok());
    }

    /// Controlled, offline fixture for the *saved* delegate receipt boundary.
    /// It does not imply an external AI provider was contacted or succeeded.
    fn mock_saved_delegate_receipt(
        engine: &Engine,
        call: &mobile_prime::WorkerCall,
        answer: &str,
    ) -> Value {
        let store = engine.delegation_store().unwrap();
        let (_, ticket) = store.submit_and_start(
            &call.provider, &call.model, &call.prompt, true,
        ).unwrap();
        let job = store.complete(&ticket.job_id, &ticket.ticket_id, answer).unwrap();
        json!({"job":job,"requestId":call.request_id,"providerReceipt":true,
            "modelToolsExecuted":false,"backgroundReplay":false})
    }

    #[tokio::test]
    async fn prime_history_after_failed_mock_provider_retains_only_verified_provenance() {
        let home=std::env::temp_dir().join(format!("prime-history-provider-{}",Uuid::new_v4()));
        let engine=Engine::new(home.clone()).unwrap();
        let store=engine.prime_store().unwrap();
        let prior=store.start("Review one bounded provider result",true).unwrap();
        let call=store.spawn(&prior.id,"Research","openai","gpt-4o-mini",
            "Review provider receipt",&Uuid::new_v4().to_string(),true).unwrap();
        let mut mismatched=mock_saved_delegate_receipt(&engine,&call,"Private test report should not archive");
        mismatched["job"]["model"]=json!("wrong-model");
        let result=engine.run_prime_delegate_via(store.clone(),call.clone(),move |_|async move {
            Ok(mismatched)
        }).await;
        assert!(result.is_err());
        assert_eq!(store.status().unwrap()["activeWorkerCount"],0);
        assert_eq!(store.status().unwrap()["task"]["workers"][0]["status"],"failed");
        store.cancel_task(&prior.id,true).unwrap();
        let current=store.start("Explicit subsequent review",true).unwrap();
        let status=store.status().unwrap();
        assert_eq!(status["task"]["id"],current.id);
        assert_eq!(status["history"].as_array().unwrap().len(),1);
        let archived=&status["history"][0];
        assert_eq!(archived["id"],prior.id);
        assert_eq!(archived["workers"][0]["status"],"failed");
        assert_eq!(archived["workers"][0]["providerVerified"],false);
        assert_eq!(archived["workers"][0]["verifiedReportCount"],0);
        assert_eq!(archived["autoVerified"],false);
        assert!(!archived.to_string().contains("Private test report"));
        assert!(!archived.to_string().contains(&call.request_id));
        assert!(!store.still_running(&prior.id,&call.worker_id,&call.request_id).unwrap());
        drop(store);
        drop(engine);
        let reopened=Engine::new(home.clone()).unwrap();
        let restored=reopened.prime_store().unwrap().status().unwrap();
        assert_eq!(restored["history"],status["history"]);
        assert_eq!(restored["activeWorkerCount"],0);
        assert_eq!(restored["task"]["id"],current.id);
        drop(reopened);
        fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn prime_mock_transport_two_concurrent_workers_retain_selected_provider_and_real_store_receipts() {
        use tokio::sync::Barrier;
        let home = std::env::temp_dir().join(format!("prime-mock-two-{}", Uuid::new_v4()));
        let engine = Engine::new(home.clone()).unwrap();
        let prime = engine.prime_store().unwrap();
        let task = prime.start("Audit two independent AI outputs", true).unwrap();
        let openai = prime.spawn(&task.id,"Analysis","openai","gpt-4o-mini",
            "Check bounded worker state",&Uuid::new_v4().to_string(),true).unwrap();
        let anthropic = prime.spawn(&task.id,"Tests","anthropic","claude-3-haiku",
            "Outline tests",&Uuid::new_v4().to_string(),true).unwrap();
        assert_ne!(openai.worker_id,anthropic.worker_id);
        let receipt_a = mock_saved_delegate_receipt(&engine,&openai,"Mocked stored analysis report");
        let receipt_b = mock_saved_delegate_receipt(&engine,&anthropic,"Mocked stored test report");
        let gate = Arc::new(Barrier::new(3));
        let gate_a = gate.clone();
        let gate_b = gate.clone();
        let run_a = engine.run_prime_delegate_via(prime.clone(),openai.clone(),move |request| async move {
            assert_eq!(request["provider"],"openai");
            assert_eq!(request["model"],"gpt-4o-mini");
            assert_eq!(request["requestId"],openai.request_id);
            assert_eq!(request["primeWorkerId"],openai.worker_id);
            assert_eq!(request["userInitiated"],true);
            assert_eq!(request.get("allowMcpTools"),None);
            gate_a.wait().await;
            Ok(receipt_a)
        });
        let run_b = engine.run_prime_delegate_via(prime.clone(),anthropic.clone(),move |request| async move {
            assert_eq!(request["provider"],"anthropic");
            assert_eq!(request["model"],"claude-3-haiku");
            assert_eq!(request["requestId"],anthropic.request_id);
            assert_eq!(request["primeWorkerId"],anthropic.worker_id);
            assert_eq!(request["userInitiated"],true);
            assert_eq!(request.get("allowLocalTools"),None);
            gate_b.wait().await;
            Ok(receipt_b)
        });
        let observe = async {
            assert_eq!(prime.status().unwrap()["task"]["workers"].as_array().unwrap().len(),2);
            assert_eq!(prime.status().unwrap()["task"]["workers"][0]["status"],"running");
            assert_eq!(prime.status().unwrap()["task"]["workers"][1]["status"],"running");
            gate.wait().await;
        };
        let (result_a,result_b,()) = tokio::join!(run_a,run_b,observe);
        let result_a = result_a.unwrap();
        let result_b = result_b.unwrap();
        assert_eq!(result_a["providerReceipt"],true);
        assert_eq!(result_b["providerReceipt"],true);
        assert_eq!(result_a["worker"]["provider"],"openai");
        assert_eq!(result_b["worker"]["provider"],"anthropic");
        assert_eq!(result_a["worker"]["reports"][1]["text"],"Mocked stored analysis report");
        assert_eq!(result_b["worker"]["reports"][1]["text"],"Mocked stored test report");
        assert_eq!(prime.status().unwrap()["task"]["workers"][0]["status"],"waiting");
        assert!(prime.finish_task(&task.id,"Unverified summary",true).is_err());
        for id in [&result_a["worker"]["id"],&result_b["worker"]["id"]] {
            assert!(prime.finish_worker(&task.id,id.as_str().unwrap(),false).is_err());
            prime.finish_worker(&task.id,id.as_str().unwrap(),true).unwrap();
        }
        assert_eq!(prime.finish_task(&task.id,"User reviewed both mocked reports",true).unwrap().status,
            mobile_prime::TaskStatus::Finished);
        drop(engine);
        fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn prime_mock_transport_rejects_forged_mismatched_stale_and_unconfirmed_receipts() {
        let home = std::env::temp_dir().join(format!("prime-mock-invalid-{}",Uuid::new_v4()));
        let engine = Engine::new(home.clone()).unwrap();
        let prime = engine.prime_store().unwrap();
        // Each case gets a fresh Prime task, never reuses a previous worker.
        for tamper in ["provider","model","prompt","request","missing_job","unverified","tools"] {
            let task = prime.start("Test receipt binding",true).unwrap();
            let call = prime.spawn(&task.id,"Worker","openai","gpt-4o-mini",
                "Return bounded text",&Uuid::new_v4().to_string(),true).unwrap();
            let mut forged = mock_saved_delegate_receipt(&engine,&call,"Controlled test answer");
            match tamper {
                "provider" => forged["job"]["provider"] = json!("anthropic"),
                "model" => forged["job"]["model"] = json!("other-model"),
                "prompt" => forged["job"]["task"] = json!("different worker instruction"),
                "request" => forged["requestId"] = json!(Uuid::new_v4().to_string()),
                "missing_job" => forged["job"]["id"] = json!(Uuid::new_v4().to_string()),
                "unverified" => forged["providerReceipt"] = json!(false),
                "tools" => forged["modelToolsExecuted"] = json!(true),
                _ => unreachable!(),
            }
            let result = engine.run_prime_delegate_via(prime.clone(),call.clone(),move |_request| async move {
                Ok(forged)
            }).await;
            assert!(result.is_err(),"Forged {tamper} receipt passed validation");
            let status = prime.status().unwrap();
            assert_eq!(status["task"]["workers"][0]["status"],"failed", "{tamper}");
            assert!(status["task"]["workers"][0]["reports"].as_array().unwrap()
                .iter().all(|r| r["provider_verified"] != true),"{tamper}");
            prime.cancel_task(&task.id,true).unwrap();
        }
        drop(engine);
        fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn prime_cancel_before_dispatch_and_while_mock_response_pending_never_saves_late_success() {
        let home = std::env::temp_dir().join(format!("prime-mock-cancel-{}",Uuid::new_v4()));
        let engine = Engine::new(home.clone()).unwrap();
        let prime = engine.prime_store().unwrap();
        let task = prime.start("Cancellation isolation",true).unwrap();
        let first = prime.spawn(&task.id,"First","openai","gpt-4o-mini",
            "Do not start after cancel",&Uuid::new_v4().to_string(),true).unwrap();
        prime.cancel_worker(&task.id,&first.worker_id,true).unwrap();
        let result = engine.run_prime_delegate_via(prime.clone(),first,|_request| async {
            panic!("Transport must not start after Prime cancellation");
            #[allow(unreachable_code)]
            Ok(Value::Null)
        }).await;
        assert!(result.is_err());

        let second = prime.spawn(&task.id,"Second","openrouter","model-v1",
            "Cancel while transport is pending",&Uuid::new_v4().to_string(),true).unwrap();
        let valid_receipt = mock_saved_delegate_receipt(&engine,&second,"This controlled answer arrived too late");
        let (started_tx,started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx,release_rx) = tokio::sync::oneshot::channel::<()>();
        let run = engine.run_prime_delegate_via(prime.clone(),second.clone(),move |_request| async move {
            let _ = started_tx.send(());
            let _ = release_rx.await;
            Ok(valid_receipt)
        });
        let cancel = async {
            started_rx.await.unwrap();
            let (job,request) = prime.cancel_worker(&task.id,&second.worker_id,true).unwrap();
            assert_eq!(job.status,mobile_prime::WorkerStatus::Cancelled);
            assert_eq!(request.as_deref(),Some(second.request_id.as_str()));
            release_tx.send(()).unwrap();
        };
        let (outcome,()) = tokio::join!(run,cancel);
        assert!(outcome.is_err());
        let status = prime.status().unwrap();
        assert_eq!(status["task"]["workers"][1]["status"],"cancelled");
        assert!(status["task"]["workers"][1]["reports"].as_array().unwrap()
            .iter().all(|r| r["provider_verified"] != true));
        drop(engine);
        fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn prime_dropped_mock_transport_future_marks_interruption_without_reply_or_retry() {
        let home = std::env::temp_dir().join(format!("prime-mock-drop-{}",Uuid::new_v4()));
        let engine = Engine::new(home.clone()).unwrap();
        let prime = engine.prime_store().unwrap();
        let task = prime.start("Dropped HTTP future",true).unwrap();
        let call = prime.spawn(&task.id,"Worker","anthropic","claude-model",
            "Return no result",&Uuid::new_v4().to_string(),true).unwrap();
        let (started_tx,started_rx) = tokio::sync::oneshot::channel::<()>();
        let mut pending = Box::pin(engine.run_prime_delegate_via(prime.clone(),call.clone(),move |_request| async move {
            let _ = started_tx.send(());
            std::future::pending::<Result<Value,String>>().await
        }));
        tokio::select! {
            _ = &mut pending => panic!("Mock transport unexpectedly returned"),
            _ = started_rx => {}
        }
        drop(pending);
        assert_eq!(prime.status().unwrap()["task"]["workers"][0]["status"],"failed");
        assert!(!prime.still_running(&task.id,&call.worker_id,&call.request_id).unwrap());
        assert_eq!(prime.status().unwrap()["task"]["workers"][0]["reports"].as_array().unwrap().len(),1);
        drop(engine);
        let reopened = mobile_prime::PrimeStore::open_or_create(&home.join("prime-state")).unwrap();
        assert_eq!(reopened.status().unwrap()["task"]["workers"][0]["status"],"failed");
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn scoped_rules_api_rejects_unknown_chat_and_keeps_global_and_local_isolated() {
        let folder=std::env::temp_dir().join(format!("mcp-scoped-chat-{}",Uuid::new_v4()));
        let engine=Engine::new(folder.clone()).unwrap();
        let a=Uuid::new_v4().to_string(); let b=Uuid::new_v4().to_string();
        let mk=|id:String| Session{id,at:"now".into(),provider:"openai".into(),
            model:"test-model".into(),messages:vec![Message{role:"user".into(),text:"test".into()}],
            actions:vec![]};
        engine.save_sessions(&[mk(a.clone()),mk(b.clone())]).unwrap();
        let save=|id:Option<&str>,scope:&str,text:&str|json!({"action":"save",
            "scope":scope,"sessionId":id,"enabled":true,"text":text,"userInitiated":true});
        let unknown=Uuid::new_v4().to_string();
        assert!(engine.scoped_rules_request(save(Some(&unknown),"session","UNKNOWN")).is_err());
        assert!(engine.scoped_rules_request(json!({"action":"get","sessionId":unknown})).is_err());
        assert!(engine.scoped_rules_request(save(None,"session","NONE")).is_err());
        engine.scoped_rules_request(save(Some(&a),"session","A LOCAL")).unwrap();
        engine.scoped_rules_request(save(None,"global","GLOBAL")).unwrap();
        let scoped_a=engine.scoped_rules_request(json!({"action":"get","sessionId":a})).unwrap();
        let scoped_b=engine.scoped_rules_request(json!({"action":"get","sessionId":b})).unwrap();
        assert_eq!(scoped_a["session"]["text"],"A LOCAL");
        assert_eq!(scoped_b["session"]["text"],"");
        assert_eq!(scoped_b["global"]["text"],"GLOBAL");
        // A deleted/evicted chat can no longer mutate or read its private scope.
        engine.save_sessions(&[mk(b.clone())]).unwrap();
        assert!(engine.scoped_rules_request(save(Some(&a),"session","STALE")).is_err());
        assert!(engine.scoped_rules_request(json!({"action":"get","sessionId":a})).is_err());
        assert!(!standing_rules::instructions_scoped(&folder,Some(&b)).unwrap().join("\n").contains("A LOCAL"));
        let _=fs::remove_dir_all(folder);
    }
    #[test]
    fn validates_real_provider_settings_only() {
        // The API-key agent path is distinct from ChatGPT OAuth; Android
        // supports both, but OAuth receipts never imply API function calling.
        assert!(valid_provider("openai"));
        assert!(valid_provider("anthropic"));
        assert!(valid_provider("openrouter"));
        assert!(!valid_provider(CHATGPT_BROWSER_PROVIDER)); // Browser uses verified native receipts.
        assert!(!valid_provider("demo"));
        assert!(!valid_provider("https://evil.example"));
        assert!(valid_model("anthropic/claude-sonnet-4"));
        assert!(!valid_model("model\nAuthorization"));
    }
    #[test]
    fn keys_are_ephemeral_and_state_does_not_leak_them() {
        let folder = std::env::temp_dir().join(format!("mcp-core-test-{}", Uuid::new_v4()));
        let engine = Engine::new(folder).expect("init");
        if cfg!(target_os = "android") {
            assert_eq!(engine.state().unwrap()["providers"],
                json!([CHATGPT_BROWSER_PROVIDER,"openai","anthropic","openrouter"]));
        }
        engine.set_key("openai", "synthetic-test-secret").unwrap();
        let state = engine.state().unwrap().to_string();
        assert!(state.contains("openai"));
        assert!(!state.contains("synthetic-test-secret"));
    }
    #[test]
    fn unsupported_methods_do_not_mutate_state() {
        let folder = std::env::temp_dir().join(format!("mcp-core-test-{}", Uuid::new_v4()));
        let engine = Engine::new(folder).unwrap();
        assert!(engine.set_key("hermes", "abc").is_err());
        assert!(engine.set_key("openai", "bad\r\nkey").is_err());
        assert!(engine.sessions().unwrap().is_empty());
        let id = Uuid::new_v4().to_string();
        let (send, mut recv) = tokio::sync::oneshot::channel::<()>();
        engine.chat_cancellations.lock().unwrap().insert(id.clone(), send);
        assert_eq!(engine.cancel_chat(&id).unwrap()["cancelRequested"], true);
        assert!(recv.try_recv().is_ok());
        assert!(engine.cancel_chat(&id).is_err());
    }
    #[test]
    fn session_roundtrip_and_replacement_preserve_existing_chat() {
        let folder = std::env::temp_dir().join(format!("mcp-core-test-{}", Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let first = Session {
            id: Uuid::new_v4().to_string(),
            at: "test".into(),
            provider: "openai".into(),
            model: "test-model".into(),
            messages: vec![Message { role: "user".into(), text: "synthetic history".into() }],
            actions: vec![],
        };
        engine.save_sessions(&[first.clone()]).unwrap();
        let export = engine.export_session_markdown(&first.id).unwrap();
        assert!(export["content"].as_str().unwrap().contains("synthetic history"));
        assert!(!export["content"].as_str().unwrap().contains("synthetic-test-secret"));
        assert!(engine.export_session_markdown("../sessions.json").is_err());
        let loaded = engine.sessions().unwrap();
        assert_eq!(loaded[0].id, first.id);
        assert_eq!(loaded[0].messages[0].text, "synthetic history");
        engine.save_sessions(&[first]).unwrap();
        assert_eq!(engine.sessions().unwrap().len(), 1);
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn overlapping_chat_saves_preserve_new_sessions_and_reject_stale_resume() {
        let folder = std::env::temp_dir().join(format!("mcp-chat-race-{}", Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let input = |session_id: Option<String>, prompt: &str| ChatRequest {
            prompt: prompt.into(), provider: "openai".into(), model: "test-model".into(),
            request_id: None, session_id, allow_mcp_tools:false, allow_local_tools:false,
            reasoning_effort:None,
        };
        let first = engine.append_completed_turn(input(None, "one"), None, "reply one".into()).unwrap();
        let second = engine.append_completed_turn(input(None, "two"), None, "reply two".into()).unwrap();
        let id = first["sessionId"].as_str().unwrap();
        assert_ne!(first["sessionId"], second["sessionId"]);
        assert_eq!(engine.sessions().unwrap().len(), 2);
        let old = engine.sessions().unwrap().into_iter().find(|s| s.id == id).unwrap();
        engine.append_completed_turn(input(Some(id.into()), "three"), Some(old.clone()), "reply three".into()).unwrap();
        assert!(engine.append_completed_turn(input(Some(id.into()), "stale"), Some(old), "untrusted stale reply".into()).is_err());
        let saved = engine.sessions().unwrap();
        assert_eq!(saved.len(), 2);
        assert!(!saved.iter().any(|s| s.messages.iter().any(|m| m.text == "untrusted stale reply")));
        fs::remove_dir_all(folder).unwrap();
    }
    fn synthetic_browser_receipt(
        session: Option<Session>, conversation_id: Option<String>,
    ) -> BrowserChatTurnReceipt {
        BrowserChatTurnReceipt {
            request_id: Uuid::new_v4().to_string(),
            session_id: session.as_ref().map(|s| s.id.clone()),
            browser_conversation_id: conversation_id,
            prompt: "synthetic browser prompt".into(),
            answer: "synthetic observed assistant reply".into(),
            expected_session: session,
            status: BrowserTurnReceiptStatus::Confirmed,
            used_mcp_tools: false,
        }
    }
    #[test]
    fn oauth_chat_claims_before_dispatch_persists_and_rejects_website_mixing() {
        let folder = std::env::temp_dir().join(format!("mcp-oauth-{}", Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let request_id = Uuid::new_v4().to_string();
        assert!(engine.chatgpt_oauth_session_snapshot(None).unwrap().1.is_empty());
        let receipt = OAuthChatTurnReceipt {
            request_id:request_id.clone(),session_id:None,
            prompt:"Hello OAuth".into(),answer:"Native verified answer".into(),
            expected_session:None,
        };
        assert!(engine.persist_oauth_chat_turn(receipt.clone()).unwrap_err().contains("not claimed"));
        engine.claim_oauth_chat_request(&request_id).unwrap();
        assert!(engine.claim_oauth_chat_request(&request_id).unwrap_err().contains("already been claimed"));
        let saved = engine.persist_oauth_chat_turn(receipt).unwrap();
        assert_eq!(saved["state"], "completed");
        assert_eq!(saved["actions"], json!([]));
        let session_id = saved["sessionId"].as_str().unwrap();
        let (snapshot, context) = engine.chatgpt_oauth_session_snapshot(Some(session_id)).unwrap();
        assert_eq!(context.len(),2);
        assert_eq!(context[1].text,"Native verified answer");
        assert_eq!(snapshot.as_ref().unwrap().actions[0]["type"],"chatgpt_oauth_turn");
        let second_id = Uuid::new_v4().to_string();
        engine.claim_oauth_chat_request(&second_id).unwrap();
        let next = OAuthChatTurnReceipt {
            request_id: second_id, session_id:Some(session_id.to_owned()),
            prompt:"Follow-up".into(),answer:"Second native answer".into(),
            expected_session:snapshot,
        };
        assert!(engine.persist_oauth_chat_turn(next).is_ok());
        let reopened = Engine::new(folder.clone()).unwrap();
        assert!(reopened.claim_oauth_chat_request(&request_id).is_err());
        let browser = synthetic_browser_receipt(None,Some(Uuid::new_v4().to_string()));
        let browser_result = reopened.persist_browser_chat_turn(browser).unwrap();
        assert!(reopened.chatgpt_oauth_session_snapshot(browser_result["sessionId"].as_str())
            .unwrap_err().contains("another ChatGPT transport"));
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn oauth_chat_stale_snapshots_and_untrusted_answers_do_not_save() {
        let folder = std::env::temp_dir().join(format!("mcp-oauth-stale-{}",Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let original_id = Uuid::new_v4().to_string();
        engine.claim_oauth_chat_request(&original_id).unwrap();
        let first = engine.persist_oauth_chat_turn(OAuthChatTurnReceipt {
            request_id:original_id,session_id:None,prompt:"one".into(),
            answer:"reply".into(),expected_session:None,
        }).unwrap();
        let id = first["sessionId"].as_str().unwrap().to_owned();
        let (snapshot,_) = engine.chatgpt_oauth_session_snapshot(Some(&id)).unwrap();
        let id2 = Uuid::new_v4().to_string();
        engine.claim_oauth_chat_request(&id2).unwrap();
        let accepted = engine.persist_oauth_chat_turn(OAuthChatTurnReceipt {
            request_id:id2,session_id:Some(id.clone()),
            prompt:"two".into(),answer:"second".into(),expected_session:snapshot.clone(),
        });
        assert!(accepted.is_ok());
        let id3 = Uuid::new_v4().to_string();
        engine.claim_oauth_chat_request(&id3).unwrap();
        assert!(engine.persist_oauth_chat_turn(OAuthChatTurnReceipt {
            request_id:id3,session_id:Some(id.clone()),
            prompt:"stale".into(),answer:"should not save".into(),expected_session:snapshot,
        }).unwrap_err().contains("changed"));
        assert!(!engine.sessions().unwrap()[0].messages.iter()
            .any(|message| message.text == "should not save"));
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn browser_receipt_persists_without_api_provider_and_rejects_replay_after_restart() {
        let folder = std::env::temp_dir().join(format!("mcp-browser-replay-{}", Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        assert!(!valid_provider(CHATGPT_BROWSER_PROVIDER));
        let website_id = Uuid::new_v4().to_string();
        let receipt = synthetic_browser_receipt(None, Some(website_id.clone()));
        let request_id = receipt.request_id.clone();
        let answer = receipt.answer.clone();
        let result = engine.persist_browser_chat_turn(receipt.clone()).unwrap();
        assert_eq!(result["saved"], true);
        assert_eq!(result["status"], "confirmed");
        assert_eq!(result["provider"], CHATGPT_BROWSER_PROVIDER);
        assert_eq!(result["model"], CHATGPT_BROWSER_MODEL);
        assert_eq!(result["browserConversationId"], website_id);
        assert_eq!(result["answer"], answer);
        let stored = engine.sessions().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].messages.len(), 2);
        assert_eq!(stored[0].messages[0].text, receipt.prompt);
        assert_eq!(stored[0].messages[1].text, receipt.answer);
        assert_eq!(stored[0].actions[0]["requestId"], request_id);
        assert!(stored[0].actions[0].to_string().contains("hostConfirmed"));
        assert!(!stored[0].actions[0].to_string().contains(&answer));
        assert_eq!(fs::metadata(folder.join("chatgpt-browser-request-ids").join(&request_id))
            .unwrap().len(), 0);
        // Reopen a completely new Engine to exercise *durable* replay
        // rejection when the volatile host/UI request state is gone.
        let reopened = Engine::new(folder.clone()).unwrap();
        assert!(reopened.persist_browser_chat_turn(receipt).unwrap_err().contains("already been claimed"));
        assert_eq!(reopened.sessions().unwrap(), stored);
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn browser_receipt_enforces_provider_session_snapshot_and_website_binding() {
        let folder = std::env::temp_dir().join(format!("mcp-browser-session-{}", Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let api = engine.append_completed_turn(ChatRequest {
            prompt: "synthetic API prompt".into(), provider: "openai".into(),
            model: "test-model".into(), session_id: None, request_id: None,
            allow_mcp_tools: false,
            allow_local_tools: false,
            reasoning_effort: None,
        }, None, "synthetic API reply".into()).unwrap();
        let api_id = api["sessionId"].as_str().unwrap();
        assert!(engine.chatgpt_browser_session_snapshot(Some(api_id)).is_err());
        let api_session = engine.sessions().unwrap().into_iter().find(|s| s.id == api_id).unwrap();
        let invalid = synthetic_browser_receipt(Some(api_session), Some(Uuid::new_v4().to_string()));
        assert!(engine.persist_browser_chat_turn(invalid).unwrap_err().contains("provider"));

        let browser_id = Uuid::new_v4().to_string();
        let first = engine.persist_browser_chat_turn(
            synthetic_browser_receipt(None, Some(browser_id.clone()))).unwrap();
        let app_session_id = first["sessionId"].as_str().unwrap();
        let before = engine.chatgpt_browser_session_snapshot(Some(app_session_id)).unwrap().unwrap();
        let matching = synthetic_browser_receipt(Some(before.clone()), Some(browser_id.clone()));
        let matching_result = engine.persist_browser_chat_turn(matching).unwrap();
        assert_eq!(matching_result["sessionId"], app_session_id);

        // A second browser response prepared from the same older transcript
        // must not silently discard the newer assistant turn.
        let stale = synthetic_browser_receipt(Some(before.clone()), Some(browser_id.clone()));
        assert!(engine.persist_browser_chat_turn(stale).unwrap_err().contains("changed"));
        let current = engine.chatgpt_browser_session_snapshot(Some(app_session_id)).unwrap().unwrap();
        assert_eq!(current.messages.len(), 4);
        let wrong_conversation = synthetic_browser_receipt(
            Some(current.clone()), Some(Uuid::new_v4().to_string()));
        assert!(engine.persist_browser_chat_turn(wrong_conversation)
            .unwrap_err().contains("conversation changed"));
        let wrong_session_id = synthetic_browser_receipt(Some(current.clone()),Some(browser_id));
        let wrong_session_id = BrowserChatTurnReceipt {
            session_id: Some(Uuid::new_v4().to_string()), ..wrong_session_id
        };
        assert!(engine.persist_browser_chat_turn(wrong_session_id)
            .unwrap_err().contains("pre-send snapshot"));
        assert_eq!(engine.sessions().unwrap().len(), 2);
        assert_eq!(engine.sessions().unwrap()[0].messages.len(), 4);
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn browser_receipt_rejects_unconfirmed_mcp_oversize_and_unbound_followup() {
        let folder = std::env::temp_dir().join(format!("mcp-browser-invalid-{}", Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let receipt = synthetic_browser_receipt(None,None);
        for status in [BrowserTurnReceiptStatus::Pending, BrowserTurnReceiptStatus::Failed] {
            assert!(engine.persist_browser_chat_turn(BrowserChatTurnReceipt {
                status, ..receipt.clone()
            }).is_err());
        }
        assert!(engine.persist_browser_chat_turn(BrowserChatTurnReceipt {
            used_mcp_tools: true, ..receipt.clone()
        }).is_err());
        assert!(engine.persist_browser_chat_turn(BrowserChatTurnReceipt {
            answer: "x".repeat(MAX_BROWSER_ANSWER + 1), ..receipt.clone()
        }).is_err());
        assert!(engine.persist_browser_chat_turn(BrowserChatTurnReceipt {
            prompt: "x".repeat(MAX_PROMPT + 1), ..receipt.clone()
        }).is_err());
        assert!(engine.persist_browser_chat_turn(BrowserChatTurnReceipt {
            browser_conversation_id: Some("untrusted URL".into()), ..receipt.clone()
        }).is_err());
        assert!(engine.sessions().unwrap().is_empty());
        assert!(!folder.join("chatgpt-browser-request-ids").exists());
        let saved = engine.persist_browser_chat_turn(receipt).unwrap();
        let id = saved["sessionId"].as_str().unwrap();
        let prior = engine.chatgpt_browser_session_snapshot(Some(id)).unwrap().unwrap();
        assert!(engine.persist_browser_chat_turn(synthetic_browser_receipt(
            Some(prior), Some(Uuid::new_v4().to_string())))
            .unwrap_err().contains("not bound"));
        assert_eq!(engine.sessions().unwrap()[0].messages.len(), 2);
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn concurrent_engine_instances_claim_only_one_browser_request_uuid() {
        let folder = std::env::temp_dir().join(format!("mcp-browser-concurrent-{}", Uuid::new_v4()));
        let first = Engine::new(folder.clone()).unwrap();
        let second = Engine::new(folder.clone()).unwrap();
        let receipt = synthetic_browser_receipt(None, Some(Uuid::new_v4().to_string()));
        let gate = Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            let another_gate = gate.clone();
            let another_receipt = receipt.clone();
            let one = scope.spawn(move || {
                another_gate.wait();
                first.persist_browser_chat_turn(another_receipt)
            });
            let two = scope.spawn(move || {
                gate.wait();
                second.persist_browser_chat_turn(receipt)
            });
            let outcomes = [one.join().unwrap(), two.join().unwrap()];
            assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
            assert_eq!(outcomes.iter().filter(|r| r.is_err()).count(), 1);
        });
        let saved = Engine::new(folder.clone()).unwrap().sessions().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].messages.len(), 2);
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn approved_tool_audit_metadata_survives_subsequent_plain_chat_turns() {
        let folder = std::env::temp_dir().join(format!("mcp-audit-{}",Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let input = |session_id:Option<String>,prompt:&str| ChatRequest {
            prompt:prompt.into(),provider:"openai".into(),model:"synthetic-model".into(),
            session_id,request_id:None,allow_mcp_tools:false,allow_local_tools:false,
            reasoning_effort:None,
        };
        let metadata = json!({"type":"mcp_tool_call","name":"test_read",
            "approvedByUser":true,"resultIsError":false,"resultStored":false});
        let result = engine.append_completed_turn_with_actions(
            input(None,"approved synthetic call"),None,"verified reply".into(),
            vec![metadata.clone()]).unwrap();
        let id = result["sessionId"].as_str().unwrap();
        let previous = engine.sessions().unwrap().into_iter().find(|s|s.id==id).unwrap();
        engine.append_completed_turn(input(Some(id.into()),"normal follow up"),
            Some(previous),"normal response".into()).unwrap();
        let saved = engine.sessions().unwrap();
        assert_eq!(saved.len(),1);
        assert_eq!(saved[0].actions,[metadata]);
        assert_eq!(saved[0].messages.len(),4);
        assert!(!saved[0].actions[0].to_string().contains("verified reply"));
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn chat_stream_progress_is_uuid_scoped_and_not_a_connection_claim() {
        let engine = Engine::new(std::env::temp_dir().join(format!("mcp-progress-{}", Uuid::new_v4()))).unwrap();
        let id = Uuid::new_v4().to_string();
        engine.chat_progress.lock().unwrap().insert(id.clone(), "synthetic fragment".into());
        assert_eq!(engine.chat_stream_progress(&id).unwrap()["text"], "synthetic fragment");
        assert_eq!(engine.chat_stream_progress(&Uuid::new_v4().to_string()).unwrap()["active"], false);
        assert!(engine.chat_stream_progress("../../bad").is_err());
    }
    #[test]
    fn mcp_disconnect_invalidates_in_flight_connection_generation() {
        let engine = Engine::new(std::env::temp_dir().join(format!("mcp-generation-{}", Uuid::new_v4()))).unwrap();
        let first_connect = engine.mcp_generation.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        let later_connect = engine.mcp_generation.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        assert_ne!(first_connect, later_connect);
        assert_eq!(engine.mcp_generation.load(Ordering::SeqCst), later_connect);
        assert_eq!(engine.disconnect_mcp().unwrap()["connected"], false);
        assert_ne!(engine.mcp_generation.load(Ordering::SeqCst), later_connect);
        assert!(engine.mcp.lock().unwrap().is_none());
    }
    #[test]
    fn goal_ai_prompt_tracks_only_verified_checkpoint_steps_and_never_claims_actions() {
        let folder = std::env::temp_dir().join(format!("goal-ai-prompt-{}",Uuid::new_v4()));
        fs::create_dir_all(&folder).unwrap();
        let spec = mobile_goal::GoalSpec {
            title:"Synthetic goal".into(), max_turns:4, max_retries_per_step:1,
            steps:vec![
                mobile_goal::GoalStep { description:"Review architecture".into(), requires_approval:false },
                mobile_goal::GoalStep { description:"Draft test plan".into(), requires_approval:false },
            ],
        };
        let mut goal = mobile_goal::GoalDriver::create(&folder,spec).unwrap();
        let initial = goal_ai_step_prompt(&goal.snapshot()).unwrap();
        assert!(initial.contains("Current step 1/2: Review architecture"));
        assert!(!initial.contains("Earlier steps marked complete"));
        assert!(initial.contains("do not assume completion"));
        let ticket = goal.begin_turn().unwrap();
        // A provider turn alone does not satisfy a Goal checkpoint.
        assert_eq!(goal.snapshot().status,mobile_goal::GoalStatus::InTurn);
        assert_eq!(goal.snapshot().current_step,0);
        goal.record_outcome(&ticket.id,mobile_goal::TurnOutcome::Completed(
            "Synthetic user-verified receipt".into())).unwrap();
        let second = goal_ai_step_prompt(&goal.snapshot()).unwrap();
        assert!(second.contains("Current step 2/2: Draft test plan"));
        assert!(second.contains("Earlier steps marked complete by the user"));
        assert!(second.contains("Review architecture"));
        assert!(!second.contains("Synthetic user-verified receipt"));
        fs::remove_dir_all(&folder).unwrap();
    }
    #[test]
    fn dropped_goal_provider_future_releases_reservation_and_durably_interrupts_only_its_turn() {
        let folder = std::env::temp_dir().join(format!("goal-drop-{}",Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let goal_root = folder.join("goal-state");
        fs::create_dir_all(&goal_root).unwrap();
        let spec = mobile_goal::GoalSpec {
            title:"Synthetic interruption".into(),max_turns:3,max_retries_per_step:2,
            steps:vec![mobile_goal::GoalStep {
                description:"Review synthetic evidence".into(),requires_approval:false,
            }],
        };
        let mut driver = mobile_goal::GoalDriver::create(&goal_root,spec).unwrap();
        let ticket = driver.begin_turn().unwrap();
        let goal_id = driver.snapshot().id;
        *engine.goal.lock().unwrap() = Some(driver);
        let request_id = Uuid::new_v4().to_string();
        *engine.goal_ai_inflight.lock().unwrap() = Some(request_id.clone());
        drop(GoalAiCleanup { engine:&engine,request_id,
            turn:Some((goal_id,ticket.id)),settled:false,
            external_effect_uncertain:false });
        assert!(engine.goal_ai_inflight.lock().unwrap().is_none());
        let snapshot = engine.goal.lock().unwrap().as_ref().unwrap().snapshot();
        assert_eq!(snapshot.status,mobile_goal::GoalStatus::Ready);
        assert_eq!(snapshot.turns_used,1);
        assert_eq!(snapshot.history.len(),1);
        assert_eq!(snapshot.current_step,0);
        assert!(matches!(snapshot.history[0].outcome,mobile_goal::TurnOutcome::RetryableFailure(_)));
        drop(engine.goal.lock().unwrap().take());
        let reopened = mobile_goal::GoalDriver::open(&goal_root).unwrap();
        assert_eq!(reopened.snapshot().turns_used,1);
        assert_eq!(reopened.snapshot().history.len(),1);
        assert_eq!(reopened.snapshot().status,mobile_goal::GoalStatus::Ready);
        let summary = goal_review::summarize(&reopened.snapshot()).unwrap();
        assert_eq!(summary["events"][0]["kind"],"retryable_failure");
        assert_eq!(summary["independentlyVerified"],false);
        assert!(!summary.to_string().contains("Review synthetic evidence"));
        drop(reopened);
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn completed_provider_receipt_keeps_goal_turn_awaiting_explicit_user_decision() {
        let folder = std::env::temp_dir().join(format!("goal-receipt-{}",Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        fs::create_dir_all(folder.join("goal-state")).unwrap();
        let spec = mobile_goal::GoalSpec {
            title:"Synthetic manual goal".into(),max_turns:3,max_retries_per_step:1,
            steps:vec![mobile_goal::GoalStep {
                description:"User must decide".into(),requires_approval:false,
            }],
        };
        let mut driver = mobile_goal::GoalDriver::create(&folder.join("goal-state"),spec).unwrap();
        let ticket = driver.begin_turn().unwrap();
        let goal_id = driver.snapshot().id;
        *engine.goal.lock().unwrap() = Some(driver);
        let request_id = Uuid::new_v4().to_string();
        *engine.goal_ai_inflight.lock().unwrap() = Some(request_id.clone());
        drop(GoalAiCleanup {engine:&engine,request_id,
            turn:Some((goal_id,ticket.id.clone())),settled:true,
            external_effect_uncertain:false});
        assert!(engine.goal_ai_inflight.lock().unwrap().is_none());
        let snapshot = engine.goal.lock().unwrap().as_ref().unwrap().snapshot();
        assert_eq!(snapshot.status,mobile_goal::GoalStatus::InTurn);
        assert_eq!(snapshot.current_step,0);
        assert_eq!(snapshot.history.len(),0);
        assert_eq!(snapshot.active_turn.unwrap().id,ticket.id);
        drop(engine.goal.lock().unwrap().take());
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn dropped_post_dispatch_goal_requires_manual_reconciliation_not_retry() {
        let folder = std::env::temp_dir().join(format!("goal-uncertain-{}",Uuid::new_v4()));
        let goal_dir = folder.join("goal-state");
        fs::create_dir_all(&goal_dir).unwrap();
        let engine = Engine::new(folder.clone()).unwrap();
        let spec = mobile_goal::GoalSpec {
            title:"Synthetic effect boundary".into(),max_turns:4,max_retries_per_step:2,
            steps:vec![mobile_goal::GoalStep{
                description:"Inspect synthetic tool".into(),requires_approval:false,
            }],
        };
        let mut driver = mobile_goal::GoalDriver::create(&goal_dir,spec).unwrap();
        let ticket = driver.begin_turn().unwrap();
        let goal_id = driver.snapshot().id;
        *engine.goal.lock().unwrap() = Some(driver);
        let request_id = Uuid::new_v4().to_string();
        *engine.goal_ai_inflight.lock().unwrap() = Some(request_id.clone());
        drop(GoalAiCleanup {engine:&engine,request_id,
            turn:Some((goal_id,ticket.id)),settled:false,
            external_effect_uncertain:true});
        let snapshot = engine.goal.lock().unwrap().as_ref().unwrap().snapshot();
        assert_eq!(snapshot.status,mobile_goal::GoalStatus::AwaitingApproval);
        assert_eq!(snapshot.turns_used,1);
        assert_eq!(snapshot.history.len(),1);
        assert_eq!(snapshot.current_step,0);
        assert!(matches!(snapshot.history[0].outcome,mobile_goal::TurnOutcome::NeedsApproval(_)));
        assert!(engine.goal.lock().unwrap().as_mut().unwrap().begin_turn().is_err());
        drop(engine.goal.lock().unwrap().take());
        let recovered = mobile_goal::GoalDriver::open(&goal_dir).unwrap();
        assert_eq!(recovered.snapshot().status,mobile_goal::GoalStatus::AwaitingApproval);
        drop(recovered);
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn initial_goal_open_recovers_stale_plain_provider_before_caching_for_any_route() {
        let folder = std::env::temp_dir().join(format!("goal-first-open-{}",Uuid::new_v4()));
        let goal_dir = folder.join("goal-state");
        fs::create_dir_all(&goal_dir).unwrap();
        let spec = mobile_goal::GoalSpec {
            title:"Synthetic stale turn".into(),max_turns:4,max_retries_per_step:2,
            steps:vec![mobile_goal::GoalStep{
                description:"Check cached first-open".into(),requires_approval:false,
            }],
        };
        let mut previous = mobile_goal::GoalDriver::create(&goal_dir,spec).unwrap();
        previous.begin_turn().unwrap();
        drop(previous);
        let engine = Engine::new(folder.clone()).unwrap();
        let mut guard = engine.goal.lock().unwrap();
        engine.load_saved_goal(&mut guard).unwrap();
        let snapshot = guard.as_ref().unwrap().snapshot();
        assert_eq!(snapshot.status,mobile_goal::GoalStatus::Ready);
        assert_eq!(snapshot.turns_used,1);
        assert_eq!(snapshot.history.len(),1);
        assert_eq!(snapshot.current_step,0);
        // Another route must see the same reconciled state rather than
        // attempting to repair or spend another turn.
        engine.load_saved_goal(&mut guard).unwrap();
        assert_eq!(guard.as_ref().unwrap().snapshot(),snapshot);
        drop(guard.take());
        drop(guard);
        let reopened = mobile_goal::GoalDriver::open(&goal_dir).unwrap();
        assert_eq!(reopened.snapshot(),snapshot);
        drop(reopened);
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn restart_recovers_possible_mcp_dispatch_as_manual_gate_with_no_replay() {
        let folder = std::env::temp_dir().join(format!("goal-mcp-restart-{}",Uuid::new_v4()));
        let goal_dir = folder.join("goal-state");
        let lease_dir = folder.join("goal-mcp-state");
        fs::create_dir_all(&goal_dir).unwrap();
        fs::create_dir_all(&lease_dir).unwrap();
        let spec = mobile_goal::GoalSpec {
            title:"Synthetic external tool".into(),max_turns:4,max_retries_per_step:1,
            steps:vec![mobile_goal::GoalStep{
                description:"Review external side effect".into(),requires_approval:false,
            }],
        };
        let mut original = mobile_goal::GoalDriver::create(&goal_dir,spec).unwrap();
        let active = original.begin_turn().unwrap();
        let goal = original.snapshot();
        let tools = vec![json!({"name":"read_status","description":"synthetic safe tool",
            "inputSchema":{"type":"object","additionalProperties":false,
            "properties":{"query":{"type":"string"}},"required":["query"]}})];
        let call = provider_tools::ModelToolCall {
            id:"call_synthetic123".into(),name:"read_status".into(),
            arguments:json!({"query":"synthetic"}),
        };
        let mut lease = goal_mcp::GoalMcpLease::propose(&lease_dir,&goal,
            &Uuid::new_v4().to_string(),1,&call,&tools,120_000).unwrap();
        let review_id = lease.snapshot().unwrap().review_id;
        lease.approve(&goal,&review_id,true).unwrap();
        let permit = lease.begin_dispatch(&goal,&review_id,1,&tools).unwrap();
        let (_ticket,prepared) = permit.into_call();
        assert_eq!(prepared.arguments,json!({"query":"synthetic"}));
        drop(lease);
        drop(original);
        let engine = Engine::new(folder.clone()).unwrap();
        let mut guard = engine.goal.lock().unwrap();
        engine.load_saved_goal(&mut guard).unwrap();
        let reconciled = guard.as_ref().unwrap().snapshot();
        assert_eq!(reconciled.status,mobile_goal::GoalStatus::AwaitingApproval);
        assert_eq!(reconciled.turns_used,1);
        assert_eq!(reconciled.current_step,0);
        assert_eq!(reconciled.history.len(),1);
        assert_eq!(reconciled.history[0].ticket.id,active.id);
        assert!(matches!(reconciled.history[0].outcome,mobile_goal::TurnOutcome::NeedsApproval(_)));
        assert!(guard.as_mut().unwrap().begin_turn().is_err());
        let disk_lease = goal_mcp::GoalMcpLease::open(&lease_dir,&active.id).unwrap();
        assert_eq!(disk_lease.snapshot().unwrap().status,goal_mcp::ReviewStatus::DispatchUncertain);
        drop(guard.take());drop(guard);drop(disk_lease);
        fs::remove_dir_all(folder).unwrap();
    }
    #[tokio::test]
    async fn goal_ai_network_route_is_denied_on_host_without_state_or_provider_call() {
        let folder = std::env::temp_dir().join(format!("goal-ai-host-{}",Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let input = json!({"userInitiated":true,"provider":"openai","model":"test",
            "requestId":Uuid::new_v4().to_string()});
        assert!(engine.run_mobile_goal_step(&input).await.is_err());
        assert!(engine.goal.lock().unwrap().is_none());
        assert!(engine.chat_cancellations.lock().unwrap().is_empty());
        assert!(engine.goal_ai_inflight.lock().unwrap().is_none());
        assert!(!folder.join("goal-state").exists());
    }
    #[tokio::test]
    async fn goal_mcp_review_routes_are_denied_on_host_without_creating_a_ledger() {
        let folder = std::env::temp_dir().join(format!("goal-mcp-host-{}",Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let identity = Uuid::new_v4().to_string();
        let input = json!({"action":"approve","userInitiated":true,
            "goalId":identity,"turnId":Uuid::new_v4().to_string(),
            "requestId":Uuid::new_v4().to_string(),
            "reviewId":Uuid::new_v4().to_string()});
        assert!(engine.request("/api/goal-tool-review",Some(input.clone())).await.is_err());
        assert!(engine.reviewed_goal_tool(&input).await.is_err());
        assert!(engine.mobile_goal_review().is_err());
        assert!(!folder.join("goal-mcp-state").exists());
        assert!(engine.pending_goal_tool.lock().unwrap().is_none());
        assert!(engine.goal_ai_inflight.lock().unwrap().is_none());
    }
    #[tokio::test]
    async fn goal_draft_network_route_is_host_denied_without_creating_a_goal() {
        let folder = std::env::temp_dir().join(format!("goal-draft-host-{}",Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let input = json!({"userInitiated":true,"provider":"anthropic","model":"test",
            "objective":"Plan synthetic verification","requestId":Uuid::new_v4().to_string()});
        assert!(engine.draft_mobile_goal_plan(&input).await.is_err());
        assert!(!folder.join("goal-state").exists());
        assert!(engine.goal.lock().unwrap().is_none());
        assert!(engine.chat_cancellations.lock().unwrap().is_empty());
        assert!(engine.chat_progress.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn workspace_content_search_is_android_gesture_only_and_name_search_still_works() {
        let folder = std::env::temp_dir().join(format!("workspace-content-host-{}", Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let user_files = folder.join("user-files");
        fs::create_dir_all(&user_files).unwrap();
        fs::write(user_files.join("synthetic-note.txt"), "Synthetic searched content").unwrap();
        assert!(engine.request("/api/workspace-search", Some(json!({
            "query":"Synthetic", "mode":"content", "userInitiated":true
        }))).await.is_err(), "host must not claim Android user action");
        assert!(engine.request("/api/workspace-search", Some(json!({
            "query":"Synthetic", "mode":"content", "userInitiated":false
        }))).await.is_err());
        assert!(engine.request("/api/workspace-search", Some(json!({
            "query":"synthetic", "mode":{"unknown":true}, "userInitiated":true
        }))).await.is_err());
        let existing = engine.request("/api/workspace-search", Some(json!({
            "query":"synthetic", "mode":"name"
        }))).await.unwrap();
        assert_eq!(existing["results"][0]["path"], "user-files/synthetic-note.txt");
        assert_eq!(fs::read_to_string(user_files.join("synthetic-note.txt")).unwrap(),
            "Synthetic searched content");
        fs::remove_dir_all(folder).unwrap();
    }
    #[test]
    fn provider_proposals_must_match_verified_schema_before_review_and_approval() {
        // Real provider envelopes are syntactically valid and their tool name
        // is allowlisted, yet invalid arguments must not reach user approval.
        // The same validator is called again immediately before tools/call.
        let tools = vec![json!({"name":"read_status","description":"synthetic only",
            "inputSchema":{"type":"object","additionalProperties":false,
                "properties":{"path":{"type":"string","minLength":2,"maxLength":8},
                    "limit":{"type":"integer","minimum":1,"maximum":3}},
                "required":["path","limit"]}})];
        let allowed = vec!["read_status".to_owned()];
        for provider in ["openai", "openrouter", "anthropic"] {
            for arguments in [
                json!({"path":"ok","limit":2}),
                json!({"path":"ok"}),
                json!({"path":"ok","limit":"2"}),
                json!({"path":"ok","limit":4}),
                json!({"path":"ok","limit":2,"unknown":true}),
            ] {
                let envelope = if provider == "anthropic" {
                    json!({"type":"message","role":"assistant","stop_reason":"tool_use",
                        "content":[{"type":"tool_use","id":"toolu_synthetic",
                            "name":"read_status","input":arguments}]})
                } else {
                    json!({"choices":[{"index":0,"finish_reason":"tool_calls",
                        "message":{"role":"assistant","content":null,
                            "tool_calls":[{"id":"call_synthetic","type":"function",
                                "function":{"name":"read_status",
                                    "arguments":arguments.to_string()}}]}}]})
                };
                let parsed = provider_tools::parse_provider_turn(provider, &envelope, &allowed)
                    .expect("valid model proposal envelope");
                let provider_tools::ProviderTurn::ToolProposal(proposal) = parsed else {
                    panic!("tool envelope parsed as plain text");
                };
                let approved_by_schema = arguments == json!({"path":"ok","limit":2});
                assert_eq!(validate_provider_mcp_proposal(&tools, &proposal).is_ok(),
                    approved_by_schema, "offer boundary: {provider} {arguments}");
                // Simulate the second independent check after explicit user
                // approval; a changed or stale proposed argument is rejected.
                let mut before_execute = proposal.clone();
                before_execute.arguments["limit"] = json!(4);
                assert!(validate_provider_mcp_proposal(&tools, &before_execute).is_err());
                assert_eq!(validate_provider_mcp_proposal(&tools, &proposal).is_ok(),
                    approved_by_schema, "approval boundary: {provider} {arguments}");
            }
        }
    }
    #[test]
    fn invalid_json_is_not_overwritten_by_loading_state() {
        let folder = std::env::temp_dir().join(format!("mcp-core-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("sessions.json"), b"invalid_json").unwrap();
        let engine = Engine::new(folder.clone()).unwrap();
        assert!(engine.state().is_err());
        assert_eq!(fs::read(folder.join("sessions.json")).unwrap(), b"invalid_json");
        fs::remove_dir_all(folder).unwrap();
    }
}
