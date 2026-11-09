//! Android remote MCP permissions. Tool exposure is the intersection of
//! explicit native user choices, the current OS folder grant and the
//! implemented, bounded tool handlers. No checkbox grants arbitrary shell
//! access or permission to read app credentials.

use crate::android_mcp_files::McpReadRoot;
use serde_json::{json, Map, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const CAPABILITY_KEYS: [&str; 10] = [
    "browseFolders",
    "searchFiles",
    "readFiles",
    "fileMetadata",
    "createFiles",
    "editFiles",
    "moveFiles",
    "deleteFiles",
    "browserControl",
    "terminal",
];
const READ_END: usize = 4;
const WRITE_END: usize = 8;
const BROWSER_INDEX: usize = 8;
const TERMINAL_INDEX: usize = 9;

const POLICY_FILE: &str = "mcp-tool-policy-v1.json";
const POLICY_MAX_BYTES: u64 = 4096;
const COMMAND_RULES_FILE: &str = "mcp-command-rules-v1.json";
const COMMAND_RULES_MAX_BYTES: u64 = 4096;
const COMMAND_RULES_MAX: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum CommandRuleMode {
    #[default]
    Allowlist,
    Denylist,
}
impl CommandRuleMode {
    fn label(self) -> &'static str {
        match self { Self::Allowlist => "allowlist", Self::Denylist => "denylist" }
    }
    fn parse(value: &str) -> Result<Self,String> {
        match value {
            "allowlist" => Ok(Self::Allowlist),
            "denylist" => Ok(Self::Denylist),
            _ => Err("Unknown command rule mode".into()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CommandRules {
    enabled: bool,
    mode: CommandRuleMode,
    rules: Vec<String>,
}
impl CommandRules {
    fn validate(enabled: bool, mode: &str, rules: Vec<String>) -> Result<Self,String> {
        let mode=CommandRuleMode::parse(mode)?;
        if rules.len()>COMMAND_RULES_MAX {return Err("Too many command rules".into())}
        let mut unique=std::collections::HashSet::new();
        for rule in &rules {
            let valid=matches!(rule.as_str(),"pwd"|"ls"|"ls *"|"cat *"|"stat *") ||
                ["ls ","cat ","stat "].iter().any(|prefix|{
                    rule.strip_prefix(prefix).is_some_and(|path|{
                        !path.is_empty() && path.len()<=512 &&
                        !path.starts_with('/') && !path.starts_with('.') &&
                        !path.contains(['\\',':','*','?','\n','\r','\0',';','|','&','>','<','"','\'']) &&
                        path.split('/').all(|s|!s.is_empty()&&s!="."&&s!=".."&&!s.starts_with('.'))
                    })
                });
            if !valid || !unique.insert(rule.as_str()) {
                return Err("Only unique pwd, ls, ls *, cat *, stat * or exact safe paths are supported".into())
            }
        }
        if enabled && mode==CommandRuleMode::Allowlist && rules.is_empty() {
            return Err("An enabled Allowlist needs at least one rule".into())
        }
        Ok(Self{enabled,mode,rules})
    }
    fn permits(&self,command:&str,path:Option<&str>) -> bool {
        if !self.enabled {return false}
        let invocation=match path {
            Some(path) => format!("{command} {path}"),
            None => command.to_owned(),
        };
        let matched=self.rules.iter().any(|rule|{
            rule==&invocation ||
                rule.strip_suffix(" *").is_some_and(|prefix|{
                    prefix==command && path.is_some_and(|p|!p.is_empty())
                })
        });
        match self.mode {
            CommandRuleMode::Allowlist => matched,
            CommandRuleMode::Denylist => !matched,
        }
    }
    fn json(&self,available:bool) -> Value {
        json!({"enabled":self.enabled,"mode":self.mode.label(),
            "rules":self.rules,"available":available,
            "supportedCommands":["pwd","ls","cat","stat"]})
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Capabilities([bool; 10]);

impl Capabilities {
    fn parse(value: &Value) -> Result<Self, String> {
        let fields = value
            .as_object()
            .ok_or_else(|| "MCP capabilities must be an object".to_owned())?;
        if fields.len() != CAPABILITY_KEYS.len() {
            return Err("MCP capabilities require exactly ten known boolean fields".into());
        }
        let mut result = [false; 10];
        for (index, key) in CAPABILITY_KEYS.iter().enumerate() {
            result[index] = fields
                .get(*key)
                .and_then(Value::as_bool)
                .ok_or_else(|| format!("Invalid MCP capability: {key}"))?;
        }
        Ok(Self(result))
    }
    fn to_value(self) -> Value {
        let mut result = Map::with_capacity(CAPABILITY_KEYS.len());
        for (key, enabled) in CAPABILITY_KEYS.iter().zip(self.0) {
            result.insert((*key).to_owned(), Value::Bool(enabled));
        }
        Value::Object(result)
    }
    fn effective(self, available: Self) -> Self {
        Self(std::array::from_fn(|i| self.0[i] && available.0[i]))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Preset {
    #[default]
    ReadOnly,
    FilesTerminal,
    Full,
    Custom,
}

impl Preset {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "read_only" => Ok(Self::ReadOnly),
            "files_terminal" => Ok(Self::FilesTerminal),
            "full" => Ok(Self::Full),
            "custom" => Ok(Self::Custom),
            _ => Err("Unknown Android MCP permission preset".into()),
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::FilesTerminal => "files_terminal",
            Self::Full => "full",
            Self::Custom => "custom",
        }
    }
    fn canonical(self) -> Option<Capabilities> {
        let mut bits = [false; 10];
        match self {
            Self::ReadOnly => bits[..READ_END].fill(true),
            Self::FilesTerminal => {
                bits[..WRITE_END].fill(true);
                bits[TERMINAL_INDEX] = true;
            }
            Self::Full => bits.fill(true),
            Self::Custom => return None,
        }
        Some(Capabilities(bits))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Selection {
    // The startup label is read_only but all requests default to DENIED.
    preset: Preset,
    requested: Capabilities,
}

impl Selection {
    fn snapshot(self, available: Capabilities) -> Value {
        json!({
            "preset":self.preset.label(),
            "requested":self.requested.to_value(),
            "effective":self.requested.effective(available).to_value(),
            "available":available.to_value(),
        })
    }
}

/// A cloneable gate shared by Tauri status/configuration and the inbound
/// tunnel. Only explicitly selected capability booleans (NOT credentials,
/// grants, tool output or request approvals) are persisted in the app's
/// private config. Every invocation revalidates OS authorization and scope.
#[derive(Clone, Default)]
pub struct AndroidMcpPermissions {
    state: Arc<Mutex<Selection>>,
    command_rules: Arc<Mutex<CommandRules>>,
    read_root: Option<Arc<McpReadRoot>>,
    policy_file: Option<PathBuf>,
    command_rules_file: Option<PathBuf>,
}

impl AndroidMcpPermissions {
    /// Root and settings are derived ONLY from the Tauri AppHandle's private
    /// directory. The optional Step4 shared-storage selection is independently
    /// verified via the native Kotlin plugin on each read, not provided by JS.
    pub fn for_app(app: tauri::AppHandle, app_data_dir: &Path) -> Self {
        let policy_file = app_data_dir.join(POLICY_FILE);
        let restored = load_selection(&policy_file).unwrap_or_default();
        Self {
            state: Arc::new(Mutex::new(restored)),
            command_rules: Arc::new(Mutex::new(
                load_command_rules(&app_data_dir.join(COMMAND_RULES_FILE)).unwrap_or_default())),
            read_root: Some(Arc::new(McpReadRoot::from_app_dir(app_data_dir, app))),
            policy_file: Some(policy_file),
            command_rules_file: Some(app_data_dir.join(COMMAND_RULES_FILE)),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_root(root: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(Selection::default())),
            command_rules: Arc::new(Mutex::new(CommandRules::default())),
            read_root: Some(Arc::new(McpReadRoot::for_tests(root))),
            policy_file: None,
            command_rules_file: None,
        }
    }

    fn availability(&self) -> Capabilities {
        let mut bits = [false; 10];
        let read_access = self.read_root.as_ref().is_some_and(|root| root.available());
        bits[..READ_END].fill(read_access);
        // Privileged operations are bounded to the currently granted root and
        // are independently checked during every call, never by UI alone.
        let write_access=self.read_root.as_ref().is_some_and(|root|root.write_available());
        bits[READ_END..WRITE_END].fill(write_access);
        let rules_enabled=self.command_rules.lock().ok()
            .is_some_and(|rules|rules.enabled &&
                (rules.mode==CommandRuleMode::Denylist || !rules.rules.is_empty()));
        bits[TERMINAL_INDEX]=read_access && rules_enabled;
        // Browser actions are exposed by the separately verified native Plugin
        // registry, not as a counterfeit Core browser/control tool.
        Capabilities(bits)
    }

    /// One fresh OS/root probe per catalog request, not four independent
    /// Kotlin IPC calls for the same set of read-only tools.
    pub(crate) fn effective_file_catalog(&self) -> [bool; READ_END] {
        let requested = match self.state.lock() {
            Ok(state) => state.requested,
            Err(_) => return [false; READ_END],
        };
        let available = self.availability();
        std::array::from_fn(|index| requested.0[index] && available.0[index])
    }

    /// The five implemented write routes are exposed only with an explicit
    /// matching user policy and a fresh writable folder receipt.
    pub(crate) fn effective_write_catalog(&self) -> [bool; WRITE_END-READ_END] {
        let requested=match self.state.lock() {
            Ok(state)=>state.requested,
            Err(_)=>return [false;WRITE_END-READ_END],
        };
        let available=self.availability();
        std::array::from_fn(|i|requested.0[READ_END+i]&&available.0[READ_END+i])
    }

    fn snapshot(&self) -> Result<Value, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "MCP permission state unavailable")?;
        let selection = *state;
        drop(state);
        Ok(selection.snapshot(self.availability()))
    }

    pub(crate) fn configure(
        &self,
        preset: &str,
        requested: Value,
        user_initiated: bool,
    ) -> Result<Value, String> {
        if !user_initiated {
            return Err("MCP tool permissions require explicit user action".into());
        }
        let preset = Preset::parse(preset)?;
        let requested = Capabilities::parse(&requested)?;
        if preset
            .canonical()
            .is_some_and(|canonical| canonical != requested)
        {
            return Err("MCP requested capabilities do not match chosen preset".into());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "MCP permission state unavailable")?;
        let next = Selection { preset, requested };
        if let Some(path) = &self.policy_file {
            // No in-memory state changes if a persisted setting cannot be
            // saved safely. Android's same-directory rename is atomic.
            save_selection(path, next)?;
        }
        *state = next;
        drop(state);
        Ok(next.snapshot(self.availability()))
    }

    /// Gate any future privileged tools/call route. Safe app_info and echo are
    /// always available, while no unimplemented route can become authorized.
    pub(crate) fn permits_remote_tool(&self, name: &str) -> bool {
        if matches!(name, "app_info" | "echo") {
            return true;
        }
        let index = match name {
            "files/browse" => 0,
            "files/search" => 1,
            "files/read" => 2,
            "files/metadata" => 3,
            "files/create" => 4,
            "files/edit" => 5,
            "files/apply_patch" => 5,
            "files/move" => 6,
            "files/delete" => 7,
            "browser/control" => BROWSER_INDEX,
            "terminal/exec" => TERMINAL_INDEX,
            _ => return false,
        };
        let requested = self
            .state
            .lock()
            .ok()
            .is_some_and(|state| state.requested.0[index]);
        requested && self.availability().0[index]
    }

    pub(crate) fn execute_read_tool(
        &self,
        name: &str,
        args: &Map<String, Value>,
    ) -> Result<String, &'static str> {
        if !matches!(
            name,
            "files/browse" | "files/search" | "files/read" | "files/metadata"
        ) {
            return Err("Unknown remote file tool");
        }
        if !self.permits_remote_tool(name) {
            return Err("Remote file permission not enabled or OS folder unavailable");
        }
        let root = self
            .read_root
            .as_ref()
            .ok_or("No authorized MCP file root")?;
        let result = root.run(name, args)?;
        // A change to a fine-grained checkbox during the read must not emit
        // content from an earlier permission snapshot.
        if !self.permits_remote_tool(name) {
            return Err("Remote file permission changed during read");
        }
        Ok(result)
    }

    pub(crate) fn execute_write_tool(
        &self,name:&str,args:&Map<String,Value>
    )->Result<String,&'static str> {
        if !matches!(name,"files/create"|"files/edit"|"files/apply_patch"|
            "files/move"|"files/delete") {return Err("Unknown file mutation tool")}
        if !self.permits_remote_tool(name) {
            return Err("File mutation permission not enabled or approved folder unavailable")
        }
        let root=self.read_root.as_ref().ok_or("No authorized MCP file root")?;
        let output=root.run_write(name,args)?;
        if !self.permits_remote_tool(name) {
            // The mutation has already completed; do not claim it was undone.
            return Err("File permission changed immediately after mutation")
        }
        Ok(output)
    }

    fn command_rules_status(&self)->Result<Value,String> {
        let rules=self.command_rules.lock().map_err(|_|"Command rules unavailable")?;
        Ok(rules.json(self.read_root.is_some()))
    }
    fn set_command_rules(&self,enabled:bool,mode:&str,rules:Vec<String>,
        user_initiated:bool)->Result<Value,String> {
        if !user_initiated {return Err("Explicit user action required for command rules".into())}
        let selection=CommandRules::validate(enabled,mode,rules)?;
        let mut current=self.command_rules.lock().map_err(|_|"Command rules unavailable")?;
        if let Some(path)=self.command_rules_file.as_ref(){
            save_command_rules(path,&selection)?;
        }
        *current=selection;
        Ok(current.json(self.read_root.is_some()))
    }
    pub(crate) fn execute_terminal_tool(&self,args:&Map<String,Value>)
        ->Result<String,&'static str> {
        if args.keys().any(|key|key!="command"&&key!="path") {
            return Err("Unknown terminal tool argument")
        }
        let command=args.get("command").and_then(Value::as_str)
            .ok_or("Terminal command required")?;
        if !matches!(command,"pwd"|"ls"|"stat"|"cat") {
            return Err("Only bounded read-only Android commands are supported")
        }
        let path=args.get("path").map(|v|v.as_str().ok_or("Invalid terminal path"))
            .transpose()?;
        if (command=="pwd" && path.is_some()) ||
            (matches!(command,"cat"|"stat") && path.is_none()) {
            return Err("Missing or unexpected terminal file path")
        }
        if !self.permits_remote_tool("terminal/exec") {
            return Err("Remote terminal permission or command allowlist unavailable")
        }
        let permitted=self.command_rules.lock().ok().is_some_and(|policy|
            policy.permits(command,path));
        if !permitted {return Err("Command denied by user's Android command rules")}
        let root=self.read_root.as_ref().ok_or("No authorized terminal file root")?;
        // NEVER spawn a shell, process, Termux or Android command from remote
        // MCP. These commands are bounded virtual read-only file operations.
        let mut path_args=Map::new();
        if let Some(path)=path {path_args.insert("path".into(),json!(path));}
        let output=match command {
            "pwd"=>json!({"path":".","scope":"Approved MCP folder","readOnly":true}).to_string(),
            "ls"=>root.run("files/browse",&path_args)?,
            "cat"=>root.run("files/read",&path_args)?,
            "stat"=>root.run("files/metadata",&path_args)?,
            _=>return Err("Unsupported terminal command"),
        };
        if !self.permits_remote_tool("terminal/exec") {
            return Err("Remote terminal permission revoked during command")
        }
        Ok(output)
    }
}

fn validate_policy(value: &Value) -> Option<Selection> {
    let map = value.as_object()?;
    if map.len() != 3
        || map.get("version")?.as_u64()? != 1
        || !map.contains_key("preset")
        || !map.contains_key("requested")
    {
        return None;
    }
    let preset = Preset::parse(map.get("preset")?.as_str()?).ok()?;
    let requested = Capabilities::parse(map.get("requested")?).ok()?;
    if preset
        .canonical()
        .is_some_and(|canonical| canonical != requested)
    {
        return None;
    }
    Some(Selection { preset, requested })
}

fn load_selection(path: &Path) -> Option<Selection> {
    let info = fs::symlink_metadata(path).ok()?;
    if !info.is_file() || info.file_type().is_symlink() || info.len() > POLICY_MAX_BYTES {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(POLICY_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > POLICY_MAX_BYTES {
        return None;
    }
    let receipt = serde_json::from_slice(&bytes).ok()?;
    validate_policy(&receipt)
}

fn save_selection(path: &Path, selection: Selection) -> Result<(), String> {
    let parent = path.parent().ok_or("MCP policy directory unavailable")?;
    if !parent.is_dir()
        || fs::symlink_metadata(parent)
            .map_err(|_| "MCP policy directory unavailable")?
            .file_type()
            .is_symlink()
    {
        return Err("MCP policy directory unavailable".into());
    }
    if let Ok(info) = fs::symlink_metadata(path) {
        if !info.is_file() || info.file_type().is_symlink() {
            return Err("MCP policy file must not be a symlink".into());
        }
    }
    let document = json!({
        "version":1,
        "preset":selection.preset.label(),
        "requested":selection.requested.to_value(),
    });
    let encoded = serde_json::to_vec(&document).map_err(|_| "MCP policy cannot be encoded")?;
    if encoded.len() as u64 > POLICY_MAX_BYTES {
        return Err("MCP policy too large".into());
    }
    let temp = parent.join(format!(".mcp-tool-policy-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|_| "MCP policy temporary file unavailable")?;
        file.write_all(&encoded)
            .map_err(|_| "MCP policy write failed")?;
        file.sync_all().map_err(|_| "MCP policy sync failed")?;
        drop(file);
        // On Android/Linux rename within this private directory replaces the
        // destination atomically. No non-atomic fallback is used.
        fs::rename(&temp, path).map_err(|_| "MCP policy atomic save failed".to_owned())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn load_command_rules(path:&Path)->Option<CommandRules> {
    let info=fs::symlink_metadata(path).ok()?;
    if !info.is_file()||info.file_type().is_symlink()||
        info.len()>COMMAND_RULES_MAX_BYTES {return None}
    let data=fs::read(path).ok()?;
    if data.len() as u64>COMMAND_RULES_MAX_BYTES {return None}
    let value:Value=serde_json::from_slice(&data).ok()?;
    let obj=value.as_object()?;
    if obj.len()!=4||obj.get("version")?.as_u64()!=Some(1) {return None}
    let enabled=obj.get("enabled")?.as_bool()?;
    let mode=obj.get("mode")?.as_str()?;
    let rules=obj.get("rules")?.as_array()?.iter()
        .map(|v|v.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    CommandRules::validate(enabled,mode,rules).ok()
}
fn save_command_rules(path:&Path,rules:&CommandRules)->Result<(),String> {
    let parent=path.parent().ok_or("Command rules directory unavailable")?;
    let info=fs::symlink_metadata(parent).map_err(|_|"Command rules directory unavailable")?;
    if !info.is_dir()||info.file_type().is_symlink(){
        return Err("Command rules directory unavailable".into())
    }
    if let Ok(info)=fs::symlink_metadata(path){
        if !info.is_file()||info.file_type().is_symlink(){
            return Err("Command rules file cannot be a link".into())
        }
    }
    let bytes=serde_json::to_vec(&json!({
        "version":1,"enabled":rules.enabled,"mode":rules.mode.label(),"rules":rules.rules
    })).map_err(|_|"Cannot encode command rules")?;
    if bytes.len() as u64>COMMAND_RULES_MAX_BYTES {
        return Err("Command rules too large".into())
    }
    let tmp=parent.join(format!(".mcp-command-rules-{}.tmp",uuid::Uuid::new_v4()));
    let operation=(||->Result<(),String>{
        let mut file=OpenOptions::new().write(true).create_new(true).open(&tmp)
            .map_err(|_|"Cannot stage command rules")?;
        file.write_all(&bytes).map_err(|_|"Cannot write command rules")?;
        file.sync_all().map_err(|_|"Cannot sync command rules")?;
        drop(file);
        fs::rename(&tmp,path).map_err(|_|"Cannot commit command rules atomically".into())
    })();
    if operation.is_err(){let _=fs::remove_file(&tmp);}
    operation
}

#[tauri::command]
pub async fn android_mcp_permissions_status(
    state: tauri::State<'_, AndroidMcpPermissions>,
) -> Result<Value, String> {
    let permissions = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || permissions.snapshot())
        .await
        .map_err(|_| "MCP permission status interrupted".to_owned())?
}

#[tauri::command]
pub async fn android_mcp_permissions_configure(
    state: tauri::State<'_, AndroidMcpPermissions>,
    preset: String,
    requested: Value,
    user_initiated: bool,
) -> Result<Value, String> {
    if !cfg!(target_os = "android") {
        return Err("Remote MCP permissions are configurable only on Android".into());
    }
    let permissions = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        permissions.configure(&preset, requested, user_initiated)
    })
    .await
    .map_err(|_| "MCP permission change interrupted".to_owned())?
}

#[tauri::command]
pub async fn android_mcp_command_rules_status(
    state:tauri::State<'_,AndroidMcpPermissions>,
)->Result<Value,String>{
    let permissions=state.inner().clone();
    tauri::async_runtime::spawn_blocking(move||permissions.command_rules_status())
        .await.map_err(|_|"Command rules status interrupted".to_owned())?
}

#[tauri::command]
pub async fn android_mcp_command_rules_set(
    state:tauri::State<'_,AndroidMcpPermissions>,
    enabled:bool,mode:String,rules:Vec<String>,user_initiated:bool
)->Result<Value,String>{
    if !cfg!(target_os="android") {
        return Err("Android command policy requires native settings".into())
    }
    let permissions=state.inner().clone();
    tauri::async_runtime::spawn_blocking(move||
        permissions.set_command_rules(enabled,&mode,rules,user_initiated))
        .await.map_err(|_|"Command rules save interrupted".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "mcp-permission-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    fn preset_flags(preset: &str) -> Value {
        Preset::parse(preset)
            .unwrap()
            .canonical()
            .unwrap()
            .to_value()
    }

    #[test]
    fn safe_default_denies_all_privileged_tools() {
        let state = AndroidMcpPermissions::default();
        let status = state.snapshot().unwrap();
        assert_eq!(status["preset"], "read_only");
        for key in CAPABILITY_KEYS {
            assert_eq!(status["requested"][key], false);
            assert_eq!(status["effective"][key], false);
            assert_eq!(status["available"][key], false);
        }
        assert!(state.permits_remote_tool("app_info"));
        assert!(state.permits_remote_tool("echo"));
        assert!(!state.permits_remote_tool("files/read"));
        assert!(!state.permits_remote_tool("terminal/exec"));
    }

    #[test]
    fn presets_record_intent_but_never_grant_unavailable_tools() {
        for (preset, true_count) in [("read_only", 4), ("files_terminal", 9), ("full", 10)] {
            let requested = preset_flags(preset);
            assert_eq!(
                CAPABILITY_KEYS
                    .iter()
                    .filter(|key| requested[**key] == true)
                    .count(),
                true_count
            );
            let state = AndroidMcpPermissions::default();
            let status = state.configure(preset, requested.clone(), true).unwrap();
            assert_eq!(status["preset"], preset);
            assert_eq!(status["requested"], requested);
            for key in CAPABILITY_KEYS {
                assert_eq!(status["available"][key], false);
                assert_eq!(status["effective"][key], false);
            }
        }
        assert_eq!(preset_flags("files_terminal")["browserControl"], false);
        assert_eq!(preset_flags("files_terminal")["terminal"], true);
    }

    #[test]
    fn custom_records_bounded_choice_with_fail_closed_effective_gate() {
        let state = AndroidMcpPermissions::default();
        let mut requested = Capabilities::default().to_value();
        requested["terminal"] = json!(true);
        requested["deleteFiles"] = json!(true);
        let status = state.configure("custom", requested.clone(), true).unwrap();
        assert_eq!(status["requested"], requested);
        assert_eq!(status["effective"]["terminal"], false);
        assert_eq!(status["effective"]["deleteFiles"], false);
        for tool in [
            "files/browse",
            "files/read",
            "files/delete",
            "browser/control",
            "terminal/exec",
            "terminal_run",
            "shell",
        ] {
            assert!(!state.permits_remote_tool(tool), "{tool}");
        }
    }

    #[test]
    fn explicit_user_action_and_consistent_preset_are_required() {
        let state = AndroidMcpPermissions::default();
        let full = preset_flags("full");
        assert!(state.configure("full", full.clone(), false).is_err());
        assert!(state.configure("bogus", full.clone(), true).is_err());
        assert!(state.configure("read_only", full.clone(), true).is_err());
        assert_eq!(state.snapshot().unwrap()["requested"]["readFiles"], false);
        assert_eq!(state.snapshot().unwrap()["preset"], "read_only");
    }

    #[test]
    fn malformed_unknown_missing_or_nonboolean_fields_are_rejected() {
        let state = AndroidMcpPermissions::default();
        let valid = preset_flags("full");
        let mut extra = valid.clone();
        extra["root"] = json!(true);
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove("searchFiles");
        let mut wrong = valid.clone();
        wrong["terminal"] = json!("true");
        for bad in [
            extra,
            missing,
            wrong,
            json!([]),
            json!(null),
            json!({"terminal":true}),
        ] {
            assert!(state.configure("custom", bad, true).is_err());
        }
        assert_eq!(state.snapshot().unwrap()["requested"]["terminal"], false);
    }

    #[test]
    fn clones_share_in_memory_choice_but_new_instances_reset() {
        let state = AndroidMcpPermissions::default();
        let clone = state.clone();
        state.configure("full", preset_flags("full"), true).unwrap();
        assert_eq!(clone.snapshot().unwrap()["preset"], "full");
        assert_eq!(clone.snapshot().unwrap()["effective"]["terminal"], false);
        let restarted = AndroidMcpPermissions::default();
        assert_eq!(restarted.snapshot().unwrap()["preset"], "read_only");
        assert_eq!(
            restarted.snapshot().unwrap()["requested"]["terminal"],
            false
        );
    }

    #[test]
    fn real_private_folder_has_four_available_reads_but_needs_explicit_request() {
        let dir = temp_path();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("public.txt"), "shareable test text").unwrap();
        let permissions = AndroidMcpPermissions::for_test_root(dir.clone());
        let initial = permissions.snapshot().unwrap();
        for key in &CAPABILITY_KEYS[..READ_END] {
            assert_eq!(initial["available"][*key], true);
            assert_eq!(initial["requested"][*key], false);
            assert_eq!(initial["effective"][*key], false);
        }
        assert!(!permissions.permits_remote_tool("files/read"));
        let selected = permissions
            .configure("read_only", preset_flags("read_only"), true)
            .unwrap();
        assert_eq!(selected["effective"]["readFiles"], true);
        assert_eq!(selected["effective"]["browseFolders"], true);
        assert_eq!(selected["available"]["createFiles"], true,
            "The private test root is writable, but read_only never grants writes");
        assert_eq!(selected["effective"]["createFiles"], false);
        let mut args = Map::new();
        args.insert("path".to_owned(), json!("public.txt"));
        let content: Value =
            serde_json::from_str(&permissions.execute_read_tool("files/read", &args).unwrap())
                .unwrap();
        assert_eq!(content["text"], "shareable test text");
        assert!(permissions
            .execute_read_tool("files/create", &args)
            .is_err());
        assert!(!permissions.permits_remote_tool("terminal/exec"));

        // A revoked/destroyed root takes effect on the very next call,
        // regardless of saved checkbox state.
        fs::remove_dir_all(&dir).unwrap();
        assert!(!permissions.permits_remote_tool("files/read"));
        let changed = permissions.snapshot().unwrap();
        assert_eq!(changed["available"]["readFiles"], false);
        assert_eq!(changed["effective"]["readFiles"], false);
        assert!(permissions.execute_read_tool("files/read", &args).is_err());
    }

    #[test]
    fn app_private_policy_loads_only_exact_bounded_saved_preference() {
        let dir = temp_path();
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(POLICY_FILE);
        let selected = Selection {
            preset: Preset::ReadOnly,
            requested: Capabilities::parse(&preset_flags("read_only")).unwrap(),
        };
        save_selection(&file, selected).unwrap();
        assert_eq!(load_selection(&file), Some(selected));
        let raw: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(raw["version"], 1);
        assert!(raw.get("credential").is_none());
        assert!(raw.get("effective").is_none());
        assert!(raw.get("available").is_none());

        for corrupted in [
            json!({"version":1,"preset":"full","requested":preset_flags("read_only")}),
            json!({"version":999,"preset":"read_only","requested":preset_flags("read_only")}),
            json!({"version":1,"preset":"custom","requested":{},"secret":"leak"}),
        ] {
            fs::write(&file, serde_json::to_vec(&corrupted).unwrap()).unwrap();
            assert_eq!(load_selection(&file), None);
        }
        fs::write(&file, vec![b'x'; POLICY_MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(load_selection(&file), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn command_allowlist_enforces_exact_read_only_grammar_without_processes() {
        let dir=temp_path();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("note.txt"),"private test text").unwrap();
        let gate=AndroidMcpPermissions::for_test_root(dir.clone());
        gate.configure("full",preset_flags("full"),true).unwrap();
        assert!(!gate.permits_remote_tool("terminal/exec"),
            "A Full preset is never sufficient to expose terminal commands");
        assert!(gate.set_command_rules(true,"allowlist",vec!["pwd".into()],false).is_err());
        for unsafe_rule in ["git status","rm *","pwd; cat secret","cat ../private",
            "ls | sh","cat * && ls","cat /etc/passwd"] {
            assert!(CommandRules::validate(true,"allowlist",vec![unsafe_rule.into()]).is_err(),
                "Unsafe command rule accepted: {unsafe_rule}");
        }
        let state=gate.set_command_rules(true,"allowlist",
            vec!["pwd".into(),"cat *".into(),"stat *".into()],true).unwrap();
        assert_eq!(state["enabled"],true);
        assert!(gate.permits_remote_tool("terminal/exec"));
        let mut cmd=Map::new();
        cmd.insert("command".into(),json!("pwd"));
        assert!(gate.execute_terminal_tool(&cmd).unwrap().contains("Approved MCP folder"));
        cmd.insert("command".into(),json!("cat"));
        cmd.insert("path".into(),json!("note.txt"));
        assert!(gate.execute_terminal_tool(&cmd).unwrap().contains("private test text"));
        cmd.insert("command".into(),json!("ls"));
        assert!(gate.execute_terminal_tool(&cmd).is_err(),
            "Allowlist must not permit commands not selected");
        cmd.insert("command".into(),json!("cat"));
        cmd.insert("path".into(),json!("../private"));
        assert!(gate.execute_terminal_tool(&cmd).is_err(),
            "The allowed verb cannot bypass the OS-bound path validator");
        cmd.insert("command".into(),json!("sh"));
        assert!(gate.execute_terminal_tool(&cmd).is_err(),
            "No shell process is ever exposed");
        gate.set_command_rules(true,"denylist",vec!["cat *".into()],true).unwrap();
        cmd.insert("command".into(),json!("cat"));
        cmd.insert("path".into(),json!("note.txt"));
        assert!(gate.execute_terminal_tool(&cmd).is_err());
        cmd.insert("command".into(),json!("pwd"));
        cmd.remove("path");
        assert!(gate.execute_terminal_tool(&cmd).is_ok());
        gate.set_command_rules(false,"allowlist",vec!["pwd".into()],true).unwrap();
        assert!(!gate.permits_remote_tool("terminal/exec"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn command_rules_persist_only_exact_user_choices_fail_closed() {
        let dir=temp_path();
        fs::create_dir_all(&dir).unwrap();
        let file=dir.join(COMMAND_RULES_FILE);
        assert!(load_command_rules(&file).is_none());
        let state=CommandRules::validate(true,"allowlist",
            vec!["pwd".into(),"ls *".into()]).unwrap();
        save_command_rules(&file,&state).unwrap();
        assert_eq!(load_command_rules(&file),Some(state));
        let stored:Value=serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert!(stored.get("effective").is_none());
        assert!(stored.get("approved").is_none());
        for invalid in [
            json!({"version":1,"mode":"allowlist","enabled":true,"rules":["git status"]}),
            json!({"version":1,"mode":"allowlist","enabled":true,"rules":[]}),
            json!({"version":1,"mode":"allowlist","enabled":true,"rules":["pwd"],"secret":"x"}),
            json!({"version":2,"mode":"allowlist","enabled":true,"rules":["pwd"]}),
        ] {
            fs::write(&file,invalid.to_string()).unwrap();
            assert!(load_command_rules(&file).is_none());
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
