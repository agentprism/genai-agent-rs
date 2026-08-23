//! Native file-backed credential leases from Architecture v2 part 1 §3.8.

use crate::{
    ApiKeyCredential, CancellationToken, Credential, CredentialInfo, CredentialLease,
    CredentialStore, LocalBoxFuture, LocalCredentialLease, LocalCredentialStore, OAuthCredential,
    ProviderId, ProviderOAuthExtra, SecretString, SendBoxFuture, StoreError, Timestamp,
};
use futures_util::future::{Either, select};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Current schema version of the native credential file.
pub const CREDENTIAL_FILE_SCHEMA_VERSION: u32 = 1;

const LOCK_RETRY_DELAY: Duration = Duration::from_millis(5);

/// Native JSON credential store with cross-process read-modify-write leases.
///
/// The store writes a single document containing `schema_version` and a map
/// keyed by provider ID. Secrets are plaintext in that document, so callers
/// must place it in a host-owned private directory. On Unix, files created by
/// this implementation are restricted to the owner.
///
/// A stable `<path>.lock` companion is locked instead of the credential file
/// itself because commits atomically replace the credential file. The lock is
/// held from lease acquisition until commit or drop, including while OAuth is
/// refreshed by the provider resolver.
#[derive(Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
    lock_path: PathBuf,
}

impl FileCredentialStore {
    /// Creates a store backed by `path` without performing I/O.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_path = companion_lock_path(&path);
        Self { path, lock_path }
    }

    /// Returns the credential document path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the stable companion lock-file path.
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

impl fmt::Debug for FileCredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileCredentialStore")
            .field("path", &self.path)
            .field("lock_path", &self.lock_path)
            .finish()
    }
}

impl CredentialStore for FileCredentialStore {
    fn read(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<Credential>, StoreError>> {
        let path = self.path.clone();
        let lock_path = self.lock_path.clone();
        Box::pin(async move {
            let _lock = acquire_file_lock(&lock_path, FileLockMode::Shared, &cancellation).await?;
            cancellation.check().map_err(store_cancelled)?;
            let document = read_document(&path)?;
            document
                .credentials
                .get(provider.as_str())
                .map(PersistedCredential::to_credential)
                .transpose()
        })
    }

    fn list(
        &self,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Vec<CredentialInfo>, StoreError>> {
        let path = self.path.clone();
        let lock_path = self.lock_path.clone();
        Box::pin(async move {
            let _lock = acquire_file_lock(&lock_path, FileLockMode::Shared, &cancellation).await?;
            cancellation.check().map_err(store_cancelled)?;
            let document = read_document(&path)?;
            Ok(document
                .credentials
                .into_iter()
                .map(|(provider, credential)| CredentialInfo {
                    provider: ProviderId::new(provider),
                    credential_type: credential.credential_type(),
                })
                .collect())
        })
    }

    fn acquire_lease(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn CredentialLease>, StoreError>> {
        let store = self.clone();
        Box::pin(async move {
            let lock =
                acquire_file_lock(&store.lock_path, FileLockMode::Exclusive, &cancellation).await?;
            cancellation.check().map_err(store_cancelled)?;
            let document = read_document(&store.path)?;
            let current = document
                .credentials
                .get(provider.as_str())
                .map(PersistedCredential::to_credential)
                .transpose()?;
            Ok(Box::new(FileCredentialLease {
                store,
                provider,
                document: Some(document),
                current,
                replacement: None,
                lock: Some(lock),
            }) as Box<dyn CredentialLease>)
        })
    }
}

/// Local-executor adapter around [`FileCredentialStore`].
#[derive(Clone, Debug)]
pub struct LocalFileCredentialStore {
    inner: FileCredentialStore,
}

impl LocalFileCredentialStore {
    /// Creates a local store backed by `path` without performing I/O.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            inner: FileCredentialStore::new(path),
        }
    }

    /// Returns the credential document path.
    pub fn path(&self) -> &Path {
        self.inner.path()
    }

    /// Returns the stable companion lock-file path.
    pub fn lock_path(&self) -> &Path {
        self.inner.lock_path()
    }
}

struct LocalFileCredentialLease {
    inner: Option<Box<dyn CredentialLease>>,
}

impl LocalCredentialLease for LocalFileCredentialLease {
    fn current(&self) -> Option<&Credential> {
        self.inner.as_deref().and_then(CredentialLease::current)
    }

    fn replace(&mut self, credential: Option<Credential>) {
        self.inner
            .as_deref_mut()
            .expect("local file credential lease is live")
            .replace(credential);
    }

    fn commit(mut self: Box<Self>) -> LocalBoxFuture<'static, Result<(), StoreError>> {
        let lease = self
            .inner
            .take()
            .expect("local file credential lease is live");
        Box::pin(async move { lease.commit().await })
    }
}

impl LocalCredentialStore for LocalFileCredentialStore {
    fn read(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<Credential>, StoreError>> {
        let store = self.inner.clone();
        Box::pin(async move { CredentialStore::read(&store, provider, cancellation).await })
    }

    fn list(
        &self,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Vec<CredentialInfo>, StoreError>> {
        let store = self.inner.clone();
        Box::pin(async move { CredentialStore::list(&store, cancellation).await })
    }

    fn acquire_lease(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Box<dyn LocalCredentialLease>, StoreError>> {
        let store = self.inner.clone();
        Box::pin(async move {
            let lease = CredentialStore::acquire_lease(&store, provider, cancellation).await?;
            Ok(Box::new(LocalFileCredentialLease { inner: Some(lease) })
                as Box<dyn LocalCredentialLease>)
        })
    }
}

struct FileCredentialLease {
    store: FileCredentialStore,
    provider: ProviderId,
    document: Option<PersistedCredentialFile>,
    current: Option<Credential>,
    replacement: Option<Option<Credential>>,
    lock: Option<File>,
}

impl CredentialLease for FileCredentialLease {
    fn current(&self) -> Option<&Credential> {
        self.current.as_ref()
    }

    fn replace(&mut self, credential: Option<Credential>) {
        self.replacement = Some(credential);
    }

    fn commit(mut self: Box<Self>) -> SendBoxFuture<'static, Result<(), StoreError>> {
        Box::pin(async move {
            if let Some(replacement) = self.replacement.take() {
                let mut document = self
                    .document
                    .take()
                    .expect("file credential lease owns its document");
                match replacement {
                    Some(credential) => {
                        document.credentials.insert(
                            self.provider.as_str().to_owned(),
                            PersistedCredential::from_credential(credential),
                        );
                    }
                    None => {
                        document.credentials.remove(self.provider.as_str());
                    }
                }
                write_document(&self.store.path, &document)?;
            }
            self.lock.take();
            Ok(())
        })
    }
}

#[derive(Clone, Copy)]
enum FileLockMode {
    Shared,
    Exclusive,
}

async fn acquire_file_lock(
    path: &Path,
    mode: FileLockMode,
    cancellation: &CancellationToken,
) -> Result<File, StoreError> {
    ensure_parent(path)?;
    let file = open_private_lock_file(path)?;

    loop {
        cancellation.check().map_err(store_cancelled)?;
        let result = match mode {
            FileLockMode::Shared => File::try_lock_shared(&file),
            FileLockMode::Exclusive => File::try_lock(&file),
        };
        match result {
            Ok(()) => return Ok(file),
            Err(error) => {
                let error: io::Error = error.into();
                if error.kind() != io::ErrorKind::WouldBlock {
                    return Err(io_store_error("credential_lock", "lock", path, error));
                }
                let delay = Box::pin(futures_timer::Delay::new(LOCK_RETRY_DELAY));
                let cancelled = Box::pin(cancellation.cancelled());
                if let Either::Right(((), _)) = select(delay, cancelled).await {
                    return Err(store_cancelled(crate::CancellationError));
                }
            }
        }
    }
}

fn open_private_lock_file(path: &Path) -> Result<File, StoreError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    set_private_create_mode(&mut options);
    let file = options
        .open(path)
        .map_err(|error| io_store_error("credential_lock", "open lock file", path, error))?;
    set_private_file_permissions(&file, path)?;
    Ok(file)
}

fn read_document(path: &Path) -> Result<PersistedCredentialFile, StoreError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PersistedCredentialFile::default());
        }
        Err(error) => {
            return Err(io_store_error(
                "credential_read",
                "read credential file",
                path,
                error,
            ));
        }
    };
    let document: PersistedCredentialFile = serde_json::from_slice(&bytes).map_err(|error| {
        StoreError::new(
            "credential_format",
            format!("credential file {} is invalid: {error}", path.display()),
        )
    })?;
    if document.schema_version != CREDENTIAL_FILE_SCHEMA_VERSION {
        return Err(StoreError::new(
            "credential_schema",
            format!(
                "credential file {} has unsupported schema version {}",
                path.display(),
                document.schema_version
            ),
        ));
    }
    Ok(document)
}

fn write_document(path: &Path, document: &PersistedCredentialFile) -> Result<(), StoreError> {
    ensure_parent(path)?;
    let parent = usable_parent(path);
    let mut temporary = tempfile::Builder::new()
        .prefix(".pi-ai-credentials-")
        .tempfile_in(parent)
        .map_err(|error| {
            io_store_error(
                "credential_write",
                "create temporary credential file",
                path,
                error,
            )
        })?;
    set_private_file_permissions(temporary.as_file(), path)?;

    let mut bytes = serde_json::to_vec_pretty(document).map_err(|error| {
        StoreError::new(
            "credential_format",
            format!(
                "could not encode credential file {}: {error}",
                path.display()
            ),
        )
    })?;
    bytes.push(b'\n');
    temporary.write_all(&bytes).map_err(|error| {
        io_store_error(
            "credential_write",
            "write temporary credential file",
            path,
            error,
        )
    })?;
    temporary.as_file_mut().sync_all().map_err(|error| {
        io_store_error(
            "credential_write",
            "sync temporary credential file",
            path,
            error,
        )
    })?;
    temporary.persist(path).map_err(|error| {
        io_store_error(
            "credential_write",
            "replace credential file",
            path,
            error.error,
        )
    })?;
    sync_parent_directory(parent, path)?;
    Ok(())
}

fn ensure_parent(path: &Path) -> Result<(), StoreError> {
    let parent = usable_parent(path);
    fs::create_dir_all(parent).map_err(|error| {
        io_store_error(
            "credential_write",
            "create credential directory",
            parent,
            error,
        )
    })
}

fn usable_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn companion_lock_path(path: &Path) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_owned();
    value.push(".lock");
    value.into()
}

fn store_cancelled(_error: crate::CancellationError) -> StoreError {
    StoreError::new("cancelled", "credential operation cancelled")
}

fn io_store_error(code: &str, operation: &str, path: &Path, error: io::Error) -> StoreError {
    StoreError::new(
        code,
        format!("could not {operation} {}: {error}", path.display()),
    )
}

#[cfg(unix)]
fn set_private_create_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_create_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_private_file_permissions(file: &File, path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| {
            io_store_error(
                "credential_write",
                "set credential file permissions",
                path,
                error,
            )
        })
}

#[cfg(not(unix))]
fn set_private_file_permissions(_file: &File, _path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path, target: &Path) -> Result<(), StoreError> {
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            io_store_error(
                "credential_write",
                "sync credential directory after replacing",
                target,
                error,
            )
        })
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path, _target: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedCredentialFile {
    schema_version: u32,
    credentials: BTreeMap<String, PersistedCredential>,
}

impl Default for PersistedCredentialFile {
    fn default() -> Self {
        Self {
            schema_version: CREDENTIAL_FILE_SCHEMA_VERSION,
            credentials: BTreeMap::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum PersistedCredential {
    #[serde(rename = "api_key")]
    ApiKey {
        key: Option<String>,
        #[serde(default)]
        environment: BTreeMap<String, String>,
    },
    #[serde(rename = "oauth")]
    OAuth {
        access: String,
        refresh: String,
        expires_at: Timestamp,
        extra: PersistedProviderOAuthExtra,
    },
}

impl PersistedCredential {
    fn credential_type(&self) -> crate::CredentialType {
        match self {
            Self::ApiKey { .. } => crate::CredentialType::ApiKey,
            Self::OAuth { .. } => crate::CredentialType::OAuth,
        }
    }

    fn from_credential(credential: Credential) -> Self {
        match credential {
            Credential::ApiKey(credential) => Self::ApiKey {
                key: credential.key.map(SecretString::into_secret),
                environment: credential.environment,
            },
            Credential::OAuth(credential) => Self::OAuth {
                access: credential.access.into_secret(),
                refresh: credential.refresh.into_secret(),
                expires_at: credential.expires_at,
                extra: PersistedProviderOAuthExtra::from_extra(credential.extra),
            },
        }
    }

    fn to_credential(&self) -> Result<Credential, StoreError> {
        Ok(match self {
            Self::ApiKey { key, environment } => Credential::ApiKey(ApiKeyCredential {
                key: key.clone().map(SecretString::new),
                environment: environment.clone(),
            }),
            Self::OAuth {
                access,
                refresh,
                expires_at,
                extra,
            } => Credential::OAuth(OAuthCredential {
                access: SecretString::new(access),
                refresh: SecretString::new(refresh),
                expires_at: *expires_at,
                extra: extra.to_extra()?,
            }),
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "provider", content = "value")]
enum PersistedProviderOAuthExtra {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "radius")]
    Radius {
        gateway_url: url::Url,
        organization_id: Option<String>,
    },
    #[serde(rename = "github_copilot")]
    GitHubCopilot {
        api_endpoint: url::Url,
        account_id: Option<String>,
    },
    #[serde(rename = "openai_codex")]
    OpenAiCodex { account_id: String },
    #[serde(rename = "custom")]
    Custom {
        schema: crate::ExtensionId,
        schema_version: u32,
        value_json: String,
    },
}

impl PersistedProviderOAuthExtra {
    fn from_extra(extra: ProviderOAuthExtra) -> Self {
        match extra {
            ProviderOAuthExtra::None => Self::None,
            ProviderOAuthExtra::Radius {
                gateway_url,
                organization_id,
            } => Self::Radius {
                gateway_url,
                organization_id,
            },
            ProviderOAuthExtra::GitHubCopilot {
                api_endpoint,
                account_id,
            } => Self::GitHubCopilot {
                api_endpoint,
                account_id,
            },
            ProviderOAuthExtra::OpenAiCodex { account_id } => Self::OpenAiCodex { account_id },
            ProviderOAuthExtra::Custom {
                schema,
                schema_version,
                value,
            } => Self::Custom {
                schema,
                schema_version,
                value_json: value.get().to_owned(),
            },
        }
    }

    fn to_extra(&self) -> Result<ProviderOAuthExtra, StoreError> {
        Ok(match self {
            Self::None => ProviderOAuthExtra::None,
            Self::Radius {
                gateway_url,
                organization_id,
            } => ProviderOAuthExtra::Radius {
                gateway_url: gateway_url.clone(),
                organization_id: organization_id.clone(),
            },
            Self::GitHubCopilot {
                api_endpoint,
                account_id,
            } => ProviderOAuthExtra::GitHubCopilot {
                api_endpoint: api_endpoint.clone(),
                account_id: account_id.clone(),
            },
            Self::OpenAiCodex { account_id } => ProviderOAuthExtra::OpenAiCodex {
                account_id: account_id.clone(),
            },
            Self::Custom {
                schema,
                schema_version,
                value_json,
            } => ProviderOAuthExtra::Custom {
                schema: schema.clone(),
                schema_version: *schema_version,
                value: RawValue::from_string(value_json.clone()).map_err(|error| {
                    StoreError::new(
                        "credential_format",
                        format!("custom OAuth credential JSON is invalid: {error}"),
                    )
                })?,
            },
        })
    }
}
