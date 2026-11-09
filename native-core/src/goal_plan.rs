//! Parse REAL provider-generated Goal plan text as a validated suggestion only.
//! This module never calls a provider, writes a file, creates Goal, executes
//! a tool or treats model output as user approval. Prime displays and confirms
//! the returned shape before mobile_goal::GoalSpec deserialization/creation.
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserializer;
use serde_json::{Map, Number, Value};
use std::fmt;

pub const MAX_DRAFT_BYTES: usize = 16 * 1024;
const MAX_JSON_DEPTH: usize = 3;
const MAX_JSON_NODES: usize = 96;
const MAX_ARRAY_ITEMS: usize = 16;
const MAX_MAP_FIELDS: usize = 8;

/// Unlike serde_json::Value's default object parser, this rejects duplicated
/// keys at *every* depth before a duplicate can overwrite the earlier value.
struct Seed<'a> {
    depth: usize,
    nodes: &'a mut usize,
}
struct Checked<'a> {
    depth: usize,
    nodes: &'a mut usize,
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, de: D) -> Result<Value, D::Error> {
        *self.nodes += 1;
        if self.depth > MAX_JSON_DEPTH || *self.nodes > MAX_JSON_NODES {
            return Err(de::Error::custom(
                "Goal JSON nesting or node limit exceeded",
            ));
        }
        de.deserialize_any(Checked {
            depth: self.depth,
            nodes: self.nodes,
        })
    }
}
impl<'de> Visitor<'de> for Checked<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("bounded JSON with distinct keys")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Number::from(v).into())
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Number::from(v).into())
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("Non-finite Goal number"))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }
    fn visit_map<M: MapAccess<'de>>(self, mut seq: M) -> Result<Value, M::Error> {
        let mut values = Map::new();
        while let Some(k) = seq.next_key::<String>()? {
            if values.contains_key(&k) {
                return Err(de::Error::custom("Duplicate JSON key"));
            }
            if values.len() >= MAX_MAP_FIELDS {
                return Err(de::Error::custom("Too many Goal object fields"));
            }
            let v = seq.next_value_seed(Seed {
                depth: self.depth + 1,
                nodes: &mut *self.nodes,
            })?;
            values.insert(k, v);
        }
        Ok(Value::Object(values))
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Value, S::Error> {
        let mut values = Vec::new();
        while let Some(v) = seq.next_element_seed(Seed {
            depth: self.depth + 1,
            nodes: &mut *self.nodes,
        })? {
            if values.len() >= MAX_ARRAY_ITEMS {
                return Err(de::Error::custom("Too many Goal array items"));
            }
            values.push(v);
        }
        Ok(Value::Array(values))
    }
}

fn unsafe_scalar(ch: char) -> bool {
    let u = ch as u32;
    ch.is_control()
        || (ch.is_whitespace() && ch != ' ')
        || matches!(u, 0xad | 0x34f | 0x61c | 0x115f..=0x1160
            | 0x180e | 0x200b..=0x200f | 0x202a..=0x202e
            | 0x2060..=0x206f | 0xfe00..=0xfe0f | 0xfeff
            | 0xfff9..=0xfffb | 0xe0000..=0xe007f)
        || (0xfdd0..=0xfdef).contains(&u)
        || (u & 0xfffe) == 0xfffe
}

fn credential_marker(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "bearer ",
        "authorization:",
        "api_key",
        "api-key",
        "apikey",
        "x-api-key",
        "token=",
        "token:",
        "access_token",
        "refresh_token",
        "password=",
        "password:",
        "client_secret",
        "secret=",
        "secret:",
        "-----begin private key",
        "github_pat_",
        "ghp_",
        "gho_",
        "sk-ant-",
        "sk-proj-",
        "xoxb-",
        "xoxp-",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
        || lower
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .any(|piece| piece.starts_with("sk-") && piece.len() >= 16)
        || s.split(|c: char| !c.is_ascii_alphanumeric()).any(|piece| {
            (piece.starts_with("AKIA") && piece.len() >= 16)
                || (piece.starts_with("AIza") && piece.len() >= 30)
        })
}

fn fake_tool_call(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "tool_calls",
        "tool_call",
        "tool_use",
        "function_call",
        "tools/call",
        "call_tool",
        "invoke_tool",
        "execute_tool",
        "mcp__",
        "to=functions",
        "assistant to=",
        "<tool",
        "<function",
        "<|im_start|>",
        "[im_start]",
        "role:system",
        "system prompt:",
        "\"role\"",
    ];
    // Descriptions are user-facing prose, not a nested tool payload.
    s.contains(['{', '}'])
        || s.contains("\u{60}\u{60}\u{60}")
        || MARKERS.iter().any(|m| lower.contains(m))
}

fn valid_text(s: &str, bytes: usize) -> bool {
    !s.is_empty()
        && s.len() <= bytes
        && s.trim() == s
        && !s.chars().any(unsafe_scalar)
        && !credential_marker(s)
        && !fake_tool_call(s)
}

fn exact_fields(o: &Map<String, Value>, names: &[&str]) -> bool {
    o.len() == names.len() && names.iter().all(|n| o.contains_key(*n))
}

/// Validate exactly one provider text JSON object. Result is NOT an approved
/// Goal and must be shown to user for review before any Goal creation.
pub fn parse_draft(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() || raw.len() > MAX_DRAFT_BYTES {
        return Err("Goal draft must be nonempty UTF-8 <=16 KiB".into());
    }
    let trimmed = raw.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return Err("Goal output must be one JSON object, no fences or prose".into());
    }
    let mut nodes = 0;
    let mut decoder = serde_json::Deserializer::from_str(raw);
    let parsed = Seed {
        depth: 0,
        nodes: &mut nodes,
    }
    .deserialize(&mut decoder)
    .map_err(|e| format!("Invalid Goal JSON: {e}"))?;
    decoder
        .end()
        .map_err(|_| "Goal JSON has trailing content")?;
    let root = parsed.as_object().ok_or("Goal output must be an object")?;
    if !exact_fields(
        root,
        &["title", "steps", "max_turns", "max_retries_per_step"],
    ) {
        return Err("Goal draft has unknown or missing top-level fields".into());
    }
    let title = root["title"].as_str().ok_or("Goal title must be text")?;
    if !valid_text(title, 200) {
        return Err("Unsafe or oversized Goal title".into());
    }
    let steps = root["steps"]
        .as_array()
        .ok_or("Goal steps must be an array")?;
    if steps.is_empty() || steps.len() > 16 {
        return Err("Goal needs 1..16 steps".into());
    }
    for (index, item) in steps.iter().enumerate() {
        let o = item.as_object().ok_or("Goal step must be an object")?;
        if !exact_fields(o, &["description", "requires_approval"]) {
            return Err(format!(
                "Goal step {} has unknown/missing fields",
                index + 1
            ));
        }
        if !o["description"]
            .as_str()
            .is_some_and(|s| valid_text(s, 240))
        {
            return Err(format!("Goal step {} description is invalid", index + 1));
        }
        if !o["requires_approval"].is_boolean() {
            return Err(format!(
                "Goal step {} requires_approval must be boolean",
                index + 1
            ));
        }
    }
    let turns = root["max_turns"]
        .as_u64()
        .ok_or("max_turns must be unsigned integer")?;
    if turns < steps.len() as u64 || turns > 64 {
        return Err("max_turns must be >= steps and <=64".into());
    }
    let retries = root["max_retries_per_step"]
        .as_u64()
        .ok_or("max_retries_per_step must be unsigned integer")?;
    if retries > 5 {
        return Err("max_retries_per_step must be <=5".into());
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    fn valid() -> Value {
        json!({
            "title": "Build a safe offline Android prototype",
            "steps": [
                {"description":"Review the proposed design with the user",
                 "requires_approval":true},
                {"description":"Write a bounded development checklist",
                 "requires_approval":false}
            ],
            "max_turns": 6,
            "max_retries_per_step": 1
        })
    }

    fn good_raw() -> String {
        valid().to_string()
    }
    fn draft_from(step: Value) -> String {
        let mut item = valid();
        item["steps"] = json!([step]);
        item.to_string()
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    struct CompatibleStep {
        description: String,
        requires_approval: bool,
    }
    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    struct CompatibleGoalSpec {
        title: String,
        steps: Vec<CompatibleStep>,
        max_turns: u16,
        max_retries_per_step: u8,
    }

    #[test]
    fn accepts_realistic_extracted_openai_plain_text_goal() {
        let openai_text = r#"{
          "title": "Design a small Android dashboard",
          "steps": [
            {"description": "Agree on scope with the user", "requires_approval": true},
            {"description": "Implement the chosen layout", "requires_approval": false},
            {"description": "Review test results with the user", "requires_approval": true}
          ],
          "max_turns": 8,
          "max_retries_per_step": 2
        }"#;
        let result = parse_draft(openai_text).unwrap();
        let decoded: CompatibleGoalSpec = serde_json::from_value(result.clone()).unwrap();
        assert_eq!(decoded.steps.len(), 3);
        assert!(decoded.steps[0].requires_approval);
        assert_eq!(decoded.max_turns, 8);
        assert_eq!(result["title"], "Design a small Android dashboard");
        assert_eq!(result.as_object().unwrap().len(), 4);
        // Only the draft is returned: this has not approved or created Goal.
    }

    #[test]
    fn accepts_realistic_extracted_anthropic_text_and_unicode() {
        let anthropic_text = r#"{"title":"Rancang antara muka Android 🌿","steps":[{"description":"Semak keperluan pengguna","requires_approval":true},{"description":"Rancang ujian fungsi","requires_approval":false}],"max_turns":2,"max_retries_per_step":0}"#;
        let result = parse_draft(anthropic_text).unwrap();
        let typed: CompatibleGoalSpec = serde_json::from_value(result).unwrap();
        assert_eq!(typed.title, "Rancang antara muka Android 🌿");
        assert_eq!(typed.steps.len(), 2);
        assert_eq!(typed.max_retries_per_step, 0);
        assert_eq!(
            parse_draft(&format!(" \n{anthropic_text}\r\n"))
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn refuses_provider_envelopes_markdown_prose_prefix_suffix_and_multiple_json() {
        let raw = good_raw();
        let cases = [
            format!("Here is your plan:\n{raw}"),
            format!("{raw}\nThe plan is complete."),
            format!("\u{60}\u{60}\u{60}json\n{raw}\n\u{60}\u{60}\u{60}"),
            format!("{raw}{raw}"),
            format!("{raw} true"),
            format!("{{}} {raw}"),
            format!("[{raw}]"),
            json!({"choices":[{"message":{"content":raw}}]}).to_string(),
            json!({"content":[{"type":"text","text":raw}]}).to_string(),
            "null".to_owned(),
            "true".to_owned(),
            "\"quoted response\"".to_owned(),
            String::new(),
            "  \r\n".to_owned(),
        ];
        for item in cases {
            assert!(parse_draft(&item).is_err(), "accepted: {item}");
        }
        assert!(parse_draft(&good_raw()).is_ok());
    }

    #[test]
    fn rejects_duplicate_json_keys_at_root_step_and_unknown_nested_object() {
        let duplicate_top = r#"{"title":"A","title":"B","steps":[{"description":"Plan","requires_approval":true}],"max_turns":1,"max_retries_per_step":0}"#;
        let duplicate_step = r#"{"title":"A","steps":[{"description":"Plan","description":"Overwrite","requires_approval":true}],"max_turns":1,"max_retries_per_step":0}"#;
        let duplicate_bool = r#"{"title":"A","steps":[{"description":"Plan","requires_approval":true,"requires_approval":false}],"max_turns":1,"max_retries_per_step":0}"#;
        let duplicate_unknown = r#"{"title":"A","steps":[{"description":"Plan","requires_approval":true,"unknown":{"id":1,"id":2}}],"max_turns":1,"max_retries_per_step":0}"#;
        let duplicate_escape = r#"{"title":"A","tit\u006ce":"B","steps":[{"description":"Plan","requires_approval":true}],"max_turns":1,"max_retries_per_step":0}"#;
        for text in [
            duplicate_top,
            duplicate_step,
            duplicate_bool,
            duplicate_unknown,
            duplicate_escape,
        ] {
            assert!(parse_draft(text).is_err(), "duplicate accepted: {text}");
        }
        // Ensure nested repeated unknown keys do not get silently overwritten
        // before the overall draft is rejected.
        let unknown_nested = r#"{"title":"A","steps":[{"description":"Plan","requires_approval":true,"tool":{"a":1,"a":2}}],"max_turns":1,"max_retries_per_step":0}"#;
        assert!(parse_draft(unknown_nested).is_err());
    }

    #[test]
    fn rejects_missing_extra_fields_wrong_types_and_unknown_nested_payload() {
        let mut item = valid();
        item.as_object_mut().unwrap().remove("max_turns");
        assert!(parse_draft(&item.to_string()).is_err());
        let mut item = valid();
        item["extra"] = json!("not allowed");
        assert!(parse_draft(&item.to_string()).is_err());
        let mut item = valid();
        item["steps"][0]["extra"] = json!("not allowed");
        assert!(parse_draft(&item.to_string()).is_err());
        for bad in [
            json!({"description":"Review", "requires_approval":"true"}),
            json!({"description":"Review", "requires_approval":1}),
            json!({"description":null,"requires_approval":true}),
            json!({"description":"Review"}),
            json!({"description":"Review","requires_approval":true,"payload":{"tool_calls":[]}}),
            json!(null),
            json!("Review"),
        ] {
            assert!(parse_draft(&draft_from(bad)).is_err());
        }
        for bad in [
            json!(null),
            json!(3),
            json!(false),
            json!({"nested":"payload"}),
        ] {
            let mut draft = valid();
            draft["title"] = bad;
            assert!(parse_draft(&draft.to_string()).is_err());
        }
    }

    #[test]
    fn rejects_control_bidi_invisible_noncharacters_and_unicode_byte_overflow() {
        for unsafe_text in [
            "Hidden\ncommand",
            "Control\tcharacter",
            "Bad\rline",
            "a\u{0000}b",
            "a\u{200b}b",
            "a\u{200e}b",
            "a\u{202e}b",
            "a\u{2067}b",
            "a\u{feff}b",
            "a\u{fdd0}b",
            "a\u{fffe}b",
            "a\u{e0001}b",
            " Leading whitespace",
            "Trailing whitespace ",
            "   ",
        ] {
            let mut draft = valid();
            draft["title"] = json!(unsafe_text);
            assert!(
                parse_draft(&draft.to_string()).is_err(),
                "unsafe title: {unsafe_text:?}"
            );
            let mut draft = valid();
            draft["steps"][0]["description"] = json!(unsafe_text);
            assert!(
                parse_draft(&draft.to_string()).is_err(),
                "unsafe step: {unsafe_text:?}"
            );
        }
        let mut draft = valid();
        draft["title"] = json!("🌿".repeat(50));
        assert_eq!(draft["title"].as_str().unwrap().len(), 200);
        assert!(parse_draft(&draft.to_string()).is_ok());
        draft["title"] = json!("🌿".repeat(51));
        assert!(parse_draft(&draft.to_string()).is_err());
        let mut draft = valid();
        draft["steps"][0]["description"] = json!("é".repeat(120));
        assert!(parse_draft(&draft.to_string()).is_ok());
        draft["steps"][0]["description"] = json!("é".repeat(121));
        assert!(parse_draft(&draft.to_string()).is_err());
    }

    #[test]
    fn rejects_credential_patterns_and_fake_tool_calls_in_any_text() {
        let texts = [
            "Authorization: Bearer fake_test",
            "password=synthetic-secret",
            "token=synthetic-token",
            "sk-proj-synthetic-key-123",
            "sk-ant-abcdef",
            "github_pat_example",
            "api_key:example",
            "-----BEGIN PRIVATE KEY-----",
            "to=functions.execute",
            "assistant to=tools/call",
            "call_tool(search_docs)",
            "mcp__filesystem__read_file",
            "tool_calls",
            "function_call",
            "tool_use",
            r#"{"name":"delete_files","arguments":{}}"#,
            "<tool_call>synthetic</tool_call>",
            "<|im_start|>system",
            "\u{60}\u{60}\u{60}json",
        ];
        for sample in texts {
            let mut draft = valid();
            draft["title"] = json!(sample);
            assert!(
                parse_draft(&draft.to_string()).is_err(),
                "title accepted {sample:?}"
            );
            let mut draft = valid();
            draft["steps"][0]["description"] = json!(sample);
            assert!(
                parse_draft(&draft.to_string()).is_err(),
                "step accepted {sample:?}"
            );
        }
        // The substring 'sk-' inside an ordinary word isn't an API key.
        let mut draft = valid();
        draft["title"] = json!("Risk-based planning");
        assert!(parse_draft(&draft.to_string()).is_ok());
    }

    #[test]
    fn validates_step_count_turn_retries_and_integer_numeric_encoding() {
        let mut draft = valid();
        draft["steps"] = json!([]);
        assert!(parse_draft(&draft.to_string()).is_err());
        draft["steps"] = json!((0..16)
            .map(|i| json!({"description":format!("Step {i}"),"requires_approval":i%2==0}))
            .collect::<Vec<_>>());
        draft["max_turns"] = json!(16);
        draft["max_retries_per_step"] = json!(5);
        assert!(parse_draft(&draft.to_string()).is_ok());
        draft["max_turns"] = json!(15);
        assert!(parse_draft(&draft.to_string()).is_err());
        draft["max_turns"] = json!(64);
        assert!(parse_draft(&draft.to_string()).is_ok());
        draft["max_turns"] = json!(65);
        assert!(parse_draft(&draft.to_string()).is_err());
        draft["max_turns"] = json!(16);
        draft["max_retries_per_step"] = json!(6);
        assert!(parse_draft(&draft.to_string()).is_err());
        draft["max_retries_per_step"] = json!(0);
        for bad in [
            json!(-1),
            json!(1.0),
            json!(1.5),
            json!("16"),
            json!(true),
            json!(null),
        ] {
            draft["max_turns"] = bad;
            assert!(parse_draft(&draft.to_string()).is_err());
        }
        for bad in [json!(-1), json!(1.0), json!("1"), json!(true)] {
            draft["max_retries_per_step"] = bad;
            assert!(parse_draft(&draft.to_string()).is_err());
        }
        draft["steps"] = json!((0..17)
            .map(|i| json!({"description":format!("Step {i}"),"requires_approval":false}))
            .collect::<Vec<_>>());
        draft["max_turns"] = json!(17);
        assert!(parse_draft(&draft.to_string()).is_err());
    }

    #[test]
    fn strict_json_depth_size_and_syntactic_injections_fail_closed() {
        let mut draft = valid();
        draft["steps"][0]["description"] = json!("x".repeat(240));
        assert!(parse_draft(&draft.to_string()).is_ok());
        draft["steps"][0]["description"] = json!("x".repeat(241));
        assert!(parse_draft(&draft.to_string()).is_err());
        assert!(parse_draft(&format!("{}{}", good_raw(), " ".repeat(MAX_DRAFT_BYTES))).is_err());
        let deep_unknown = r#"{"title":"Goal","steps":[{"description":"Plan","requires_approval":true,"unexpected":{"a":{"b":{"c":1}}}}],"max_turns":1,"max_retries_per_step":0}"#;
        assert!(parse_draft(deep_unknown).is_err());
        for malformed in [
            r#"{"title":"Wrong", "steps": [{"description":"Bad","requires_approval":true}],"max_turns":1,"max_retries_per_step":0,}"#,
            r#"{"title":"Wrong", // comment
            "steps":[]}"#,
            r#"{"title":"Wrong", "steps": []"#,
            r#"{"title":"Wrong","steps":[]} extra"#,
        ] {
            assert!(parse_draft(malformed).is_err());
        }
    }
}
