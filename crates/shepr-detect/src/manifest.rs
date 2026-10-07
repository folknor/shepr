//! Screen-detection manifest format, loading and matching.
//!
//! Bundled manifests are compiled into the binary for the agents
//! [`bundled_manifest_source`] names, and compiled once per process on first use. There
//! are no local overrides and no reload: a detection change ships as a new
//! build.
//!
//! # Format
//!
//! ```toml
//! id = "claude"
//!
//! [[rules]]
//! id = "working"
//! state = "working"
//! regex = ['^\x{2810} ']
//! ```
//!
//! A manifest identifies an agent and contains ordered rules. Rules combine a
//! state and priority with matchers, nested gates, evidence flags and a region
//! selection. See [`AgentManifest`], [`ManifestRule`], [`ManifestGate`],
//! [`RegionSpec`] and the `MAX_*` constants for schema details and limits.
//! A `[matchers]` table names reusable gate bodies within one manifest;
//! `matcher = "name"` inserts one at a gate, and `rule = "agent.rule_id"`
//! reuses another bundled rule's matcher body while keeping the current rule's
//! state and priority.
//!
//! `regex` and `line_regex` patterns may use named character classes, expanded
//! before the pattern compiles. `{spinner}` matches one nonblank Braille cell
//! (U+2801 through U+28FF): the blank cell U+2800 renders as a space and never
//! counts as activity. `{spinner_run}` matches a run of Braille cells holding
//! at least one nonblank cell, for spinners several cells wide. `{dots_spinner}`
//! matches only the ten frames of the common dots spinner. Claude's
//! `{claude_live_glyph}` and `{claude_background_glyph}` classes share its
//! working glyph sets; the latter omits U+2733, Claude's idle title marker. A
//! manifest spells no Braille range of its own.
//!
//! A rule matches only when its conditions hold. The highest-priority match
//! wins, with manifest order breaking ties. No match uses the manifest's
//! fallback (`Idle` when omitted). Evidence flags describe visible state;
//! `skip_state_update` preserves prior state for agent-owned viewers.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;
use shepr_core::limits::UTF8_MAX_BYTES_PER_CODEPOINT;

use shepr_agent::{Agent, AgentState, parse_agent_label};

use crate::limits::{
    MAX_GATE_DEPTH, MAX_MANIFEST_PREVIEW_CHARS, MAX_MATCHER_CHARS, MAX_MATCHERS_PER_GATE,
    MAX_REFERENCE_DEPTH, MAX_REGION_LINE_COUNT, MAX_REGIONS_PER_MANIFEST, MAX_RULES_PER_MANIFEST,
    MAX_TOTAL_GATES, MAX_TOTAL_MATCHERS, MIN_REGION_LINE_COUNT,
};
use crate::{AgentDetection, Detection};

/// The named classes a manifest pattern may use (see the module docs). No
/// expansion contains another name.
const NAMED_REGEX_CLASSES: &[(&str, &str)] = &[
    ("{spinner}", r"[\x{2801}-\x{28FF}]"),
    (
        "{spinner_run}",
        r"[\x{2800}-\x{28FF}]*[\x{2801}-\x{28FF}][\x{2800}-\x{28FF}]*",
    ),
    (
        "{dots_spinner}",
        r"[\x{280B}\x{2819}\x{2839}\x{2838}\x{283C}\x{2834}\x{2826}\x{2827}\x{2807}\x{280F}]",
    ),
    (
        "{claude_live_glyph}",
        r"[\x{002A}\x{00B7}\x{2722}\x{2733}\x{2736}\x{273B}\x{273D}]",
    ),
    (
        "{claude_background_glyph}",
        r"[\x{002A}\x{00B7}\x{2722}\x{2736}\x{273B}\x{273D}]",
    ),
];

fn expand_named_regex_classes(pattern: &str) -> String {
    NAMED_REGEX_CLASSES
        .iter()
        .fold(pattern.to_string(), |expanded, (name, class)| {
            expanded.replace(name, class)
        })
}

shepr_core::named_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub enum FallbackReason {
        DefaultKnownAgentIdleFallback => "default_known_agent_idle_fallback",
        NoScreenManifest => "no_screen_manifest",
        ManifestUnknownFallback => "manifest_unknown_fallback",
        UnknownAgent => "unknown_agent",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkippedUpdateReason {
    MatchedRule { rule_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplainedAgent {
    Known(Agent),
    UnknownLabel(String),
}

impl ExplainedAgent {
    pub fn label(&self) -> &str {
        match self {
            Self::Known(agent) => agent.label(),
            Self::UnknownLabel(label) => label,
        }
    }
}

/// Input to the detection engine, carrying the screen snapshot plus any
/// OSC-derived evidence captured from the terminal title / progress sequences.
/// `osc_title` and `osc_progress` are `None` when there is no such evidence;
/// a region reading from one then has empty text. `osc_progress` is the
/// canonical `4;state[;percent]` spelling of the last ConEmu progress report
/// (for example `4;3` or `4;0;0`).
#[derive(Debug, Clone, Copy)]
pub struct DetectionInput<'a> {
    pub screen: &'a str,
    pub osc_title: Option<&'a str>,
    pub osc_progress: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectionExplain {
    pub agent: ExplainedAgent,
    pub verdict: AgentDetection,
    pub matched_rule: Option<MatchedRule>,
    pub skipped_update_reason: Option<SkippedUpdateReason>,
    pub fallback_reason: Option<FallbackReason>,
    pub evaluated_rules: Vec<EvaluatedRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedRule {
    pub id: String,
    pub priority: i32,
    pub region: RegionSpec,
    pub state: AgentState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluatedRule {
    pub id: String,
    pub priority: i32,
    pub region: RegionSpec,
    pub evidence: RuleEvidence,
    pub state: AgentState,
    pub matched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEvidence {
    pub contains: Vec<String>,
    pub regex: Vec<String>,
    pub line_regex: Vec<String>,
    pub all_count: usize,
    pub any_count: usize,
    pub not_count: usize,
    pub region_bytes: usize,
    pub region_preview: String,
}

/// A manifest ready for evaluation. The process-wide set hands these out by
/// shared reference, so a detection tick never clones the rule tree, and the
/// compiled regexes keep their search caches warm across ticks and panes.
#[derive(Debug)]
struct CompiledManifest {
    fallback: ManifestFallback,
    /// One compiled rule per source rule, in manifest order. Explain evidence
    /// is retained by each rule's compiled root gate.
    rules: Vec<CompiledRule>,
    /// Every distinct region any rule or gate reads; gates refer to regions by
    /// index so each region is extracted at most once per detection input.
    regions: Vec<CompiledRegion>,
    /// Rule indices by descending priority, manifest order within a priority.
    /// The first match in this order is the rule `explain` would select, so the
    /// detection path can stop there.
    priority_order: Vec<usize>,
    /// Whether an unchanged input can produce `Unknown` from a rule or fallback.
    unknown_is_stable: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentManifest {
    id: String,
    #[serde(default = "default_fallback")]
    fallback: ManifestFallback,
    /// Reusable gate bodies. References are expanded before validation, so the
    /// matching and complexity limits apply to the effective rule trees.
    #[serde(default)]
    matchers: BTreeMap<String, ManifestGate>,
    #[serde(default)]
    rules: Vec<ManifestRule>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ManifestRule {
    id: String,
    /// Reuse another rule's matcher body, retaining this rule's verdict and
    /// priority. Rule references are qualified as `agent.rule_id`.
    #[serde(default, rename = "rule")]
    rule_reference: Option<String>,
    #[serde(default = "default_state")]
    state: AgentState,
    #[serde(default)]
    priority: i32,
    #[serde(default = "default_region")]
    region: String,
    /// `visible_idle` and `visible_blocker`: the matched screen visibly shows
    /// that state's live chrome. Validation requires the rule's `state` to be
    /// the corresponding one. Working has no such flag: nothing treats visible
    /// working chrome differently from any other working evidence.
    #[serde(default)]
    visible_idle: bool,
    #[serde(default)]
    visible_blocker: bool,
    /// The screen is an agent-owned viewer (a transcript, a picker) that says
    /// nothing about the live state, so the pane keeps its previous state.
    /// Requires `state = "unknown"` and no `visible_*` flag.
    #[serde(default)]
    skip_state_update: bool,
    #[serde(default)]
    all: Vec<ManifestGate>,
    #[serde(default)]
    any: Vec<ManifestGate>,
    #[serde(default, rename = "not")]
    not_gate: Vec<ManifestGate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

/// A gate: an inline table with the same matcher and gate keys a rule has,
/// plus an optional `region`. Every listed matcher must hold: `contains`
/// needles occur case-insensitively, `regex` patterns match somewhere in the
/// region, `line_regex` patterns each match at least one line. `all` needs
/// every nested gate, `any` at least one, `not` none.
///
/// Every gate needs a positive matcher (`contains`, `regex`, `line_regex`,
/// `all` or `any`); a gate inside `not` may consist of nested `not` gates only.
/// Gate depth counts the rule's root matcher as level one, up to
/// `MAX_GATE_DEPTH`.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ManifestGate {
    /// Reuse a gate from this manifest's `[matchers]` table.
    #[serde(default)]
    matcher: Option<String>,
    /// Reuse a rule's matcher body, qualified as `agent.rule_id`.
    #[serde(default, rename = "rule")]
    rule_reference: Option<String>,
    /// Region this gate (and its nested gates) reads instead of the enclosing
    /// one. Lets a rule on one input AND/OR/NOT controls on another, e.g. a
    /// title-spinner rule that stands down while a dialog is on screen.
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    all: Vec<ManifestGate>,
    #[serde(default)]
    any: Vec<ManifestGate>,
    #[serde(default, rename = "not")]
    not_gate: Vec<ManifestGate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

#[derive(Debug)]
struct CompiledRule {
    id: String,
    verdict: AgentDetection,
    priority: i32,
    gate: CompiledGate,
    /// Index of the rule's own region, used for `explain` evidence.
    region_index: usize,
    /// Distinct region indices the rule's gate tree reads.
    regions_used: Vec<usize>,
}

#[derive(Debug)]
struct CompiledGate {
    region: usize,
    all: Vec<CompiledGate>,
    any: Vec<CompiledGate>,
    not_gate: Vec<CompiledGate>,
    contains: Vec<CompiledContains>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
}

#[derive(Debug)]
struct CompiledContains {
    original_needle: String,
    lowercase_needle: String,
    prefix: Vec<usize>,
    case_ignorable: &'static Regex,
    cased: &'static Regex,
}

impl CompiledContains {
    // Edge case, accepted: `str::to_lowercase` applies final sigma in the
    // needle's own context, while `matches` lowercases the text per character
    // with a context-aware sigma. A needle ending in capital sigma (U+03A3)
    // therefore lowercases to a final sigma and cannot match mid-word text.
    // Bundled manifests are English and never hit it.
    fn new(needle: &str) -> Result<Self, String> {
        let case_ignorable = case_ignorable_regex()?;
        let cased = cased_regex()?;
        let lowercase_needle = needle.to_lowercase();
        let bytes = lowercase_needle.as_bytes();
        let mut prefix = vec![0; bytes.len()];
        for index in 1..bytes.len() {
            let mut matched = prefix[index - 1];
            while matched > 0 && bytes[index] != bytes[matched] {
                matched = prefix[matched - 1];
            }
            if bytes[index] == bytes[matched] {
                matched += 1;
            }
            prefix[index] = matched;
        }
        Ok(Self {
            original_needle: needle.to_string(),
            lowercase_needle,
            prefix,
            case_ignorable,
            cased,
        })
    }

    fn matches(&self, text: &str) -> bool {
        let needle = self.lowercase_needle.as_bytes();
        if needle.is_empty() {
            return true;
        }

        let mut matched = 0;
        for (index, character) in text.char_indices() {
            if character == 'Σ' {
                let lowercase_sigma = if final_sigma(text, index, self.case_ignorable, self.cased) {
                    'ς'
                } else {
                    'σ'
                };
                if self.feed(lowercase_sigma, needle, &mut matched) {
                    return true;
                }
            } else {
                for lowercase_character in character.to_lowercase() {
                    if self.feed(lowercase_character, needle, &mut matched) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn feed(&self, character: char, needle: &[u8], matched: &mut usize) -> bool {
        let mut encoded = [0; UTF8_MAX_BYTES_PER_CODEPOINT];
        for &byte in character.encode_utf8(&mut encoded).as_bytes() {
            while *matched > 0 && byte != needle[*matched] {
                *matched = self.prefix[*matched - 1];
            }
            if byte == needle[*matched] {
                *matched += 1;
                if *matched == needle.len() {
                    return true;
                }
            }
        }
        false
    }
}

fn final_sigma(text: &str, index: usize, case_ignorable: &Regex, cased: &Regex) -> bool {
    fn case_ignorable_then_cased(
        mut characters: impl Iterator<Item = char>,
        case_ignorable: &Regex,
        cased: &Regex,
    ) -> bool {
        for character in characters.by_ref() {
            if !character_matches_unicode_property(character, case_ignorable) {
                return character_matches_unicode_property(character, cased);
            }
        }
        false
    }

    case_ignorable_then_cased(text[..index].chars().rev(), case_ignorable, cased)
        && !case_ignorable_then_cased(
            text[index + 'Σ'.len_utf8()..].chars(),
            case_ignorable,
            cased,
        )
}

fn character_matches_unicode_property(character: char, property: &Regex) -> bool {
    let mut encoded = [0; UTF8_MAX_BYTES_PER_CODEPOINT];
    property.is_match(character.encode_utf8(&mut encoded))
}

fn case_ignorable_regex() -> Result<&'static Regex, String> {
    static PROPERTY: OnceLock<Result<Regex, String>> = OnceLock::new();
    PROPERTY
        .get_or_init(|| Regex::new(r"\p{Case_Ignorable}").map_err(|error| error.to_string()))
        .as_ref()
        .map_err(Clone::clone)
}

fn cased_regex() -> Result<&'static Regex, String> {
    static PROPERTY: OnceLock<Result<Regex, String>> = OnceLock::new();
    PROPERTY
        .get_or_init(|| Regex::new(r"\p{Cased}").map_err(|error| error.to_string()))
        .as_ref()
        .map_err(Clone::clone)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompiledRegion {
    spec: RegionSpec,
}

// A region declaration generates both directions, including parameter prefixes.
macro_rules! region_specs {
    ($(#[$attr:meta])* pub enum $name:ident {
        $($(#[$unit_attr:meta])* $unit:ident => $unit_name:literal,)*
        ;
        $($(#[$count_attr:meta])* $count:ident(usize) => $count_name:literal),* $(,)?
    }) => {
        $(#[$attr])*
        pub enum $name {
            $($(#[$unit_attr])* $unit,)*
            $($(#[$count_attr])* $count(usize)),*
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    $(Self::$unit => f.write_str($unit_name),)*
                    $(Self::$count(count) => write!(f, "{}({count})", $count_name)),*
                }
            }
        }

        impl $name {
            fn parse(spec: &str) -> Option<Self> {
                let trimmed = spec.trim();
                match trimmed {
                    $($unit_name => Some(Self::$unit),)*
                    _ => {
                        $(if let Some(count) = region_count(trimmed, $count_name) {
                            return Some(Self::$count(count));
                        })*
                        None
                    }
                }
            }

            // Not gated on a test cfg: one here would put the rest of this
            // file below a test cfg, which the skip-after-scopes check refuses.
            #[cfg_attr(
                not(test),
                expect(dead_code, reason = "only the round-trip test enumerates every spelling")
            )]
            fn spelling_examples() -> Vec<Self> {
                vec![$(Self::$unit,)* $(Self::$count(MIN_REGION_LINE_COUNT), Self::$count(MAX_REGION_LINE_COUNT)),*]
            }
        }
    };
}

region_specs! {
    /// The text a rule or gate reads, named in a manifest by the spelling
    /// [`RegionSpec::parse`] accepts.
    ///
    /// The Codex prompt regions share one structure: a prompt line is `›` or
    /// starts with `› `, a block marker line starts with `•`, `■`, a cross mark
    /// (U+2717) or a check mark (U+2713), and the current prompt is the last prompt
    /// line with no block marker below it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RegionSpec {
        /// `whole_recent`: the whole detection snapshot.
        WholeRecent => "whole_recent",
        /// `codex_after_last_prompt_marker`: text after Codex's last `›` prompt line.
        CodexAfterLastPromptMarker => "codex_after_last_prompt_marker",
        /// `codex_before_current_prompt_marker`: text before Codex's current `›` prompt line.
        CodexBeforeCurrentPromptMarker => "codex_before_current_prompt_marker",
        /// `codex_whole_recent_without_current_prompt_marker`: empty while a current
        /// Codex prompt exists.
        CodexWholeRecentWithoutCurrentPromptMarker => "codex_whole_recent_without_current_prompt_marker",
        /// `claude_prompt_box_body`: lines inside Claude's bottom-most prompt box.
        ClaudePromptBoxBody => "claude_prompt_box_body",
        /// `claude_last_non_empty_above_prompt_box`: the last non-empty line before
        /// Claude's bottom-most prompt box, or the last non-empty screen line without one.
        ClaudeLastNonEmptyAbovePromptBox => "claude_last_non_empty_above_prompt_box",
        /// `after_last_horizontal_rule`: text after the last `─` rule line.
        AfterLastHorizontalRule => "after_last_horizontal_rule",
        /// `osc_title`: the last OSC window title, not the screen.
        OscTitle => "osc_title",
        /// `osc_progress`: the last OSC 9;4 progress report as `4;state[;percent]`,
        /// not the screen.
        OscProgress => "osc_progress",
        ;
        /// `bottom_non_empty_lines(N)` and `top_non_empty_lines(N)`: bounded by
        /// `MIN_REGION_LINE_COUNT..=MAX_REGION_LINE_COUNT`, written without a leading zero.
        BottomNonEmptyLines(usize) => "bottom_non_empty_lines",
        TopNonEmptyLines(usize) => "top_non_empty_lines",
    }
}

impl RegionSpec {
    /// Extract this region without building a line index for a detection tick.
    fn extract<'a>(self, input: DetectionInput<'a>) -> &'a str {
        match self {
            Self::OscTitle => input.osc_title.unwrap_or(""),
            Self::OscProgress => input.osc_progress.unwrap_or(""),
            Self::WholeRecent => input.screen,
            Self::AfterLastHorizontalRule => after_last_horizontal_rule(input.screen),
            Self::CodexAfterLastPromptMarker => codex_after_last_prompt_marker(input.screen),
            Self::CodexBeforeCurrentPromptMarker => {
                codex_before_current_prompt_marker(input.screen)
            }
            Self::CodexWholeRecentWithoutCurrentPromptMarker => {
                codex_whole_recent_without_current_prompt_marker(input.screen)
            }
            Self::ClaudePromptBoxBody => claude_prompt_box_body(input.screen).unwrap_or(""),
            Self::ClaudeLastNonEmptyAbovePromptBox => {
                let Some((top_start, _, _)) = claude_prompt_box_bounds(input.screen) else {
                    return last_non_empty_line(input.screen);
                };
                last_non_empty_line(&input.screen[..top_start.min(input.screen.len())])
            }
            Self::BottomNonEmptyLines(count) => bottom_non_empty_lines(input.screen, count),
            Self::TopNonEmptyLines(count) => top_non_empty_lines(input.screen, count),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ManifestFallback {
    Idle,
    Unknown,
}

impl From<ManifestFallback> for AgentState {
    fn from(value: ManifestFallback) -> Self {
        match value {
            ManifestFallback::Idle => AgentState::Idle,
            ManifestFallback::Unknown => AgentState::Unknown,
        }
    }
}

fn default_region() -> String {
    "whole_recent".to_string()
}

fn default_fallback() -> ManifestFallback {
    ManifestFallback::Idle
}

fn default_state() -> AgentState {
    AgentState::Unknown
}

/// The bundled screen-detection rules for `agent`, or `None` for an agent
/// without screen detection. Exhaustive, so a new agent is classified here.
///
/// Most of these manifests are pinned by no captured-screen test, and that is
/// deliberate: shepr keeps no capture corpus of its own. Manifest fixes come
/// from upstream herdr, which `scripts/upstream_watch.py` surfaces.
pub const fn bundled_manifest_source(agent: Agent) -> Option<&'static str> {
    match agent {
        Agent::Pi => Some(include_str!("manifests/pi.toml")),
        Agent::Claude => Some(include_str!("manifests/claude.toml")),
        Agent::Codex => Some(include_str!("manifests/codex.toml")),
        Agent::Gemini => Some(include_str!("manifests/gemini.toml")),
        Agent::Cursor => Some(include_str!("manifests/cursor.toml")),
        Agent::Devin => Some(include_str!("manifests/devin.toml")),
        Agent::Antigravity => Some(include_str!("manifests/antigravity.toml")),
        Agent::Cline => Some(include_str!("manifests/cline.toml")),
        Agent::Omp | Agent::Mastracode => None,
        Agent::OpenCode => Some(include_str!("manifests/opencode.toml")),
        Agent::GithubCopilot => Some(include_str!("manifests/github-copilot.toml")),
        Agent::Kimi => Some(include_str!("manifests/kimi.toml")),
        Agent::Kiro => Some(include_str!("manifests/kiro.toml")),
        Agent::Droid => Some(include_str!("manifests/droid.toml")),
        Agent::Amp => Some(include_str!("manifests/amp.toml")),
        Agent::Grok => Some(include_str!("manifests/grok.toml")),
        Agent::Kilo => Some(include_str!("manifests/kilo.toml")),
        Agent::Qodercli => Some(include_str!("manifests/qodercli.toml")),
        Agent::Qwen => Some(include_str!("manifests/qwen.toml")),
        Agent::Letta => Some(include_str!("manifests/letta.toml")),
        Agent::Maki => Some(include_str!("manifests/maki.toml")),
        Agent::Muse => Some(include_str!("manifests/muse.toml")),
    }
}

/// The agents with bundled screen-detection rules.
fn screen_manifest_agents() -> impl Iterator<Item = Agent> {
    Agent::all().filter(|agent| bundled_manifest_source(*agent).is_some())
}

/// Every screen-manifest agent's bundled manifest, compiled once per process
/// on first use and never replaced.
static MANIFESTS: OnceLock<Vec<(Agent, Option<CompiledManifest>)>> = OnceLock::new();

fn manifests() -> &'static [(Agent, Option<CompiledManifest>)] {
    MANIFESTS.get_or_init(|| {
        screen_manifest_agents()
            .map(|agent| {
                let loaded = bundled_manifest(agent);
                (agent, loaded)
            })
            .collect()
    })
}

/// Compile every bundled manifest now, so the first detection tick does not
/// pay for it. Server startup calls this before restoring panes.
pub fn compile_bundled_manifests() {
    manifests();
}

/// The compiled bundled manifest for `agent`, or `None` for an agent without
/// screen detection. An invalid bundled manifest fails startup.
fn loaded(agent: Agent) -> Option<&'static CompiledManifest> {
    manifests()
        .iter()
        .find(|(candidate, _)| *candidate == agent)
        .and_then(|(_, loaded)| loaded.as_ref())
}

/// Production detection path. Runs per identified pane on every detection
/// tick, so it evaluates rules in priority order, stops at the first match,
/// and builds none of the evidence `explain` reports.
pub fn detect_with_osc(agent: Agent, input: DetectionInput<'_>) -> AgentDetection {
    detect_with_manifest(input, loaded(agent))
}

fn detect_with_manifest(
    input: DetectionInput<'_>,
    loaded: Option<&CompiledManifest>,
) -> AgentDetection {
    let Some(loaded) = loaded else {
        return fallback_detection(None);
    };
    let mut texts = RegionTexts::new(input);
    for &index in &loaded.priority_order {
        let Some(rule) = loaded.rules.get(index) else {
            continue;
        };
        if compiled_rule_matches(rule, &loaded.regions, &mut texts) {
            return rule_detection(rule);
        }
    }
    fallback_detection(Some(loaded))
}

/// Whether this screen detector can report a stable `Unknown` for `agent`.
/// Agents without screen manifests report `Unknown`; a compiled manifest
/// can report it through its fallback or a rule that updates state.
pub fn screen_unknown_is_stable(agent: Agent) -> bool {
    loaded(agent).is_none_or(|manifest| manifest.unknown_is_stable)
}

pub fn explain_with_input(agent: Agent, input: DetectionInput<'_>) -> DetectionExplain {
    explain_with_manifest(agent, input, loaded(agent))
}

fn explain_with_manifest(
    agent: Agent,
    input: DetectionInput<'_>,
    loaded: Option<&CompiledManifest>,
) -> DetectionExplain {
    let Some(loaded) = loaded else {
        return fallback_explain(agent, None);
    };
    explain_loaded_manifest(agent, input, loaded)
}

/// Explain a captured detector input against the bundled manifest for `agent_label`.
pub fn explain_for_label(agent_label: &str, input: DetectionInput<'_>) -> DetectionExplain {
    let Some(agent) = parse_agent_label(agent_label) else {
        return DetectionExplain {
            agent: ExplainedAgent::UnknownLabel(agent_label.to_string()),
            verdict: AgentDetection::State(Detection::Unknown),
            matched_rule: None,
            skipped_update_reason: None,
            fallback_reason: Some(FallbackReason::UnknownAgent),
            evaluated_rules: Vec::new(),
        };
    };
    explain_with_input(agent, input)
}

fn rule_state(rule: &CompiledRule) -> AgentState {
    rule.verdict.state()
}

fn rule_detection(rule: &CompiledRule) -> AgentDetection {
    rule.verdict
}

/// State reported when no rule matched, or when no compiled manifest exists.
fn fallback_state(manifest: Option<&CompiledManifest>) -> AgentState {
    manifest.map_or(AgentState::Unknown, |manifest| manifest.fallback.into())
}

fn fallback_detection(manifest: Option<&CompiledManifest>) -> AgentDetection {
    AgentDetection::State(Detection::new(fallback_state(manifest), false))
}

fn explain_loaded_manifest(
    agent: Agent,
    input: DetectionInput<'_>,
    loaded: &CompiledManifest,
) -> DetectionExplain {
    let mut texts = RegionTexts::new(input);
    let mut matched_rules = Vec::with_capacity(loaded.rules.len());
    let mut evaluated_rules = Vec::with_capacity(loaded.rules.len());

    for rule in &loaded.rules {
        let matched = compiled_rule_matches(rule, &loaded.regions, &mut texts);
        let region_text = texts.text(rule.region_index);
        matched_rules.push(matched);
        evaluated_rules.push(EvaluatedRule {
            id: rule.id.clone(),
            priority: rule.priority,
            region: loaded.regions[rule.region_index].spec,
            evidence: rule_evidence(rule, region_text),
            state: rule_state(rule),
            matched,
        });
    }

    let matched = loaded
        .priority_order
        .iter()
        .copied()
        .find(|&index| matched_rules.get(index).copied().unwrap_or(false));
    let Some(rule) = matched.and_then(|index| loaded.rules.get(index)) else {
        return fallback_explain(agent, Some((loaded, evaluated_rules)));
    };

    let detection = rule_detection(rule);
    let skipped_update_reason =
        rule.verdict
            .skip_state_update()
            .then(|| SkippedUpdateReason::MatchedRule {
                rule_id: rule.id.clone(),
            });

    DetectionExplain {
        agent: ExplainedAgent::Known(agent),
        verdict: detection,
        matched_rule: Some(MatchedRule {
            id: rule.id.clone(),
            priority: rule.priority,
            region: loaded.regions[rule.region_index].spec,
            state: detection.state(),
        }),
        skipped_update_reason,
        fallback_reason: None,
        evaluated_rules,
    }
}

fn fallback_explain(
    agent: Agent,
    context: Option<(&CompiledManifest, Vec<EvaluatedRule>)>,
) -> DetectionExplain {
    let manifest = context.as_ref().map(|(manifest, _)| *manifest);
    let manifest_fallback = context.as_ref().map(|(manifest, _)| manifest.fallback);
    let evaluated_rules = context.map_or_else(Vec::new, |(_, evaluated)| evaluated);

    DetectionExplain {
        agent: ExplainedAgent::Known(agent),
        verdict: fallback_detection(manifest),
        matched_rule: None,
        skipped_update_reason: None,
        fallback_reason: match manifest_fallback {
            None => Some(FallbackReason::NoScreenManifest),
            Some(ManifestFallback::Unknown) => Some(FallbackReason::ManifestUnknownFallback),
            Some(ManifestFallback::Idle) => Some(FallbackReason::DefaultKnownAgentIdleFallback),
        },
        evaluated_rules,
    }
}

fn bundled_manifest(agent: Agent) -> Option<CompiledManifest> {
    let id = agent.label();
    bundled_manifest_source(agent)?;
    // These bytes are compiled into the executable and the all-bundled test
    // validates them. Startup eagerly compiles them before restoring panes;
    // a broken build must fail there rather than silently disable detection.
    let catalog = bundled_manifest_catalog()
        .unwrap_or_else(|error| panic!("invalid bundled detection manifest catalog: {error}"));
    let manifest = catalog
        .get(id)
        .cloned()
        .unwrap_or_else(|| panic!("missing bundled detection manifest for {id}"));
    Some(
        compile_manifest_with_catalog(manifest, catalog)
            .unwrap_or_else(|error| panic!("invalid bundled detection manifest for {id}: {error}")),
    )
}

fn parse_manifest_source(content: &str) -> Result<AgentManifest, String> {
    toml::from_str::<AgentManifest>(content).map_err(|err| err.to_string())
}

type ManifestCatalog = BTreeMap<String, AgentManifest>;

fn bundled_manifest_catalog() -> Result<&'static ManifestCatalog, String> {
    static CATALOG: OnceLock<Result<ManifestCatalog, String>> = OnceLock::new();
    match CATALOG.get_or_init(|| {
        let mut catalog = BTreeMap::new();
        for agent in Agent::all() {
            let Some(source) = bundled_manifest_source(agent) else {
                continue;
            };
            let manifest = parse_manifest_source(source)?;
            if manifest.id != agent.label() {
                return Err(format!(
                    "manifest id {} does not match agent label {}",
                    manifest.id,
                    agent.label()
                ));
            }
            if catalog.insert(manifest.id.clone(), manifest).is_some() {
                return Err(format!("duplicate bundled manifest id {}", agent.label()));
            }
        }
        Ok(catalog)
    }) {
        Ok(catalog) => Ok(catalog),
        Err(error) => Err(error.clone()),
    }
}

fn compile_manifest_with_catalog(
    manifest: AgentManifest,
    catalog: &ManifestCatalog,
) -> Result<CompiledManifest, String> {
    if manifest.rules.is_empty() {
        return Err("manifest must contain at least one rule".to_string());
    }
    if manifest.rules.len() > MAX_RULES_PER_MANIFEST {
        return Err(format!(
            "manifest contains {} rules, max is {MAX_RULES_PER_MANIFEST}",
            manifest.rules.len()
        ));
    }
    for name in manifest.matchers.keys() {
        if name.trim().is_empty() {
            return Err("manifest matcher name must not be empty".to_string());
        }
    }

    let mut unknown_is_stable = manifest.fallback == ManifestFallback::Unknown;
    let mut complexity = ManifestComplexity::default();
    let mut regions = RegionTable::default();
    let mut rules = Vec::with_capacity(manifest.rules.len());
    let owner = manifest.id.clone();
    for rule in manifest.rules {
        unknown_is_stable |= !rule.skip_state_update && rule.state == AgentState::Unknown;
        let root_gate = expand_rule_gate(&rule, &owner, catalog, &mut Vec::new(), 0, 0)?;
        rules.push(compile_rule(
            rule,
            &root_gate,
            &mut regions,
            &mut complexity,
        )?);
    }

    let mut priority_order: Vec<usize> = (0..rules.len()).collect();
    // Stable sort keeps manifest order within a priority; detection and explain
    // both select the first matching rule in this shared order.
    priority_order.sort_by_key(|&index| std::cmp::Reverse(rules[index].priority));
    Ok(CompiledManifest {
        fallback: manifest.fallback,
        rules,
        regions: regions.regions,
        priority_order,
        unknown_is_stable,
    })
}

fn expand_rule_gate(
    rule: &ManifestRule,
    owner: &str,
    catalog: &ManifestCatalog,
    stack: &mut Vec<String>,
    reference_depth: usize,
    gate_depth: usize,
) -> Result<ManifestGate, String> {
    if let Some(reference) = &rule.rule_reference {
        if !rule.all.is_empty()
            || !rule.any.is_empty()
            || !rule.not_gate.is_empty()
            || !rule.contains.is_empty()
            || !rule.regex.is_empty()
            || !rule.line_regex.is_empty()
            || rule.region != default_region()
        {
            return Err(format!(
                "rule {} combines a rule reference with inline matchers or a region",
                rule.id
            ));
        }
        return expand_rule_reference(
            reference,
            owner,
            catalog,
            stack,
            reference_depth,
            gate_depth,
        );
    }

    let root = ManifestGate {
        matcher: None,
        rule_reference: None,
        region: Some(rule.region.clone()),
        all: rule.all.clone(),
        any: rule.any.clone(),
        not_gate: rule.not_gate.clone(),
        contains: rule.contains.clone(),
        regex: rule.regex.clone(),
        line_regex: rule.line_regex.clone(),
    };
    expand_gate(root, owner, catalog, stack, reference_depth, gate_depth)
}

fn expand_rule_reference(
    reference: &str,
    _owner: &str,
    catalog: &ManifestCatalog,
    stack: &mut Vec<String>,
    reference_depth: usize,
    gate_depth: usize,
) -> Result<ManifestGate, String> {
    if reference_depth >= MAX_REFERENCE_DEPTH {
        return Err(format!(
            "manifest reference chain exceeds max depth {MAX_REFERENCE_DEPTH}"
        ));
    }
    let (agent, rule_id) = split_rule_reference(reference)?;
    let identity = format!("rule:{agent}.{rule_id}");
    if stack.contains(&identity) {
        return Err(format!("manifest reference cycle at {reference:?}"));
    }
    let Some(manifest) = catalog.get(agent) else {
        return Err(format!(
            "rule reference {reference:?} names an unknown agent"
        ));
    };
    let mut matching_rules = manifest.rules.iter().filter(|rule| rule.id == rule_id);
    let Some(rule) = matching_rules.next() else {
        return Err(format!(
            "rule reference {reference:?} names an unknown rule"
        ));
    };
    if matching_rules.next().is_some() {
        return Err(format!("rule reference {reference:?} is ambiguous"));
    }

    stack.push(identity);
    let result = expand_rule_gate(rule, agent, catalog, stack, reference_depth + 1, gate_depth);
    stack.pop();
    result
}

fn split_rule_reference(reference: &str) -> Result<(&str, &str), String> {
    let Some((agent, rule)) = reference.split_once('.') else {
        return Err(format!(
            "rule reference {reference:?} must be qualified as agent.rule_id"
        ));
    };
    if agent.is_empty() || rule.is_empty() || rule.contains('.') {
        return Err(format!("invalid qualified rule reference {reference:?}"));
    }
    Ok((agent, rule))
}

fn expand_gate(
    mut gate: ManifestGate,
    owner: &str,
    catalog: &ManifestCatalog,
    stack: &mut Vec<String>,
    reference_depth: usize,
    gate_depth: usize,
) -> Result<ManifestGate, String> {
    if gate_depth >= MAX_GATE_DEPTH {
        return Err(format!("rule exceeds max gate depth {MAX_GATE_DEPTH}"));
    }
    if gate.matcher.is_some() || gate.rule_reference.is_some() {
        let has_inline_matchers = gate.region.is_some()
            || !gate.all.is_empty()
            || !gate.any.is_empty()
            || !gate.not_gate.is_empty()
            || !gate.contains.is_empty()
            || !gate.regex.is_empty()
            || !gate.line_regex.is_empty();
        if has_inline_matchers || (gate.matcher.is_some() && gate.rule_reference.is_some()) {
            return Err("a matcher or rule reference cannot have inline gate fields".to_string());
        }
        if reference_depth >= MAX_REFERENCE_DEPTH {
            return Err(format!(
                "manifest reference chain exceeds max depth {MAX_REFERENCE_DEPTH}"
            ));
        }
        if let Some(name) = gate.matcher.take() {
            let identity = format!("matcher:{owner}.{name}");
            if stack.contains(&identity) {
                return Err(format!("manifest matcher reference cycle at {name:?}"));
            }
            let Some(manifest) = catalog.get(owner) else {
                return Err(format!("matcher reference owner {owner:?} is missing"));
            };
            let Some(source) = manifest.matchers.get(&name) else {
                return Err(format!("unknown matcher reference {name:?} in {owner}"));
            };
            stack.push(identity);
            let result = expand_gate(
                source.clone(),
                owner,
                catalog,
                stack,
                reference_depth + 1,
                gate_depth,
            );
            stack.pop();
            return result;
        }
        if let Some(reference) = gate.rule_reference.take() {
            return expand_rule_reference(
                &reference,
                owner,
                catalog,
                stack,
                reference_depth,
                gate_depth,
            );
        }
    }

    for nested in &mut gate.all {
        *nested = expand_gate(
            nested.clone(),
            owner,
            catalog,
            stack,
            reference_depth,
            gate_depth + 1,
        )?;
    }
    for nested in &mut gate.any {
        *nested = expand_gate(
            nested.clone(),
            owner,
            catalog,
            stack,
            reference_depth,
            gate_depth + 1,
        )?;
    }
    for nested in &mut gate.not_gate {
        *nested = expand_gate(
            nested.clone(),
            owner,
            catalog,
            stack,
            reference_depth,
            gate_depth + 1,
        )?;
    }
    Ok(gate)
}

#[derive(Default)]
struct ManifestComplexity {
    total_gates: usize,
    total_matchers: usize,
}

fn compile_rule(
    rule: ManifestRule,
    root_gate: &ManifestGate,
    regions: &mut RegionTable,
    complexity: &mut ManifestComplexity,
) -> Result<CompiledRule, String> {
    if rule.id.trim().is_empty() {
        return Err("manifest rule id must not be empty".to_string());
    }
    if rule.visible_idle && rule.state != AgentState::Idle {
        return Err(format!(
            "rule {} uses visible_idle without state = \"idle\"",
            rule.id
        ));
    }
    if rule.visible_blocker && rule.state != AgentState::Blocked {
        return Err(format!(
            "rule {} uses visible_blocker without state = \"blocked\"",
            rule.id
        ));
    }
    if rule.skip_state_update {
        if rule.state != AgentState::Unknown {
            return Err(format!(
                "rule {} uses skip_state_update without state = \"unknown\"",
                rule.id
            ));
        }
        if rule.visible_idle || rule.visible_blocker {
            return Err(format!(
                "rule {} uses skip_state_update with visible state evidence",
                rule.id
            ));
        }
    }

    let mut regions_used = Vec::new();
    let rule_id = &rule.id;
    let gate = compile_gate(
        GateSource::from_gate(root_gate),
        0,
        GateRequirement::Positive,
        "rule",
        0,
        regions,
        complexity,
        &mut regions_used,
    )
    .map_err(|error| format!("rule {rule_id} has invalid matcher gates: {error}"))?;

    Ok(CompiledRule {
        id: rule.id,
        verdict: if rule.skip_state_update {
            AgentDetection::Skip
        } else {
            AgentDetection::State(Detection::new(
                rule.state,
                rule.visible_idle || rule.visible_blocker,
            ))
        },
        priority: rule.priority,
        region_index: gate.region,
        gate,
        regions_used,
    })
}

#[derive(Clone, Copy)]
struct GateSource<'a> {
    region: Option<&'a str>,
    all: &'a [ManifestGate],
    any: &'a [ManifestGate],
    not_gate: &'a [ManifestGate],
    contains: &'a [String],
    regex: &'a [String],
    line_regex: &'a [String],
}

impl<'a> GateSource<'a> {
    fn from_gate(gate: &'a ManifestGate) -> Self {
        Self {
            region: gate.region.as_deref(),
            all: &gate.all,
            any: &gate.any,
            not_gate: &gate.not_gate,
            contains: &gate.contains,
            regex: &gate.regex,
            line_regex: &gate.line_regex,
        }
    }

    fn has_positive_matcher(self) -> bool {
        !self.contains.is_empty()
            || !self.regex.is_empty()
            || !self.line_regex.is_empty()
            || !self.all.is_empty()
            || !self.any.is_empty()
    }

    fn has_any_matcher(self) -> bool {
        self.has_positive_matcher() || !self.not_gate.is_empty()
    }
}

#[derive(Clone, Copy)]
enum GateRequirement {
    Positive,
    Any,
}

#[derive(Default)]
struct RegionTable {
    regions: Vec<CompiledRegion>,
}

impl RegionTable {
    fn intern(&mut self, spec: &str) -> Result<usize, String> {
        let spec = RegionSpec::parse(spec).ok_or_else(|| format!("invalid region {spec:?}"))?;
        if let Some(index) = self.regions.iter().position(|region| region.spec == spec) {
            return Ok(index);
        }
        if self.regions.len() >= MAX_REGIONS_PER_MANIFEST {
            return Err(format!(
                "manifest reads more than {MAX_REGIONS_PER_MANIFEST} distinct regions"
            ));
        }
        self.regions.push(CompiledRegion { spec });
        Ok(self.regions.len() - 1)
    }
}

fn compile_gate(
    gate: GateSource<'_>,
    inherited_region: usize,
    requirement: GateRequirement,
    context: &str,
    depth: usize,
    table: &mut RegionTable,
    complexity: &mut ManifestComplexity,
    regions_used: &mut Vec<usize>,
) -> Result<CompiledGate, String> {
    if depth >= MAX_GATE_DEPTH {
        return Err(format!("{context} exceeds max gate depth {MAX_GATE_DEPTH}"));
    }
    complexity.total_gates += 1;
    if complexity.total_gates > MAX_TOTAL_GATES {
        return Err(format!("manifest exceeds max gate count {MAX_TOTAL_GATES}"));
    }

    let region = match gate.region {
        Some(spec) => table.intern(spec)?,
        None => inherited_region,
    };
    if !regions_used.contains(&region) {
        regions_used.push(region);
    }

    let matcher_count = gate.contains.len() + gate.regex.len() + gate.line_regex.len();
    if matcher_count > MAX_MATCHERS_PER_GATE {
        return Err(format!(
            "{context} has {matcher_count} direct matchers, max is {MAX_MATCHERS_PER_GATE}"
        ));
    }
    complexity.total_matchers += matcher_count;
    if complexity.total_matchers > MAX_TOTAL_MATCHERS {
        return Err(format!(
            "manifest exceeds max matcher count {MAX_TOTAL_MATCHERS}"
        ));
    }
    for value in gate
        .contains
        .iter()
        .chain(gate.regex.iter())
        .chain(gate.line_regex.iter())
    {
        if value.chars().count() > MAX_MATCHER_CHARS {
            return Err(format!(
                "{context} matcher exceeds max length {MAX_MATCHER_CHARS}"
            ));
        }
    }
    let has_required_matcher = match requirement {
        GateRequirement::Positive => gate.has_positive_matcher(),
        GateRequirement::Any => gate.has_any_matcher(),
    };
    if !has_required_matcher {
        return Err(match requirement {
            GateRequirement::Positive => format!("{context} must contain a positive matcher"),
            GateRequirement::Any => format!("{context} must contain a matcher"),
        });
    }

    let mut all = Vec::with_capacity(gate.all.len());
    for nested in gate.all {
        all.push(compile_gate(
            GateSource::from_gate(nested),
            region,
            GateRequirement::Positive,
            "all gate",
            depth + 1,
            table,
            complexity,
            regions_used,
        )?);
    }
    let mut any = Vec::with_capacity(gate.any.len());
    for nested in gate.any {
        any.push(compile_gate(
            GateSource::from_gate(nested),
            region,
            GateRequirement::Positive,
            "any gate",
            depth + 1,
            table,
            complexity,
            regions_used,
        )?);
    }
    let mut not_gate = Vec::with_capacity(gate.not_gate.len());
    for nested in gate.not_gate {
        not_gate.push(compile_gate(
            GateSource::from_gate(nested),
            region,
            GateRequirement::Any,
            "not gate",
            depth + 1,
            table,
            complexity,
            regions_used,
        )?);
    }

    let contains = gate
        .contains
        .iter()
        .map(|needle| CompiledContains::new(needle))
        .collect::<Result<_, _>>()?;
    let regex = gate
        .regex
        .iter()
        .map(|pattern| {
            let expanded = expand_named_regex_classes(pattern);
            Regex::new(&expanded).map_err(|err| format!("invalid regex pattern {pattern:?}: {err}"))
        })
        .collect::<Result<_, _>>()?;
    let line_regex = gate
        .line_regex
        .iter()
        .map(|pattern| {
            let expanded = expand_named_regex_classes(pattern);
            Regex::new(&expanded)
                .map_err(|err| format!("invalid line_regex pattern {pattern:?}: {err}"))
        })
        .collect::<Result<_, _>>()?;
    Ok(CompiledGate {
        region,
        all,
        any,
        not_gate,
        contains,
        regex,
        line_regex,
    })
}

/// Per-input region texts, extracted lazily and at most once each. The fixed
/// cache is bounded by the validated distinct-region limit and needs no heap.
struct RegionTexts<'a> {
    input: DetectionInput<'a>,
    texts: [Option<&'a str>; MAX_REGIONS_PER_MANIFEST],
}

impl<'a> RegionTexts<'a> {
    fn new(input: DetectionInput<'a>) -> Self {
        Self {
            input,
            texts: [None; MAX_REGIONS_PER_MANIFEST],
        }
    }

    fn prepare(&mut self, regions: &[CompiledRegion], indices: &[usize]) {
        for &index in indices {
            let (Some(slot), Some(region)) = (self.texts.get_mut(index), regions.get(index)) else {
                continue;
            };
            if slot.is_some() {
                continue;
            }
            *slot = Some(region.spec.extract(self.input));
        }
    }

    fn text(&self, index: usize) -> &'a str {
        self.texts.get(index).and_then(|text| *text).unwrap_or("")
    }
}

fn compiled_rule_matches(
    rule: &CompiledRule,
    regions: &[CompiledRegion],
    texts: &mut RegionTexts<'_>,
) -> bool {
    texts.prepare(regions, &rule.regions_used);
    compiled_gate_matches(&rule.gate, texts)
}

fn rule_evidence(rule: &CompiledRule, region_text: &str) -> RuleEvidence {
    RuleEvidence {
        contains: rule
            .gate
            .contains
            .iter()
            .map(|matcher| matcher.original_needle.clone())
            .collect(),
        regex: rule
            .gate
            .regex
            .iter()
            .map(|regex| regex.as_str().to_string())
            .collect(),
        line_regex: rule
            .gate
            .line_regex
            .iter()
            .map(|regex| regex.as_str().to_string())
            .collect(),
        all_count: rule.gate.all.len(),
        any_count: rule.gate.any.len(),
        not_count: rule.gate.not_gate.len(),
        region_bytes: region_text.len(),
        region_preview: bounded_preview(region_text),
    }
}

fn bounded_preview(text: &str) -> String {
    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(MAX_MANIFEST_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
}

fn every_line_regex_matches(regexes: &[Regex], text: &str) -> bool {
    if regexes.is_empty() {
        return true;
    }
    if regexes.len() > MAX_MATCHERS_PER_GATE {
        // Validation rejects this shape; fail closed if an unvalidated gate reaches detection.
        return false;
    }

    let mut matched = [false; MAX_MATCHERS_PER_GATE];
    let mut remaining = regexes.len();
    for line in text.lines() {
        for (index, regex) in regexes.iter().enumerate() {
            if !matched[index] && regex.is_match(line) {
                matched[index] = true;
                remaining -= 1;
            }
        }
        if remaining == 0 {
            return true;
        }
    }
    false
}

fn compiled_gate_matches(gate: &CompiledGate, texts: &RegionTexts<'_>) -> bool {
    let text = texts.text(gate.region);
    if !gate.contains.iter().all(|needle| needle.matches(text)) {
        return false;
    }

    if !gate.regex.iter().all(|regex| regex.is_match(text)) {
        return false;
    }

    if !every_line_regex_matches(&gate.line_regex, text) {
        return false;
    }

    if !gate
        .all
        .iter()
        .all(|nested| compiled_gate_matches(nested, texts))
    {
        return false;
    }

    if !gate.any.is_empty()
        && !gate
            .any
            .iter()
            .any(|nested| compiled_gate_matches(nested, texts))
    {
        return false;
    }

    if gate
        .not_gate
        .iter()
        .any(|nested| compiled_gate_matches(nested, texts))
    {
        return false;
    }

    true
}

fn region_count(spec: &str, name: &str) -> Option<usize> {
    let count = spec
        .strip_prefix(name)?
        .strip_prefix('(')?
        .strip_suffix(')')?;
    if count.is_empty()
        || count.starts_with('0')
        || !count.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    count
        .parse::<usize>()
        .ok()
        .filter(|count| (MIN_REGION_LINE_COUNT..=MAX_REGION_LINE_COUNT).contains(count))
}

fn bottom_non_empty_lines(content: &str, count: usize) -> &str {
    let Some(line) = content
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(count)
        .last()
    else {
        return "";
    };
    &content[line_start_offset(content, line)..]
}

fn top_non_empty_lines(content: &str, count: usize) -> &str {
    let Some(line) = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(count)
        .last()
    else {
        return "";
    };
    &content[..line_end_offset(content, line)]
}

fn codex_after_last_prompt_marker(content: &str) -> &str {
    let mut after = None;
    for line in content.lines() {
        if codex_prompt_line(line) {
            after = Some(line_end_offset(content, line));
        }
    }
    after.map_or(content, |offset| &content[offset..])
}

fn codex_before_current_prompt_marker(content: &str) -> &str {
    let Some(prompt_start) = current_codex_prompt_start(content) else {
        return content;
    };
    &content[..prompt_start.min(content.len())]
}

fn codex_whole_recent_without_current_prompt_marker(content: &str) -> &str {
    if current_codex_prompt_start(content).is_some() {
        ""
    } else {
        content
    }
}

/// The current prompt line's start offset, or `None` when no prompt exists or
/// a block marker follows the last prompt.
fn current_codex_prompt_start(content: &str) -> Option<usize> {
    let mut prompt_start = None;
    let mut marker_after_prompt = false;

    for line in content.lines() {
        if codex_prompt_line(line) {
            prompt_start = Some(line_start_offset(content, line));
            marker_after_prompt = false;
        } else if prompt_start.is_some() && codex_block_marker_line(line) {
            marker_after_prompt = true;
        }
    }

    if marker_after_prompt {
        return None;
    }
    prompt_start
}

fn codex_prompt_line(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn codex_block_marker_line(line: &str) -> bool {
    line.starts_with('•')
        || line.starts_with('■')
        || line.starts_with('\u{2717}')
        || line.starts_with('\u{2713}')
}

fn claude_prompt_box_body(content: &str) -> Option<&str> {
    let (_, top_end, bottom_start) = claude_prompt_box_bounds(content)?;
    Some(&content[top_end.min(content.len())..bottom_start.min(content.len())])
}

fn claude_prompt_box_bounds(content: &str) -> Option<(usize, usize, usize)> {
    let mut penultimate_rule = None;
    let mut last_rule = None;
    for line in content.lines() {
        if is_horizontal_rule(line) {
            penultimate_rule = last_rule;
            last_rule = Some((
                line_start_offset(content, line),
                line_end_offset(content, line),
            ));
        }
    }
    let (top_start, top_end) = penultimate_rule?;
    let (bottom_start, _) = last_rule?;
    Some((top_start, top_end, bottom_start))
}

fn after_last_horizontal_rule(content: &str) -> &str {
    let mut last_rule_end = 0usize;
    let mut offset = 0usize;
    for segment in content.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let line = if segment.ends_with('\n') {
            line.strip_suffix('\r').unwrap_or(line)
        } else {
            line
        };
        let next_offset = offset + segment.len();
        if is_horizontal_rule(line) {
            last_rule_end = next_offset;
        }
        offset = next_offset;
    }
    &content[last_rule_end..]
}

fn last_non_empty_line(content: &str) -> &str {
    content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    let rule_chars = trimmed.chars().take_while(|&ch| ch == '─').count();
    if rule_chars == 0 {
        return false;
    }

    let rule_bytes = trimmed
        .char_indices()
        .nth(rule_chars)
        .map_or(trimmed.len(), |(index, _)| index);
    let suffix = trimmed[rule_bytes..].trim_start();

    suffix.is_empty() || rule_chars >= 3
}

fn line_start_offset(content: &str, line: &str) -> usize {
    // `str::lines()` strips both bytes of CRLF. Use the borrowed line's
    // original start address so slices retain the exact line-ending width.
    line.as_ptr()
        .addr()
        .saturating_sub(content.as_ptr().addr())
        .min(content.len())
}

fn line_end_offset(content: &str, line: &str) -> usize {
    let start = line_start_offset(content, line);
    content[start..]
        .find('\n')
        .map_or(content.len(), |offset| start + offset + 1)
}

/// Parse a bundled manifest and check its identity against its owning agent.
#[cfg(test)]
fn parse_bundled_manifest(label: &str, content: &str) -> Result<CompiledManifest, String> {
    let manifest = parse_manifest_source(content)?;
    if manifest.id != label {
        return Err(format!(
            "manifest id {} does not match agent label {label}",
            manifest.id
        ));
    }
    let mut catalog = (*bundled_manifest_catalog()?).clone();
    catalog.insert(label.to_string(), manifest.clone());
    compile_manifest_with_catalog(manifest, &catalog)
}

#[cfg(test)]
fn compile_manifest(manifest: AgentManifest) -> Result<CompiledManifest, String> {
    let mut catalog = BTreeMap::new();
    catalog.insert(manifest.id.clone(), manifest.clone());
    compile_manifest_with_catalog(manifest, &catalog)
}

#[cfg(test)]
fn parse_manifest(content: &str) -> Result<CompiledManifest, String> {
    compile_manifest(parse_manifest_source(content)?)
}

#[cfg(test)]
pub fn detect(agent: Agent, screen_content: &str) -> AgentDetection {
    detect_with_osc(
        agent,
        DetectionInput {
            screen: screen_content,
            osc_title: None,
            osc_progress: None,
        },
    )
}

#[cfg(test)]
fn region<'a>(input: DetectionInput<'a>, spec: &str) -> &'a str {
    RegionSpec::parse(spec).map_or("", |spec| spec.extract(input))
}

#[cfg(test)]
mod tests;
