//! Android Plugins catalog and verified runtime tool registry.
//!
//! This maintains separate Plugins module settings on the existing shared Core
//! MCP tunnel. A saved ID or enabled switch is configuration only: neither
//! proves that the Core tunnel is connected or accepting tool calls.
//! No network connection, tool execution, or readiness probing happens when
//! a plugin is registered, enabled, or restored from saved configuration.
//!
//! `tools_list()` publishes native definitions supported by a real host probe,
//! or actual verified MCP `tools/list` results bound to a live McpSession.
//! All model-facing names are per-binding aliases. Unapproved `execute_tool`
//! permits only native status checks; actions require a separate, trusted
//! approval followed by `execute_approved_tool`, which rechecks live OS state.
//! The caller MUST authenticate the tunnel and authorize side effects; this
//! registry does not implement an approval decision or automatic execution.
use crate::{local_agent_tools, mcp_client};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use uuid::Uuid;

pub use crate::local_agent_tools::AndroidLocalToolHost;
pub use crate::mcp_client::McpSession as VerifiedRemoteMcpSession;

/// Pure compiled-in operations, used only by the trusted Android Plugins host
/// adapter. Unrecognized names always delegate to the existing Android host.
pub fn execute_curated_host_tool(name: &str, args: &Value) -> Option<Result<Value, String>> {
    local_agent_tools::curated_host_execute(name, args)
}

// Older catalogs allowed 12 entries; leave two slots for compulsory curated
// migration even when every legacy slot was occupied.
const MAX_PLUGINS: usize = 16;
// Native 13 existing + 2 curated tools fit Core's 16-definition MCP cap.
const MAX_EXPOSED_TOOLS: usize = 16;
const MAX_CONFIG_BYTES: usize = 16 * 1024;
const MAX_MODEL_DEFINITIONS_BYTES: usize = 32 * 1024;

/// Only compiled, explicitly verified native capabilities may be advertised.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginSource {
    NativeBrowser,
    NativeApp,
    NativeSecurityAudit,
    NativeReverseEngineering,
    RemoteMcp { tunnel_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginSpec {
    pub id: String,
    pub name: String,
    pub source: PluginSource,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginStatus {
    pub id: String,
    pub name: String,
    pub source: PluginSource,
    pub enabled: bool,
    /// Whether an app-bundled module is currently registered for use. The
    /// code itself ships inside the APK and is never downloaded dynamically.
    /// An external MCP descriptor does not constitute an installed bundle.
    pub installed: bool,
    pub installation: &'static str,
    pub update_supported: bool,
    /// Native: capabilities published after a supported OS probe, including
    /// actions which are temporarily unavailable in the current OS state.
    /// Remote: definitions from the verified live MCP session.
    pub verified_tool_count: usize,
    /// Native: actions available on the most recent genuine OS status probe.
    /// Execution rechecks the OS again, so this is diagnostic, not authority.
    /// Remote: currently bound MCP definitions (unchanged behavior).
    pub ready_tool_count: usize,
    pub tools_available: bool,
    /// Core tunnel authentication is NEVER proven by this catalog. The tunnel
    /// owner separately reports its actual authenticated live state.
    pub plugins_tunnel_verified: bool,
    pub state: &'static str,
    pub category: &'static str,
    pub version: &'static str,
    pub risk: &'static str,
    pub source_label: &'static str,
    pub permissions: Vec<&'static str>,
    pub tool_names: Vec<String>,
}

/// Verified identity for a pending Plugins call. Inspection does not approve
/// or run the tool; the caller must authorize the exact proposed call itself.
#[derive(Clone, Debug)]
pub struct PluginToolInfo {
    pub plugin_id: String,
    pub plugin_name: String,
    pub original_name: String,
    pub source: PluginSource,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigV1 {
    version: u8,
    plugins: Vec<PluginSpec>,
}

/// Version 2 persists native alias identities and optional bundled-module
/// uninstall tombstones, never a live tool binding, OS permission, pending
/// request, or approval grant.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigV2 {
    version: u8,
    plugins: Vec<PluginSpec>,
    native_generations: BTreeMap<String, String>,
    /// Tombstones for user-uninstalled compiled modules. Their code remains
    /// part of the APK, but their tools are never rebound or advertised.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    bundled_uninstalled: Vec<String>,
}

/// This is a manifest of capabilities already compiled into the signed APK.
/// It is deliberately not a network marketplace or arbitrary archive loader.
fn is_bundled_id(id: &str) -> bool {
    matches!(id, "browser" | "app_control" | "security_audit" | "reverse_engineering")
}

enum RuntimeBinding {
    Native {
        host: Arc<dyn AndroidLocalToolHost>,
        // A fixed set of actually implemented native capabilities is bound
        // only after the OS confirms that this capability is supported.
        verified_names: Vec<String>,
        // Last verified OS state; never used to authorize tool execution.
        last_ready_names: Vec<String>,
    },
    Remote {
        session: VerifiedRemoteMcpSession,
    },
}

struct Entry {
    spec: PluginSpec,
    /// Persisted for native plugins only; ephemeral for remote MCP bindings.
    /// Explicit revocation/rebind/toggle rotates it; process restart does not.
    generation: Uuid,
    runtime: Option<RuntimeBinding>,
}

impl Entry {
    fn drop_runtime(&mut self) {
        if let Some(RuntimeBinding::Remote { session }) = self.runtime.as_mut() {
            session.disconnect();
        }
        self.runtime = None;
    }

    fn revoke(&mut self) {
        self.drop_runtime();
        self.generation = Uuid::new_v4();
    }

    fn live_tools(&self) -> Vec<Value> {
        if !self.spec.enabled {
            return Vec::new();
        }
        match &self.runtime {
            Some(RuntimeBinding::Native { verified_names, .. }) => local_agent_tools::definitions()
                .into_iter()
                .filter(|item| {
                    item["name"]
                        .as_str()
                        .is_some_and(|name| verified_names.iter().any(|allowed| allowed == name))
                })
                .collect(),
            Some(RuntimeBinding::Remote { session }) => session.tool_definitions(),
            _ => Vec::new(),
        }
    }

    fn model_alias(&self, index: usize, original_name: &str) -> Option<String> {
        let token = self.generation.simple().to_string();
        match self.spec.source {
            // The native suffix is bound to the *tool identity*, never its
            // position in a state-dependent list. Even 32-byte plugin IDs fit
            // MCP's 64-byte alias limit. A stale start can never become stop.
            PluginSource::NativeBrowser | PluginSource::NativeApp
            | PluginSource::NativeSecurityAudit | PluginSource::NativeReverseEngineering => {
                let suffix = native_alias_suffix(original_name)?;
                Some(format!("p_{}_{}_{}", self.spec.id, &token[..16], suffix))
            }
            // Remote MCP aliases and their indexing are unchanged.
            PluginSource::RemoteMcp { .. } => {
                Some(format!("p_{}_{}_{}", self.spec.id, &token[..16], index))
            }
        }
    }
}

fn native_alias_suffix(original_name: &str) -> Option<&'static str> {
    Some(match original_name {
        "android_browser_status" => "b_status",
        "android_browser_start" => "b_start",
        "android_browser_stop" => "b_stop",
        "android_browser_snapshot" => "b_snapshot",
        "android_browser_click" => "b_click",
        "android_browser_fill" => "b_fill",
        "android_app_status" => "a_status",
        "android_app_start" => "a_start",
        "android_app_stop" => "a_stop",
        "android_app_snapshot" => "a_snapshot",
        "android_app_click" => "a_click",
        "android_app_fill" => "a_fill",
        "android_app_gesture" => "a_gesture",
        "security_audit_analyze" => "s_analyze",
        "reverse_engineering_dex_header" => "r_dex_header",
        _ => return None,
    })
}

/// The fixed native catalog is derived only from implemented tool definitions
/// after a real OS-supported status probe, never from the renderer or config.
/// Transient browser/app readiness is independently checked at call time.
fn native_capability_names(source: &PluginSource) -> Vec<String> {
    let names: &[&str] = match source {
        PluginSource::NativeBrowser => &[
            "android_browser_status",
            "android_browser_start",
            "android_browser_snapshot",
            "android_browser_click",
            "android_browser_fill",
            "android_browser_stop",
        ],
        PluginSource::NativeApp => &[
            "android_app_status",
            "android_app_start",
            "android_app_stop",
            "android_app_snapshot",
            "android_app_click",
            "android_app_fill",
            "android_app_gesture",
        ],
        PluginSource::NativeSecurityAudit => &["security_audit_analyze"],
        PluginSource::NativeReverseEngineering => &["reverse_engineering_dex_header"],
        PluginSource::RemoteMcp { .. } => &[],
    };
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// Use real OS-derived availability rules for diagnostic readiness and
/// immediately before executing a separately approved action.
fn native_tool_names(source: &PluginSource, receipt: &Value) -> Result<Vec<String>, String> {
    let status_name = match source {
        PluginSource::NativeBrowser => "android_browser_status",
        PluginSource::NativeApp => "android_app_status",
        PluginSource::NativeSecurityAudit => "security_audit_internal_status",
        PluginSource::NativeReverseEngineering => "reverse_engineering_internal_status",
        PluginSource::RemoteMcp { .. } => return Err("Not a native plugin".into()),
    };
    let checked = local_agent_tools::checked_result(status_name, receipt.clone())?;
    if receipt["supported"] != true {
        return Err("Native plugin did not confirm implemented support".into());
    }
    let names: Vec<&str> = match source {
        PluginSource::NativeBrowser => {
            if checked["running"] == true && checked["profileIsolated"] != true {
                return Err("App-owned isolated browser capability not verified".into());
            }
            let mut names = vec!["android_browser_status"];
            if checked["running"] == true {
                names.push("android_browser_stop");
                if checked["ready"] == true {
                    names.extend([
                        "android_browser_snapshot",
                        "android_browser_click",
                        "android_browser_fill",
                    ]);
                }
            } else {
                names.push("android_browser_start");
            }
            names
        }
        PluginSource::NativeApp => {
            let mut names = vec!["android_app_status"];
            if checked["connected"] == true {
                names.push("android_app_start");
            }
            if checked["active"] == true {
                names.push("android_app_stop");
                if checked["visible"] == true && checked["automationActionsAvailable"] == true {
                    names.extend([
                        "android_app_snapshot",
                        "android_app_click",
                        "android_app_gesture",
                    ]);
                    if checked["selectedAppMode"] == "realtime" {
                        names.push("android_app_fill");
                    }
                }
            }
            names
        }
        PluginSource::NativeSecurityAudit => vec!["security_audit_analyze"],
        PluginSource::NativeReverseEngineering => vec!["reverse_engineering_dex_header"],
        PluginSource::RemoteMcp { .. } => unreachable!(),
    };
    Ok(names.into_iter().map(str::to_owned).collect())
}

/// Independent, bounded registry. Prime can own it under a Mutex, persist only
/// `export_config()` (no secrets), and rebind runtime tools after app restart.
/// Merely restoring a config NEVER restores tool readiness or live authority.
#[derive(Default)]
pub struct AndroidPluginRegistry {
    plugins: Vec<Entry>,
    bundled_uninstalled: BTreeSet<String>,
}

#[derive(Clone, Copy)]
enum NativeCallAuthority {
    Unapproved,
    Approved,
}

impl AndroidPluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install only truthful, currently implemented native entries, disabled
    /// by default. Neither the browser nor Accessibility is marked ready.
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry
            .add_plugin(PluginSpec {
                id: "browser".into(),
                name: "App-owned browser".into(),
                source: PluginSource::NativeBrowser,
                enabled: false,
            })
            .expect("Static browser builtin must be valid");
        registry
            .add_plugin(PluginSpec {
                id: "app_control".into(),
                name: "Android app control".into(),
                source: PluginSource::NativeApp,
                enabled: false,
            })
            .expect("Static app-control builtin must be valid");
        registry.add_curated_builtins().expect("Static bundled catalog must fit");
        registry
    }

    fn add_curated_builtins(&mut self) -> Result<(), String> {
        for (id, name, source) in [
            ("security_audit", "Security audit", PluginSource::NativeSecurityAudit),
            ("reverse_engineering", "Reverse engineering", PluginSource::NativeReverseEngineering),
        ] {
            if !self.plugins.iter().any(|entry| entry.spec.id == id) {
                self.add_plugin(PluginSpec {id:id.into(),name:name.into(),source,enabled:false})?;
            }
        }
        Ok(())
    }

    pub fn from_config(raw: &str) -> Result<Self, String> {
        if raw.len() > MAX_CONFIG_BYTES {
            return Err("Plugin catalog config exceeds 16 KiB".into());
        }
        let envelope: Value =
            serde_json::from_str(raw).map_err(|_| "Invalid Android plugin catalog config")?;
        let (plugins, generations, uninstalled) = match envelope["version"].as_u64() {
            Some(1) => {
                let v1: ConfigV1 = serde_json::from_value(envelope)
                    .map_err(|_| "Invalid Android plugin v1 catalog config")?;
                (v1.plugins, None, Vec::new())
            }
            Some(2) => {
                let v2: ConfigV2 = serde_json::from_value(envelope)
                    .map_err(|_| "Invalid Android plugin v2 catalog config")?;
                (v2.plugins, Some(v2.native_generations), v2.bundled_uninstalled)
            }
            _ => return Err("Unsupported plugin catalog version or capacity".into()),
        };
        if plugins.len() > MAX_PLUGINS {
            return Err("Unsupported plugin catalog version or capacity".into());
        }
        let mut registry = Self::new();
        for plugin in plugins {
            // The persisted file is not an authority to invent native modules
            // or impersonate a compiled ID with a remote MCP source. Keep the
            // generic add_plugin helper for isolated tests, but restore only
            // the four genuine module identities shipped in the APK.
            let canonical = match plugin.source {
                PluginSource::NativeBrowser => plugin.id == "browser",
                PluginSource::NativeApp => plugin.id == "app_control",
                PluginSource::NativeSecurityAudit => plugin.id == "security_audit",
                PluginSource::NativeReverseEngineering =>
                    plugin.id == "reverse_engineering",
                PluginSource::RemoteMcp { .. } => !is_bundled_id(&plugin.id),
            };
            if !canonical {
                return Err("Saved plugin cannot impersonate bundled Android code".into());
            }
            registry.add_plugin(plugin)?;
        }
        if let Some(mut generations) = generations {
            for entry in registry.plugins.iter_mut() {
                if matches!(
                    entry.spec.source,
                    PluginSource::NativeBrowser | PluginSource::NativeApp
                    | PluginSource::NativeSecurityAudit | PluginSource::NativeReverseEngineering
                ) {
                    let saved = generations
                        .remove(&entry.spec.id)
                        .ok_or("Native plugin alias generation missing")?;
                    let generation = Uuid::parse_str(&saved)
                        .map_err(|_| "Invalid native plugin alias generation")?;
                    if generation.get_version_num() != 4 || generation.is_nil() {
                        return Err("Invalid native plugin alias generation".into());
                    }
                    entry.generation = generation;
                }
            }
            if !generations.is_empty() {
                return Err("Unexpected native plugin alias generation".into());
            }
        }
        // Upgrade existing v1/v2 catalogs without changing browser/app
        // identities or granting authority to the new disabled modules.
        // Existing known native identities were validated above; missing new
        // modules get fresh UUIDs that are persisted before tools/list.
        registry.add_curated_builtins()?;
        // Tombstones cannot carry enabled authority or name arbitrary
        // external/remote entries. Reject duplicates and forged IDs rather
        // than silently repairing an untrusted saved catalog.
        let mut seen = BTreeSet::new();
        for id in uninstalled {
            if !is_bundled_id(&id) || !seen.insert(id.clone()) {
                return Err("Invalid uninstalled Android bundled module".into());
            }
            let entry = registry.entry(&id)?;
            if !matches!(
                entry.spec.source,
                PluginSource::NativeBrowser | PluginSource::NativeApp
                | PluginSource::NativeSecurityAudit | PluginSource::NativeReverseEngineering
            ) || entry.spec.enabled {
                return Err("Uninstalled bundled module cannot be enabled".into());
            }
        }
        registry.bundled_uninstalled = seen;
        Ok(registry)
    }

    /// Native alias generations are durable identities, not permission grants.
    /// No runtime host, readiness, bearer token, tool args or approvals persist.
    pub fn export_config(&self) -> Result<String, String> {
        let config = ConfigV2 {
            version: 2,
            plugins: self
                .plugins
                .iter()
                .map(|entry| entry.spec.clone())
                .collect(),
            native_generations: self
                .plugins
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.spec.source,
                        PluginSource::NativeBrowser | PluginSource::NativeApp
                        | PluginSource::NativeSecurityAudit | PluginSource::NativeReverseEngineering
                    )
                })
                .map(|entry| (entry.spec.id.clone(), entry.generation.to_string()))
                .collect(),
            bundled_uninstalled: self.bundled_uninstalled.iter().cloned().collect(),
        };
        let result = serde_json::to_string(&config)
            .map_err(|_| "Cannot serialize Android plugin catalog")?;
        if result.len() > MAX_CONFIG_BYTES {
            return Err("Plugin catalog config exceeds 16 KiB".into());
        }
        Ok(result)
    }

    pub fn add_plugin(&mut self, spec: PluginSpec) -> Result<(), String> {
        validate_spec(&spec)?;
        if self.plugins.len() >= MAX_PLUGINS {
            return Err("Plugin catalog is full".into());
        }
        if self.plugins.iter().any(|item| item.spec.id == spec.id) {
            return Err("Duplicate plugin ID".into());
        }
        if matches!(
            &spec.source,
            PluginSource::NativeBrowser | PluginSource::NativeApp
            | PluginSource::NativeSecurityAudit | PluginSource::NativeReverseEngineering
        ) && self
            .plugins
            .iter()
            .any(|item| item.spec.source == spec.source)
        {
            return Err("Native plugin capability already has a catalog entry".into());
        }
        self.plugins.push(Entry {
            spec,
            generation: Uuid::new_v4(),
            runtime: None,
        });
        Ok(())
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<(), String> {
        if self.bundled_uninstalled.contains(id) {
            return Err("Install the bundled module before enabling it".into());
        }
        let entry = self.entry_mut(id)?;
        if entry.spec.enabled != enabled {
            entry.revoke(); // re-enabling requires a fresh, real verification
            entry.spec.enabled = enabled;
        }
        Ok(())
    }

    /// Register a module that is already compiled into this APK. There is no
    /// third-party code loading or implicit Enable on installation.
    pub fn install_bundled(&mut self, id: &str) -> Result<(), String> {
        if !is_bundled_id(id) {
            return Err("External plugin bundle installation is unsupported".into());
        }
        let entry = self.entry(id)?;
        if !matches!(
            entry.spec.source,
            PluginSource::NativeBrowser | PluginSource::NativeApp
            | PluginSource::NativeSecurityAudit | PluginSource::NativeReverseEngineering
        ) {
            return Err("Plugin does not match a compiled Android module".into());
        }
        if self.bundled_uninstalled.remove(id) {
            let entry = self.entry_mut(id)?;
            entry.revoke();
            entry.spec.enabled = false;
        }
        Ok(())
    }

    /// Unregister a bundled module. Revocation invalidates all its published
    /// aliases; the APK's immutable code cannot be removed independently.
    pub fn uninstall_bundled(&mut self, id: &str) -> Result<(), String> {
        if !is_bundled_id(id) {
            return Err("External plugin bundle uninstall is unsupported".into());
        }
        let already_uninstalled = self.bundled_uninstalled.contains(id);
        let entry = self.entry_mut(id)?;
        if !matches!(
            entry.spec.source,
            PluginSource::NativeBrowser | PluginSource::NativeApp
            | PluginSource::NativeSecurityAudit | PluginSource::NativeReverseEngineering
        ) {
            return Err("Plugin does not match a compiled Android module".into());
        }
        if !already_uninstalled {
            entry.revoke();
            entry.spec.enabled = false;
            self.bundled_uninstalled.insert(id.to_owned());
        }
        Ok(())
    }

    /// Versioned native code can only be upgraded in a signed app update.
    /// Returning a fake successful update would misrepresent installed code.
    pub fn check_bundled_update(&self, id: &str) -> Result<(), String> {
        if !is_bundled_id(id) {
            return Err("Third-party plugin updates are unsupported".into());
        }
        self.entry(id)?;
        Err("Bundled plugin updates require a signed Mcp Agent Ai APK update".into())
    }

    pub fn remove_plugin(&mut self, id: &str) -> Result<(), String> {
        if is_bundled_id(id) {
            return self.uninstall_bundled(id);
        }
        let position = self
            .plugins
            .iter()
            .position(|entry| entry.spec.id == id)
            .ok_or("Unknown Android plugin")?;
        self.plugins[position].revoke();
        self.plugins.remove(position);
        Ok(())
    }

    pub fn invalidate_runtime(&mut self, id: &str) -> Result<(), String> {
        self.entry_mut(id)?.revoke();
        Ok(())
    }

    /// Native proof comes from an actual read-only app-owned plugin status
    /// command, NOT a renderer-provided `ready: true` or a name-only catalog.
    /// No popup, browser navigation or third-party app launch occurs here.
    /// Once genuine OS support is confirmed, the entire implemented native
    /// capability set is advertised using stable identity-based aliases.
    /// A successful re-probe only updates diagnostic OS readiness; it cannot
    /// rotate aliases just because the browser/app changes state. Every call
    /// still rechecks the OS before dispatch, regardless of the last probe.
    pub async fn verify_native(
        &mut self,
        id: &str,
        host: Arc<dyn AndroidLocalToolHost>,
    ) -> Result<usize, String> {
        let source = {
            let entry = self.entry(id)?;
            if !entry.spec.enabled {
                return Err("Enable plugin before native verification".into());
            }
            match entry.spec.source {
                PluginSource::NativeBrowser => PluginSource::NativeBrowser,
                PluginSource::NativeApp => PluginSource::NativeApp,
                PluginSource::NativeSecurityAudit => PluginSource::NativeSecurityAudit,
                PluginSource::NativeReverseEngineering => PluginSource::NativeReverseEngineering,
                PluginSource::RemoteMcp { .. } => {
                    return Err("Cannot verify remote MCP as a native plugin".into())
                }
            }
        };
        let command = match source {
            PluginSource::NativeBrowser => "android_browser_status",
            PluginSource::NativeApp => "android_app_status",
            PluginSource::NativeSecurityAudit => "security_audit_internal_status",
            PluginSource::NativeReverseEngineering => "reverse_engineering_internal_status",
            PluginSource::RemoteMcp { .. } => unreachable!(),
        };
        let proof = async {
            let receipt = host.execute(command, json!({})).await?;
            native_tool_names(&source, &receipt)
        }
        .await;
        let last_ready_names = match proof {
            Ok(names) => names,
            Err(error) => {
                // Transient OS failure invalidates runtime readiness, NOT the
                // persisted identity. A revoked alias cannot execute without
                // a verified binding, but a later successful probe can reuse
                // the same published name.
                self.entry_mut(id)?.drop_runtime();
                return Err(error);
            }
        };
        let proposed = native_capability_names(&source);
        let prospective = self
            .plugins
            .iter()
            .filter(|entry| entry.spec.id != id)
            .map(|entry| entry.live_tools().len())
            .sum::<usize>()
            + proposed.len();
        if prospective > MAX_EXPOSED_TOOLS {
            return Err("Plugin tool registry would exceed 16 exposed tools".into());
        }
        let entry = self.entry_mut(id)?;
        if !entry.spec.enabled || entry.spec.source != source {
            return Err("Plugin configuration changed during native verification".into());
        }
        if let Some(RuntimeBinding::Native {
            host: previous,
            verified_names,
            last_ready_names: previous_ready,
        }) = &mut entry.runtime
        {
            if Arc::ptr_eq(previous, &host) && verified_names == &proposed {
                *previous_ready = last_ready_names;
                return Ok(proposed.len());
            }
        }
        // The first genuine probe after process restart restores a persisted
        // native alias, but never readiness or host authority. Rebinding a
        // currently bound DIFFERENT host explicitly rotates the identity.
        if entry.runtime.is_some() {
            entry.revoke();
        }
        let count = proposed.len();
        entry.runtime = Some(RuntimeBinding::Native {
            host,
            verified_names: proposed,
            last_ready_names,
        });
        Ok(count)
    }

    /// The separate Plugins tunnel ID selects config only. The *actual* MCP
    /// session must have successfully completed `initialize` AND the bounded
    /// `tools/list` verification. An ID by itself can never grant tool access.
    pub fn attach_remote_session(
        &mut self,
        id: &str,
        tunnel_id: &str,
        session: VerifiedRemoteMcpSession,
    ) -> Result<usize, String> {
        let entry = self.entry(id)?;
        if !entry.spec.enabled {
            return Err("Enable remote plugin before binding".into());
        }
        match &entry.spec.source {
            PluginSource::RemoteMcp { tunnel_id: saved } if saved == tunnel_id => {}
            _ => return Err("Plugin tunnel identity or source does not match".into()),
        }
        let tools = session.tool_definitions();
        if tools.is_empty() {
            return Err("Remote MCP tools/list is not verified; nothing is ready".into());
        }
        if tools.len() > MAX_EXPOSED_TOOLS
            || self
                .plugins
                .iter()
                .filter(|item| item.spec.id != id)
                .map(|item| item.live_tools().len())
                .sum::<usize>()
                + tools.len()
                > MAX_EXPOSED_TOOLS
        {
            return Err("Plugin registry exceeds verified tool budget".into());
        }
        let entry = self.entry_mut(id)?;
        entry.revoke();
        entry.runtime = Some(RuntimeBinding::Remote { session });
        Ok(tools.len())
    }

    pub fn list(&self) -> Vec<PluginStatus> {
        self.plugins
            .iter()
            .map(|entry| {
                let count = entry.live_tools().len();
                let ready_count = if !entry.spec.enabled {
                    0
                } else {
                    match &entry.runtime {
                        Some(RuntimeBinding::Native {
                            last_ready_names, ..
                        }) => last_ready_names.len(),
                        Some(RuntimeBinding::Remote { .. }) => count,
                        None => 0,
                    }
                };
                PluginStatus {
                    id: entry.spec.id.clone(),
                    name: entry.spec.name.clone(),
                    source: entry.spec.source.clone(),
                    enabled: entry.spec.enabled,
                    installed: is_bundled_id(&entry.spec.id)
                        && !self.bundled_uninstalled.contains(&entry.spec.id),
                    installation: if is_bundled_id(&entry.spec.id) {
                        "bundled_with_app"
                    } else {
                        "external_unavailable"
                    },
                    update_supported: false,
                    verified_tool_count: count,
                    ready_tool_count: ready_count,
                    tools_available: count > 0,
                    plugins_tunnel_verified: false,
                    state: if self.bundled_uninstalled.contains(&entry.spec.id) {
                        "not_installed"
                    } else if !entry.spec.enabled {
                        "disabled"
                    } else if count > 0 {
                        match entry.spec.source {
                            PluginSource::RemoteMcp { .. } => "remote_mcp_tools_verified",
                            _ => "native_tools_verified",
                        }
                    } else {
                        "enabled_waiting_for_verification"
                    },
                    category: match entry.spec.source {
                        PluginSource::NativeBrowser | PluginSource::NativeApp => "automation",
                        PluginSource::NativeSecurityAudit => "security",
                        PluginSource::NativeReverseEngineering => "development",
                        PluginSource::RemoteMcp { .. } => "integration",
                    },
                    version: match entry.spec.source {
                        PluginSource::RemoteMcp { .. } => "external",
                        _ => "0.1.0",
                    },
                    risk: match entry.spec.source {
                        PluginSource::NativeSecurityAudit | PluginSource::NativeReverseEngineering => "read_only",
                        _ => "approval_required",
                    },
                    source_label: match entry.spec.source {
                        PluginSource::RemoteMcp { .. } => "Verified remote MCP",
                        _ => "Bundled Android",
                    },
                    permissions: match entry.spec.source {
                        PluginSource::NativeBrowser => vec!["User-approved app-owned browser session"],
                        PluginSource::NativeApp => vec!["User-approved Android Accessibility session"],
                        PluginSource::NativeSecurityAudit | PluginSource::NativeReverseEngineering => Vec::new(),
                        PluginSource::RemoteMcp { .. } => vec!["Remote MCP tool permissions depend on provider"],
                    },
                    tool_names: match entry.spec.source {
                        PluginSource::RemoteMcp { .. } => entry.live_tools().iter()
                            .filter_map(|tool|tool["name"].as_str().map(str::to_owned)).collect(),
                        _ => native_capability_names(&entry.spec.source),
                    },
                }
            })
            .collect()
    }

    /// UI projection contains no secrets or synthetic tool readiness.
    pub fn catalog(&self) -> Value {
        json!({"version":1,"plugins":self.list(),
            "toolCount":self.tools_list().len(),
            "pluginsTunnelVerified":false})
    }

    /// Provider-neutral MCP-compatible definitions. Tool names contain a
    /// fresh per-binding identity; old names become unusable after revocation.
    pub fn tools_list(&self) -> Vec<Value> {
        let mut output = Vec::new();
        for entry in &self.plugins {
            for (index, tool) in entry.live_tools().iter().enumerate() {
                if output.len() >= MAX_EXPOSED_TOOLS {
                    return Vec::new();
                }
                let Some(name) = tool["name"].as_str() else {
                    return Vec::new();
                };
                let Some(schema) = tool.get("inputSchema") else {
                    return Vec::new();
                };
                let description = tool["description"].as_str().unwrap_or("");
                let Some(alias) = entry.model_alias(index, name) else {
                    return Vec::new();
                };
                let translated = json!({"name":alias,
                    "description":format!("{}: {}",entry.spec.name,description),
                    "inputSchema":schema});
                if name.len() > 64
                    || serde_json::to_vec(&translated).map_or(true, |bytes| bytes.len() > 4 * 1024)
                {
                    return Vec::new();
                }
                output.push(translated);
            }
        }
        if serde_json::to_vec(&output)
            .map_or(true, |bytes| bytes.len() > MAX_MODEL_DEFINITIONS_BYTES)
        {
            return Vec::new();
        }
        output
    }

    /// Inspect the exact current alias and arguments before asking a user to
    /// approve a pending call. This does not execute or authorize anything.
    pub fn inspect_tool(&self, alias: &str, args: &Value) -> Result<PluginToolInfo, String> {
        let (entry, original_name) = self.checked_tool(alias, args)?;
        Ok(PluginToolInfo {
            plugin_id: entry.spec.id.clone(),
            plugin_name: entry.spec.name.clone(),
            original_name,
            source: entry.spec.source.clone(),
        })
    }

    fn checked_tool(&self, alias: &str, args: &Value) -> Result<(&Entry, String), String> {
        let (entry, original_name, verified_tools) = self.find_tool(alias)?;
        mcp_client::validate_model_arguments(&verified_tools, &original_name, args)?;
        if matches!(
            entry.spec.source,
            PluginSource::NativeBrowser | PluginSource::NativeApp
            | PluginSource::NativeSecurityAudit | PluginSource::NativeReverseEngineering
        ) {
            local_agent_tools::validate(&original_name, args)?;
        }
        Ok((entry, original_name))
    }

    /// Safe for unapproved inbound Plugins RPC: only native status reads are
    /// executable. Remote MCP behavior retains the existing verified-session
    /// rules and restrictions.
    pub async fn execute_tool(&self, model_alias: &str, args: Value) -> Result<Value, String> {
        self.execute_tool_inner(model_alias, args, NativeCallAuthority::Unapproved)
            .await
    }

    /// Called only after a trusted caller independently approves this exact
    /// tool call. No JSON argument or enabled toggle can grant this authority.
    /// The alias, arguments and actual OS state are rechecked before dispatch.
    pub async fn execute_approved_tool(&self, alias: &str, args: Value) -> Result<Value, String> {
        self.execute_tool_inner(alias, args, NativeCallAuthority::Approved)
            .await
    }

    async fn execute_tool_inner(
        &self,
        model_alias: &str,
        args: Value,
        authority: NativeCallAuthority,
    ) -> Result<Value, String> {
        let (entry, original_name) = self.checked_tool(model_alias, &args)?;
        match entry
            .runtime
            .as_ref()
            .ok_or("Plugin has no verified runtime")?
        {
            RuntimeBinding::Native { host, .. } => {
                // An old probe is insufficient at dispatch: reprobe actual OS
                // status to avoid invoking stale browser snapshot/Accessibility
                // operations after the Activity or service changed state.
                let status_name = match entry.spec.source {
                    PluginSource::NativeBrowser => "android_browser_status",
                    PluginSource::NativeApp => "android_app_status",
                    PluginSource::NativeSecurityAudit => "security_audit_internal_status",
                    PluginSource::NativeReverseEngineering => "reverse_engineering_internal_status",
                    _ => return Err("Native plugin source changed".into()),
                };
                let curated_read_only = matches!(entry.spec.source,
                    PluginSource::NativeSecurityAudit | PluginSource::NativeReverseEngineering);
                if !matches!(authority, NativeCallAuthority::Approved)
                    && original_name != status_name && !curated_read_only
                {
                    return Err("Native Plugins actions require separate approval".into());
                }
                let now = host.execute(status_name, json!({})).await?;
                if !native_tool_names(&entry.spec.source, &now)?
                    .iter()
                    .any(|name| name == &original_name)
                {
                    return Err("Native Plugins tool is no longer available".into());
                }
                let native_args = local_agent_tools::dispatch_arguments(&original_name, &args)?;
                let receipt = host.execute(&original_name, native_args).await?;
                local_agent_tools::checked_result(&original_name, receipt)
            }
            RuntimeBinding::Remote { session } => {
                // MCP core validates again against the *session's* tools/list
                // generation and revocation Arc. A failed call is never retried.
                let mut current = session.clone();
                let result = current.call_tool(&original_name, args).await?;
                if result["isError"] == true {
                    return Err("Verified remote MCP tool reported an error".into());
                }
                let bounded = &result["result"];
                if serde_json::to_vec(bounded).map_or(true, |bytes| bytes.len() > 8 * 1024) {
                    return Err("Remote plugin output exceeds provider-safe 8 KiB".into());
                }
                Ok(bounded.clone())
            }
        }
    }

    fn entry(&self, id: &str) -> Result<&Entry, String> {
        self.plugins
            .iter()
            .find(|entry| entry.spec.id == id)
            .ok_or("Unknown Android plugin".into())
    }
    fn entry_mut(&mut self, id: &str) -> Result<&mut Entry, String> {
        self.plugins
            .iter_mut()
            .find(|entry| entry.spec.id == id)
            .ok_or("Unknown Android plugin".into())
    }
    fn find_tool(&self, alias: &str) -> Result<(&Entry, String, Vec<Value>), String> {
        if alias.len() > 64
            || !alias
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err("Unknown or invalid plugin tool alias".into());
        }
        for entry in &self.plugins {
            let verified_tools = entry.live_tools();
            for (index, tool) in verified_tools.iter().enumerate() {
                let name = tool["name"]
                    .as_str()
                    .ok_or("Verified plugin tool name missing")?;
                if entry.model_alias(index, name).as_deref() == Some(alias) {
                    return Ok((entry, name.to_owned(), verified_tools));
                }
            }
        }
        Err("Unknown, disabled, expired or revoked plugin tool alias".into())
    }
}

fn validate_spec(spec: &PluginSpec) -> Result<(), String> {
    if spec.id.len() < 2
        || spec.id.len() > 32
        || !spec.id.as_bytes()[0].is_ascii_lowercase()
        || !spec
            .id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        || spec.id.ends_with('-')
        || spec.id.ends_with('_')
    {
        return Err("Plugin ID must be a stable lowercase slug (2–32 bytes)".into());
    }
    if spec.name.is_empty()
        || spec.name.trim() != spec.name
        || spec.name.chars().count() > 60
        || spec.name.chars().any(char::is_control)
    {
        return Err("Invalid Android plugin display name".into());
    }
    // Catalog config cannot invent or impersonate compiled marketplace tools.
    if (spec.id == "security_audit") != (spec.source == PluginSource::NativeSecurityAudit)
        || (spec.id == "reverse_engineering")
            != (spec.source == PluginSource::NativeReverseEngineering) {
        return Err("Curated plugin identity is reserved for its compiled module".into());
    }
    if (spec.id == "security_audit" && spec.name != "Security audit")
        || (spec.id == "reverse_engineering" && spec.name != "Reverse engineering") {
        return Err("Curated plugin display name must match bundled module".into());
    }
    if let PluginSource::RemoteMcp { tunnel_id } = &spec.source {
        if tunnel_id.len() < 8
            || tunnel_id.len() > 128
            || !tunnel_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err("Remote plugin must have a separate bounded Plugins tunnel ID".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::{future::Future, pin::Pin};

    struct CompiledCuratedHost;
    impl AndroidLocalToolHost for CompiledCuratedHost {
        fn execute<'a>(&'a self,name:&'a str,args:Value)
            ->Pin<Box<dyn Future<Output=Result<Value,String>> + Send + 'a>> {
            Box::pin(async move {
                execute_curated_host_tool(name,&args)
                    .unwrap_or_else(||Err("Curated test host refuses Android device tools".into()))
            })
        }
    }

    #[tokio::test]
    async fn curated_builtin_install_routing_revocation_and_metadata() {
        let mut registry=AndroidPluginRegistry::with_builtins();
        assert_eq!(registry.list().len(),4);
        for id in ["security_audit","reverse_engineering"] {
            let item=registry.list().into_iter().find(|s|s.id==id).unwrap();
            assert!(!item.enabled);
            assert_eq!(item.risk,"read_only");
            assert_eq!(item.source_label,"Bundled Android");
            assert_eq!(item.tool_names.len(),1);
            assert!(item.permissions.is_empty());
        }
        assert!(registry.tools_list().is_empty());
        registry.set_enabled("security_audit",true).unwrap();
        registry.set_enabled("reverse_engineering",true).unwrap();
        let host=Arc::new(CompiledCuratedHost);
        registry.verify_native("security_audit",host.clone()).await.unwrap();
        registry.verify_native("reverse_engineering",host.clone()).await.unwrap();
        assert_eq!(registry.tools_list().len(),2);
        let audit_alias=alias_of(&registry,"security_audit_analyze");
        let dex_alias=alias_of(&registry,"reverse_engineering_dex_header");
        assert_ne!(audit_alias,dex_alias);
        let audited=registry.execute_tool(&audit_alias,
            json!({"text":"password = never_echo_123\nGET http://example.org"})).await.unwrap();
        assert_eq!(audited["matchedLines"]["possibleCredentials"],1);
        assert!(!audited.to_string().contains("never_echo_123"));
        assert!(registry.execute_tool(&dex_alias,json!({"hexHeader":"00".repeat(112)}))
            .await.is_err());
        assert!(registry.execute_tool(&audit_alias,json!({"text":"hello","untrusted":true}))
            .await.is_err());
        registry.set_enabled("security_audit",false).unwrap();
        assert!(registry.inspect_tool(&audit_alias,&json!({"text":"ok"})).is_err());
        registry.set_enabled("security_audit",true).unwrap();
        registry.verify_native("security_audit",host).await.unwrap();
        assert_ne!(alias_of(&registry,"security_audit_analyze"),audit_alias);
        assert!(registry.execute_tool(&audit_alias,json!({"text":"ok"})).await.is_err());
    }

    #[tokio::test]
    async fn bundled_uninstall_reinstall_persists_disabled_state_and_revokes_old_alias() {
        let mut catalog = AndroidPluginRegistry::with_builtins();
        catalog.set_enabled("security_audit", true).unwrap();
        catalog.verify_native("security_audit", Arc::new(CompiledCuratedHost))
            .await.unwrap();
        let previous = alias_of(&catalog, "security_audit_analyze");
        assert_eq!(catalog.list().iter().find(|p|p.id=="security_audit").unwrap().installed,true);

        // Uninstall unregisters the bundled tools, even though immutable code
        // still exists inside the APK; old aliases become invalid immediately.
        catalog.uninstall_bundled("security_audit").unwrap();
        let item = catalog.list().into_iter().find(|p|p.id=="security_audit").unwrap();
        assert!(!item.installed);
        assert!(!item.enabled);
        assert_eq!(item.state,"not_installed");
        assert_eq!(item.verified_tool_count,0);
        assert!(catalog.inspect_tool(&previous,&json!({"text":"ok"})).is_err());
        assert!(catalog.set_enabled("security_audit",true).is_err());
        let saved = catalog.export_config().unwrap();
        let encoded:Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(encoded["bundledUninstalled"],json!(["security_audit"]));

        let mut rebooted = AndroidPluginRegistry::from_config(&saved).unwrap();
        assert!(rebooted.tools_list().is_empty());
        assert!(!rebooted.list().iter().find(|p|p.id=="security_audit").unwrap().installed);
        assert!(rebooted.set_enabled("security_audit",true).is_err());

        // Re-registering bundled code never implicitly enables it. The prior
        // tool alias cannot be reused, even after a fresh live host probe.
        rebooted.install_bundled("security_audit").unwrap();
        assert!(rebooted.list().iter().find(|p|p.id=="security_audit").unwrap().installed);
        assert!(!rebooted.list().iter().find(|p|p.id=="security_audit").unwrap().enabled);
        assert!(rebooted.tools_list().is_empty());
        rebooted.set_enabled("security_audit",true).unwrap();
        rebooted.verify_native("security_audit",Arc::new(CompiledCuratedHost))
            .await.unwrap();
        assert_ne!(alias_of(&rebooted,"security_audit_analyze"),previous);
        assert!(rebooted.inspect_tool(&previous,&json!({"text":"ok"})).is_err());
        let restored = AndroidPluginRegistry::from_config(&rebooted.export_config().unwrap())
            .unwrap();
        assert!(restored.list().iter().find(|p|p.id=="security_audit").unwrap().installed);
        assert!(restored.list().iter().find(|p|p.id=="security_audit").unwrap().enabled);
        assert!(restored.tools_list().is_empty());
    }

    #[test]
    fn bundle_policy_rejects_external_install_update_and_tampered_tombstones() {
        let mut registry = AndroidPluginRegistry::with_builtins();
        let saved=registry.export_config().unwrap();
        assert!(registry.install_bundled("third_party_package").is_err());
        assert!(registry.uninstall_bundled("third_party_package").is_err());
        assert!(registry.check_bundled_update("security_audit").is_err());
        assert!(registry.check_bundled_update("third_party_package").is_err());
        assert_eq!(registry.export_config().unwrap(), saved);

        registry.remove_plugin("browser").unwrap();
        assert!(!registry.list().iter().find(|p|p.id=="browser").unwrap().installed);
        assert_eq!(registry.list().len(),4,
            "Remove of a compiled module must persist a tombstone, not erase its identity");
        let valid:Value=serde_json::from_str(&registry.export_config().unwrap()).unwrap();
        for invalid in [
            json!(["random_external"]),
            json!(["browser","browser"]),
            json!(["security_audit","remote_unsafe"])
        ] {
            let mut tampered=valid.clone();
            tampered["bundledUninstalled"]=invalid;
            assert!(AndroidPluginRegistry::from_config(&tampered.to_string()).is_err());
        }
        let mut installed_and_enabled=valid;
        installed_and_enabled["plugins"][0]["enabled"]=json!(true);
        assert!(AndroidPluginRegistry::from_config(&installed_and_enabled.to_string()).is_err());

        // A forged saved catalog must never claim that remote MCP is the
        // bundled browser, or that an arbitrary slug is a new native module.
        let spoof_remote=json!({"version":1,"plugins":[{
            "id":"browser","name":"Fake browser",
            "source":{"kind":"remote_mcp","tunnel_id":"plugins_external_123"},
            "enabled":true
        }]});
        assert!(AndroidPluginRegistry::from_config(&spoof_remote.to_string()).is_err());
        let spoof_native=json!({"version":1,"plugins":[{
            "id":"untrusted_extra","name":"Fake native app",
            "source":{"kind":"native_app"},"enabled":true
        }]});
        assert!(AndroidPluginRegistry::from_config(&spoof_native.to_string()).is_err());
    }

    #[test]
    fn curated_migration_preserves_legacy_alias_generation_and_disables_new_modules() {
        let mut legacy=AndroidPluginRegistry::new();
        legacy.add_plugin(browser(true)).unwrap();
        legacy.add_plugin(app(false)).unwrap();
        let prior:Value=serde_json::from_str(&legacy.export_config().unwrap()).unwrap();
        let migrated=AndroidPluginRegistry::from_config(&prior.to_string()).unwrap();
        let after:Value=serde_json::from_str(&migrated.export_config().unwrap()).unwrap();
        assert_eq!(prior["nativeGenerations"]["browser"],after["nativeGenerations"]["browser"]);
        assert_eq!(prior["nativeGenerations"]["app_control"],after["nativeGenerations"]["app_control"]);
        assert_eq!(migrated.list().len(),4);
        assert!(migrated.list().iter().find(|s|s.id=="browser").unwrap().enabled);
        for id in ["security_audit","reverse_engineering"] {
            assert!(!migrated.list().iter().find(|s|s.id==id).unwrap().enabled);
            assert!(after["nativeGenerations"][id].is_string());
        }
        let rebooted=AndroidPluginRegistry::from_config(&after.to_string()).unwrap();
        assert_eq!(rebooted.export_config().unwrap(),migrated.export_config().unwrap());
        assert!(rebooted.tools_list().is_empty());
        let mut spoof=after.clone();
        spoof["plugins"][2]["source"]=json!({"kind":"native_app"});
        assert!(AndroidPluginRegistry::from_config(&spoof.to_string()).is_err());
        let mut revoked=after;
        revoked["nativeGenerations"]["security_audit"]="missing".into();
        assert!(AndroidPluginRegistry::from_config(&revoked.to_string()).is_err());
    }

    #[test]
    fn screenshot_mode_exposes_actions_but_never_semantic_text_fill() {
        let receipt = json!({"supported":true,"connected":true,"active":true,
            "visible":true,"stopRequested":false,"targetPackage":"com.example.app",
            "automationActionsAvailable":true,"selectedAppMode":"screenshot_click"});
        let names = native_tool_names(&PluginSource::NativeApp, &receipt).unwrap();
        assert!(names.contains(&"android_app_snapshot".to_owned()));
        assert!(names.contains(&"android_app_click".to_owned()));
        assert!(names.contains(&"android_app_gesture".to_owned()));
        assert!(!names.contains(&"android_app_fill".to_owned()));
        let mut unavailable = receipt;
        unavailable["automationActionsAvailable"] = json!(false);
        let blocked = native_tool_names(&PluginSource::NativeApp, &unavailable).unwrap();
        assert!(!blocked.contains(&"android_app_snapshot".to_owned()));
        assert!(!blocked.contains(&"android_app_click".to_owned()));
    }

    struct RealStatusHost {
        status_count: AtomicUsize,
        command_count: AtomicUsize,
        supported: AtomicBool,
    }
    impl AndroidLocalToolHost for RealStatusHost {
        fn execute<'a>(
            &'a self,
            name: &'a str,
            _args: Value,
        ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
            Box::pin(async move {
                match name {
                    "android_browser_status" => {
                        self.status_count.fetch_add(1, Ordering::SeqCst);
                        Ok(
                            json!({"supported":self.supported.load(Ordering::SeqCst),"running":true,"ready":true,
                            "profileIsolated":true,"title":"Ready browser"}),
                        )
                    }
                    "android_app_status" => {
                        self.status_count.fetch_add(1, Ordering::SeqCst);
                        Ok(json!({"supported":self.supported.load(Ordering::SeqCst),
                            "connected":true,"active":true,"visible":true,
                            "stopRequested":false,"targetPackage":"com.example.app",
                            "automationActionsAvailable":true}))
                    }
                    _ => {
                        self.command_count.fetch_add(1, Ordering::SeqCst);
                        Err("Unexpected native action reached the host".into())
                    }
                }
            })
        }
    }

    struct MutableActionHost {
        browser_running: AtomicBool,
        browser_ready: AtomicBool,
        app_connected: AtomicBool,
        app_active: AtomicBool,
        action_count: AtomicUsize,
    }

    impl MutableActionHost {
        fn new() -> Self {
            Self {
                browser_running: AtomicBool::new(false),
                browser_ready: AtomicBool::new(false),
                app_connected: AtomicBool::new(true),
                app_active: AtomicBool::new(false),
                action_count: AtomicUsize::new(0),
            }
        }
    }

    impl AndroidLocalToolHost for MutableActionHost {
        fn execute<'a>(
            &'a self,
            name: &'a str,
            args: Value,
        ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
            Box::pin(async move {
                match name {
                    "android_browser_status" => {
                        let running = self.browser_running.load(Ordering::SeqCst);
                        Ok(json!({"supported":true,"running":running,
                            "ready":running && self.browser_ready.load(Ordering::SeqCst),
                            "profileIsolated":running,"title":"Test browser"}))
                    }
                    "android_browser_start" if args["url"] == "https://example.org/a" => {
                        self.action_count.fetch_add(1, Ordering::SeqCst);
                        self.browser_running.store(true, Ordering::SeqCst);
                        Ok(json!({"supported":true,"running":true,"ready":false,
                            "profileIsolated":true,"title":""}))
                    }
                    "android_browser_stop" => {
                        self.action_count.fetch_add(1, Ordering::SeqCst);
                        self.browser_running.store(false, Ordering::SeqCst);
                        self.browser_ready.store(false, Ordering::SeqCst);
                        Ok(json!({"supported":true,"running":false,"ready":false,
                            "profileIsolated":false,"title":""}))
                    }
                    "android_browser_snapshot" => {
                        self.action_count.fetch_add(1, Ordering::SeqCst);
                        Ok(json!({"snapshotId":11,"text":"Ready","title":"Browser",
                            "elements":[{"index":1,"fillable":true,"label":"Input","tag":"INPUT"}]}))
                    }
                    "android_browser_click" | "android_browser_fill"
                        if args["snapshotId"] == 11 && args["index"] == 1 =>
                    {
                        self.action_count.fetch_add(1, Ordering::SeqCst);
                        Ok(json!({"ok":true,"snapshotConsumed":true}))
                    }
                    "android_app_status" => {
                        let active = self.app_active.load(Ordering::SeqCst);
                        Ok(json!({"supported":true,
                            "connected":self.app_connected.load(Ordering::SeqCst),
                            "active":active,"visible":active,"stopRequested":false,
                            "targetPackage":"com.example.app"}))
                    }
                    "android_app_start" if args["targetPackage"] == "com.example.app" => {
                        self.action_count.fetch_add(1, Ordering::SeqCst);
                        self.app_active.store(true, Ordering::SeqCst);
                        Ok(json!({"supported":true,"connected":true,"active":true,
                            "visible":true,"stopRequested":false,"launched":true,
                            "targetPackage":"com.example.app"}))
                    }
                    "android_app_stop" => {
                        self.action_count.fetch_add(1, Ordering::SeqCst);
                        self.app_active.store(false, Ordering::SeqCst);
                        Ok(json!({"supported":true,"connected":true,"active":false,
                            "visible":false,"stopRequested":true,"targetPackage":""}))
                    }
                    _ => Err("Host rejected invalid or unsupported action".into()),
                }
            })
        }
    }

    fn alias_of(registry: &AndroidPluginRegistry, original_name: &str) -> String {
        registry
            .tools_list()
            .iter()
            .find_map(|tool| {
                let alias = tool["name"].as_str()?;
                registry
                    .find_tool(alias)
                    .ok()
                    .and_then(|(_, name, _)| (name == original_name).then(|| alias.to_owned()))
            })
            .unwrap_or_else(|| panic!("Verified tool {original_name} not advertised"))
    }

    fn browser(enabled: bool) -> PluginSpec {
        PluginSpec {
            id: "browser".into(),
            name: "App browser".into(),
            source: PluginSource::NativeBrowser,
            enabled,
        }
    }
    fn app(enabled: bool) -> PluginSpec {
        PluginSpec {
            id: "app_control".into(),
            name: "Android app control".into(),
            source: PluginSource::NativeApp,
            enabled,
        }
    }
    fn remote(id: &str, enabled: bool) -> PluginSpec {
        PluginSpec {
            id: id.into(),
            name: "Remote verified MCP".into(),
            source: PluginSource::RemoteMcp {
                tunnel_id: format!("plugins_{id}_tunnel"),
            },
            enabled,
        }
    }

    #[test]
    fn enabling_or_reloading_a_plugin_never_invents_ready_tools_or_tunnel() {
        let mut registry = AndroidPluginRegistry::new();
        registry.add_plugin(browser(false)).unwrap();
        registry.add_plugin(remote("remote1", true)).unwrap();
        assert!(registry.tools_list().is_empty());
        registry.set_enabled("browser", true).unwrap();
        assert!(registry.tools_list().is_empty());
        assert_eq!(registry.list()[0].state, "enabled_waiting_for_verification");
        assert!(!registry.list()[1].plugins_tunnel_verified);
        let saved = registry.export_config().unwrap();
        assert!(saved.contains("\"version\":2"));
        assert!(saved.contains("nativeGenerations"));
        assert!(saved.contains("plugins_remote1_tunnel"));
        assert!(!saved.contains("bearer"));
        let restored = AndroidPluginRegistry::from_config(&saved).unwrap();
        assert!(restored.tools_list().is_empty());
        assert!(restored.list().iter().all(|item| !item.tools_available));
        assert!(AndroidPluginRegistry::from_config(
            &saved.replace("\"version\":2", "\"version\":3")
        )
        .is_err());
    }

    #[test]
    fn catalog_capacity_duplicate_and_tunnel_constraints_are_fail_closed() {
        let mut registry = AndroidPluginRegistry::new();
        registry.add_plugin(browser(false)).unwrap();
        assert!(registry.add_plugin(browser(true)).is_err());
        assert!(registry
            .add_plugin(PluginSpec {
                id: "BAD ID".into(),
                ..browser(false)
            })
            .is_err());
        assert!(registry
            .add_plugin(PluginSpec {
                id: "remote2".into(),
                name: "No tunnel".into(),
                source: PluginSource::RemoteMcp {
                    tunnel_id: "".into()
                },
                enabled: true
            })
            .is_err());
        for i in 1..MAX_PLUGINS {
            registry
                .add_plugin(remote(&format!("remote{i}"), true))
                .unwrap();
        }
        assert!(registry.add_plugin(remote("overflow", false)).is_err());
        let before = registry.export_config().unwrap();
        assert!(registry.set_enabled("missing", true).is_err());
        assert_eq!(registry.export_config().unwrap(), before);
    }

    #[tokio::test]
    async fn verified_native_probe_advertises_supported_actions_but_requires_approval() {
        let mut registry = AndroidPluginRegistry::new();
        registry.add_plugin(browser(true)).unwrap();
        let host = Arc::new(RealStatusHost {
            status_count: AtomicUsize::new(0),
            command_count: AtomicUsize::new(0),
            supported: AtomicBool::new(true),
        });
        assert!(registry.tools_list().is_empty());
        assert_eq!(
            registry
                .verify_native("browser", host.clone())
                .await
                .unwrap(),
            6
        );
        let tools = registry.tools_list();
        assert_eq!(tools.len(), 6); // Fixed supported catalog, including idle-only start.
        assert_eq!(registry.list()[0].ready_tool_count, 5);
        assert_eq!(
            registry
                .verify_native("browser", host.clone())
                .await
                .unwrap(),
            6
        );
        assert_eq!(
            registry.tools_list(),
            tools,
            "Passive re-probe must not rotate aliases"
        );
        assert!(tools
            .iter()
            .all(|tool| !tool["name"].as_str().unwrap().contains("web_fetch")));
        let alias = tools[0]["name"].as_str().unwrap().to_owned();
        assert!(tools[0]["description"]
            .as_str()
            .unwrap()
            .contains("Check the state"));
        assert!(registry
            .execute_tool(&alias, json!({"url":"https://EXAMPLE.ORG/a"}))
            .await
            .is_err());
        assert!(registry
            .execute_tool(
                "android_browser_start",
                json!({"url":"https://EXAMPLE.ORG/a"})
            )
            .await
            .is_err());
        let stop_alias = alias_of(&registry, "android_browser_stop");
        assert_eq!(
            registry
                .inspect_tool(&stop_alias, &json!({}))
                .unwrap()
                .original_name,
            "android_browser_stop"
        );
        assert!(registry.execute_tool(&stop_alias, json!({})).await.is_err());
        let reply = registry.execute_tool(&alias, json!({})).await.unwrap();
        assert_eq!(reply["running"], true);
        assert!(host.status_count.load(Ordering::SeqCst) >= 3);
        assert_eq!(host.command_count.load(Ordering::SeqCst), 0);
        registry.set_enabled("browser", false).unwrap();
        assert!(registry.tools_list().is_empty());
        assert!(registry.execute_tool(&alias, json!({})).await.is_err());
        registry.set_enabled("browser", true).unwrap();
        assert!(registry.tools_list().is_empty());
        registry.verify_native("browser", host).await.unwrap();
        assert_ne!(registry.tools_list()[0]["name"], alias);
    }

    #[tokio::test]
    async fn a_failed_native_reprobe_revokes_old_verified_tools() {
        let mut registry = AndroidPluginRegistry::new();
        registry.add_plugin(browser(true)).unwrap();
        let host = Arc::new(RealStatusHost {
            status_count: AtomicUsize::new(0),
            command_count: AtomicUsize::new(0),
            supported: AtomicBool::new(true),
        });
        registry
            .verify_native("browser", host.clone())
            .await
            .unwrap();
        let old_alias = registry.tools_list()[0]["name"]
            .as_str()
            .unwrap()
            .to_owned();
        let snapshot = alias_of(&registry, "android_browser_snapshot");
        host.supported.store(false, Ordering::SeqCst);
        assert!(
            registry
                .execute_approved_tool(&snapshot, json!({}))
                .await
                .is_err(),
            "A live unsupported OS status must block even a previously approved alias"
        );
        assert_eq!(
            host.command_count.load(Ordering::SeqCst),
            0,
            "Unsupported status must not dispatch the native snapshot"
        );
        assert!(registry
            .verify_native("browser", host.clone())
            .await
            .is_err());
        assert!(registry.tools_list().is_empty());
        assert!(!registry.list()[0].tools_available);
        assert!(registry.execute_tool(&old_alias, json!({})).await.is_err());
        host.supported.store(true, Ordering::SeqCst);
        registry.verify_native("browser", host).await.unwrap();
        assert_eq!(
            registry.tools_list()[0]["name"],
            old_alias,
            "Transient OS reprobe failure must not permanently rotate a persisted alias"
        );
    }

    #[tokio::test]
    async fn supported_browser_aliases_work_across_start_ready_stop_without_relisting() {
        let mut registry = AndroidPluginRegistry::new();
        registry.add_plugin(browser(true)).unwrap();
        let host = Arc::new(MutableActionHost::new());
        assert_eq!(
            registry
                .verify_native("browser", host.clone())
                .await
                .unwrap(),
            6
        );
        assert_eq!(registry.list()[0].ready_tool_count, 2);
        let original_list = registry.tools_list();
        assert_eq!(original_list.len(), 6);
        let action_names = [
            "android_browser_status",
            "android_browser_start",
            "android_browser_snapshot",
            "android_browser_click",
            "android_browser_fill",
            "android_browser_stop",
        ];
        let aliases: Vec<String> = action_names
            .iter()
            .map(|original| alias_of(&registry, original))
            .collect();
        let unique = aliases.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(
            unique.len(),
            action_names.len(),
            "Every original native action must have a unique alias"
        );
        for (alias, original) in aliases.iter().zip(action_names) {
            assert_eq!(
                registry.find_tool(alias).unwrap().1,
                original,
                "An advertised alias must resolve to exactly its original action"
            );
        }
        let (start, snapshot, click, fill, stop) = (
            &aliases[1],
            &aliases[2],
            &aliases[3],
            &aliases[4],
            &aliases[5],
        );
        // Published capability does not make currently unready actions usable.
        assert!(registry
            .execute_approved_tool(stop, json!({}))
            .await
            .is_err());
        assert!(registry
            .execute_approved_tool(snapshot, json!({}))
            .await
            .is_err());
        assert_eq!(host.action_count.load(Ordering::SeqCst), 0);
        assert!(
            registry
                .execute_tool(start, json!({"url":"https://example.org/a"}))
                .await
                .is_err(),
            "Even currently ready actions still require separate approval"
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 0);

        let url = json!({"url":"https://example.org/a"});
        assert_eq!(
            registry
                .execute_approved_tool(start, url.clone())
                .await
                .unwrap()["running"],
            true
        );
        assert!(
            registry
                .execute_approved_tool(snapshot, json!({}))
                .await
                .is_err(),
            "Browser is running but not yet ready"
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            registry
                .execute_approved_tool(stop, json!({}))
                .await
                .unwrap()["running"],
            false,
            "The originally advertised stop alias must work without tools/list refresh"
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 2);

        assert!(registry.execute_approved_tool(start, url).await.is_ok());
        host.browser_ready.store(true, Ordering::SeqCst);
        assert_eq!(
            registry
                .execute_approved_tool(snapshot, json!({}))
                .await
                .unwrap()["snapshotId"],
            11
        );
        assert_eq!(
            registry
                .execute_approved_tool(click, json!({"snapshotId":11,"index":1}))
                .await
                .unwrap()["ok"],
            true
        );
        assert_eq!(
            registry
                .execute_approved_tool(fill, json!({"snapshotId":11,"index":1,"value":"safe"}))
                .await
                .unwrap()["ok"],
            true
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 6);
        assert!(
            registry
                .execute_approved_tool(start, json!({"url":"https://example.org/a"}))
                .await
                .is_err(),
            "Start must be unavailable while the browser is running"
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 6);
        assert_eq!(
            registry
                .execute_approved_tool(stop, json!({}))
                .await
                .unwrap()["running"],
            false
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 7);

        assert_eq!(
            registry.tools_list(),
            original_list,
            "The published capability catalog must remain stable across OS transitions"
        );
        for (alias, original) in aliases.iter().zip(action_names) {
            assert_eq!(
                registry.find_tool(alias).unwrap().1,
                original,
                "A cached alias must never silently resolve to a different operation"
            );
        }
        assert_eq!(registry.verify_native("browser", host).await.unwrap(), 6);
        assert_eq!(registry.list()[0].ready_tool_count, 2);
        assert_eq!(
            registry.tools_list(),
            original_list,
            "A harmless native re-probe must not invalidate cached tool aliases"
        );
    }

    #[test]
    fn native_identity_aliases_fit_mcp_limit_with_maximal_plugin_id() {
        let mut registry = AndroidPluginRegistry::new();
        let mut spec = browser(true);
        spec.id = "b".repeat(32);
        registry.add_plugin(spec).unwrap();
        let entry = registry.entry(&"b".repeat(32)).unwrap();
        let aliases = native_capability_names(&PluginSource::NativeBrowser)
            .iter()
            .map(|name| entry.model_alias(0, name).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(aliases.len(), 6);
        assert_eq!(
            aliases
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            6
        );
        assert!(aliases.iter().all(|alias| alias.len() <= 64));
        // Remote MCP retains its pre-existing generation + ordinal naming.
        let remote = Entry {
            spec: remote("remote1", true),
            generation: Uuid::new_v4(),
            runtime: None,
        };
        let token = remote.generation.simple().to_string();
        assert_eq!(
            remote.model_alias(3, "any_verified_remote_name").unwrap(),
            format!("p_remote1_{}_3", &token[..16])
        );
    }

    #[tokio::test]
    async fn native_restart_reuses_persisted_alias_but_rebind_and_revocation_rotate() {
        let mut registry = AndroidPluginRegistry::new();
        registry.add_plugin(browser(true)).unwrap();
        let first_host = Arc::new(MutableActionHost::new());
        registry
            .verify_native("browser", first_host.clone())
            .await
            .unwrap();
        let original = alias_of(&registry, "android_browser_start");

        let saved = registry.export_config().unwrap();
        let mut reopened = AndroidPluginRegistry::from_config(&saved).unwrap();
        assert!(
            reopened.tools_list().is_empty(),
            "Restored configuration cannot attest a live Android capability"
        );
        reopened
            .verify_native("browser", Arc::new(MutableActionHost::new()))
            .await
            .unwrap();
        let after_restart = alias_of(&reopened, "android_browser_start");
        assert_eq!(
            after_restart, original,
            "A new process must retain a saved native alias after a genuine fresh OS probe"
        );
        assert!(reopened
            .inspect_tool(&original, &json!({"url":"https://example.org/a"}))
            .is_ok());

        let second_host = Arc::new(MutableActionHost::new());
        reopened
            .verify_native("browser", second_host.clone())
            .await
            .unwrap();
        let after_rebind = alias_of(&reopened, "android_browser_start");
        assert_ne!(
            after_rebind, after_restart,
            "Binding to a different native host revokes the previous identity"
        );
        assert!(reopened
            .inspect_tool(&after_restart, &json!({"url":"https://example.org/a"}))
            .is_err());

        reopened.invalidate_runtime("browser").unwrap();
        assert!(reopened.tools_list().is_empty());
        assert!(reopened
            .inspect_tool(&after_rebind, &json!({"url":"https://example.org/a"}))
            .is_err());
        reopened
            .verify_native("browser", second_host)
            .await
            .unwrap();
        assert_ne!(
            alias_of(&reopened, "android_browser_start"),
            after_rebind,
            "Explicit runtime revocation must force a new verified alias generation"
        );
    }

    #[tokio::test]
    async fn v1_migrates_enabled_settings_to_v2_without_restoring_authority() {
        let legacy =
            json!({"version":1,"plugins":[browser(true),app(true),remote("remote1",true)]});
        let mut restored = AndroidPluginRegistry::from_config(&legacy.to_string()).unwrap();
        assert!(restored.list().iter().take(3).all(|plugin| plugin.enabled));
        assert!(restored.list().iter().skip(3).all(|plugin| !plugin.enabled));
        assert_eq!(
            restored.tools_list(),
            Vec::<Value>::new(),
            "Enabled settings in v1 do not convey a runtime capability"
        );
        let migrated = restored.export_config().unwrap();
        let persisted: Value = serde_json::from_str(&migrated).unwrap();
        assert_eq!(persisted["version"], 2);
        assert_eq!(persisted["plugins"].as_array().unwrap().len(), 5);
        assert_eq!(persisted["nativeGenerations"].as_object().unwrap().len(), 4);
        assert!(
            persisted["nativeGenerations"]["remote1"].is_null(),
            "Remote MCP aliases are deliberately not persisted"
        );
        assert!(!migrated.contains("runtime"));
        assert!(!migrated.contains("approved"));
        assert!(!migrated.contains("ready"));
        assert!(!migrated.contains("bearer"));
        let mut rebooted = AndroidPluginRegistry::from_config(&migrated).unwrap();
        assert!(rebooted.tools_list().is_empty());
        let first_host = Arc::new(MutableActionHost::new());
        restored.verify_native("browser", first_host).await.unwrap();
        let second_host = Arc::new(MutableActionHost::new());
        rebooted
            .verify_native("browser", second_host)
            .await
            .unwrap();
        assert_eq!(
            alias_of(&restored, "android_browser_status"),
            alias_of(&rebooted, "android_browser_status")
        );
    }

    #[tokio::test]
    async fn cached_app_status_alias_executes_after_verified_restart_without_relisting() {
        let mut original = AndroidPluginRegistry::new();
        original.add_plugin(app(true)).unwrap();
        let host1 = Arc::new(RealStatusHost {
            status_count: AtomicUsize::new(0),
            command_count: AtomicUsize::new(0),
            supported: AtomicBool::new(true),
        });
        original.verify_native("app_control", host1).await.unwrap();
        let old_status = alias_of(&original, "android_app_status");
        let saved = original.export_config().unwrap();
        let mut rebooted = AndroidPluginRegistry::from_config(&saved).unwrap();
        assert!(
            rebooted.tools_list().is_empty(),
            "Reloading saved catalog must never restore OS runtime permissions"
        );
        let host2 = Arc::new(RealStatusHost {
            status_count: AtomicUsize::new(0),
            command_count: AtomicUsize::new(0),
            supported: AtomicBool::new(true),
        });
        rebooted
            .verify_native("app_control", host2.clone())
            .await
            .unwrap();
        let result = rebooted.execute_tool(&old_status, json!({})).await.unwrap();
        assert_eq!(result["active"], true);
        assert_eq!(result["connected"], true);
        assert_eq!(alias_of(&rebooted, "android_app_status"), old_status);
        assert_eq!(
            host2.command_count.load(Ordering::SeqCst),
            0,
            "Read-only cached status must not trigger an action"
        );
        assert!(host2.status_count.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn persisted_revoke_and_disabled_transition_never_reanimate_old_alias() {
        let mut catalog = AndroidPluginRegistry::new();
        catalog.add_plugin(browser(true)).unwrap();
        catalog
            .verify_native("browser", Arc::new(MutableActionHost::new()))
            .await
            .unwrap();
        let before = alias_of(&catalog, "android_browser_status");
        let original = catalog.export_config().unwrap();

        catalog.set_enabled("browser", false).unwrap();
        let disabled = catalog.export_config().unwrap();
        let mut rebooted = AndroidPluginRegistry::from_config(&disabled).unwrap();
        assert!(!rebooted.list()[0].enabled);
        assert!(rebooted.inspect_tool(&before, &json!({})).is_err());
        rebooted.set_enabled("browser", true).unwrap();
        let reenabled = rebooted.export_config().unwrap();
        let mut reopened = AndroidPluginRegistry::from_config(&reenabled).unwrap();
        assert!(reopened.tools_list().is_empty());
        reopened
            .verify_native("browser", Arc::new(MutableActionHost::new()))
            .await
            .unwrap();
        assert_ne!(alias_of(&reopened, "android_browser_status"), before);
        assert!(reopened.inspect_tool(&before, &json!({})).is_err());

        // Explicit runtime revocation also persists even if the plugin stays enabled.
        let current = alias_of(&reopened, "android_browser_status");
        reopened.invalidate_runtime("browser").unwrap();
        let revoked = reopened.export_config().unwrap();
        let mut third = AndroidPluginRegistry::from_config(&revoked).unwrap();
        third
            .verify_native("browser", Arc::new(MutableActionHost::new()))
            .await
            .unwrap();
        assert_ne!(alias_of(&third, "android_browser_status"), current);
        assert!(third.inspect_tool(&current, &json!({})).is_err());
        assert_ne!(original, revoked);
    }

    #[test]
    fn native_generation_config_rejects_missing_unknown_or_forged_identity() {
        let mut registry = AndroidPluginRegistry::new();
        registry.add_plugin(browser(true)).unwrap();
        let valid: Value = serde_json::from_str(&registry.export_config().unwrap()).unwrap();
        let mut missing = valid.clone();
        missing["nativeGenerations"] = json!({});
        assert!(AndroidPluginRegistry::from_config(&missing.to_string()).is_err());
        let mut unknown = valid.clone();
        unknown["nativeGenerations"]["other"] = json!(Uuid::new_v4().to_string());
        assert!(AndroidPluginRegistry::from_config(&unknown.to_string()).is_err());
        let mut bad_uuid = valid.clone();
        bad_uuid["nativeGenerations"]["browser"] = json!("00000000-0000-0000-0000-000000000000");
        assert!(AndroidPluginRegistry::from_config(&bad_uuid.to_string()).is_err());
        let mut injected = valid.clone();
        injected["runtimeReady"] = json!(true);
        assert!(AndroidPluginRegistry::from_config(&injected.to_string()).is_err());
        let mut extra_v1 = json!({"version":1,"plugins":[browser(true)],"nativeGenerations":{}});
        assert!(AndroidPluginRegistry::from_config(&extra_v1.to_string()).is_err());
        extra_v1
            .as_object_mut()
            .unwrap()
            .remove("nativeGenerations");
        assert!(AndroidPluginRegistry::from_config(&extra_v1.to_string()).is_ok());
    }

    #[tokio::test]
    async fn plugins_native_aliases_cannot_call_actions_even_when_host_is_ready() {
        for (spec, id, unsafe_tools) in [
            (
                browser(true),
                "browser",
                &[
                    "android_browser_start",
                    "android_browser_stop",
                    "android_browser_snapshot",
                    "android_browser_click",
                    "android_browser_fill",
                ][..],
            ),
            (
                app(true),
                "app_control",
                &[
                    "android_app_start",
                    "android_app_stop",
                    "android_app_snapshot",
                    "android_app_click",
                    "android_app_fill",
                    "android_app_gesture",
                ][..],
            ),
        ] {
            let mut registry = AndroidPluginRegistry::new();
            registry.add_plugin(spec).unwrap();
            let host = Arc::new(RealStatusHost {
                status_count: AtomicUsize::new(0),
                command_count: AtomicUsize::new(0),
                supported: AtomicBool::new(true),
            });
            let expected = if id == "browser" { 6 } else { 7 };
            assert_eq!(
                registry.verify_native(id, host.clone()).await.unwrap(),
                expected
            );
            let tools = registry.tools_list();
            assert_eq!(
                tools.len(),
                expected,
                "{id}: host exposes supported native capabilities"
            );
            let status_alias = tools[0]["name"].as_str().unwrap().to_owned();
            assert!(registry
                .execute_tool(&status_alias, json!({}))
                .await
                .is_ok());

            let action_args = |name: &str| match name {
                "android_browser_start" => json!({"url":"https://example.org"}),
                "android_browser_click" => json!({"snapshotId":1,"index":1}),
                "android_browser_fill" => json!({"snapshotId":1,"index":1,"value":"safe"}),
                "android_app_start" => json!({"targetPackage":"com.example.app"}),
                "android_app_click" => {
                    json!({"snapshotId":"00000000-0000-4000-8000-000000000000","index":1})
                }
                "android_app_fill" => {
                    json!({"snapshotId":"00000000-0000-4000-8000-000000000000","index":1,"value":"safe"})
                }
                "android_app_gesture" => {
                    json!({"snapshotId":"00000000-0000-4000-8000-000000000000",
                    "kind":"tap","x":10,"y":10,"toX":10,"toY":10,"durationMs":100})
                }
                _ => json!({}),
            };
            for name in unsafe_tools {
                assert!(
                    registry
                        .execute_tool(name, action_args(name))
                        .await
                        .is_err(),
                    "{id}: direct action name must fail"
                );
            }
            for alias in tools.iter().skip(1) {
                let alias = alias["name"].as_str().unwrap();
                let (_, name, _) = registry.find_tool(alias).unwrap();
                let args = action_args(&name);
                assert!(
                    registry.inspect_tool(alias, &args).is_ok(),
                    "{id}: action is available for separate approval"
                );
                assert!(
                    registry.execute_tool(alias, args).await.is_err(),
                    "{id}: unapproved action alias must not dispatch"
                );
            }
            assert_eq!(
                host.command_count.load(Ordering::SeqCst),
                0,
                "{id}: no action may reach the Android host"
            );

            registry.set_enabled(id, false).unwrap();
            assert!(registry
                .execute_tool(&status_alias, json!({}))
                .await
                .is_err());
            for action in tools.iter().skip(1) {
                let stale = action["name"].as_str().unwrap();
                assert!(registry.inspect_tool(stale, &json!({})).is_err());
                assert!(registry
                    .execute_approved_tool(stale, json!({}))
                    .await
                    .is_err());
            }
            registry.set_enabled(id, true).unwrap();
            registry.verify_native(id, host.clone()).await.unwrap();
            assert_ne!(registry.tools_list()[0]["name"], status_alias);
            assert!(
                registry
                    .execute_tool(&status_alias, json!({}))
                    .await
                    .is_err(),
                "{id}: stale status alias must not become valid again"
            );
            assert_eq!(host.command_count.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn approved_browser_actions_recheck_schema_runtime_and_binding() {
        let mut registry = AndroidPluginRegistry::new();
        registry.add_plugin(browser(true)).unwrap();
        let host = Arc::new(MutableActionHost::new());
        assert_eq!(
            registry
                .verify_native("browser", host.clone())
                .await
                .unwrap(),
            6
        );
        assert_eq!(registry.tools_list().len(), 6); // Capability count stays stable when idle.
        assert_eq!(registry.list()[0].ready_tool_count, 2); // Only status + start are ready.
        let start = alias_of(&registry, "android_browser_start");
        let valid = json!({"url":"HTTPS://example.org/a"});
        let info = registry.inspect_tool(&start, &valid).unwrap();
        assert_eq!(info.plugin_id, "browser");
        assert_eq!(info.plugin_name, "App browser");
        assert_eq!(info.original_name, "android_browser_start");
        assert_eq!(info.source, PluginSource::NativeBrowser);
        let _copy = info.clone();

        // A valid proposal and even a forged approval field cannot execute in
        // the old unapproved inbound RPC path.
        assert!(registry.execute_tool(&start, valid.clone()).await.is_err());
        assert!(registry
            .inspect_tool(&start, &json!({"url":"http://example.org/a"}))
            .is_err());
        assert!(registry.inspect_tool(&start, &json!({"url":33})).is_err());
        assert!(registry
            .inspect_tool(
                &start,
                &json!({"url":"https://example.org/a","alwaysAllow":true})
            )
            .is_err());
        assert!(registry
            .execute_approved_tool(&start, json!({"url":"http://example.org/a"}))
            .await
            .is_err());
        assert_eq!(host.action_count.load(Ordering::SeqCst), 0);

        let started = registry.execute_approved_tool(&start, valid).await.unwrap();
        assert_eq!(started["running"], true);
        assert_eq!(host.action_count.load(Ordering::SeqCst), 1);
        assert!(
            registry
                .execute_approved_tool(&start, json!({"url":"https://example.org/a"}))
                .await
                .is_err(),
            "A start alias becomes unavailable once browser is running"
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 1);

        assert_eq!(
            registry
                .verify_native("browser", host.clone())
                .await
                .unwrap(),
            6
        );
        assert_eq!(
            alias_of(&registry, "android_browser_start"),
            start,
            "Reverification on state transition must retain the action's identity"
        );
        assert_eq!(registry.list()[0].ready_tool_count, 2); // Running, but not yet ready.
        assert!(registry
            .execute_approved_tool(&start, json!({"url":"https://example.org/a"}))
            .await
            .is_err());

        host.browser_ready.store(true, Ordering::SeqCst);
        assert_eq!(
            registry
                .verify_native("browser", host.clone())
                .await
                .unwrap(),
            6
        );
        assert_eq!(registry.list()[0].ready_tool_count, 5);
        assert_eq!(alias_of(&registry, "android_browser_start"), start);
        let snapshot = alias_of(&registry, "android_browser_snapshot");
        let click = alias_of(&registry, "android_browser_click");
        let fill = alias_of(&registry, "android_browser_fill");
        let stop = alias_of(&registry, "android_browser_stop");
        assert!(registry
            .inspect_tool(&click, &json!({"snapshotId":11,"index":0}))
            .is_err());
        assert!(registry
            .inspect_tool(
                &fill,
                &json!({"snapshotId":11,"index":1,"value":"bad\nvalue"})
            )
            .is_err());
        assert!(registry.execute_tool(&snapshot, json!({})).await.is_err());
        assert!(registry
            .execute_tool(&click, json!({"snapshotId":11,"index":1}))
            .await
            .is_err());
        assert_eq!(host.action_count.load(Ordering::SeqCst), 1);

        assert_eq!(
            registry
                .execute_approved_tool(&snapshot, json!({}))
                .await
                .unwrap()["snapshotId"],
            11
        );
        assert_eq!(
            registry
                .execute_approved_tool(&click, json!({"snapshotId":11,"index":1}))
                .await
                .unwrap()["ok"],
            true
        );
        assert_eq!(
            registry
                .execute_approved_tool(&fill, json!({"snapshotId":11,"index":1,"value":"safe"}))
                .await
                .unwrap()["ok"],
            true
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 4);

        host.browser_ready.store(false, Ordering::SeqCst);
        assert!(
            registry
                .execute_approved_tool(&click, json!({"snapshotId":11,"index":1}))
                .await
                .is_err(),
            "Live readiness loss must block action before host call"
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 4);
        assert_eq!(
            registry
                .verify_native("browser", host.clone())
                .await
                .unwrap(),
            6
        );
        assert_eq!(registry.list()[0].ready_tool_count, 2);
        assert_eq!(alias_of(&registry, "android_browser_snapshot"), snapshot);
        assert_eq!(alias_of(&registry, "android_browser_stop"), stop);
        assert!(
            registry
                .execute_approved_tool(&snapshot, json!({}))
                .await
                .is_err(),
            "A still-advertised snapshot must fail closed when the browser is not ready"
        );
        let fresh_stop = alias_of(&registry, "android_browser_stop");
        assert_eq!(
            fresh_stop, stop,
            "Stop alias must remain stable until explicit revocation"
        );
        assert_eq!(
            registry
                .execute_approved_tool(&fresh_stop, json!({}))
                .await
                .unwrap()["running"],
            false
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 5);
        assert!(registry
            .execute_approved_tool(&fresh_stop, json!({}))
            .await
            .is_err());

        registry.set_enabled("browser", false).unwrap();
        assert!(registry.inspect_tool(&fresh_stop, &json!({})).is_err());
        assert!(registry
            .execute_approved_tool(&fresh_stop, json!({}))
            .await
            .is_err());
        assert_eq!(host.action_count.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn approved_app_actions_require_live_service_and_revoke_stale_aliases() {
        let mut registry = AndroidPluginRegistry::new();
        registry.add_plugin(app(true)).unwrap();
        let host = Arc::new(MutableActionHost::new());
        assert_eq!(
            registry
                .verify_native("app_control", host.clone())
                .await
                .unwrap(),
            7
        );
        assert_eq!(registry.list()[0].ready_tool_count, 2);
        let start = alias_of(&registry, "android_app_start");
        let valid = json!({"targetPackage":"com.example.app"});
        assert_eq!(
            registry.inspect_tool(&start, &valid).unwrap().source,
            PluginSource::NativeApp
        );
        assert!(registry
            .inspect_tool(&start, &json!({"targetPackage":"com.android.settings"}))
            .is_err());
        assert!(registry.execute_tool(&start, valid.clone()).await.is_err());
        assert_eq!(host.action_count.load(Ordering::SeqCst), 0);

        host.app_connected.store(false, Ordering::SeqCst);
        assert!(
            registry
                .execute_approved_tool(&start, valid.clone())
                .await
                .is_err(),
            "Previously verified start cannot operate with service disconnected"
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 0);
        host.app_connected.store(true, Ordering::SeqCst);

        assert_eq!(
            registry
                .execute_approved_tool(&start, valid.clone())
                .await
                .unwrap()["active"],
            true
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            registry
                .verify_native("app_control", host.clone())
                .await
                .unwrap(),
            7
        );
        assert_eq!(alias_of(&registry, "android_app_start"), start);
        assert_eq!(registry.list()[0].ready_tool_count, 3);
        let stop = alias_of(&registry, "android_app_stop");
        assert!(registry.execute_tool(&stop, json!({})).await.is_err());
        assert_eq!(
            registry
                .execute_approved_tool(&stop, json!({}))
                .await
                .unwrap()["active"],
            false
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 2);
        assert!(
            registry
                .execute_approved_tool(&stop, json!({}))
                .await
                .is_err(),
            "Stop is unavailable after the app is inactive"
        );
        assert_eq!(host.action_count.load(Ordering::SeqCst), 2);

        registry.invalidate_runtime("app_control").unwrap();
        assert!(registry.inspect_tool(&stop, &json!({})).is_err());
        assert!(registry
            .execute_approved_tool(&stop, json!({}))
            .await
            .is_err());
        assert_eq!(host.action_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn remote_catalog_cannot_claim_tools_from_tunnel_id_or_unverified_json() {
        let mut registry = AndroidPluginRegistry::new();
        registry.add_plugin(remote("remote1", true)).unwrap();
        assert!(registry.tools_list().is_empty());
        assert!(!registry.list()[0].tools_available);
        assert!(!registry.list()[0].plugins_tunnel_verified);
        let malformed = r#"{"version":1,"plugins":[{"id":"remote1","name":"Remote MCP",
            "source":{"kind":"remote_mcp","tunnel_id":"plugins_abc123"},
            "enabled":true,"tools":[{"name":"invented_ready_tool"}]}]}"#;
        assert!(AndroidPluginRegistry::from_config(malformed).is_err());
        assert_eq!(registry.catalog()["toolCount"], 0);
    }
}
