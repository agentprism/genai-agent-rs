//! Skill discovery contracts and deterministic content identities.

use agentprism_ai::{LocalBoxFuture, SendBoxFuture};
use ignore::{Match, gitignore::GitignoreBuilder};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Path, PathBuf},
};

/// Persisted recorded-skill schema version.
pub const RECORDED_SKILL_SCHEMA_VERSION: u32 = 1;

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1_024;
const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

/// Stable open identifier for one skill.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SkillId(String);

impl SkillId {
    /// Creates an identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SkillId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable SHA-256 identity for loaded external content.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentDigest(String);

impl ContentDigest {
    /// Computes a domain-separated SHA-256 digest.
    pub fn sha256(domain: &str, fields: impl IntoIterator<Item = impl AsRef<[u8]>>) -> Self {
        let mut digest = Sha256::new();
        update_digest_field(&mut digest, domain.as_bytes());
        for field in fields {
            update_digest_field(&mut digest, field.as_ref());
        }
        Self(format!("sha256:{:x}", digest.finalize()))
    }

    /// Borrows the algorithm-prefixed digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Model-visible metadata for one skill.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SkillDescriptor {
    /// Stable catalog identity.
    pub id: SkillId,
    /// Model-visible skill name.
    pub name: String,
    /// Short description of when to use the skill.
    pub description: String,
    /// Addressed location of the declaring markdown file.
    pub location: String,
    /// Whether ordinary model-visible discovery excludes this skill.
    pub disable_model_invocation: bool,
}

/// One ordered prompt fragment supplied by a skill.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptFragment {
    /// Optional fragment label for application-defined composition.
    pub name: Option<String>,
    /// Exact model-visible fragment text.
    pub content: String,
}

/// One byte-preserved resource attached to a skill.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SkillResource {
    /// Skill-relative resource path.
    pub path: String,
    /// Exact resource bytes.
    pub data: Vec<u8>,
}

/// Fully loaded skill with a deterministic content identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LoadedSkill {
    /// Validated model-visible metadata.
    pub descriptor: SkillDescriptor,
    /// Ordered prompt fragments.
    pub prompt_fragments: Vec<PromptFragment>,
    /// Attached resources.
    pub resources: Vec<SkillResource>,
    /// Digest of metadata, prompt fragments, and resources.
    pub digest: ContentDigest,
}

impl LoadedSkill {
    /// Builds a loaded skill and computes its stable digest.
    pub fn new(
        descriptor: SkillDescriptor,
        prompt_fragments: Vec<PromptFragment>,
        resources: Vec<SkillResource>,
    ) -> Result<Self, SkillError> {
        let diagnostics = validate_skill_descriptor(&descriptor);
        if !diagnostics.is_empty() {
            return Err(SkillError::InvalidMetadata { diagnostics });
        }
        let digest = digest_skill(&descriptor, &prompt_fragments, &resources);
        Ok(Self {
            descriptor,
            prompt_fragments,
            resources,
            digest,
        })
    }

    /// Concatenates the skill's ordered prompt fragments.
    pub fn prompt_text(&self) -> String {
        self.prompt_fragments
            .iter()
            .map(|fragment| fragment.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Stable skill-loading diagnostic code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillDiagnosticCode {
    /// Metadata lookup or symlink resolution failed.
    FileInfoFailed,
    /// Directory enumeration failed.
    ListFailed,
    /// A skill or ignore file could not be read.
    ReadFailed,
    /// A source document could not be parsed.
    ParseFailed,
    /// Parsed metadata violates the skill contract.
    InvalidMetadata,
}

/// Non-fatal warning discovered while inspecting a skill document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SkillDiagnostic {
    /// Stable diagnostic classification.
    pub code: SkillDiagnosticCode,
    /// Human-readable diagnostic.
    pub message: String,
    /// Addressed source path.
    pub path: String,
}

/// Result of inspecting one markdown skill declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillDocumentResult {
    /// Loaded skill when the document contains the required metadata.
    pub skill: Option<LoadedSkill>,
    /// Warnings emitted while inspecting the document.
    pub diagnostics: Vec<SkillDiagnostic>,
}

/// Skill catalog failure.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SkillError {
    /// A requested skill is absent.
    NotFound {
        /// Missing skill identity.
        id: SkillId,
    },
    /// More than one skill declared the same identity.
    Duplicate {
        /// Duplicate skill identity.
        id: SkillId,
    },
    /// Skill metadata is invalid.
    InvalidMetadata {
        /// Every metadata diagnostic found in one validation pass.
        diagnostics: Vec<SkillDiagnostic>,
    },
    /// Skill content changed after durable operation admission.
    DigestMismatch {
        /// Skill whose content changed.
        id: SkillId,
        /// Digest recorded by the admitted operation.
        recorded: ContentDigest,
        /// Digest currently supplied by the catalog.
        current: ContentDigest,
    },
    /// Catalog backend failure.
    Catalog {
        /// Sanitized backend diagnostic.
        message: String,
    },
}

impl fmt::Display for SkillError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { id } => write!(formatter, "skill {id} was not found"),
            Self::Duplicate { id } => write!(formatter, "skill {id} is declared more than once"),
            Self::InvalidMetadata { diagnostics } => write!(
                formatter,
                "skill metadata is invalid: {}",
                diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Self::DigestMismatch {
                id,
                recorded,
                current,
            } => write!(
                formatter,
                "skill {id} changed since admission ({recorded} != {current})"
            ),
            Self::Catalog { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SkillError {}

/// Send skill discovery and loading seam from Architecture v2 part 2 §7.9.
pub trait SkillCatalog: Send + Sync + 'static {
    /// Lists the catalog's current validated descriptors.
    fn list(&self) -> SendBoxFuture<'_, Result<Vec<SkillDescriptor>, SkillError>>;

    /// Loads one skill by stable identity.
    fn load(&self, id: &SkillId) -> SendBoxFuture<'_, Result<LoadedSkill, SkillError>>;
}

/// Single-threaded counterpart of [`SkillCatalog`].
pub trait LocalSkillCatalog: 'static {
    /// Lists the catalog's current validated descriptors.
    fn list(&self) -> LocalBoxFuture<'_, Result<Vec<SkillDescriptor>, SkillError>>;

    /// Loads one skill by stable identity.
    fn load(&self, id: &SkillId) -> LocalBoxFuture<'_, Result<LoadedSkill, SkillError>>;
}

/// Deterministic immutable catalog for embedded resources, tests, and FFI input.
#[derive(Clone, Debug, Default)]
pub struct StaticSkillCatalog {
    skills: BTreeMap<SkillId, LoadedSkill>,
    order: Vec<SkillId>,
}

impl StaticSkillCatalog {
    /// Builds a catalog, rejecting duplicate identities.
    pub fn new(skills: impl IntoIterator<Item = LoadedSkill>) -> Result<Self, SkillError> {
        let mut by_id = BTreeMap::new();
        let mut order = Vec::new();
        for skill in skills {
            let id = skill.descriptor.id.clone();
            if by_id.insert(id.clone(), skill).is_some() {
                return Err(SkillError::Duplicate { id });
            }
            order.push(id);
        }
        Ok(Self {
            skills: by_id,
            order,
        })
    }

    /// Lists descriptors without requiring trait-method disambiguation.
    pub fn list(&self) -> SendBoxFuture<'_, Result<Vec<SkillDescriptor>, SkillError>> {
        Box::pin(async move { Ok(self.descriptors_in_order()) })
    }

    /// Loads a skill without requiring trait-method disambiguation.
    pub fn load(&self, id: &SkillId) -> SendBoxFuture<'_, Result<LoadedSkill, SkillError>> {
        let id = id.clone();
        Box::pin(async move {
            self.skills
                .get(&id)
                .cloned()
                .ok_or(SkillError::NotFound { id })
        })
    }

    fn descriptors_in_order(&self) -> Vec<SkillDescriptor> {
        self.order
            .iter()
            .map(|id| self.skills[id].descriptor.clone())
            .collect()
    }
}

impl SkillCatalog for StaticSkillCatalog {
    fn list(&self) -> SendBoxFuture<'_, Result<Vec<SkillDescriptor>, SkillError>> {
        Self::list(self)
    }

    fn load(&self, id: &SkillId) -> SendBoxFuture<'_, Result<LoadedSkill, SkillError>> {
        Self::load(self, id)
    }
}

impl LocalSkillCatalog for StaticSkillCatalog {
    fn list(&self) -> LocalBoxFuture<'_, Result<Vec<SkillDescriptor>, SkillError>> {
        Box::pin(async move { Ok(self.descriptors_in_order()) })
    }

    fn load(&self, id: &SkillId) -> LocalBoxFuture<'_, Result<LoadedSkill, SkillError>> {
        let id = id.clone();
        Box::pin(async move {
            self.skills
                .get(&id)
                .cloned()
                .ok_or(SkillError::NotFound { id })
        })
    }
}

/// Skill catalog discovered from native filesystem directories.
///
/// Discovery follows pinned Pi's traversal contract: a directory-level
/// `SKILL.md` takes precedence over descendants, direct root markdown files
/// may declare skills, nested directories are recursive, symlink targets are
/// inspected without replacing their addressed paths, and `.gitignore`,
/// `.ignore`, and `.fdignore` rules accumulate from the root downward.
#[derive(Clone, Debug)]
pub struct NativeSkillCatalog {
    catalog: StaticSkillCatalog,
    diagnostics: Vec<SkillDiagnostic>,
}

impl NativeSkillCatalog {
    /// Discovers skills beneath the supplied directories.
    ///
    /// Missing roots are skipped. Other filesystem failures are retained as
    /// non-fatal diagnostics, matching Pi's loader.
    pub fn discover(
        directories: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> Result<Self, SkillError> {
        let mut skills = Vec::new();
        let mut diagnostics = Vec::new();

        for directory in directories {
            let directory = absolute_addressed_path(directory.as_ref()).map_err(|error| {
                SkillError::Catalog {
                    message: format!("could not resolve skill root: {error}"),
                }
            })?;
            let root_kind = match resolve_native_kind(&directory) {
                Ok(kind) => kind,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    diagnostics.push(filesystem_diagnostic(
                        SkillDiagnosticCode::FileInfoFailed,
                        &directory,
                        error,
                    ));
                    continue;
                }
            };
            if root_kind != NativeFileKind::Directory {
                continue;
            }

            let mut ignore_builder = GitignoreBuilder::new(&directory);
            discover_native_directory(
                &directory,
                &directory,
                true,
                &mut ignore_builder,
                &mut skills,
                &mut diagnostics,
            );
        }

        Ok(Self {
            catalog: StaticSkillCatalog::new(skills)?,
            diagnostics,
        })
    }

    /// Returns discovery diagnostics in deterministic traversal order.
    pub fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.diagnostics
    }

    /// Lists descriptors without trait-method disambiguation.
    pub fn list(&self) -> SendBoxFuture<'_, Result<Vec<SkillDescriptor>, SkillError>> {
        self.catalog.list()
    }

    /// Loads a discovered skill without trait-method disambiguation.
    pub fn load(&self, id: &SkillId) -> SendBoxFuture<'_, Result<LoadedSkill, SkillError>> {
        self.catalog.load(id)
    }
}

impl SkillCatalog for NativeSkillCatalog {
    fn list(&self) -> SendBoxFuture<'_, Result<Vec<SkillDescriptor>, SkillError>> {
        Self::list(self)
    }

    fn load(&self, id: &SkillId) -> SendBoxFuture<'_, Result<LoadedSkill, SkillError>> {
        Self::load(self, id)
    }
}

impl LocalSkillCatalog for NativeSkillCatalog {
    fn list(&self) -> LocalBoxFuture<'_, Result<Vec<SkillDescriptor>, SkillError>> {
        Box::pin(async move { Ok(self.catalog.descriptors_in_order()) })
    }

    fn load(&self, id: &SkillId) -> LocalBoxFuture<'_, Result<LoadedSkill, SkillError>> {
        let id = id.clone();
        Box::pin(async move {
            self.catalog
                .skills
                .get(&id)
                .cloned()
                .ok_or(SkillError::NotFound { id })
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeFileKind {
    File,
    Directory,
    Other,
}

#[derive(Clone, Debug)]
struct NativeDirectoryEntry {
    name: String,
    path: PathBuf,
}

#[allow(clippy::too_many_arguments)]
fn discover_native_directory(
    root: &Path,
    directory: &Path,
    include_root_files: bool,
    ignore_builder: &mut GitignoreBuilder,
    skills: &mut Vec<LoadedSkill>,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    let kind = match resolve_native_kind(directory) {
        Ok(kind) => kind,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            diagnostics.push(filesystem_diagnostic(
                SkillDiagnosticCode::FileInfoFailed,
                directory,
                error,
            ));
            return;
        }
    };
    if kind != NativeFileKind::Directory {
        return;
    }

    add_native_ignore_rules(root, directory, ignore_builder, diagnostics);

    let read_dir = match fs::read_dir(directory) {
        Ok(read_dir) => read_dir,
        Err(error) => {
            diagnostics.push(filesystem_diagnostic(
                SkillDiagnosticCode::ListFailed,
                directory,
                error,
            ));
            return;
        }
    };
    let mut entries = Vec::new();
    for entry in read_dir {
        match entry {
            Ok(entry) => entries.push(NativeDirectoryEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path(),
            }),
            Err(error) => diagnostics.push(filesystem_diagnostic(
                SkillDiagnosticCode::ListFailed,
                directory,
                error,
            )),
        }
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));

    for entry in &entries {
        if entry.name != "SKILL.md" {
            continue;
        }
        let kind = match resolve_entry_kind(entry, diagnostics) {
            Some(kind) => kind,
            None => continue,
        };
        if kind != NativeFileKind::File
            || native_path_is_ignored(root, &entry.path, false, ignore_builder)
        {
            continue;
        }
        load_native_skill_file(&entry.path, true, skills, diagnostics);
        return;
    }

    for entry in entries {
        if entry.name.starts_with('.') || entry.name == "node_modules" {
            continue;
        }
        let kind = match resolve_entry_kind(&entry, diagnostics) {
            Some(kind) => kind,
            None => continue,
        };
        if native_path_is_ignored(
            root,
            &entry.path,
            kind == NativeFileKind::Directory,
            ignore_builder,
        ) {
            continue;
        }

        match kind {
            NativeFileKind::Directory => discover_native_directory(
                root,
                &entry.path,
                false,
                ignore_builder,
                skills,
                diagnostics,
            ),
            NativeFileKind::File if include_root_files && entry.name.ends_with(".md") => {
                load_native_skill_file(&entry.path, false, skills, diagnostics);
            }
            NativeFileKind::File | NativeFileKind::Other => {}
        }
    }
}

fn resolve_entry_kind(
    entry: &NativeDirectoryEntry,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Option<NativeFileKind> {
    match resolve_native_kind(&entry.path) {
        Ok(kind) => Some(kind),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            diagnostics.push(filesystem_diagnostic(
                SkillDiagnosticCode::FileInfoFailed,
                &entry.path,
                error,
            ));
            None
        }
    }
}

fn resolve_native_kind(path: &Path) -> io::Result<NativeFileKind> {
    let metadata = fs::symlink_metadata(path)?;
    let effective = if metadata.file_type().is_symlink() {
        fs::metadata(fs::canonicalize(path)?)?
    } else {
        metadata
    };
    Ok(if effective.is_file() {
        NativeFileKind::File
    } else if effective.is_dir() {
        NativeFileKind::Directory
    } else {
        NativeFileKind::Other
    })
}

fn add_native_ignore_rules(
    root: &Path,
    directory: &Path,
    ignore_builder: &mut GitignoreBuilder,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    let relative_directory = directory.strip_prefix(root).unwrap_or(directory);
    let prefix = if relative_directory.as_os_str().is_empty() {
        String::new()
    } else {
        format!("{}/", path_for_ignore(relative_directory))
    };

    for file_name in IGNORE_FILE_NAMES {
        let ignore_path = directory.join(file_name);
        let metadata = match fs::symlink_metadata(&ignore_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                diagnostics.push(filesystem_diagnostic(
                    SkillDiagnosticCode::FileInfoFailed,
                    &ignore_path,
                    error,
                ));
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let content = match fs::read_to_string(&ignore_path) {
            Ok(content) => content,
            Err(error) => {
                diagnostics.push(filesystem_diagnostic(
                    SkillDiagnosticCode::ReadFailed,
                    &ignore_path,
                    error,
                ));
                continue;
            }
        };
        for line in content.lines() {
            let Some(pattern) = prefix_ignore_pattern(line, &prefix) else {
                continue;
            };
            if let Err(error) = ignore_builder.add_line(Some(ignore_path.clone()), &pattern) {
                diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::ParseFailed,
                    message: error.to_string(),
                    path: ignore_path.display().to_string(),
                });
            }
        }
    }
}

fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || (trimmed.starts_with('#') && !trimmed.starts_with("\\#")) {
        return None;
    }

    let (negated, mut pattern) = if let Some(pattern) = line.strip_prefix('!') {
        (true, pattern)
    } else if let Some(pattern) = line.strip_prefix("\\!") {
        (false, pattern)
    } else {
        (false, line)
    };
    if let Some(without_root) = pattern.strip_prefix('/') {
        pattern = without_root;
    }
    let prefixed = format!("{prefix}{pattern}");
    Some(if negated {
        format!("!{prefixed}")
    } else {
        prefixed
    })
}

fn native_path_is_ignored(
    root: &Path,
    path: &Path,
    is_directory: bool,
    ignore_builder: &GitignoreBuilder,
) -> bool {
    let Ok(matcher) = ignore_builder.build() else {
        return false;
    };
    let relative = path.strip_prefix(root).unwrap_or(path);
    matches!(
        matcher.matched(path_for_ignore(relative), is_directory),
        Match::Ignore(_)
    )
}

fn load_native_skill_file(
    path: &Path,
    declared_skill: bool,
    skills: &mut Vec<LoadedSkill>,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            diagnostics.push(filesystem_diagnostic(
                SkillDiagnosticCode::ReadFailed,
                path,
                error,
            ));
            return;
        }
    };
    let result = parse_skill_document(
        path.display().to_string(),
        &content,
        declared_skill,
        Vec::new(),
    );
    if let Some(skill) = result.skill {
        skills.push(skill);
    }
    diagnostics.extend(result.diagnostics);
}

fn absolute_addressed_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn path_for_ignore(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn filesystem_diagnostic(
    code: SkillDiagnosticCode,
    path: &Path,
    error: io::Error,
) -> SkillDiagnostic {
    SkillDiagnostic {
        code,
        message: error.to_string(),
        path: path.display().to_string(),
    }
}

/// Durable identity of a skill admitted into an operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecordedSkill {
    /// Persisted-record schema version.
    pub schema_version: u32,
    /// Admitted skill identity.
    pub id: SkillId,
    /// Admitted content digest.
    pub digest: ContentDigest,
}

impl RecordedSkill {
    /// Captures the identity and digest of a loaded skill.
    pub fn from_loaded(skill: &LoadedSkill) -> Self {
        Self {
            schema_version: RECORDED_SKILL_SCHEMA_VERSION,
            id: skill.descriptor.id.clone(),
            digest: skill.digest.clone(),
        }
    }
}

/// Policy for catalog changes observed while resuming durable work.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SkillResumePolicy {
    /// Reject changed content rather than silently changing admitted behavior.
    #[default]
    RequireRecordedDigest,
    /// Explicitly accept the catalog's current content.
    AllowChangedContent,
}

/// Loads a skill for a resumed Send operation and enforces its recorded digest.
pub fn load_skill_for_resume<'a>(
    catalog: &'a dyn SkillCatalog,
    recorded: &'a RecordedSkill,
    policy: SkillResumePolicy,
) -> SendBoxFuture<'a, Result<LoadedSkill, SkillError>> {
    Box::pin(async move {
        let skill = catalog.load(&recorded.id).await?;
        validate_resume_digest(skill, recorded, policy)
    })
}

/// Loads a skill for a resumed local operation and enforces its recorded digest.
pub fn load_local_skill_for_resume<'a>(
    catalog: &'a dyn LocalSkillCatalog,
    recorded: &'a RecordedSkill,
    policy: SkillResumePolicy,
) -> LocalBoxFuture<'a, Result<LoadedSkill, SkillError>> {
    Box::pin(async move {
        let skill = catalog.load(&recorded.id).await?;
        validate_resume_digest(skill, recorded, policy)
    })
}

/// Parses one Pi-compatible markdown skill declaration.
///
/// Declared `SKILL.md` files report missing metadata. Direct root markdown
/// documents without a description are treated as documentation and skipped.
pub fn parse_skill_document(
    file_path: impl Into<String>,
    content: &str,
    declared_skill: bool,
    resources: Vec<SkillResource>,
) -> SkillDocumentResult {
    let file_path = file_path.into();
    let parsed = match parse_frontmatter(content) {
        Ok(parsed) => parsed,
        Err(message) => {
            return SkillDocumentResult {
                skill: None,
                diagnostics: if declared_skill {
                    vec![SkillDiagnostic {
                        code: SkillDiagnosticCode::ParseFailed,
                        message,
                        path: file_path,
                    }]
                } else {
                    Vec::new()
                },
            };
        }
    };

    let description = parsed.frontmatter.description.unwrap_or_default();
    if !declared_skill && description.trim().is_empty() {
        return SkillDocumentResult {
            skill: None,
            diagnostics: Vec::new(),
        };
    }

    let parent_name = parent_directory_name(&file_path);
    let name = parsed
        .frontmatter
        .name
        .unwrap_or_else(|| parent_name.clone());
    let descriptor = SkillDescriptor {
        id: SkillId::new(name.clone()),
        name,
        description,
        location: file_path.clone(),
        disable_model_invocation: parsed.frontmatter.disable_model_invocation,
    };
    let diagnostics = validate_skill_descriptor_against_parent(&descriptor, &parent_name);
    if descriptor.description.trim().is_empty() {
        return SkillDocumentResult {
            skill: None,
            diagnostics,
        };
    }

    let prompt_fragments = vec![PromptFragment {
        name: None,
        content: parsed.body,
    }];
    let digest = digest_skill(&descriptor, &prompt_fragments, &resources);
    SkillDocumentResult {
        skill: Some(LoadedSkill {
            descriptor,
            prompt_fragments,
            resources,
            digest,
        }),
        diagnostics,
    }
}

/// Formats one explicit skill invocation using Pi's model-visible XML block.
pub fn format_skill_invocation(
    skill: &LoadedSkill,
    additional_instructions: Option<&str>,
) -> String {
    let location = &skill.descriptor.location;
    let parent = parent_path(location);
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.descriptor.name,
        location,
        parent,
        skill.prompt_text()
    );
    additional_instructions.map_or(block.clone(), |instructions| {
        format!("{block}\n\n{instructions}")
    })
}

#[derive(Debug, Default)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    disable_model_invocation: bool,
}

struct ParsedSkillDocument {
    frontmatter: SkillFrontmatter,
    body: String,
}

fn parse_frontmatter(content: &str) -> Result<ParsedSkillDocument, String> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return Ok(ParsedSkillDocument {
            frontmatter: SkillFrontmatter::default(),
            body: normalized,
        });
    }
    let Some(relative_end) = normalized[3..].find("\n---") else {
        return Ok(ParsedSkillDocument {
            frontmatter: SkillFrontmatter::default(),
            body: normalized,
        });
    };
    let end = relative_end + 3;
    let yaml_start = usize::min(4, end);
    let yaml = &normalized[yaml_start..end];
    let frontmatter = parse_open_skill_metadata(yaml)?;
    let body_start = usize::min(end + 4, normalized.len());
    Ok(ParsedSkillDocument {
        frontmatter,
        body: normalized[body_start..].trim().to_owned(),
    })
}

fn parse_open_skill_metadata(yaml: &str) -> Result<SkillFrontmatter, String> {
    if yaml.trim().is_empty() {
        return Ok(SkillFrontmatter::default());
    }
    let value: Value = serde_yaml::from_str(yaml).map_err(|error| error.to_string())?;
    let Some(mapping) = value.as_mapping() else {
        return Ok(SkillFrontmatter::default());
    };
    Ok(SkillFrontmatter {
        name: skill_mapping_string(mapping, "name"),
        description: skill_mapping_string(mapping, "description"),
        disable_model_invocation: matches!(
            mapping.get(Value::String("disable-model-invocation".to_owned())),
            Some(Value::Bool(true))
        ),
    })
}

fn skill_mapping_string(mapping: &Mapping, key: &str) -> Option<String> {
    mapping
        .get(Value::String(key.to_owned()))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn validate_skill_descriptor(descriptor: &SkillDescriptor) -> Vec<SkillDiagnostic> {
    validate_skill_descriptor_against_parent(
        descriptor,
        &parent_directory_name(&descriptor.location),
    )
}

fn validate_skill_descriptor_against_parent(
    descriptor: &SkillDescriptor,
    parent_name: &str,
) -> Vec<SkillDiagnostic> {
    let mut messages = Vec::new();
    if descriptor.name != parent_name {
        messages.push(format!(
            "name \"{}\" does not match parent directory \"{parent_name}\"",
            descriptor.name
        ));
    }
    let name_length = descriptor.name.encode_utf16().count();
    if name_length > MAX_NAME_LENGTH {
        messages.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name_length
        ));
    }
    if !descriptor
        .name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        messages.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".into(),
        );
    }
    if descriptor.name.starts_with('-') || descriptor.name.ends_with('-') {
        messages.push("name must not start or end with a hyphen".into());
    }
    if descriptor.name.contains("--") {
        messages.push("name must not contain consecutive hyphens".into());
    }
    if descriptor.description.trim().is_empty() {
        messages.push("description is required".into());
    } else if descriptor.description.encode_utf16().count() > MAX_DESCRIPTION_LENGTH {
        messages.push(format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
            descriptor.description.encode_utf16().count()
        ));
    }
    messages
        .into_iter()
        .map(|message| SkillDiagnostic {
            code: SkillDiagnosticCode::InvalidMetadata,
            message,
            path: descriptor.location.clone(),
        })
        .collect()
}

fn digest_skill(
    descriptor: &SkillDescriptor,
    prompt_fragments: &[PromptFragment],
    resources: &[SkillResource],
) -> ContentDigest {
    let mut fields = vec![
        descriptor.id.as_str().as_bytes().to_vec(),
        descriptor.name.as_bytes().to_vec(),
        descriptor.description.as_bytes().to_vec(),
        descriptor.location.as_bytes().to_vec(),
        vec![u8::from(descriptor.disable_model_invocation)],
    ];
    for fragment in prompt_fragments {
        fields.push(
            fragment
                .name
                .as_deref()
                .unwrap_or_default()
                .as_bytes()
                .to_vec(),
        );
        fields.push(fragment.content.as_bytes().to_vec());
    }
    let mut resources = resources.iter().collect::<Vec<_>>();
    resources.sort_by(|left, right| left.path.cmp(&right.path));
    for resource in resources {
        fields.push(resource.path.as_bytes().to_vec());
        fields.push(resource.data.clone());
    }
    ContentDigest::sha256("pi-agent-harness.skill.v1", fields)
}

fn update_digest_field(digest: &mut Sha256, field: &[u8]) {
    digest.update((field.len() as u64).to_be_bytes());
    digest.update(field);
}

fn validate_resume_digest(
    skill: LoadedSkill,
    recorded: &RecordedSkill,
    policy: SkillResumePolicy,
) -> Result<LoadedSkill, SkillError> {
    if skill.digest == recorded.digest || policy == SkillResumePolicy::AllowChangedContent {
        return Ok(skill);
    }
    Err(SkillError::DigestMismatch {
        id: recorded.id.clone(),
        recorded: recorded.digest.clone(),
        current: skill.digest,
    })
}

fn parent_directory_name(path: &str) -> String {
    let normalized = path.trim_end_matches(['/', '\\']);
    let mut components = normalized.rsplit(['/', '\\']);
    let _file = components.next();
    components.next().unwrap_or_default().to_owned()
}

fn parent_path(path: &str) -> String {
    let normalized = path.trim_end_matches(['/', '\\']);
    let slash = normalized.rfind('/');
    let backslash = normalized.rfind('\\');
    match slash.max(backslash) {
        Some(2) if normalized.as_bytes().get(1) == Some(&b':') => normalized[..3].to_owned(),
        Some(0) | None => Path::new("/").display().to_string(),
        Some(index) => normalized[..index].to_owned(),
    }
}
