//! Explicitly user-driven Node/npm and pinned SSH project operations on Android.
//! This is NOT Termux IPC. Termux has another Android UID and a separate HOME.
//! No shell runs locally: each operation supplies a fixed executable and argv.
//! An npm script or a Node script can execute arbitrary project code; these
//! commands require a fresh user action and are not accessible to MCP/Goal.

use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};
use tauri::AppHandle;

const SYSTEM_PATH: &str = "/system/bin:/system/xbin:/vendor/bin:/product/bin";
const MAX_STDOUT: usize = 16 * 1024;
const MAX_STDERR: usize = 16 * 1024;
const MAX_DISK_OUTPUT: u64 = 256 * 1024;
const MAX_HOSTS: usize = 32;
static PROCESS_GATE: Mutex<()> = Mutex::new(());
static PIN_GATE: Mutex<()> = Mutex::new(());

fn user_action(user_initiated: bool) -> Result<(), String> {
    if user_initiated {
        Ok(())
    } else {
        Err("Project execution or SSH changes require an explicit user action".into())
    }
}

fn valid_project_id(id: &str) -> bool {
    (id.len() == 36 && uuid::Uuid::parse_str(id).is_ok_and(|u| u.hyphenated().to_string() == id))
        || (id.starts_with("project-")
            && (9..=90).contains(&id.len())
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'))
}

fn project_id(id: &str) -> Result<(), String> {
    if valid_project_id(id) {
        Ok(())
    } else {
        Err("Invalid project catalog ID".into())
    }
}

fn executable(name: &str) -> Option<PathBuf> {
    if !matches!(name, "node" | "npm" | "ssh" | "ssh-keygen") {
        return None;
    }
    SYSTEM_PATH
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|file| file.is_file() && !file.is_symlink())
}

fn private_dir(path: &Path, create: bool) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(true),
        Ok(_) => Err("Private project directory is not a regular directory".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !create => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|_| "Cannot create app-private project directory")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                    .map_err(|_| "Cannot secure app-private project directory")?;
            }
            Ok(true)
        }
        Err(_) => Err("Cannot inspect app-private project directory".into()),
    }
}

// Matches AndroidPty::session_dir exactly, but refuses symlinked segments.
// The catalog guard is separate and must be checked before creating a folder.
fn workspace(home: &Path, id: &str, create: bool) -> Result<Option<PathBuf>, String> {
    project_id(id)?;
    if !home.is_absolute() {
        return Err("App data path must be absolute".into());
    }
    let app_root = home
        .canonicalize()
        .map_err(|_| "App data path is inaccessible")?;
    let base = app_root.join("terminal-workspaces");
    if !private_dir(&base, create)? {
        return Ok(None);
    }
    let base = base
        .canonicalize()
        .map_err(|_| "Workspace root is inaccessible")?;
    if !base.starts_with(&app_root) || base == app_root {
        return Err("Project workspace escaped app data".into());
    }
    let folder = base.join(format!("project-{id}"));
    if !private_dir(&folder, create)? {
        return Ok(None);
    }
    let folder = folder
        .canonicalize()
        .map_err(|_| "Project workspace is inaccessible")?;
    if !folder.starts_with(&base) || folder == base {
        return Err("Project workspace escaped its root".into());
    }
    Ok(Some(folder))
}

fn project_file(folder: &Path, entry: &str) -> Result<PathBuf, String> {
    if entry.is_empty()
        || entry.len() > 240
        || entry.starts_with('/')
        || entry.contains('\\')
        || entry.contains(':')
        || entry.contains('\0')
    {
        return Err("Invalid project script path".into());
    }
    let segments: Vec<_> = entry.split('/').collect();
    if segments.len() > 12
        || segments.iter().any(|part| {
            part.is_empty()
                || part.len() > 80
                || part.starts_with('.')
                || part.starts_with('-')
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
    {
        return Err("Project script must be a simple relative path".into());
    }
    if !matches!(
        Path::new(entry).extension().and_then(|e| e.to_str()),
        Some("js" | "mjs" | "cjs")
    ) {
        return Err("Only JavaScript project files can be executed".into());
    }
    let mut joined = folder.to_owned();
    for part in segments {
        joined.push(part);
        let meta = fs::symlink_metadata(&joined).map_err(|_| "Project script does not exist")?;
        if meta.file_type().is_symlink() {
            return Err("Symlinked project scripts or directories are not allowed".into());
        }
    }
    if !joined.is_file()
        || !joined
            .canonicalize()
            .map_err(|_| "Invalid project script")?
            .starts_with(
                folder
                    .canonicalize()
                    .map_err(|_| "Invalid project workspace")?,
            )
    {
        return Err("Project script escaped its workspace".into());
    }
    Ok(joined)
}

fn npm_script(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 80
        || name.starts_with('-')
        || name.starts_with('.')
        || name.contains("..")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/'))
    {
        return Err("Invalid npm script name".into());
    }
    Ok(())
}

fn node_args(action: &str, entry: Option<&str>, folder: &Path) -> Result<Vec<String>, String> {
    match action {
        "version" if entry.is_none() => Ok(vec!["--version".into()]),
        "run" => Ok(vec![project_file(
            folder,
            entry.ok_or("Node entry is missing")?,
        )?
        .to_string_lossy()
        .into_owned()]),
        _ => Err("Unsupported Node action or extra entry".into()),
    }
}

fn npm_args(action: &str, script: Option<&str>) -> Result<Vec<String>, String> {
    match action {
        "version" if script.is_none() => Ok(vec!["--version".into()]),
        "ci" | "install" if script.is_none() => Ok(vec![
            action.into(),
            "--ignore-scripts".into(),
            "--no-audit".into(),
            "--no-fund".into(),
            "--no-progress".into(),
        ]),
        "run" => {
            let name = script.ok_or("npm script name is missing")?;
            npm_script(name)?;
            Ok(vec!["run".into(), name.into()])
        }
        _ => Err("Unsupported npm action or extra script".into()),
    }
}

fn host_name(host: &str) -> Result<String, String> {
    if host.is_empty()
        || host.len() > 253
        || host.starts_with('-')
        || host.starts_with('.')
        || host.ends_with('.')
        || host.contains("..")
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return Err("SSH host must be an IPv4 address or simple DNS name".into());
    }
    Ok(host.to_ascii_lowercase())
}

fn ssh_username(user: &str) -> Result<(), String> {
    if user.is_empty()
        || user.len() > 32
        || !user.as_bytes()[0].is_ascii_alphabetic()
        || !user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err("Invalid SSH username".into());
    }
    Ok(())
}

fn host_label(host: &str, port: u16) -> Result<String, String> {
    if port == 0 {
        return Err("Invalid SSH port".into());
    }
    let name = host_name(host)?;
    Ok(if port == 22 {
        name
    } else {
        format!("[{name}]:{port}")
    })
}

fn ssh_key_line(public_key: &str) -> Result<String, String> {
    let items: Vec<_> = public_key.split_ascii_whitespace().collect();
    if items.len() != 2 || items[0] != "ssh-ed25519" || items[1].len() != 68 {
        return Err("Only an SSH Ed25519 public host key is accepted".into());
    }
    // SSH Ed25519 wire key: string("ssh-ed25519"), string(32-byte key).
    // 51 bytes encode as precisely 68 unpadded Base64 characters.
    let mut wire = Vec::with_capacity(51);
    for chunk in items[1].as_bytes().chunks_exact(4) {
        let mut digits = [0u8; 4];
        for (i, c) in chunk.iter().copied().enumerate() {
            digits[i] = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => return Err("SSH host public key Base64 is invalid".into()),
            };
        }
        wire.push((digits[0] << 2) | (digits[1] >> 4));
        wire.push((digits[1] << 4) | (digits[2] >> 2));
        wire.push((digits[2] << 6) | digits[3]);
    }
    if wire.len() != 51
        || wire[..4] != [0, 0, 0, 11]
        || wire[4..15] != *b"ssh-ed25519"
        || wire[15..19] != [0, 0, 0, 32]
    {
        return Err("SSH Ed25519 host key encoding is invalid".into());
    }
    Ok(format!("ssh-ed25519 {}", items[1]))
}

fn remote_dir(dir: &str) -> Result<(), String> {
    if !dir.starts_with('/')
        || dir.len() > 256
        || dir.contains("//")
        || dir
            .split('/')
            .skip(1)
            .any(|part| part == "." || part == "..")
        || !dir
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
    {
        return Err("Remote root must be an absolute simple POSIX directory".into());
    }
    Ok(())
}

fn remote_action(action: &str, dir: Option<&str>) -> Result<String, String> {
    match action {
        "probe" if dir.is_none() => Ok("true".into()),
        "pwd" | "ls" | "git-status" => {
            let root = dir.ok_or("Remote project directory is missing")?;
            remote_dir(root)?;
            let suffix = match action {
                "pwd" => "pwd",
                "ls" => "ls -la",
                _ => "git status --short",
            };
            // Strict character validation above excludes all remote-shell
            // expansions and metacharacters. Only suffix is a fixed command.
            Ok(format!("cd {root} && {suffix}"))
        }
        _ => Err("Unsupported SSH action or remote directory".into()),
    }
}

fn secure_file(path: &Path, private_key: bool) -> Result<bool, String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err("Cannot inspect SSH file".into()),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 64 * 1024 {
        return Err("SSH file must be a small regular file, never a symlink".into());
    }
    #[cfg(unix)]
    if private_key {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err("SSH private key permissions are too broad".into());
        }
    }
    let _ = private_key;
    Ok(true)
}

fn ssh_folder(workspace: &Path) -> Result<PathBuf, String> {
    let path = workspace.join(".mcp-ssh");
    private_dir(&path, true)?;
    Ok(path)
}

fn parse_host_pins(raw: &str) -> Result<Vec<String>, String> {
    if raw.len() > 16 * 1024 {
        return Err("SSH known_hosts exceeds limit".into());
    }
    let mut lines = Vec::new();
    for line in raw.lines() {
        let parts: Vec<&str> = line.split_ascii_whitespace().collect();
        if parts.len() != 3 || line != format!("{} {} {}", parts[0], parts[1], parts[2]) {
            return Err("Invalid or unsupported SSH host pin file".into());
        }
        let label = parts[0];
        if let Some(rest) = label.strip_prefix('[') {
            let (host, port) = rest
                .split_once("]:")
                .ok_or("Unsupported SSH host pin name")?;
            let port = port.parse::<u16>().map_err(|_| "Invalid SSH pin port")?;
            if port == 22 || host_label(host, port)? != label {
                return Err("Unsupported SSH host pin name".into());
            }
        } else if host_label(label, 22)? != label {
            return Err("Unsupported SSH host pin name".into());
        }
        ssh_key_line(&format!("{} {}", parts[1], parts[2]))?;
        if lines
            .iter()
            .any(|prev: &String| prev.split(' ').next() == Some(label))
        {
            return Err("Duplicate SSH host pin detected".into());
        }
        lines.push(line.to_owned());
    }
    if lines.len() > MAX_HOSTS {
        return Err("Maximum SSH host pins reached".into());
    }
    Ok(lines)
}

fn pin_host(folder: &Path, host: &str, port: u16, public_key: &str) -> Result<bool, String> {
    let _gate = PIN_GATE.lock().map_err(|_| "Host pin lock failed")?;
    let label = host_label(host, port)?;
    let key = ssh_key_line(public_key)?;
    let dest = folder.join("known_hosts");
    let mut lines = Vec::<String>::new();
    if secure_file(&dest, false)? {
        let raw = fs::read_to_string(&dest).map_err(|_| "SSH known_hosts unreadable")?;
        lines = parse_host_pins(&raw)?;
    }
    if let Some(existing) = lines
        .iter()
        .find(|line| line.split(' ').next() == Some(&label))
    {
        if *existing == format!("{label} {key}") {
            return Ok(false);
        }
        return Err("Different SSH host key is pinned; refusing silent trust replacement".into());
    }
    if lines.len() >= MAX_HOSTS {
        return Err("Maximum SSH host pins reached".into());
    }
    let line = format!("{label} {key}\n");
    let mut options = OpenOptions::new();
    options.write(true).append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(dest).map_err(|_| "Cannot save SSH host pin")?;
    file.write_all(line.as_bytes())
        .map_err(|_| "Cannot write SSH host pin")?;
    file.sync_all().map_err(|_| "Cannot sync SSH host pin")?;
    Ok(true)
}

struct OutputFiles {
    stdout: PathBuf,
    stderr: PathBuf,
}
impl Drop for OutputFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.stdout);
        let _ = fs::remove_file(&self.stderr);
    }
}

fn read_capped(path: &Path, max: usize) -> Result<(String, bool), String> {
    let file = File::open(path).map_err(|_| "Cannot inspect project command output")?;
    let mut buf = Vec::new();
    file.take((max + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|_| "Cannot read project command output")?;
    let truncated = buf.len() > max;
    buf.truncate(max);
    Ok((String::from_utf8_lossy(&buf).into_owned(), truncated))
}

// Runs a program directly (no local shell or command interpolation), with one
// process in flight, deadline and bounded temporary output files.
fn run_bounded(
    binary: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    tool: &str,
    action: &str,
) -> Result<Value, String> {
    use std::{
        process::{Command, Stdio},
        thread,
    };
    user_action(true)?;
    if !binary.is_file() {
        return Err("Project executable is unavailable".into());
    }
    let _lock = PROCESS_GATE
        .try_lock()
        .map_err(|_| "Another project operation is running; retry after it finishes")?;
    let tmp = cwd.join("tmp");
    private_dir(&tmp, true)?;
    let suffix = uuid::Uuid::new_v4().to_string();
    let paths = OutputFiles {
        stdout: tmp.join(format!(".project-out-{suffix}")),
        stderr: tmp.join(format!(".project-err-{suffix}")),
    };
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let out = options
        .open(&paths.stdout)
        .map_err(|_| "Cannot create command output file")?;
    let err = options
        .open(&paths.stderr)
        .map_err(|_| "Cannot create command error file")?;
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("HOME", cwd)
        .env("TMPDIR", &tmp)
        .env("PATH", SYSTEM_PATH)
        .env("npm_config_cache", cwd.join(".npm-cache"))
        .env("CI", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    #[cfg(target_os = "android")]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|_| "Project program could not start in this Android app UID")?;
    let started = Instant::now();
    let mut timed_out = false;
    let mut over_limit = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => {
                #[cfg(target_os = "android")]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err("Project process wait failed".into());
            }
        }
        let size = fs::metadata(&paths.stdout)
            .map(|m| m.len())
            .unwrap_or(0)
            .saturating_add(fs::metadata(&paths.stderr).map(|m| m.len()).unwrap_or(0));
        if started.elapsed() >= timeout || size > MAX_DISK_OUTPUT {
            timed_out = started.elapsed() >= timeout;
            over_limit = size > MAX_DISK_OUTPUT;
            #[cfg(target_os = "android")]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            break child.wait().map_err(|_| "Could not reap project process")?;
        }
        thread::sleep(Duration::from_millis(20));
    };
    // A Node/npm/SSH child may fork; stop its own process group on Android
    // after the leader exits too, so detached helpers do not survive this turn.
    #[cfg(target_os = "android")]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let output_size = fs::metadata(&paths.stdout)
        .map(|m| m.len())
        .unwrap_or(0)
        .saturating_add(fs::metadata(&paths.stderr).map(|m| m.len()).unwrap_or(0));
    if output_size > MAX_DISK_OUTPUT {
        over_limit = true;
    }
    let (stdout, a) = read_capped(&paths.stdout, MAX_STDOUT)?;
    let (stderr, b) = read_capped(&paths.stderr, MAX_STDERR)?;
    Ok(json!({
        "tool":tool, "action":action, "processStarted":true,
        "completed":!timed_out && !over_limit,
        "success":status.success() && !timed_out && !over_limit,
        "exitCode":status.code(),
        "timedOut":timed_out, "outputLimitExceeded":over_limit,
        "truncated":a || b, "stdout":stdout, "stderr":stderr,
        "durationMs":started.elapsed().as_millis().min(u32::MAX as u128)
    }))
}

async fn catalog_project(app: &AppHandle, id: &str) -> Result<(), String> {
    #[cfg(target_os = "android")]
    use tauri::Manager;
    project_id(id)?;
    #[cfg(target_os = "android")]
    {
        let engine = app
            .try_state::<std::sync::Arc<mcp_agent_core::Engine>>()
            .ok_or("Project catalog engine unavailable")?;
        let result = engine
            .request("/api/mobile-projects", Some(json!({"action":"status"})))
            .await?;
        if !result["catalog"]["projects"]
            .as_array()
            .is_some_and(|list| list.iter().any(|p| p["id"] == id))
        {
            return Err("Project ID is not present in the native project catalog".into());
        }
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err("Android projects runtime is unavailable on this host".into())
    }
}

fn android_only() -> Result<(), String> {
    if cfg!(target_os = "android") {
        Ok(())
    } else {
        Err("Project Node/npm/SSH process execution is Android-only".into())
    }
}

#[tauri::command]
pub async fn android_project_runtime_status(
    app: AppHandle,
    project_id: String,
) -> Result<Value, String> {
    android_only()?;
    catalog_project(&app, &project_id).await?;
    #[cfg(target_os = "android")]
    {
        use tauri::Manager;
        let home = app
            .path()
            .app_data_dir()
            .map_err(|_| "App data path unavailable")?;
        let existing = workspace(&home, &project_id, false)?;
        let key_exists = if let Some(ref folder) = existing {
            secure_file(&folder.join(".mcp-ssh").join("id_ed25519"), true).unwrap_or(false)
        } else {
            false
        };
        let termux = crate::android_terminal::termux_status(app.clone())
            .unwrap_or_else(|_| json!({"supported":false,"statusUnverified":true}));
        let probes = crate::android_pty::android_pty_capabilities(app).await?;
        Ok(json!({
            "supported":true, "projectId":project_id,
            "workspaceExists":existing.is_some(),
            "executionContext":"app-private-uid", "sandboxed":false,
            "node":probes["node"], "npm":probes["npm"],
            "ssh":probes["sshClient"],
            "sshKeygenExecutableFound":executable("ssh-keygen").is_some(),
            "sshIdentityExists":key_exists,
            "npmInstallTested":false,
            "sshAuthenticationTested":false,
            "sshRemoteActionsImplemented":["probe","pwd","ls","git-status"],
            "sshProjectSyncImplemented":false,
            "requiresUserInitiated":true,
            "termux":termux,
            "termuxWorkspaceShared":false,
            "termuxOpenCommand":"android_termux_open",
            "termuxInteractiveSetup":"pkg update && pkg install nodejs-lts openssh git"
        }))
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        unreachable!()
    }
}

#[tauri::command]
pub async fn android_project_node(
    app: AppHandle,
    project_id: String,
    action: String,
    entry: Option<String>,
    user_initiated: bool,
) -> Result<Value, String> {
    android_only()?;
    user_action(user_initiated)?;
    catalog_project(&app, &project_id).await?;
    #[cfg(target_os = "android")]
    {
        use tauri::Manager;
        let home = app
            .path()
            .app_data_dir()
            .map_err(|_| "App data path unavailable")?;
        tauri::async_runtime::spawn_blocking(move || {
            let folder = workspace(&home, &project_id, true)?.ok_or("Workspace unavailable")?;
            let args = node_args(&action, entry.as_deref(), &folder)?;
            let bin = executable("node")
                .ok_or("Node is not executable inside this app UID; Termux is separate")?;
            let mut result = run_bounded(
                &bin,
                &args,
                &folder,
                Duration::from_secs(if action == "version" { 5 } else { 30 }),
                "node",
                &action,
            )?;
            result["projectId"] = json!(project_id);
            Ok(result)
        })
        .await
        .map_err(|_| "Node operation interrupted".to_string())?
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, action, entry);
        unreachable!()
    }
}

#[tauri::command]
pub async fn android_project_npm(
    app: AppHandle,
    project_id: String,
    action: String,
    script: Option<String>,
    user_initiated: bool,
) -> Result<Value, String> {
    android_only()?;
    user_action(user_initiated)?;
    catalog_project(&app, &project_id).await?;
    #[cfg(target_os = "android")]
    {
        use tauri::Manager;
        let home = app
            .path()
            .app_data_dir()
            .map_err(|_| "App data path unavailable")?;
        tauri::async_runtime::spawn_blocking(move || {
            let folder = workspace(&home, &project_id, true)?.ok_or("Workspace unavailable")?;
            let args = npm_args(&action, script.as_deref())?;
            let bin = executable("npm")
                .ok_or("npm is not executable inside this app UID; Termux is separate")?;
            let mut result = run_bounded(
                &bin,
                &args,
                &folder,
                Duration::from_secs(if action == "version" { 5 } else { 120 }),
                "npm",
                &action,
            )?;
            result["projectId"] = json!(project_id);
            Ok(result)
        })
        .await
        .map_err(|_| "npm operation interrupted".to_string())?
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, action, script);
        unreachable!()
    }
}

#[tauri::command]
pub async fn android_project_ssh_identity(
    app: AppHandle,
    project_id: String,
    confirm_unencrypted_key_storage: bool,
    user_initiated: bool,
) -> Result<Value, String> {
    android_only()?;
    user_action(user_initiated)?;
    if !confirm_unencrypted_key_storage {
        return Err(
            "Confirm that the generated SSH key has no passphrase and is visible to app-UID code"
                .into(),
        );
    }
    catalog_project(&app, &project_id).await?;
    #[cfg(target_os = "android")]
    {
        use tauri::Manager;
        let home = app
            .path()
            .app_data_dir()
            .map_err(|_| "App data path unavailable")?;
        tauri::async_runtime::spawn_blocking(move || {
            let folder = workspace(&home, &project_id, true)?.ok_or("Workspace unavailable")?;
            let ssh = ssh_folder(&folder)?;
            let key = ssh.join("id_ed25519");
            let pubkey = ssh.join("id_ed25519.pub");
            let existing = secure_file(&key, true)?;
            if existing != secure_file(&pubkey, false)? {
                return Err("Incomplete SSH identity; refusing to overwrite any key".into());
            }
            let generated = if existing {
                false
            } else {
                let bin = executable("ssh-keygen")
                    .ok_or("ssh-keygen is not available inside this Android app")?;
                let args = vec![
                    "-q".into(),
                    "-t".into(),
                    "ed25519".into(),
                    "-N".into(),
                    "".into(),
                    "-f".into(),
                    key.to_string_lossy().into_owned(),
                ];
                let result = run_bounded(
                    &bin,
                    &args,
                    &folder,
                    Duration::from_secs(15),
                    "ssh-keygen",
                    "create-identity",
                )?;
                if result["success"] != true {
                    return Err("SSH key generation failed; existing files preserved".into());
                }
                if !secure_file(&key, true)? || !secure_file(&pubkey, false)? {
                    return Err("SSH key generation did not create a verified identity".into());
                }
                true
            };
            let public = fs::read_to_string(&pubkey).map_err(|_| "Public SSH key unavailable")?;
            if public.len() > 512 || !public.starts_with("ssh-ed25519 ") {
                return Err("Generated public SSH key format invalid".into());
            }
            Ok(json!({"projectId":project_id, "generated":generated,
                "publicKey":public.trim(), "privateKeyExported":false,
                "privateKeyStoredIn":"app-private-project-dir",
                "privateKeyPassphraseProtected":false,
                "remoteAuthorizationVerified":false}))
        })
        .await
        .map_err(|_| "SSH identity operation interrupted".to_string())?
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        unreachable!()
    }
}

#[tauri::command]
pub async fn android_project_ssh_pin(
    app: AppHandle,
    project_id: String,
    host: String,
    port: u16,
    public_key: String,
    confirmed_trusted_host_key: bool,
    user_initiated: bool,
) -> Result<Value, String> {
    android_only()?;
    user_action(user_initiated)?;
    if !confirmed_trusted_host_key {
        return Err("SSH host key must be independently verified and explicitly approved".into());
    }
    host_label(&host, port)?;
    ssh_key_line(&public_key)?;
    catalog_project(&app, &project_id).await?;
    #[cfg(target_os = "android")]
    {
        use tauri::Manager;
        let home = app
            .path()
            .app_data_dir()
            .map_err(|_| "App data path unavailable")?;
        tauri::async_runtime::spawn_blocking(move || {
            let folder = workspace(&home, &project_id, true)?.ok_or("Workspace unavailable")?;
            let ssh = ssh_folder(&folder)?;
            let inserted = pin_host(&ssh, &host, port, &public_key)?;
            Ok(
                json!({"projectId":project_id,"host":host_name(&host)?, "port":port,
                "pinned":true, "newPin":inserted, "replacedExistingPin":false,
                "remoteHostVerifiedByApp":false}),
            )
        })
        .await
        .map_err(|_| "SSH host pin operation interrupted".to_string())?
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, host, port, public_key);
        unreachable!()
    }
}

#[tauri::command]
pub async fn android_project_ssh_run(
    app: AppHandle,
    project_id: String,
    host: String,
    port: u16,
    username: String,
    action: String,
    remote_dir: Option<String>,
    user_initiated: bool,
) -> Result<Value, String> {
    android_only()?;
    user_action(user_initiated)?;
    host_label(&host, port)?;
    ssh_username(&username)?;
    remote_action(&action, remote_dir.as_deref())?;
    catalog_project(&app, &project_id).await?;
    #[cfg(target_os = "android")]
    {
        use tauri::Manager;
        let home = app
            .path()
            .app_data_dir()
            .map_err(|_| "App data path unavailable")?;
        tauri::async_runtime::spawn_blocking(move || {
            let folder = workspace(&home, &project_id, true)?.ok_or("Workspace unavailable")?;
            let ssh_dir = ssh_folder(&folder)?;
            let key = ssh_dir.join("id_ed25519");
            let known = ssh_dir.join("known_hosts");
            if !secure_file(&key, true)? || !secure_file(&known, false)? {
                return Err("Project SSH identity and trusted host key are required".into());
            }
            let label = host_label(&host, port)?;
            let pinned = fs::read_to_string(&known).map_err(|_| "Host pin file unavailable")?;
            if !parse_host_pins(&pinned)?
                .iter()
                .any(|line| line.split(' ').next() == Some(label.as_str()))
            {
                return Err("SSH host key has not been independently pinned".into());
            }
            let bin =
                executable("ssh").ok_or("ssh is not executable inside this Android app UID")?;
            let mut args = vec![
                "-F".into(),
                "/dev/null".into(),
                "-T".into(),
                "-p".into(),
                port.to_string(),
            ];
            for option in [
                "BatchMode=yes",
                "PasswordAuthentication=no",
                "KbdInteractiveAuthentication=no",
                "StrictHostKeyChecking=yes",
                "GlobalKnownHostsFile=/dev/null",
                "UpdateHostKeys=no",
                "VerifyHostKeyDNS=no",
                "ForwardAgent=no",
                "ForwardX11=no",
                "ClearAllForwardings=yes",
                "PermitLocalCommand=no",
                "IdentitiesOnly=yes",
                "IdentityAgent=none",
                "CanonicalizeHostname=no",
                "ControlMaster=no",
                "ProxyCommand=none",
                "ConnectTimeout=8",
                "ConnectionAttempts=1",
                "LogLevel=ERROR",
            ] {
                args.extend(["-o".into(), option.into()]);
            }
            args.extend([
                "-o".into(),
                format!("UserKnownHostsFile={}", known.to_string_lossy()),
            ]);
            args.extend(["-i".into(), key.to_string_lossy().into_owned()]);
            args.push(format!("{}@{}", username, host_name(&host)?));
            args.push(remote_action(&action, remote_dir.as_deref())?);
            let mut result = run_bounded(
                &bin,
                &args,
                &folder,
                Duration::from_secs(30),
                "ssh",
                &action,
            )?;
            result["projectId"] = json!(project_id);
            result["host"] = json!(host_name(&host)?);
            result["port"] = json!(port);
            result["hostKeyPolicy"] = json!("strict-pinned");
            result["authenticationMethod"] = json!("publickey-only");
            result["remoteDir"] = json!(remote_dir);
            Ok(result)
        })
        .await
        .map_err(|_| "SSH project command interrupted".to_string())?
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, host, port, username, action, remote_dir);
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_project_escape_and_ssh_shell_injection() {
        assert!(valid_project_id("123e4567-e89b-42d3-a456-426614174000"));
        assert!(valid_project_id("project-local_123"));
        for bad in ["../data", "project-../../", "Project-A", "-F", "x;true"] {
            assert!(!valid_project_id(bad), "{bad}");
        }
        for bad in [
            "evil.com;touch /tmp/x",
            "-oProxyCommand=sh",
            "a@b",
            "[::1]",
            "a/b",
            "",
            "host..example",
        ] {
            assert!(host_label(bad, 22).is_err(), "{bad}");
        }
        assert_eq!(host_label("EXAMPLE.org", 22).unwrap(), "example.org");
        assert_eq!(
            host_label("example.org", 2222).unwrap(),
            "[example.org]:2222"
        );
        for bad in ["root;touch /tmp/x", "root@host", "-o", "a b", ""] {
            assert!(ssh_username(bad).is_err(), "{bad}");
        }
        for bad in [
            "/tmp;rm -rf /",
            "../etc",
            "/tmp/../root",
            "/tmp/$(id)",
            "/tmp/space here",
            "~/project",
            "/tmp//foo",
            "",
        ] {
            assert!(remote_dir(bad).is_err(), "{bad}");
        }
        assert_eq!(
            remote_action("git-status", Some("/home/user/repo")).unwrap(),
            "cd /home/user/repo && git status --short"
        );
        assert!(remote_action("arbitrary-command", None).is_err());
        assert!(remote_action("probe", Some("/home")).is_err());
        assert!(ssh_key_line("ssh-rsa AAAA").is_err());
        assert!(ssh_key_line("ssh-ed25519 $(touch /tmp/pwn)").is_err());
        assert!(user_action(false).is_err());
    }

    #[test]
    fn node_and_npm_argv_are_allowlisted() {
        let home = std::env::temp_dir().join(format!("mcp-project-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&home).unwrap();
        fs::write(home.join("index.js"), b"console.log('test');").unwrap();
        assert_eq!(
            node_args("version", None, &home).unwrap(),
            vec!["--version"]
        );
        assert_eq!(
            node_args("run", Some("index.js"), &home).unwrap(),
            vec![home.join("index.js").to_string_lossy().into_owned()]
        );
        for bad in [
            "../secret.js",
            "/etc/passwd",
            "index.js;sh",
            "src/../../x.js",
            "file.txt",
            "index.js\n--inspect",
            "-e.js",
        ] {
            assert!(node_args("run", Some(bad), &home).is_err(), "{bad}");
        }
        assert!(node_args("eval", Some("index.js"), &home).is_err());
        assert!(npm_args("install", None)
            .unwrap()
            .contains(&"--ignore-scripts".into()));
        assert_eq!(
            npm_args("run", Some("build:android")).unwrap(),
            vec!["run", "build:android"]
        );
        assert!(npm_args("run", Some("--ignore-scripts")).is_err());
        assert!(npm_args("run", Some("build;curl evil")).is_err());
        assert!(npm_args("exec", None).is_err());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn project_workspace_does_not_follow_symlinks_or_mix_project_ids() {
        let home = std::env::temp_dir().join(format!("mcp-project-home-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&home).unwrap();
        assert!(workspace(&home, "project-one", false).unwrap().is_none());
        let one = workspace(&home, "project-one", true).unwrap().unwrap();
        let two = workspace(&home, "project-two", true).unwrap().unwrap();
        assert_ne!(one, two);
        assert!(one.ends_with("project-project-one"));
        assert!(workspace(&home, "../escape", true).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&one, two.join("link")).unwrap();
            assert!(project_file(&two, "link/run.js").is_err());
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn bounded_process_returns_real_exit_code_and_not_false_success() {
        let home = std::env::temp_dir().join(format!("mcp-runtime-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&home).unwrap();
        let binary = std::env::current_exe().unwrap();
        let ok = run_bounded(
            &binary,
            &["--help".into()],
            &home,
            Duration::from_secs(5),
            "test",
            "help",
        )
        .unwrap();
        assert_eq!(ok["success"], true);
        assert_eq!(ok["processStarted"], true);
        let bad = run_bounded(
            &binary,
            &["--not-a-real-argument".into()],
            &home,
            Duration::from_secs(5),
            "test",
            "bad",
        )
        .unwrap();
        assert_eq!(bad["success"], false);
        assert_eq!(bad["processStarted"], true);
        assert_eq!(bad["timedOut"], false);
        let timed_out = run_bounded(
            &binary,
            &[
                "--exact".into(),
                "android_project_runtime::tests::bounded_child_wait".into(),
                "--nocapture".into(),
            ],
            &home,
            Duration::from_millis(40),
            "test",
            "timeout",
        )
        .unwrap();
        assert_eq!(timed_out["processStarted"], true);
        assert_eq!(timed_out["success"], false);
        assert_eq!(timed_out["timedOut"], true);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn bounded_child_wait() {
        std::thread::sleep(Duration::from_millis(400));
    }

    #[test]
    fn pins_are_additive_and_conflicting_trust_is_rejected() {
        let root = std::env::temp_dir().join(format!("mcp-pins-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4f";
        assert_eq!(pin_host(&root, "EXAMPLE.com", 22, &key).unwrap(), true);
        assert_eq!(pin_host(&root, "example.com", 22, &key).unwrap(), false);
        assert!(pin_host(
            &root,
            "example.com",
            22,
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8g"
        )
        .is_err());
        assert_eq!(pin_host(&root, "example.com", 2200, &key).unwrap(), true);
        assert_eq!(
            fs::read_to_string(root.join("known_hosts"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        fs::write(root.join("known_hosts"), format!("* {key}\n")).unwrap();
        assert!(pin_host(&root, "another.example", 22, key).is_err());
        assert!(parse_host_pins(&format!("EXAMPLE.com {key}\n")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
