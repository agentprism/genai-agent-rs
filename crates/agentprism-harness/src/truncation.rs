//! UTF-8- and logical-line-aware reference-tool output truncation.

use serde::{Deserialize, Serialize};

/// Pi-compatible default logical-line limit.
pub const DEFAULT_MAX_LINES: usize = 2_000;
/// Pi-compatible default UTF-8 byte limit.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1_024;

/// Which side of an oversized value is retained.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationStrategy {
    /// Retain complete lines from the beginning.
    #[default]
    Head,
    /// Retain complete lines from the end, allowing a suffix of one oversized final line.
    Tail,
    /// Divide both limits between the beginning and end.
    HeadAndTail,
}

/// Independent limits applied to reference-tool output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TruncationLimits {
    /// Maximum UTF-8 bytes retained.
    pub max_bytes: usize,
    /// Maximum logical lines retained.
    pub max_lines: usize,
    /// Retention side.
    pub strategy: TruncationStrategy,
}

impl Default for TruncationLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            strategy: TruncationStrategy::Head,
        }
    }
}

/// Limit responsible for truncating output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncatedBy {
    /// Logical-line bound was reached first.
    Lines,
    /// UTF-8 byte bound was reached first.
    Bytes,
}

/// Bounded output plus complete pre-truncation accounting.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TruncationResult {
    /// Retained valid UTF-8 output.
    pub content: String,
    /// Whether any source content was omitted.
    pub truncated: bool,
    /// First effective limit, absent when output was not truncated.
    pub truncated_by: Option<TruncatedBy>,
    /// Logical lines in the complete input; one trailing newline does not add a line.
    pub total_lines: u64,
    /// UTF-8 bytes in the complete input.
    pub total_bytes: u64,
    /// Complete logical lines represented by the retained output.
    pub output_lines: u64,
    /// UTF-8 bytes in the retained output.
    pub output_bytes: u64,
    /// Whether tail retention kept only a suffix of the final source line.
    pub last_line_partial: bool,
    /// Whether head retention omitted an oversized first line entirely.
    pub first_line_exceeds_limit: bool,
    /// Applied logical-line limit.
    pub max_lines: usize,
    /// Applied byte limit.
    pub max_bytes: usize,
}

/// Applies the strategy selected in `limits`.
pub fn truncate(content: &str, limits: TruncationLimits) -> TruncationResult {
    match limits.strategy {
        TruncationStrategy::Head => truncate_head(content, limits),
        TruncationStrategy::Tail => truncate_tail(content, limits),
        TruncationStrategy::HeadAndTail => truncate_head_and_tail(content, limits),
    }
}

/// Retains complete leading logical lines under both limits.
pub fn truncate_head(content: &str, limits: TruncationLimits) -> TruncationResult {
    let lines = logical_lines(content);
    let total_lines = lines.len() as u64;
    let total_bytes = content.len() as u64;
    if fits(total_lines, total_bytes, limits) {
        return complete_result(content, total_lines, total_bytes, limits);
    }

    if lines
        .first()
        .is_some_and(|line| line.len() > limits.max_bytes)
    {
        return truncated_result(
            String::new(),
            total_lines,
            total_bytes,
            0,
            Some(TruncatedBy::Bytes),
            false,
            true,
            limits,
        );
    }

    let mut retained = Vec::new();
    let mut retained_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    for (index, line) in lines.iter().enumerate().take(limits.max_lines) {
        let required = line.len() + usize::from(index > 0);
        if retained_bytes.saturating_add(required) > limits.max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        retained.push(*line);
        retained_bytes += required;
    }
    if retained.len() >= limits.max_lines && retained_bytes <= limits.max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    let output = retained.join("\n");
    truncated_result(
        output,
        total_lines,
        total_bytes,
        retained.len() as u64,
        Some(truncated_by),
        false,
        false,
        limits,
    )
}

/// Retains trailing logical lines under both limits.
pub fn truncate_tail(content: &str, limits: TruncationLimits) -> TruncationResult {
    let lines = logical_lines(content);
    let total_lines = lines.len() as u64;
    let total_bytes = content.len() as u64;
    if fits(total_lines, total_bytes, limits) {
        return complete_result(content, total_lines, total_bytes, limits);
    }

    let mut retained = Vec::new();
    let mut retained_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    for line in lines.iter().rev().take(limits.max_lines) {
        let required = line.len() + usize::from(!retained.is_empty());
        if retained_bytes.saturating_add(required) > limits.max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if retained.is_empty() {
                let suffix = utf8_suffix(line, limits.max_bytes);
                retained_bytes = suffix.len();
                retained.push(suffix);
                last_line_partial = true;
            }
            break;
        }
        retained.push((*line).to_owned());
        retained_bytes += required;
    }
    retained.reverse();
    if retained.len() >= limits.max_lines && retained_bytes <= limits.max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    let output = retained.join("\n");
    let output_lines = retained.len() as u64;
    truncated_result(
        output,
        total_lines,
        total_bytes,
        output_lines,
        Some(truncated_by),
        last_line_partial,
        false,
        limits,
    )
}

/// Formats a byte count using Pi's reference-tool display units.
pub fn format_size(bytes: u64) -> String {
    if bytes < 1_024 {
        format!("{bytes}B")
    } else if bytes < 1_024 * 1_024 {
        format!("{:.1}KB", bytes as f64 / 1_024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1_024.0 * 1_024.0))
    }
}

fn truncate_head_and_tail(content: &str, limits: TruncationLimits) -> TruncationResult {
    let lines = logical_lines(content);
    let total_lines = lines.len() as u64;
    let total_bytes = content.len() as u64;
    if fits(total_lines, total_bytes, limits) {
        return complete_result(content, total_lines, total_bytes, limits);
    }

    let head_lines = limits.max_lines.div_ceil(2);
    let tail_lines = limits.max_lines.saturating_sub(head_lines);
    let separator_bytes = usize::from(head_lines > 0 && tail_lines > 0 && limits.max_bytes > 0);
    let usable_bytes = limits.max_bytes.saturating_sub(separator_bytes);
    let head_bytes = usable_bytes.div_ceil(2);
    let tail_bytes = usable_bytes.saturating_sub(head_bytes);
    let head = truncate_head(
        content,
        TruncationLimits {
            max_bytes: head_bytes,
            max_lines: head_lines,
            strategy: TruncationStrategy::Head,
        },
    );
    let tail = truncate_tail(
        content,
        TruncationLimits {
            max_bytes: tail_bytes,
            max_lines: tail_lines,
            strategy: TruncationStrategy::Tail,
        },
    );
    let output = match (head.content.is_empty(), tail.content.is_empty()) {
        (false, false) => format!("{}\n{}", head.content, tail.content),
        (false, true) => head.content,
        (true, false) => tail.content,
        (true, true) => String::new(),
    };
    let output_lines = logical_lines(&output).len() as u64;
    let truncated_by = if total_bytes > limits.max_bytes as u64 {
        TruncatedBy::Bytes
    } else {
        TruncatedBy::Lines
    };
    truncated_result(
        output,
        total_lines,
        total_bytes,
        output_lines,
        Some(truncated_by),
        tail.last_line_partial,
        head.first_line_exceeds_limit,
        limits,
    )
}

fn logical_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines = content.split('\n').collect::<Vec<_>>();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn utf8_suffix(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_owned();
    }
    let minimum = line.len().saturating_sub(max_bytes);
    let start = line
        .char_indices()
        .map(|(index, _)| index)
        .find(|index| *index >= minimum)
        .unwrap_or(line.len());
    line[start..].to_owned()
}

fn fits(total_lines: u64, total_bytes: u64, limits: TruncationLimits) -> bool {
    total_lines <= limits.max_lines as u64 && total_bytes <= limits.max_bytes as u64
}

fn complete_result(
    content: &str,
    total_lines: u64,
    total_bytes: u64,
    limits: TruncationLimits,
) -> TruncationResult {
    TruncationResult {
        content: content.to_owned(),
        truncated: false,
        truncated_by: None,
        total_lines,
        total_bytes,
        output_lines: total_lines,
        output_bytes: total_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines: limits.max_lines,
        max_bytes: limits.max_bytes,
    }
}

#[allow(clippy::too_many_arguments)]
fn truncated_result(
    content: String,
    total_lines: u64,
    total_bytes: u64,
    output_lines: u64,
    truncated_by: Option<TruncatedBy>,
    last_line_partial: bool,
    first_line_exceeds_limit: bool,
    limits: TruncationLimits,
) -> TruncationResult {
    let output_bytes = content.len() as u64;
    TruncationResult {
        content,
        truncated: true,
        truncated_by,
        total_lines,
        total_bytes,
        output_lines,
        output_bytes,
        last_line_partial,
        first_line_exceeds_limit,
        max_lines: limits.max_lines,
        max_bytes: limits.max_bytes,
    }
}
