#![allow(dead_code)]

//! Block rendering for tool calls and other transcript elements.
//! Pure functions over protocol payloads; the GUI maps the resulting
//! rho-local styles to the active Zed theme.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use tau_proto::{CborValue, ToolUsePayload, ToolUseState, ToolUseStatus, cbor_field};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum BlockStyle {
    #[default]
    Default,
    TokenStatsDelta,
    TokenStatsCacheHit,
    TokenStatsCacheWarn,
    TokenStatsCacheMiss,
    TokenStatsUp,
    TokenStatsDown,
    TokenStatsInput,
    TokenStatsOutput,
    TokenStatsLatency,
    TokenStatsSigma,
    ToolOutput,
    ToolName,
    ToolMode,
    ToolArgs,
    ToolStatusSuccess,
    ToolStatusInfo,
    ToolStatusError,
    ToolStatusTime,
    Progress,
    DiffAdded,
    DiffRemoved,
    DiffAddedInline,
    DiffRemovedInline,
    DiffContext,
    DiffHunkHeader,
    StatusRole,
    StatusContext,
    StatusTools,
    ActionOutput,
    ActionLabel,
    ActionValue,
    ActionId,
    ActionError,
    SystemInfo,
    SystemImportant,
    SystemPath,
    SystemStatus,
    ExtensionLifecycle,
    ExtensionStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StyledSpan {
    pub(crate) text: String,
    pub(crate) style: BlockStyle,
}

impl StyledSpan {
    fn new(text: impl Into<String>, style: BlockStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StyledBlock {
    spans: Vec<StyledSpan>,
}

impl StyledBlock {
    fn new(spans: Vec<StyledSpan>) -> Self {
        Self { spans }
    }

    pub(crate) fn spans(&self) -> &[StyledSpan] {
        &self.spans
    }
}

#[cfg(test)]
pub(crate) fn format_turn_stats_line(
    usage: &tau_proto::ProviderTokenUsage,
    previous_usage: Option<&tau_proto::ProviderTokenUsage>,
    turn_latency: Option<Duration>,
    total_latency: Option<Duration>,
) -> String {
    turn_stats_parts(usage, previous_usage, turn_latency, total_latency)
        .into_iter()
        .map(|part| part.text)
        .collect()
}

pub(crate) fn render_turn_stats_block(
    usage: &tau_proto::ProviderTokenUsage,
    previous_usage: Option<&tau_proto::ProviderTokenUsage>,
    turn_latency: Option<Duration>,
    total_latency: Option<Duration>,
) -> StyledBlock {
    StyledBlock::new(
        turn_stats_parts(usage, previous_usage, turn_latency, total_latency)
            .into_iter()
            .map(|part| StyledSpan::new(part.text, part.style))
            .collect(),
    )
}

const CACHE_HIT_WARNING_PERCENT: u8 = 90;
// Prompt-cache hits only accrue in coarse provider cache blocks; allow
// the last partial block to miss without flagging the turn.
const CACHE_GRANULARITY_TOKENS: u64 = 512;

struct TurnStatsPart {
    text: String,
    style: BlockStyle,
}

impl TurnStatsPart {
    fn new(text: impl Into<String>, style: BlockStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

fn turn_stats_parts(
    usage: &tau_proto::ProviderTokenUsage,
    previous_usage: Option<&tau_proto::ProviderTokenUsage>,
    turn_latency: Option<Duration>,
    total_latency: Option<Duration>,
) -> Vec<TurnStatsPart> {
    let previous_sent_tokens = previous_usage.map_or(0, |usage| usage.prompt_sent_tokens);
    let previous_received_tokens = previous_usage.map_or(0, |usage| usage.response_received_tokens);
    let turn_cache_possible = previous_sent_tokens.saturating_add(previous_received_tokens);
    let new_prompt_tokens = usage.prompt_sent_tokens.saturating_sub(turn_cache_possible);
    let mut parts = Vec::new();

    parts.push(TurnStatsPart::new("Δ", BlockStyle::TokenStatsDelta));
    let turn_cache_hit_percent =
        cache_hit_percent(Some(turn_cache_possible), Some(usage.prompt_cached_tokens)).unwrap_or(0);
    parts.push(TurnStatsPart::new(
        format!(
            "{turn_cache_hit_percent}% {}/{}",
            format_token_count(usage.prompt_cached_tokens),
            format_token_count(turn_cache_possible),
        ),
        cache_hit_style_name(turn_cache_possible, usage.prompt_cached_tokens),
    ));
    parts.push(TurnStatsPart::new(" ↑", BlockStyle::TokenStatsUp));
    parts.push(TurnStatsPart::new(
        format_token_count(new_prompt_tokens),
        BlockStyle::TokenStatsInput,
    ));
    parts.push(TurnStatsPart::new(" ↓", BlockStyle::TokenStatsDown));
    parts.push(TurnStatsPart::new(
        format_token_count(usage.response_received_tokens),
        BlockStyle::TokenStatsOutput,
    ));
    if let Some(latency) = turn_latency {
        parts.push(TurnStatsPart::new(
            format!(" {}", StatusBarDuration(latency)),
            BlockStyle::TokenStatsLatency,
        ));
    }

    parts.push(TurnStatsPart::new(" Σ", BlockStyle::TokenStatsSigma));
    parts.push(TurnStatsPart::new(" ↑", BlockStyle::TokenStatsUp));
    parts.push(TurnStatsPart::new(
        format!(
            "{}/{}",
            format_token_count(usage.stats.total.cached_tokens),
            format_token_count(usage.stats.total.sent_tokens),
        ),
        BlockStyle::TokenStatsInput,
    ));
    parts.push(TurnStatsPart::new(" ↓", BlockStyle::TokenStatsDown));
    parts.push(TurnStatsPart::new(
        format_token_count(usage.stats.total.received_tokens),
        BlockStyle::TokenStatsOutput,
    ));
    if let Some(latency) = total_latency {
        parts.push(TurnStatsPart::new(
            format!(" {}", StatusBarDuration(latency)),
            BlockStyle::TokenStatsLatency,
        ));
    }

    parts
}

struct StatusBarDuration(Duration);

impl fmt::Display for StatusBarDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const MILLIS_MAX: Duration = Duration::from_secs(5);
        const SECONDS_MAX: Duration = Duration::from_secs(5 * 60);

        if self.0 < MILLIS_MAX {
            write!(f, "{}ms", self.0.as_millis())
        } else if self.0 < SECONDS_MAX {
            write!(f, "{}s", self.0.as_secs())
        } else {
            write!(f, "{}m", self.0.as_secs() / 60)
        }
    }
}

fn cache_hit_style_name(possible_cached_tokens: u64, cached_tokens: u64) -> BlockStyle {
    let cacheable_prefix_floor =
        possible_cached_tokens / CACHE_GRANULARITY_TOKENS * CACHE_GRANULARITY_TOKENS;
    if cacheable_prefix_floor <= cached_tokens {
        BlockStyle::TokenStatsCacheHit
    } else if cache_hit_percent(Some(possible_cached_tokens), Some(cached_tokens))
        .is_some_and(|percent| CACHE_HIT_WARNING_PERCENT < percent)
    {
        BlockStyle::TokenStatsCacheWarn
    } else {
        BlockStyle::TokenStatsCacheMiss
    }
}

pub(crate) fn cache_hit_percent(
    possible_cached_tokens: Option<u64>,
    cached_tokens: Option<u64>,
) -> Option<u8> {
    let possible_cached_tokens = possible_cached_tokens?;
    let cached_tokens = cached_tokens?;
    if possible_cached_tokens == 0 {
        return Some(0);
    }
    let clamped_cached_tokens = cached_tokens.min(possible_cached_tokens);
    let percent = clamped_cached_tokens.saturating_mul(100) / possible_cached_tokens;
    Some(percent.min(100) as u8)
}

/// Build the iTerm2 OSC 1337 `SetUserVar` escape sequence for the
/// given (name, value) pair, with `value` base64-encoded.
///
/// When `in_tmux` is true the sequence is wrapped in
/// `\x1bPtmux;...\x1b\\` and the inner ESC is doubled so tmux passes
/// the OSC through to the outer terminal instead of consuming it.
/// Mirrors the shape used by the `user-notification.sh` reference
/// script. Caller is responsible for detecting tmux (typically by
/// checking `$TMUX`).
pub(crate) fn build_osc1337_set_user_var(name: &str, value: &str, in_tmux: bool) -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    let encoded = STANDARD.encode(value.as_bytes());
    if in_tmux {
        format!("\x1bPtmux;\x1b\x1b]1337;SetUserVar={name}={encoded}\x07\x1b\\")
    } else {
        format!("\x1b]1337;SetUserVar={name}={encoded}\x07")
    }
}

pub(crate) fn format_token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        let whole = tokens / 1_000;
        let tenth = (tokens % 1_000) / 100;
        if tenth == 0 {
            return format!("{whole}k");
        }
        return format!("{whole}.{tenth}k");
    }
    let whole = tokens / 1_000_000;
    let tenth = (tokens % 1_000_000) / 100_000;
    if tenth == 0 {
        return format!("{whole}m");
    }
    format!("{whole}.{tenth}m")
}

/// Format the `+N/-M` chip from a `DiffSummary` sub-tree on a tool
/// result as themed suffix segments. `+N` is painted with the
/// diff-added style and `-M` with the diff-removed style, matching
/// `git diff --shortstat`. The parens and slash stay in the muted info
/// Decode a `DiffSummary` sub-tree from a tool result, if present and
/// non-empty. Round-trips the CBOR sub-value through ciborium.
pub(crate) fn extract_diff(details: &CborValue) -> Option<tau_proto::DiffSummary> {
    let diff = cbor_field(details, "diff")?;
    let mut buf = Vec::new();
    ciborium::ser::into_writer(diff, &mut buf).ok()?;
    let summary: tau_proto::DiffSummary = ciborium::de::from_reader(buf.as_slice()).ok()?;
    if summary.added == 0 && summary.removed == 0 {
        return None;
    }
    Some(summary)
}

/// Which status-suffix style the completion block should use.
#[derive(Clone, Copy)]
pub(crate) enum ToolStatus {
    Success,
    Warning,
    Error,
    Pending,
    Info,
    Progress,
    DiffAdded,
    DiffRemoved,
    /// Agent id or legacy role suffix, painted like the status-bar role chip.
    Role,
    Context,
    Tools,
    Time,
}

/// Status variant for completed compaction lines. Kept separate from
/// tool-call display state because compaction is not a model-visible tool
/// invocation.
#[derive(Clone, Copy)]
pub(crate) enum CompactionStatus {
    Success,
    Progress,
}

#[derive(Clone)]
pub(crate) struct ToolSuffixSegment {
    pub(crate) text: String,
    pub(crate) status: ToolStatus,
    /// When true, suppress the implicit space the renderer normally
    /// inserts before this segment. Used to glue parts of a multi-span
    /// chip (e.g. the colored `+N/-M` diff stat) into one continuous
    /// run.
    pub(crate) no_leading_space: bool,
}

/// Decomposed tool-call label, painted as themed spans:
/// `<tool_name> <mode> <args> <range> <suffix...>`.
#[derive(Clone)]
pub(crate) struct ToolCallDisplay {
    pub(crate) tool_name: String,
    pub(crate) mode: String,
    pub(crate) args: String,
    pub(crate) range: Option<String>,
    pub(crate) suffixes: Vec<ToolSuffixSegment>,
    pub(crate) payload: Option<ToolUsePayload>,
}
#[derive(Clone, Debug, Default)]
pub(crate) struct ToolSummaryDisplay {
    pub(crate) total: u64,
    pub(crate) completed: u64,
    pub(crate) ok: u64,
    pub(crate) err: u64,
    pub(crate) matches: u64,
    pub(crate) lines: u64,
    pub(crate) bytes: u64,
    pub(crate) added: u64,
    pub(crate) removed: u64,
}

/// Build the completion descriptor for a finished `delegate` call by
/// carrying the cached progress (args + counters from the latest
/// [`tau_proto::DelegateProgress`]) and replacing the trailing
/// in-progress chip with output stats + the final `ok`/`err: message`
/// status. The input stats stay as a marked chip so delegate rendering
/// can show input first, then output, then progress counters.
pub(crate) fn build_delegate_completion_display(
    cached: Option<&ToolUseState>,
    details: &CborValue,
    error: Option<&str>,
) -> ToolUseState {
    let response_text = delegate_response_text(details);
    let mut display = cached.cloned().unwrap_or_else(|| ToolUseState {
        args: String::new(),
        ..Default::default()
    });
    let input_stats = display.stats;
    display.stats = tau_proto::ToolUseStats::for_text(response_text);
    if !input_stats.is_empty() {
        display
            .info_chips
            .push(format!("↘︎{}", format_tool_use_state_stats(&input_stats)));
    }
    match error {
        Some(msg) if !msg.is_empty() => {
            display.status = ToolUseStatus::Error;
            display.status_text = first_error_line(msg);
        }
        _ => {
            display.status = ToolUseStatus::Success;
            display.status_text = "ok".to_owned();
        }
    }
    display
}

fn delegate_response_text(details: &CborValue) -> &str {
    match details {
        CborValue::Text(text) => text.as_str(),
        CborValue::Map(entries) => entries
            .iter()
            .find_map(|(key, value)| match (key, value) {
                (CborValue::Text(key), CborValue::Text(text)) if key == "output" => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .unwrap_or_default(),
        _ => "",
    }
}

fn tool_suffix(text: String, status: ToolStatus) -> ToolSuffixSegment {
    ToolSuffixSegment {
        text,
        status,
        no_leading_space: false,
    }
}

pub(crate) fn pending_tool_call_display(tool_name: &str) -> ToolCallDisplay {
    ToolCallDisplay {
        tool_name: tool_name.to_owned(),
        mode: String::new(),
        args: String::new(),
        range: None,
        suffixes: vec![tool_suffix("pending".to_owned(), ToolStatus::Pending)],
        payload: None,
    }
}
fn info_suffix(text: String) -> ToolSuffixSegment {
    tool_suffix(text, ToolStatus::Info)
}

/// Build a streaming block whose body uses `body_name` styling and
/// whose trailing `…` indicator uses [`names::PROGRESS_INDICATOR`], so
/// the indicator can be themed independently. The leading space before
/// the indicator is skipped when the body is empty or already ends in
/// whitespace, so the `…` doesn't double up whitespace or land one
/// column off the left margin on a fresh line.
pub(crate) fn streaming_block(body_style: BlockStyle, body_text: impl Into<String>) -> StyledBlock {
    let body_text = body_text.into();
    let needs_space = body_text
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace());

    let mut spans = Vec::with_capacity(3);
    if !body_text.is_empty() {
        spans.push(StyledSpan::new(body_text, body_style));
    }
    if needs_space {
        spans.push(StyledSpan::new(" ", body_style));
    }
    spans.push(StyledSpan::new(
        tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        BlockStyle::Progress,
    ));

    StyledBlock::new(spans)
}

pub(crate) fn tool_duration_suffix(duration: Duration) -> ToolSuffixSegment {
    tool_suffix(format_tool_duration(duration), ToolStatus::Time)
}

pub(crate) fn format_tool_duration(duration: Duration) -> String {
    format!("{}s", duration.as_secs())
}

fn output_stats_suffix(text: &str) -> ToolSuffixSegment {
    info_suffix(format_stats(
        None,
        Some(text.lines().count() as u64),
        Some(text.len() as u64),
    ))
}

fn abbreviate_inline_text(text: &str) -> String {
    const EDGE_CHARS: usize = 20;

    let one_line = text.lines().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = one_line.chars().collect();
    if chars.len() <= EDGE_CHARS * 2 {
        return one_line;
    }

    let head: String = chars.iter().take(EDGE_CHARS).copied().collect();
    let tail: String = chars
        .iter()
        .skip(chars.len() - EDGE_CHARS)
        .copied()
        .collect();
    format!("{head}┄{tail}")
}

/// Render a `delegate` display with a dedicated suffix for the delegated agent.
/// Completed delegates show input stats (`↘︎`) before output stats (`↖︎`), then
/// progress counters and the final status. Cached descriptors may still have
/// ` +role` embedded in `args`; strip that legacy copy so the line does not
/// render both the old role chip and the new agent chip.
pub(crate) fn render_delegate_display(
    display: &ToolUseState,
    agent_id: Option<&str>,
    legacy_role: Option<&str>,
) -> ToolCallDisplay {
    let mut rendered = render_tool_use_state("agent_start", display);
    let stats_chip = format_tool_use_state_stats(&display.stats);
    if !stats_chip.is_empty() {
        let marker = match display.status {
            ToolUseStatus::InProgress => "↘︎",
            ToolUseStatus::Success | ToolUseStatus::Warning | ToolUseStatus::Error => "↖︎",
        };
        if let Some(suffix) = rendered
            .suffixes
            .iter_mut()
            .find(|suffix| suffix.text == stats_chip)
        {
            suffix.text = format!("{marker}{}", suffix.text);
        }
    }
    for suffix in &mut rendered.suffixes {
        normalize_delegate_input_stats_suffix(suffix);
    }
    if !matches!(display.status, ToolUseStatus::InProgress) {
        move_delegate_completion_stats_first(&mut rendered.suffixes, &stats_chip);
    }

    if let Some(role) = legacy_role.filter(|role| !role.is_empty()) {
        let legacy_suffix = format!(" +{role}");
        if let Some(args) = rendered.args.strip_suffix(&legacy_suffix) {
            rendered.args = args.to_owned();
        }
    }

    if let Some(agent_id) = agent_id.filter(|agent_id| !agent_id.is_empty()) {
        rendered
            .suffixes
            .insert(0, tool_suffix(format!("@{agent_id}"), ToolStatus::Role));
    } else if let Some(role) = legacy_role.filter(|role| !role.is_empty()) {
        rendered
            .suffixes
            .insert(0, tool_suffix(format!("+{role}"), ToolStatus::Role));
    }
    rendered
}

fn normalize_delegate_input_stats_suffix(suffix: &mut ToolSuffixSegment) {
    if !matches!(suffix.status, ToolStatus::Info) {
        return;
    }
    let normalized = suffix
        .text
        .strip_prefix("↘︎ ")
        .or_else(|| suffix.text.strip_prefix("↘︎"))
        .filter(|stats| !stats.is_empty())
        .map(|stats| format!("↘︎{stats}"));
    if let Some(normalized) = normalized {
        suffix.text = normalized;
    }
}

fn is_delegate_input_stats_suffix(suffix: &ToolSuffixSegment) -> bool {
    matches!(suffix.status, ToolStatus::Info)
        && suffix.text.starts_with("↘︎")
        && suffix.text.len() > "↘︎".len()
}

fn move_delegate_completion_stats_first(
    suffixes: &mut Vec<ToolSuffixSegment>,
    output_stats_chip: &str,
) {
    let mut input_stats = Vec::new();
    let mut rest = Vec::with_capacity(suffixes.len());
    for suffix in suffixes.drain(..) {
        if is_delegate_input_stats_suffix(&suffix) {
            input_stats.push(suffix);
        } else {
            rest.push(suffix);
        }
    }
    if input_stats.is_empty() {
        *suffixes = rest;
        return;
    }

    let output_stats_text =
        (!output_stats_chip.is_empty()).then(|| format!("↖︎{output_stats_chip}"));
    let insert_at = rest
        .iter()
        .position(|suffix| {
            output_stats_text
                .as_deref()
                .is_some_and(|text| suffix.text == text)
                || matches!(
                    suffix.status,
                    ToolStatus::Tools
                        | ToolStatus::Context
                        | ToolStatus::Success
                        | ToolStatus::Warning
                        | ToolStatus::Error
                        | ToolStatus::Progress
                )
        })
        .unwrap_or(rest.len());
    rest.splice(insert_at..insert_at, input_stats);
    *suffixes = rest;
}

/// Render a [`ToolUseState`] descriptor directly to a
/// [`ToolCallDisplay`]. The generic path the renderer takes when the
/// tool side attached a display descriptor to its result/error event —
/// no `match tool_name` arms needed. Falls back to
/// [`format_tool_completion`] for older events that didn't carry a
/// descriptor.
pub(crate) fn render_tool_use_state(tool_name: &str, display: &ToolUseState) -> ToolCallDisplay {
    let mut suffixes: Vec<ToolSuffixSegment> = Vec::new();
    // Diff `+N -M` chips (themed green/red) are derived from the
    // payload so file-editing tools don't have to push them as info chips.
    if let Some(ToolUsePayload::Diff(summary)) = &display.payload
        && (summary.added > 0 || summary.removed > 0)
    {
        if summary.added > 0 {
            suffixes.push(tool_suffix(
                format!("+{}", summary.added),
                ToolStatus::DiffAdded,
            ));
        }
        if summary.removed > 0 {
            suffixes.push(ToolSuffixSegment {
                text: format!("-{}", summary.removed),
                status: ToolStatus::DiffRemoved,
                no_leading_space: summary.added > 0,
            });
        }
    }
    let stats_chip = format_tool_use_state_stats(&display.stats);
    if !stats_chip.is_empty() {
        suffixes.push(info_suffix(stats_chip));
    }
    for counter in &display.progress_counters {
        suffixes.push(format_progress_counter(counter));
    }
    for chip in &display.info_chips {
        suffixes.push(info_suffix(chip.clone()));
    }
    let status_kind = match display.status {
        ToolUseStatus::Success => ToolStatus::Success,
        ToolUseStatus::Warning => ToolStatus::Warning,
        ToolUseStatus::Error => ToolStatus::Error,
        ToolUseStatus::InProgress => ToolStatus::Progress,
    };
    let mut status_text =
        if display.status_text.is_empty() && matches!(display.status, ToolUseStatus::InProgress) {
            tau_proto::PROGRESS_INDICATOR_TEXT.to_owned()
        } else {
            display.status_text.clone()
        };
    if matches!(display.status, ToolUseStatus::Error) {
        status_text = error_status_text(&status_text);
    }
    suffixes.push(tool_suffix(status_text, status_kind));
    ToolCallDisplay {
        tool_name: tool_name.to_owned(),
        mode: display.mode.clone(),
        args: display.args.clone(),
        range: display.range.as_ref().and_then(format_tool_use_range),
        suffixes,
        payload: display.payload.clone(),
    }
}

fn format_progress_counter(counter: &tau_proto::ProgressCounter) -> ToolSuffixSegment {
    let body = match counter.unit {
        tau_proto::ProgressUnit::Count => match (counter.complete, counter.total) {
            (Some(c), Some(t)) => format!("{c}/{t}"),
            (Some(c), None) => c.to_string(),
            (None, Some(t)) => format!("-/{t}"),
            (None, None) => "-".to_owned(),
        },
        tau_proto::ProgressUnit::Percent => match (counter.complete, counter.total) {
            (Some(p), Some(t)) => format!("{p}%/{}", format_token_count(t)),
            (Some(p), None) => format!("{p}%"),
            (None, Some(t)) => format!("-%/{}", format_token_count(t)),
            (None, None) => "-%".to_owned(),
        },
        tau_proto::ProgressUnit::Tokens => match (counter.complete, counter.total) {
            (Some(c), Some(t)) => format!("{}/{}", format_token_count(c), format_token_count(t)),
            (Some(c), None) => format_token_count(c),
            (None, Some(t)) => format!("-/{}", format_token_count(t)),
            (None, None) => "-".to_owned(),
        },
    };
    match counter.label.as_deref() {
        Some("ctx") => tool_suffix(format!("#{body}"), ToolStatus::Context),
        Some("tools") => tool_suffix(format!("%{body}"), ToolStatus::Tools),
        Some(label) => info_suffix(format!("{label}: {body}")),
        None => info_suffix(body),
    }
}

fn format_tool_use_range(range: &tau_proto::ToolUseRange) -> Option<String> {
    match (range.start.as_deref(), range.end.as_deref()) {
        (Some(start), Some(end)) => Some(format!("{start}..{end}")),
        (Some(start), None) => Some(format!("{start}..")),
        (None, Some(end)) => Some(format!("..{end}")),
        (None, None) => None,
    }
}

fn format_tool_use_state_stats(stats: &tau_proto::ToolUseStats) -> String {
    format_stats(stats.matches, stats.lines, stats.bytes)
}

fn format_stats(matches: Option<u64>, lines: Option<u64>, bytes: Option<u64>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(m) = matches {
        parts.push(m.to_string());
    }
    if let Some(l) = lines {
        parts.push(format!("{l}L"));
    }
    if let Some(b) = bytes {
        parts.push(format_tool_use_state_bytes(b));
    }
    parts.join(", ")
}

fn format_tool_use_state_bytes(bytes: u64) -> String {
    if bytes >= 1024 {
        let k = bytes as f64 / 1024.0;
        if k >= 100.0 {
            format!("{k:.0}kB")
        } else {
            format!("{k:.1}kB")
        }
    } else {
        format!("{bytes}B")
    }
}

/// Minimal display for events that didn't ship a [`ToolUseState`]
/// (old logs and any extension that hasn't migrated). Renders just
/// `<tool_name> ok` or `<tool_name> err: <short message>` — the chip
/// shape is intentionally generic so future tool names render without
/// touching this code.
pub(crate) fn synthesize_fallback_display(tool_name: &str, error: Option<&str>) -> ToolUseState {
    let (status, status_text) = match error {
        Some(msg) if !msg.is_empty() => (ToolUseStatus::Error, first_error_line(msg)),
        _ => (ToolUseStatus::Success, "ok".to_owned()),
    };
    let _ = tool_name;
    ToolUseState {
        args: String::new(),
        status,
        status_text,
        ..Default::default()
    }
}

fn first_error_line(message: &str) -> String {
    message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn error_status_text(label: &str) -> String {
    let label = label.trim();
    if label.is_empty() || label == "err" {
        return "err".to_owned();
    }
    if label.starts_with("err:") {
        return label.to_owned();
    }
    format!("err: {label}")
}

pub(crate) fn build_tool_summary_display(summary: &ToolSummaryDisplay) -> ToolCallDisplay {
    let mut suffixes = Vec::new();
    if 0 < summary.added {
        suffixes.push(tool_suffix(
            format!("+{}", summary.added),
            ToolStatus::DiffAdded,
        ));
    }
    if 0 < summary.removed {
        suffixes.push(ToolSuffixSegment {
            text: format!("-{}", summary.removed),
            status: ToolStatus::DiffRemoved,
            no_leading_space: 0 < summary.added,
        });
    }
    let stats = format_stats(
        (0 < summary.matches).then_some(summary.matches),
        (0 < summary.lines).then_some(summary.lines),
        (0 < summary.bytes).then_some(summary.bytes),
    );
    if !stats.is_empty() {
        suffixes.push(info_suffix(stats));
    }
    if 0 < summary.ok {
        suffixes.push(tool_suffix(
            format!("ok: {}", summary.ok),
            ToolStatus::Success,
        ));
    }
    if 0 < summary.err {
        suffixes.push(tool_suffix(
            format!("err: {}", summary.err),
            ToolStatus::Error,
        ));
    }
    if summary.completed < summary.total {
        suffixes.push(tool_suffix(
            tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            ToolStatus::Progress,
        ));
    }
    ToolCallDisplay {
        tool_name: "tools".to_owned(),
        mode: String::new(),
        args: format!("{}/{}", summary.completed, summary.total),
        range: None,
        suffixes,
        payload: None,
    }
}

/// Render a completed provider-side compaction item as a compact session
/// status line. Compaction is not a model-visible tool invocation, so this
/// paints the small lifecycle line directly instead of fabricating a
/// `ToolUseState`.
pub(crate) fn render_compaction_block(
    status_text: impl Into<String>,
    status: CompactionStatus,
) -> StyledBlock {
    let status_text = status_text.into();
    let status_style = match status {
        CompactionStatus::Success => BlockStyle::ToolStatusSuccess,
        CompactionStatus::Progress => BlockStyle::Progress,
    };
    let mut spans = vec![
        StyledSpan::new("compact", BlockStyle::ToolName),
        StyledSpan::new(" ", BlockStyle::ToolArgs),
    ];
    for (index, part) in status_text.split(' ').enumerate() {
        if 0 < index {
            spans.push(StyledSpan::new(" ", status_style));
        }
        let style = if part.starts_with('#') {
            BlockStyle::StatusContext
        } else {
            status_style
        };
        spans.push(StyledSpan::new(part.to_owned(), style));
    }
    StyledBlock::new(spans)
}

fn style_for_tool_status(status: ToolStatus) -> BlockStyle {
    match status {
        ToolStatus::Success => BlockStyle::ToolStatusSuccess,
        ToolStatus::Warning | ToolStatus::Pending | ToolStatus::Info => BlockStyle::ToolStatusInfo,
        ToolStatus::Error => BlockStyle::ToolStatusError,
        ToolStatus::Progress => BlockStyle::Progress,
        ToolStatus::DiffAdded => BlockStyle::DiffAdded,
        ToolStatus::DiffRemoved => BlockStyle::DiffRemoved,
        ToolStatus::Role => BlockStyle::StatusRole,
        ToolStatus::Context => BlockStyle::StatusContext,
        ToolStatus::Tools => BlockStyle::StatusTools,
        ToolStatus::Time => BlockStyle::ToolStatusTime,
    }
}

/// Paints a [`ToolCallDisplay`] onto a styled block.
pub(crate) fn render_tool_block(display: &ToolCallDisplay) -> StyledBlock {
    let is_shell_command = matches!(display.tool_name.as_str(), "shell" | "shell_command");
    let mut spans = if is_shell_command {
        let mut text = "$".to_owned();
        if !display.args.is_empty() {
            text.push(' ');
            text.push_str(&abbreviate_inline_text(&display.args));
        }
        vec![StyledSpan::new(text, BlockStyle::ToolArgs)]
    } else {
        vec![StyledSpan::new(
            display.tool_name.clone(),
            BlockStyle::ToolName,
        )]
    };
    if !display.mode.is_empty() {
        spans.push(StyledSpan::new(" ", BlockStyle::ToolArgs));
        spans.push(StyledSpan::new(
            abbreviate_inline_text(&display.mode),
            BlockStyle::ToolMode,
        ));
    }
    if !display.args.is_empty() && !is_shell_command {
        spans.push(StyledSpan::new(" ", BlockStyle::ToolArgs));
        spans.push(StyledSpan::new(
            abbreviate_inline_text(&display.args),
            BlockStyle::ToolArgs,
        ));
    }
    if let Some(range) = &display.range {
        spans.push(StyledSpan::new(" ", BlockStyle::ToolArgs));
        spans.push(StyledSpan::new(
            abbreviate_inline_text(range),
            BlockStyle::ToolArgs,
        ));
    }
    for suffix in &display.suffixes {
        if !suffix.no_leading_space && !suffix.text.starts_with(':') {
            spans.push(StyledSpan::new(" ", BlockStyle::ToolArgs));
        }
        spans.push(StyledSpan::new(
            abbreviate_inline_text(&suffix.text),
            style_for_tool_status(suffix.status),
        ));
    }
    if let Some(ToolUsePayload::Text { text }) = &display.payload {
        spans.push(StyledSpan::new("\n", BlockStyle::ToolArgs));
        spans.push(StyledSpan::new(text.clone(), BlockStyle::ToolArgs));
    }
    StyledBlock::new(spans)
}

/// Like [`render_tool_block`] but appends an expanded unified-diff
/// body when `expanded` is true and `diff` has hunks. The first line
/// is the themed tool header (with `+N/-M` chip); the body, if
/// rendered, comes after a `\n` so `layout_lines` wraps each diff line
/// independently.
pub(crate) fn render_diff_tool_block(
    display: &ToolCallDisplay,
    diff: &tau_proto::DiffSummary,
    expanded: bool,
) -> StyledBlock {
    let mut spans = render_tool_block(display).spans;

    if !expanded || diff.hunks.is_empty() {
        return StyledBlock::new(spans);
    }

    for hunk in &diff.hunks {
        spans.push(StyledSpan::new("\n", BlockStyle::DiffContext));
        spans.push(StyledSpan::new(
            format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            ),
            BlockStyle::DiffHunkHeader,
        ));
        for line in &hunk.lines {
            spans.push(StyledSpan::new("\n", BlockStyle::DiffContext));
            match line {
                tau_proto::DiffLine::Equal { text } => {
                    spans.push(StyledSpan::new(format!(" {text}"), BlockStyle::DiffContext));
                }
                tau_proto::DiffLine::Add { text } => {
                    spans.push(StyledSpan::new(format!("+{text}"), BlockStyle::DiffAdded));
                }
                tau_proto::DiffLine::Remove { text } => {
                    spans.push(StyledSpan::new(format!("-{text}"), BlockStyle::DiffRemoved));
                }
                tau_proto::DiffLine::Modify { old, new } => {
                    spans.push(StyledSpan::new("-", BlockStyle::DiffRemoved));
                    push_segments(
                        &mut spans,
                        old,
                        BlockStyle::DiffRemoved,
                        BlockStyle::DiffRemovedInline,
                    );
                    spans.push(StyledSpan::new("\n", BlockStyle::DiffContext));
                    spans.push(StyledSpan::new("+", BlockStyle::DiffAdded));
                    push_segments(
                        &mut spans,
                        new,
                        BlockStyle::DiffAdded,
                        BlockStyle::DiffAddedInline,
                    );
                }
            }
        }
    }
    StyledBlock::new(spans)
}

fn push_segments(
    spans: &mut Vec<StyledSpan>,
    segments: &[tau_proto::DiffSegment],
    base: BlockStyle,
    inline: BlockStyle,
) {
    for seg in segments {
        match seg {
            tau_proto::DiffSegment::Equal { text } => {
                spans.push(StyledSpan::new(text.clone(), base));
            }
            // Within a Modify line, only the *changed* sub-slice on
            // each side is meaningful. Hide the *other* side's slice
            // so we don't double up (e.g. the - line shouldn't show
            // the new tokens, only the old).
            tau_proto::DiffSegment::Remove { text } => {
                spans.push(StyledSpan::new(text.clone(), inline));
            }
            tau_proto::DiffSegment::Add { text } => {
                spans.push(StyledSpan::new(text.clone(), inline));
            }
        }
    }
}

/// Render a user `!`/`!!` shell block: a `shell <cmd>` header in the
/// same three-span theme used for tool calls, with streaming output
/// below in the default style.
///
/// `status_suffix`:
///   - `Some("running")` while the command is in-flight (info style),
///   - `Some("[0]")` / `Some("[N]")` on completion (success / error style,
///     keyed off exit code),
///   - `Some("cancelled")` on cancel (info style).
pub(crate) fn render_shell_block(
    command: &str,
    output: &str,
    status_suffix: Option<&str>,
) -> StyledBlock {
    let status_style = match status_suffix {
        Some(s) if s.starts_with("[0]") => BlockStyle::ToolStatusSuccess,
        Some(s) if s.starts_with('[') => BlockStyle::ToolStatusError,
        _ => BlockStyle::ToolStatusInfo,
    };

    let mut spans = vec![StyledSpan::new(
        format!("$ {}", abbreviate_inline_text(command)),
        BlockStyle::ToolArgs,
    )];
    if let Some(suffix) = status_suffix {
        spans.push(StyledSpan::new(" ", BlockStyle::ToolArgs));
        spans.push(StyledSpan::new(
            abbreviate_inline_text(suffix),
            status_style,
        ));
    }
    if !output.is_empty() {
        spans.push(StyledSpan::new("\n", BlockStyle::ToolArgs));
        spans.push(StyledSpan::new(output.to_owned(), BlockStyle::ToolArgs));
    }
    StyledBlock::new(spans)
}

pub(crate) fn render_action_output_block(text: &str) -> StyledBlock {
    let styles = ActionStyles {
        output: BlockStyle::ActionOutput,
        label: BlockStyle::ActionLabel,
        value: BlockStyle::ActionValue,
        id: BlockStyle::ActionId,
    };
    let mut spans = Vec::new();
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        push_action_line(&mut spans, body, styles);
        if line.ends_with('\n') {
            spans.push(StyledSpan::new("\n", styles.output));
        }
    }
    StyledBlock::new(spans)
}

pub(crate) fn render_action_error_block(action_id: &str, message: &str) -> StyledBlock {
    StyledBlock::new(vec![
        StyledSpan::new(action_id.to_owned(), BlockStyle::ActionId),
        StyledSpan::new(": ", BlockStyle::ActionOutput),
        StyledSpan::new(message.to_owned(), BlockStyle::ActionError),
    ])
}

#[derive(Clone, Copy)]
struct ActionStyles {
    output: BlockStyle,
    label: BlockStyle,
    value: BlockStyle,
    id: BlockStyle,
}

fn push_action_line(spans: &mut Vec<StyledSpan>, line: &str, styles: ActionStyles) {
    if push_action_approval_heading(spans, line, styles) {
        return;
    }
    if push_action_label_line(spans, line, styles) {
        return;
    }

    let mut index = 0;
    if let Some(end) = leading_action_id_end(line) {
        spans.push(StyledSpan::new(line[..end].to_owned(), styles.id));
        index = end;
    }
    push_action_tokens(spans, &line[index..], styles);
}

fn push_action_approval_heading(
    spans: &mut Vec<StyledSpan>,
    line: &str,
    styles: ActionStyles,
) -> bool {
    let Some((prefix, id)) = line.rsplit_once(' ') else {
        return false;
    };
    if !prefix.to_ascii_lowercase().contains("approval") || !is_action_id_token(id) {
        return false;
    }
    spans.push(StyledSpan::new(format!("{prefix} "), styles.output));
    spans.push(StyledSpan::new(id.to_owned(), styles.id));
    true
}

fn push_action_label_line(spans: &mut Vec<StyledSpan>, line: &str, styles: ActionStyles) -> bool {
    let Some(colon) = line.find(':') else {
        return false;
    };
    if line[..colon].contains(char::is_whitespace) {
        return false;
    }
    let label = &line[..=colon];
    let mut value = &line[colon + 1..];
    spans.push(StyledSpan::new(label.to_owned(), styles.label));
    if let Some(stripped) = value.strip_prefix(' ') {
        spans.push(StyledSpan::new(" ", styles.output));
        value = stripped;
    }
    let value_style = if is_action_id_label(&line[..colon]) && is_action_id_token(value) {
        styles.id
    } else {
        styles.value
    };
    spans.push(StyledSpan::new(value.to_owned(), value_style));
    true
}

fn push_action_tokens(spans: &mut Vec<StyledSpan>, text: &str, styles: ActionStyles) {
    let mut rest = text;
    while !rest.is_empty() {
        let split_at = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        if 0 < split_at {
            spans.push(StyledSpan::new(rest[..split_at].to_owned(), styles.output));
            rest = &rest[split_at..];
            continue;
        }
        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..token_end];
        push_action_token(spans, token, styles);
        rest = &rest[token_end..];
    }
}

fn push_action_token(spans: &mut Vec<StyledSpan>, token: &str, styles: ActionStyles) {
    let Some(eq) = token.find('=') else {
        spans.push(StyledSpan::new(token.to_owned(), styles.output));
        return;
    };
    if eq == 0 {
        spans.push(StyledSpan::new(token.to_owned(), styles.output));
        return;
    }
    spans.push(StyledSpan::new(token[..=eq].to_owned(), styles.label));
    spans.push(StyledSpan::new(token[eq + 1..].to_owned(), styles.value));
}

fn leading_action_id_end(line: &str) -> Option<usize> {
    let end = line.find(char::is_whitespace)?;
    let token = &line[..end];
    let rest = &line[end..];
    (is_action_id_token(token) && rest.contains('=')).then_some(end)
}

fn is_action_id_label(label: &str) -> bool {
    label == "id" || label.ends_with("_id") || label.ends_with("-id")
}

fn is_action_id_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 16
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub(crate) fn render_harness_notice(notice: &tau_proto::HarnessNotice) -> StyledBlock {
    let important = matches!(
        notice.level,
        tau_proto::NoticeLevel::Critical | tau_proto::NoticeLevel::Warning
    );

    if !important
        && let Some(path) = notice
            .message
            .strip_prefix("session dir: ")
            .and_then(|path| path.strip_suffix('/'))
    {
        return system_path_block("session dir: ", Path::new(path), "/");
    }

    let style = if important {
        BlockStyle::SystemImportant
    } else {
        BlockStyle::SystemInfo
    };
    StyledBlock::new(vec![StyledSpan::new(notice.message.clone(), style)])
}

pub(crate) fn ui_dir_block(path: &Path) -> StyledBlock {
    system_path_block("ui dir: ", path, "/")
}

pub(crate) fn session_status_block(path: &Path, suffix: &str, status: &str) -> StyledBlock {
    StyledBlock::new(vec![
        StyledSpan::new("session dir: ", BlockStyle::ExtensionLifecycle),
        StyledSpan::new(
            format!("{}{}", display_path(path), suffix),
            BlockStyle::SystemPath,
        ),
        StyledSpan::new(" ", BlockStyle::ExtensionLifecycle),
        StyledSpan::new(status.to_owned(), BlockStyle::ExtensionStatus),
    ])
}

fn system_path_block(prefix: &str, path: &Path, suffix: &str) -> StyledBlock {
    StyledBlock::new(vec![
        StyledSpan::new(prefix.to_owned(), BlockStyle::SystemInfo),
        StyledSpan::new(
            format!("{}{}", display_path(path), suffix),
            BlockStyle::SystemPath,
        ),
    ])
}

pub(crate) fn system_loaded_block(path: &Path, content: &str) -> StyledBlock {
    StyledBlock::new(vec![
        StyledSpan::new("loaded: ", BlockStyle::SystemInfo),
        StyledSpan::new(display_path(path), BlockStyle::SystemPath),
        StyledSpan::new(" ", BlockStyle::SystemInfo),
        StyledSpan::new(
            output_stats_suffix(content).text,
            BlockStyle::ToolStatusInfo,
        ),
    ])
}

pub(crate) fn agent_context_ready_block(agent_id: &tau_proto::AgentId) -> StyledBlock {
    StyledBlock::new(vec![
        StyledSpan::new("agent ", BlockStyle::SystemInfo),
        StyledSpan::new(format!("@{agent_id}"), BlockStyle::StatusRole),
        StyledSpan::new(" context ", BlockStyle::SystemInfo),
        StyledSpan::new("ready", BlockStyle::SystemStatus),
    ])
}

pub(crate) fn extension_status_block(extension_name: &str, status: &str) -> StyledBlock {
    StyledBlock::new(vec![
        StyledSpan::new("extension ", BlockStyle::ExtensionLifecycle),
        StyledSpan::new(extension_name.to_owned(), BlockStyle::ExtensionLifecycle),
        StyledSpan::new(" ", BlockStyle::ExtensionLifecycle),
        StyledSpan::new(status.to_owned(), BlockStyle::ExtensionStatus),
    ])
}

fn display_path(path: &Path) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return path.display().to_string();
    };
    let home = Path::new(&home);
    if home.as_os_str().is_empty() {
        return path.display().to_string();
    }
    let Ok(suffix) = path.strip_prefix(home) else {
        return path.display().to_string();
    };
    if suffix.as_os_str().is_empty() {
        "~".to_owned()
    } else {
        format!("~/{}", suffix.display())
    }
}
