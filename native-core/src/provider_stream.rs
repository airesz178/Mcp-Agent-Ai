//! Incremental bounded SSE parser for text-only model responses.
//! No network I/O, tool dispatch, credential handling or timers here.
//! The calling HTTP client owns the request timeout and cancellation.
use serde_json::Value;

const MAX_EVENT: usize = 64 * 1024;
const MAX_TOTAL: usize = 1024 * 1024;
const MAX_ANSWER: usize = 20_000;

#[derive(Default)]
struct AnthropicState {
    started: bool,
    active_block: Option<u64>,
    active_thinking: bool,
    next_block: u64,
    ended: bool,
}

pub(super) struct Collector {
    provider: String,
    pending: Vec<u8>,
    total: usize,
    answer: String,
    done: bool,
    openai_stopped: bool,
    usage_seen: bool,
    anthropic: AnthropicState,
    first_frame: bool,
}

impl Collector {
    pub(super) fn new(provider: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            pending: Vec::new(),
            total: 0,
            answer: String::new(),
            done: false,
            openai_stopped: false,
            usage_seen: false,
            anthropic: AnthropicState::default(),
            first_frame: true,
        }
    }

    pub(super) fn feed(
        &mut self,
        chunk: &[u8],
        mut on_delta: impl FnMut(&str),
    ) -> Result<(), String> {
        if !matches!(
            self.provider.as_str(),
            "openai" | "openrouter" | "anthropic"
        ) {
            return Err("Unsupported provider stream".into());
        }
        if self.done && !chunk.is_empty() {
            return Err("Unexpected data after provider stream finished".into());
        }
        self.total = self
            .total
            .checked_add(chunk.len())
            .ok_or("Provider stream too large")?;
        if self.total > MAX_TOTAL {
            return Err("Provider stream too large".into());
        }
        self.pending.extend_from_slice(chunk);
        while let Some((offset, delimiter)) = frame_boundary(&self.pending) {
            if offset > MAX_EVENT {
                return Err("Provider stream event too large".into());
            }
            let frame = self.pending[..offset].to_vec();
            self.pending.drain(..offset + delimiter);
            let decoded =
                std::str::from_utf8(&frame).map_err(|_| "Invalid provider stream encoding")?;
            // A UTF-8 BOM is permitted only at the start of an SSE stream.
            let decoded = if self.first_frame {
                decoded.strip_prefix('\u{feff}').unwrap_or(decoded)
            } else {
                decoded
            };
            self.first_frame = false;
            let (event_name, data) = parse_sse_frame(decoded)?;
            if let Some(data) = data {
                if data == "[DONE]" {
                    if self.provider == "anthropic"
                        || !self.openai_stopped
                        || event_name.as_deref().is_some_and(|name| name != "message")
                    {
                        return Err("Unexpected provider completion marker".into());
                    }
                    self.done = true;
                } else {
                    let payload: Value =
                        serde_json::from_str(&data).map_err(|_| "Invalid provider stream event")?;
                    if !payload.is_object() {
                        return Err("Provider stream event must be a JSON object".into());
                    }
                    if payload.get("error").is_some_and(|error| !error.is_null())
                        || payload["type"] == "error"
                        || event_name.as_deref() == Some("error")
                    {
                        return Err("Provider reported a stream error".into());
                    }
                    if self.provider == "anthropic" {
                        self.anthropic_event(event_name.as_deref(), &payload, &mut on_delta)?;
                    } else {
                        self.chat_event(event_name.as_deref(), &payload, &mut on_delta)?;
                    }
                }
            } else if event_name.as_deref().is_some_and(|name| name != "ping") {
                // Comments/metadata-only keepalive events are allowed. An event
                // with a declared non-ping type needs a JSON data payload.
                return Err("Provider SSE event is missing data".into());
            }
            if self.done {
                if !self.pending.is_empty()
                    && !self.pending.iter().all(|byte| byte.is_ascii_whitespace())
                {
                    return Err("Unexpected data after provider stream finished".into());
                }
                self.pending.clear();
                break;
            }
        }
        if self.pending.len() > MAX_EVENT {
            return Err("Provider stream event too large".into());
        }
        Ok(())
    }

    fn append(&mut self, delta: &str, on_delta: &mut impl FnMut(&str)) -> Result<(), String> {
        if self.answer.len().saturating_add(delta.len()) > MAX_ANSWER {
            return Err("Provider answer exceeds safety limit".into());
        }
        self.answer.push_str(delta);
        if !delta.is_empty() {
            on_delta(delta);
        }
        Ok(())
    }

    fn chat_event(
        &mut self,
        event_name: Option<&str>,
        event: &Value,
        on_delta: &mut impl FnMut(&str),
    ) -> Result<(), String> {
        if event_name.is_some_and(|name| name != "message")
            || event
                .get("object")
                .is_some_and(|kind| kind != "chat.completion.chunk")
        {
            return Err("Unsupported chat-completions SSE event type".into());
        }
        let choices = event["choices"]
            .as_array()
            .ok_or("Provider stream choices missing")?;
        if choices.is_empty() {
            // Optional OpenAI stream_options.include_usage terminal chunk.
            if !self.openai_stopped || self.usage_seen || !event["usage"].is_object() {
                return Err("Unexpected empty provider choices".into());
            }
            self.usage_seen = true;
            return Ok(());
        }
        if choices.len() != 1 {
            return Err("Provider stream contains multiple choices".into());
        }
        if self.openai_stopped {
            return Err("Provider emitted more content after finish_reason".into());
        }
        let choice = &choices[0];
        if choice["index"].as_u64() != Some(0) {
            return Err("Invalid provider stream choice index".into());
        }
        let delta = choice["delta"]
            .as_object()
            .ok_or("Provider stream delta missing")?;
        if delta.get("role").is_some_and(|role| role != "assistant")
            || delta
                .get("tool_calls")
                .is_some_and(|calls| !calls.is_null())
            || delta
                .get("function_call")
                .is_some_and(|call| !call.is_null())
            || delta
                .get("refusal")
                .is_some_and(|refusal| !refusal.is_null())
            || delta.get("audio").is_some_and(|audio| !audio.is_null())
        {
            return Err("Unsupported provider stream role or non-text content".into());
        }
        let text = match delta.get("content") {
            None | Some(Value::Null) => "",
            Some(Value::String(content)) => content,
            _ => return Err("Provider stream content must be text".into()),
        };
        self.append(text, on_delta)?;
        match choice.get("finish_reason") {
            None | Some(Value::Null) => {}
            Some(Value::String(reason)) if reason == "stop" => {
                self.openai_stopped = true;
            }
            _ => return Err("Provider stream did not finish normally".into()),
        }
        Ok(())
    }

    fn anthropic_event(
        &mut self,
        event_name: Option<&str>,
        event: &Value,
        on_delta: &mut impl FnMut(&str),
    ) -> Result<(), String> {
        let kind = event["type"]
            .as_str()
            .ok_or("Anthropic stream event type missing")?;
        if event_name.is_some_and(|name| name != kind) {
            return Err("Anthropic SSE event name and JSON type mismatch".into());
        }
        let state = &self.anthropic;
        match kind {
            "ping" => Ok(()),
            "message_start" => {
                if state.started || state.ended || state.active_block.is_some() {
                    return Err("Duplicate or out-of-order Anthropic message_start".into());
                }
                let message = &event["message"];
                if message["type"] != "message"
                    || message["role"] != "assistant"
                    || !message["content"]
                        .as_array()
                        .is_some_and(|content| content.is_empty())
                    || message
                        .get("stop_reason")
                        .is_some_and(|reason| !reason.is_null())
                {
                    return Err("Invalid Anthropic message_start".into());
                }
                self.anthropic.started = true;
                Ok(())
            }
            "content_block_start" => {
                if !state.started || state.ended || state.active_block.is_some() {
                    return Err("Unexpected Anthropic content block start".into());
                }
                let index = event["index"]
                    .as_u64()
                    .ok_or("Anthropic block index missing")?;
                if index != state.next_block || index >= 64 {
                    return Err("Unexpected Anthropic block index".into());
                }
                let block = &event["content_block"];
                match block["type"].as_str() {
                    Some("text") => {
                        let initial = block["text"]
                            .as_str()
                            .ok_or("Anthropic text block has no text")?;
                        self.append(initial, on_delta)?;
                        self.anthropic.active_thinking = false;
                    }
                    Some("thinking") if block["thinking"].as_str() == Some("") => {
                        // Anthropic can stream hidden thinking before text.
                        // Validate its lifecycle but never expose, log or
                        // append thinking/signature bytes to the response.
                        self.anthropic.active_thinking = true;
                    }
                    Some("redacted_thinking") if block["data"].as_str().is_some() => {
                        self.anthropic.active_thinking = true;
                    }
                    _ => return Err("Unsupported Anthropic content block".into()),
                }
                self.anthropic.active_block = Some(index);
                Ok(())
            }
            "content_block_delta" => {
                let index = event["index"]
                    .as_u64()
                    .ok_or("Anthropic block index missing")?;
                if !state.started || state.ended || state.active_block != Some(index) {
                    return Err("Unexpected Anthropic content block delta".into());
                }
                if self.anthropic.active_thinking {
                    return match event["delta"]["type"].as_str() {
                        Some("thinking_delta") if event["delta"]["thinking"].as_str().is_some() => Ok(()),
                        Some("signature_delta") if event["delta"]["signature"].as_str().is_some() => Ok(()),
                        _ => Err("Unsupported Anthropic thinking block delta".into()),
                    };
                }
                if event["delta"]["type"] != "text_delta" {
                    return Err("Anthropic non-text delta is unsupported".into());
                }
                let text = event["delta"]["text"]
                    .as_str()
                    .ok_or("Anthropic text delta is missing text")?;
                self.append(text, on_delta)
            }
            "content_block_stop" => {
                let index = event["index"]
                    .as_u64()
                    .ok_or("Anthropic block index missing")?;
                if !state.started || state.ended || state.active_block != Some(index) {
                    return Err("Unexpected Anthropic content block stop".into());
                }
                self.anthropic.active_block = None;
                self.anthropic.active_thinking = false;
                self.anthropic.next_block += 1;
                Ok(())
            }
            "message_delta" => {
                if !state.started || state.ended || state.active_block.is_some() {
                    return Err("Unexpected Anthropic message_delta".into());
                }
                if event["delta"]["stop_reason"] != "end_turn"
                    || event["delta"]
                        .get("stop_sequence")
                        .is_some_and(|sequence| !sequence.is_null())
                {
                    return Err("Anthropic stream stopped without end_turn".into());
                }
                self.anthropic.ended = true;
                Ok(())
            }
            "message_stop" => {
                if !state.started || !state.ended || state.active_block.is_some() {
                    return Err("Anthropic message_stop without completed end_turn".into());
                }
                self.done = true;
                Ok(())
            }
            _ => Err("Unsupported Anthropic SSE event type".into()),
        }
    }

    pub(super) fn finish(self) -> Result<String, String> {
        if !self.done {
            return Err("Provider stream ended without completion".into());
        }
        if self.answer.trim().is_empty() {
            return Err("Provider returned no usable text".into());
        }
        Ok(self.answer)
    }
}

/// Extract only standard SSE fields. JSON is parsed only once an entire frame
/// including its blank-line delimiter has arrived, so UTF-8 can span chunks.
fn parse_sse_frame(frame: &str) -> Result<(Option<String>, Option<String>), String> {
    let mut name = None;
    let mut data = Vec::new();
    for line in frame.split(['\r', '\n']).filter(|line| !line.is_empty()) {
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => {
                if name.replace(value.to_owned()).is_some() || value.is_empty() {
                    return Err("Invalid duplicate SSE event name".into());
                }
            }
            "data" => data.push(value),
            "id" if !value.contains('\0') => {}
            "retry" if value.bytes().all(|b| b.is_ascii_digit()) => {}
            _ => return Err("Invalid provider SSE field".into()),
        }
    }
    Ok((name, (!data.is_empty()).then(|| data.join("\n"))))
}

/// Standard LF and CRLF frame delimiters, plus CR-only line endings.
/// Newlines may be split across network chunks: no UTF-8 decoding until a
/// complete blank-line-delimited SSE frame is available.
fn frame_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < bytes.len() {
        let first = match bytes[i] {
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => i + 2,
            b'\r' | b'\n' => i + 1,
            _ => {
                i += 1;
                continue;
            }
        };
        if first < bytes.len() {
            let second = match bytes[first] {
                b'\r' if bytes.get(first + 1) == Some(&b'\n') => first + 2,
                b'\r' | b'\n' => first + 1,
                _ => {
                    i = first;
                    continue;
                }
            };
            return Some((i, second - i));
        }
        i = first;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chat_delta(text: &str) -> Vec<u8> {
        format!(
            "data: {}\n\n",
            json!({"object":"chat.completion.chunk","choices":[
                {"index":0,"delta":{"content":text},"finish_reason":null}
            ]})
        )
        .into_bytes()
    }

    fn chat_end() -> &'static [u8] {
        b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
    }

    fn anthropic_ok() -> Vec<u8> {
        [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"stop_reason\":null}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ].concat().into_bytes()
    }

    #[test]
    fn utf8_fragmented_by_every_byte_and_crlf_split() {
        let mut stream = Collector::new("openai");
        let mut observed = String::new();
        let mut bytes = chat_delta("Hi 🌿漢🙂");
        bytes.extend_from_slice(chat_end());
        // Feed one byte at a time: this splits multi-byte UTF-8, JSON, SSE
        // blank lines and completion markers at all possible boundaries.
        for b in bytes {
            stream.feed(&[b], |delta| observed.push_str(delta)).unwrap();
        }
        assert_eq!(observed, "Hi 🌿漢🙂");
        assert_eq!(stream.finish().unwrap(), observed);
        let mut stream = Collector::new("openrouter");
        stream.feed(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\r", |_| {}).unwrap();
        stream.feed(b"\n\r\n", |_| {}).unwrap();
        stream.feed(chat_end(), |_| {}).unwrap();
        assert_eq!(stream.finish().unwrap(), "Hi");
    }

    #[test]
    fn anthropic_real_lifecycle_and_message_stop() {
        let mut stream = Collector::new("anthropic");
        let mut deltas = Vec::new();
        for piece in anthropic_ok().chunks(7) {
            stream
                .feed(piece, |text| deltas.push(text.to_owned()))
                .unwrap();
        }
        assert_eq!(deltas, ["ok"]);
        assert_eq!(stream.finish().unwrap(), "ok");
    }

    #[test]
    fn anthropic_adaptive_thinking_is_never_exposed_as_answer_text() {
        let frames = [
            json!({"type":"message_start","message":{"type":"message","role":"assistant","content":[],"stop_reason":null}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"private reasoning"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque-signature"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Only the answer"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}),
            json!({"type":"message_stop"}),
        ];
        let mut stream = Collector::new("anthropic");
        let mut exposed = String::new();
        for frame in frames {
            let kind = frame["type"].as_str().unwrap();
            let wire = format!("event: {kind}\ndata: {frame}\n\n");
            for part in wire.as_bytes().chunks(9) {
                stream.feed(part, |delta| exposed.push_str(delta)).unwrap();
            }
        }
        assert_eq!(exposed, "Only the answer");
        assert_eq!(stream.finish().unwrap(), "Only the answer");
    }

    #[test]
    fn rejects_missing_end_invalid_json_and_overflow() {
        let mut stream = Collector::new("openrouter");
        stream.feed(&chat_delta("text"), |_| {}).unwrap();
        assert!(stream.finish().is_err());
        let mut bad = Collector::new("openai");
        assert!(bad.feed(b"data: not-json\n\n", |_| {}).is_err());
        let mut oversized = Collector::new("openai");
        assert!(oversized.feed(&vec![b'x'; MAX_EVENT + 1], |_| {}).is_err());
        let mut error = Collector::new("anthropic");
        assert!(error
            .feed(b"data: {\"type\":\"error\"}\n\n", |_| {})
            .is_err());
    }

    #[test]
    fn chat_requires_explicit_stop_before_done_and_rejects_extra() {
        let mut early = Collector::new("openai");
        assert!(early.feed(b"data: [DONE]\n\n", |_| {}).is_err());
        let mut early = Collector::new("openrouter");
        early.feed(&chat_delta("Hello"), |_| {}).unwrap();
        assert!(early.feed(b"data: [DONE]\n\n", |_| {}).is_err());
        let mut late = Collector::new("openai");
        late.feed(&chat_delta("Hello"), |_| {}).unwrap();
        late.feed(chat_end(), |_| {}).unwrap();
        assert!(late.feed(&chat_delta("unexpected"), |_| {}).is_err());
        let mut duplicate = Collector::new("openai");
        assert!(duplicate
            .feed(&[chat_end(), b"data: [DONE]\n\n"].concat(), |_| {})
            .is_err());
        let mut trailing = Collector::new("openai");
        assert!(trailing
            .feed(&[chat_end(), b"garbage"].concat(), |_| {})
            .is_err());
    }

    #[test]
    fn chat_rejects_multiple_choices_nontext_tools_finish_reasons() {
        let errors = [
            json!({"choices":[{"index":0,"delta":{"content":"yes"},"finish_reason":null},
                {"index":1,"delta":{"content":"no"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"id":"x"}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"content":[{"type":"text","text":"hi"}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"role":"tool","content":"x"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":"length"}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]}),
            json!({"choices":[{"index":1,"delta":{"content":"x"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"refusal":"no"},"finish_reason":null}]}),
            json!({"error":{"message":"fail"},"choices":[]}),
        ];
        for bad in errors {
            let mut stream = Collector::new("openai");
            let frame = format!("data: {bad}\n\n");
            assert!(stream.feed(frame.as_bytes(), |_| {}).is_err(), "{bad}");
        }
    }

    #[test]
    fn chat_usage_chunk_after_stop_is_allowed_before_done_only() {
        let mut stream = Collector::new("openrouter");
        stream.feed(&chat_delta("Hello"), |_| {}).unwrap();
        stream
            .feed(
                b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                |_| {},
            )
            .unwrap();
        stream
            .feed(
                b"data: {\"choices\":[],\"usage\":{\"total_tokens\":5}}\n\n",
                |_| {},
            )
            .unwrap();
        stream.feed(b"data: [DONE]\n\n", |_| {}).unwrap();
        assert_eq!(stream.finish().unwrap(), "Hello");
        let mut duplicate = Collector::new("openai");
        duplicate.feed(&chat_delta("ok"), |_| {}).unwrap();
        duplicate
            .feed(
                b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                |_| {},
            )
            .unwrap();
        duplicate
            .feed(
                b"data: {\"choices\":[],\"usage\":{\"total_tokens\":5}}\n\n",
                |_| {},
            )
            .unwrap();
        assert!(duplicate
            .feed(
                b"data: {\"choices\":[],\"usage\":{\"total_tokens\":5}}\n\n",
                |_| {}
            )
            .is_err());
    }

    #[test]
    fn anthropic_rejects_missing_lifecycle_and_wrong_stop_reason() {
        let events = [
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".as_slice(),
            b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n",
            b"data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\"role\":\"user\",\"content\":[]}}\n\n",
            b"data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\"}]}}\n\n",
            b"data: [DONE]\n\n",
        ];
        for input in events {
            let mut stream = Collector::new("anthropic");
            assert!(stream.feed(input, |_| {}).is_err());
        }
        let mut wrong_stop = anthropic_ok();
        let old = b"\"stop_reason\":\"end_turn\"";
        let found = wrong_stop
            .windows(old.len())
            .position(|chunk| chunk == old)
            .unwrap();
        wrong_stop.splice(
            found..found + old.len(),
            b"\"stop_reason\":\"max_tokens\"".iter().copied(),
        );
        assert!(Collector::new("anthropic")
            .feed(&wrong_stop, |_| {})
            .is_err());
        let mut mismatch = anthropic_ok();
        let old = b"event: content_block_delta";
        let found = mismatch
            .windows(old.len())
            .position(|chunk| chunk == old)
            .unwrap();
        mismatch.splice(
            found..found + old.len(),
            b"event: message_stop".iter().copied(),
        );
        assert!(Collector::new("anthropic").feed(&mismatch, |_| {}).is_err());
    }

    #[test]
    fn fragmented_utf8_in_anthropic_blocks_and_multiple_blocks() {
        let raw = [
            "data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"🌿\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"漢\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        ].concat();
        let mut stream = Collector::new("anthropic");
        for byte in raw.as_bytes() {
            stream.feed(&[*byte], |_| {}).unwrap();
        }
        assert_eq!(stream.finish().unwrap(), "🌿漢");
    }

    #[test]
    fn sse_rejects_malformed_fields_and_incomplete_events() {
        for input in [
            "junk: value\n\n",
            "event: message\nevent: message\ndata: {}\n\n",
            "event: error\ndata: {}\n\n",
            "data: {invalid}\n\n",
        ] {
            assert!(Collector::new("openai")
                .feed(input.as_bytes(), |_| {})
                .is_err());
        }
        let mut truncated = Collector::new("openai");
        truncated.feed(&chat_delta("hi"), |_| {}).unwrap();
        truncated.feed(b"data: [DONE]\n", |_| {}).unwrap();
        assert!(truncated.finish().is_err());
        let mut bad_utf8 = Collector::new("openai");
        assert!(bad_utf8.feed(b"data: \xff\n\n", |_| {}).is_err());
        let mut stream = Collector::new("openai");
        stream.feed(b": keepalive\n\n", |_| {}).unwrap();
        stream.feed(&[b"\xef\xbb\xbfdata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n".as_slice(), chat_end()].concat(), |_| {}).unwrap_err();
        // BOM is permitted only at the first frame.
    }

    #[test]
    fn allows_initial_bom_heartbeat_multiline_data_and_cr_only_frames() {
        let mut bom = Collector::new("openai");
        let mut received = String::new();
        let first = [
            b"\xef\xbb\xbfdata: ".as_slice(),
            json!({"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]})
                .to_string()
                .as_bytes(),
            b"\n\n",
        ]
        .concat();
        for part in first.chunks(2) {
            bom.feed(part, |text| received.push_str(text)).unwrap();
        }
        bom.feed(b": comment keepalive\n\n", |_| {}).unwrap();
        bom.feed(chat_end(), |_| {}).unwrap();
        assert_eq!(received, "ok");
        assert_eq!(bom.finish().unwrap(), "ok");

        // CR-only line separators are legal SSE. Split the data: JSON over
        // two data fields joined by a single newline, still valid JSON.
        let mut cr = Collector::new("openrouter");
        cr.feed(b"event: message\rdata: {\"choices\":\rdata: [{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\r\r", |_| {}).unwrap();
        cr.feed(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\r\r",
            |_| {},
        )
        .unwrap();
        cr.feed(b"data: [DONE]\r\r", |_| {}).unwrap();
        assert_eq!(cr.finish().unwrap(), "x");
    }

    #[test]
    fn broken_anthropic_block_indices_and_non_text_deltas_fail_closed() {
        let prefix = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        ].concat();
        for bad in [
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"wrong"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"secret"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":null}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"run"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        ] {
            let mut stream = Collector::new("anthropic");
            stream.feed(prefix.as_bytes(), |_| {}).unwrap();
            assert!(
                stream
                    .feed(format!("data: {bad}\n\n").as_bytes(), |_| {})
                    .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn enforce_event_total_answer_and_provider_limits() {
        let mut oversized = Collector::new("openai");
        assert!(oversized.feed(&vec![b'a'; MAX_TOTAL + 1], |_| {}).is_err());
        let mut limit = Collector::new("openai");
        for _ in 0..20 {
            limit.feed(&chat_delta(&"a".repeat(1000)), |_| {}).unwrap();
        }
        assert!(limit.feed(&chat_delta("b"), |_| {}).is_err());
        assert!(Collector::new("invalid")
            .feed(&chat_delta("x"), |_| {})
            .is_err());
    }
}
