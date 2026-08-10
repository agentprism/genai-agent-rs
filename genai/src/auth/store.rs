//! Credential persistence.
//!
//! [`CredentialStore`] is the app-owned storage seam (pi's `CredentialStore`,
//! credential-store.ts / types.ts:65-94). [`FileCredentialStore`] is the default
//! file-backed implementation:
//!
//! - On-disk layout is a JSON object keyed by provider id, one credential per
//!   provider — exactly pi's `auth.json` shape. This keeps mixed provider files
//!   (api-key entries for other providers) intact: only the requested provider's
//!   entry is (de)serialized as an [`OAuthCredential`].
//! - Default path: `$GENAI_AUTH_FILE`, else `~/.genai/auth.json`.
//! - Writes are atomic (write to a temp file in the same directory, then rename).
//!   On unix the credential file and temp file are `0600` and the parent
//!   directory `0700` (an existing directory is tightened too, best-effort). The
//!   temp file is created exclusively (`O_CREAT | O_EXCL | O_NOFOLLOW`) with a
//!   CSPRNG-random name, so it never overwrites, follows a symlink, or uses a
//!   guessable path. See [`crate::auth::genai_integration`] for the cross-process
//!   advisory lock ([`CredentialStore::lock_path`]) used around refreshes.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::auth::credential::OAuthCredential;
use crate::auth::error::{Error, Result};

/// Environment variable that overrides the credential file path.
pub const AUTH_FILE_ENV: &str = "GENAI_AUTH_FILE";
/// Default directory under the home directory.
pub const DEFAULT_AUTH_DIR: &str = ".genai";
/// Default credential file name.
pub const DEFAULT_AUTH_FILE: &str = "auth.json";

/// App-owned credential storage keyed by provider id.
///
/// Object-safe so it can be used as `Arc<dyn CredentialStore>`.
pub trait CredentialStore: Send + Sync {
    /// Read the stored credential for a provider (possibly expired), or `None`.
    fn load(&self, provider_id: &str) -> Result<Option<OAuthCredential>>;

    /// Persist (create or replace) the credential for a provider.
    fn store(&self, provider_id: &str, credential: &OAuthCredential) -> Result<()>;

    /// Remove the credential for a provider (logout). Missing is not an error.
    fn delete(&self, provider_id: &str) -> Result<()>;

    /// List provider ids that currently have a stored credential.
    fn list(&self) -> Result<Vec<String>>;

    /// Path to a sidecar advisory-lock file for **cross-process** serialization
    /// of a read-modify-write refresh, or `None` if this store cannot be
    /// cross-process-locked.
    ///
    /// The genai resolver ([`crate::auth::genai_integration`]) acquires an OS advisory
    /// lock (`flock`) on this path around its load-check-refresh-store sequence
    /// so two processes sharing the same credential file cannot both POST the
    /// same rotating refresh token. A sidecar `.lock` file (rather than the
    /// credential file itself) is used because the atomic write replaces the
    /// credential file's inode via `rename`, which would drop a lock held on the
    /// old inode. The default returns `None` (no cross-process locking).
    fn lock_path(&self) -> Option<PathBuf> {
        None
    }

    /// Serialized read-modify-write (pi's `modify`, types.ts:86-90).
    ///
    /// `f` receives the current credential and returns the next one (or `None`
    /// to leave the entry unchanged). Returns the post-write credential. The
    /// default implementation is load-then-store and is *not* atomic across
    /// processes; [`FileCredentialStore`] overrides it under an in-process lock.
    fn modify(
        &self,
        provider_id: &str,
        f: &mut dyn FnMut(Option<OAuthCredential>) -> Result<Option<OAuthCredential>>,
    ) -> Result<Option<OAuthCredential>> {
        let current = self.load(provider_id)?;
        let next = f(current.clone())?;
        if let Some(cred) = &next {
            self.store(provider_id, cred)?;
        }
        Ok(next.or(current))
    }
}

/// Default file-backed [`CredentialStore`] writing a pi-parity `auth.json`.
#[derive(Debug)]
pub struct FileCredentialStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileCredentialStore {
    /// Create a store at an explicit path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    /// Create a store at the default path (`$GENAI_AUTH_FILE`, else `~/.genai/auth.json`).
    pub fn with_default_path() -> Result<Self> {
        Ok(Self::new(default_path()?))
    }

    /// The resolved credential file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read_root(&self) -> Result<Map<String, Value>> {
        match std::fs::read(&self.path) {
            Ok(bytes) if bytes.is_empty() => Ok(Map::new()),
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes)? {
                Value::Object(map) => Ok(map),
                _ => Ok(Map::new()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    fn write_root(&self, root: &Map<String, Value>) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(root)?;
        atomic_write(&self.path, &bytes)
    }
}

impl CredentialStore for FileCredentialStore {
    fn load(&self, provider_id: &str) -> Result<Option<OAuthCredential>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let root = self.read_root()?;
        match root.get(provider_id) {
            Some(value) => Ok(Some(serde_json::from_value(value.clone())?)),
            None => Ok(None),
        }
    }

    fn store(&self, provider_id: &str, credential: &OAuthCredential) -> Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut root = self.read_root()?;
        root.insert(provider_id.to_string(), serde_json::to_value(credential)?);
        self.write_root(&root)
    }

    fn delete(&self, provider_id: &str) -> Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut root = self.read_root()?;
        if root.remove(provider_id).is_some() {
            self.write_root(&root)?;
        }
        Ok(())
    }

    fn list(&self) -> Result<Vec<String>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        Ok(self.read_root()?.keys().cloned().collect())
    }

    fn lock_path(&self) -> Option<PathBuf> {
        // Sidecar `<auth.json>.lock` (append, do not replace the extension).
        let mut os = self.path.clone().into_os_string();
        os.push(".lock");
        Some(PathBuf::from(os))
    }

    fn modify(
        &self,
        provider_id: &str,
        f: &mut dyn FnMut(Option<OAuthCredential>) -> Result<Option<OAuthCredential>>,
    ) -> Result<Option<OAuthCredential>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut root = self.read_root()?;
        let current = match root.get(provider_id) {
            Some(value) => Some(serde_json::from_value::<OAuthCredential>(value.clone())?),
            None => None,
        };
        let next = f(current.clone())?;
        if let Some(cred) = &next {
            root.insert(provider_id.to_string(), serde_json::to_value(cred)?);
            self.write_root(&root)?;
        }
        Ok(next.or(current))
    }
}

/// Resolve the default credential file path from the ambient environment.
pub fn default_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok());
    resolve_default_path(std::env::var(AUTH_FILE_ENV).ok(), home)
}

/// Pure path resolution used by [`default_path`] (kept separate for testability).
fn resolve_default_path(env_override: Option<String>, home: Option<String>) -> Result<PathBuf> {
    if let Some(override_path) = env_override.filter(|s| !s.is_empty()) {
        return Ok(expand_tilde(&override_path, home.as_deref()));
    }
    let home = home
        .filter(|s| !s.is_empty())
        .ok_or(Error::HomeDirNotFound)?;
    Ok(PathBuf::from(home)
        .join(DEFAULT_AUTH_DIR)
        .join(DEFAULT_AUTH_FILE))
}

/// Expand a single leading `~` (or `~/...`) using `home`, if available.
fn expand_tilde(path: &str, home: Option<&str>) -> PathBuf {
    if path == "~" {
        if let Some(home) = home {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// Number of exclusive-create attempts before giving up on a unique temp name.
/// With a 96-bit random suffix a collision is astronomically unlikely; the retry
/// only guards against a stale leftover temp file with the same random name.
const MAX_TEMP_CREATE_ATTEMPTS: usize = 8;

/// Atomically write `bytes` to `path`: temp file in the same dir + rename.
///
/// Hardening (unix): the parent dir is created (or tightened) to `0700`; the
/// temp file is created with `O_CREAT | O_EXCL` (never overwrites) and
/// `O_NOFOLLOW` (never opens through a symlink) at mode `0600`; the temp suffix
/// is a fresh CSPRNG value (not a predictable pid+timestamp). The `rename` is
/// atomic on the same filesystem, so a crash cannot leave a half-written or
/// world-readable credential file in place of the real one.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    create_dir_secure(parent)?;

    // Unique temp name in the same directory so `rename` is atomic (same fs).
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "auth.json".to_string());

    let mut last_err: Option<Error> = None;
    for _ in 0..MAX_TEMP_CREATE_ATTEMPTS {
        let tmp = parent.join(format!(".{file_name}.tmp.{}", random_suffix()?));
        match write_file_secure(&tmp, bytes) {
            Ok(()) => {
                return match std::fs::rename(&tmp, path) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let _ = std::fs::remove_file(&tmp);
                        Err(Error::Io(e))
                    }
                };
            }
            // O_EXCL collision with a leftover temp file: retry with a new suffix.
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(Error::Io(e));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not create a unique temp file for the credential store",
        ))
    }))
}

fn write_file_secure(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_CREAT | O_EXCL (create_new) + O_NOFOLLOW + mode 0600: exclusive,
        // symlink-refusing, owner-only creation of a brand-new temp file.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        file.flush()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // No mode/O_NOFOLLOW on non-unix, but still create exclusively (O_EXCL).
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(bytes)?;
        file.flush()?;
        Ok(())
    }
}

/// Ensure the credential directory exists and is `0700` (unix).
///
/// A newly created directory is made `0700` at creation. An **existing** target
/// directory is also tightened to `0700` (best-effort: a chmod failure — e.g. a
/// directory the process does not own — is ignored rather than failing the
/// write, since the credential file itself is always `0600`). Filesystem roots
/// and `.` / `..` are never chmod'd.
fn create_dir_secure(dir: &Path) -> Result<()> {
    if dir.as_os_str().is_empty() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if dir.exists() {
            let is_dot = matches!(dir.as_os_str().to_str(), Some(".") | Some(".."));
            // Skip roots (parent == None) and the current/parent dir markers.
            if dir.parent().is_some() && !is_dot {
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
            return Ok(());
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        if dir.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(dir)?;
        Ok(())
    }
}

/// A fresh CSPRNG hex suffix for the temp file name (replaces the old,
/// predictable pid+timestamp so the temp path is unguessable).
fn random_suffix() -> Result<String> {
    let mut bytes = [0u8; 12];
    getrandom::getrandom(&mut bytes).map_err(|e| Error::Random(e.to_string()))?;
    let mut out = String::with_capacity(bytes.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &b in &bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (tempfile::TempDir, FileCredentialStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        (dir, FileCredentialStore::new(path))
    }

    #[test]
    fn write_read_delete_round_trip() {
        let (_dir, store) = tmp_store();
        assert_eq!(store.load("openai-codex").unwrap(), None);
        assert!(store.list().unwrap().is_empty());

        let cred = OAuthCredential::new("acc", Some("ref".into()), Some(123), Some("acct".into()));
        store.store("openai-codex", &cred).unwrap();

        assert_eq!(store.load("openai-codex").unwrap(), Some(cred.clone()));
        assert_eq!(store.list().unwrap(), vec!["openai-codex".to_string()]);

        store.delete("openai-codex").unwrap();
        assert_eq!(store.load("openai-codex").unwrap(), None);
        // Deleting a missing entry is a no-op.
        store.delete("openai-codex").unwrap();
    }

    #[test]
    fn atomic_replace_and_preserves_other_providers() {
        let (_dir, store) = tmp_store();
        // Seed a non-oauth entry for another provider directly on disk.
        std::fs::write(
            store.path(),
            br#"{"other":{"type":"api_key","key":"sk-xyz"}}"#,
        )
        .unwrap();

        let cred = OAuthCredential::new("acc1", Some("ref1".into()), Some(1), None);
        store.store("openai-codex", &cred).unwrap();

        // Replace it (atomic overwrite).
        let cred2 = OAuthCredential::new("acc2", Some("ref2".into()), Some(2), None);
        store.store("openai-codex", &cred2).unwrap();
        assert_eq!(store.load("openai-codex").unwrap(), Some(cred2));

        // The unrelated api-key entry must be untouched.
        let root: Value = serde_json::from_slice(&std::fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(root["other"]["key"], "sk-xyz");

        // No stray temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(store.path().parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp files leaked: {leftovers:?}");
    }

    #[test]
    fn modify_read_modify_write() {
        let (_dir, store) = tmp_store();
        let seed = OAuthCredential::new("old", Some("r".into()), Some(1), None);
        store.store("p", &seed).unwrap();

        let mut saw_current: Option<OAuthCredential> = None;
        let result = store
            .modify("p", &mut |current| {
                saw_current = current.clone();
                let mut next = current.unwrap();
                next.access_token = "new".into();
                Ok(Some(next))
            })
            .unwrap();

        assert_eq!(saw_current.unwrap().access_token, "old");
        assert_eq!(result.unwrap().access_token, "new");
        assert_eq!(store.load("p").unwrap().unwrap().access_token, "new");
    }

    #[cfg(unix)]
    #[test]
    fn file_and_dir_have_secure_perms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join(".genai");
        let path = nested.join("auth.json");
        let store = FileCredentialStore::new(&path);

        let cred = OAuthCredential::new("acc", None, None, None);
        store.store("openai-codex", &cred).unwrap();

        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "auth.json must be 0600, got {file_mode:o}"
        );

        let dir_mode = std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "dir must be 0700, got {dir_mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn write_file_secure_is_exclusive_and_symlink_refusing() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();

        // Fresh create succeeds and is 0600.
        let target = dir.path().join("fresh.bin");
        write_file_secure(&target, b"hello").unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "temp file must be 0600, got {mode:o}");

        // O_EXCL: creating over an existing path fails with AlreadyExists.
        let err = write_file_secure(&target, b"again").unwrap_err();
        match err {
            Error::Io(e) => assert_eq!(
                e.kind(),
                std::io::ErrorKind::AlreadyExists,
                "expected AlreadyExists, got {e:?}"
            ),
            other => panic!("unexpected error: {other:?}"),
        }

        // O_EXCL + O_NOFOLLOW: a symlink at the target path is refused (never
        // followed / overwritten), so a symlink-swap attack cannot redirect the
        // write onto an attacker-chosen file.
        let link = dir.path().join("link.bin");
        std::os::unix::fs::symlink(dir.path().join("elsewhere.bin"), &link).unwrap();
        assert!(
            write_file_secure(&link, b"nope").is_err(),
            "write through a symlink must be refused"
        );
        // The symlink target was not created.
        assert!(!dir.path().join("elsewhere.bin").exists());
    }

    #[cfg(unix)]
    #[test]
    fn existing_target_dir_is_tightened_to_0700() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        let outer = tempfile::tempdir().unwrap();
        // Pre-create the credential dir with loose 0755 perms.
        let nested = outer.path().join("creds");
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(&nested)
            .unwrap();
        assert_eq!(
            std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o755
        );

        let store = FileCredentialStore::new(nested.join("auth.json"));
        let cred = OAuthCredential::new("acc", None, None, None);
        store.store("openai-codex", &cred).unwrap();

        // The existing dir was tightened to 0700 on write.
        let dir_mode = std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "existing dir must be tightened to 0700, got {dir_mode:o}"
        );
    }

    #[test]
    fn lock_path_is_sidecar_of_auth_file() {
        let store = FileCredentialStore::new("/tmp/whatever/auth.json");
        assert_eq!(
            store.lock_path(),
            Some(PathBuf::from("/tmp/whatever/auth.json.lock"))
        );
    }

    #[test]
    fn resolve_default_path_env_override_wins() {
        let p = resolve_default_path(Some("/tmp/custom/auth.json".into()), Some("/home/u".into()))
            .unwrap();
        assert_eq!(p, PathBuf::from("/tmp/custom/auth.json"));
    }

    #[test]
    fn resolve_default_path_expands_tilde_override() {
        let p = resolve_default_path(Some("~/creds.json".into()), Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/creds.json"));
    }

    #[test]
    fn resolve_default_path_uses_home_default() {
        let p = resolve_default_path(None, Some("/home/u".into())).unwrap();
        assert_eq!(p, PathBuf::from("/home/u/.genai/auth.json"));
    }

    #[test]
    fn resolve_default_path_errors_without_home() {
        let err = resolve_default_path(None, None).unwrap_err();
        assert!(matches!(err, Error::HomeDirNotFound));
    }

    #[test]
    fn default_path_reads_env_override() {
        // `default_path()` is a thin wrapper: it reads `AUTH_FILE_ENV` (plus HOME/USERPROFILE)
        // and delegates to `resolve_default_path`. Assert the wrapper agrees with resolving from
        // the current ambient environment, i.e. that it wires `AUTH_FILE_ENV` and HOME through.
        //
        // NOTE: the original crate (edition 2021) set/restored the env var with
        // `std::env::set_var`. After folding into genai-agentprism (edition 2024, which makes
        // `std::env::set_var`/`remove_var` `unsafe`, and which sets `unsafe_code = "forbid"`
        // crate-wide so no `unsafe` block is permitted) that global-mutation approach no longer
        // compiles. The override *logic* — that a non-empty `AUTH_FILE_ENV` override wins over
        // HOME — is still covered directly by `resolve_default_path_env_override_wins`.
        let home = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok());
        let expected = resolve_default_path(std::env::var(AUTH_FILE_ENV).ok(), home);
        assert_eq!(default_path().ok(), expected.ok());
    }
}
