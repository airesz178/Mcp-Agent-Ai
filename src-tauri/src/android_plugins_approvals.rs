//! A bounded, device-local approval broker for inbound ChatGPT Plugins calls.
//! A remote MCP argument never grants authority. A foreground Android UI
//! decision must match an outstanding opaque request ID, or a user-persisted
//! rule must match the exact tunnel, module, tool and target scope.
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const MAX_PENDING: usize = 4;
const MAX_RULES: usize = 48;
const MAX_RULE_BYTES: usize = 16 * 1024;
const MAX_WAIT: Duration = Duration::from_secs(22);
const RESPONSE_RESERVE: Duration = Duration::from_secs(6);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    id: String,
    tunnel_id: String,
    plugin_id: String,
    plugin_name: String,
    tool_name: String,
    scope: String,
}

impl Rule {
    fn matches(&self, other: &Self) -> bool {
        self.tunnel_id == other.tunnel_id
            && self.plugin_id == other.plugin_id
            && self.tool_name == other.tool_name
            && self.scope == other.scope
    }

    fn to_value(&self) -> Value {
        json!({"id":self.id,"tunnelId":self.tunnel_id,"pluginId":self.plugin_id,
            "pluginName":self.plugin_name,"toolName":self.tool_name,"scope":self.scope})
    }

    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        if object.len() != 6 { return None; }
        let read = |key: &str, max: usize| -> Option<String> {
            let s = object.get(key)?.as_str()?;
            if s.is_empty() || s.len() > max || s.chars().any(char::is_control) {
                None
            } else {
                Some(s.to_owned())
            }
        };
        let result = Self {
            id: read("id", 36)?,
            tunnel_id: read("tunnelId", 128)?,
            plugin_id: read("pluginId", 32)?,
            plugin_name: read("pluginName", 80)?,
            tool_name: read("toolName", 80)?,
            scope: read("scope", 256)?,
        };
        Uuid::parse_str(&result.id).ok()?;
        if !result.plugin_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || !result.tool_name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || !result.tunnel_id.starts_with("tunnel_")
        {
            return None;
        }
        Some(result)
    }
}

fn load_rules(path: &Path) -> Vec<Rule> {
    let Ok(raw) = fs::read_to_string(path) else { return Vec::new() };
    if raw.len() > MAX_RULE_BYTES { return Vec::new(); }
    let Ok(data) = serde_json::from_str::<Value>(&raw) else { return Vec::new() };
    if data.as_object().is_none_or(|o| o.len() != 2)
        || data["version"] != 1 { return Vec::new(); }
    let Some(items) = data["rules"].as_array() else { return Vec::new() };
    if items.len() > MAX_RULES { return Vec::new(); }
    let mut rules = Vec::new();
    for item in items {
        let Some(rule) = Rule::from_value(item) else { return Vec::new() };
        if rules.iter().any(|existing: &Rule| existing.id == rule.id || existing.matches(&rule)) {
            return Vec::new();
        }
        rules.push(rule);
    }
    rules
}

fn save_rules(path: &Path, rules: &[Rule]) -> Result<(), String> {
    if rules.len() > MAX_RULES { return Err("Always allow rule limit reached".into()); }
    let content = json!({"version":1,"rules":rules.iter().map(Rule::to_value).collect::<Vec<_>>()}).to_string();
    if content.len() > MAX_RULE_BYTES { return Err("Always allow rules exceed storage limit".into()); }
    let parent = path.parent().ok_or("Approvals storage path unavailable")?;
    fs::create_dir_all(parent).map_err(|_| "Approvals storage unavailable")?;
    let temp = parent.join(format!("approvals-{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<(), String> {
        let mut file = fs::OpenOptions::new().write(true).create_new(true)
            .open(&temp).map_err(|_| "Approvals storage unavailable")?;
        file.write_all(content.as_bytes()).map_err(|_| "Approvals storage unavailable")?;
        file.sync_all().map_err(|_| "Approvals storage unavailable")?;
        fs::rename(&temp, path).map_err(|_| "Approvals storage unavailable")?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&temp); }
    result
}

#[derive(Clone, Debug)]
pub(crate) struct ApprovalIntent {
    pub plugin_id: String,
    pub plugin_name: String,
    pub tool_name: String,
    /// These strings contain no untrusted raw page text, form data or secrets.
    pub summary: String,
    pub scope: String,
    pub can_always_allow: bool,
}

/// Notification events identify only a temporary request. Tool arguments,
/// page contents, URLs, permissions and tunnel IDs never enter OS alerts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ApprovalNotification {
    Pending { request_id: String, expires_at_ms: u64 },
    Finished { request_id: String },
}

type Notifier = Arc<dyn Fn(ApprovalNotification) + Send + Sync>;

struct Pending {
    rule: Rule,
    summary: String,
    can_always_allow: bool,
    deadline: Instant,
    expires_at_ms: u64,
    verdict: Option<bool>,
}

#[derive(Default)]
struct State {
    pending: BTreeMap<String, Pending>,
    rules: Vec<Rule>,
}

pub(crate) struct ApprovalBroker {
    path: PathBuf,
    state: Mutex<State>,
    changed: Condvar,
    notifier: Option<Notifier>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .unwrap_or_default().as_millis().try_into().unwrap_or(u64::MAX)
}

impl ApprovalBroker {
    pub fn new(path: PathBuf) -> Self {
        let rules = load_rules(&path);
        Self { path, state: Mutex::new(State { rules, ..State::default() }), changed: Condvar::new(), notifier: None }
    }

    pub fn with_notifier(path: PathBuf, notifier: Notifier) -> Self {
        let mut broker = Self::new(path);
        broker.notifier = Some(notifier);
        broker
    }

    fn notify(&self, event: ApprovalNotification) {
        if let Some(notifier) = &self.notifier {
            // A notification never grants authority or changes the outcome.
            // The native bridge fails closed on notification/OS errors.
            notifier(event);
        }
    }

    pub fn snapshot(&self, tunnel_id: Option<&str>) -> Result<Value, String> {
        let guard = self.state.lock().map_err(|_| "Approval state unavailable")?;
        let pending = guard.pending.iter().filter(|(_, item)| {
            item.verdict.is_none() && Instant::now() < item.deadline
                && tunnel_id == Some(item.rule.tunnel_id.as_str())
        }).map(|(id, item)| json!({"requestId":id,"pluginName":item.rule.plugin_name,
            "toolName":item.rule.tool_name,"summary":item.summary,"scope":item.rule.scope,
            "canAlwaysAllow":item.can_always_allow,"expiresAtMs":item.expires_at_ms}))
            .collect::<Vec<_>>();
        let always_allowed = guard.rules.iter().filter(|rule| tunnel_id == Some(rule.tunnel_id.as_str()))
            .map(|rule|json!({"ruleId":rule.id,"pluginName":rule.plugin_name,
                "toolName":rule.tool_name,"scope":rule.scope}))
            .collect::<Vec<_>>();
        Ok(json!({"pending":pending,"alwaysAllowed":always_allowed}))
    }

    /// Wait on a blocking worker while the native UI displays the approval.
    /// The OpenAI response lease bounds this wait and leaves time for POST.
    pub fn authorize(&self, intent: ApprovalIntent, tunnel_id: &str, remote_deadline: Instant) -> Result<bool, String> {
        if intent.plugin_id.len() > 32 || intent.plugin_name.len() > 80
            || intent.tool_name.len() > 80 || intent.summary.len() > 250 || intent.scope.len() > 256
            || intent.scope.is_empty() || !tunnel_id.starts_with("tunnel_")
        { return Err("Invalid Plugins approval target".into()); }
        let rule = Rule { id: Uuid::new_v4().to_string(), tunnel_id: tunnel_id.into(),
            plugin_id: intent.plugin_id, plugin_name:intent.plugin_name,
            tool_name:intent.tool_name, scope:intent.scope };
        let cutoff = remote_deadline.checked_sub(RESPONSE_RESERVE)
            .unwrap_or(remote_deadline);
        let deadline = Instant::now().checked_add(MAX_WAIT).unwrap_or(cutoff).min(cutoff);
        let now = Instant::now();
        if now >= deadline { return Ok(false); }
        let mut guard = self.state.lock().map_err(|_| "Approval state unavailable")?;
        // An old saved "Always allow" rule must never silently authorize a
        // newer session-only action. The caller explicitly decides which
        // actions are eligible for durable grants.
        if intent.can_always_allow && guard.rules.iter().any(|existing| existing.matches(&rule)) {
            return Ok(true);
        }
        if guard.pending.len() >= MAX_PENDING { return Err("Too many pending Plugins approvals".into()); }
        let id = Uuid::new_v4().to_string();
        let expires_at_ms = now_ms().saturating_add((deadline-now).as_millis() as u64);
        guard.pending.insert(id.clone(), Pending { rule, summary:intent.summary,
            can_always_allow:intent.can_always_allow, deadline, expires_at_ms, verdict:None });
        drop(guard);
        self.notify(ApprovalNotification::Pending {
            request_id: id.clone(), expires_at_ms,
        });
        let mut guard = self.state.lock().map_err(|_| "Approval state unavailable")?;
        loop {
            let verdict = guard.pending.get(&id).and_then(|item| item.verdict);
            if let Some(verdict) = verdict {
                guard.pending.remove(&id);
                drop(guard);
                self.notify(ApprovalNotification::Finished { request_id:id });
                return Ok(verdict && Instant::now() < deadline);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                guard.pending.remove(&id);
                drop(guard);
                self.notify(ApprovalNotification::Finished { request_id:id });
                return Ok(false);
            }
            let (next, _) = self.changed.wait_timeout(guard, remaining)
                .map_err(|_| "Approval state unavailable")?;
            guard = next;
        }
    }

    pub fn decide(&self, request_id:&str, decision:&str) -> Result<bool, String> {
        let mut guard = self.state.lock().map_err(|_| "Approval state unavailable")?;
        let pending = guard.pending.get(request_id).ok_or("Approval request expired or unknown")?;
        if pending.verdict.is_some() || Instant::now() >= pending.deadline {
            return Err("Approval request expired or already decided".into());
        }
        if decision == "always_allow" {
            if !pending.can_always_allow { return Err("Always allow is unavailable for this action".into()); }
            let rule = pending.rule.clone();
            if !guard.rules.iter().any(|item| item.matches(&rule)) {
                let mut next = guard.rules.clone();
                next.push(rule);
                save_rules(&self.path, &next)?;
                guard.rules = next;
            }
        } else if !matches!(decision,"approve"|"reject") {
            return Err("Invalid Plugins approval decision".into());
        }
        guard.pending.get_mut(request_id).ok_or("Approval request missing")?.verdict = Some(decision != "reject");
        self.changed.notify_all();
        Ok(true)
    }

    pub fn revoke(&self, rule_id:&str, tunnel_id: &str) -> Result<bool, String> {
        let mut guard = self.state.lock().map_err(|_| "Approval state unavailable")?;
        let mut next = guard.rules.clone();
        let old_len = next.len();
        next.retain(|rule| !(rule.id == rule_id && rule.tunnel_id == tunnel_id));
        if next.len() == old_len { return Err("Always allow rule not found".into()) }
        save_rules(&self.path, &next)?;
        guard.rules = next;
        Ok(true)
    }

    pub fn invalidate_pending(&self, plugin_id: Option<&str>) -> Result<(), String> {
        let mut guard = self.state.lock().map_err(|_| "Approval state unavailable")?;
        for item in guard.pending.values_mut() {
            if plugin_id.is_none_or(|id| item.rule.plugin_id == id) {
                item.verdict = Some(false);
            }
        }
        self.changed.notify_all();
        Ok(())
    }

    pub fn revoke_plugin_rules(&self, plugin_id:&str) -> Result<(), String> {
        let mut guard = self.state.lock().map_err(|_| "Approval state unavailable")?;
        let mut next = guard.rules.clone();
        next.retain(|rule| rule.plugin_id != plugin_id);
        if next.len() != guard.rules.len() {
            save_rules(&self.path, &next)?;
            guard.rules = next;
        }
        for item in guard.pending.values_mut() {
            if item.rule.plugin_id == plugin_id { item.verdict=Some(false); }
        }
        self.changed.notify_all();
        Ok(())
    }

    pub fn revoke_all_rules(&self) -> Result<(), String> {
        let mut guard = self.state.lock().map_err(|_| "Approval state unavailable")?;
        if !guard.rules.is_empty() {
            save_rules(&self.path, &[])?;
            guard.rules.clear();
        }
        for item in guard.pending.values_mut() { item.verdict=Some(false); }
        self.changed.notify_all();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};

    fn broker() -> (Arc<ApprovalBroker>, PathBuf) {
        let dir=std::env::temp_dir().join(format!("mcp-plugin-approval-{}",Uuid::new_v4()));
        (Arc::new(ApprovalBroker::new(dir.join("rules.json"))), dir)
    }
    fn request() -> ApprovalIntent {
        ApprovalIntent{plugin_id:"browser".into(),plugin_name:"Browser".into(),
            tool_name:"android_browser_start".into(),summary:"Open HTTPS origin".into(),
            scope:"https://example.org".into(),can_always_allow:true}
    }
    fn pending_id(broker:&ApprovalBroker) -> String {
        for _ in 0..100 {
            let state=broker.snapshot(Some("tunnel_0123456789abcdef0123456789abcdef")).unwrap();
            if let Some(id)=state["pending"].as_array().unwrap().first().and_then(|p|p["requestId"].as_str()) {
                return id.to_owned();
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("Pending approval did not arrive");
    }
    #[test]
    fn reject_is_one_time_and_remote_or_unknown_cannot_self_approve() {
        let (broker, dir)=broker();
        let thread_broker=broker.clone();
        let task=thread::spawn(move || thread_broker.authorize(request(),"tunnel_0123456789abcdef0123456789abcdef",Instant::now()+Duration::from_secs(10)).unwrap());
        let id=pending_id(&broker);
        assert!(broker.decide("bogus","approve").is_err());
        assert!(broker.decide(&id,"reject").unwrap());
        assert!(!task.join().unwrap());
        assert_eq!(broker.snapshot(Some("tunnel_0123456789abcdef0123456789abcdef")).unwrap()["alwaysAllowed"],json!([]));
        let _=fs::remove_dir_all(dir);
    }
    #[test]
    fn approve_allows_exactly_one_call_and_requires_new_decision_on_retry() {
        let (broker, dir)=broker();
        let tunnel="tunnel_0123456789abcdef0123456789abcdef";
        let first=broker.clone();
        let task=thread::spawn(move || first.authorize(request(),tunnel,
            Instant::now()+Duration::from_secs(10)).unwrap());
        let id=pending_id(&broker);
        assert!(broker.decide(&id,"approve").unwrap());
        assert!(task.join().unwrap());
        assert_eq!(broker.snapshot(Some(tunnel)).unwrap()["alwaysAllowed"], json!([]));

        let second=broker.clone();
        let repeated=thread::spawn(move || second.authorize(request(),tunnel,
            Instant::now()+Duration::from_secs(10)).unwrap());
        let new_id=pending_id(&broker);
        assert_ne!(new_id,id,"A second call must get a new opaque request ID");
        assert!(broker.decide(&new_id,"reject").unwrap());
        assert!(!repeated.join().unwrap());
        let _=fs::remove_dir_all(dir);
    }
    #[test]
    fn always_allow_is_scoped_persisted_revocable_and_never_other_targets() {
        let (broker,dir)=broker();
        let tunnel="tunnel_0123456789abcdef0123456789abcdef";
        let thread_broker=broker.clone();
        let task=thread::spawn(move || thread_broker.authorize(request(),tunnel,Instant::now()+Duration::from_secs(10)).unwrap());
        let id=pending_id(&broker);
        assert!(broker.decide(&id,"always_allow").unwrap());
        assert!(task.join().unwrap());
        let reopened=ApprovalBroker::new(dir.join("rules.json"));
        assert!(reopened.authorize(request(),tunnel,Instant::now()+Duration::from_secs(10)).unwrap());
        let snapshot=reopened.snapshot(Some(tunnel)).unwrap();
        let rule_id=snapshot["alwaysAllowed"][0]["ruleId"].as_str().unwrap();
        assert_eq!(snapshot["alwaysAllowed"][0]["scope"],"https://example.org");
        let different_tunnel="tunnel_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(reopened.snapshot(Some(different_tunnel)).unwrap()["alwaysAllowed"],json!([]));
        assert!(reopened.revoke(rule_id,tunnel).unwrap());
        assert_eq!(reopened.snapshot(Some(tunnel)).unwrap()["alwaysAllowed"],json!([]));
        let _=fs::remove_dir_all(dir);
    }
    #[test]
    fn old_saved_rule_cannot_bypass_new_session_only_approval_policy() {
        let (broker,dir)=broker();
        let tunnel="tunnel_0123456789abcdef0123456789abcdef";
        let first=broker.clone();
        let started=thread::spawn(move || first.authorize(request(),tunnel,
            Instant::now()+Duration::from_secs(10)).unwrap());
        let original_id=pending_id(&broker);
        broker.decide(&original_id,"always_allow").unwrap();
        assert!(started.join().unwrap());

        // Even if an earlier version stored permission for this same tool
        // and scope, the new caller can explicitly require session-only
        // consent and must receive a fresh on-device decision.
        let next=broker.clone();
        let task=thread::spawn(move || next.authorize(ApprovalIntent {
            can_always_allow:false,..request()
        },tunnel,Instant::now()+Duration::from_secs(10)).unwrap());
        let new_id=pending_id(&broker);
        assert_ne!(new_id,original_id);
        assert!(broker.decide(&new_id,"always_allow").is_err());
        broker.decide(&new_id,"reject").unwrap();
        assert!(!task.join().unwrap());
        let _=fs::remove_dir_all(dir);
    }
    #[test]
    fn expiry_and_invalidation_are_fail_closed() {
        let (broker,dir)=broker();
        let t="tunnel_0123456789abcdef0123456789abcdef";
        assert!(!broker.authorize(request(),t,Instant::now()+Duration::from_millis(5)).unwrap());
        let worker=broker.clone();
        let task=thread::spawn(move || worker.authorize(request(),t,Instant::now()+Duration::from_secs(10)).unwrap());
        let id=pending_id(&broker);
        broker.invalidate_pending(Some("browser")).unwrap();
        assert!(!task.join().unwrap());
        assert!(broker.decide(&id,"approve").is_err());
        let _=fs::remove_dir_all(dir);
    }
    #[test]
    fn notification_is_generic_one_shot_and_closed_after_decision() {
        let (_, dir) = broker();
        let events=Arc::new(Mutex::new(Vec::<ApprovalNotification>::new()));
        let received=events.clone();
        let broker=Arc::new(ApprovalBroker::with_notifier(
            dir.join("rules.json"),
            Arc::new(move |notice| received.lock().unwrap().push(notice)),
        ));
        let tunnel="tunnel_0123456789abcdef0123456789abcdef";
        let worker=broker.clone();
        let task=thread::spawn(move ||worker.authorize(request(),tunnel,
            Instant::now()+Duration::from_secs(10)).unwrap());
        let id=pending_id(&broker);
        assert!(broker.decide(&id,"approve").unwrap());
        assert!(task.join().unwrap());
        let observed=events.lock().unwrap().clone();
        assert_eq!(observed.len(),2);
        assert!(matches!(&observed[0],ApprovalNotification::Pending{request_id,expires_at_ms}
            if request_id==&id && *expires_at_ms>=now_ms().saturating_sub(11_000)));
        assert_eq!(observed[1],ApprovalNotification::Finished{request_id:id});
        let _=fs::remove_dir_all(dir);
    }
}
