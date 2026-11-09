//! Pure, bounded tool proposal parsing and *manual* continuation formatting.
//! A proposal is UNTRUSTED model output, not authorization to execute a tool.
//! The host must separately authorize and execute a single approved tool, then
//! explicitly supply a verified result. This module performs no I/O.

use serde_json::{json, Map, Value};
use std::collections::HashSet;

const MAX_ARGS: usize = 8 * 1024;
const MAX_RESULT: usize = 8 * 1024;
const MAX_TEXT: usize = 32 * 1024;
const MAX_PROVIDER_RESPONSE: usize = 128 * 1024;
const MAX_REQUEST: usize = 128 * 1024;
const MAX_MESSAGES: usize = 64;
const MAX_TOOLS: usize = 16;
const MAX_TOOL_DEFINITION: usize = 16 * 1024;
const MAX_NAME: usize = 128;
const MAX_ID: usize = 128;

#[derive(Clone, Debug, PartialEq)]
pub struct ModelToolCall {
    pub id: String,
    pub name: String,
    /// JSON object, never an unparsed executable string.
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProviderTurn {
    Text(String),
    ToolProposal(ModelToolCall),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    ChatCompletions,
    Anthropic,
}

fn provider_kind(provider: &str) -> Result<Kind, String> {
    match provider {
        "openai" | "openrouter" => Ok(Kind::ChatCompletions),
        "anthropic" => Ok(Kind::Anthropic),
        _ => Err("Unsupported tool-call provider".into()),
    }
}

fn json_bytes(value: &Value, max: usize) -> Result<Vec<u8>, String> {
    let bytes = serde_json::to_vec(value).map_err(|_| "Invalid JSON envelope")?;
    if bytes.len() > max {
        return Err("JSON envelope exceeds size limit".into());
    }
    Ok(bytes)
}

fn object(value: &Value) -> Result<&Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| "Expected JSON object".into())
}

fn exact_keys(obj: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    if obj.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("Unsupported response fields".into());
    }
    Ok(())
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| "Missing or invalid response field".into())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn validate_allowlist(allowed_names: &[String]) -> Result<(), String> {
    if allowed_names.len() > MAX_TOOLS {
        return Err("Too many approved tool names".into());
    }
    let mut unique = HashSet::new();
    for name in allowed_names {
        if !valid_name(name) || !unique.insert(name.as_str()) {
            return Err("Invalid approved tool names".into());
        }
    }
    Ok(())
}

fn approved_call(
    id: &str,
    name: &str,
    arguments: Value,
    allowed_names: &[String],
) -> Result<ProviderTurn, String> {
    if !valid_id(id) {
        return Err("Missing or invalid tool-call ID".into());
    }
    if !valid_name(name) || !allowed_names.iter().any(|allowed| allowed == name) {
        return Err("Tool name was not explicitly approved".into());
    }
    if !arguments.is_object() || json_bytes(&arguments, MAX_ARGS).is_err() {
        return Err("Tool arguments must be a JSON object within 8 KiB".into());
    }
    // JSON syntax alone cannot authorize a nested tool envelope: this module
    // only ever proposes one top-level tool, never delegation to another tool.
    // Actual value/schema matching is intentionally enforced by the host's
    // verified MCP definition, both before review and before tools/call.
    reject_nested_tool_calls(&arguments)?;
    reject_credentials(&arguments)?;
    Ok(ProviderTurn::ToolProposal(ModelToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments,
    }))
}

fn reject_nested_tool_calls(value: &Value) -> Result<(), String> {
    let mut pending = vec![(value, 0usize)];
    let mut visited = 0usize;
    while let Some((node, depth)) = pending.pop() {
        visited += 1;
        if depth > 32 || visited > 4096 {
            return Err("Nested tool content exceeds safety limit".into());
        }
        match node {
            Value::Object(map) => {
                if map.keys().any(|key| {
                    matches!(
                        key.as_str(),
                        "tool_calls"
                            | "tool_call_id"
                            | "tool_use"
                            | "tool_result"
                            | "tool_use_id"
                            | "function_call"
                    )
                }) || map.get("type").and_then(Value::as_str).is_some_and(|kind| {
                    matches!(kind, "tool_use" | "tool_result" | "function_call")
                }) || map.get("role").and_then(Value::as_str) == Some("tool")
                {
                    return Err("Nested tool-call envelopes are not permitted".into());
                }
                pending.extend(map.values().map(|child| (child, depth + 1)));
            }
            Value::Array(items) => {
                pending.extend(items.iter().map(|child| (child, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Checks common credential markers without printing sensitive content. This is
/// deliberately conservative, not a universal secret detector. The host must
/// additionally ensure all tool results are authorized and actually verified.
fn reject_credentials(value: &Value) -> Result<(), String> {
    let mut pending = vec![(value, 0usize)];
    let mut seen = 0usize;
    while let Some((node, depth)) = pending.pop() {
        seen += 1;
        if depth > 32 || seen > 4096 {
            return Err("Untrusted content nesting exceeds limit".into());
        }
        match node {
            Value::Object(map) => {
                for (key, child) in map {
                    let key = key.to_ascii_lowercase().replace('-', "_");
                    if matches!(
                        key.as_str(),
                        "password"
                            | "passwd"
                            | "auth"
                            | "authorization"
                            | "token"
                            | "api_key"
                            | "apikey"
                            | "access_token"
                            | "accesstoken"
                            | "session_token"
                            | "refresh_token"
                            | "refreshtoken"
                            | "secret"
                            | "secret_key"
                            | "secretkey"
                            | "client_secret"
                            | "clientsecret"
                            | "private_key"
                            | "privatekey"
                            | "credentials"
                            | "cookie"
                            | "set_cookie"
                    ) || key.ends_with("_api_key")
                        || key.ends_with("_apikey")
                        || key.ends_with("apikey")
                        || key.ends_with("_token")
                        || key.ends_with("token")
                        || key.ends_with("_secret")
                        || key.ends_with("secret")
                        || key.ends_with("_password")
                        || key.ends_with("password")
                        || key.ends_with("_private_key")
                        || key.ends_with("privatekey")
                        || key.ends_with("_authorization")
                        || key.ends_with("_cookie")
                    {
                        return Err("Credential-bearing content is not allowed".into());
                    }
                    pending.push((child, depth + 1));
                }
            }
            Value::Array(items) => {
                for child in items {
                    pending.push((child, depth + 1));
                }
            }
            Value::String(content) => {
                let normalized = content.to_ascii_lowercase();
                if [
                    "bearer ",
                    "sk-",
                    "ghp_",
                    "gho_",
                    "xoxb-",
                    "-----begin private key-----",
                    "-----begin rsa private key-----",
                    "password=",
                    "password:",
                    "api_key=",
                    "api_key:",
                    "x-api-key:",
                    "x_api_key:",
                    "authorization:",
                    "access_token=",
                    "access_token:",
                    "secret=",
                    "secret:",
                    "cookie:",
                    "token=",
                    "token:",
                    "refresh_token=",
                    "refresh_token:",
                ]
                .iter()
                .any(|marker| normalized.contains(marker))
                {
                    return Err("Credential-bearing content is not allowed".into());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Parse *one completed* provider message, never streaming deltas. Arbitrary
/// provider metadata is ignored, but response role/content/finish semantics
/// are strictly validated. No tool invocation takes place.
pub fn parse_provider_turn(
    provider: &str,
    payload: &Value,
    allowed_names: &[String],
) -> Result<ProviderTurn, String> {
    validate_allowlist(allowed_names)?;
    json_bytes(payload, MAX_PROVIDER_RESPONSE)?;
    match provider_kind(provider)? {
        Kind::ChatCompletions => parse_chat_completions(payload, allowed_names),
        Kind::Anthropic => parse_anthropic(payload, allowed_names),
    }
}

fn parse_chat_completions(
    payload: &Value,
    allowed_names: &[String],
) -> Result<ProviderTurn, String> {
    // Preserve the compatible minimal envelope accepted by the host's tests,
    // while rejecting contradictory metadata when a provider supplies it.
    if payload.get("error").is_some_and(|error| !error.is_null())
        || payload
            .get("object")
            .is_some_and(|object| object.as_str() != Some("chat.completion"))
        || payload
            .get("id")
            .is_some_and(|id| !id.as_str().is_some_and(valid_id))
        || payload
            .get("created")
            .is_some_and(|created| created.as_u64().is_none())
        || payload.get("model").is_some_and(|model| {
            !model
                .as_str()
                .is_some_and(|model| !model.is_empty() && model.len() <= 128)
        })
    {
        return Err("Invalid chat-completions envelope metadata".into());
    }
    let choices = payload
        .get("choices")
        .and_then(Value::as_array)
        .ok_or("Invalid chat-completions choices")?;
    if choices.len() != 1 {
        return Err("Expected exactly one completed response choice".into());
    }
    let choice = &choices[0];
    exact_keys(
        object(choice)?,
        &["index", "message", "finish_reason", "logprobs"],
    )?;
    if choice.get("index").and_then(Value::as_u64) != Some(0)
        || (choice.get("logprobs").is_some() && !choice["logprobs"].is_null())
    {
        return Err("Unsupported response choice".into());
    }
    let finish = required_str(choice, "finish_reason")?;
    let message = choice.get("message").ok_or("Missing assistant message")?;
    exact_keys(
        object(message)?,
        &["role", "content", "tool_calls", "refusal"],
    )?;
    if required_str(message, "role")? != "assistant"
        || (message.get("refusal").is_some() && !message["refusal"].is_null())
    {
        return Err("Unsupported response role or refusal".into());
    }
    let calls = match message.get("tool_calls") {
        None | Some(Value::Null) => None,
        Some(Value::Array(values)) if values.is_empty() => None,
        Some(Value::Array(values)) => Some(values),
        _ => return Err("Invalid tool-call list".into()),
    };
    if let Some(calls) = calls {
        if finish != "tool_calls" || calls.len() != 1 {
            return Err("Only one completed tool call is supported".into());
        }
        // Valid completions may include a preamble with tool_calls. It is
        // untrusted and intentionally discarded; only the call is proposed.
        if let Some(content) = message.get("content") {
            if !content.is_null() && !content.as_str().is_some_and(|s| s.len() <= MAX_TEXT) {
                return Err("Unsupported tool-call message content".into());
            }
            // The preamble is discarded, but must not conceal credential data
            // before the proposal is displayed for user approval.
            if !content.is_null() {
                reject_credentials(content)?;
            }
        }
        let call = &calls[0];
        exact_keys(object(call)?, &["id", "type", "function"])?;
        if required_str(call, "type")? != "function" {
            return Err("Unsupported tool-call type".into());
        }
        let function = call.get("function").ok_or("Missing tool function")?;
        exact_keys(object(function)?, &["name", "arguments"])?;
        let raw = required_str(function, "arguments")?;
        if raw.len() > MAX_ARGS {
            return Err("Tool arguments exceed 8 KiB".into());
        }
        let arguments: Value =
            serde_json::from_str(raw).map_err(|_| "Tool arguments are not valid JSON")?;
        return approved_call(
            required_str(call, "id")?,
            required_str(function, "name")?,
            arguments,
            allowed_names,
        );
    }
    if finish != "stop" {
        return Err("Incomplete or unsupported chat-completions finish reason".into());
    }
    let text = required_str(message, "content")?;
    if text.trim().is_empty() || text.len() > MAX_TEXT {
        return Err("Model text exceeds limit".into());
    }
    reject_credentials(&Value::String(text.to_owned()))?;
    Ok(ProviderTurn::Text(text.to_owned()))
}

fn parse_anthropic(payload: &Value, allowed_names: &[String]) -> Result<ProviderTurn, String> {
    if payload.get("error").is_some_and(|error| !error.is_null())
        || payload
            .get("id")
            .is_some_and(|id| !id.as_str().is_some_and(valid_id))
        || payload.get("model").is_some_and(|model| {
            !model
                .as_str()
                .is_some_and(|model| !model.is_empty() && model.len() <= 128)
        })
    {
        return Err("Invalid Anthropic envelope metadata".into());
    }
    if required_str(payload, "type")? != "message" || required_str(payload, "role")? != "assistant"
    {
        return Err("Unsupported Anthropic envelope role or type".into());
    }
    let reason = required_str(payload, "stop_reason")?;
    let blocks = payload
        .get("content")
        .and_then(Value::as_array)
        .ok_or("Invalid Anthropic content blocks")?;
    if blocks.is_empty() || blocks.len() > 16 {
        return Err("Unsupported Anthropic block count".into());
    }
    let mut text = String::new();
    let mut proposal: Option<ProviderTurn> = None;
    for block in blocks {
        match required_str(block, "type")? {
            "text" => {
                exact_keys(object(block)?, &["type", "text"])?;
                let fragment = required_str(block, "text")?;
                if text.len() + fragment.len() + 1 > MAX_TEXT {
                    return Err("Model text exceeds limit".into());
                }
                reject_credentials(&Value::String(fragment.to_owned()))?;
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(fragment);
            }
            "tool_use" => {
                exact_keys(object(block)?, &["type", "id", "name", "input"])?;
                if proposal.is_some() {
                    return Err("Multiple model tool calls are unsupported".into());
                }
                proposal = Some(approved_call(
                    required_str(block, "id")?,
                    required_str(block, "name")?,
                    block.get("input").ok_or("Missing tool arguments")?.clone(),
                    allowed_names,
                )?);
            }
            _ => return Err("Unsupported Anthropic content block".into()),
        }
    }
    match proposal {
        // Anthropic may include a bounded text preamble with one tool_use.
        // Only the validated tool proposal survives; no text is executed.
        Some(call) if reason == "tool_use" => Ok(call),
        Some(_) => Err("Incomplete Anthropic tool content".into()),
        None if reason == "end_turn" && !text.trim().is_empty() => Ok(ProviderTurn::Text(text)),
        None => Err("Incomplete or unsupported Anthropic stop reason".into()),
    }
}

fn validate_history(
    kind: Kind,
    original: &[Value],
) -> Result<(Vec<Value>, Option<String>), String> {
    // Two messages (assistant call, tool result) are always appended.
    if original.len() > MAX_MESSAGES - 2 {
        return Err("Too many original messages".into());
    }
    let mut messages = Vec::with_capacity(original.len());
    let mut system = None;
    for (index, message) in original.iter().enumerate() {
        exact_keys(object(message)?, &["role", "content"])?;
        let role = required_str(message, "role")?;
        let valid_role = match kind {
            Kind::ChatCompletions => matches!(role, "system" | "developer" | "user" | "assistant"),
            Kind::Anthropic => matches!(role, "system" | "user" | "assistant"),
        };
        if !valid_role {
            return Err("Unsupported original message role".into());
        }
        let content = required_str(message, "content")?;
        if content.len() > MAX_TEXT {
            return Err("Original message exceeds size limit".into());
        }
        reject_credentials(&Value::String(content.to_owned()))?;
        if kind == Kind::Anthropic && role == "system" {
            if index != 0 || system.is_some() {
                return Err("Anthropic system message must be first and unique".into());
            }
            system = Some(content.to_owned());
        } else {
            messages.push(message.clone());
        }
    }
    Ok((messages, system))
}

fn validated_tool_definition(kind: Kind, value: &Value) -> Result<(String, Value), String> {
    json_bytes(value, MAX_TOOL_DEFINITION)?;
    // The host's provider-neutral tool schema uses inputSchema. Convert it to
    // the provider-specific wrapper rather than forwarding unexpected fields.
    if value.get("inputSchema").is_some() {
        exact_keys(object(value)?, &["name", "description", "inputSchema"])?;
        let name = required_str(value, "name")?;
        let description = value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !valid_name(name)
            || description.len() > 1024
            || value.get("description").is_some_and(|v| !v.is_string())
        {
            return Err("Invalid provider-neutral tool definition".into());
        }
        let schema = value.get("inputSchema").ok_or("Missing input schema")?;
        if !schema.is_object() {
            return Err("Invalid provider-neutral tool schema".into());
        }
        reject_credentials(value)?;
        let translated = match kind {
            Kind::ChatCompletions => json!({"type":"function","function":{
                "name":name,"description":description,"parameters":schema}}),
            Kind::Anthropic => json!({"name":name,"description":description,"input_schema":schema}),
        };
        return Ok((name.to_owned(), translated));
    }
    let (name, result) = match kind {
        Kind::ChatCompletions => {
            exact_keys(object(value)?, &["type", "function"])?;
            if required_str(value, "type")? != "function" {
                return Err("Unsupported tool definition type".into());
            }
            let function = value.get("function").ok_or("Missing tool function")?;
            exact_keys(
                object(function)?,
                &["name", "description", "parameters", "strict"],
            )?;
            let name = required_str(function, "name")?;
            let description = function
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            if description.len() > 1024
                || function.get("description").is_some_and(|v| !v.is_string())
            {
                return Err("Invalid tool description".into());
            }
            let schema = function
                .get("parameters")
                .ok_or("Missing tool parameters")?;
            if !schema.is_object() {
                return Err("Invalid tool parameter schema".into());
            }
            let mut safe = json!({"name":name,"description":description,"parameters":schema});
            if let Some(strict) = function.get("strict") {
                if !strict.is_boolean() {
                    return Err("Invalid tool strict flag".into());
                }
                safe["strict"] = strict.clone();
            }
            (name.to_owned(), json!({"type":"function","function":safe}))
        }
        Kind::Anthropic => {
            exact_keys(object(value)?, &["name", "description", "input_schema"])?;
            let name = required_str(value, "name")?;
            let description = value
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            if description.len() > 1024 || value.get("description").is_some_and(|v| !v.is_string())
            {
                return Err("Invalid tool description".into());
            }
            let schema = value
                .get("input_schema")
                .ok_or("Missing tool input schema")?;
            if !schema.is_object() {
                return Err("Invalid tool input schema".into());
            }
            (
                name.to_owned(),
                json!({"name":name,"description":description,"input_schema":schema}),
            )
        }
    };
    if !valid_name(&name) {
        return Err("Invalid tool definition name".into());
    }
    reject_credentials(&result)?;
    Ok((name, result))
}

/// Formats exactly one already-authorized and completed tool result. This does
/// not execute anything or approve subsequent calls: tool_choice is "none".
/// `tools` must contain a unique provider-compatible definition for the
/// proposal's exact name; only that definition is forwarded (least exposure).
pub fn continuation_payload(
    provider: &str,
    model: &str,
    original_messages: &[Value],
    tools: &[Value],
    proposal: &ModelToolCall,
    verified_tool_result: &Value,
) -> Result<Value, String> {
    let kind = provider_kind(provider)?;
    if model.is_empty()
        || model.len() > 128
        || !model.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
    {
        return Err("Invalid model name".into());
    }
    reject_credentials(&Value::String(model.to_owned()))?;
    if !valid_id(&proposal.id) || !valid_name(&proposal.name) {
        return Err("Invalid proposed tool identity".into());
    }
    if !proposal.arguments.is_object() || json_bytes(&proposal.arguments, MAX_ARGS).is_err() {
        return Err("Tool arguments must be a JSON object within 8 KiB".into());
    }
    reject_nested_tool_calls(&proposal.arguments)?;
    reject_credentials(&proposal.arguments)?;
    let (mut messages, system) = validate_history(kind, original_messages)?;
    if tools.is_empty() || tools.len() > MAX_TOOLS {
        return Err("Approved tool definitions are missing or too numerous".into());
    }
    let mut names = HashSet::new();
    let mut approved: Option<Value> = None;
    for tool in tools {
        let (name, validated) = validated_tool_definition(kind, tool)?;
        if !names.insert(name.clone()) {
            return Err("Duplicate tool definition".into());
        }
        if name == proposal.name {
            approved = Some(validated);
        }
    }
    let approved = approved.ok_or("Proposed tool is absent from approved tool definitions")?;
    let encoded_result = json_bytes(verified_tool_result, MAX_RESULT)?;
    reject_nested_tool_calls(verified_tool_result)?;
    reject_credentials(verified_tool_result)?;
    let result_content = if let Some(text) = verified_tool_result.as_str() {
        text.to_owned()
    } else {
        String::from_utf8(encoded_result).map_err(|_| "Invalid tool result encoding")?
    };
    if result_content.len() > MAX_RESULT {
        return Err("Tool result exceeds 8 KiB".into());
    }
    let continuation = match kind {
        Kind::ChatCompletions => {
            messages.push(json!({
                "role":"assistant","content":null,
                "tool_calls":[{"id":proposal.id,"type":"function",
                    "function":{"name":proposal.name,
                        "arguments":serde_json::to_string(&proposal.arguments).map_err(|_| "Invalid tool arguments")?}}]
            }));
            messages
                .push(json!({"role":"tool","tool_call_id":proposal.id,"content":result_content}));
            json!({"model":model,"messages":messages,"tools":[approved],"tool_choice":"none"})
        }
        Kind::Anthropic => {
            messages.push(json!({"role":"assistant","content":[{"type":"tool_use",
                "id":proposal.id,"name":proposal.name,"input":proposal.arguments}]}));
            messages.push(json!({"role":"user","content":[{"type":"tool_result",
                "tool_use_id":proposal.id,"content":result_content}]}));
            let mut body = json!({"model":model,"max_tokens":2048,"messages":messages,
                "tools":[approved],"tool_choice":{"type":"none"}});
            if let Some(system) = system {
                body["system"] = Value::String(system);
            }
            body
        }
    };
    json_bytes(&continuation, MAX_REQUEST)?;
    Ok(continuation)
}

/// Continue a short *sequential* Android agent tool run. This is not the
/// remote MCP approval path above: the caller independently authorizes EACH
/// proposal against the original current user instruction and saves a native
/// audit checkpoint before dispatch. No assistant text grants authority.
/// Allow a further proposal only when the remaining action budget permits it.
pub fn local_agent_continuation_payload(
    provider: &str,
    model: &str,
    original_messages: &[Value],
    tools: &[Value],
    verified_calls: &[(ModelToolCall, Value)],
    allow_next_tool: bool,
) -> Result<Value, String> {
    let kind = provider_kind(provider)?;
    if model.is_empty()
        || model.len() > 128
        || !model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':' | b'/'))
    {
        return Err("Invalid local agent model".into());
    }
    if verified_calls.is_empty()
        || verified_calls.len() > 3
        || tools.is_empty()
        || tools.len() > MAX_TOOLS
    {
        return Err("Local agent continuation needs one to three verified results".into());
    }
    let (mut messages, system) = validate_history(kind, original_messages)?;
    if messages.len() + 2 * verified_calls.len() > MAX_MESSAGES {
        return Err("Local agent context exceeds message budget".into());
    }
    let mut available = std::collections::HashMap::new();
    let mut translated = Vec::with_capacity(tools.len());
    for value in tools {
        let (name, definition) = validated_tool_definition(kind, value)?;
        if available.insert(name, ()).is_some() {
            return Err("Duplicate local agent tool name".into());
        }
        translated.push(definition);
    }
    let mut ids = HashSet::new();
    for (call, result) in verified_calls {
        if !ids.insert(call.id.as_str())
            || !valid_id(&call.id)
            || !available.contains_key(&call.name)
            || !call.arguments.is_object()
        {
            return Err("Invalid or replayed Android local tool call".into());
        }
        json_bytes(&call.arguments, MAX_ARGS)?;
        reject_nested_tool_calls(&call.arguments)?;
        reject_credentials(&call.arguments)?;
        let encoded = json_bytes(result, MAX_RESULT)?;
        reject_nested_tool_calls(result)?;
        reject_credentials(result)?;
        let result_text =
            String::from_utf8(encoded).map_err(|_| "Local agent result invalid encoding")?;
        match kind {
            Kind::ChatCompletions => {
                messages.push(json!({"role":"assistant","content":null,"tool_calls":[{
                    "id":call.id,"type":"function","function":{
                        "name":call.name,"arguments":serde_json::to_string(&call.arguments)
                            .map_err(|_| "Invalid local tool JSON")?}}]}));
                messages.push(json!({"role":"tool","tool_call_id":call.id,"content":result_text}));
            }
            Kind::Anthropic => {
                messages.push(json!({"role":"assistant","content":[{
                    "type":"tool_use","id":call.id,"name":call.name,"input":call.arguments}]}));
                messages.push(json!({"role":"user","content":[{
                    "type":"tool_result","tool_use_id":call.id,"content":result_text}]}));
            }
        }
    }
    let mut payload = match kind {
        Kind::ChatCompletions => json!({"model":model,"messages":messages,
            "tools":translated,"tool_choice":if allow_next_tool {"auto"} else {"none"}}),
        Kind::Anthropic => {
            let mut body = json!({"model":model,"max_tokens":2048,"messages":messages,
                "tools":translated,"tool_choice":{"type":if allow_next_tool {"auto"} else {"none"}}});
            if let Some(system) = system {
                body["system"] = json!(system);
            }
            body
        }
    };
    json_bytes(&payload, MAX_REQUEST)?;
    Ok(std::mem::take(&mut payload))
}

#[cfg(test)]
mod local_agent_tests {
    use super::*;

    fn call(id: &str, name: &str) -> ModelToolCall {
        ModelToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        }
    }

    #[test]
    fn sequential_agent_results_are_bounded_and_tool_choice_changes_at_limit() {
        let tools = vec![
            json!({"name":"android_browser_status","description":"Read status",
            "inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
            json!({"name":"android_browser_snapshot","description":"Read page",
            "inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        ];
        let base = vec![json!({"role":"user","content":"Read page"})];
        let calls = vec![(
            call("call_001", "android_browser_status"),
            json!({"ready":true}),
        )];
        for provider in ["openai", "openrouter", "anthropic"] {
            let ready =
                local_agent_continuation_payload(provider, "gpt-6", &base, &tools, &calls, true)
                    .unwrap();
            if provider == "anthropic" {
                assert_eq!(ready["tool_choice"]["type"], "auto");
            } else {
                assert_eq!(ready["tool_choice"], "auto");
            }
            assert_eq!(ready["messages"].as_array().unwrap().len(), 3);
            let two = vec![
                calls[0].clone(),
                (
                    call("call_002", "android_browser_snapshot"),
                    json!({"snapshotId":5}),
                ),
            ];
            let last =
                local_agent_continuation_payload(provider, "gpt-6", &base, &tools, &two, false)
                    .unwrap();
            if provider == "anthropic" {
                assert_eq!(last["tool_choice"]["type"], "none");
            } else {
                assert_eq!(last["tool_choice"], "none");
            }
            assert_eq!(last["messages"].as_array().unwrap().len(), 5);
            let dup = vec![
                calls[0].clone(),
                (
                    call("call_001", "android_browser_snapshot"),
                    json!({"snapshotId":5}),
                ),
            ];
            assert!(
                local_agent_continuation_payload(provider, "gpt-6", &base, &tools, &dup, true)
                    .is_err()
            );
        }
        assert!(
            local_agent_continuation_payload("openai", "gpt-6", &base, &tools, &[], true).is_err()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> Vec<String> {
        vec!["read_status".into()]
    }

    fn openai_call(name: &str, arguments: &str) -> Value {
        json!({"id":"chatcmpl_1","choices":[{"index":0,"finish_reason":"tool_calls",
            "message":{"role":"assistant","content":null,"tool_calls":[{
                "id":"call_001","type":"function",
                "function":{"name":name,"arguments":arguments}}]}}]})
    }

    fn anthropic_call(name: &str, input: Value) -> Value {
        json!({"id":"msg_1","type":"message","role":"assistant","stop_reason":"tool_use",
            "content":[{"type":"tool_use","id":"toolu_001","name":name,"input":input}]})
    }

    fn tool_for(provider: &str) -> Vec<Value> {
        if provider == "anthropic" {
            vec![
                json!({"name":"read_status","description":"Read synthetic status",
                "input_schema":{"type":"object","properties":{"id":{"type":"string"}}}}),
            ]
        } else {
            vec![json!({"type":"function","function":{"name":"read_status",
                "description":"Read synthetic status",
                "parameters":{"type":"object","properties":{"id":{"type":"string"}}}}})]
        }
    }

    fn generic_tool() -> Vec<Value> {
        vec![
            json!({"name":"read_status","description":"Read synthetic status",
            "inputSchema":{"type":"object","properties":{"id":{"type":"string"}}}}),
        ]
    }

    #[test]
    fn chat_completions_text_and_approved_proposals() {
        let text = json!({"choices":[{"index":0,"finish_reason":"stop",
            "message":{"role":"assistant","content":"Synthetic answer"}}]});
        for provider in ["openai", "openrouter"] {
            assert_eq!(
                parse_provider_turn(provider, &text, &allowed()).unwrap(),
                ProviderTurn::Text("Synthetic answer".into())
            );
            assert_eq!(
                parse_provider_turn(
                    provider,
                    &openai_call("read_status", "{\"id\":\"s1\"}"),
                    &allowed()
                )
                .unwrap(),
                ProviderTurn::ToolProposal(ModelToolCall {
                    id: "call_001".into(),
                    name: "read_status".into(),
                    arguments: json!({"id":"s1"})
                })
            );
        }
    }

    #[test]
    fn anthropic_text_blocks_and_approved_proposal() {
        let text = json!({"type":"message","role":"assistant","stop_reason":"end_turn",
            "content":[{"type":"text","text":"Hello"},{"type":"text","text":"World"}]});
        assert_eq!(
            parse_provider_turn("anthropic", &text, &allowed()).unwrap(),
            ProviderTurn::Text("Hello\nWorld".into())
        );
        assert_eq!(
            parse_provider_turn(
                "anthropic",
                &anthropic_call("read_status", json!({"id":"x"})),
                &allowed()
            )
            .unwrap(),
            ProviderTurn::ToolProposal(ModelToolCall {
                id: "toolu_001".into(),
                name: "read_status".into(),
                arguments: json!({"id":"x"})
            })
        );
    }

    #[test]
    fn valid_assistant_preambles_do_not_expand_tool_permissions() {
        let mut openai = openai_call("read_status", "{}");
        openai["choices"][0]["message"]["content"] = json!("Working on it...");
        assert!(matches!(
            parse_provider_turn("openai", &openai, &allowed()),
            Ok(ProviderTurn::ToolProposal(_))
        ));
        let mut anthropic = anthropic_call("read_status", json!({}));
        anthropic["content"]
            .as_array_mut()
            .unwrap()
            .insert(0, json!({"type":"text","text":"Working on it..."}));
        assert!(matches!(
            parse_provider_turn("anthropic", &anthropic, &allowed()),
            Ok(ProviderTurn::ToolProposal(_))
        ));
        openai["choices"][0]["message"]["content"] = json!([{"type":"image_url"}]);
        assert!(parse_provider_turn("openai", &openai, &allowed()).is_err());
    }

    #[test]
    fn rejects_unapproved_multiple_missing_id_and_corrupt_arguments() {
        assert!(
            parse_provider_turn("openai", &openai_call("unapproved", "{}"), &allowed()).is_err()
        );
        assert!(parse_provider_turn(
            "anthropic",
            &anthropic_call("unapproved", json!({})),
            &allowed()
        )
        .is_err());
        assert!(parse_provider_turn(
            "openai",
            &openai_call("read_status", "not-json"),
            &allowed()
        )
        .is_err());
        assert!(
            parse_provider_turn("openai", &openai_call("read_status", "[]"), &allowed()).is_err()
        );
        assert!(parse_provider_turn(
            "openai",
            &openai_call(
                "read_status",
                &format!("{{\"x\":\"{}\"}}", "x".repeat(8200))
            ),
            &allowed()
        )
        .is_err());
        let mut missing_id = openai_call("read_status", "{}");
        missing_id["choices"][0]["message"]["tool_calls"][0]
            .as_object_mut()
            .unwrap()
            .remove("id");
        assert!(parse_provider_turn("openai", &missing_id, &allowed()).is_err());
        let mut double = openai_call("read_status", "{}");
        let call = double["choices"][0]["message"]["tool_calls"][0].clone();
        double["choices"][0]["message"]["tool_calls"]
            .as_array_mut()
            .unwrap()
            .push(call);
        assert!(parse_provider_turn("openrouter", &double, &allowed()).is_err());
        let mut two = anthropic_call("read_status", json!({}));
        let item = two["content"][0].clone();
        two["content"].as_array_mut().unwrap().push(item);
        assert!(parse_provider_turn("anthropic", &two, &allowed()).is_err());
        assert!(parse_provider_turn(
            "anthropic",
            &anthropic_call("read_status", json!([1, 2])),
            &allowed()
        )
        .is_err());
    }

    #[test]
    fn rejects_invalid_roles_blocks_and_incomplete_envelopes() {
        let mut chat = openai_call("read_status", "{}");
        chat["choices"][0]["message"]["role"] = json!("user");
        assert!(parse_provider_turn("openai", &chat, &allowed()).is_err());
        chat["choices"][0]["message"]["role"] = json!("assistant");
        chat["choices"][0]["finish_reason"] = json!("length");
        assert!(parse_provider_turn("openai", &chat, &allowed()).is_err());
        let mut anthropic = anthropic_call("read_status", json!({}));
        anthropic["content"][0]["type"] = json!("thinking");
        assert!(parse_provider_turn("anthropic", &anthropic, &allowed()).is_err());
        anthropic["content"][0]["type"] = json!("tool_use");
        anthropic["stop_reason"] = json!("end_turn");
        assert!(parse_provider_turn("anthropic", &anthropic, &allowed()).is_err());
        assert!(parse_provider_turn("ollama", &chat, &allowed()).is_err());
    }

    #[test]
    fn continuation_openai_openrouter_and_anthropic_are_bounded_and_ordered() {
        let original_chat = vec![
            json!({"role":"system","content":"You are helpful"}),
            json!({"role":"user","content":"Check status"}),
        ];
        for provider in ["openai", "openrouter"] {
            let proposal = match parse_provider_turn(
                provider,
                &openai_call("read_status", "{\"id\":\"x\"}"),
                &allowed(),
            )
            .unwrap()
            {
                ProviderTurn::ToolProposal(p) => p,
                _ => panic!("expected proposal"),
            };
            let request = continuation_payload(
                provider,
                "model-1",
                &original_chat,
                &generic_tool(),
                &proposal,
                &json!({"ok":true}),
            )
            .unwrap();
            assert_eq!(request["messages"].as_array().unwrap().len(), 4);
            assert_eq!(request["messages"][2]["tool_calls"][0]["id"], "call_001");
            assert_eq!(request["messages"][3]["role"], "tool");
            assert_eq!(request["messages"][3]["tool_call_id"], "call_001");
            assert_eq!(request["tool_choice"], "none");
            assert_eq!(request["tools"].as_array().unwrap().len(), 1);
            assert_eq!(request["tools"][0]["function"]["name"], "read_status");
            assert_eq!(
                request["tools"][0]["function"]["parameters"]["type"],
                "object"
            );
        }
        let proposal = match parse_provider_turn(
            "anthropic",
            &anthropic_call("read_status", json!({"id":"x"})),
            &allowed(),
        )
        .unwrap()
        {
            ProviderTurn::ToolProposal(p) => p,
            _ => panic!("expected proposal"),
        };
        let request = continuation_payload(
            "anthropic",
            "claude-1",
            &[
                json!({"role":"system","content":"Synthetic safety instructions"}),
                json!({"role":"user","content":"Check status"}),
            ],
            &generic_tool(),
            &proposal,
            &json!({"ok":true}),
        )
        .unwrap();
        assert_eq!(request["system"], "Synthetic safety instructions");
        assert_eq!(request["messages"].as_array().unwrap().len(), 3);
        assert_eq!(request["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(request["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(
            request["messages"][2]["content"][0]["tool_use_id"],
            "toolu_001"
        );
        assert_eq!(request["tool_choice"]["type"], "none");
        assert_eq!(request["max_tokens"], 2048);
        assert_eq!(request["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn continuation_rejects_oversized_results_credentials_and_unknown_tools() {
        let p = ModelToolCall {
            id: "call_1".into(),
            name: "read_status".into(),
            arguments: json!({"id":"x"}),
        };
        let messages = vec![json!({"role":"user","content":"ok"})];
        let tools = tool_for("openai");
        assert!(continuation_payload(
            "openai",
            "gpt-1",
            &messages,
            &tools,
            &p,
            &json!({"body":"x".repeat(8200)})
        )
        .is_err());
        assert!(continuation_payload(
            "openai",
            "gpt-1",
            &messages,
            &tools,
            &p,
            &json!({"api_key":"SENSITIVE"})
        )
        .is_err());
        assert!(continuation_payload(
            "openai",
            "gpt-1",
            &messages,
            &tools,
            &p,
            &json!("Bearer SENSITIVE")
        )
        .is_err());
        assert!(continuation_payload("openai", "gpt-1", &messages, &[], &p, &json!({})).is_err());
        assert!(continuation_payload(
            "openai",
            "gpt-1",
            &messages,
            &tool_for("anthropic"),
            &p,
            &json!({})
        )
        .is_err());
        assert!(continuation_payload(
            "openai",
            "gpt-1",
            &[json!({"role":"tool","content":"unexpected"})],
            &tools,
            &p,
            &json!({})
        )
        .is_err());
        assert!(continuation_payload(
            "openai",
            "gpt-1",
            &[json!({"role":"user","content":"sk-secret"})],
            &tools,
            &p,
            &json!({})
        )
        .is_err());
        assert!(continuation_payload(
            "openai",
            "gpt-1",
            &messages,
            &tools,
            &ModelToolCall {
                name: "unknown".into(),
                ..p
            },
            &json!({})
        )
        .is_err());
    }

    #[test]
    fn rejects_anthropic_system_in_middle_and_secret_tool_descriptions() {
        let call = ModelToolCall {
            id: "toolu_1".into(),
            name: "read_status".into(),
            arguments: json!({}),
        };
        let bad_system = vec![
            json!({"role":"user","content":"Hello"}),
            json!({"role":"system","content":"Late"}),
        ];
        assert!(continuation_payload(
            "anthropic",
            "claude-1",
            &bad_system,
            &generic_tool(),
            &call,
            &json!({})
        )
        .is_err());
        let leaking_tool = vec![
            json!({"name":"read_status","description":"Bearer SECRET", "inputSchema":{"type":"object"}}),
        ];
        assert!(continuation_payload(
            "anthropic",
            "claude-1",
            &[],
            &leaking_tool,
            &call,
            &json!({})
        )
        .is_err());
    }

    #[test]
    fn realistic_openai_openrouter_and_anthropic_envelopes() {
        // These mimic the complete metadata and content shapes returned by
        // actual Chat Completions / Messages endpoints, without network access.
        let chat = json!({
            "id":"chatcmpl-synthetic01","object":"chat.completion","created":1730000000,
            "model":"gpt-4.1-mini","system_fingerprint":"fp_synthetic",
            "choices":[{"index":0,"finish_reason":"tool_calls","logprobs":null,
                "message":{"role":"assistant","content":null,"refusal":null,
                    "tool_calls":[{"id":"call_synthetic01","type":"function",
                        "function":{"name":"read_status","arguments":"{\"id\":\"123\"}"}}]}}],
            "usage":{"prompt_tokens":50,"completion_tokens":21,"total_tokens":71}
        });
        for provider in ["openai", "openrouter"] {
            let ProviderTurn::ToolProposal(call) =
                parse_provider_turn(provider, &chat, &allowed()).unwrap()
            else {
                panic!("actual chat-completions shape must produce proposal")
            };
            assert_eq!(call.arguments, json!({"id":"123"}));
            let continued = continuation_payload(
                provider,
                "gpt-4.1-mini",
                &[json!({"role":"user","content":"Check synthetic status"})],
                &generic_tool(),
                &call,
                &json!({"ok":true}),
            )
            .unwrap();
            assert_eq!(continued["model"], "gpt-4.1-mini");
            assert_eq!(continued["tool_choice"], "none");
            assert_eq!(continued["messages"][2]["role"], "tool");
        }
        let anthropic = json!({
            "id":"msg_synthetic01","type":"message","role":"assistant",
            "model":"claude-sonnet-4-5","stop_reason":"tool_use","stop_sequence":null,
            "content":[{"type":"text","text":"I'll inspect the requested status."},
                {"type":"tool_use","id":"toolu_synthetic01","name":"read_status",
                    "input":{"id":"123"}}],
            "usage":{"input_tokens":50,"output_tokens":30,
                "cache_creation_input_tokens":0,"cache_read_input_tokens":0}
        });
        let ProviderTurn::ToolProposal(call) =
            parse_provider_turn("anthropic", &anthropic, &allowed()).unwrap()
        else {
            panic!("actual Messages shape must produce proposal")
        };
        let continued = continuation_payload(
            "anthropic",
            "claude-sonnet-4-5",
            &[
                json!({"role":"system","content":"No automatic tool execution"}),
                json!({"role":"user","content":"Check synthetic status"}),
            ],
            &generic_tool(),
            &call,
            &json!({"ok":true}),
        )
        .unwrap();
        assert_eq!(continued["system"], "No automatic tool execution");
        assert_eq!(continued["tool_choice"]["type"], "none");
        assert_eq!(continued["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(
            continued["messages"][2]["content"][0]["type"],
            "tool_result"
        );
    }

    #[test]
    fn rejects_nested_tool_calls_in_json_arguments_and_verified_results() {
        let nested = json!({"request":{"items":[{"tool_calls":[{"id":"call_nested"}]}]}});
        let openai = openai_call("read_status", &nested.to_string());
        assert!(parse_provider_turn("openai", &openai, &allowed()).is_err());
        assert!(parse_provider_turn(
            "anthropic",
            &anthropic_call("read_status", nested),
            &allowed()
        )
        .is_err());
        let disguised = json!({"data":{"type":"tool_use","id":"other","name":"unsafe"}});
        assert!(parse_provider_turn(
            "anthropic",
            &anthropic_call("read_status", disguised),
            &allowed()
        )
        .is_err());
        let proposal = ModelToolCall {
            id: "call_test".into(),
            name: "read_status".into(),
            arguments: json!({"id":"1"}),
        };
        let history = vec![json!({"role":"user","content":"Check"})];
        assert!(continuation_payload(
            "openai",
            "gpt-4.1",
            &history,
            &generic_tool(),
            &proposal,
            &json!({"status":{"tool_result":{"id":"unexpected"}}})
        )
        .is_err());
        let mutated = ModelToolCall {
            arguments: json!({
                "another":{"function_call":{"name":"other","arguments":{}}}
            }),
            ..proposal.clone()
        };
        assert!(continuation_payload(
            "openai",
            "gpt-4.1",
            &history,
            &generic_tool(),
            &mutated,
            &json!({"ok":true})
        )
        .is_err());
    }

    #[test]
    fn oversized_results_arguments_and_credential_aliases_fail_closed() {
        let call = ModelToolCall {
            id: "call_test".into(),
            name: "read_status".into(),
            arguments: json!({}),
        };
        let history = vec![json!({"role":"user","content":"Check"})];
        let tools = generic_tool();
        // JSON encoding, not character count, governs the maximum result.
        let exact = json!({"data":"a".repeat(MAX_RESULT - 11)});
        assert_eq!(serde_json::to_vec(&exact).unwrap().len(), MAX_RESULT);
        assert!(continuation_payload("openai", "gpt-4.1", &history, &tools, &call, &exact).is_ok());
        let over = json!({"data":"a".repeat(MAX_RESULT - 10)});
        assert!(continuation_payload("openai", "gpt-4.1", &history, &tools, &call, &over).is_err());
        for alias in [
            json!({"responseHeaders":{"X-API-Key":"SYNTHETIC"}}),
            json!({"server":{"authToken":"SYNTHETIC"}}),
            json!({"session":{"refresh_token":"SYNTHETIC"}}),
            json!({"text":"Authorization: Bearer SYNTHETIC"}),
        ] {
            assert!(continuation_payload(
                "anthropic",
                "claude-sonnet-4-5",
                &history,
                &tools,
                &call,
                &alias
            )
            .is_err());
        }
        let mut chat = openai_call("read_status", "{}");
        chat["choices"][0]["message"]["content"] = json!("Authorization: Bearer SYNTHETIC");
        assert!(parse_provider_turn("openai", &chat, &allowed()).is_err());
        let text = json!({"choices":[{"index":0,"finish_reason":"stop",
            "message":{"role":"assistant","content":"Authorization: Bearer SYNTHETIC"}}]});
        assert!(parse_provider_turn("openai", &text, &allowed()).is_err());
        assert!(parse_provider_turn(
            "anthropic",
            &json!({
                "type":"message","role":"assistant","stop_reason":"end_turn",
                "content":[{"type":"text","text":"sk-SYNTHETIC"}]
            }),
            &allowed()
        )
        .is_err());
    }

    #[test]
    fn bad_provider_envelope_metadata_and_empty_messages_are_rejected() {
        let mut chat = openai_call("read_status", "{}");
        for (key, invalid) in [
            ("object", json!("chat.completion.chunk")),
            ("id", json!(null)),
            ("created", json!("not-a-timestamp")),
            ("error", json!({"message":"synthetic error"})),
        ] {
            chat[key] = invalid;
            assert!(parse_provider_turn("openai", &chat, &allowed()).is_err());
            chat.as_object_mut().unwrap().remove(key);
        }
        let mut anthropic = anthropic_call("read_status", json!({}));
        anthropic["error"] = json!({"type":"error","message":"bad"});
        assert!(parse_provider_turn("anthropic", &anthropic, &allowed()).is_err());
        let empty_chat = json!({"choices":[{"index":0,"finish_reason":"stop",
            "message":{"role":"assistant","content":"   "}}]});
        assert!(parse_provider_turn("openrouter", &empty_chat, &allowed()).is_err());
        let empty_anthropic = json!({"type":"message","role":"assistant",
            "stop_reason":"end_turn","content":[{"type":"text","text":""}]});
        assert!(parse_provider_turn("anthropic", &empty_anthropic, &allowed()).is_err());
    }

    #[test]
    fn valid_json_but_invalid_tool_schema_stays_a_proposal_not_authorization() {
        // Schema matching belongs to the host's validated MCP tools/list
        // definition, both before display and again immediately before call.
        // This pure module cannot authorize an arbitrary syntactically valid
        // object. The host's schema tests enforce the fail-closed boundary.
        let missing_required_field = openai_call("read_status", "{\"wrong\":1}");
        assert!(matches!(
            parse_provider_turn("openai", &missing_required_field, &allowed()),
            Ok(ProviderTurn::ToolProposal(_))
        ));
    }
}
