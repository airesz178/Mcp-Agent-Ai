//! Bounded operations for the app-owned workspace on Android. Writes require an
//! explicit per-call Rust boolean guard and are restricted to existing private
//! directories. This is NOT an operating-system shell or shared user storage.
use serde_json::{json, Value};
#[cfg(unix)]
use std::fs::File;
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};
use uuid::Uuid;

const MAX_FILES: usize = 100;
const MAX_READ: u64 = 16 * 1024;
const MAX_SEARCH_FILES: usize = 256;
const MAX_SEARCH_RESULTS: usize = 40;
const MAX_SEARCH_DEPTH: usize = 6;
const MAX_SEARCH_TEXT_BYTES: usize = 1024 * 1024;
const MAX_SEARCH_EXCERPT_CHARS: usize = 160;
const MAX_WRITE_BYTES: usize = 64 * 1024;
const WRITABLE_FOLDER: &str = "user-files";

#[derive(Clone, Copy)]
enum WriteMode {
    Create,
    Replace,
}

/// Canonical, app-private destination inside the dedicated `user-files/` root.
/// Reject all hidden components, Windows alternate data streams/device aliases,
/// junctions and symlinks. No missing directories are created implicitly.
fn private_write_path(home: &Path, relative: &str, mode: WriteMode) -> Result<PathBuf, String> {
    if !relative.starts_with("user-files/")
        || relative.len() > 256
        || relative.contains(['\0', '\r', '\n', '\\', ':'])
    {
        return Err("Invalid private workspace write path".into());
    }
    let root_meta = fs::symlink_metadata(home).map_err(|_| "Private workspace unavailable")?;
    if !root_meta.is_dir() || is_link_or_reparse(&root_meta) {
        return Err("Private workspace must be an actual directory".into());
    }
    let root = home
        .canonicalize()
        .map_err(|_| "Private workspace unavailable")?;
    let mut current = root.clone();
    let components: Vec<&str> = relative.split('/').collect();
    for (index, component) in components.iter().enumerate() {
        if component.is_empty()
            || *component == "."
            || *component == ".."
            || component.starts_with('.')
            || component.ends_with(['.', ' '])
            || component
                .chars()
                .any(|ch| ch.is_control() || "*?<>|\"".contains(ch))
            || Path::new(component).components().count() != 1
            || !matches!(
                Path::new(component).components().next(),
                Some(Component::Normal(_))
            )
        {
            return Err("Hidden or unsafe workspace path component".into());
        }
        // Windows device names refer to special files, even with extensions.
        let base = component
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end()
            .to_ascii_uppercase();
        if matches!(
            base.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "CLOCK$"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        ) {
            return Err("Reserved workspace path component".into());
        }
        current.push(component);
        if index + 1 < components.len() {
            let meta = fs::symlink_metadata(&current)
                .map_err(|_| "Workspace parent folder does not exist")?;
            if !meta.is_dir() || is_link_or_reparse(&meta) {
                return Err("Workspace parent must be a real directory".into());
            }
        }
    }
    let parent = current.parent().ok_or("Workspace parent missing")?;
    if !parent
        .canonicalize()
        .map_err(|_| "Workspace parent unavailable")?
        .starts_with(&root)
    {
        return Err("Access outside app workspace denied".into());
    }
    match fs::symlink_metadata(&current) {
        Ok(meta) if is_link_or_reparse(&meta) || !meta.is_file() => {
            Err("Workspace target must be a regular file, not a link or directory".into())
        }
        Ok(_) if matches!(mode, WriteMode::Create) => Err("Workspace file already exists".into()),
        Ok(_) => Ok(current),
        Err(e) if e.kind() == io::ErrorKind::NotFound && matches!(mode, WriteMode::Create) => {
            Ok(current)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err("Workspace file does not exist".into())
        }
        Err(_) => Err("Cannot inspect workspace target".into()),
    }
}

/// The only writable folder. It is created after explicit opt-in; the app root
/// and its session, keys, MCP, goal and skills data remain non-writable here.
fn ensure_user_files_root(home: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(home).map_err(|_| "Private workspace unavailable")?;
    if !meta.is_dir() || is_link_or_reparse(&meta) {
        return Err("Private workspace must be an actual directory".into());
    }
    let private = home.join(WRITABLE_FOLDER);
    match fs::symlink_metadata(&private) {
        Ok(meta) if meta.is_dir() && !is_link_or_reparse(&meta) => Ok(()),
        Ok(_) => Err("Private user-files root is not a real directory".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&private)
            .map_err(|_| "Cannot initialize private user-files folder".into()),
        Err(_) => Err("Cannot inspect private user-files folder".into()),
    }
}

fn is_link_or_reparse(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        } // FILE_ATTRIBUTE_REPARSE_POINT
    }
    false
}

#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(old_name: *const u16, new_name: *const u16, flags: u32) -> i32;
    }
    let old_name: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let new_name: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    const MOVEFILE_REPLACE_EXISTING: u32 = 1;
    const MOVEFILE_WRITE_THROUGH: u32 = 8;
    if unsafe {
        MoveFileExW(
            old_name.as_ptr(),
            new_name.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } != 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

/// Stages and syncs bytes before committing. Create uses an atomic hard-link,
/// which fails if the target exists. Replace uses an atomic same-directory rename.
/// Any pre-commit error preserves the original file; only our unique temporary
/// file is removed on failure. The caller must own and gate each write request.
fn write_text_impl(
    home: &Path,
    relative: &str,
    content: &str,
    allow_write: bool,
    mode: WriteMode,
    commit: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Result<Value, String> {
    if !allow_write {
        return Err("Explicit workspace write permission required".into());
    }
    if content.len() > MAX_WRITE_BYTES || content.contains('\0') {
        return Err("Workspace text must be UTF-8, NUL-free, and at most 64 KiB".into());
    }
    // Validate path syntax before making even the dedicated folder.
    if !relative.starts_with("user-files/")
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == ".." || part.starts_with('.'))
    {
        return Err("Writes are restricted to private user-files/ paths".into());
    }
    ensure_user_files_root(home)?;
    let target = private_write_path(home, relative, mode)?;
    if matches!(mode, WriteMode::Replace) {
        let metadata =
            fs::metadata(&target).map_err(|_| "Cannot inspect existing workspace file")?;
        if metadata.len() > MAX_WRITE_BYTES as u64 {
            return Err("Existing workspace file exceeds 64 KiB".into());
        }
        let old = fs::read(&target).map_err(|_| "Cannot read existing workspace file")?;
        if old.contains(&0) || std::str::from_utf8(&old).is_err() {
            return Err("Existing workspace file is not UTF-8 text".into());
        }
    }
    let parent = target.parent().ok_or("Workspace parent missing")?;
    let temporary = parent.join(format!(".cos-text-{}.tmp", Uuid::new_v4()));
    let staged = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|_| "Cannot stage workspace text")?;
        file.write_all(content.as_bytes())
            .map_err(|_| "Cannot write staged text")?;
        file.sync_all().map_err(|_| "Cannot sync staged text")?;
        drop(file);
        // Recheck the target after staging. The atomic Create commit also refuses
        // another writer's newly created target without overwriting its content.
        let _ = private_write_path(home, relative, mode)?;
        commit(&temporary, &target).map_err(|_| "Cannot commit staged workspace text")?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary); // only our own staging file
    staged?;
    // Android/Unix durability: sync parent after a namespace update where allowed.
    // A post-commit sync error cannot roll back the completed atomic operation.
    #[cfg(unix)]
    let _ = File::open(parent).and_then(|directory| directory.sync_all());
    Ok(json!({"path":relative,"bytes":content.len(),"written":true,
        "created":matches!(mode,WriteMode::Create),"scope":"Private application data"}))
}

/// Create one UTF-8 text file under an existing app-private directory.
/// `allow_write` must be true for this exact explicit UI action; false denies.
pub fn create_text(
    home: &Path,
    relative: &str,
    content: &str,
    allow_write: bool,
) -> Result<Value, String> {
    write_text_impl(
        home,
        relative,
        content,
        allow_write,
        WriteMode::Create,
        |from, to| fs::hard_link(from, to),
    )
}

/// Atomically replace one existing app-private UTF-8 text file (64 KiB max).
/// `allow_write` must be true for this exact explicit UI action; false denies.
pub fn replace_text(
    home: &Path,
    relative: &str,
    content: &str,
    allow_write: bool,
) -> Result<Value, String> {
    write_text_impl(
        home,
        relative,
        content,
        allow_write,
        WriteMode::Replace,
        atomic_replace,
    )
}

fn safe_path(home: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.len() > 256 || relative.contains('\0') {
        return Err("Invalid workspace path".into());
    }
    let mut path = home.to_owned();
    for c in Path::new(relative).components() {
        match c {
            Component::Normal(name) => path.push(name),
            Component::CurDir => {}
            _ => return Err("Path must remain inside the app workspace".into()),
        }
    }
    let root = home.canonicalize().map_err(|_| "Workspace unavailable")?;
    let target = path
        .canonicalize()
        .map_err(|_| "File or folder not found")?;
    if !target.starts_with(&root) {
        return Err("Access outside app workspace denied".into());
    }
    // Refuse symlinks on every segment rather than merely checking the target.
    let mut checked = home.to_owned();
    for component in Path::new(relative).components() {
        if let Component::Normal(part) = component {
            checked.push(part);
            if fs::symlink_metadata(&checked)
                .map_err(|_| "Path unavailable")?
                .file_type()
                .is_symlink()
            {
                return Err("Symbolic links not supported".into());
            }
        }
    }
    Ok(target)
}

pub fn list(home: &Path, relative: &str) -> Result<Value, String> {
    fs::create_dir_all(home).map_err(|_| "App workspace unavailable")?;
    let folder = safe_path(home, relative)?;
    if !folder.is_dir() {
        return Err("Not a folder".into());
    }
    let mut entries = Vec::new();
    for item in fs::read_dir(&folder)
        .map_err(|_| "Cannot list app workspace")?
        .take(MAX_FILES + 1)
    {
        if entries.len() >= MAX_FILES {
            return Err("Folder contains too many entries".into());
        }
        let item = item.map_err(|_| "Cannot read directory entry")?;
        let metadata = fs::symlink_metadata(item.path()).map_err(|_| "Cannot inspect entry")?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let name = item.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        entries.push(json!({"name": name, "directory": metadata.is_dir(),
            "bytes": if metadata.is_file() {metadata.len()} else {0}}));
    }
    entries.sort_by(|a, b| {
        let da = a["directory"].as_bool().unwrap_or(false);
        let db = b["directory"].as_bool().unwrap_or(false);
        db.cmp(&da)
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    Ok(
        json!({"path":relative, "entries":entries, "readOnly":true, "scope":"Private application data"}),
    )
}

pub fn read(home: &Path, relative: &str) -> Result<Value, String> {
    let path = safe_path(home, relative)?;
    let meta = fs::metadata(&path).map_err(|_| "File unavailable")?;
    if !meta.is_file() {
        return Err("Not a file".into());
    }
    if meta.len() > MAX_READ {
        return Err("Only small text files (16 KiB max) may be opened".into());
    }
    let bytes = fs::read(path).map_err(|_| "Cannot read app file")?;
    if bytes.contains(&0) {
        return Err("Binary file preview unavailable".into());
    }
    let content = String::from_utf8(bytes).map_err(|_| "Only UTF-8 text preview supported")?;
    Ok(json!({"path":relative, "content":content, "readOnly":true}))
}

/// Metadata is read from the canonical, bounded, symlink-free app workspace.
/// No absolute host paths are ever sent to the WebView.
pub fn properties(home: &Path, relative: &str) -> Result<Value, String> {
    let path = safe_path(home, relative)?;
    let meta = fs::metadata(path).map_err(|_| "Cannot inspect app file")?;
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_millis()).ok());
    Ok(
        json!({"path": relative, "name": Path::new(relative).file_name()
        .map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Workspace".into()),
        "directory": meta.is_dir(), "bytes": if meta.is_file() {meta.len()} else {0},
        "modifiedMs": modified_ms, "scope": "Private application data", "readOnly": true}),
    )
}

/// Deterministic name-only file search, never searches file contents, secrets,
/// hidden folders or the phone's general storage. Bounds are enforced even
/// when directories contain large numbers of items.
pub fn search(home: &Path, query: &str) -> Result<Value, String> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() || needle.len() > 80 || needle.contains(['\0', '\n', '\r']) {
        return Err("Search requires 1-80 characters".into());
    }
    fs::create_dir_all(home).map_err(|_| "App workspace unavailable")?;
    let root = home
        .canonicalize()
        .map_err(|_| "App workspace unavailable")?;
    let mut stack = vec![(root.clone(), String::new(), 0usize)];
    let mut examined = 0;
    let mut matches = Vec::new();
    while let Some((folder, prefix, depth)) = stack.pop() {
        let mut children = Vec::new();
        for item in fs::read_dir(&folder).map_err(|_| "Cannot search app workspace")? {
            examined += 1;
            if examined > MAX_SEARCH_FILES {
                return Err("Search reached its file safety limit".into());
            }
            let item = item.map_err(|_| "Cannot inspect directory entry")?;
            let name = item.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let meta = fs::symlink_metadata(item.path()).map_err(|_| "Cannot inspect file")?;
            if meta.file_type().is_symlink() {
                continue;
            }
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if relative.len() > 256 {
                continue;
            }
            if name.to_lowercase().contains(&needle) {
                matches.push(
                    json!({"path":relative,"name":name,"directory":meta.is_dir(),
                    "bytes": if meta.is_file() {meta.len()} else {0}}),
                );
            }
            if meta.is_dir() && depth < MAX_SEARCH_DEPTH {
                children.push((item.path(), relative, depth + 1));
            }
        }
        children.sort_by(|a, b| a.1.cmp(&b.1));
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    matches.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    let total = matches.len();
    matches.truncate(MAX_SEARCH_RESULTS);
    Ok(
        json!({"query":query,"results":matches,"total":total,"truncated":total>MAX_SEARCH_RESULTS,
        "scope":"Private application data","readOnly":true}),
    )
}

/// Disallow unsafe, invisible and non-portable names, including Windows
/// special devices and alternate-stream syntax. Search never follows a name
/// that could mean a different filesystem object on another platform.
fn search_component_ok(name: &str) -> bool {
    if name.is_empty()
        || name.len() > 255
        || name.starts_with('.')
        || name.ends_with(['.', ' '])
        || name.trim() != name
        || name
            .chars()
            .any(|c| c.is_control() || search_invisible(c) || "/\\:*?\"<>|".contains(c))
        || !matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
        || Path::new(name).components().count() != 1
    {
        return false;
    }
    let base = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    !matches!(
        base.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "CLOCK$"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn search_invisible(ch: char) -> bool {
    matches!(
        ch as u32,
        0x00AD | 0x034F | 0x061C | 0x115F..=0x1160 | 0x1680
            | 0x17B4..=0x17B5 | 0x180B..=0x180F
            | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F
            | 0x2800 | 0x3164 | 0xE000..=0xF8FF | 0xFDD0..=0xFDEF
            | 0xFE00..=0xFE0F | 0xFEFF | 0xFFA0 | 0xFFFE..=0xFFFF
            | 0xFFF9..=0xFFFB | 0xF0000..=0xFFFFD
            | 0x100000..=0x10FFFD | 0xE0000..=0xE0FFF
    )
}

fn search_hidden_meta(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x2 != 0 {
            return true; // FILE_ATTRIBUTE_HIDDEN
        }
    }
    let _ = meta;
    false
}

fn search_query_ok(query: &str) -> bool {
    !query.is_empty()
        && query.len() <= 80
        && query.trim() == query
        && query.chars().all(|ch| {
            !ch.is_control() && !search_invisible(ch) && (ch == ' ' || !ch.is_whitespace())
        })
}

/// Check every component using the existing safe_path containment guard AND
/// explicit link/reparse metadata. Never use a user-provided absolute path.
fn search_verified_path(home: &Path, relative: &str, root: &Path) -> Result<PathBuf, String> {
    if relative.len() > 256
        || !relative.starts_with(WRITABLE_FOLDER)
        || !relative.split('/').enumerate().all(|(i, part)| {
            (i == 0 && part == WRITABLE_FOLDER) || (i != 0 && search_component_ok(part))
        })
    {
        return Err("Unsafe private user-files search path".into());
    }
    let mut cursor = home.to_path_buf();
    for segment in relative.split('/') {
        cursor.push(segment);
        let meta = fs::symlink_metadata(&cursor).map_err(|_| "Workspace search entry changed")?;
        if is_link_or_reparse(&meta)
            || search_hidden_meta(&meta)
            || (!meta.is_dir() && !meta.is_file())
        {
            return Err("Workspace links or special entries are not searchable".into());
        }
    }
    let checked = safe_path(home, relative)?;
    if !checked.starts_with(root) {
        return Err("Search cannot leave private user-files".into());
    }
    Ok(checked)
}

/// Open with no-follow semantics on the final component; the caller must have
/// validated *all* ancestors. We additionally verify the opened file handle's
/// true OS path to reject an ancestor swapped to a link between validation
/// and open. Never read from a handle whose path is outside user-files/.
fn search_open_file(path: &Path, root: &Path) -> Result<fs::File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x20000); // Linux/Android O_NOFOLLOW
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options
        .open(path)
        .map_err(|_| "Cannot safely open user text file")?;
    let meta = file
        .metadata()
        .map_err(|_| "Cannot inspect opened text file")?;
    if !meta.is_file() || is_link_or_reparse(&meta) || search_hidden_meta(&meta) {
        return Err("Opened workspace entry is not a regular file".into());
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::fd::AsRawFd;
        let fd_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let actual = fd_path
            .canonicalize()
            .map_err(|_| "Cannot verify opened text file path")?;
        if !actual.starts_with(root) {
            return Err("Opened file escaped private user-files".into());
        }
    }
    #[cfg(windows)]
    search_verify_windows_file_handle(&file, root)?;
    Ok(file)
}

#[cfg(windows)]
fn search_verify_windows_file_handle(file: &fs::File, root: &Path) -> Result<(), String> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFinalPathNameByHandleW(
            handle: *mut c_void,
            path: *mut u16,
            length: u32,
            flags: u32,
        ) -> u32;
    }
    // Fixed bounded buffer; no untrusted provider path reaches the response.
    let mut buffer = vec![0u16; 4096];
    let used = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle() as *mut c_void,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            0,
        )
    } as usize;
    if used == 0 || used >= buffer.len() {
        return Err("Cannot verify opened workspace file handle".into());
    }
    let handle_path =
        String::from_utf16(&buffer[..used]).map_err(|_| "Unsafe path on workspace file handle")?;
    fn canonical_windows_string(p: &str) -> String {
        let prefix_removed = p.strip_prefix(r"\\?\").unwrap_or(p);
        prefix_removed.replace('/', "\\").to_lowercase()
    }
    let actual = canonical_windows_string(&handle_path);
    let allowed = canonical_windows_string(&root.to_string_lossy());
    if !actual.starts_with(&(allowed + "\\")) {
        return Err("Opened file escaped private user-files".into());
    }
    Ok(())
}

/// Search only app-created/user-owned UTF-8 text files under the pre-existing
/// app-private user-files/ directory. Unlike name search, this never traverses
/// settings/history/keys or general storage. Returns one bounded hit per file.
/// A safety-budget overrun is an error, never an incomplete silent result.
pub fn search_text(home: &Path, query: &str) -> Result<Value, String> {
    if !search_query_ok(query) {
        return Err("Text search query requires 1..80 visible UTF-8 bytes".into());
    }
    let empty = || {
        json!({"query":query,"results":[],"total":0,"truncated":false,
            "scope":"App-private user-files/","readOnly":true})
    };
    let home_meta = fs::symlink_metadata(home).map_err(|_| "App-private workspace unavailable")?;
    if !home_meta.is_dir() || is_link_or_reparse(&home_meta) {
        return Err("App-private workspace must be a real directory".into());
    }
    let user_root = home.join(WRITABLE_FOLDER);
    match fs::symlink_metadata(&user_root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(empty()),
        Err(_) => return Err("Cannot inspect private user-files folder".into()),
        Ok(meta) if !meta.is_dir() || is_link_or_reparse(&meta) || search_hidden_meta(&meta) => {
            return Err("Private user-files must be a real directory".into());
        }
        Ok(_) => {}
    }
    let root = search_verified_path(
        home,
        WRITABLE_FOLDER,
        &home
            .canonicalize()
            .map_err(|_| "App-private workspace unavailable")?,
    )?;
    let needle = query.to_lowercase();
    let mut stack = vec![(WRITABLE_FOLDER.to_owned(), 0usize)];
    let mut examined = 0usize;
    let mut scanned_bytes = 0usize;
    let mut hits = Vec::<(String, usize, String)>::new();
    let mut skipped_by_limit = false;
    while let Some((relative_dir, depth)) = stack.pop() {
        let folder = search_verified_path(home, &relative_dir, &root)?;
        let meta = fs::symlink_metadata(&folder).map_err(|_| "Search folder changed")?;
        if !meta.is_dir() || is_link_or_reparse(&meta) || search_hidden_meta(&meta) {
            return Err("Search folder is not a regular directory".into());
        }
        let mut children = Vec::new();
        for item in fs::read_dir(&folder).map_err(|_| "Cannot list private user-files")? {
            examined += 1;
            if examined > MAX_SEARCH_FILES {
                return Err("Text search exceeded 256 directory entries".into());
            }
            let item = item.map_err(|_| "Cannot inspect private entry")?;
            let name = match item.file_name().into_string() {
                Ok(name) if search_component_ok(&name) => name,
                _ => continue,
            };
            let relative = format!("{relative_dir}/{name}");
            if relative.len() > 256 {
                skipped_by_limit = true;
                continue;
            }
            let meta = fs::symlink_metadata(item.path())
                .map_err(|_| "Cannot inspect private search entry")?;
            if is_link_or_reparse(&meta)
                || search_hidden_meta(&meta)
                || (!meta.is_dir() && !meta.is_file())
            {
                continue;
            }
            children.push((relative, meta.is_dir(), meta.len()));
        }
        // Sorted before scanning contents AND before pushing nested directories.
        children.sort_by(|a, b| a.0.cmp(&b.0));
        let mut subdirs = Vec::new();
        for (relative, directory, advertised_size) in children {
            if directory {
                if depth < MAX_SEARCH_DEPTH - 1 {
                    subdirs.push((relative, depth + 1));
                } else {
                    skipped_by_limit = true;
                }
                continue;
            }
            if advertised_size > MAX_READ {
                skipped_by_limit = true;
                continue;
            }
            let file_path = search_verified_path(home, &relative, &root)?;
            let mut file = search_open_file(&file_path, &root)?;
            let actual_size = file
                .metadata()
                .map_err(|_| "Cannot inspect opened private file")?
                .len();
            if actual_size > MAX_READ {
                skipped_by_limit = true;
                continue;
            }
            let remaining = MAX_SEARCH_TEXT_BYTES - scanned_bytes;
            if actual_size as usize > remaining {
                return Err("Text search exceeded 1 MiB content scan budget".into());
            }
            let mut bytes = Vec::with_capacity(actual_size as usize);
            Read::by_ref(&mut file)
                // The cap is based on remaining budget, not just stat().len(),
                // because the file may grow between metadata and read().
                .take(MAX_READ.min(remaining as u64))
                .read_to_end(&mut bytes)
                .map_err(|_| "Cannot read bounded private text file")?;
            let after_size = file
                .metadata()
                .map_err(|_| "Opened private file changed")?
                .len();
            if after_size > MAX_READ {
                skipped_by_limit = true;
                continue;
            }
            if after_size as usize > remaining {
                return Err("Text search exceeded 1 MiB content scan budget".into());
            }
            if after_size != bytes.len() as u64 {
                return Err("Private text file changed during search".into());
            }
            scanned_bytes += bytes.len();
            if bytes.iter().any(|byte| {
                *byte == 0
                    || *byte == 0x7f
                    || (*byte < 0x20 && !matches!(*byte, b'\t' | b'\n' | b'\r'))
            }) {
                continue;
            }
            let text = match std::str::from_utf8(&bytes) {
                Ok(text) => text,
                Err(_) => continue,
            };
            if let Some((line, excerpt)) =
                text.lines().enumerate().find_map(|(index, text_line)| {
                    if !text_line.to_lowercase().contains(&needle) {
                        return None;
                    }
                    // Keep excerpt anchored near a match rather than exposing
                    // entire long lines. Strip control/invisible characters.
                    let clean: String = text_line
                        .chars()
                        .filter(|c| !c.is_control() && !search_invisible(*c))
                        .collect();
                    let normalized = clean.to_lowercase();
                    let byte_position = normalized.find(&needle).unwrap_or(0);
                    let char_position = normalized[..byte_position].chars().count();
                    let start = char_position.saturating_sub(40);
                    let excerpt: String = clean
                        .chars()
                        .skip(start)
                        .take(MAX_SEARCH_EXCERPT_CHARS)
                        .collect();
                    Some((index + 1, excerpt))
                })
            {
                hits.push((relative, line, excerpt));
            }
        }
        for subdir in subdirs.into_iter().rev() {
            stack.push(subdir);
        }
    }
    hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let total = hits.len();
    let results: Vec<Value> = hits
        .into_iter()
        .take(MAX_SEARCH_RESULTS)
        .map(|(path, line, excerpt)| json!({"path":path,"line":line,"excerpt":excerpt}))
        .collect();
    Ok(json!({"query":query,"results":results,"total":total,
        "truncated":skipped_by_limit || total > MAX_SEARCH_RESULTS,
        "scope":"App-private user-files/","readOnly":true}))
}

pub fn terminal(home: &Path, command: &str) -> Result<Value, String> {
    if command.len() > 512 || command.contains(['\r', '\n', '\0']) {
        return Err("Invalid terminal input".into());
    }
    let args: Vec<&str> = command.split_whitespace().collect();
    let cmd = args.first().copied().unwrap_or("").to_ascii_lowercase();
    let output = match (cmd.as_str(), args.len()) {
        ("", 0) => String::new(),
        ("help", 1) => "Available: help, pwd, ls [folder], cat <file>, echo <text>, clear (screen only).\nCommands are read-only and restricted to private app data. No Android shell is executed.".to_owned(),
        ("pwd", 1) => "app://workspace/".to_owned(),
        ("ls", 1 | 2) => {
            let folder = if args.len() == 2 { args[1] } else { "" };
            let value = list(home, folder)?;
            let result: Vec<String> = value["entries"].as_array().unwrap().iter().map(|e| {
                format!("{}{}", e["name"].as_str().unwrap_or(""), if e["directory"] == true { "/" } else { "" })
            }).collect();
            if result.is_empty() { "(empty app workspace)".into() } else { result.join("\n") }
        },
        ("cat", 2) => read(home, args[1])?["content"].as_str().unwrap_or("").to_owned(),
        ("echo", _) => args[1..].join(" "),
        _ => "Unknown command. Type help for supported commands.".into()
    };
    Ok(json!({"output": output, "limited": true, "shell": false}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    struct SyntheticWorkspace(PathBuf);
    impl SyntheticWorkspace {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("mcp-text-workspace-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn user_files(&self) -> PathBuf {
            let path = self.0.join(WRITABLE_FOLDER);
            if !path.exists() {
                fs::create_dir(&path).unwrap();
            }
            path
        }
    }
    impl Drop for SyntheticWorkspace {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn writes_are_denied_without_explicit_true_guard_and_no_files_appear() {
        let fixture = SyntheticWorkspace::new();
        fs::write(fixture.0.join("existing.txt"), "original").unwrap();
        assert!(create_text(&fixture.0, "user-files/new.txt", "not allowed", false).is_err());
        assert!(replace_text(&fixture.0, "user-files/existing.txt", "not allowed", false).is_err());
        assert!(!fixture.0.join("user-files").exists());
        assert!(replace_text(&fixture.0, "existing.txt", "not allowed", true).is_err());
        assert_eq!(
            fs::read_to_string(fixture.0.join("existing.txt")).unwrap(),
            "original"
        );
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    fn create_requires_new_destination_and_never_overwrites_existing_files() {
        let fixture = SyntheticWorkspace::new();
        fs::create_dir(fixture.user_files().join("notes")).unwrap();
        let result = create_text(
            &fixture.0,
            "user-files/notes/fresh.txt",
            "New synthetic file",
            true,
        )
        .unwrap();
        assert_eq!(result["written"], true);
        assert_eq!(result["created"], true);
        assert_eq!(result["bytes"], 18);
        assert_eq!(
            fs::read_to_string(fixture.0.join("user-files/notes/fresh.txt")).unwrap(),
            "New synthetic file"
        );
        assert!(create_text(
            &fixture.0,
            "user-files/notes/fresh.txt",
            "overwrite attempted",
            true
        )
        .is_err());
        assert_eq!(
            fs::read_to_string(fixture.0.join("user-files/notes/fresh.txt")).unwrap(),
            "New synthetic file"
        );
        assert!(replace_text(&fixture.0, "user-files/notes/missing.txt", "invalid", true).is_err());
        assert!(!fixture.0.join("user-files/notes/missing.txt").exists());
    }

    #[test]
    fn opt_in_initializes_only_user_files_and_never_touches_app_control_files() {
        let fixture = SyntheticWorkspace::new();
        for protected in [
            "sessions.json",
            "projects-state.json",
            "mcp-state.json",
            "skills-v1.json",
            "goal-v1.json",
            "keys.json",
        ] {
            fs::write(fixture.0.join(protected), "Private control data").unwrap();
            assert!(replace_text(&fixture.0, protected, "Not permitted", true).is_err());
            assert!(create_text(&fixture.0, protected, "Not permitted", true).is_err());
            assert_eq!(
                fs::read_to_string(fixture.0.join(protected)).unwrap(),
                "Private control data"
            );
        }
        assert!(!fixture.0.join(WRITABLE_FOLDER).exists());
        let created = create_text(&fixture.0, "user-files/example.txt", "safe", true).unwrap();
        assert_eq!(created["path"], "user-files/example.txt");
        assert!(fixture.0.join("user-files/example.txt").is_file());
        assert_eq!(
            list(&fixture.0, "").unwrap()["entries"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["name"] == "user-files")
                .count(),
            1
        );
    }

    #[test]
    fn concurrent_create_collision_staged_file_cannot_replace_winner() {
        let fixture = SyntheticWorkspace::new();
        let failed = write_text_impl(
            &fixture.0,
            "user-files/collision.txt",
            "loser",
            true,
            WriteMode::Create,
            |staged, target| {
                fs::write(target, "winning creator")?;
                fs::hard_link(staged, target)
            },
        );
        assert!(failed.is_err());
        assert_eq!(
            fs::read_to_string(fixture.0.join("user-files/collision.txt")).unwrap(),
            "winning creator"
        );
        assert_eq!(
            fs::read_dir(fixture.0.join("user-files")).unwrap().count(),
            1
        );
    }

    #[test]
    fn replacement_is_atomic_and_failed_staging_preserves_original() {
        let fixture = SyntheticWorkspace::new();
        fs::write(fixture.user_files().join("edit.txt"), "Before").unwrap();
        let failed = write_text_impl(
            &fixture.0,
            "user-files/edit.txt",
            "Would fail",
            true,
            WriteMode::Replace,
            |_staging, _target| Err(io::Error::other("synthetic commit failure")),
        );
        assert!(failed.is_err());
        assert_eq!(
            fs::read_to_string(fixture.0.join("user-files/edit.txt")).unwrap(),
            "Before"
        );
        assert_eq!(
            fs::read_dir(fixture.0.join("user-files")).unwrap().count(),
            1
        );
        let result =
            replace_text(&fixture.0, "user-files/edit.txt", "After: UTF-8 😊", true).unwrap();
        assert_eq!(result["created"], false);
        assert_eq!(
            fs::read_to_string(fixture.0.join("user-files/edit.txt")).unwrap(),
            "After: UTF-8 😊"
        );
        assert_eq!(
            fs::read_dir(fixture.0.join("user-files")).unwrap().count(),
            1
        );
    }

    #[test]
    fn refused_paths_include_traversal_absolute_hidden_windows_aliases_and_missing_parents() {
        let fixture = SyntheticWorkspace::new();
        fs::create_dir(fixture.user_files().join(".private")).unwrap();
        fs::create_dir(fixture.user_files().join("public")).unwrap();
        for unsafe_name in [
            "",
            "../outside.txt",
            "public/../outside.txt",
            "./hidden.txt",
            ".private/a.txt",
            "public/.hidden.txt",
            ".secret",
            "/tmp/outside.txt",
            "C:/Windows/file.txt",
            "public\\file.txt",
            "user-files/public//bad.txt",
            "user-files/CON.txt",
            "user-files/public/file.txt:ads",
            "user-files/public/trailing. ",
            "user-files/public/file.txt/",
            "user-files/missing/file.txt",
            "sessions.json",
            "mcp.json",
            "user-files/../sessions.json",
            "user-files/.private/a.txt",
            "user-files/public/.hidden.txt",
        ] {
            assert!(
                create_text(&fixture.0, unsafe_name, "synthetic", true).is_err(),
                "unsafe create path was allowed: {unsafe_name}"
            );
            assert!(
                replace_text(&fixture.0, unsafe_name, "synthetic", true).is_err(),
                "unsafe replacement path was allowed: {unsafe_name}"
            );
        }
        assert_eq!(
            fs::read_dir(fixture.0.join("user-files/public"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn refuse_symlink_targets_and_parent_symlinks_when_supported_by_os() {
        let fixture = SyntheticWorkspace::new();
        fs::create_dir(fixture.user_files().join("actual")).unwrap();
        fs::write(
            fixture.0.join("user-files/actual/existing.txt"),
            "Must remain safe",
        )
        .unwrap();
        #[cfg(unix)]
        let make_file_link = std::os::unix::fs::symlink;
        #[cfg(windows)]
        let make_file_link = std::os::windows::fs::symlink_file;
        #[cfg(unix)]
        let make_dir_link = std::os::unix::fs::symlink;
        #[cfg(windows)]
        let make_dir_link = std::os::windows::fs::symlink_dir;

        let file_link = fixture.0.join("user-files/linked.txt");
        let directory_link = fixture.0.join("user-files/linked_directory");
        let file_result =
            make_file_link(fixture.0.join("user-files/actual/existing.txt"), &file_link);
        let dir_result = make_dir_link(fixture.0.join("user-files/actual"), &directory_link);
        // Windows without symlink privilege cannot create links in a synthetic
        // folder; only skip that specific OS-permission-limited assertion.
        for error in [file_result.as_ref().err(), dir_result.as_ref().err()]
            .into_iter()
            .flatten()
        {
            #[cfg(windows)]
            assert_eq!(
                error.raw_os_error(),
                Some(1314),
                "Unexpected symlink error: {error}"
            );
            #[cfg(not(windows))]
            panic!("Unexpected symlink creation error: {error}");
        }
        if file_result.is_ok() {
            assert!(create_text(&fixture.0, "user-files/linked.txt", "bad", true).is_err());
            assert!(replace_text(&fixture.0, "user-files/linked.txt", "bad", true).is_err());
        }
        if dir_result.is_ok() {
            assert!(create_text(
                &fixture.0,
                "user-files/linked_directory/new.txt",
                "bad",
                true
            )
            .is_err());
            assert!(replace_text(
                &fixture.0,
                "user-files/linked_directory/existing.txt",
                "bad",
                true
            )
            .is_err());
        }
        assert_eq!(
            fs::read_to_string(fixture.0.join("user-files/actual/existing.txt")).unwrap(),
            "Must remain safe"
        );
    }

    #[test]
    fn enforce_exact_64_kib_utf8_boundary_and_refuse_binary_or_oversize_old_files() {
        let fixture = SyntheticWorkspace::new();
        let max = "界".repeat(MAX_WRITE_BYTES / 3);
        let fit = format!("{}x", max);
        assert_eq!(fit.len(), MAX_WRITE_BYTES);
        assert!(create_text(&fixture.0, "user-files/max.txt", &fit, true).is_ok());
        assert_eq!(
            fs::metadata(fixture.0.join("user-files/max.txt"))
                .unwrap()
                .len(),
            MAX_WRITE_BYTES as u64
        );
        assert!(replace_text(
            &fixture.0,
            "user-files/max.txt",
            "x".repeat(MAX_WRITE_BYTES + 1).as_str(),
            true
        )
        .is_err());
        assert!(create_text(
            &fixture.0,
            "user-files/oversize.txt",
            &"x".repeat(MAX_WRITE_BYTES + 1),
            true
        )
        .is_err());
        assert!(!fixture.0.join("user-files/oversize.txt").exists());
        assert!(create_text(&fixture.0, "user-files/binary.txt", "has\0nul", true).is_err());
        fs::write(fixture.user_files().join("invalid.txt"), [0xff, 0xfe, 0xfd]).unwrap();
        assert!(replace_text(
            &fixture.0,
            "user-files/invalid.txt",
            "cannot overwrite binary",
            true
        )
        .is_err());
        fs::write(
            fixture.0.join("user-files/huge.txt"),
            vec![b'x'; MAX_WRITE_BYTES + 1],
        )
        .unwrap();
        assert!(replace_text(
            &fixture.0,
            "user-files/huge.txt",
            "cannot overwrite large file",
            true
        )
        .is_err());
        assert_eq!(
            fs::read(fixture.0.join("user-files/invalid.txt")).unwrap(),
            [0xff, 0xfe, 0xfd]
        );
        assert_eq!(
            fs::metadata(fixture.0.join("user-files/huge.txt"))
                .unwrap()
                .len(),
            MAX_WRITE_BYTES as u64 + 1
        );
    }
    #[test]
    fn read_only_workspace_enforces_size_and_traversal() {
        let home = std::env::temp_dir().join(format!("mcp-app-workspace-{}", Uuid::new_v4()));
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("test.txt"), "hello").unwrap();
        fs::write(home.join("large.txt"), vec![b'x'; 16385]).unwrap();
        assert_eq!(read(&home, "test.txt").unwrap()["content"], "hello");
        assert!(read(&home, "../test.txt").is_err());
        assert!(read(&home, "/etc/passwd").is_err());
        assert!(read(&home, "large.txt").is_err());
        assert_eq!(terminal(&home, "cat test.txt").unwrap()["output"], "hello");
        assert_eq!(
            terminal(&home, "pwd").unwrap()["output"],
            "app://workspace/"
        );
        assert_eq!(
            terminal(&home, "Pwd").unwrap()["output"],
            "app://workspace/"
        );
        assert_eq!(properties(&home, "test.txt").unwrap()["bytes"], 5);
        assert_eq!(properties(&home, "test.txt").unwrap()["readOnly"], true);
        assert!(properties(&home, "../test.txt").is_err());
        assert_eq!(search(&home, "TEST").unwrap()["total"], 1);
        assert!(search(&home, "../").unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(search(&home, "").is_err());
        assert!(terminal(&home, "echo hi\nrm x").is_err());
        let entries = list(&home, "").unwrap();
        assert_eq!(entries["readOnly"], true);
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn private_filename_search_is_bounded_and_skips_hidden_and_symlink_entries() {
        let home = std::env::temp_dir().join(format!("mcp-workspace-search-{}", Uuid::new_v4()));
        fs::create_dir_all(home.join("folder")).unwrap();
        fs::create_dir_all(home.join(".hidden")).unwrap();
        fs::write(home.join("folder").join("example.txt"), "synthetic").unwrap();
        fs::write(home.join(".hidden").join("secret-example.txt"), "synthetic").unwrap();
        let result = search(&home, "EXAMPLE").unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["results"][0]["path"], "folder/example.txt");
        assert!(search(&home, &"x".repeat(81)).is_err());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn content_search_is_strictly_scoped_and_missing_root_does_not_create_it() {
        let fixture = SyntheticWorkspace::new();
        fs::write(fixture.0.join("sessions.json"), "needle private history").unwrap();
        fs::write(fixture.0.join("keys.json"), "needle private secret").unwrap();
        assert_eq!(
            search_text(&fixture.0, "needle").unwrap(),
            json!({"query":"needle","results":[],"total":0,"truncated":false,
                "scope":"App-private user-files/","readOnly":true})
        );
        assert!(!fixture.0.join(WRITABLE_FOLDER).exists());
        fs::write(
            fixture.user_files().join("note.txt"),
            "normal\nneedle user text",
        )
        .unwrap();
        let found = search_text(&fixture.0, "needle").unwrap();
        assert_eq!(found["total"], 1);
        assert_eq!(found["results"][0]["path"], "user-files/note.txt");
        assert_eq!(found["results"][0]["line"], 2);
        assert_eq!(found["results"][0]["excerpt"], "needle user text");
        assert_eq!(found["scope"], "App-private user-files/");
        assert_eq!(found["readOnly"], true);
        assert!(!found.to_string().contains("private history"));
        assert!(!found.to_string().contains("private secret"));
    }

    #[test]
    fn content_search_unicode_and_strict_query_validation() {
        let fixture = SyntheticWorkspace::new();
        fs::write(
            fixture.user_files().join("Résumé.txt"),
            "First line\nDéjà VU — 你好 café 😊\nthird",
        )
        .unwrap();
        let result = search_text(&fixture.0, "DÉJÀ").unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["results"][0]["line"], 2);
        assert!(result["results"][0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("你好 café"));
        assert_eq!(search_text(&fixture.0, "😊").unwrap()["total"], 1);
        for invalid in [
            "",
            " ",
            " needle",
            "needle ",
            "\n",
            "a\tb",
            "a\u{200b}b",
            "a\u{202e}b",
            "a\u{034f}b",
            "a\u{fe0f}b",
            "a\u{00a0}b",
            "\u{7f}",
        ] {
            assert!(search_text(&fixture.0, invalid).is_err(), "{invalid:?}");
        }
        assert!(search_text(&fixture.0, &"界".repeat(26)).is_ok()); // 78 UTF-8 bytes
        assert!(search_text(&fixture.0, &"界".repeat(27)).is_err()); // 81 UTF-8 bytes
    }

    #[test]
    fn content_search_exact_16_kib_per_file_binary_filter_and_short_excerpt() {
        let fixture = SyntheticWorkspace::new();
        let root = fixture.user_files();
        let exact = format!("{}needle\n", "x".repeat(MAX_READ as usize - 7));
        assert_eq!(exact.len(), MAX_READ as usize);
        fs::write(root.join("exact.txt"), exact).unwrap();
        fs::write(
            root.join("oversize.txt"),
            format!("needle{}", "z".repeat(MAX_READ as usize)),
        )
        .unwrap();
        fs::write(root.join("nul.txt"), b"needle\0binary").unwrap();
        fs::write(root.join("not-utf8.txt"), b"needle\xff").unwrap();
        fs::write(root.join("control.txt"), b"needle\x01hidden").unwrap();
        fs::write(root.join(".hidden.txt"), b"needle hidden").unwrap();
        fs::create_dir(root.join(".hidden-folder")).unwrap();
        fs::write(root.join(".hidden-folder/invisible.txt"), b"needle").unwrap();
        let result = search_text(&fixture.0, "needle").unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["results"][0]["path"], "user-files/exact.txt");
        assert_eq!(result["results"][0]["line"], 1);
        assert!(result["results"][0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("needle"));
        assert!(
            result["results"][0]["excerpt"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 160
        );
        assert_eq!(result["truncated"], true); // skipped oversized text
    }

    #[test]
    fn content_search_rejects_symlink_escape_and_private_root_links() {
        let fixture = SyntheticWorkspace::new();
        let root = fixture.user_files();
        fs::write(fixture.0.join("secret.txt"), "needle external secret").unwrap();
        fs::write(root.join("valid.txt"), "needle user owned").unwrap();
        #[cfg(unix)]
        let symlink_file = std::os::unix::fs::symlink;
        #[cfg(windows)]
        let symlink_file = std::os::windows::fs::symlink_file;
        #[cfg(unix)]
        let symlink_dir = std::os::unix::fs::symlink;
        #[cfg(windows)]
        let symlink_dir = std::os::windows::fs::symlink_dir;
        let file = symlink_file(fixture.0.join("secret.txt"), root.join("external.txt"));
        let dir = symlink_dir(&fixture.0, root.join("outside"));
        for result in [&file, &dir] {
            if let Err(error) = result {
                #[cfg(windows)]
                assert_eq!(error.raw_os_error(), Some(1314));
                #[cfg(not(windows))]
                panic!("Unexpected synthetic symlink failure: {error}");
            }
        }
        let result = search_text(&fixture.0, "needle").unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["results"][0]["path"], "user-files/valid.txt");
        assert!(!result.to_string().contains("external secret"));
        if file.is_ok() {
            assert!(search_verified_path(&fixture.0, "user-files/external.txt", &root).is_err());
        }
        let root_link_fixture = SyntheticWorkspace::new();
        let linked = symlink_dir(&root, root_link_fixture.0.join("user-files"));
        if linked.is_ok() {
            assert!(search_text(&root_link_fixture.0, "needle").is_err());
        }
    }

    #[test]
    fn content_search_fails_closed_at_entry_and_global_byte_budgets() {
        let fixture = SyntheticWorkspace::new();
        let root = fixture.user_files();
        // 65 * 16 KiB exceeds the global 1 MiB byte budget, even if no hit.
        for index in 0..65 {
            fs::write(
                root.join(format!("blob-{index:03}.txt")),
                vec![b'x'; MAX_READ as usize],
            )
            .unwrap();
        }
        let err = search_text(&fixture.0, "needle").unwrap_err();
        assert!(err.contains("1 MiB"), "{err}");
        let another = SyntheticWorkspace::new();
        let root = another.user_files();
        for index in 0..257 {
            fs::write(root.join(format!("entry-{index:03}.txt")), b"a").unwrap();
        }
        let err = search_text(&another.0, "needle").unwrap_err();
        assert!(err.contains("256"), "{err}");
    }

    #[test]
    fn content_search_deterministic_order_results_cap_and_depth() {
        let fixture = SyntheticWorkspace::new();
        let root = fixture.user_files();
        for index in (0..44).rev() {
            fs::write(
                root.join(format!("sample-{index:03}.txt")),
                "needle\nneedle",
            )
            .unwrap();
        }
        let first = search_text(&fixture.0, "needle").unwrap();
        let repeat = search_text(&fixture.0, "needle").unwrap();
        assert_eq!(first, repeat);
        assert_eq!(first["total"], 44);
        assert_eq!(
            first["results"].as_array().unwrap().len(),
            MAX_SEARCH_RESULTS
        );
        assert_eq!(first["results"][0]["path"], "user-files/sample-000.txt");
        assert_eq!(first["results"][39]["path"], "user-files/sample-039.txt");
        assert_eq!(first["results"][0]["line"], 1);
        assert_eq!(first["truncated"], true);
        let mut directory = root;
        for index in 1..=MAX_SEARCH_DEPTH {
            directory = directory.join(format!("level-{index}"));
            fs::create_dir(&directory).unwrap();
        }
        fs::write(directory.join("too-deep.txt"), "needle").unwrap();
        let bounded = search_text(&fixture.0, "needle").unwrap();
        assert_eq!(bounded["total"], 44);
        assert_eq!(bounded["truncated"], true);
    }
}
