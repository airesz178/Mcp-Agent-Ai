//! Chat-initiated, user-intent-scoped Android automation. No renderer approve
//! panel and no regex execution of assistant text. Only structured model tool
//! calls validated against the ORIGINAL authored prompt may reach the native
//! AppHandle host, with a metadata-only audit preceding every dispatch.
use super::{
    local_agent_audit,
    local_agent_intent::{self, Intent, Progress},
    local_agent_tools::{self, AndroidLocalToolHost},
    provider_tools::{self, ModelToolCall, ProviderTurn},
    ChatCleanup, ChatRequest, Engine, Message, Session,
};
use serde_json::{json, Value};
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use uuid::Uuid;

const MAX_NATIVE_STEPS: usize = 3;
const MAX_CHAT_WALL: Duration = Duration::from_secs(55);
const MAX_DIRECT_WALL: Duration = Duration::from_secs(30);

/// No tokio feature changes outside the owned source folder. This sends a
/// one-shot signal using a bounded sleeping OS thread; losing the caller never
/// dispatches or retries a native action. At most three active chats are held.
async fn wall_clock(duration: Duration) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(duration);
        let _ = tx.send(());
    });
    let _ = rx.await;
}

fn canonical_request_id(id: &str) -> Result<(), String> {
    let uuid = Uuid::parse_str(id).map_err(|_| "Agent chat requires request UUID v4")?;
    if uuid.get_version_num() != 4 || uuid.hyphenated().to_string() != id {
        return Err("Agent chat requires canonical request UUID v4".into());
    }
    Ok(())
}

impl Engine {
    async fn trusted_package(
        &self,
        host: &Arc<dyn AndroidLocalToolHost>,
        scope: &Intent,
    ) -> Result<Option<String>, String> {
        if let Intent::AppOpen { label } = scope {
            let resolved = host
                .resolve_app_label(label)
                .await?
                .ok_or("Android could not uniquely resolve the installed app label")?;
            let (package, display_name) = resolved;
            if !display_name.eq_ignore_ascii_case(label) {
                return Err("Android resolved a different app label; no launch authorized".into());
            }
            local_agent_tools::validate("android_app_start", &json!({"targetPackage":package}))?;
            Ok(Some(package))
        } else {
            Ok(None)
        }
    }

    /// The provider may propose multiple *sequential* native calls, never
    /// multiple or nested calls in one provider response. Each is checked
    /// again against the exact original user intent and the current native
    /// snapshot, not against a webpage or the assistant's follow-up wording.
    async fn run_scoped_model_loop(
        &self,
        input: &ChatRequest,
        context: &[Message],
        instructions: &[String],
        key: &str,
        host: &Arc<dyn AndroidLocalToolHost>,
        generation: u64,
        scope: &Intent,
        tools: &[Value],
        package: Option<&str>,
    ) -> Result<(String, Vec<Value>, Vec<String>), String> {
        let mut progress = Progress::default();
        let mut history: Vec<(ModelToolCall, Value)> = Vec::new();
        let mut actions: Vec<Value> = Vec::new();
        let mut audit_ids = Vec::new();
        let mut provider_turn = self
            .provider_tool_offer(
                &input.provider,
                &input.model,
                key,
                context,
                instructions,
                tools,
                input.reasoning_effort.as_deref(),
            )
            .await?;
        let original_messages = context
            .iter()
            .map(|m| json!({"role":m.role,"content":m.text}))
            .collect::<Vec<_>>();
        let mut provider_history = original_messages;
        if !instructions.is_empty() {
            provider_history.insert(
                0,
                json!({"role":"system","content":format!(
                "User-enabled response instructions:\n\n{}",instructions.join("\n\n"))}),
            );
        }
        let names = tools
            .iter()
            .filter_map(|v| v["name"].as_str())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        loop {
            match provider_turn {
                ProviderTurn::Text(answer) => {
                    if history.is_empty() {
                        return Err("Model did not issue an actual Android tool call; no action was performed".into());
                    }
                    let confirmed = actions
                        .iter()
                        .filter_map(|action| action["name"].as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Ok((format!(
                        "Verified app-owned native tools: {confirmed}. No other device actions were confirmed.\n\nAI response: {answer}"
                    ),actions,audit_ids));
                }
                ProviderTurn::ToolProposal(proposal) => {
                    local_agent_intent::authorize(scope, package, &proposal, &progress)?;
                    let native_args =
                        local_agent_tools::dispatch_arguments(&proposal.name, &proposal.arguments)?;
                    if self.local_tool_generation.load(Ordering::SeqCst) != generation
                        || !self
                            .local_tool_host
                            .lock()
                            .map_err(|_| "Android agent host lock unavailable")?
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(current, host))
                    {
                        return Err("Native Android agent host changed; no action executed".into());
                    }
                    let id = Uuid::new_v4().to_string();
                    let request_id = input.request_id.as_deref().ok_or("Request ID missing")?;
                    local_agent_audit::before_intent_dispatch(
                        &self.home,
                        &id,
                        request_id,
                        &proposal.name,
                    )?;
                    // Host does not see arbitrary provider prose; only the
                    // checked name/typed args of this one bounded proposal.
                    let result = host.execute(&proposal.name, native_args).await;
                    let verified = result
                        .and_then(|raw| local_agent_tools::checked_result(&proposal.name, raw));
                    if verified.is_err() {
                        let _ = local_agent_audit::advance(&self.home, &id, "outcome_uncertain");
                    }
                    let verified = verified?;
                    local_agent_audit::advance(&self.home, &id, "native_confirmed")?;
                    local_agent_intent::record(scope, &proposal.name, &verified, &mut progress);
                    actions.push(json!({"type":"android_local_tool_call",
                        "name":proposal.name,"approvedByUser":false,
                        "userIntentAuthorized":true,"resultIsError":false,"resultStored":false}));
                    audit_ids.push(id);
                    history.push((proposal, verified));
                    // A verified primary action (open, stop, click, fill)
                    // finishes this instruction. In particular do NOT read
                    // a just-opened WebView before navigation is ready or
                    // let a model chain unrelated post-launch actions.
                    let allow_next = history.len() < MAX_NATIVE_STEPS && !progress.primary_done;
                    let payload = provider_tools::local_agent_continuation_payload(
                        &input.provider,
                        &input.model,
                        &provider_history,
                        tools,
                        &history,
                        allow_next,
                    )?;
                    provider_turn = self
                        .provider_tool_exchange(
                            &input.provider,
                            key,
                            &payload,
                            if allow_next { &names } else { &[] },
                            input.reasoning_effort.as_deref(),
                        )
                        .await?;
                }
            }
        }
    }

    /// Called *only* for an explicitly classified current-user Android chat
    /// instruction. Other messages are handled by the ordinary text provider.
    pub(super) async fn run_intent_agent_chat(
        &self,
        input: ChatRequest,
        previous: Option<Session>,
        context: Vec<Message>,
        instructions: Vec<String>,
        key: String,
        host: Arc<dyn AndroidLocalToolHost>,
        generation: u64,
        scope: Intent,
    ) -> Result<Value, String> {
        let request_id = input
            .request_id
            .as_deref()
            .ok_or("Agent request ID missing")?
            .to_owned();
        canonical_request_id(&request_id)?;
        let (sender, mut cancelled) = tokio::sync::oneshot::channel();
        {
            let mut active = self
                .chat_cancellations
                .lock()
                .map_err(|_| "Agent cancellation coordinator unavailable")?;
            if active.contains_key(&request_id) || active.len() >= 3 {
                return Err("Agent request is duplicate or all slots are busy".into());
            }
            active.insert(request_id.clone(), sender);
        }
        let _cleanup = ChatCleanup {
            engine: self,
            id: request_id.clone(),
        };
        self.chat_progress
            .lock()
            .map_err(|_| "Chat progress unavailable")?
            .insert(request_id.clone(), String::new());
        let result = tokio::select! {
            result=async {
                let package=self.trusted_package(&host,&scope).await?;
                let definitions=local_agent_intent::definitions_for(&scope);
                self.run_scoped_model_loop(&input,&context,&instructions,&key,&host,
                    generation,&scope,&definitions,package.as_deref()).await
            } => result,
            _=&mut cancelled => Err("Android agent cancelled; prior native effects may have occurred".into()),
            _=wall_clock(MAX_CHAT_WALL) => Err("Android agent time budget reached; native outcome may be uncertain".into()),
        };
        let still_active = self
            .chat_cancellations
            .lock()
            .map_err(|_| "Agent cancellation unavailable")?
            .remove(&request_id)
            .is_some();
        if !still_active {
            return Err("Android agent cancelled; do not retry uncertain native actions".into());
        }
        let (answer, actions, audits) = result?;
        let saved = self.append_completed_turn_with_actions(input, previous, answer, actions)?;
        let mut warning = false;
        for id in &audits {
            if local_agent_audit::advance(&self.home, id, "answer_saved").is_err() {
                warning = true;
            }
        }
        if warning {
            Ok(
                json!({"sessionId":saved["sessionId"],"answer":saved["answer"],
                "actions":saved["actions"],"auditWarning":"Native answer saved but audit finalization failed; review before repeating"}),
            )
        } else {
            Ok(saved)
        }
    }

    /// Explicit native action fallback for text-only ChatGPT OAuth sessions.
    /// A direct command has NO model inference and NEVER claims that OAuth
    /// function calling took place. It stores a separate native-local chat.
    pub(super) async fn run_direct_local_chat_action(&self, body: Value) -> Result<Value, String> {
        if !cfg!(target_os = "android") && !cfg!(test) {
            return Err("Native local chat automation requires Android".into());
        }
        let data = body
            .as_object()
            .ok_or("Native chat action must be object")?;
        if data.keys().any(|key| {
            ![
                "prompt",
                "action",
                "arguments",
                "requestId",
                "sessionId",
                "userInitiated",
            ]
            .contains(&key.as_str())
        }) {
            return Err("Unexpected native chat action fields".into());
        }
        if body["userInitiated"] != true {
            return Err("Native chat action must originate in user-authored instruction".into());
        }
        let prompt = body["prompt"]
            .as_str()
            .ok_or("Current chat instruction missing")?;
        let scope = local_agent_intent::parse(prompt)
            .ok_or("Unclear native command; use exact HTTPS URL or 'buka <installed app>'")?;
        let action = body["action"]
            .as_str()
            .ok_or("Native action name missing")?;
        let arguments = body
            .get("arguments")
            .ok_or("Native action arguments missing")?;
        let request_id = body["requestId"]
            .as_str()
            .ok_or("Native request UUID missing")?;
        canonical_request_id(request_id)?;
        let session_id = match body.get("sessionId") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if Uuid::parse_str(s).is_ok() => Some(s.clone()),
            _ => return Err("Native session ID invalid".into()),
        };
        let previous = if let Some(ref session) = session_id {
            let old = self
                .sessions()?
                .into_iter()
                .find(|s| &s.id == session)
                .ok_or("Native chat session missing")?;
            if old.provider != "local-automation" || old.model != "native-agent" {
                return Err(
                    "Native chat action cannot alter an OAuth or API-provider session".into(),
                );
            }
            Some(old)
        } else {
            None
        };
        let host = self
            .local_tool_host
            .lock()
            .map_err(|_| "Android native host unavailable")?
            .as_ref()
            .cloned()
            .ok_or("Android native automation host not installed")?;
        // One shared deadline covers trusted label lookup AND native launch;
        // neither may hang chat indefinitely. If the deadline fires after a
        // dispatch, its pre-dispatch ledger remains conservatively uncertain.
        let (name, id, checked) = tokio::select! {
            result=async {
                let package=self.trusted_package(&host,&scope).await?;
                let (name,typed_args)=local_agent_intent::direct_arguments(
                    &scope,action,arguments,package.as_deref())?;
                local_agent_tools::validate(&name,&typed_args)?;
                let id=Uuid::new_v4().to_string();
                local_agent_audit::before_intent_dispatch(&self.home,&id,request_id,&name)?;
                let native_result=host.execute(&name,typed_args).await;
                let checked=native_result.and_then(|value|
                    local_agent_tools::checked_result(&name,value));
                if checked.is_err() {
                    let _=local_agent_audit::advance(&self.home,&id,"outcome_uncertain");
                }
                let checked=checked?;
                local_agent_audit::advance(&self.home,&id,"native_confirmed")?;
                Ok::<_,String>((name,id,checked))
            } => result,
            _=wall_clock(MAX_DIRECT_WALL) => Err("Android native action exceeded time budget; outcome uncertain".into()),
        }?;
        let answer = local_agent_intent::native_message(&name, &checked)?;
        let input = ChatRequest {
            prompt: prompt.to_owned(),
            provider: "local-automation".into(),
            model: "native-agent".into(),
            session_id,
            request_id: Some(request_id.to_owned()),
            allow_mcp_tools: false,
            allow_local_tools: false,
            reasoning_effort: None,
        };
        let meta = json!({"type":"android_local_direct_action","name":name,
            "approvedByUser":false,"userIntentAuthorized":true,
            "resultIsError":false,"resultStored":false,"nativeConfirmed":true});
        let saved = self.append_completed_turn_with_actions(input, previous, answer, vec![meta])?;
        if local_agent_audit::advance(&self.home, &id, "answer_saved").is_err() {
            return Ok(
                json!({"sessionId":saved["sessionId"],"answer":saved["answer"],
                "actions":saved["actions"],"auditWarning":"Local native reply saved, but audit finalization failed"}),
            );
        }
        Ok(saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as TestOrdering};

    struct FakeNativeHost {
        dispatched: AtomicUsize,
        resolved: AtomicUsize,
    }

    impl AndroidLocalToolHost for FakeNativeHost {
        fn execute<'a>(
            &'a self,
            name: &'a str,
            args: Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.dispatched.fetch_add(1, TestOrdering::SeqCst);
                match name {
                    "android_app_start"
                        if args["targetPackage"] == "com.google.android.youtube" =>
                    {
                        Ok(json!({"connected":true,"active":true,"visible":true,
                            "stopRequested":false,"launched":true,
                            "targetPackage":"com.google.android.youtube"}))
                    }
                    "android_browser_start" if args["url"] == "https://example.org/post" => {
                        Ok(json!({"supported":true,"running":true,"ready":false,
                            "profileIsolated":true,"title":""}))
                    }
                    "android_browser_start"
                        if args["url"] == "https://example.org/"
                            || args["url"] == "https://EXAMPLE.ORG/" =>
                    {
                        Ok(json!({"supported":true,"running":true,"ready":false,
                            "profileIsolated":true,"title":""}))
                    }
                    _ => Err("Fake host rejects unknown native request".into()),
                }
            })
        }

        fn resolve_app_label<'a>(
            &'a self,
            query: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<(String, String)>, String>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                self.resolved.fetch_add(1, TestOrdering::SeqCst);
                Ok(if query.eq_ignore_ascii_case("YouTube") {
                    Some(("com.google.android.youtube".into(), "YouTube".into()))
                } else {
                    None
                })
            })
        }
    }

    #[tokio::test]
    async fn explicit_android_user_chat_calls_real_mock_host_once_and_records_only_evidence() {
        let folder = std::env::temp_dir().join(format!("native-direct-agent-{}", Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let host = Arc::new(FakeNativeHost {
            dispatched: AtomicUsize::new(0),
            resolved: AtomicUsize::new(0),
        });
        engine
            .install_android_local_tool_host(host.clone())
            .unwrap();
        let request = Uuid::new_v4().to_string();
        let body = json!({"prompt":"buka YouTube","action":"android_app_start",
            "arguments":{"query":"YouTube"},"requestId":request,
            "sessionId":null,"userInitiated":true});
        let receipt = engine
            .run_direct_local_chat_action(body.clone())
            .await
            .unwrap();
        assert_eq!(host.dispatched.load(TestOrdering::SeqCst), 1);
        assert_eq!(host.resolved.load(TestOrdering::SeqCst), 1);
        assert_eq!(receipt["actions"][0]["nativeConfirmed"], true);
        assert_eq!(receipt["actions"][0]["approvedByUser"], false);
        assert_eq!(receipt["actions"][0]["userIntentAuthorized"], true);
        assert!(receipt["answer"]
            .as_str()
            .unwrap()
            .contains("Blue Accessibility"));
        let id = receipt["sessionId"].as_str().unwrap();
        let stored = engine.sessions().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, id);
        assert_eq!(stored[0].provider, "local-automation");
        assert_eq!(stored[0].model, "native-agent");
        // The same current-user UUID cannot launch a second time, including
        // a fresh fabricated review UUID on the identical native action.
        assert!(engine.run_direct_local_chat_action(body).await.is_err());
        assert_eq!(host.dispatched.load(TestOrdering::SeqCst), 1);
        let wrong = json!({"prompt":"buka YouTube","action":"android_app_start",
            "arguments":{"query":"WhatsApp"},"requestId":Uuid::new_v4().to_string(),
            "sessionId":null,"userInitiated":true});
        assert!(engine.run_direct_local_chat_action(wrong).await.is_err());
        assert_eq!(host.dispatched.load(TestOrdering::SeqCst), 1);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[tokio::test]
    async fn direct_link_requires_exact_user_url_and_does_not_touch_oauth_session() {
        let folder = std::env::temp_dir().join(format!("native-link-agent-{}", Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let host = Arc::new(FakeNativeHost {
            dispatched: AtomicUsize::new(0),
            resolved: AtomicUsize::new(0),
        });
        engine
            .install_android_local_tool_host(host.clone())
            .unwrap();
        let valid = json!({"prompt":"buka https://example.org/post","action":"android_browser_start",
            "arguments":{"url":"https://example.org/post"},"requestId":Uuid::new_v4().to_string(),
            "sessionId":null,"userInitiated":true});
        let result = engine.run_direct_local_chat_action(valid).await.unwrap();
        assert!(result["answer"].as_str().unwrap().contains("running=true"));
        let bad = json!({"prompt":"https://example.org/post","action":"android_browser_start",
            "arguments":{"url":"https://unrelated.org/post"},"requestId":Uuid::new_v4().to_string(),
            "sessionId":null,"userInitiated":true});
        assert!(engine.run_direct_local_chat_action(bad).await.is_err());
        let browser_session = result["sessionId"].as_str().unwrap();
        let wrong_provider = engine.persist_oauth_chat_turn(super::super::OAuthChatTurnReceipt {
            request_id: Uuid::new_v4().to_string(),
            session_id: Some(browser_session.into()),
            prompt: "hello".into(),
            answer: "OAuth reply".into(),
            expected_session: None,
        });
        assert!(wrong_provider.is_err());
        assert_eq!(host.dispatched.load(TestOrdering::SeqCst), 1);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[tokio::test]
    async fn auto_capitalized_https_url_dispatches_canonical_scheme_only() {
        let folder = std::env::temp_dir().join(format!("native-capital-url-{}", Uuid::new_v4()));
        let engine = Engine::new(folder.clone()).unwrap();
        let host = Arc::new(FakeNativeHost {
            dispatched: AtomicUsize::new(0),
            resolved: AtomicUsize::new(0),
        });
        engine
            .install_android_local_tool_host(host.clone())
            .unwrap();
        for (prompt, args) in [
            ("Https://example.org/", "https://example.org/"),
            ("HTTPS://EXAMPLE.ORG/", "HTTPS://EXAMPLE.ORG/"),
        ] {
            let receipt = engine
                .run_direct_local_chat_action(json!({
                    "prompt":prompt,"action":"android_browser_start",
                    "arguments":{"url":args},"requestId":Uuid::new_v4().to_string(),
                    "sessionId":null,"userInitiated":true
                }))
                .await
                .unwrap();
            assert_eq!(receipt["actions"][0]["nativeConfirmed"], true);
        }
        assert_eq!(host.dispatched.load(TestOrdering::SeqCst), 2);
        assert_eq!(host.resolved.load(TestOrdering::SeqCst), 0);
        std::fs::remove_dir_all(folder).unwrap();
    }
}
