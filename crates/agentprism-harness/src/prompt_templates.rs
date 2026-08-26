//! Deterministic prompt-template registration and argument substitution.

use crate::ContentDigest;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use serde_yaml::{Mapping, Value as YamlValue};
use std::{
    collections::BTreeMap,
    fmt, fs,
    path::{Path, PathBuf},
};

/// Persisted recorded-template schema version.
pub const RECORDED_PROMPT_TEMPLATE_SCHEMA_VERSION: u32 = 1;

/// Arguments supplied to a prompt-template invocation.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TemplateArguments {
    /// One-based positional arguments used by template placeholders.
    pub positional: Vec<String>,
}

impl TemplateArguments {
    /// Creates positional arguments.
    pub fn new(arguments: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            positional: arguments.into_iter().map(Into::into).collect(),
        }
    }

    /// Parses Pi's simple single- and double-quoted command argument syntax.
    pub fn parse(command_line: &str) -> Self {
        let mut arguments = Vec::new();
        let mut current = String::new();
        let mut quote = None;
        for character in command_line.chars() {
            match quote {
                Some(active) if character == active => quote = None,
                Some(_) => current.push(character),
                None if character == '\'' || character == '"' => quote = Some(character),
                None if character == ' ' || character == '\t' => {
                    if !current.is_empty() {
                        arguments.push(std::mem::take(&mut current));
                    }
                }
                None => current.push(character),
            }
        }
        if !current.is_empty() {
            arguments.push(current);
        }
        Self {
            positional: arguments,
        }
    }
}

/// Missing-placeholder behavior for a prompt-template registry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MissingTemplateArgumentPolicy {
    /// Reject a positional or sliced placeholder whose first argument is absent.
    Reject,
    /// Match pinned Pi's helper by substituting an empty string.
    #[default]
    PiCompatibleEmpty,
}

/// Registered prompt template.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptTemplate {
    /// Stable lookup name.
    pub name: String,
    /// Optional command-list description.
    pub description: Option<String>,
    /// Optional model-visible argument hint.
    pub argument_hint: Option<String>,
    /// Exact template source after frontmatter removal.
    pub content: String,
    /// Digest of all template fields.
    pub digest: ContentDigest,
}

impl PromptTemplate {
    /// Creates a template and computes its stable digest.
    pub fn new(
        name: impl Into<String>,
        description: Option<String>,
        argument_hint: Option<String>,
        content: impl Into<String>,
    ) -> Result<Self, TemplateError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(TemplateError::InvalidTemplate {
                name,
                message: "template name must not be empty".into(),
            });
        }
        let content = content.into();
        let digest = digest_template(
            &name,
            description.as_deref(),
            argument_hint.as_deref(),
            &content,
        );
        Ok(Self {
            name,
            description,
            argument_hint,
            content,
            digest,
        })
    }

    /// Parses a markdown template using Pi-compatible frontmatter and fallback description rules.
    pub fn from_markdown(file_name: &str, source: &str) -> Result<Self, TemplateError> {
        let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
        let (frontmatter, content) =
            parse_template_frontmatter(&normalized).map_err(|message| {
                TemplateError::InvalidTemplate {
                    name: file_name.to_owned(),
                    message,
                }
            })?;
        let name = strip_markdown_extension(file_name);
        let first_line = content.lines().find(|line| !line.trim().is_empty());
        let description = match frontmatter.description {
            Some(description) if !description.is_empty() => Some(description),
            _ => first_line.map(fallback_description),
        };
        Self::new(name, description, frontmatter.argument_hint, content)
    }
}

/// Fully rendered prompt-template output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RenderedPrompt {
    /// Template lookup name.
    pub template_name: String,
    /// Digest of the exact template that was rendered.
    pub template_digest: ContentDigest,
    /// Deterministically substituted prompt text.
    pub content: String,
}

/// Prompt-template failure.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TemplateError {
    /// No template has the requested name.
    NotFound {
        /// Missing name.
        name: String,
    },
    /// More than one template has the same name.
    Duplicate {
        /// Duplicate name.
        name: String,
    },
    /// A required positional placeholder has no argument.
    MissingArgument {
        /// Exact placeholder from the source template.
        placeholder: String,
        /// One-based first required argument.
        index: usize,
    },
    /// Template metadata or frontmatter is invalid.
    InvalidTemplate {
        /// Template name or source filename.
        name: String,
        /// Sanitized validation diagnostic.
        message: String,
    },
}

impl fmt::Display for TemplateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { name } => write!(formatter, "prompt template {name} was not found"),
            Self::Duplicate { name } => {
                write!(
                    formatter,
                    "prompt template {name} is declared more than once"
                )
            }
            Self::MissingArgument { placeholder, index } => write!(
                formatter,
                "prompt template placeholder {placeholder} requires argument {index}"
            ),
            Self::InvalidTemplate { name, message } => {
                write!(formatter, "prompt template {name} is invalid: {message}")
            }
        }
    }
}

impl std::error::Error for TemplateError {}

/// Thread-safe prompt-template registry from Architecture v2 part 2 §7.9.
pub trait PromptTemplateRegistry: Send + Sync + 'static {
    /// Resolves and renders one named template.
    fn resolve(
        &self,
        name: &str,
        arguments: &TemplateArguments,
    ) -> Result<RenderedPrompt, TemplateError>;
}

/// Single-threaded counterpart of [`PromptTemplateRegistry`].
pub trait LocalPromptTemplateRegistry: 'static {
    /// Resolves and renders one named template.
    fn resolve(
        &self,
        name: &str,
        arguments: &TemplateArguments,
    ) -> Result<RenderedPrompt, TemplateError>;
}

/// Immutable deterministic prompt-template registry.
#[derive(Clone, Debug)]
pub struct StaticPromptTemplateRegistry {
    templates: BTreeMap<String, PromptTemplate>,
    missing_argument_policy: MissingTemplateArgumentPolicy,
}

/// One filesystem input and optional caller-defined provenance label.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptTemplateSource {
    /// Markdown file or non-recursively scanned directory.
    pub path: PathBuf,
    /// Opaque provenance retained on diagnostics.
    pub source: Option<JsonValue>,
}

impl PromptTemplateSource {
    /// Creates an unsourced filesystem input.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            source: None,
        }
    }

    /// Attaches an opaque provenance label.
    pub fn with_source(mut self, source: impl Into<JsonValue>) -> Self {
        self.source = Some(source.into());
        self
    }
}

/// Stable severity emitted by prompt-template discovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptTemplateDiagnosticSeverity {
    /// Pi reports discovery failures as non-fatal warnings.
    Warning,
}

/// Stable prompt-template discovery diagnostic code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptTemplateDiagnosticCode {
    /// Reading metadata or resolving a link target failed.
    FileInfoFailed,
    /// Listing a directory failed.
    ListFailed,
    /// Reading a template file failed.
    ReadFailed,
    /// Parsing template frontmatter failed.
    ParseFailed,
}

/// Non-fatal prompt-template discovery diagnostic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptTemplateDiagnostic {
    /// Diagnostic severity. Pinned Pi currently emits warnings only.
    #[serde(rename = "type")]
    pub severity: PromptTemplateDiagnosticSeverity,
    /// Stable machine-readable diagnostic code.
    pub code: PromptTemplateDiagnosticCode,
    /// Addressed file or directory that failed.
    pub path: PathBuf,
    /// Opaque provenance supplied for the input.
    pub source: Option<JsonValue>,
    /// Sanitized diagnostic message.
    pub message: String,
}

/// One discovered template paired with caller-defined provenance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourcedPromptTemplate {
    /// Parsed prompt template.
    pub prompt_template: PromptTemplate,
    /// Opaque provenance supplied for the input.
    pub source: Option<JsonValue>,
}

/// Immutable registry discovered from native markdown files.
#[derive(Clone, Debug)]
pub struct NativePromptTemplateRegistry {
    registry: StaticPromptTemplateRegistry,
    sourced_templates: Vec<SourcedPromptTemplate>,
    diagnostics: Vec<PromptTemplateDiagnostic>,
}

impl NativePromptTemplateRegistry {
    /// Loads explicit markdown files and direct markdown children of each
    /// directory, preserving caller source order and sorting within a directory.
    pub fn discover(
        paths: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> Result<Self, TemplateError> {
        Self::discover_sourced(
            paths
                .into_iter()
                .map(|path| PromptTemplateSource::new(path.as_ref())),
        )
    }

    /// Loads prompt templates while retaining caller provenance on warnings.
    pub fn discover_sourced(
        sources: impl IntoIterator<Item = PromptTemplateSource>,
    ) -> Result<Self, TemplateError> {
        let mut templates = Vec::new();
        let mut sourced_templates = Vec::new();
        let mut diagnostics = Vec::new();
        for source in sources {
            let metadata = match fs::metadata(&source.path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    diagnostics.push(template_diagnostic(
                        &source,
                        PromptTemplateDiagnosticCode::FileInfoFailed,
                        source.path.clone(),
                        error,
                    ));
                    continue;
                }
            };
            let files = if metadata.is_dir() {
                match direct_markdown_files(&source.path) {
                    Ok(files) => files,
                    Err(error) => {
                        diagnostics.push(template_diagnostic(
                            &source,
                            PromptTemplateDiagnosticCode::ListFailed,
                            source.path.clone(),
                            error,
                        ));
                        continue;
                    }
                }
            } else if metadata.is_file() && has_markdown_extension(&source.path) {
                vec![source.path.clone()]
            } else {
                continue;
            };

            for path in files {
                let contents = match fs::read_to_string(&path) {
                    Ok(contents) => contents,
                    Err(error) => {
                        diagnostics.push(template_diagnostic(
                            &source,
                            PromptTemplateDiagnosticCode::ReadFailed,
                            path,
                            error,
                        ));
                        continue;
                    }
                };
                let file_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match PromptTemplate::from_markdown(&file_name, &contents) {
                    Ok(template) => {
                        sourced_templates.push(SourcedPromptTemplate {
                            prompt_template: template.clone(),
                            source: source.source.clone(),
                        });
                        templates.push(template);
                    }
                    Err(error) => diagnostics.push(template_diagnostic(
                        &source,
                        PromptTemplateDiagnosticCode::ParseFailed,
                        path,
                        error,
                    )),
                }
            }
        }

        Ok(Self {
            registry: StaticPromptTemplateRegistry::new(templates)?,
            sourced_templates,
            diagnostics,
        })
    }

    /// Lists discovered templates in caller traversal order.
    pub fn templates(&self) -> Vec<PromptTemplate> {
        self.sourced_templates
            .iter()
            .map(|sourced| sourced.prompt_template.clone())
            .collect()
    }

    /// Returns every non-fatal discovery warning.
    pub fn diagnostics(&self) -> &[PromptTemplateDiagnostic] {
        &self.diagnostics
    }

    /// Returns discovered templates in traversal order with their provenance.
    pub fn sourced_templates(&self) -> &[SourcedPromptTemplate] {
        &self.sourced_templates
    }

    /// Resolves a discovered template.
    pub fn resolve(
        &self,
        name: &str,
        arguments: &TemplateArguments,
    ) -> Result<RenderedPrompt, TemplateError> {
        self.registry.resolve(name, arguments)
    }
}

impl PromptTemplateRegistry for NativePromptTemplateRegistry {
    fn resolve(
        &self,
        name: &str,
        arguments: &TemplateArguments,
    ) -> Result<RenderedPrompt, TemplateError> {
        Self::resolve(self, name, arguments)
    }
}

impl LocalPromptTemplateRegistry for NativePromptTemplateRegistry {
    fn resolve(
        &self,
        name: &str,
        arguments: &TemplateArguments,
    ) -> Result<RenderedPrompt, TemplateError> {
        Self::resolve(self, name, arguments)
    }
}

fn direct_markdown_files(directory: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.path();
        if has_markdown_extension(&path) && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn has_markdown_extension(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "md")
}

fn template_diagnostic(
    source: &PromptTemplateSource,
    code: PromptTemplateDiagnosticCode,
    path: PathBuf,
    error: impl fmt::Display,
) -> PromptTemplateDiagnostic {
    PromptTemplateDiagnostic {
        severity: PromptTemplateDiagnosticSeverity::Warning,
        code,
        path,
        source: source.source.clone(),
        message: error.to_string(),
    }
}

impl StaticPromptTemplateRegistry {
    /// Builds a Pi-compatible registry and rejects duplicate names.
    pub fn new(templates: impl IntoIterator<Item = PromptTemplate>) -> Result<Self, TemplateError> {
        Self::with_policy(templates, MissingTemplateArgumentPolicy::PiCompatibleEmpty)
    }

    /// Builds a registry with explicit missing-placeholder behavior.
    pub fn with_policy(
        templates: impl IntoIterator<Item = PromptTemplate>,
        missing_argument_policy: MissingTemplateArgumentPolicy,
    ) -> Result<Self, TemplateError> {
        let mut by_name = BTreeMap::new();
        for template in templates {
            let name = template.name.clone();
            if by_name.insert(name.clone(), template).is_some() {
                return Err(TemplateError::Duplicate { name });
            }
        }
        Ok(Self {
            templates: by_name,
            missing_argument_policy,
        })
    }

    /// Lists templates in stable name order.
    pub fn templates(&self) -> Vec<PromptTemplate> {
        self.templates.values().cloned().collect()
    }

    /// Resolves a template without requiring trait-method disambiguation.
    pub fn resolve(
        &self,
        name: &str,
        arguments: &TemplateArguments,
    ) -> Result<RenderedPrompt, TemplateError> {
        let template = self
            .templates
            .get(name)
            .ok_or_else(|| TemplateError::NotFound {
                name: name.to_owned(),
            })?;
        Ok(RenderedPrompt {
            template_name: template.name.clone(),
            template_digest: template.digest.clone(),
            content: substitute_template_arguments(
                &template.content,
                arguments,
                self.missing_argument_policy,
            )?,
        })
    }
}

impl PromptTemplateRegistry for StaticPromptTemplateRegistry {
    fn resolve(
        &self,
        name: &str,
        arguments: &TemplateArguments,
    ) -> Result<RenderedPrompt, TemplateError> {
        Self::resolve(self, name, arguments)
    }
}

impl LocalPromptTemplateRegistry for StaticPromptTemplateRegistry {
    fn resolve(
        &self,
        name: &str,
        arguments: &TemplateArguments,
    ) -> Result<RenderedPrompt, TemplateError> {
        Self::resolve(self, name, arguments)
    }
}

/// Durable identity of a prompt template admitted into an operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecordedPromptTemplate {
    /// Persisted-record schema version.
    pub schema_version: u32,
    /// Admitted template name.
    pub name: String,
    /// Admitted template digest.
    pub digest: ContentDigest,
}

impl RecordedPromptTemplate {
    /// Captures a registered template's identity.
    pub fn from_template(template: &PromptTemplate) -> Self {
        Self {
            schema_version: RECORDED_PROMPT_TEMPLATE_SCHEMA_VERSION,
            name: template.name.clone(),
            digest: template.digest.clone(),
        }
    }
}

/// Applies `$N`, `$@`, `$ARGUMENTS`, `${@:N}`, and `${@:N:L}` substitutions.
pub fn substitute_template_arguments(
    content: &str,
    arguments: &TemplateArguments,
    missing_policy: MissingTemplateArgumentPolicy,
) -> Result<String, TemplateError> {
    let result = replace_numeric_placeholders(content, arguments, missing_policy)?;
    let result = replace_range_placeholders(&result, arguments, missing_policy)?;
    let all_arguments = arguments.positional.join(" ");
    Ok(result
        .replace("$ARGUMENTS", &all_arguments)
        .replace("$@", &all_arguments))
}

fn replace_numeric_placeholders(
    content: &str,
    arguments: &TemplateArguments,
    missing_policy: MissingTemplateArgumentPolicy,
) -> Result<String, TemplateError> {
    let bytes = content.as_bytes();
    let mut output = String::with_capacity(content.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        let Some(relative_dollar) = bytes[cursor..].iter().position(|byte| *byte == b'$') else {
            output.push_str(&content[cursor..]);
            break;
        };
        let dollar = cursor + relative_dollar;
        output.push_str(&content[cursor..dollar]);

        let digit_start = dollar + 1;
        let mut digit_end = digit_start;
        while digit_end < bytes.len() && bytes[digit_end].is_ascii_digit() {
            digit_end += 1;
        }
        if digit_end > digit_start {
            let placeholder = &content[dollar..digit_end];
            let index = content[digit_start..digit_end]
                .parse::<usize>()
                .unwrap_or(usize::MAX);
            if index == 0 || index > arguments.positional.len() {
                handle_missing_argument(missing_policy, placeholder, index)?;
            } else {
                output.push_str(&arguments.positional[index - 1]);
            }
            cursor = digit_end;
            continue;
        }

        output.push('$');
        cursor = dollar + 1;
    }
    Ok(output)
}

fn replace_range_placeholders(
    content: &str,
    arguments: &TemplateArguments,
    missing_policy: MissingTemplateArgumentPolicy,
) -> Result<String, TemplateError> {
    let mut output = String::with_capacity(content.len());
    let mut cursor = 0;
    while cursor < content.len() {
        let Some(relative_start) = content[cursor..].find("${@:") else {
            output.push_str(&content[cursor..]);
            break;
        };
        let start_offset = cursor + relative_start;
        output.push_str(&content[cursor..start_offset]);
        let Some(close_relative) = content[start_offset..].find('}') else {
            output.push_str(&content[start_offset..]);
            break;
        };
        let close = start_offset + close_relative;
        let placeholder = &content[start_offset..=close];
        let range = &content[start_offset + 4..close];
        let Some((start, length)) = parse_range(range) else {
            output.push_str("${@:");
            cursor = start_offset + 4;
            continue;
        };
        // Pi subtracts one and then clamps a negative start to zero, so both
        // `${@:0}` and `${@:1}` begin at the first argument.
        let start_index = start.saturating_sub(1);
        if start_index >= arguments.positional.len() && length != Some(0) {
            handle_missing_argument(missing_policy, placeholder, start)?;
        } else {
            let end = length.map_or(arguments.positional.len(), |length| {
                start_index
                    .saturating_add(length)
                    .min(arguments.positional.len())
            });
            output.push_str(&arguments.positional[start_index..end].join(" "));
        }
        cursor = close + 1;
    }
    Ok(output)
}

#[derive(Default)]
struct PromptTemplateFrontmatter {
    description: Option<String>,
    argument_hint: Option<String>,
}

fn parse_template_frontmatter(
    normalized: &str,
) -> Result<(PromptTemplateFrontmatter, String), String> {
    if !normalized.starts_with("---") {
        return Ok((PromptTemplateFrontmatter::default(), normalized.to_owned()));
    }
    let Some(relative_end) = normalized[3..].find("\n---") else {
        return Ok((PromptTemplateFrontmatter::default(), normalized.to_owned()));
    };
    let end = relative_end + 3;
    let start = usize::min(4, end);
    let yaml = &normalized[start..end];
    let frontmatter = parse_open_template_metadata(yaml)?;
    let body_start = usize::min(end + 4, normalized.len());
    Ok((frontmatter, normalized[body_start..].trim().to_owned()))
}

fn parse_open_template_metadata(yaml: &str) -> Result<PromptTemplateFrontmatter, String> {
    if yaml.trim().is_empty() {
        return Ok(PromptTemplateFrontmatter::default());
    }
    let value: YamlValue = serde_yaml::from_str(yaml).map_err(|error| error.to_string())?;
    let Some(mapping) = value.as_mapping() else {
        return Ok(PromptTemplateFrontmatter::default());
    };
    Ok(PromptTemplateFrontmatter {
        description: mapping_string(mapping, "description"),
        argument_hint: mapping_string(mapping, "argument-hint"),
    })
}

fn mapping_string(mapping: &Mapping, key: &str) -> Option<String> {
    mapping
        .get(YamlValue::String(key.to_owned()))
        .and_then(YamlValue::as_str)
        .map(str::to_owned)
}

fn strip_markdown_extension(file_name: &str) -> String {
    if file_name
        .get(file_name.len().saturating_sub(3)..)
        .is_some_and(|extension| extension.eq_ignore_ascii_case(".md"))
    {
        file_name[..file_name.len() - 3].to_owned()
    } else {
        file_name.to_owned()
    }
}

fn fallback_description(first_line: &str) -> String {
    const MAX_UTF16_CODE_UNITS: usize = 60;

    let total_code_units = first_line.encode_utf16().count();
    if total_code_units <= MAX_UTF16_CODE_UNITS {
        return first_line.to_owned();
    }

    // JavaScript's String.length and slice bounds are UTF-16 code units. Rust
    // strings cannot contain a lone surrogate, so retain only complete Unicode
    // scalar values when the 60-unit boundary would bisect a surrogate pair.
    let mut description = String::new();
    let mut retained_code_units = 0;
    for character in first_line.chars() {
        let character_code_units = character.len_utf16();
        if retained_code_units + character_code_units > MAX_UTF16_CODE_UNITS {
            break;
        }
        description.push(character);
        retained_code_units += character_code_units;
    }
    description.push_str("...");
    description
}

fn parse_range(range: &str) -> Option<(usize, Option<usize>)> {
    let mut components = range.split(':');
    let start = parse_decimal_saturating(components.next()?)?;
    let length = match components.next() {
        Some(length) => Some(parse_decimal_saturating(length)?),
        None => None,
    };
    if components.next().is_some() {
        return None;
    }
    Some((start, length))
}

fn parse_decimal_saturating(value: &str) -> Option<usize> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(value.bytes().fold(0_usize, |parsed, byte| {
        parsed
            .saturating_mul(10)
            .saturating_add(usize::from(byte - b'0'))
    }))
}

fn handle_missing_argument(
    policy: MissingTemplateArgumentPolicy,
    placeholder: &str,
    index: usize,
) -> Result<(), TemplateError> {
    match policy {
        MissingTemplateArgumentPolicy::Reject => Err(TemplateError::MissingArgument {
            placeholder: placeholder.to_owned(),
            index,
        }),
        MissingTemplateArgumentPolicy::PiCompatibleEmpty => Ok(()),
    }
}

fn digest_template(
    name: &str,
    description: Option<&str>,
    argument_hint: Option<&str>,
    content: &str,
) -> ContentDigest {
    ContentDigest::sha256(
        "pi-agent-harness.prompt-template.v1",
        [
            name.as_bytes(),
            description.unwrap_or_default().as_bytes(),
            argument_hint.unwrap_or_default().as_bytes(),
            content.as_bytes(),
        ],
    )
}
