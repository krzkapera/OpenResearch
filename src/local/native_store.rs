//! Provider-native state used by chats launched through OpenResearch.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{anyhow, Result};

#[path = "opencode_db.rs"]
pub mod opencode_database;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeStore {
    Isolated,
    Legacy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSessionLocation {
    pub store: NativeStore,
    pub path: PathBuf,
}

static LINK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn user_env(key: &str) -> Option<OsString> {
    crate::local::shell_env::var(key)
        .or_else(|| crate::config::synced_env_var(key).map(|value| value.trim().into()))
}

fn user_env_path(key: &str) -> Option<PathBuf> {
    user_env(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn opencode_db(store: NativeStore) -> PathBuf {
    match store {
        NativeStore::Isolated => crate::store::data_dir().join("agents/opencode/opencode.db"),
        NativeStore::Legacy => {
            let data = user_env_path("XDG_DATA_HOME")
                .unwrap_or_else(|| home_dir().join(".local/share"))
                .join("opencode");
            data.join(user_env_path("OPENCODE_DB").unwrap_or_else(|| PathBuf::from("opencode.db")))
        }
    }
}

pub fn claude_home(store: NativeStore) -> PathBuf {
    match store {
        NativeStore::Isolated => crate::store::data_dir().join("agents/claude"),
        NativeStore::Legacy => {
            user_env_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home_dir().join(".claude"))
        }
    }
}

pub fn claude_secure_storage_config_dir() -> OsString {
    // Empty intentionally selects Claude's unsuffixed default Keychain namespace.
    user_env("CLAUDE_SECURESTORAGE_CONFIG_DIR")
        .or_else(|| user_env("CLAUDE_CONFIG_DIR"))
        .unwrap_or_default()
}

/// Which Codex account a Codex-family harness runs on. `Primary` is the stock
/// `codex` harness (`$CODEX_HOME`, default `~/.codex`); `Secondary` is the
/// fork's `codex2` harness: the same CLI against a second, fully separate
/// home (`$ORX_CODEX2_HOME`, default `~/.codex2`) with its own login, sessions
/// and state. Nothing of one home is ever read or linked into the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodexProfile {
    Primary,
    Secondary,
}

impl CodexProfile {
    /// The profile behind a harness id; `None` for a non-Codex harness.
    pub fn from_harness(harness_id: &str) -> Option<Self> {
        match harness_id {
            "codex" => Some(Self::Primary),
            "codex2" => Some(Self::Secondary),
            _ => None,
        }
    }

    /// The profile of a session's harness id, `Primary` for anything else
    /// (the stock behaviour every pre-`codex2` caller had).
    pub fn of_harness(harness_id: &str) -> Self {
        Self::from_harness(harness_id).unwrap_or(Self::Primary)
    }

    pub fn harness_id(self) -> &'static str {
        match self {
            Self::Primary => "codex",
            Self::Secondary => "codex2",
        }
    }

    /// Both profiles, primary first.
    pub const ALL: [Self; 2] = [Self::Primary, Self::Secondary];
}

/// Env var that relocates the `codex2` home (the counterpart of `CODEX_HOME`).
pub const CODEX2_HOME_ENV: &str = "ORX_CODEX2_HOME";

pub fn codex_home(store: NativeStore) -> PathBuf {
    codex_home_for(CodexProfile::Primary, store)
}

pub fn codex_home_for(profile: CodexProfile, store: NativeStore) -> PathBuf {
    codex_home_with(
        profile,
        store,
        crate::store::data_dir,
        home_dir,
        user_env_path,
    )
}

/// [`codex_home_for`] over explicit roots, so the mapping is testable without
/// touching the process environment.
fn codex_home_with(
    profile: CodexProfile,
    store: NativeStore,
    data_dir: impl FnOnce() -> PathBuf,
    home: impl FnOnce() -> PathBuf,
    env: impl FnOnce(&str) -> Option<PathBuf>,
) -> PathBuf {
    match (profile, store) {
        (CodexProfile::Primary, NativeStore::Isolated) => data_dir().join("agents/codex"),
        (CodexProfile::Primary, NativeStore::Legacy) => {
            env("CODEX_HOME").unwrap_or_else(|| home().join(".codex"))
        }
        (CodexProfile::Secondary, NativeStore::Isolated) => data_dir().join("agents/codex2"),
        (CodexProfile::Secondary, NativeStore::Legacy) => {
            env(CODEX2_HOME_ENV).unwrap_or_else(|| home().join(".codex2"))
        }
    }
}

pub fn cursor_home(store: NativeStore) -> PathBuf {
    match store {
        NativeStore::Isolated => crate::store::data_dir().join("agents/cursor"),
        NativeStore::Legacy => user_env_path("CURSOR_CONFIG_DIR")
            .or_else(|| user_env_path("XDG_CONFIG_HOME").map(|root| root.join("cursor")))
            .unwrap_or_else(|| home_dir().join(".cursor")),
    }
}

pub fn antigravity_home(store: NativeStore) -> PathBuf {
    match store {
        NativeStore::Isolated => crate::store::data_dir().join("agents/antigravity"),
        NativeStore::Legacy => user_env_path("ANTIGRAVITY_HOME")
            .or_else(|| user_env_path("GEMINI_CONFIG_DIR"))
            .unwrap_or_else(|| home_dir().join(".gemini").join("antigravity-cli")),
    }
}

pub fn opencode_session(native_id: &str) -> Result<Option<NativeSessionLocation>> {
    let isolated = opencode_db(NativeStore::Isolated);
    let legacy = opencode_db(NativeStore::Legacy);
    for (store, db) in [
        (NativeStore::Isolated, isolated.clone()),
        (NativeStore::Legacy, legacy),
    ] {
        if store == NativeStore::Legacy && db == isolated {
            continue;
        }
        let found = opencode_has_session(&db, native_id)?;
        if found {
            return Ok(Some(NativeSessionLocation { store, path: db }));
        }
    }
    Ok(None)
}

pub(crate) struct OpenCodeRelocation {
    database: PathBuf,
    sessions: Vec<(String, String)>,
    v2_sessions: Vec<(String, String)>,
    projects: Vec<(String, String)>,
}

impl OpenCodeRelocation {
    pub(crate) fn apply(self) -> Result<()> {
        let mut connection = rusqlite::Connection::open_with_flags(
            &self.database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let transaction = connection.transaction()?;
        for (table, column, paths) in [
            ("session", "directory", self.sessions),
            ("session_v2", "directory", self.v2_sessions),
            ("project", "worktree", self.projects),
        ] {
            for (old, new) in paths {
                transaction.execute(
                    &format!("UPDATE {table} SET {column} = ?2 WHERE {column} = ?1"),
                    rusqlite::params![old, new],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }
}

pub(crate) fn opencode_relocation(
    database: &Path,
    relocate: impl Fn(&Path) -> PathBuf,
) -> Result<Option<OpenCodeRelocation>> {
    if !database.is_file() {
        return Ok(None);
    }
    let connection = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut changes = OpenCodeRelocation {
        database: database.to_path_buf(),
        sessions: Vec::new(),
        v2_sessions: Vec::new(),
        projects: Vec::new(),
    };
    for (table, column, paths) in [
        ("session", "directory", &mut changes.sessions),
        ("session_v2", "directory", &mut changes.v2_sessions),
        ("project", "worktree", &mut changes.projects),
    ] {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
            rusqlite::params![table, column],
            |row| row.get(0),
        )?;
        if !exists {
            continue;
        }
        let mut query = connection.prepare(&format!("SELECT DISTINCT {column} FROM {table}"))?;
        for path in query.query_map([], |row| row.get::<_, String>(0))? {
            let old = path?;
            let new = relocate(Path::new(&old));
            if new != Path::new(&old) {
                paths.push((old, new.to_string_lossy().into_owned()));
            }
        }
    }
    Ok((!changes.sessions.is_empty()
        || !changes.v2_sessions.is_empty()
        || !changes.projects.is_empty())
    .then_some(changes))
}

pub(crate) fn opencode_has_session(db: &Path, native_id: &str) -> Result<bool> {
    opencode_database::has_session(db, native_id)
}

pub fn codex_sqlite_override(store: NativeStore, home: &Path) -> Option<String> {
    (store == NativeStore::Isolated).then(|| format!("sqlite_home={}", toml_string(home)))
}

pub fn claude_session(native_id: &str) -> Result<Option<NativeSessionLocation>> {
    session_location(claude_home, &["projects"], native_id)
}

pub fn codex_session(native_id: &str) -> Result<Option<NativeSessionLocation>> {
    codex_session_for(CodexProfile::Primary, native_id)
}

/// A rollout in one profile's stores only — what resume, recovery and
/// compaction use, so a `codex2` thread is never resumed from `~/.codex`.
pub fn codex_session_for(
    profile: CodexProfile,
    native_id: &str,
) -> Result<Option<NativeSessionLocation>> {
    codex_session_in(&codex_home_for, profile, native_id)
}

/// A rollout in whichever profile holds it (thread ids are UUIDs, so at most
/// one does). Read-only lookups by thread id only — never for resuming.
pub fn codex_session_any(native_id: &str) -> Result<Option<NativeSessionLocation>> {
    codex_session_any_in(&codex_home_for, native_id)
}

fn codex_session_in(
    homes: &dyn Fn(CodexProfile, NativeStore) -> PathBuf,
    profile: CodexProfile,
    native_id: &str,
) -> Result<Option<NativeSessionLocation>> {
    session_location(
        |store| homes(profile, store),
        &["sessions", "archived_sessions"],
        native_id,
    )
}

fn codex_session_any_in(
    homes: &dyn Fn(CodexProfile, NativeStore) -> PathBuf,
    native_id: &str,
) -> Result<Option<NativeSessionLocation>> {
    for profile in CodexProfile::ALL {
        if let Some(found) = codex_session_in(homes, profile, native_id)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

pub fn cursor_session(native_id: &str) -> Result<Option<NativeSessionLocation>> {
    let isolated = cursor_home(NativeStore::Isolated);
    for (store, root) in [
        (NativeStore::Isolated, isolated.clone()),
        (NativeStore::Legacy, cursor_home(NativeStore::Legacy)),
    ] {
        if store == NativeStore::Legacy && root == isolated {
            continue;
        }
        let path = match cursor_session_path(&root, native_id) {
            Ok(path) => path,
            Err(_) if store == NativeStore::Legacy => continue,
            Err(error) => return Err(error),
        };
        if let Some(path) = path {
            return Ok(Some(NativeSessionLocation { store, path }));
        }
    }
    Ok(None)
}

/// Cursor CLI print-mode chats live at `chats/<workspace-md5>/<uuid>/store.db`
/// (and ACP sessions at `acp-sessions/<uuid>/`). Walk those trees only — never
/// `projects/`, which is the IDE's transcript dump and can be huge.
fn cursor_session_path(root: &Path, native_id: &str) -> Result<Option<PathBuf>> {
    for tree in ["chats", "acp-sessions"] {
        if let Some(path) = named_dir_session(&root.join(tree), native_id, 3)? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn named_dir_session(root: &Path, native_id: &str, depth: usize) -> Result<Option<PathBuf>> {
    if depth == 0 {
        return Ok(None);
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if path.file_name().and_then(|name| name.to_str()) == Some(native_id) {
            return Ok(Some(path));
        }
        if let Some(path) = named_dir_session(&path, native_id, depth - 1)? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn session_location(
    home: impl Fn(NativeStore) -> PathBuf,
    trees: &[&str],
    native_id: &str,
) -> Result<Option<NativeSessionLocation>> {
    let isolated = home(NativeStore::Isolated);
    for (store, root) in [
        (NativeStore::Isolated, isolated.clone()),
        (NativeStore::Legacy, home(NativeStore::Legacy)),
    ] {
        if store == NativeStore::Legacy && root == isolated {
            continue;
        }
        for tree in trees {
            let path = match tree_session_path(&root.join(tree), native_id) {
                Ok(path) => path,
                Err(_) if store == NativeStore::Legacy => continue,
                Err(error) => return Err(error),
            };
            if let Some(path) = path {
                return Ok(Some(NativeSessionLocation { store, path }));
            }
        }
    }
    Ok(None)
}

fn tree_session_path(root: &Path, native_id: &str) -> Result<Option<PathBuf>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            if let Some(path) = tree_session_path(&path, native_id)? {
                return Ok(Some(path));
            }
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".jsonl"))
            .is_some_and(|name| {
                name == native_id
                    || name
                        .strip_suffix(native_id)
                        .is_some_and(|prefix| prefix.ends_with('-'))
            })
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Quote a path for a TOML `-c` override; JSON escaping is compatible except for DEL.
pub fn toml_string(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy())
        .unwrap_or_else(|_| "\"\"".to_string())
        .replace('\u{7f}', "\\u007F")
}

pub fn prepare_opencode(store: NativeStore) -> Result<PathBuf> {
    let db = opencode_db(store);
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(db)
}

pub fn prepare_claude(store: NativeStore) -> Result<PathBuf> {
    let root = claude_home(store);
    let legacy = claude_home(NativeStore::Legacy);
    if store == NativeStore::Legacy || root == legacy {
        std::fs::create_dir_all(&root)?;
        return Ok(root);
    }
    prepare_links(
        &root,
        &legacy,
        &[
            home_dir().join(".claude.json"),
            legacy.join("settings.json"),
            legacy.join("settings.local.json"),
            legacy.join("CLAUDE.md"),
            legacy.join(".credentials.json"),
            legacy.join("plugins"),
            legacy.join("skills"),
        ],
    )?;
    Ok(root)
}

pub fn prepare_codex(store: NativeStore) -> Result<PathBuf> {
    prepare_codex_for(CodexProfile::Primary, store)
}

/// The profile's `CODEX_HOME` for `store`: the legacy home itself, or an
/// isolated home whose shared files are links into that profile's legacy home.
pub fn prepare_codex_for(profile: CodexProfile, store: NativeStore) -> Result<PathBuf> {
    let root = codex_home_for(profile, store);
    let legacy = codex_home_for(profile, NativeStore::Legacy);
    if store == NativeStore::Legacy || root == legacy {
        std::fs::create_dir_all(&root)?;
        return Ok(root);
    }
    let mut sources = vec![
        legacy.join("auth.json"),
        legacy.join("config.toml"),
        legacy.join("AGENTS.md"),
        legacy.join("plugins"),
        legacy.join("skills"),
        legacy.join("prompts"),
        legacy.join("packages"),
    ];
    match std::fs::read_dir(&legacy) {
        Ok(entries) => sources.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".config.toml"))
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    prepare_links(&root, &legacy, &sources)?;
    Ok(crate::paths::canonicalize(&root).unwrap_or(root))
}

pub fn prepare_cursor(store: NativeStore) -> Result<PathBuf> {
    let root = cursor_home(store);
    let legacy = cursor_home(NativeStore::Legacy);
    if store == NativeStore::Legacy || root == legacy {
        std::fs::create_dir_all(&root)?;
        return Ok(root);
    }
    prepare_links(
        &root,
        &legacy,
        &[
            legacy.join("cli-config.json"),
            legacy.join("skills"),
            legacy.join("skills-cursor"),
            legacy.join("plugins"),
        ],
    )?;
    Ok(root)
}

fn prepare_links(root: &Path, lock_root: &Path, sources: &[PathBuf]) -> Result<()> {
    let _guard = LINK_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    std::fs::create_dir_all(root)?;
    let mut process_lock = sources
        .iter()
        .any(|source| source.exists())
        .then(|| {
            std::fs::create_dir_all(lock_root)?;
            Ok::<_, std::io::Error>(fd_lock::RwLock::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(lock_root.join(".openresearch-native-links.lock"))?,
            ))
        })
        .transpose()?;
    let _process_guard = process_lock.as_mut().map(|lock| lock.write()).transpose()?;
    for source in sources {
        let Some(name) = source.file_name() else {
            continue;
        };
        reconcile_link(source, &root.join(name))?;
    }
    Ok(())
}

fn reconcile_link(source: &Path, destination: &Path) -> Result<()> {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let marker = destination.with_file_name(format!("{name}.orx-managed-link"));
    let source_metadata = match std::fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(destination)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
                && std::fs::read_link(destination).is_ok_and(|target| target == source)
            {
                remove_link(destination)?;
            }
            if marker.is_file() {
                std::fs::remove_file(marker)?;
            }
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    match std::fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if std::fs::read_link(destination).is_ok_and(|target| target == source) {
                write_marker(&marker, source)?;
                return Ok(());
            }
            remove_link(destination)?;
        }
        Ok(metadata) if metadata.is_file() && source_metadata.is_file() && marker.is_file() => {
            // Windows' hard-link fallback is the source itself: nothing to adopt.
            if same_file::is_same_file(destination, source).unwrap_or(false) {
                write_marker(&marker, source)?;
                return Ok(());
            }
            if std::fs::read_to_string(&marker).ok().as_deref() == file_hash(source)?.as_deref() {
                adopt_managed_file(destination, source)?;
            } else {
                preserve_conflict(destination)?;
            }
        }
        Ok(_) => {
            preserve_conflict(destination)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    create_symlink(source, destination).map_err(|error| {
        anyhow!(
            "could not link {} to {}: {error}",
            destination.display(),
            source.display()
        )
    })?;
    write_marker(&marker, source)?;
    Ok(())
}

fn adopt_managed_file(from: &Path, to: &Path) -> Result<()> {
    let name = to
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let id = uuid::Uuid::new_v4();
    let backup = to.with_file_name(format!(".{name}.orx-backup"));
    let staged = to.with_file_name(format!(".{name}.orx-staged-{id}"));
    std::fs::copy(from, &staged)?;
    if backup.exists() {
        std::fs::remove_file(&backup)?;
    }
    std::fs::rename(to, &backup)?;
    if let Err(error) = std::fs::rename(&staged, to) {
        std::fs::rename(&backup, to)?;
        return Err(error.into());
    }
    std::fs::remove_file(from)?;
    Ok(())
}

fn preserve_conflict(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    std::fs::rename(
        path,
        path.with_file_name(format!("{name}.orx-conflict-{}", uuid::Uuid::new_v4())),
    )?;
    Ok(())
}

fn file_hash(path: &Path) -> Result<Option<String>> {
    if path.is_file() {
        Ok(Some(format!("{:x}", Sha256::digest(std::fs::read(path)?))))
    } else {
        Ok(None)
    }
}

fn write_marker(marker: &Path, source: &Path) -> Result<()> {
    std::fs::write(marker, file_hash(source)?.unwrap_or_default())?;
    Ok(())
}

#[cfg(not(windows))]
fn remove_link(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

#[cfg(windows)]
fn remove_link(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::FileTypeExt;

    if std::fs::symlink_metadata(path)?
        .file_type()
        .is_symlink_dir()
    {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(unix)]
pub(crate) fn create_symlink(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, destination)
}

pub(crate) fn copy_symlink(source: &Path, destination: &Path) -> std::io::Result<()> {
    let target = std::fs::read_link(source)?;
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, destination);
    #[cfg(windows)]
    if source.metadata()?.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    }
}

/// Without Developer Mode, Windows refuses symlinks; a junction reads back like one,
/// a hard link does not (see `reconcile_link`).
#[cfg(windows)]
pub(crate) fn create_symlink(source: &Path, destination: &Path) -> std::io::Result<()> {
    let directory = source.is_dir();
    let attempt = if directory {
        std::os::windows::fs::symlink_dir(source, destination)
    } else {
        std::os::windows::fs::symlink_file(source, destination)
    };
    match attempt {
        Err(error)
            if error.raw_os_error()
                == Some(windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD as i32) => {}
        result => return result,
    }
    if directory {
        create_junction(source, destination)
    } else {
        std::fs::hard_link(source, destination)
    }
}

/// `mklink /J` is the only junction maker short of reparse-point FFI. Both paths are quoted,
/// and a Windows path cannot contain the `"` that would end its quoting.
#[cfg(windows)]
fn create_junction(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;

    let output = std::process::Command::new("cmd")
        .arg("/d")
        .arg("/c")
        .raw_arg(format!(
            "mklink /J \"{}\" \"{}\"",
            destination.display(),
            source.display()
        ))
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    let detail = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    Err(std::io::Error::other(format!(
        "mklink /J: {}",
        String::from_utf8_lossy(detail).trim()
    )))
}

/// Outside the unix-only `tests`: hard links exist everywhere.
#[cfg(test)]
mod hard_link_tests {
    use super::*;

    #[test]
    fn a_hard_linked_destination_survives_an_edit_through_the_source() {
        let root = std::env::temp_dir().join(format!("orx-hard-link-{}", uuid::Uuid::new_v4()));
        let legacy = root.join("legacy");
        let isolated = root.join("isolated");
        let source = legacy.join("config.toml");
        let destination = isolated.join("config.toml");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&isolated).unwrap();
        std::fs::write(&source, "first").unwrap();
        std::fs::hard_link(&source, &destination).unwrap();
        write_marker(
            &destination.with_file_name("config.toml.orx-managed-link"),
            &source,
        )
        .unwrap();

        std::fs::write(&source, "second").unwrap();
        prepare_links(&isolated, &legacy, std::slice::from_ref(&source)).unwrap();

        assert!(same_file::is_same_file(&destination, &source).unwrap());
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "second");
        assert!(!std::fs::read_dir(&isolated)
            .unwrap()
            .flatten()
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .contains(".orx-conflict-")));

        // A launch with nothing edited must not adopt the link back as a copy.
        prepare_links(&isolated, &legacy, std::slice::from_ref(&source)).unwrap();
        assert!(same_file::is_same_file(&destination, &source).unwrap());
        assert!(!legacy.join(".config.toml.orx-backup").exists());
        assert!(!std::fs::symlink_metadata(&destination)
            .unwrap()
            .file_type()
            .is_symlink());
        std::fs::remove_dir_all(root).ok();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn native_store_smoke_test() {
        let root = std::env::temp_dir().join(format!("orx-native-store-{}", uuid::Uuid::new_v4()));
        let source = root.join("legacy/config.toml");
        let legacy = source.parent().unwrap().to_path_buf();
        let isolated = root.join("isolated");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(&source, "old").unwrap();
        prepare_links(&isolated, &legacy, std::slice::from_ref(&source)).unwrap();
        std::fs::remove_file(isolated.join("config.toml")).unwrap();
        std::fs::write(isolated.join("config.toml"), "new").unwrap();

        prepare_links(&isolated, &legacy, std::slice::from_ref(&source)).unwrap();

        assert_eq!(std::fs::read_to_string(&source).unwrap(), "new");
        assert!(source
            .parent()
            .unwrap()
            .read_dir()
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".orx-backup")));
        assert!(std::fs::symlink_metadata(isolated.join("config.toml"))
            .unwrap()
            .file_type()
            .is_symlink());
        std::fs::remove_file(isolated.join("config.toml")).unwrap();
        std::fs::write(isolated.join("config.toml"), "isolated change").unwrap();
        std::fs::write(&source, "legacy change").unwrap();
        prepare_links(&isolated, &legacy, std::slice::from_ref(&source)).unwrap();
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "legacy change");
        let auth = root.join("legacy/auth.json");
        std::fs::write(&auth, "legacy").unwrap();
        std::fs::write(isolated.join("auth.json"), "isolated").unwrap();
        prepare_links(&isolated, &legacy, std::slice::from_ref(&auth)).unwrap();
        assert_eq!(std::fs::read_to_string(&auth).unwrap(), "legacy");
        assert!(std::fs::read_dir(&isolated)
            .unwrap()
            .flatten()
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .contains(".orx-conflict-")));
        let session = root.join("sessions/2026/08/25/rollout-session-id.jsonl");
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::write(session, "{}").unwrap();
        assert!(tree_session_path(&root.join("sessions"), "session-id")
            .unwrap()
            .is_some());
        assert!(tree_session_path(&root.join("sessions"), "ession-id")
            .unwrap()
            .is_none());
        let chat_id = "e0ad13d3-d977-43a7-9994-e739975e82ec";
        let chat = root.join("chats").join("abc123def456").join(chat_id);
        std::fs::create_dir_all(&chat).unwrap();
        std::fs::write(chat.join("store.db"), []).unwrap();
        assert_eq!(
            cursor_session_path(&root, chat_id).unwrap().as_deref(),
            Some(chat.as_path())
        );
        assert!(cursor_session_path(&root, "missing-id").unwrap().is_none());
        let db = root.join("opencode.db");
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute_batch("CREATE TABLE session (id TEXT, directory TEXT); CREATE TABLE message (id TEXT, session_id TEXT, data TEXT); CREATE TABLE part (id TEXT, message_id TEXT, session_id TEXT, data TEXT); INSERT INTO session VALUES ('id', '/tmp');")
            .unwrap();
        assert!(opencode_has_session(&db, "id").unwrap());
        assert!(!opencode_has_session(&db, "missing").unwrap());
        std::fs::remove_dir_all(root).ok();
    }
}

#[cfg(test)]
mod codex_profile_tests {
    use super::*;

    fn roots() -> (PathBuf, PathBuf) {
        (PathBuf::from("/data/orx"), PathBuf::from("/home/ra"))
    }

    fn home(profile: CodexProfile, store: NativeStore, env: &[(&str, &str)]) -> PathBuf {
        let (data, user) = roots();
        codex_home_with(
            profile,
            store,
            || data,
            || user,
            |key| {
                env.iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| PathBuf::from(v))
            },
        )
    }

    #[test]
    fn profiles_map_to_harness_ids() {
        assert_eq!(
            CodexProfile::from_harness("codex"),
            Some(CodexProfile::Primary)
        );
        assert_eq!(
            CodexProfile::from_harness("codex2"),
            Some(CodexProfile::Secondary)
        );
        assert_eq!(CodexProfile::from_harness("claude-code"), None);
        assert_eq!(CodexProfile::from_harness("codex3"), None);
        assert_eq!(CodexProfile::of_harness("opencode"), CodexProfile::Primary);
        for profile in CodexProfile::ALL {
            assert_eq!(
                CodexProfile::from_harness(profile.harness_id()),
                Some(profile)
            );
        }
    }

    #[test]
    fn primary_home_is_unchanged() {
        use CodexProfile::Primary;
        assert_eq!(
            home(Primary, NativeStore::Legacy, &[]),
            PathBuf::from("/home/ra/.codex")
        );
        assert_eq!(
            home(
                Primary,
                NativeStore::Legacy,
                &[("CODEX_HOME", "/elsewhere")]
            ),
            PathBuf::from("/elsewhere")
        );
        // ORX_CODEX2_HOME never moves the primary account.
        assert_eq!(
            home(
                Primary,
                NativeStore::Legacy,
                &[(CODEX2_HOME_ENV, "/second")]
            ),
            PathBuf::from("/home/ra/.codex")
        );
        assert_eq!(
            home(Primary, NativeStore::Isolated, &[]),
            PathBuf::from("/data/orx/agents/codex")
        );
    }

    #[test]
    fn secondary_home_is_separate_and_configurable() {
        use CodexProfile::Secondary;
        assert_eq!(
            home(Secondary, NativeStore::Legacy, &[]),
            PathBuf::from("/home/ra/.codex2")
        );
        // CODEX_HOME (the primary's override) never leaks into codex2.
        assert_eq!(
            home(
                Secondary,
                NativeStore::Legacy,
                &[("CODEX_HOME", "/elsewhere")]
            ),
            PathBuf::from("/home/ra/.codex2")
        );
        assert_eq!(
            home(
                Secondary,
                NativeStore::Legacy,
                &[(CODEX2_HOME_ENV, "/second")]
            ),
            PathBuf::from("/second")
        );
        assert_eq!(
            home(Secondary, NativeStore::Isolated, &[]),
            PathBuf::from("/data/orx/agents/codex2")
        );
    }

    #[test]
    fn resume_lookup_stays_inside_the_profile_home() {
        let root = std::env::temp_dir().join(format!("orx-codex2-{}", uuid::Uuid::new_v4()));
        let homes = |profile: CodexProfile, store: NativeStore| {
            let name = match (profile, store) {
                (CodexProfile::Primary, NativeStore::Legacy) => "dot-codex",
                (CodexProfile::Primary, NativeStore::Isolated) => "agents/codex",
                (CodexProfile::Secondary, NativeStore::Legacy) => "dot-codex2",
                (CodexProfile::Secondary, NativeStore::Isolated) => "agents/codex2",
            };
            root.join(name)
        };
        let primary_thread = "11111111-1111-4111-8111-111111111111";
        let secondary_thread = "22222222-2222-4222-8222-222222222222";
        let write = |dir: &str, thread: &str| {
            let path = root.join(dir).join(format!(
                "sessions/2026/10/29/rollout-2026-10-29T20-00-00-{thread}.jsonl"
            ));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{}").unwrap();
            path
        };
        let primary_path = write("agents/codex", primary_thread);
        let secondary_path = write("dot-codex2", secondary_thread);

        let found = codex_session_in(&homes, CodexProfile::Secondary, secondary_thread)
            .unwrap()
            .unwrap();
        assert_eq!(found.path, secondary_path);
        assert_eq!(found.store, NativeStore::Legacy);
        let found = codex_session_in(&homes, CodexProfile::Primary, primary_thread)
            .unwrap()
            .unwrap();
        assert_eq!(found.path, primary_path);
        assert_eq!(found.store, NativeStore::Isolated);

        // Neither account resumes the other's thread.
        assert!(
            codex_session_in(&homes, CodexProfile::Primary, secondary_thread)
                .unwrap()
                .is_none()
        );
        assert!(
            codex_session_in(&homes, CodexProfile::Secondary, primary_thread)
                .unwrap()
                .is_none()
        );

        // Read-only identity lookups find either.
        assert_eq!(
            codex_session_any_in(&homes, secondary_thread)
                .unwrap()
                .unwrap()
                .path,
            secondary_path
        );
        assert_eq!(
            codex_session_any_in(&homes, primary_thread)
                .unwrap()
                .unwrap()
                .path,
            primary_path
        );
        std::fs::remove_dir_all(root).ok();
    }
}
