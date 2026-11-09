//! Human-operated Android workspace shell, separate from the bounded read-only
//! AppTerminalPlugin and from all AI/MCP/Goal execution routes.
//! A PTY is not a filesystem sandbox: the child shares the application's UID.
//! The renderer must obtain explicit user consent before opening a shell.
use serde_json::{json, Value};
use tauri::AppHandle;

// The PTY starts with this exact Android system PATH. Probe the same
// directories: Termux has a separate process UID and filesystem.
#[cfg(any(target_os = "android", test))]
const PTY_SYSTEM_PATH: &str = "/system/bin:/system/xbin:/vendor/bin:/product/bin";

#[cfg(any(target_os = "android", test))]
fn probe_program(
    executable: &std::path::Path,
    arguments: &[&str],
    home: &std::path::Path,
    timeout: std::time::Duration,
) -> (bool, bool, bool) {
    use std::{
        process::{Command, Stdio},
        time::Instant,
    };
    if !executable.is_file() {
        return (false, false, false);
    }
    let mut process = match Command::new(executable)
        .args(arguments)
        .env_clear()
        .env("HOME", home)
        .env("TMPDIR", home)
        .env("PATH", PTY_SYSTEM_PATH)
        .env("npm_config_cache", home.join("npm-cache"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(process) => process,
        Err(_) => return (true, false, false),
    };
    let started = Instant::now();
    loop {
        match process.try_wait() {
            Ok(Some(status)) => return (true, status.success(), false),
            Err(_) => {
                let _ = process.kill();
                let _ = process.wait();
                return (true, false, false);
            }
            Ok(None) if started.elapsed() >= timeout => {
                let _ = process.kill();
                let _ = process.wait();
                return (true, false, true);
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
}

#[cfg(target_os = "android")]
fn probe_pty_tool(
    name: &str,
    arguments: &[&str],
    home: &std::path::Path,
    timeout: std::time::Duration,
) -> Value {
    // Names are fixed by the caller, never supplied by a model or renderer.
    let executable = PTY_SYSTEM_PATH
        .split(':')
        .map(|directory| std::path::Path::new(directory).join(name))
        .find(|path| path.is_file());
    let (found, verified, timed_out) = match executable {
        Some(path) => probe_program(&path, arguments, home, timeout),
        None => (false, false, false),
    };
    json!({
        "executableFound":found,
        "probeSucceeded":verified,
        "probeTimedOut":timed_out
    })
}

fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() == 36
        && [8, 13, 18, 23].iter().all(|&i| b[i] == b'-')
        && b.iter().enumerate().all(|(i, c)| {
            [8, 13, 18, 23].contains(&i) || (c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        })
        && (b'1'..=b'8').contains(&b[14])
        && matches!(b[19], b'8' | b'9' | b'a' | b'b')
}

fn validate_create(
    id: &str,
    project_id: Option<&str>,
    cols: u16,
    rows: u16,
    user_initiated: bool,
) -> Result<(), String> {
    if !user_initiated {
        return Err("A terminal must be opened explicitly by the user".into());
    }
    if !valid_id(id) || project_id.is_some_and(|id| !valid_id(id)) {
        return Err("A canonical terminal/project UUID is required".into());
    }
    if !(2..=300).contains(&cols) || !(1..=150).contains(&rows) {
        return Err("Terminal dimensions are out of range".into());
    }
    Ok(())
}

fn validate_write(id: &str, data: &str, user_initiated: bool) -> Result<(), String> {
    if !user_initiated {
        return Err("AI and background callers cannot write to a user terminal".into());
    }
    if !valid_id(id) || data.is_empty() || data.len() > 4096 {
        return Err("Invalid terminal input or input exceeds 4096 bytes".into());
    }
    Ok(())
}

#[cfg(target_os = "android")]
mod platform {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use std::{
        collections::HashMap,
        fs::{self, File},
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::process::CommandExt,
        },
        process::{Command, Stdio},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Condvar, Mutex,
        },
        thread,
        time::Duration,
    };
    use tauri::{Emitter, Manager};

    const MAX_SESSIONS: usize = 4;
    const HIGH_WATER: usize = 131_072;
    const LOW_WATER: usize = 32_768;

    struct Flow {
        queued: usize,
        closed: bool,
    }
    struct Entry {
        pid: i32,
        writer: Arc<Mutex<File>>,
        flow: Arc<(Mutex<Flow>, Condvar)>,
    }
    pub struct AndroidPty {
        entries: Arc<Mutex<HashMap<String, Entry>>>,
        foreground: AtomicBool,
    }
    impl Default for AndroidPty {
        fn default() -> Self {
            Self {
                entries: Arc::new(Mutex::new(HashMap::new())),
                foreground: AtomicBool::new(true),
            }
        }
    }

    impl Drop for AndroidPty {
        fn drop(&mut self) {
            self.stop_all();
        }
    }

    impl AndroidPty {
        pub fn stop_all(&self) {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            for (_, entry) in entries.drain() {
                stop(entry);
            }
        }
    }
    pub fn set_foreground(app: &AppHandle, focused: bool) {
        if let Some(manager) = app.try_state::<AndroidPty>() {
            manager.foreground.store(focused, Ordering::SeqCst);
            if !focused {
                manager.stop_all();
            }
        }
    }

    fn stop(entry: Entry) {
        let (lock, signal) = &*entry.flow;
        let mut flow = lock.lock().unwrap_or_else(|e| e.into_inner());
        flow.closed = true;
        signal.notify_all();
        drop(flow);
        // Separate session/process group; do not target unrelated Android processes.
        if entry.pid > 1 {
            unsafe {
                libc::kill(-entry.pid, libc::SIGKILL);
            }
        }
    }

    fn make_pty(cols: u16, rows: u16) -> Result<(File, File), String> {
        let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(format!(
                "Cannot open Android PTY: {}",
                std::io::Error::last_os_error()
            ));
        }
        let master = unsafe { File::from_raw_fd(fd) };
        if unsafe { libc::grantpt(fd) } != 0 || unsafe { libc::unlockpt(fd) } != 0 {
            return Err(format!(
                "Cannot unlock Android PTY: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut name = [0 as libc::c_char; 128];
        if unsafe { libc::ptsname_r(fd, name.as_mut_ptr(), name.len()) } != 0 {
            return Err("Android PTY slave name unavailable".into());
        }
        let slave_fd = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
            )
        };
        if slave_fd < 0 {
            return Err(format!(
                "Cannot open Android PTY slave: {}",
                std::io::Error::last_os_error()
            ));
        }
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(slave_fd, libc::TIOCSWINSZ, &size) } < 0 {
            return Err("Cannot size Android PTY".into());
        }
        Ok((master, slave))
    }

    fn session_dir(
        app: &AppHandle,
        project_id: Option<&str>,
    ) -> Result<std::path::PathBuf, String> {
        let base = app
            .path()
            .app_data_dir()
            .map_err(|e| e.to_string())?
            .join("terminal-workspaces");
        let leaf = match project_id {
            Some(id) => format!("project-{id}"),
            None => "default".into(),
        };
        fs::create_dir_all(&base).map_err(|e| e.to_string())?;
        let root = base.canonicalize().map_err(|e| e.to_string())?;
        let folder = root.join(leaf);
        fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        let real = folder.canonicalize().map_err(|e| e.to_string())?;
        if !real.starts_with(&root) || real == root {
            return Err("Terminal working directory escaped its initial folder".into());
        }
        fs::create_dir_all(real.join("tmp")).map_err(|e| e.to_string())?;
        Ok(real)
    }

    pub fn status(app: AppHandle) -> Result<Value, String> {
        let managed = app.state::<AndroidPty>();
        let count = managed
            .entries
            .lock()
            .map_err(|_| "PTY state lock failed")?
            .len();
        Ok(json!({
            "supported": true, "kind": "android-in-app-pty", "shell": "/system/bin/sh",
            "active": count, "maxSessions": MAX_SESSIONS,
            "userInitiatedRequired": true, "automatedCommandsAllowed": false,
            "sandboxed": false, "termuxRequired": false
        }))
    }

    // This checks programs accessible to THIS app's PTY. Merely detecting
    // Termux is deliberately insufficient: Android apps cannot normally
    // execute each other's package-private Node, npm or OpenSSH binaries.
    pub fn capabilities(app: AppHandle) -> Result<Value, String> {
        let home = app.path().app_data_dir().map_err(|e| e.to_string())?;
        let short = Duration::from_secs(2);
        let npm_time = Duration::from_secs(4);
        let node = probe_pty_tool(
            "node",
            &["-e", "if (1 + 1 !== 2) process.exit(17)"],
            &home,
            short,
        );
        let npm = probe_pty_tool("npm", &["--version"], &home, npm_time);
        let ssh = probe_pty_tool("ssh", &["-V"], &home, short);
        let git = probe_pty_tool("git", &["--version"], &home, short);
        let python = probe_pty_tool("python3", &["-c", "assert 1 + 1 == 2"], &home, short);
        let node_ready = node["probeSucceeded"] == true;
        let npm_ready = npm["probeSucceeded"] == true && node_ready;
        Ok(json!({
            "supported":true,
            "kind":"android-in-app-pty",
            "executionContext":"app-uid-user-operated-shell",
            "termuxShared":false,
            "automatedCommandsAllowed":false,
            "node":node,
            "npm":npm,
            "sshClient":ssh,
            "git":git,
            "python3":python,
            "nodeCodeExecutionProbeSucceeded":node_ready,
            "nodeNpmCliProbesSucceeded":npm_ready,
            "npmInstallTested":false,
            "sshProjectTransportImplemented":false,
            "sshAuthenticationTested":false
        }))
    }

    pub fn create(
        app: AppHandle,
        id: String,
        project_id: Option<String>,
        cols: u16,
        rows: u16,
        user_initiated: bool,
    ) -> Result<Value, String> {
        validate_create(&id, project_id.as_deref(), cols, rows, user_initiated)?;
        let manager = app.state::<AndroidPty>();
        let mut entries = manager
            .entries
            .lock()
            .map_err(|_| "PTY state lock failed")?;
        if !manager.foreground.load(Ordering::SeqCst) {
            return Err("Terminal cannot start while Android is in the background".into());
        }
        if entries.contains_key(&id) {
            return Err("Terminal ID already active".into());
        }
        if entries.len() >= MAX_SESSIONS {
            return Err("Close a terminal before opening another (maximum 4)".into());
        }

        let cwd = session_dir(&app, project_id.as_deref())?;
        let (master, slave) = make_pty(cols, rows)?;
        let mut command = Command::new("/system/bin/sh");
        command
            .arg("-i")
            .current_dir(&cwd)
            .env_clear()
            .env("HOME", &cwd)
            .env("PWD", &cwd)
            .env("TMPDIR", cwd.join("tmp"))
            .env("PATH", PTY_SYSTEM_PATH)
            .env("TERM", "xterm-256color")
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::from(slave.try_clone().map_err(|e| e.to_string())?))
            .stdout(Stdio::from(slave.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(slave));
        // The child becomes its own session leader with the PTY as controlling
        // terminal. Parent retains master; all standard streams share slave.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let writer = Arc::new(Mutex::new(master.try_clone().map_err(|e| e.to_string())?));
        let mut child = command
            .spawn()
            .map_err(|e| format!("Android shell failed to start: {e}"))?;
        let pid = child.id() as i32;
        let flow = Arc::new((
            Mutex::new(Flow {
                queued: 0,
                closed: false,
            }),
            Condvar::new(),
        ));
        entries.insert(
            id.clone(),
            Entry {
                pid,
                writer,
                flow: flow.clone(),
            },
        );
        drop(entries);

        let read_id = id.clone();
        let receipt_id = id.clone();
        let read_app = app.clone();
        let child_exited = Arc::new(AtomicBool::new(false));
        let reader_exited = child_exited.clone();
        let reader_handle = thread::spawn(move || {
            let mut reader = master;
            let mut buffer = [0u8; 4096];
            loop {
                let (lock, signal) = &*flow;
                let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                while !state.closed && state.queued >= HIGH_WATER {
                    let waited = signal.wait_timeout(state, Duration::from_secs(10));
                    match waited {
                        Ok((next, timeout)) => {
                            state = next;
                            if timeout.timed_out() && state.queued >= HIGH_WATER {
                                if !reader_exited.load(Ordering::Acquire) {
                                    unsafe {
                                        libc::kill(-pid, libc::SIGKILL);
                                    }
                                }
                                state.closed = true;
                            }
                        }
                        Err(_) => return,
                    }
                }
                if state.closed {
                    break;
                }
                drop(state);
                // A descendant may keep the slave open after the shell exits.
                // Poll with a bound instead of waiting forever on a read/join.
                let mut pollfd = libc::pollfd {
                    fd: reader.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let polled = unsafe { libc::poll(&mut pollfd, 1, 250) };
                if polled < 0 {
                    break;
                }
                if polled == 0 {
                    if reader_exited.load(Ordering::Acquire) {
                        break;
                    }
                    continue;
                }
                let count = match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                if state.closed {
                    break;
                }
                state.queued = state.queued.saturating_add(count);
                drop(state);
                if read_app.emit("mcp-android-pty", json!({
                    "id":read_id, "kind":"output", "data":STANDARD.encode(&buffer[..count]), "bytes":count
                })).is_err() { unsafe { libc::kill(-pid, libc::SIGKILL); } break; }
            }
        });

        let wait_app = app.clone();
        let wait_entries = manager.entries.clone();
        thread::spawn(move || {
            let code = child
                .wait()
                .ok()
                .and_then(|status| status.code())
                .unwrap_or(-1);
            child_exited.store(true, Ordering::Release);
            // Drain the PTY's final output before notifying the renderer of exit.
            let _ = reader_handle.join();
            let mut entries = wait_entries.lock().unwrap_or_else(|e| e.into_inner());
            if entries.get(&id).is_some_and(|entry| entry.pid == pid) {
                if let Some(entry) = entries.remove(&id) {
                    let (lock, signal) = &*entry.flow;
                    if let Ok(mut state) = lock.lock() {
                        state.closed = true;
                        signal.notify_all();
                    }
                }
                drop(entries);
                let _ = wait_app.emit(
                    "mcp-android-pty",
                    json!({
                        "id":id, "kind":"exit", "exitCode":code
                    }),
                );
            }
        });
        Ok(
            json!({"id":receipt_id, "shell":"sh", "cwd":cwd, "projectId":project_id,
                  "cols":cols, "rows":rows, "started":true}),
        )
    }

    pub fn write(
        app: AppHandle,
        id: String,
        data: String,
        user_initiated: bool,
    ) -> Result<(), String> {
        validate_write(&id, &data, user_initiated)?;
        let manager = app.state::<AndroidPty>();
        let writer = manager
            .entries
            .lock()
            .map_err(|_| "PTY state lock failed")?
            .get(&id)
            .ok_or("Terminal is closed")?
            .writer
            .clone();
        let mut writer = writer.lock().map_err(|_| "PTY write lock failed")?;
        writer
            .write_all(data.as_bytes())
            .map_err(|e| format!("Terminal input failed: {e}"))
    }

    pub fn resize(app: AppHandle, id: String, cols: u16, rows: u16) -> Result<(), String> {
        if !valid_id(&id) || !(2..=300).contains(&cols) || !(1..=150).contains(&rows) {
            return Err("Invalid terminal resize".into());
        }
        let manager = app.state::<AndroidPty>();
        let writer = manager
            .entries
            .lock()
            .map_err(|_| "PTY state lock failed")?
            .get(&id)
            .ok_or("Terminal is closed")?
            .writer
            .clone();
        let writer = writer.lock().map_err(|_| "PTY write lock failed")?;
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(writer.as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
            return Err("Terminal resize failed".into());
        }
        Ok(())
    }

    pub fn acknowledge(app: AppHandle, id: String, count: usize) -> Result<(), String> {
        if !valid_id(&id) || count > 4096 {
            return Err("Invalid terminal output acknowledgement".into());
        }
        let manager = app.state::<AndroidPty>();
        let entries = manager
            .entries
            .lock()
            .map_err(|_| "PTY state lock failed")?;
        let entry = entries.get(&id).ok_or("Terminal is closed")?;
        let (lock, signal) = &*entry.flow;
        let mut flow = lock.lock().map_err(|_| "PTY flow lock failed")?;
        flow.queued = flow.queued.saturating_sub(count);
        if flow.queued < LOW_WATER {
            signal.notify_one();
        }
        Ok(())
    }

    pub fn close(app: AppHandle, id: String, user_initiated: bool) -> Result<(), String> {
        if !user_initiated || !valid_id(&id) {
            return Err("Closing a terminal requires an explicit user action".into());
        }
        let manager = app.state::<AndroidPty>();
        if let Some(entry) = manager
            .entries
            .lock()
            .map_err(|_| "PTY state lock failed")?
            .remove(&id)
        {
            stop(entry);
        }
        Ok(())
    }
}

#[cfg(target_os = "android")]
pub use platform::{set_foreground, AndroidPty};

#[tauri::command]
pub async fn android_pty_status(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        platform::status(app)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(json!({"supported":false,"kind":"android-in-app-pty","active":0,"termuxRequired":false}))
    }
}

/// An on-demand, bounded process probe. This is separate from frequent PTY
/// status polling so app startup never blocks on external tool version checks.
#[tauri::command]
pub async fn android_pty_capabilities(app: AppHandle) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    {
        tauri::async_runtime::spawn_blocking(move || platform::capabilities(app))
            .await
            .map_err(|_| "Android PTY capability probe interrupted".to_owned())?
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(json!({
            "supported":false,
            "kind":"android-in-app-pty",
            "executionContext":"none",
            "termuxShared":false,
            "automatedCommandsAllowed":false,
            "nodeCodeExecutionProbeSucceeded":false,
            "nodeNpmCliProbesSucceeded":false,
            "npmInstallTested":false,
            "sshProjectTransportImplemented":false,
            "sshAuthenticationTested":false
        }))
    }
}

#[tauri::command]
pub async fn android_pty_create(
    app: AppHandle,
    id: String,
    project_id: Option<String>,
    cols: u16,
    rows: u16,
    user_initiated: bool,
) -> Result<Value, String> {
    validate_create(&id, project_id.as_deref(), cols, rows, user_initiated)?;
    #[cfg(target_os = "android")]
    {
        tauri::async_runtime::spawn_blocking(move || {
            platform::create(app, id, project_id, cols, rows, user_initiated)
        })
        .await
        .map_err(|_| "PTY startup interrupted".to_owned())?
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, id, project_id);
        Err("Interactive Android PTY is unavailable on Windows".into())
    }
}

#[tauri::command]
pub async fn android_pty_write(
    app: AppHandle,
    id: String,
    data: String,
    user_initiated: bool,
) -> Result<(), String> {
    validate_write(&id, &data, user_initiated)?;
    #[cfg(target_os = "android")]
    {
        tauri::async_runtime::spawn_blocking(move || platform::write(app, id, data, user_initiated))
            .await
            .map_err(|_| "PTY write interrupted".to_owned())?
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, id, data);
        Err("Interactive Android PTY is unavailable on Windows".into())
    }
}

#[tauri::command]
pub async fn android_pty_resize(
    app: AppHandle,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        platform::resize(app, id, cols, rows)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, id, cols, rows);
        Err("Interactive Android PTY is unavailable on Windows".into())
    }
}

#[tauri::command]
pub async fn android_pty_ack(app: AppHandle, id: String, count: usize) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        platform::acknowledge(app, id, count)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, id, count);
        Err("Interactive Android PTY is unavailable on Windows".into())
    }
}

#[tauri::command]
pub async fn android_pty_close(
    app: AppHandle,
    id: String,
    user_initiated: bool,
) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        platform::close(app, id, user_initiated)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, id, user_initiated);
        Err("Interactive Android PTY is unavailable on Windows".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "123e4567-e89b-42d3-a456-426614174000";
    #[test]
    fn session_requires_valid_id_dimensions_and_explicit_consent() {
        assert!(validate_create(ID, None, 80, 24, true).is_ok());
        assert!(validate_create(ID, Some(ID), 80, 24, true).is_ok());
        assert!(validate_create(ID, None, 80, 24, false).is_err());
        assert!(validate_create("../../secrets", None, 80, 24, true).is_err());
        assert!(validate_create(ID, Some("../a"), 80, 24, true).is_err());
        assert!(validate_create(ID, None, 0, 24, true).is_err());
        assert!(validate_create(ID, None, 80, 151, true).is_err());
    }
    #[test]
    fn terminal_input_denies_implicit_automation_and_oversized_payloads() {
        assert!(validate_write(ID, "ls\n", true).is_ok());
        assert!(validate_write(ID, "pwd\n", false).is_err());
        assert!(validate_write(ID, "", true).is_err());
        assert!(validate_write(ID, &"a".repeat(4097), true).is_err());
        assert!(validate_write("bad-id", "ls", true).is_err());
    }

    #[test]
    fn toolchain_probe_reports_an_actual_process_exit_not_a_filename_guess() {
        // This test runs a real locally compiled test executable with a
        // harmless version/help argument; it does not simulate Node/npm.
        let executable = std::env::current_exe().unwrap();
        let home = std::env::temp_dir();
        assert_eq!(
            probe_program(&executable, &["--help"], &home, std::time::Duration::from_secs(5)),
            (true, true, false)
        );
        let invalid = probe_program(
            &executable,
            &["--unrecognized-option-for-runtime-probe"],
            &home,
            std::time::Duration::from_secs(5),
        );
        assert_eq!(invalid, (true, false, false));
        let missing = probe_program(
            &home.join(format!("mcp-no-such-executable-{}", std::process::id())),
            &["--version"],
            &home,
            std::time::Duration::from_secs(1),
        );
        assert_eq!(missing, (false, false, false));
    }
}
