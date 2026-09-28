//! Screen-detection manifests: the format, the loader and the matcher.
//!
//! # Where manifests come from
//!
//! Every agent marked for screen detection in the agent descriptor table has
//! a bundled manifest in
//! `crates/shepr-agent/src/detect/manifests/`. A local override at
//! `<config dir>/agent-detection/<agent label>.toml` replaces the bundled one
//! wholesale when its `id` (or one of its `aliases`) names that agent. An
//! override that does not parse, validate or compile is ignored with a warning
//! and the bundled manifest stays active. Manifests are read once and cached;
//! `shepr server reload-agent-manifests` rereads them.
//!
//! # Format
//!
//! ```toml
//! id = "claude"                  # agent label; must match the file it overrides
//! aliases = ["claude-code"]      # optional
//!
//! [[rules]]
//! id = "osc_title_working"       # required, non-empty
//! state = "working"              # idle | working | blocked | unknown
//! priority = 1100                # default 0; highest matching rule wins
//! region = "osc_title"           # default "whole_recent"
//! visible_working = true         # optional evidence flags, see below
//! regex = ['^\x{2810} ']
//! not = [
//!   { region = "bottom_non_empty_lines(12)", contains = ["esc to cancel"] },
//! ]
//! ```
//!
//! A rule matches when its own matchers and gates all hold against its
//! region. Among matching rules the highest `priority` wins; equal priorities
//! go to the rule listed first. No match gives the agent's fallback state
//! (`Unknown` for Codex, `Idle` for every other agent).
//!
//! Matchers, usable on a rule and on any gate; every listed one must hold:
//!
//! - `contains = [..]`: every needle occurs in the region, case-insensitively.
//! - `regex = [..]`: every pattern matches somewhere in the region text.
//! - `line_regex = [..]`: every pattern matches at least one line.
//!
//! Gates, also usable on a rule and nested inside any gate:
//!
//! - `all = [gate, ..]`: every gate matches.
//! - `any = [gate, ..]`: at least one gate matches.
//! - `not = [gate, ..]`: no gate matches.
//!
//! Each gate is an inline table with the same matcher and gate keys plus an
//! optional `region`. A gate without `region` reads the region of whatever
//! encloses it (the rule, or the parent gate). A gate with `region` reads that
//! region instead, and its nested gates inherit it. This lets one rule combine
//! controls from different inputs, for example a rule on `osc_title` whose
//! `not` gate reads `bottom_non_empty_lines(12)` so a title spinner stands
//! down while a dialog's controls are on screen. Encode invariant controls as
//! explicit AND (`all` / sibling matchers) and OR (`any`) gates rather than
//! one loose needle.
//!
//! Evidence and skip flags on a rule:
//!
//! - `visible_idle`, `visible_working`, `visible_blocker`: the matched screen
//!   visibly shows that state's live chrome. Each only counts when the rule's
//!   `state` is the corresponding one.
//! - `skip_state_update = true`: the screen is an agent-owned viewer (a
//!   transcript, a picker) that says nothing about the live state; the pane
//!   keeps its previous state. Requires `state = "unknown"` and no `visible_*`
//!   flag.
//!
//! # Regions
//!
//! - `whole_recent`: the whole detection snapshot.
//! - `osc_title`, `osc_progress`: the last OSC window title / OSC 9;4
//!   progress string, not the screen.
//! - `bottom_lines(N)`: the last N lines.
//! - `bottom_non_empty_lines(N)`: from the Nth-last non-empty line to the end.
//! - `top_non_empty_lines(N)`: from the start through the Nth non-empty line.
//!   For all three regions, N is 1..=65535 written without a leading zero.
//! - `after_last_horizontal_rule`: text after the last `─` rule line.
//! - `prompt_box_body`: lines between the top border of the bottom-most
//!   `─`-bordered box and the next rule.
//! - `above_prompt_box`: everything above that box (the whole snapshot when
//!   there is no box); `last_non_empty_above_prompt_box` is its last non-empty
//!   line.
//! - Codex prompt structure, where a prompt line is `›` or starts with `› `,
//!   a block marker line starts with `•`, `■`, a cross mark (U+2717) or a
//!   check mark (U+2713), and the current
//!   prompt is the last prompt line with no block marker below it:
//!   `after_last_prompt_marker`, `before_current_prompt_marker`,
//!   `whole_recent_without_current_prompt_marker` (empty while a current
//!   prompt exists), `current_prompt_block_marker` (the last block marker line
//!   above the current prompt) and `after_current_prompt_block_marker`.
//!
//! # Limits
//!
//! Rule count, gate depth, gate count, matchers per gate, total matchers and
//! matcher length are capped by the `MAX_*` constants below. Every gate needs a positive matcher (`contains`, `regex`, `line_regex`,
//! `all` or `any`); a gate inside `not` may consist of nested `not` gates only.
//! Gate depth counts the rule's root matcher as level one, with at most eight
//! levels total.
//! Unknown keys are rejected.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock},
};

use regex::Regex;
use serde::Deserialize;

use super::{Agent, AgentDetection, AgentState, agent_label, parse_agent_label};

pub const DEFAULT_KNOWN_AGENT_IDLE_FALLBACK: &str = "default_known_agent_idle_fallback";
pub const NO_SCREEN_MANIFEST_FALLBACK: &str = "no_screen_manifest";

/// Input to the detection engine, carrying the screen snapshot plus any
/// OSC-derived strings captured from the terminal title / progress sequences.
/// Pass empty strings for `osc_title` and `osc_progress` when the data is not
/// available - behavior is identical to the pre-OSC engine in that case.
#[derive(Debug, Clone, Copy)]
pub struct DetectionInput<'a> {
    pub screen: &'a str,
    pub osc_title: &'a str,
    pub osc_progress: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectionExplain {
    pub agent: Option<String>,
    pub state: AgentState,
    pub source: Option<ManifestSource>,
    pub matched_rule: Option<MatchedRule>,
    pub screen_detection_skipped: bool,
    pub visible_idle: bool,
    pub visible_blocker: bool,
    pub visible_working: bool,
    pub skip_state_update: bool,
    pub skipped_update_reason: Option<String>,
    pub fallback_reason: Option<String>,
    pub evaluated_rules: Vec<EvaluatedRule>,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestSource {
    Bundled,
    Override(PathBuf),
}

impl ManifestSource {
    pub fn label(&self) -> String {
        match self {
            Self::Bundled => "bundled".to_string(),
            Self::Override(path) => path.display().to_string(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::Override(_) => "local override",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentManifestSummary {
    pub agent: Agent,
    pub active_source: ManifestSource,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedRule {
    pub id: String,
    pub priority: i32,
    pub region: String,
    pub state: AgentState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluatedRule {
    pub id: String,
    pub priority: i32,
    pub region: String,
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

/// A manifest ready for evaluation. The cache hands these out behind an `Arc`,
/// so a detection tick never clones the rule tree, and the compiled regexes
/// keep their search caches warm across ticks and panes.
#[derive(Debug)]
struct LoadedManifest {
    manifest: AgentManifest,
    /// One entry per manifest rule, in manifest order.
    compiled_rules: Vec<CompiledRule>,
    /// Every distinct region any rule or gate reads; gates refer to regions by
    /// index so each region is extracted at most once per detection input.
    regions: Vec<CompiledRegion>,
    /// Rule indices by descending priority, manifest order within a priority.
    /// The first match in this order is the rule `explain` would select, so the
    /// detection path can stop there.
    priority_order: Vec<usize>,
    source: ManifestSource,
    warning: Option<String>,
}

#[derive(Debug)]
struct CompiledManifest {
    compiled_rules: Vec<CompiledRule>,
    regions: Vec<CompiledRegion>,
}

#[derive(Debug, Clone)]
struct ManifestCache {
    manifests: Vec<(Agent, Option<Arc<LoadedManifest>>)>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentManifest {
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    rules: Vec<ManifestRule>,
    #[serde(skip)]
    compiled: Option<CompiledManifest>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ManifestRule {
    id: String,
    state: Option<ManifestState>,
    #[serde(default)]
    priority: i32,
    #[serde(default = "default_region")]
    region: String,
    #[serde(default)]
    visible_idle: bool,
    #[serde(default)]
    visible_blocker: bool,
    #[serde(default)]
    visible_working: bool,
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

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ManifestGate {
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
    gate: CompiledGate,
    /// Index of the rule's own region, used for `explain` evidence.
    region: usize,
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
    lowercase_needle: String,
    prefix: Vec<usize>,
    case_ignorable: &'static Regex,
    cased: &'static Regex,
}

impl CompiledContains {
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
        let mut encoded = [0; 4];
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
            if !is_case_ignorable(character, case_ignorable) {
                return is_cased(character, cased);
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

fn is_case_ignorable(character: char, property: &Regex) -> bool {
    let mut encoded = [0; 4];
    property.is_match(character.encode_utf8(&mut encoded))
}

fn is_cased(character: char, property: &Regex) -> bool {
    let mut encoded = [0; 4];
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionSpec {
    WholeRecent,
    AfterLastPromptMarker,
    BeforeCurrentPromptMarker,
    WholeRecentWithoutCurrentPromptMarker,
    CurrentPromptBlockMarker,
    AfterCurrentPromptBlockMarker,
    PromptBoxBody,
    AbovePromptBox,
    LastNonEmptyAbovePromptBox,
    AfterLastHorizontalRule,
    OscTitle,
    OscProgress,
    BottomLines(usize),
    BottomNonEmptyLines(usize),
    TopNonEmptyLines(usize),
}

impl RegionSpec {
    fn parse(spec: &str) -> Option<Self> {
        let trimmed = spec.trim();
        Some(match trimmed {
            "whole_recent" => Self::WholeRecent,
            "after_last_prompt_marker" => Self::AfterLastPromptMarker,
            "before_current_prompt_marker" => Self::BeforeCurrentPromptMarker,
            "whole_recent_without_current_prompt_marker" => {
                Self::WholeRecentWithoutCurrentPromptMarker
            }
            "current_prompt_block_marker" => Self::CurrentPromptBlockMarker,
            "after_current_prompt_block_marker" => Self::AfterCurrentPromptBlockMarker,
            "prompt_box_body" => Self::PromptBoxBody,
            "above_prompt_box" => Self::AbovePromptBox,
            "last_non_empty_above_prompt_box" => Self::LastNonEmptyAbovePromptBox,
            "after_last_horizontal_rule" => Self::AfterLastHorizontalRule,
            "osc_title" => Self::OscTitle,
            "osc_progress" => Self::OscProgress,
            _ => {
                if let Some(count) = region_count(trimmed, "bottom_lines") {
                    Self::BottomLines(count)
                } else if let Some(count) = region_count(trimmed, "bottom_non_empty_lines") {
                    Self::BottomNonEmptyLines(count)
                } else {
                    Self::TopNonEmptyLines(region_count(trimmed, "top_non_empty_lines")?)
                }
            }
        })
    }

    /// Extract this region without building a line index for a detection tick.
    fn extract<'a>(self, input: DetectionInput<'a>) -> &'a str {
        // OSC regions source from their dedicated fields, not the screen.
        match self {
            Self::OscTitle => return input.osc_title,
            Self::OscProgress => return input.osc_progress,
            Self::WholeRecent => return input.screen,
            Self::AfterLastHorizontalRule => return after_last_horizontal_rule(input.screen),
            _ => {}
        }
        let content = input.screen;
        match self {
            Self::AfterLastPromptMarker => after_last_prompt_marker(content),
            Self::BeforeCurrentPromptMarker => before_current_prompt_marker(content),
            Self::WholeRecentWithoutCurrentPromptMarker => {
                whole_recent_without_current_prompt_marker(content)
            }
            Self::CurrentPromptBlockMarker => current_prompt_block_marker(content).unwrap_or(""),
            Self::AfterCurrentPromptBlockMarker => {
                after_current_prompt_block_marker(content).unwrap_or("")
            }
            Self::PromptBoxBody => prompt_box_body(content).unwrap_or(""),
            Self::AbovePromptBox => above_prompt_box(content),
            Self::LastNonEmptyAbovePromptBox => last_non_empty_line(above_prompt_box(content)),
            Self::BottomLines(count) => bottom_lines(content, count),
            Self::BottomNonEmptyLines(count) => bottom_non_empty_lines(content, count),
            Self::TopNonEmptyLines(count) => top_non_empty_lines(content, count),
            Self::OscTitle
            | Self::OscProgress
            | Self::WholeRecent
            | Self::AfterLastHorizontalRule => "",
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ManifestState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

impl From<ManifestState> for AgentState {
    fn from(value: ManifestState) -> Self {
        match value {
            ManifestState::Idle => AgentState::Idle,
            ManifestState::Working => AgentState::Working,
            ManifestState::Blocked => AgentState::Blocked,
            ManifestState::Unknown => AgentState::Unknown,
        }
    }
}

fn default_region() -> String {
    "whole_recent".to_string()
}

const BUNDLED_MANIFESTS: &[(&str, &str)] = &[
    ("amp", include_str!("manifests/amp.toml")),
    ("agy", include_str!("manifests/antigravity.toml")),
    ("claude", include_str!("manifests/claude.toml")),
    ("cline", include_str!("manifests/cline.toml")),
    ("codex", include_str!("manifests/codex.toml")),
    ("cursor", include_str!("manifests/cursor.toml")),
    ("devin", include_str!("manifests/devin.toml")),
    ("droid", include_str!("manifests/droid.toml")),
    ("gemini", include_str!("manifests/gemini.toml")),
    ("grok", include_str!("manifests/grok.toml")),
    ("kilo", include_str!("manifests/kilo.toml")),
    ("kimi", include_str!("manifests/kimi.toml")),
    ("kiro", include_str!("manifests/kiro.toml")),
    ("letta", include_str!("manifests/letta.toml")),
    ("maki", include_str!("manifests/maki.toml")),
    ("muse", include_str!("manifests/muse.toml")),
    ("opencode", include_str!("manifests/opencode.toml")),
    ("pi", include_str!("manifests/pi.toml")),
    ("qodercli", include_str!("manifests/qodercli.toml")),
    ("qwen", include_str!("manifests/qwen.toml")),
    ("copilot", include_str!("manifests/github-copilot.toml")),
];

/// The process-wide registry. Production code reaches it only through
/// `registry()`; tests build their own `ManifestRegistry` over a private
/// override directory instead, so they never touch this, `XDG_CONFIG_HOME`,
/// or anything else another test running in the same process could observe.
static MANIFESTS: OnceLock<ManifestRegistry> = OnceLock::new();
static MANIFEST_INIT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

const MAX_RULES_PER_MANIFEST: usize = 128;
const MAX_GATE_DEPTH: usize = 8;
const MAX_TOTAL_GATES: usize = 512;
const MAX_MATCHERS_PER_GATE: usize = 32;
/// Distinct regions one manifest may read. Detection caches region texts in a
/// fixed array of this size, so the limit is checked when compiling.
const MAX_REGIONS_PER_MANIFEST: usize = 32;
const MAX_TOTAL_MATCHERS: usize = 1024;
const MAX_MATCHER_CHARS: usize = 512;

/// Loaded manifests for every screen-manifest agent, read from the bundled
/// set plus local overrides in one directory, and swapped wholesale on reload.
#[derive(Debug)]
struct ManifestRegistry {
    cache: RwLock<ManifestCache>,
    /// Serialises reloads so two concurrent reloads cannot interleave their
    /// directory reads and leave the older one installed last.
    reload_lock: Mutex<()>,
}

impl ManifestRegistry {
    fn new(override_dir: Option<&Path>) -> Self {
        Self {
            cache: RwLock::new(build_manifest_cache(override_dir)),
            reload_lock: Mutex::new(()),
        }
    }

    fn reload(&self, override_dir: &Path) -> Vec<AgentManifestSummary> {
        let _reload_guard = self
            .reload_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cache = build_manifest_cache(Some(override_dir));
        let summaries = manifest_summaries_from_cache(&cache);
        match self.cache.write() {
            Ok(mut guard) => *guard = cache,
            Err(poisoned) => *poisoned.into_inner() = cache,
        }
        summaries
    }

    fn get(&self, agent: Agent) -> Option<Arc<LoadedManifest>> {
        let guard = match self.cache.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .manifests
            .iter()
            .find(|(cached_agent, _)| *cached_agent == agent)
            .and_then(|(_, loaded)| loaded.clone())
    }

    fn summaries(&self) -> Vec<AgentManifestSummary> {
        let guard = match self.cache.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        manifest_summaries_from_cache(&guard)
    }
}

/// Reload manifests, reading local overrides from `<config_dir>/agent-detection`.
pub fn reload_manifests(config_dir: &Path) -> Vec<AgentManifestSummary> {
    let override_dir = manifest_override_dir(config_dir);
    if let Some(registry) = MANIFESTS.get() {
        return registry.reload(&override_dir);
    }

    let _init_guard = MANIFEST_INIT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(registry) = MANIFESTS.get() {
        return registry.reload(&override_dir);
    }

    // The first caller knows the config directory, so include overrides in
    // the initial build instead of compiling the bundled set twice.
    MANIFESTS
        .get_or_init(|| ManifestRegistry::new(Some(&override_dir)))
        .summaries()
}

// One shepr process owns one headless server. Bootstrap reloads its resolved
// config before constructing the app (which restores panes) or opening the API
// socket; later reloads use that app's config. CLI file explain builds a
// private registry, so no production consumer reads this cache before reload.
fn registry() -> &'static ManifestRegistry {
    if let Some(registry) = MANIFESTS.get() {
        return registry;
    }
    let _init_guard = MANIFEST_INIT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    MANIFESTS.get_or_init(|| ManifestRegistry::new(None))
}

fn build_manifest_cache(override_dir: Option<&Path>) -> ManifestCache {
    ManifestCache {
        manifests: Agent::screen_manifest_agents()
            .map(|agent| {
                (
                    agent,
                    load_manifest_uncached(agent, override_dir).map(Arc::new),
                )
            })
            .collect(),
    }
}

fn manifest_summaries_from_cache(cache: &ManifestCache) -> Vec<AgentManifestSummary> {
    cache
        .manifests
        .iter()
        .filter_map(|(agent, loaded)| {
            loaded
                .as_deref()
                .map(|loaded| manifest_summary_from_loaded(*agent, loaded))
        })
        .collect()
}

fn manifest_summary_from_loaded(agent: Agent, loaded: &LoadedManifest) -> AgentManifestSummary {
    AgentManifestSummary {
        agent,
        active_source: loaded.source.clone(),
        warning: loaded.warning.clone(),
    }
}

#[cfg(test)]
pub fn detect(agent: Agent, screen_content: &str) -> AgentDetection {
    detect_with_osc(
        agent,
        DetectionInput {
            screen: screen_content,
            osc_title: "",
            osc_progress: "",
        },
    )
}

/// Production detection path. Runs per identified pane on every detection
/// tick, so it evaluates rules in priority order, stops at the first match,
/// and builds none of the evidence `explain` reports.
pub fn detect_with_osc(agent: Agent, input: DetectionInput<'_>) -> AgentDetection {
    detect_with_manifest(agent, input, registry().get(agent).as_deref())
}

fn detect_with_manifest(
    agent: Agent,
    input: DetectionInput<'_>,
    loaded: Option<&LoadedManifest>,
) -> AgentDetection {
    let Some(loaded) = loaded else {
        return fallback_detection(agent, false);
    };
    let mut texts = RegionTexts::new(input);
    for &index in &loaded.priority_order {
        let (Some(rule), Some(compiled)) = (
            loaded.manifest.rules.get(index),
            loaded.compiled_rules.get(index),
        ) else {
            continue;
        };
        if compiled_rule_matches(compiled, &loaded.regions, &mut texts) {
            return rule_detection(rule);
        }
    }
    fallback_detection(agent, true)
}

/// Whether screen detection has a manifest for `agent`. Agents without one
/// (Omp, Mastracode) are only ever reported `Unknown` by the screen, so
/// consumers that wait for a screen-derived `Idle` must not wait on them.
pub fn has_screen_manifest(agent: Agent) -> bool {
    registry().get(agent).is_some()
}

pub fn explain(agent: Agent, screen_content: &str) -> DetectionExplain {
    explain_with_input(
        agent,
        DetectionInput {
            screen: screen_content,
            osc_title: "",
            osc_progress: "",
        },
    )
}

pub fn explain_with_input(agent: Agent, input: DetectionInput<'_>) -> DetectionExplain {
    explain_with_manifest(agent, input, registry().get(agent).as_deref())
}

fn explain_with_manifest(
    agent: Agent,
    input: DetectionInput<'_>,
    loaded: Option<&LoadedManifest>,
) -> DetectionExplain {
    let Some(loaded) = loaded else {
        return fallback_explain(Some(agent), None);
    };
    explain_loaded_manifest(agent, input, loaded)
}

/// Explain a captured screen using manifests loaded from the supplied config directory.
pub fn explain_for_label(
    agent_label: &str,
    screen_content: &str,
    config_dir: &Path,
) -> DetectionExplain {
    let Some(agent) = parse_agent_label(agent_label) else {
        return DetectionExplain {
            agent: Some(agent_label.to_string()),
            state: AgentState::Unknown,
            source: None,
            matched_rule: None,
            screen_detection_skipped: false,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
            skip_state_update: false,
            skipped_update_reason: None,
            fallback_reason: Some("unknown_agent".to_string()),
            evaluated_rules: Vec::new(),
            warning: None,
        };
    };
    let override_dir = manifest_override_dir(config_dir);
    let registry = ManifestRegistry::new(Some(&override_dir));
    explain_with_manifest(
        agent,
        DetectionInput {
            screen: screen_content,
            osc_title: "",
            osc_progress: "",
        },
        registry.get(agent).as_deref(),
    )
}

fn rule_state(rule: &ManifestRule) -> AgentState {
    rule.state.map_or(AgentState::Unknown, AgentState::from)
}

fn rule_detection(rule: &ManifestRule) -> AgentDetection {
    let state = rule_state(rule);
    AgentDetection {
        state,
        skip_state_update: rule.skip_state_update,
        visible_idle: rule.visible_idle && state == AgentState::Idle,
        visible_blocker: rule.visible_blocker && state == AgentState::Blocked,
        visible_working: rule.visible_working && state == AgentState::Working,
    }
}

/// State reported when no rule matched, or the agent has no manifest at all.
///
/// With a manifest, no match means the agent's live chrome shows none of the
/// working/blocked evidence the manifest encodes, which for every agent but
/// Codex is its idle prompt; Codex's no-match screen is ambiguous.
///
/// Without a manifest (Omp, Mastracode) the screen says nothing about the
/// agent's state, so the honest value is `Unknown`; those agents rely on
/// their full-lifecycle hook for state. Two consumers treat that `Unknown` as
/// settled rather than pending: `TerminalState::reconcile_managed_agent_at`
/// lets a managed launch of such an agent become ready on `Unknown` (there
/// is no screen `Idle` to wait for), and `should_skip_idle_screen_scan` in
/// `pane/agent_detection.rs` skips re-reading an unchanged screen for it.
fn fallback_state(agent: Agent, has_manifest: bool) -> AgentState {
    if !has_manifest || agent == Agent::Codex {
        AgentState::Unknown
    } else {
        AgentState::Idle
    }
}

fn fallback_detection(agent: Agent, has_manifest: bool) -> AgentDetection {
    AgentDetection {
        state: fallback_state(agent, has_manifest),
        skip_state_update: false,
        visible_idle: false,
        visible_blocker: false,
        visible_working: false,
    }
}

fn explain_loaded_manifest(
    agent: Agent,
    input: DetectionInput<'_>,
    loaded: &LoadedManifest,
) -> DetectionExplain {
    let mut texts = RegionTexts::new(input);
    let mut matched: Option<&ManifestRule> = None;
    let mut evaluated_rules = Vec::with_capacity(loaded.manifest.rules.len());

    for (rule, compiled_rule) in loaded
        .manifest
        .rules
        .iter()
        .zip(loaded.compiled_rules.iter())
    {
        let matched_rule = compiled_rule_matches(compiled_rule, &loaded.regions, &mut texts);
        let region_text = texts.text(compiled_rule.region);
        evaluated_rules.push(EvaluatedRule {
            id: rule.id.clone(),
            priority: rule.priority,
            region: rule.region.clone(),
            evidence: rule_evidence(rule, region_text),
            state: rule_state(rule),
            matched: matched_rule,
        });

        if !matched_rule {
            continue;
        }

        match matched {
            Some(previous) if previous.priority >= rule.priority => {}
            _ => matched = Some(rule),
        }
    }

    let Some(rule) = matched else {
        return fallback_explain(Some(agent), Some((loaded, evaluated_rules)));
    };

    let detection = rule_detection(rule);
    let skipped_update_reason = rule
        .skip_state_update
        .then(|| format!("matched_rule:{}", rule.id));

    DetectionExplain {
        agent: Some(agent_label(agent).to_string()),
        state: detection.state,
        source: Some(loaded.source.clone()),
        matched_rule: Some(MatchedRule {
            id: rule.id.clone(),
            priority: rule.priority,
            region: rule.region.clone(),
            state: detection.state,
        }),
        screen_detection_skipped: false,
        visible_idle: detection.visible_idle,
        visible_blocker: detection.visible_blocker,
        visible_working: detection.visible_working,
        skip_state_update: detection.skip_state_update,
        skipped_update_reason,
        fallback_reason: None,
        evaluated_rules,
        warning: loaded.warning.clone(),
    }
}

fn fallback_explain(
    agent: Option<Agent>,
    context: Option<(&LoadedManifest, Vec<EvaluatedRule>)>,
) -> DetectionExplain {
    let has_manifest = context.is_some();
    let (source, evaluated_rules, warning) =
        context.map_or((None, Vec::new(), None), |(loaded, evaluated)| {
            (
                Some(loaded.source.clone()),
                evaluated,
                loaded.warning.clone(),
            )
        });

    DetectionExplain {
        agent: agent.map(|agent| agent_label(agent).to_string()),
        state: agent.map_or(AgentState::Unknown, |agent| {
            fallback_state(agent, has_manifest)
        }),
        source,
        matched_rule: None,
        screen_detection_skipped: false,
        visible_idle: false,
        visible_blocker: false,
        visible_working: false,
        skip_state_update: false,
        skipped_update_reason: None,
        fallback_reason: match agent {
            Some(_) if !has_manifest => Some(NO_SCREEN_MANIFEST_FALLBACK.to_string()),
            Some(Agent::Codex) => Some("codex_state_ambiguous".to_string()),
            Some(_) => Some(DEFAULT_KNOWN_AGENT_IDLE_FALLBACK.to_string()),
            None => None,
        },
        evaluated_rules,
        warning,
    }
}

fn load_manifest_uncached(agent: Agent, override_dir: Option<&Path>) -> Option<LoadedManifest> {
    let bundled = bundled_manifest(agent);
    let Some(path) = override_dir.map(|directory| override_path(directory, agent)) else {
        return bundled.and_then(|manifest| bundled_loaded_manifest(agent, manifest));
    };
    // A stat error (EACCES, ELOOP) is not absence: the override is reported
    // as unloadable rather than silently skipped.
    let override_readable = match path.try_exists() {
        Ok(true) => Ok(()),
        Ok(false) => {
            return bundled.and_then(|manifest| bundled_loaded_manifest(agent, manifest));
        }
        Err(err) => Err(err.to_string()),
    };

    let warning = match override_readable.and_then(|()| read_override_manifest(&path)) {
        Ok(manifest) if manifest_matches_agent(&manifest, agent) => {
            match loaded_manifest(manifest, ManifestSource::Override(path.clone())) {
                Ok(loaded) => return Some(loaded),
                Err(err) => format!(
                    "ignored override {} because it could not be compiled: {err}",
                    path.display()
                ),
            }
        }
        Ok(manifest) => format!(
            "ignored override {} because manifest id {} does not match {}",
            path.display(),
            manifest.id,
            agent_label(agent)
        ),
        Err(err) => format!(
            "ignored override {} because it could not be loaded: {err}",
            path.display()
        ),
    };
    let mut loaded = bundled.and_then(|manifest| bundled_loaded_manifest(agent, manifest))?;
    loaded.warning = Some(warning);
    Some(loaded)
}

fn loaded_manifest(
    mut manifest: AgentManifest,
    source: ManifestSource,
) -> Result<LoadedManifest, String> {
    let CompiledManifest {
        compiled_rules,
        regions,
    } = match manifest.compiled.take() {
        Some(compiled) => compiled,
        None => compile_manifest(&manifest)?,
    };
    let mut priority_order: Vec<usize> = (0..manifest.rules.len()).collect();
    // Stable sort: equal priorities keep manifest order, matching the
    // first-wins tie break in `explain_loaded_manifest`.
    priority_order.sort_by_key(|&index| std::cmp::Reverse(manifest.rules[index].priority));
    Ok(LoadedManifest {
        manifest,
        compiled_rules,
        regions,
        priority_order,
        source,
        warning: None,
    })
}

fn bundled_loaded_manifest(agent: Agent, manifest: AgentManifest) -> Option<LoadedManifest> {
    match loaded_manifest(manifest, ManifestSource::Bundled) {
        Ok(loaded) => Some(loaded),
        Err(err) => {
            tracing::error!(agent = agent_label(agent), %err, "bundled manifest could not be compiled");
            None
        }
    }
}

fn bundled_manifest(agent: Agent) -> Option<AgentManifest> {
    let id = agent_label(agent);
    BUNDLED_MANIFESTS
        .iter()
        .find(|(manifest_id, _)| *manifest_id == id)
        .and_then(|(_, content)| match parse_bundled_manifest(id, content) {
            Ok(manifest) => Some(manifest),
            Err(err) => {
                tracing::error!(agent = id, %err, "bundled manifest is invalid");
                None
            }
        })
}

/// Parse a bundled manifest and hold it to the same identity check overrides
/// get: the file's `id` must be the registry key it is filed under.
fn parse_bundled_manifest(key: &str, content: &str) -> Result<AgentManifest, String> {
    let manifest = parse_manifest(content)?;
    if manifest.id != key {
        return Err(format!(
            "manifest id {} does not match registry key {key}",
            manifest.id
        ));
    }
    Ok(manifest)
}

fn read_override_manifest(path: &Path) -> Result<AgentManifest, String> {
    let content = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    parse_manifest(&content)
}

pub fn agent_state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "idle",
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
        AgentState::Unknown => "unknown",
    }
}

pub fn explain_to_json_value(explain: &DetectionExplain) -> serde_json::Value {
    let matched_rule = explain.matched_rule.as_ref().map(|rule| {
        serde_json::json!({
            "id": rule.id,
            "priority": rule.priority,
            "region": rule.region,
            "state": agent_state_label(rule.state),
        })
    });
    let evaluated_rules: Vec<_> = explain
        .evaluated_rules
        .iter()
        .map(|rule| {
            serde_json::json!({
                "id": rule.id,
                "priority": rule.priority,
                "region": rule.region,
                "state": agent_state_label(rule.state),
                "matched": rule.matched,
                "evidence": {
                    "contains": &rule.evidence.contains,
                    "regex": &rule.evidence.regex,
                    "line_regex": &rule.evidence.line_regex,
                    "all_count": rule.evidence.all_count,
                    "any_count": rule.evidence.any_count,
                    "not_count": rule.evidence.not_count,
                    "region_bytes": rule.evidence.region_bytes,
                    "region_preview": &rule.evidence.region_preview,
                },
            })
        })
        .collect();

    serde_json::json!({
        "agent": explain.agent,
        "state": agent_state_label(explain.state),
        "manifest_source": explain.source.as_ref().map(ManifestSource::label),
        "matched_rule": matched_rule,
        "visible_idle": explain.visible_idle,
        "visible_blocker": explain.visible_blocker,
        "visible_working": explain.visible_working,
        "screen_detection_skipped": explain.screen_detection_skipped,
        "skip_state_update": explain.skip_state_update,
        "skipped_update_reason": explain.skipped_update_reason,
        "fallback_reason": explain.fallback_reason,
        "warning": explain.warning,
        "evaluated_rules": evaluated_rules,
    })
}

fn parse_manifest(content: &str) -> Result<AgentManifest, String> {
    let mut manifest = toml::from_str::<AgentManifest>(content).map_err(|err| err.to_string())?;
    manifest.compiled = Some(validate_manifest(&manifest)?);
    Ok(manifest)
}

fn validate_manifest(manifest: &AgentManifest) -> Result<CompiledManifest, String> {
    if manifest.rules.is_empty() {
        return Err("manifest must contain at least one rule".to_string());
    }
    if manifest.rules.len() > MAX_RULES_PER_MANIFEST {
        return Err(format!(
            "manifest contains {} rules, max is {MAX_RULES_PER_MANIFEST}",
            manifest.rules.len()
        ));
    }

    let mut complexity = ManifestComplexity::default();
    for rule in &manifest.rules {
        if rule.id.trim().is_empty() {
            return Err("manifest rule id must not be empty".to_string());
        }
        if rule.skip_state_update {
            if rule.state != Some(ManifestState::Unknown) {
                return Err(format!(
                    "rule {} uses skip_state_update without state = \"unknown\"",
                    rule.id
                ));
            }
            if rule.visible_idle || rule.visible_blocker || rule.visible_working {
                return Err(format!(
                    "rule {} uses skip_state_update with visible state evidence",
                    rule.id
                ));
            }
        }
        validate_region_name(&rule.region)
            .map_err(|err| format!("rule {} uses invalid region: {err}", rule.id))?;
        validate_rule_gate(rule, &mut complexity)
            .map_err(|err| format!("rule {} has invalid matcher gates: {err}", rule.id))?;
    }

    compile_manifest(manifest)
}

#[derive(Default)]
struct ManifestComplexity {
    total_gates: usize,
    total_matchers: usize,
}

fn validate_rule_gate(
    rule: &ManifestRule,
    complexity: &mut ManifestComplexity,
) -> Result<(), String> {
    validate_gate(&manifest_gate_from_rule(rule), "rule", 0, complexity)
}

fn validate_gate_region(gate: &ManifestGate, context: &str) -> Result<(), String> {
    match &gate.region {
        Some(region) => validate_region_name(region)
            .map_err(|err| format!("{context} uses invalid region: {err}")),
        None => Ok(()),
    }
}

fn validate_gate(
    gate: &ManifestGate,
    context: &str,
    depth: usize,
    complexity: &mut ManifestComplexity,
) -> Result<(), String> {
    if depth >= MAX_GATE_DEPTH {
        return Err(format!("{context} exceeds max gate depth {MAX_GATE_DEPTH}"));
    }
    complexity.total_gates += 1;
    if complexity.total_gates > MAX_TOTAL_GATES {
        return Err(format!("manifest exceeds max gate count {MAX_TOTAL_GATES}"));
    }
    validate_gate_region(gate, context)?;
    validate_matcher_limits(gate, context, complexity)?;
    if !gate_has_positive_matcher(gate) {
        return Err(format!("{context} must contain a positive matcher"));
    }
    for nested in &gate.all {
        validate_gate(nested, "all gate", depth + 1, complexity)?;
    }
    for nested in &gate.any {
        validate_gate(nested, "any gate", depth + 1, complexity)?;
    }
    for nested in &gate.not_gate {
        if !gate_has_any_matcher(nested) {
            return Err(format!("{context} contains an empty not gate"));
        }
        validate_not_gate(nested, depth + 1, complexity)?;
    }
    Ok(())
}

fn validate_not_gate(
    gate: &ManifestGate,
    depth: usize,
    complexity: &mut ManifestComplexity,
) -> Result<(), String> {
    if depth >= MAX_GATE_DEPTH {
        return Err(format!("not gate exceeds max gate depth {MAX_GATE_DEPTH}"));
    }
    complexity.total_gates += 1;
    if complexity.total_gates > MAX_TOTAL_GATES {
        return Err(format!("manifest exceeds max gate count {MAX_TOTAL_GATES}"));
    }
    validate_gate_region(gate, "not gate")?;
    validate_matcher_limits(gate, "not gate", complexity)?;
    if !gate_has_any_matcher(gate) {
        return Err("not gate must contain a matcher".to_string());
    }
    for nested in &gate.all {
        validate_gate(nested, "not all gate", depth + 1, complexity)?;
    }
    for nested in &gate.any {
        validate_gate(nested, "not any gate", depth + 1, complexity)?;
    }
    for nested in &gate.not_gate {
        validate_not_gate(nested, depth + 1, complexity)?;
    }
    Ok(())
}

fn validate_matcher_limits(
    gate: &ManifestGate,
    context: &str,
    complexity: &mut ManifestComplexity,
) -> Result<(), String> {
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
    Ok(())
}

fn gate_has_positive_matcher(gate: &ManifestGate) -> bool {
    !gate.contains.is_empty()
        || !gate.regex.is_empty()
        || !gate.line_regex.is_empty()
        || !gate.all.is_empty()
        || !gate.any.is_empty()
}

fn gate_has_any_matcher(gate: &ManifestGate) -> bool {
    gate_has_positive_matcher(gate) || !gate.not_gate.is_empty()
}

fn validate_region_name(spec: &str) -> Result<(), String> {
    RegionSpec::parse(spec)
        .map(|_| ())
        .ok_or_else(|| spec.trim().to_string())
}

fn manifest_override_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("agent-detection")
}

fn override_path(override_dir: &Path, agent: Agent) -> PathBuf {
    override_dir.join(format!("{}.toml", agent_label(agent)))
}

fn manifest_matches_agent(manifest: &AgentManifest, agent: Agent) -> bool {
    let id = agent_label(agent);
    manifest.id == id
        || manifest.aliases.iter().any(|alias| alias == id)
        || parse_agent_label(&manifest.id) == Some(agent)
        || manifest
            .aliases
            .iter()
            .any(|alias| parse_agent_label(alias) == Some(agent))
}

fn manifest_gate_from_rule(rule: &ManifestRule) -> ManifestGate {
    ManifestGate {
        // The rule's own region is applied by the compiler as the root region.
        region: None,
        all: rule.all.clone(),
        any: rule.any.clone(),
        not_gate: rule.not_gate.clone(),
        contains: rule.contains.clone(),
        regex: rule.regex.clone(),
        line_regex: rule.line_regex.clone(),
    }
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

fn compile_manifest(manifest: &AgentManifest) -> Result<CompiledManifest, String> {
    let mut table = RegionTable::default();
    let rules = manifest
        .rules
        .iter()
        .map(|rule| {
            let compile = |table: &mut RegionTable| {
                let region = table.intern(&rule.region)?;
                let gate = compile_gate(&manifest_gate_from_rule(rule), region, table)?;
                let mut regions_used = Vec::new();
                collect_gate_regions(&gate, &mut regions_used);
                Ok::<_, String>(CompiledRule {
                    gate,
                    region,
                    regions_used,
                })
            };
            compile(&mut table)
                .map_err(|err| format!("rule {} could not be compiled: {err}", rule.id))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CompiledManifest {
        compiled_rules: rules,
        regions: table.regions,
    })
}

fn compile_gate(
    gate: &ManifestGate,
    inherited_region: usize,
    table: &mut RegionTable,
) -> Result<CompiledGate, String> {
    let region = match &gate.region {
        Some(spec) => table.intern(spec)?,
        None => inherited_region,
    };
    let mut compile_all = |gates: &[ManifestGate]| {
        gates
            .iter()
            .map(|nested| compile_gate(nested, region, table))
            .collect::<Result<Vec<_>, _>>()
    };
    let all = compile_all(&gate.all)?;
    let any = compile_all(&gate.any)?;
    let not_gate = compile_all(&gate.not_gate)?;
    Ok(CompiledGate {
        region,
        all,
        any,
        not_gate,
        contains: gate
            .contains
            .iter()
            .map(|needle| CompiledContains::new(needle))
            .collect::<Result<_, _>>()?,
        regex: gate
            .regex
            .iter()
            .map(|pattern| {
                Regex::new(pattern)
                    .map_err(|err| format!("invalid regex pattern {pattern:?}: {err}"))
            })
            .collect::<Result<_, _>>()?,
        line_regex: gate
            .line_regex
            .iter()
            .map(|pattern| {
                Regex::new(pattern)
                    .map_err(|err| format!("invalid line_regex pattern {pattern:?}: {err}"))
            })
            .collect::<Result<_, _>>()?,
    })
}

fn collect_gate_regions(gate: &CompiledGate, regions: &mut Vec<usize>) {
    if !regions.contains(&gate.region) {
        regions.push(gate.region);
    }
    for nested in gate.all.iter().chain(&gate.any).chain(&gate.not_gate) {
        collect_gate_regions(nested, regions);
    }
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

fn rule_evidence(rule: &ManifestRule, region_text: &str) -> RuleEvidence {
    RuleEvidence {
        contains: rule.contains.clone(),
        regex: rule.regex.clone(),
        line_regex: rule.line_regex.clone(),
        all_count: rule.all.len(),
        any_count: rule.any.len(),
        not_count: rule.not_gate.len(),
        region_bytes: region_text.len(),
        region_preview: bounded_preview(region_text),
    }
}

fn bounded_preview(text: &str) -> String {
    const MAX_CHARS: usize = 240;
    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(MAX_CHARS).collect();
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
        return regexes
            .iter()
            .all(|regex| text.lines().any(|line| regex.is_match(line)));
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

#[cfg(test)]
fn region<'a>(input: DetectionInput<'a>, spec: &str) -> &'a str {
    RegionSpec::parse(spec).map_or("", |spec| spec.extract(input))
}

const MAX_REGION_LINE_COUNT: usize = u16::MAX as usize;

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
        .filter(|count| (1..=MAX_REGION_LINE_COUNT).contains(count))
}

fn bottom_lines(content: &str, count: usize) -> &str {
    content
        .lines()
        .rev()
        .take(count)
        .last()
        .map_or("", |line| &content[line_start_offset(content, line)..])
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

fn after_last_prompt_marker(content: &str) -> &str {
    let mut after = None;
    for line in content.lines() {
        if codex_prompt_line(line) {
            after = Some(line_end_offset(content, line));
        }
    }
    after.map_or(content, |offset| &content[offset..])
}

fn before_current_prompt_marker(content: &str) -> &str {
    let Some((prompt_start, _)) = current_codex_prompt_parts(content) else {
        return content;
    };
    &content[..prompt_start.min(content.len())]
}

fn whole_recent_without_current_prompt_marker(content: &str) -> &str {
    if current_codex_prompt_parts(content).is_some() {
        ""
    } else {
        content
    }
}

fn current_prompt_block_marker(content: &str) -> Option<&str> {
    current_codex_prompt_parts(content)?.1.map(|(line, _)| line)
}

fn after_current_prompt_block_marker(content: &str) -> Option<&str> {
    let (_, marker) = current_codex_prompt_parts(content)?;
    let (_, marker_start) = marker?;
    Some(&content[marker_start..])
}

/// The current prompt line's start offset and the block marker line (text and
/// start offset) closest above it, in one pass. `None` when there is no
/// prompt line or a block marker follows the last one.
fn current_codex_prompt_parts(content: &str) -> Option<(usize, Option<(&str, usize)>)> {
    let mut prompt_start = None;
    let mut latest_marker = None;
    let mut marker_before_prompt = None;
    let mut marker_after_prompt = false;

    for line in content.lines() {
        let start = line_start_offset(content, line);
        if codex_prompt_line(line) {
            prompt_start = Some(start);
            marker_before_prompt = latest_marker;
            marker_after_prompt = false;
        } else if codex_block_marker_line(line) {
            latest_marker = Some((line, start));
            if prompt_start.is_some() {
                marker_after_prompt = true;
            }
        }
    }

    if marker_after_prompt {
        return None;
    }
    prompt_start.map(|start| (start, marker_before_prompt))
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

fn prompt_box_body(content: &str) -> Option<&str> {
    let (_, top_end, bottom_start) = prompt_box_bounds(content)?;
    Some(&content[top_end.min(content.len())..bottom_start.min(content.len())])
}

fn above_prompt_box(content: &str) -> &str {
    let Some((top_start, _, _)) = prompt_box_bounds(content) else {
        return content;
    };
    &content[..top_start.min(content.len())]
}

fn prompt_box_bounds(content: &str) -> Option<(usize, usize, usize)> {
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

#[cfg(test)]
mod tests;
