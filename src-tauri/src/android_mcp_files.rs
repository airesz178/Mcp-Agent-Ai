//! Bounded inbound MCP file reads and explicit file mutations. Every operation
//! is confined to one OS-authorized directory and refuses symlinks, traversal,
//! binary data, protected Android directories, and unbounded enumeration.
//!
//! On Android, the trusted native Step4 selection (if valid NOW) is preferred.
//! Otherwise only the app-private mcp-shared-readonly folder is accessible.
//! Its legacy name is retained to avoid moving user files; policy/config and
//! secrets live outside that folder and are never reachable here.
//! No JavaScript path, user-provided root, or remote root parameter is trusted.

use serde_json::{json, Map, Value};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

const MAX_PATH_BYTES: usize = 512;
const MAX_DEPTH: usize = 12;
const MAX_SEGMENT_BYTES: usize = 128;
const MAX_READ_BYTES: u64 = 12 * 1024;
const MAX_LIST_ENTRIES: usize = 96;
const MAX_SEARCH_VISITS: usize = 320;
const MAX_SEARCH_MATCHES: usize = 32;
const MAX_SEARCH_DEPTH: usize = 4;

#[derive(Clone)]
pub(crate) struct McpReadRoot {
    private_root: PathBuf,
    #[cfg(target_os = "android")]
    app: Option<tauri::AppHandle>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RootGrant {
    canonical: PathBuf,
    kind: RootKind,
    #[cfg(target_os = "android")]
    directory_identity: (u64, u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RootKind {
    Private,
    #[cfg(target_os = "android")]
    Selected(Vec<String>),
}

impl McpReadRoot {
    /// The constructor is called with app.path().app_data_dir(), not a path
    /// supplied by the renderer. This subfolder is deliberately isolated from
    /// app credentials and preferences, unlike sharing the entire data dir.
    pub(crate) fn from_app_dir(data_dir: &Path, _app: tauri::AppHandle) -> Self {
        let private_root = data_dir.join("mcp-shared-readonly");
        // Create only this app-owned directory. A pre-existing symlink is
        // never followed; check_root rejects anything not an actual directory.
        if !private_root.exists() {
            let _ = fs::create_dir(&private_root);
        }
        Self {
            private_root,
            #[cfg(target_os = "android")]
            app: Some(_app),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests(path: PathBuf) -> Self {
        Self {
            private_root: path,
            #[cfg(target_os = "android")]
            app: None,
        }
    }

    fn private_grant(&self) -> Result<RootGrant, &'static str> {
        let info = fs::symlink_metadata(&self.private_root)
            .map_err(|_| "App-private MCP folder unavailable")?;
        if info.file_type().is_symlink() || !info.is_dir() {
            return Err("App-private MCP folder unavailable");
        }
        let canonical = fs::canonicalize(&self.private_root)
            .map_err(|_| "App-private MCP folder unavailable")?;
        fs::read_dir(&canonical).map_err(|_| "App-private MCP folder unreadable")?;
        Ok(RootGrant {
            #[cfg(target_os = "android")]
            directory_identity: android_directory_identity(&canonical)?,
            canonical,
            kind: RootKind::Private,
        })
    }

    fn current_grant(&self) -> Result<RootGrant, &'static str> {
        #[cfg(target_os = "android")]
        if let Some(app) = &self.app {
            // This is the only trusted bridge to Kotlin SharedPreferences and
            // Android's live All-files access. It validates both the OS grant
            // and the selected directory on EVERY call, never JS-supplied paths.
            let status = crate::android_main_storage::status(app.clone())
                .map_err(|_| "Android storage status unavailable")?;
            let choice = crate::android_main_storage::selection_status(app.clone())
                .map_err(|_| "Android folder selection unavailable")?;
            if status["granted"] == true
                && status["readable"] == true
                && status["supported"] == true
                && choice["selected"] == true
            {
                let base = status["rootPath"]
                    .as_str()
                    .ok_or("Invalid Android storage root")?;
                let segments = choice["path"]
                    .as_array()
                    .ok_or("Invalid native folder selection")?
                    .iter()
                    .map(|value| value.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .ok_or("Invalid native folder selection")?;
                if segments.is_empty()
                    || segments.len() > 16
                    || segments.iter().any(|segment| !valid_segment(segment))
                    || protected_storage_relative(&segments)
                {
                    return Err("Unsafe Android folder selection");
                }
                let primary = PathBuf::from(base);
                let canonical_primary =
                    fs::canonicalize(&primary).map_err(|_| "Android storage root inaccessible")?;
                let mut selected = canonical_primary.clone();
                for segment in &segments {
                    selected.push(segment);
                    let info = fs::symlink_metadata(&selected)
                        .map_err(|_| "Selected Android folder unavailable")?;
                    if info.file_type().is_symlink() || !info.is_dir() {
                        return Err("Selected Android folder unavailable");
                    }
                }
                let canonical = fs::canonicalize(&selected)
                    .map_err(|_| "Selected Android folder unavailable")?;
                if !canonical.starts_with(&canonical_primary) || canonical == canonical_primary {
                    return Err("Selected folder outside Android storage");
                }
                fs::read_dir(&canonical).map_err(|_| "Selected folder unreadable")?;
                return Ok(RootGrant {
                    #[cfg(target_os = "android")]
                    directory_identity: android_directory_identity(&canonical)?,
                    canonical,
                    kind: RootKind::Selected(segments),
                });
            }
        }
        self.private_grant()
    }

    /// For writes, do not silently redirect to app-private storage when an
    /// existing selected Android folder loses its OS grant. Clear the native
    /// folder selection first to deliberately return to private storage.
    fn current_write_grant(&self) -> Result<RootGrant, &'static str> {
        let grant = self.current_grant()?;
        #[cfg(target_os = "android")]
        if let (Some(app), RootKind::Private) = (&self.app, &grant.kind) {
            let selection = crate::android_main_storage::selection_status(app.clone())
                .map_err(|_| "Android folder selection unavailable")?;
            if selection["selected"] == true {
                return Err("Previously selected Android folder is not currently authorized");
            }
        }
        Ok(grant)
    }

    pub(crate) fn available(&self) -> bool {
        self.current_grant().is_ok()
    }

    /// Advisory only: current OS approval + a writable-looking root. A
    /// successful status read cannot reserve write access, so run_write still
    /// checks the live grant and actual filesystem operations independently.
    pub(crate) fn write_available(&self) -> bool {
        self.current_write_grant()
            .ok()
            .and_then(|grant| fs::metadata(grant.canonical).ok())
            .is_some_and(|metadata| metadata.is_dir() && !metadata.permissions().readonly())
    }

    /// Root is looked up both before and after reading, preventing a revoked
    /// selection, changed folder, or changed scope from returning stale data.
    pub(crate) fn run(
        &self,
        name: &str,
        args: &Map<String, Value>,
    ) -> Result<String, &'static str> {
        let grant = self.current_grant()?;
        let result = match name {
            "files/browse" => browse(&grant, args),
            "files/search" => search(&grant, args),
            "files/read" => read_utf8(&grant, args),
            "files/metadata" => metadata(&grant, args),
            _ => Err("Unknown file tool"),
        }?;
        if self.current_grant()? != grant {
            return Err("Approved folder or OS grant changed during read");
        }
        Ok(result.to_string())
    }

    /// File-mutation schemas (all fields must be exact and UTF-8):
    /// - files/create:      {path, content}
    /// - files/edit:        {path, expectedText, content} (whole-file compare)
    /// - files/apply_patch: {path, oldText, newText} (ONE exact substring
    ///                      replacement, NOT a unified diff or patch script)
    /// - files/move:        {from, to} (no destination overwrite)
    /// - files/delete:      {path} (regular UTF-8 file of at most 12 KiB)
    /// Only the MCP policy gate may authorize these tools; no mutation is
    /// implied by the presence of an existing selection or a read grant.
    pub(crate) fn run_write(
        &self,
        name: &str,
        args: &Map<String, Value>,
    ) -> Result<String, &'static str> {
        let grant = self.current_write_grant()?;
        let result = match name {
            "files/create" => create_file(self, &grant, args),
            "files/edit" => edit_file(self, &grant, args),
            "files/apply_patch" => apply_exact_patch(self, &grant, args),
            "files/move" => move_file(self, &grant, args),
            "files/delete" => delete_file(self, &grant, args),
            _ => Err("Unknown file mutation tool"),
        }?;
        if self.current_write_grant()? != grant {
            // The mutation may have committed before the OS revoked access;
            // never report it as an authorized, verified result in that case.
            return Err("Approved folder or OS grant changed during mutation");
        }
        Ok(result.to_string())
    }
}

#[cfg(target_os = "android")]
fn android_directory_identity(path: &Path) -> Result<(u64, u64), &'static str> {
    use std::os::unix::fs::MetadataExt;
    let info = fs::symlink_metadata(path).map_err(|_| "Approved Android folder unavailable")?;
    if info.file_type().is_symlink() || !info.is_dir() {
        return Err("Approved Android folder is not a real directory");
    }
    Ok((info.dev(), info.ino()))
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_SEGMENT_BYTES
        && segment != "."
        && segment != ".."
        && !segment.starts_with('.')
        && !segment.contains(['/', '\\', ':'])
        && !segment.chars().any(char::is_control)
}

fn protected_storage_relative(path: &[String]) -> bool {
    path.first()
        .is_some_and(|part| part.eq_ignore_ascii_case(".android_secure"))
        || (path.len() >= 2
            && path[0].eq_ignore_ascii_case("Android")
            && (path[1].eq_ignore_ascii_case("data") || path[1].eq_ignore_ascii_case("obb")))
}

fn parse_path(value: Option<&Value>, required: bool) -> Result<Vec<String>, &'static str> {
    let path = match value {
        Some(Value::String(path)) => path.as_str(),
        None if !required => "",
        _ => return Err("Path must be a relative UTF-8 string"),
    };
    if path.len() > MAX_PATH_BYTES
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains('\\')
        || path.contains(':')
        || path.chars().any(char::is_control)
    {
        return Err("Invalid relative path");
    }
    if path.is_empty() {
        return if required {
            Err("File path required")
        } else {
            Ok(Vec::new())
        };
    }
    let segments: Vec<String> = path.split('/').map(str::to_owned).collect();
    if segments.len() > MAX_DEPTH || segments.iter().any(|s| !valid_segment(s)) {
        return Err("Unsafe relative path");
    }
    Ok(segments)
}

fn only_keys(args: &Map<String, Value>, keys: &[&str]) -> Result<(), &'static str> {
    if args.keys().any(|name| !keys.contains(&name.as_str())) {
        return Err("Unknown tool argument");
    }
    Ok(())
}

/// Canonical containment and symlink refusal on every component. The Android
/// openat implementation below additionally prevents final-component races.
fn checked_path(grant: &RootGrant, segments: &[String]) -> Result<PathBuf, &'static str> {
    let mut path = grant.canonical.clone();
    for segment in segments {
        if !valid_segment(segment) {
            return Err("Unsafe relative path");
        }
        path.push(segment);
        let info = fs::symlink_metadata(&path).map_err(|_| "Path unavailable")?;
        if info.file_type().is_symlink() {
            return Err("Symlink access refused");
        }
    }
    let canonical = fs::canonicalize(&path).map_err(|_| "Path unavailable")?;
    if !canonical.starts_with(&grant.canonical) {
        return Err("Path escapes approved folder");
    }
    Ok(canonical)
}

/// Open a path relative to a pinned root. Android uses openat with NOFOLLOW
/// for EVERY component, not just the final file. This blocks symlink swaps
/// between resolving a shared-storage path and opening it.
fn open_target(
    grant: &RootGrant,
    segments: &[String],
    directory: bool,
) -> Result<File, &'static str> {
    let _ = checked_path(grant, segments)?;
    #[cfg(target_os = "android")]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::fs::OpenOptionsExt;
        let mut current = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&grant.canonical)
            .map_err(|_| "Approved folder cannot be opened")?;
        for (i, segment) in segments.iter().enumerate() {
            let name = CString::new(segment.as_bytes()).map_err(|_| "Invalid file path")?;
            let needs_dir = i + 1 < segments.len() || directory;
            let flags = libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | if needs_dir { libc::O_DIRECTORY } else { 0 };
            let descriptor = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
            if descriptor < 0 {
                return Err("File or folder cannot be opened");
            }
            current = unsafe { File::from_raw_fd(descriptor) };
        }
        return Ok(current);
    }
    #[cfg(not(target_os = "android"))]
    {
        let canonical = checked_path(grant, segments)?;
        let file = File::open(canonical).map_err(|_| "File or folder cannot be opened")?;
        if directory && !file.metadata().map_err(|_| "Folder unavailable")?.is_dir() {
            return Err("Expected folder");
        }
        Ok(file)
    }
}

fn read_entries(
    grant: &RootGrant,
    relative: &[String],
) -> Result<Vec<(String, bool, u64)>, &'static str> {
    #[cfg(target_os = "android")]
    let folder = open_target(grant, relative, true)?;
    #[cfg(target_os = "android")]
    let location = {
        use std::os::fd::AsRawFd;
        if !folder
            .metadata()
            .map_err(|_| "Folder unavailable")?
            .is_dir()
        {
            return Err("Expected folder");
        }
        // Keep the descriptor alive until the iterator has been consumed.
        // It pins the checked directory even if a shared folder is renamed.
        PathBuf::from(format!("/proc/self/fd/{}", folder.as_raw_fd()))
    };
    #[cfg(not(target_os = "android"))]
    let location = {
        let dir = checked_path(grant, relative)?;
        if !fs::metadata(&dir)
            .map_err(|_| "Folder unavailable")?
            .is_dir()
        {
            return Err("Expected folder");
        }
        dir
    };

    let mut entries = Vec::new();
    let iterator = fs::read_dir(location).map_err(|_| "Folder unreadable")?;
    for entry in iterator.take(MAX_LIST_ENTRIES + 1) {
        let entry = entry.map_err(|_| "Folder entry unavailable")?;
        let name = match entry.file_name().into_string() {
            Ok(name) if valid_segment(&name) => name,
            _ => continue,
        };
        // Never call DirEntry.metadata here: it follows symlinks, including
        // one swapped into place after the directory entry was obtained.
        let info = fs::symlink_metadata(entry.path()).map_err(|_| "Folder entry unavailable")?;
        let kind = info.file_type();
        if kind.is_symlink() || (!kind.is_file() && !kind.is_dir()) {
            continue;
        }
        let size = if kind.is_file() { info.len() } else { 0 };
        entries.push((name, kind.is_dir(), size));
    }
    // Bounded and stable ordering without revealing host absolute paths.
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

fn browse(grant: &RootGrant, args: &Map<String, Value>) -> Result<Value, &'static str> {
    only_keys(args, &["path"])?;
    let relative = parse_path(args.get("path"), false)?;
    let entries = read_entries(grant, &relative)?;
    let truncated = entries.len() > MAX_LIST_ENTRIES;
    let list: Vec<Value> = entries
        .into_iter()
        .take(MAX_LIST_ENTRIES)
        .map(|(name, directory, bytes)| {
            json!({
                "name":name,
                "type":if directory {"directory"} else {"file"},
                "bytes":bytes
            })
        })
        .collect();
    Ok(json!({
        "path":relative.join("/"),
        "entries":list,
        "truncated":truncated
    }))
}

fn search(grant: &RootGrant, args: &Map<String, Value>) -> Result<Value, &'static str> {
    only_keys(args, &["path", "query"])?;
    let relative = parse_path(args.get("path"), false)?;
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or("Search query required")?;
    if query.len() < 2 || query.len() > 64 || query.chars().any(char::is_control) {
        return Err("Invalid search query");
    }
    let needle = query.to_lowercase();
    let mut stack = vec![(relative, 0usize)];
    let mut visited = 0usize;
    let mut matches = Vec::new();
    while let Some((path, depth)) = stack.pop() {
        let entries = read_entries(grant, &path)?;
        for (name, directory, _bytes) in entries {
            visited += 1;
            if visited > MAX_SEARCH_VISITS || matches.len() >= MAX_SEARCH_MATCHES {
                break;
            }
            let mut child = path.clone();
            child.push(name.clone());
            if name.to_lowercase().contains(&needle) {
                matches.push(json!({
                    "path":child.join("/"),
                    "type":if directory {"directory"} else {"file"}
                }));
            }
            if directory
                && depth < MAX_SEARCH_DEPTH
                && child.len() <= MAX_DEPTH
                && stack.len() < MAX_SEARCH_VISITS
            {
                stack.push((child, depth + 1));
            }
        }
        if visited > MAX_SEARCH_VISITS || matches.len() >= MAX_SEARCH_MATCHES {
            break;
        }
    }
    Ok(json!({
        "query":query,
        "matches":matches,
        "truncated":visited > MAX_SEARCH_VISITS || matches.len() >= MAX_SEARCH_MATCHES
    }))
}

fn read_utf8(grant: &RootGrant, args: &Map<String, Value>) -> Result<Value, &'static str> {
    only_keys(args, &["path"])?;
    let relative = parse_path(args.get("path"), true)?;
    let mut file = open_target(grant, &relative, false)?;
    let metadata = file.metadata().map_err(|_| "File unavailable")?;
    if !metadata.is_file() {
        return Err("Expected regular file");
    }
    if metadata.len() > MAX_READ_BYTES {
        return Err("File exceeds UTF-8 read limit");
    }
    let mut data = Vec::with_capacity((metadata.len() + 1) as usize);
    file.by_ref()
        .take(MAX_READ_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|_| "File unreadable")?;
    if data.len() as u64 > MAX_READ_BYTES {
        return Err("File exceeds UTF-8 read limit");
    }
    let text = String::from_utf8(data).map_err(|_| "Binary or invalid UTF-8 file")?;
    Ok(json!({
        "path":relative.join("/"),
        "text":text,
        "bytes":text.len()
    }))
}

fn metadata(grant: &RootGrant, args: &Map<String, Value>) -> Result<Value, &'static str> {
    only_keys(args, &["path"])?;
    let relative = parse_path(args.get("path"), false)?;
    #[cfg(target_os = "android")]
    let meta = open_target(grant, &relative, false)?
        .metadata()
        .map_err(|_| "File metadata unavailable")?;
    #[cfg(not(target_os = "android"))]
    let meta =
        fs::metadata(checked_path(grant, &relative)?).map_err(|_| "File metadata unavailable")?;
    if !meta.is_file() && !meta.is_dir() {
        return Err("Unsupported file type");
    }
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|time| time.as_millis().min(u128::from(u64::MAX)) as u64);
    Ok(json!({
        "path":relative.join("/"),
        "type":if meta.is_dir() {"directory"} else {"file"},
        "bytes":meta.len(),
        "readonly":meta.permissions().readonly(),
        "modifiedMs":modified_ms
    }))
}

/// A write path obeys the read tool's 512-byte/depth/hidden-name rules plus
/// the Android protected subtree exclusions, even within the private root.
fn write_path(value: Option<&Value>) -> Result<Vec<String>, &'static str> {
    let path = parse_path(value, true)?;
    if protected_storage_relative(&path)
        || path.windows(2).any(|pair| {
            pair[0].eq_ignore_ascii_case("Android")
                && (pair[1].eq_ignore_ascii_case("data") || pair[1].eq_ignore_ascii_case("obb"))
        })
    {
        return Err("Protected Android directory refused");
    }
    // Windows host tests must not accidentally address DOS device aliases;
    // the same names are rejected on Android for portable behavior.
    if path.iter().any(|segment| {
        let base = segment.split('.').next().unwrap_or("").to_ascii_uppercase();
        matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (base.len() == 4
                && (base.starts_with("COM") || base.starts_with("LPT"))
                && base.as_bytes()[3].is_ascii_digit()
                && base.as_bytes()[3] != b'0')
    }) {
        return Err("Reserved file name refused");
    }
    Ok(path)
}

fn write_text<'a>(
    args: &'a Map<String, Value>,
    key: &str,
    allow_empty: bool,
) -> Result<&'a str, &'static str> {
    let content = args
        .get(key)
        .and_then(Value::as_str)
        .ok_or("UTF-8 text argument required")?;
    if content.len() as u64 > MAX_READ_BYTES || content.contains('\0') {
        return Err("Text exceeds 12 KiB or contains binary NUL");
    }
    if !allow_empty && content.is_empty() {
        return Err("Nonempty replacement search text required");
    }
    Ok(content)
}

/// Holds a real directory descriptor on Android; all sensitive leaf operations
/// use *at() on that pinned parent, never user-concatenated absolute paths.
struct WriteParent {
    #[cfg(target_os = "android")]
    dir: File,
    #[cfg(not(target_os = "android"))]
    path: PathBuf,
}

fn parent_for_write(grant: &RootGrant, segments: &[String]) -> Result<WriteParent, &'static str> {
    if segments.is_empty() {
        return Err("File path required");
    }
    let parents = &segments[..segments.len() - 1];
    let _ = checked_path(grant, parents)?;
    #[cfg(target_os = "android")]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::fs::OpenOptionsExt;
        let mut dir = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(&grant.canonical)
            .map_err(|_| "Approved write root unavailable")?;
        if !dir
            .metadata()
            .map_err(|_| "Approved write root unavailable")?
            .is_dir()
        {
            return Err("Approved write root is not a folder");
        }
        use std::os::unix::fs::MetadataExt;
        let info = dir
            .metadata()
            .map_err(|_| "Approved write root unavailable")?;
        if (info.dev(), info.ino()) != grant.directory_identity {
            return Err("Approved write root changed since native selection");
        }
        for component in parents {
            let name = CString::new(component.as_bytes()).map_err(|_| "Invalid file path")?;
            let fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err("Write parent directory unavailable or unsafe");
            }
            dir = unsafe { File::from_raw_fd(fd) };
        }
        return Ok(WriteParent { dir });
    }
    #[cfg(not(target_os = "android"))]
    {
        let path = checked_path(grant, parents)?;
        if !fs::symlink_metadata(&path)
            .map_err(|_| "Write parent unavailable")?
            .is_dir()
        {
            return Err("Expected a real write parent directory");
        }
        Ok(WriteParent { path })
    }
}

/// A write never follows a symlink, including at the final leaf. O_NONBLOCK
/// prevents an attacker-supplied FIFO from hanging Android while we inspect it.
fn read_write_target(
    parent: &WriteParent,
    leaf: &str,
) -> Result<(String, fs::Metadata), &'static str> {
    #[cfg(target_os = "android")]
    let mut file = {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        let name = CString::new(leaf).map_err(|_| "Invalid file path")?;
        let fd = unsafe {
            libc::openat(
                parent.dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err("Write target cannot be opened safely");
        }
        unsafe { File::from_raw_fd(fd) }
    };
    #[cfg(not(target_os = "android"))]
    let mut file = {
        let path = parent.path.join(leaf);
        let meta = fs::symlink_metadata(&path).map_err(|_| "Write target unavailable")?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err("Only regular non-symlink files can be modified");
        }
        File::open(path).map_err(|_| "Write target cannot be opened safely")?
    };
    let metadata = file.metadata().map_err(|_| "Write target unavailable")?;
    if !metadata.is_file() || metadata.len() > MAX_READ_BYTES {
        return Err("Only regular UTF-8 files up to 12 KiB may be modified");
    }
    let mut data = Vec::with_capacity((metadata.len() + 1) as usize);
    file.by_ref()
        .take(MAX_READ_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|_| "Write target unreadable")?;
    if data.len() as u64 > MAX_READ_BYTES {
        return Err("Write target exceeds 12 KiB");
    }
    let text = String::from_utf8(data).map_err(|_| "Binary or invalid UTF-8 write target")?;
    if text.contains('\0') {
        return Err("Binary or invalid UTF-8 write target");
    }
    Ok((text, metadata))
}

/// Refuse any pre-existing target (including symlink/directory) before the
/// *atomic* no-replace commit. The preflight is for good errors, not the
/// authorization boundary: renameat2 or hard_link does the atomic guard.
fn require_absent(parent: &WriteParent, leaf: &str) -> Result<(), &'static str> {
    #[cfg(target_os = "android")]
    {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        let name = CString::new(leaf).map_err(|_| "Invalid file path")?;
        let mut info: libc::stat = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::fstatat(
                parent.dir.as_raw_fd(),
                name.as_ptr(),
                &mut info,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            return Err("Destination already exists");
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT) {
            return Err("Destination cannot be checked safely");
        }
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        match fs::symlink_metadata(parent.path.join(leaf)) {
            Ok(_) => Err("Destination already exists"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("Destination cannot be checked safely"),
        }
    }
}

fn same_file_identity(first: &fs::Metadata, second: &fs::Metadata) -> bool {
    if !first.is_file() || !second.is_file() || first.len() != second.len() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return first.dev() == second.dev()
            && first.ino() == second.ino()
            && first.mtime() == second.mtime()
            && first.mtime_nsec() == second.mtime_nsec()
            && first.ctime() == second.ctime()
            && first.ctime_nsec() == second.ctime_nsec();
    }
    #[cfg(not(unix))]
    {
        first.modified().ok() == second.modified().ok()
            && first.created().ok() == second.created().ok()
    }
}

/// Refuse stale target content or inode swaps just before destructive commit.
/// This is a best-effort compare-and-swap (the external filesystem has no
/// transactional CAS); the Android parent FD remains pinned throughout.
fn verify_unchanged(
    parent: &WriteParent,
    leaf: &str,
    original_text: &str,
    original_metadata: &fs::Metadata,
) -> Result<(), &'static str> {
    let (now, info) = read_write_target(parent, leaf)?;
    if now != original_text || !same_file_identity(original_metadata, &info) {
        return Err("File changed since it was read");
    }
    Ok(())
}

struct TemporaryWrite<'a> {
    parent: &'a WriteParent,
    name: String,
    committed: bool,
}

impl Drop for TemporaryWrite<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = remove_leaf(self.parent, &self.name);
        }
    }
}

/// Write to a hidden, unpredictable O_EXCL staging inode first, then expose
/// the fully flushed bytes through one atomic rename/link operation.
fn stage_text<'a>(
    parent: &'a WriteParent,
    content: &str,
) -> Result<TemporaryWrite<'a>, &'static str> {
    let name = format!(".mcp-tmp-{}", uuid::Uuid::new_v4().simple());
    #[cfg(target_os = "android")]
    let mut file = {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        let temp = CString::new(name.as_str()).map_err(|_| "Invalid staging filename")?;
        let fd = unsafe {
            libc::openat(
                parent.dir.as_raw_fd(),
                temp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err("Cannot create secure temporary file");
        }
        unsafe { File::from_raw_fd(fd) }
    };
    #[cfg(not(target_os = "android"))]
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(parent.path.join(&name))
        .map_err(|_| "Cannot create secure temporary file")?;
    let staging = TemporaryWrite {
        parent,
        name,
        committed: false,
    };
    let written =
        std::io::Write::write_all(&mut file, content.as_bytes()).and_then(|_| file.sync_all());
    drop(file);
    written.map_err(|_| "Cannot write or flush temporary file")?;
    Ok(staging)
}

fn remove_leaf(parent: &WriteParent, leaf: &str) -> Result<(), &'static str> {
    #[cfg(target_os = "android")]
    {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        let name = CString::new(leaf).map_err(|_| "Invalid file path")?;
        if unsafe { libc::unlinkat(parent.dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err("Cannot remove file");
        }
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        fs::remove_file(parent.path.join(leaf)).map_err(|_| "Cannot remove file")
    }
}

fn replace_from_stage(parent: &WriteParent, temp: &str, dest: &str) -> Result<(), &'static str> {
    #[cfg(target_os = "android")]
    {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        let source = CString::new(temp).map_err(|_| "Invalid temporary filename")?;
        let destination = CString::new(dest).map_err(|_| "Invalid destination filename")?;
        if unsafe {
            libc::renameat(
                parent.dir.as_raw_fd(),
                source.as_ptr(),
                parent.dir.as_raw_fd(),
                destination.as_ptr(),
            )
        } != 0
        {
            return Err("Cannot atomically replace file");
        }
        let _ = parent.dir.sync_all();
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        fs::rename(parent.path.join(temp), parent.path.join(dest))
            .map_err(|_| "Cannot atomically replace file")
    }
}

fn no_replace_move(
    from_parent: &WriteParent,
    from_name: &str,
    to_parent: &WriteParent,
    to_name: &str,
) -> Result<(), &'static str> {
    #[cfg(target_os = "android")]
    {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        let source = CString::new(from_name).map_err(|_| "Invalid source filename")?;
        let destination = CString::new(to_name).map_err(|_| "Invalid destination filename")?;
        // renameat2(R​ENAME_NOREPLACE) is a Linux syscall available for the
        // Android arm64 target. Do not emulate a racy check+rename fallback.
        const RENAME_NOREPLACE: libc::c_uint = 1;
        let moved = unsafe {
            libc::syscall(
                libc::SYS_renameat2 as libc::c_long,
                from_parent.dir.as_raw_fd(),
                source.as_ptr(),
                to_parent.dir.as_raw_fd(),
                destination.as_ptr(),
                RENAME_NOREPLACE,
            )
        };
        if moved != 0 {
            return Err("Cannot move file without overwriting target");
        }
        let _ = from_parent.dir.sync_all();
        let _ = to_parent.dir.sync_all();
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        // Atomic no-overwrite destination creation on Windows/host tests. In
        // contrast to `rename`, hard_link refuses an existing destination.
        // Both parents are under the same approved filesystem tree.
        let source = from_parent.path.join(from_name);
        let dest = to_parent.path.join(to_name);
        fs::hard_link(&source, &dest).map_err(|_| "Cannot move file without overwriting target")?;
        fs::remove_file(&source).map_err(|_| "Cannot finish moving file")
    }
}

fn create_file(
    root: &McpReadRoot,
    grant: &RootGrant,
    args: &Map<String, Value>,
) -> Result<Value, &'static str> {
    only_keys(args, &["path", "content"])?;
    let relative = write_path(args.get("path"))?;
    let text = write_text(args, "content", true)?;
    let parent = parent_for_write(grant, &relative)?;
    let leaf = relative.last().ok_or("File path required")?;
    require_absent(&parent, leaf)?;
    let mut stage = stage_text(&parent, text)?;
    if root.current_write_grant()? != *grant {
        return Err("Approved folder or OS grant changed before create");
    }
    no_replace_move(&parent, &stage.name, &parent, leaf)?;
    stage.committed = true;
    Ok(json!({"path":relative.join("/"),"bytes":text.len(),"created":true}))
}

fn atomic_replace_text(
    root: &McpReadRoot,
    grant: &RootGrant,
    relative: &[String],
    original: &str,
    original_meta: &fs::Metadata,
    next: &str,
) -> Result<(), &'static str> {
    if next.len() as u64 > MAX_READ_BYTES {
        return Err("Replacement exceeds 12 KiB");
    }
    let parent = parent_for_write(grant, relative)?;
    let leaf = relative.last().ok_or("File path required")?;
    verify_unchanged(&parent, leaf, original, original_meta)?;
    let mut stage = stage_text(&parent, next)?;
    if root.current_write_grant()? != *grant {
        return Err("Approved folder or OS grant changed before replacement");
    }
    verify_unchanged(&parent, leaf, original, original_meta)?;
    replace_from_stage(&parent, &stage.name, leaf)?;
    stage.committed = true;
    Ok(())
}

fn edit_file(
    root: &McpReadRoot,
    grant: &RootGrant,
    args: &Map<String, Value>,
) -> Result<Value, &'static str> {
    only_keys(args, &["path", "expectedText", "content"])?;
    let relative = write_path(args.get("path"))?;
    let expected = write_text(args, "expectedText", true)?;
    let next = write_text(args, "content", true)?;
    let parent = parent_for_write(grant, &relative)?;
    let (current, info) = read_write_target(&parent, relative.last().unwrap())?;
    if current != expected {
        return Err("Expected file text does not match");
    }
    atomic_replace_text(root, grant, &relative, &current, &info, next)?;
    Ok(json!({"path":relative.join("/"),"bytes":next.len(),"edited":true}))
}

fn apply_exact_patch(
    root: &McpReadRoot,
    grant: &RootGrant,
    args: &Map<String, Value>,
) -> Result<Value, &'static str> {
    only_keys(args, &["path", "oldText", "newText"])?;
    let relative = write_path(args.get("path"))?;
    let old = write_text(args, "oldText", false)?;
    let replacement = write_text(args, "newText", true)?;
    let parent = parent_for_write(grant, &relative)?;
    let (current, info) = read_write_target(&parent, relative.last().unwrap())?;
    // Count overlapping occurrences too: replacing one "aaa" in "aaaa"
    // would otherwise hide a second match. UTF-8 char boundaries are safe.
    let mut occurrences = current
        .char_indices()
        .filter_map(|(offset, _)| current[offset..].starts_with(old).then_some(offset));
    let first = occurrences.next().ok_or("Patch search text not found")?;
    if occurrences.next().is_some() {
        return Err("Patch search text must match exactly once");
    }
    let new_size = current.len() - old.len() + replacement.len();
    if new_size as u64 > MAX_READ_BYTES {
        return Err("Patched file exceeds 12 KiB");
    }
    let mut next = String::with_capacity(new_size);
    next.push_str(&current[..first]);
    next.push_str(replacement);
    next.push_str(&current[first + old.len()..]);
    atomic_replace_text(root, grant, &relative, &current, &info, &next)?;
    Ok(json!({"path":relative.join("/"),"bytes":next.len(),"applied":true}))
}

fn move_file(
    root: &McpReadRoot,
    grant: &RootGrant,
    args: &Map<String, Value>,
) -> Result<Value, &'static str> {
    only_keys(args, &["from", "to"])?;
    let source = write_path(args.get("from"))?;
    let destination = write_path(args.get("to"))?;
    if source == destination {
        return Err("Source and destination must differ");
    }
    let from_parent = parent_for_write(grant, &source)?;
    let to_parent = parent_for_write(grant, &destination)?;
    let from_leaf = source.last().unwrap();
    let to_leaf = destination.last().unwrap();
    let (current, info) = read_write_target(&from_parent, from_leaf)?;
    require_absent(&to_parent, to_leaf)?;
    if root.current_write_grant()? != *grant {
        return Err("Approved folder or OS grant changed before move");
    }
    verify_unchanged(&from_parent, from_leaf, &current, &info)?;
    no_replace_move(&from_parent, from_leaf, &to_parent, to_leaf)?;
    Ok(json!({"from":source.join("/"),"to":destination.join("/"),"moved":true}))
}

fn delete_file(
    root: &McpReadRoot,
    grant: &RootGrant,
    args: &Map<String, Value>,
) -> Result<Value, &'static str> {
    only_keys(args, &["path"])?;
    let relative = write_path(args.get("path"))?;
    let parent = parent_for_write(grant, &relative)?;
    let leaf = relative.last().unwrap();
    let (current, info) = read_write_target(&parent, leaf)?;
    if root.current_write_grant()? != *grant {
        return Err("Approved folder or OS grant changed before delete");
    }
    verify_unchanged(&parent, leaf, &current, &info)?;
    remove_leaf(&parent, leaf)?;
    Ok(json!({"path":relative.join("/"),"deleted":true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Sample {
        root: PathBuf,
        source: McpReadRoot,
    }
    impl Sample {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mcp-file-tools-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(root.join("folder")).unwrap();
            fs::write(root.join("folder").join("hello.txt"), "hello Android\n").unwrap();
            let source = McpReadRoot::for_tests(root.clone());
            Self { root, source }
        }
    }
    impl Drop for Sample {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn params(items: &[(&str, &str)]) -> Map<String, Value> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), json!(*v)))
            .collect()
    }

    #[test]
    fn read_browse_search_metadata_operate_on_real_files() {
        let case = Sample::new();
        assert!(case.source.available());
        let browse: Value = serde_json::from_str(
            &case
                .source
                .run("files/browse", &params(&[("path", "folder")]))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(browse["entries"][0]["name"], "hello.txt");
        let read: Value = serde_json::from_str(
            &case
                .source
                .run("files/read", &params(&[("path", "folder/hello.txt")]))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(read["text"], "hello Android\n");
        let meta: Value = serde_json::from_str(
            &case
                .source
                .run("files/metadata", &params(&[("path", "folder/hello.txt")]))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(meta["bytes"], 14);
        let search: Value = serde_json::from_str(
            &case
                .source
                .run("files/search", &params(&[("query", "hello")]))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(search["matches"][0]["path"], "folder/hello.txt");
    }

    #[test]
    fn traversal_absolute_hidden_and_android_protected_paths_are_refused() {
        let case = Sample::new();
        for path in [
            "../outside.txt",
            "folder/../hello.txt",
            "/etc/passwd",
            "C:\\Users\\Public",
            "folder//file",
            ".secret",
            "folder/.hidden",
            "folder\\hello.txt",
            "folder/./hello.txt",
        ] {
            assert!(
                case.source
                    .run("files/read", &params(&[("path", path)]))
                    .is_err(),
                "accepted dangerous path {path}"
            );
        }
        assert!(protected_storage_relative(&[
            "Android".into(),
            "data".into()
        ]));
        assert!(protected_storage_relative(&[
            "android".into(),
            "obb".into()
        ]));
        assert!(protected_storage_relative(&[".android_secure".into()]));
        assert!(!protected_storage_relative(&[
            "Android".into(),
            "media".into()
        ]));
    }

    #[test]
    fn unbounded_file_query_and_non_utf8_are_refused() {
        let case = Sample::new();
        fs::write(
            case.root.join("big.txt"),
            vec![b'a'; MAX_READ_BYTES as usize + 1],
        )
        .unwrap();
        fs::write(case.root.join("binary.bin"), [0xff, 0x00, 0xfe]).unwrap();
        assert!(case
            .source
            .run("files/read", &params(&[("path", "big.txt")]))
            .is_err());
        assert!(case
            .source
            .run("files/read", &params(&[("path", "binary.bin")]))
            .is_err());
        assert!(case
            .source
            .run("files/search", &params(&[("query", "q")]))
            .is_err());
        assert!(case
            .source
            .run(
                "files/search",
                &params(&[("query", "z".repeat(200).as_str())])
            )
            .is_err());
        assert!(case
            .source
            .run(
                "files/read",
                &params(&[("path", "folder/hello.txt"), ("extra", "ignore")])
            )
            .is_err());
    }

    #[test]
    fn missing_root_revokes_availability_without_fallback() {
        let case = Sample::new();
        assert!(case.source.available());
        fs::remove_dir_all(&case.root).unwrap();
        assert!(!case.source.available());
        assert!(case.source.run("files/browse", &Map::new()).is_err());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn symlink_to_external_data_is_never_followed() {
        let case = Sample::new();
        let outside = std::env::temp_dir().join(format!(
            "mcp-outside-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&outside, "secret").unwrap();
        let link = case.root.join("outside.txt");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, &link);
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&outside, &link);
        if linked.is_ok() {
            assert!(case
                .source
                .run("files/read", &params(&[("path", "outside.txt")]))
                .is_err());
            assert!(
                case.source
                    .run("files/browse", &Map::new())
                    .unwrap()
                    .contains("hello.txt")
                    == false
            );
        }
        let _ = fs::remove_file(outside);
    }

    fn write_result(
        source: &McpReadRoot,
        tool: &str,
        args: &[(&str, &str)],
    ) -> Result<Value, &'static str> {
        source
            .run_write(tool, &params(args))
            .and_then(|text| serde_json::from_str(&text).map_err(|_| "Invalid mutation JSON"))
    }

    #[test]
    fn create_edit_exact_patch_move_delete_are_bounded_atomic_and_return_small_receipts() {
        let case = Sample::new();
        assert!(case.source.write_available());
        let created = write_result(
            &case.source,
            "files/create",
            &[("path", "folder/new.txt"), ("content", "hello 🌿\n")],
        )
        .unwrap();
        assert_eq!(created["created"], true);
        assert_eq!(created["bytes"], "hello 🌿\n".len());
        assert_eq!(
            fs::read_to_string(case.root.join("folder/new.txt")).unwrap(),
            "hello 🌿\n"
        );
        assert!(write_result(
            &case.source,
            "files/create",
            &[("path", "folder/new.txt"), ("content", "overwrite")],
        )
        .is_err());
        assert_eq!(
            fs::read_to_string(case.root.join("folder/new.txt")).unwrap(),
            "hello 🌿\n"
        );

        assert!(write_result(
            &case.source,
            "files/edit",
            &[
                ("path", "folder/new.txt"),
                ("expectedText", "stale"),
                ("content", "wrong")
            ],
        )
        .is_err());
        assert_eq!(
            write_result(
                &case.source,
                "files/edit",
                &[
                    ("path", "folder/new.txt"),
                    ("expectedText", "hello 🌿\n"),
                    ("content", "one flower + two flowers\n"),
                ],
            )
            .unwrap()["edited"],
            true
        );
        let applied = write_result(
            &case.source,
            "files/apply_patch",
            &[
                ("path", "folder/new.txt"),
                ("oldText", "two"),
                ("newText", "three"),
            ],
        )
        .unwrap();
        assert_eq!(applied["applied"], true);
        assert_eq!(
            fs::read_to_string(case.root.join("folder/new.txt")).unwrap(),
            "one flower + three flowers\n"
        );
        assert!(write_result(
            &case.source,
            "files/apply_patch",
            &[
                ("path", "folder/new.txt"),
                ("oldText", "two"),
                ("newText", "four")
            ],
        )
        .is_err());

        // renameat2 RENAME_NOREPLACE (Android) or hard-link no-replace (host)
        // must refuse existing files without changing either one.
        assert!(write_result(
            &case.source,
            "files/move",
            &[("from", "folder/new.txt"), ("to", "folder/hello.txt")],
        )
        .is_err());
        assert_eq!(
            fs::read_to_string(case.root.join("folder/hello.txt")).unwrap(),
            "hello Android\n"
        );
        let moved = write_result(
            &case.source,
            "files/move",
            &[("from", "folder/new.txt"), ("to", "moved.txt")],
        )
        .unwrap();
        assert_eq!(moved["moved"], true);
        assert!(!case.root.join("folder/new.txt").exists());
        assert_eq!(
            fs::read_to_string(case.root.join("moved.txt")).unwrap(),
            "one flower + three flowers\n"
        );
        assert_eq!(
            write_result(&case.source, "files/delete", &[("path", "moved.txt")],).unwrap()
                ["deleted"],
            true
        );
        assert!(!case.root.join("moved.txt").exists());
        assert!(write_result(&case.source, "files/delete", &[("path", "moved.txt")]).is_err());
        assert_eq!(
            fs::read_dir(case.root.join("folder"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(".mcp-tmp-"))
                .count(),
            0,
            "Successful and failed mutation attempts must not leave internal staging files"
        );
    }

    #[test]
    fn exact_patch_rejects_multiple_including_overlapping_matches_and_growth() {
        let case = Sample::new();
        fs::write(case.root.join("repeated.txt"), "aaaababa").unwrap();
        for (old, next) in [("aaa", "z"), ("ba", "z"), ("", "z"), ("missing", "z")] {
            assert!(
                write_result(
                    &case.source,
                    "files/apply_patch",
                    &[
                        ("path", "repeated.txt"),
                        ("oldText", old),
                        ("newText", next)
                    ],
                )
                .is_err(),
                "duplicate/missing search accepted: {old:?}"
            );
        }
        assert_eq!(
            fs::read_to_string(case.root.join("repeated.txt")).unwrap(),
            "aaaababa"
        );
        fs::write(
            case.root.join("near-limit.txt"),
            "a".repeat(MAX_READ_BYTES as usize),
        )
        .unwrap();
        assert!(
            write_result(
                &case.source,
                "files/apply_patch",
                &[
                    ("path", "near-limit.txt"),
                    ("oldText", "a"),
                    ("newText", "aaaa")
                ],
            )
            .is_err(),
            "many matching replacements must fail before any file write"
        );
        fs::write(
            case.root.join("near-limit.txt"),
            format!("{}unique", "a".repeat(MAX_READ_BYTES as usize - 6)),
        )
        .unwrap();
        assert!(
            write_result(
                &case.source,
                "files/apply_patch",
                &[
                    ("path", "near-limit.txt"),
                    ("oldText", "unique"),
                    ("newText", "more bytes")
                ],
            )
            .is_err(),
            "patch must refuse a final file exceeding 12 KiB"
        );
        assert_eq!(
            fs::metadata(case.root.join("near-limit.txt"))
                .unwrap()
                .len(),
            MAX_READ_BYTES
        );
    }

    #[test]
    fn write_rejects_traversal_hidden_paths_nonfiles_and_unsupported_schemas() {
        let case = Sample::new();
        for path in [
            "../escaped.txt",
            "folder/../escaped.txt",
            "/absolute.txt",
            "folder//invalid.txt",
            "folder/.private",
            ".hidden",
            "folder\\windows.txt",
            "Android/data/exposed.txt",
            "safe/Android/obb/exposed.txt",
            "CON.txt",
            "folder/NUL",
            "a/./b",
            "a/../../b",
            "a/b\nc",
        ] {
            assert!(
                write_result(
                    &case.source,
                    "files/create",
                    &[("path", path), ("content", "x")]
                )
                .is_err(),
                "accepted dangerous write path {path:?}"
            );
        }
        assert!(write_result(
            &case.source,
            "files/create",
            &[("path", "folder/hello.txt"), ("content", "clobber")],
        )
        .is_err());
        assert!(write_result(
            &case.source,
            "files/edit",
            &[("path", "folder"), ("expectedText", ""), ("content", "no")],
        )
        .is_err());
        assert!(write_result(&case.source, "files/delete", &[("path", "folder")]).is_err());
        assert!(write_result(
            &case.source,
            "files/move",
            &[("from", "folder"), ("to", "relocated")]
        )
        .is_err());
        assert!(write_result(
            &case.source,
            "files/move",
            &[("from", "folder/hello.txt"), ("to", "folder/hello.txt")],
        )
        .is_err());
        for tool in [
            "files/create",
            "files/edit",
            "files/apply_patch",
            "files/move",
            "files/delete",
        ] {
            assert!(
                write_result(
                    &case.source,
                    tool,
                    &[("path", "folder/hello.txt"), ("extra", "surprise")]
                )
                .is_err(),
                "accepted unexpected argument for {tool}"
            );
        }
        assert!(write_result(&case.source, "files/unknown", &[]).is_err());
    }

    #[test]
    fn mutations_refuse_oversize_binary_and_missing_files_without_overwrite() {
        let case = Sample::new();
        let too_big = "z".repeat(MAX_READ_BYTES as usize + 1);
        fs::write(case.root.join("oversize.txt"), &too_big).unwrap();
        fs::write(case.root.join("binary.txt"), [0, 255, 0]).unwrap();
        assert!(write_result(
            &case.source,
            "files/create",
            &[("path", "big-new.txt"), ("content", &too_big)]
        )
        .is_err());
        assert!(!case.root.join("big-new.txt").exists());
        for tool in ["files/delete", "files/move"] {
            let args = if tool == "files/move" {
                vec![("from", "oversize.txt"), ("to", "elsewhere.txt")]
            } else {
                vec![("path", "oversize.txt")]
            };
            assert!(write_result(&case.source, tool, &args).is_err());
        }
        assert!(write_result(&case.source, "files/delete", &[("path", "binary.txt")]).is_err());
        assert!(write_result(
            &case.source,
            "files/edit",
            &[
                ("path", "binary.txt"),
                ("expectedText", ""),
                ("content", "x")
            ],
        )
        .is_err());
        assert!(write_result(
            &case.source,
            "files/edit",
            &[
                ("path", "absent.txt"),
                ("expectedText", ""),
                ("content", "x")
            ],
        )
        .is_err());
        assert!(write_result(
            &case.source,
            "files/move",
            &[("from", "absent.txt"), ("to", "moved.txt")]
        )
        .is_err());
        assert_eq!(fs::read(case.root.join("binary.txt")).unwrap(), [0, 255, 0]);
        assert_eq!(
            fs::read_to_string(case.root.join("oversize.txt")).unwrap(),
            too_big
        );
    }

    #[test]
    fn revoked_or_replaced_root_prevents_mutations_and_reports_no_write_capability() {
        let case = Sample::new();
        let prior = case.source.current_grant().unwrap();
        assert!(case.source.write_available());
        fs::remove_dir_all(&case.root).unwrap();
        assert!(!case.source.write_available());
        assert!(case.source.current_grant().is_err());
        assert!(write_result(
            &case.source,
            "files/create",
            &[("path", "new.txt"), ("content", "x")]
        )
        .is_err());
        assert!(write_result(&case.source, "files/delete", &[("path", "gone.txt")]).is_err());
        fs::create_dir_all(&case.root).unwrap();
        let new_grant = case.source.current_grant().unwrap();
        assert_eq!(prior, new_grant);
        assert!(case.source.write_available());
        // Model a native folder-selection change between starting a write and
        // its commit. A request with an old grant may stage privately, but
        // must never publish the target after revalidation disagrees.
        let another = case.root.join("new-selected-folder");
        fs::create_dir_all(&another).unwrap();
        let changed = McpReadRoot::for_tests(another);
        assert_ne!(changed.current_grant().unwrap(), prior);
        assert!(create_file(
            &changed,
            &prior,
            &params(&[("path", "old.txt"), ("content", "x")])
        )
        .is_err());
        assert!(!case.root.join("old.txt").exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn mutations_never_follow_symlinks_outside_the_granted_root() {
        let case = Sample::new();
        let outside = std::env::temp_dir().join(format!(
            "mcp-write-external-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "private").unwrap();
        let link = case.root.join("shortcut");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, &link);
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(&outside, &link);
        if linked.is_ok() {
            assert!(write_result(
                &case.source,
                "files/create",
                &[("path", "shortcut/foreign.txt"), ("content", "x")]
            )
            .is_err());
            assert!(write_result(
                &case.source,
                "files/edit",
                &[
                    ("path", "shortcut/secret.txt"),
                    ("expectedText", "private"),
                    ("content", "x")
                ]
            )
            .is_err());
            assert!(write_result(
                &case.source,
                "files/delete",
                &[("path", "shortcut/secret.txt")]
            )
            .is_err());
            assert!(write_result(
                &case.source,
                "files/move",
                &[("from", "folder/hello.txt"), ("to", "shortcut/moved.txt")]
            )
            .is_err());
        }
        let file_link = case.root.join("pointing.txt");
        #[cfg(unix)]
        let linked_file = std::os::unix::fs::symlink(outside.join("secret.txt"), &file_link);
        #[cfg(windows)]
        let linked_file =
            std::os::windows::fs::symlink_file(outside.join("secret.txt"), &file_link);
        if linked_file.is_ok() {
            assert!(write_result(
                &case.source,
                "files/edit",
                &[
                    ("path", "pointing.txt"),
                    ("expectedText", "private"),
                    ("content", "x")
                ]
            )
            .is_err());
            assert!(
                write_result(&case.source, "files/delete", &[("path", "pointing.txt")]).is_err()
            );
            assert!(write_result(
                &case.source,
                "files/move",
                &[("from", "pointing.txt"), ("to", "new.txt")]
            )
            .is_err());
            assert!(write_result(
                &case.source,
                "files/create",
                &[("path", "pointing.txt"), ("content", "overwrite")]
            )
            .is_err());
        }
        assert_eq!(
            fs::read_to_string(outside.join("secret.txt")).unwrap(),
            "private"
        );
        assert!(!outside.join("foreign.txt").exists());
        assert!(!outside.join("moved.txt").exists());
        assert_eq!(
            fs::read_to_string(case.root.join("folder/hello.txt")).unwrap(),
            "hello Android\n"
        );
        let _ = fs::remove_dir_all(outside);
    }
}
