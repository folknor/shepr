use super::*;
use shepr_vt::{Progress, ProgressState};

// Synthetic manifests use the Codex label; behavior tests supply their own rules.
fn local_manifest(state: &str, contains: &str) -> String {
    format!(
        r#"
id = "codex"
fallback = "unknown"

[[rules]]
id = "test"
state = "{state}"
contains = ["{contains}"]
"#
    )
}

fn rules_manifest(rules: &str) -> String {
    format!(
        r#"
id = "codex"

{rules}
"#
    )
}

/// A synthetic manifest compiled in memory and evaluated directly, so rule
/// behaviour tests never read or change the process-wide bundled set that
/// other tests in the same process see.
struct TestManifests {
    loaded: CompiledManifest,
}

impl TestManifests {
    fn new(content: &str) -> Self {
        Self {
            loaded: parse_manifest(content).expect("test precondition"),
        }
    }

    fn explain(&self, agent: Agent, screen: &str) -> DetectionExplain {
        self.explain_input(agent, screen_input(screen))
    }

    fn explain_input(&self, agent: Agent, input: DetectionInput<'_>) -> DetectionExplain {
        explain_with_manifest(agent, input, Some(&self.loaded))
    }

    fn detect(&self, screen: &str) -> AgentDetection {
        self.detect_input(screen_input(screen))
    }

    fn detect_input(&self, input: DetectionInput<'_>) -> AgentDetection {
        detect_with_manifest(input, Some(&self.loaded))
    }
}

fn screen_input(screen: &str) -> DetectionInput<'_> {
    DetectionInput {
        screen,
        osc_title: None,
        osc_progress: None,
    }
}

fn synthetic_loaded(rules: &str) -> CompiledManifest {
    parse_manifest(&rules_manifest(rules)).expect("test precondition")
}

fn detect_loaded(loaded: &CompiledManifest, input: DetectionInput<'_>) -> Option<String> {
    let mut texts = RegionTexts::new(input);
    loaded
        .priority_order
        .iter()
        .copied()
        .find(|&index| compiled_rule_matches(&loaded.rules[index], &loaded.regions, &mut texts))
        .map(|index| loaded.rules[index].id.clone())
}

#[test]
fn priority_ordered_detection_agrees_with_full_explain() {
    let loaded = synthetic_loaded(
        r#"
[[rules]]
id = "first_tie"
state = "idle"
priority = 5
contains = ["shared"]

[[rules]]
id = "second_tie"
state = "working"
priority = 5
contains = ["shared"]

[[rules]]
id = "high"
state = "blocked"
priority = 50
region = "bottom_non_empty_lines(1)"
contains = ["HIGH"]

[[rules]]
id = "low"
state = "working"
priority = -3
regex = ['low']
"#,
    );
    for screen in [
        "shared",
        "shared\nhigh",
        "high\nshared",
        "low",
        "nothing",
        "",
        "low shared",
    ] {
        let explained = explain_loaded_manifest(Agent::Codex, screen_input(screen), &loaded);
        assert_eq!(
            detect_loaded(&loaded, screen_input(screen)),
            explained.matched_rule.map(|rule| rule.id),
            "screen={screen:?}"
        );
    }
    assert_eq!(
        detect_loaded(&loaded, screen_input("shared")).as_deref(),
        Some("first_tie")
    );
    let higher_priority =
        explain_loaded_manifest(Agent::Codex, screen_input("shared\nhigh"), &loaded);
    assert_eq!(
        higher_priority
            .matched_rule
            .as_ref()
            .map(|rule| rule.id.as_str()),
        Some("high")
    );
    assert_eq!(
        higher_priority.evaluated_rules[2].evidence.contains,
        vec!["HIGH".to_string()]
    );
}

#[test]
fn distinct_regions_are_interned_and_contains_needles_are_prepared_at_load() {
    let loaded = synthetic_loaded(
        r#"
[[rules]]
id = "a"
state = "idle"
region = "bottom_non_empty_lines(2)"
regex = ['x']

[[rules]]
id = "b"
state = "working"
region = "bottom_non_empty_lines(2)"
contains = ["y"]

[[rules]]
id = "c"
state = "blocked"
regex = ['z']
"#,
    );
    assert_eq!(loaded.regions.len(), 2);
    assert_eq!(loaded.rules[0].region_index, loaded.rules[1].region_index);
    let bottom = &loaded.regions[loaded.rules[0].region_index];
    let whole = &loaded.regions[loaded.rules[2].region_index];
    assert_eq!(bottom.spec, RegionSpec::BottomNonEmptyLines(2));
    assert_eq!(whole.spec, RegionSpec::WholeRecent);
    let contains = &loaded.rules[1].gate.contains[0];
    assert!(contains.matches("Y"));
    assert!(!contains.matches("x"));
}

#[test]
fn contains_matches_keep_unicode_lowercase_semantics() {
    for (text, needle) in [
        ("İstanbul", "İSTANBUL"),
        ("STRAẞE", "straße"),
        ("ΟΣ", "ος"),
        ("Kelvin", "kelvin"),
        ("\u{039F}\u{03A3}", "\u{03BF}\u{03C3}"),
        ("\u{03A3}\u{0391}", "\u{03C3}\u{03B1}"),
        ("\u{03A3}\u{0391}", "\u{03C2}\u{03B1}"),
        ("\u{0391} \u{03A3}", "\u{03C3}"),
        ("\u{0391}\u{03A3}'.", "\u{03C2}'"),
        ("\u{0391}\u{03A3}'\u{0392}", "\u{03C3}'\u{03B2}"),
        ("\u{212A}elvin", "kelvin"),
        ("Esc TO\nInterrupt", "to\ninterrupt"),
        ("Esc TO\r\nInterrupt", "to\ninterrupt"),
        ("aaab", "aab"),
        ("ABABAC", "abac"),
        ("anything", ""),
    ] {
        let expected = text.to_lowercase().contains(&needle.to_lowercase());
        let compiled = CompiledContains::new(needle).expect("Unicode property regexes compile");
        assert_eq!(
            compiled.matches(text),
            expected,
            "{text:?} contains {needle:?}"
        );
    }
}

#[test]
fn gate_region_reads_a_different_input_than_its_rule() {
    let loaded = synthetic_loaded(
        r#"
[[rules]]
id = "title_working"
state = "working"
priority = 10
region = "osc_title"
regex = ['^spin ']
not = [
  { region = "bottom_non_empty_lines(2)", contains = ["esc to cancel"], any = [
    { contains = ["do you want to proceed?"] },
  ] },
]
"#,
    );
    let working = DetectionInput {
        screen: "output\nesc to interrupt",
        osc_title: Some("spin task"),
        osc_progress: None,
    };
    assert_eq!(
        detect_loaded(&loaded, working).as_deref(),
        Some("title_working")
    );
    let dialog = DetectionInput {
        screen: "Do you want to proceed?\nEsc to cancel",
        osc_title: Some("spin task"),
        osc_progress: None,
    };
    assert_eq!(detect_loaded(&loaded, dialog), None);
    let stale_dialog = DetectionInput {
        screen: "Do you want to proceed?\nEsc to cancel\nlater output\nmore output",
        osc_title: Some("spin task"),
        osc_progress: None,
    };
    assert_eq!(
        detect_loaded(&loaded, stale_dialog).as_deref(),
        Some("title_working")
    );
    assert!(
        parse_manifest(&rules_manifest(
            r#"
[[rules]]
id = "bad_gate_region"
state = "working"
contains = ["x"]
all = [{ region = "bottom_lines", contains = ["y"] }]
"#
        ))
        .is_err()
    );
}

fn bundled_loaded(agent: Agent) -> CompiledManifest {
    bundled_manifest(agent).expect("test precondition")
}

fn named_class_matches(matcher: &str, class: &str, text: &str) -> bool {
    let source = format!(
        "id = \"codex\"\n\n[[rules]]\nid = \"named_class\"\nstate = \"working\"\n{matcher} = ['^{class}$']\n"
    );
    TestManifests::new(&source).detect(text).state() == AgentState::Working
}

#[test]
fn named_manifest_regex_classes_preserve_their_intended_glyph_sets() {
    for matcher in ["regex", "line_regex"] {
        assert!(named_class_matches(matcher, "{spinner}", "\u{2801}"));
        assert!(named_class_matches(matcher, "{spinner}", "\u{28FF}"));
        assert!(!named_class_matches(matcher, "{spinner}", "\u{2800}"));
        assert!(named_class_matches(
            matcher,
            "{spinner_run}",
            "\u{2800}\u{2801}"
        ));
        assert!(named_class_matches(matcher, "{spinner_run}", "\u{28FF}"));
        assert!(!named_class_matches(
            matcher,
            "{spinner_run}",
            "\u{2800}\u{2800}"
        ));
    }
    for frame in "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".chars() {
        assert!(named_class_matches(
            "regex",
            "{dots_spinner}",
            &frame.to_string()
        ));
    }
    assert!(!named_class_matches("regex", "{dots_spinner}", "\u{2801}"));

    assert!(named_class_matches(
        "regex",
        "{claude_live_glyph}",
        "\u{2733}"
    ));
    assert!(named_class_matches("regex", "{claude_live_glyph}", "*"));
    assert!(!named_class_matches("regex", "{claude_live_glyph}", "◐"));
    assert!(named_class_matches(
        "regex",
        "{claude_background_glyph}",
        "*"
    ));
    assert!(!named_class_matches(
        "regex",
        "{claude_background_glyph}",
        "\u{2733}"
    ));
}

#[test]
fn bundled_manifests_do_not_spell_braille_ranges_directly() {
    // One Braille codepoint in any spelling a manifest pattern can carry: a
    // regex `\x{28..}`, `\u{28..}` or `\u28..` escape, or the literal glyph.
    let braille = r"(?:\\x\{28[0-9A-Fa-f]{2}\}|\\u\{28[0-9A-Fa-f]{2}\}|\\u28[0-9A-Fa-f]{2}|[\x{2800}-\x{28FF}])";
    let raw_braille_range =
        Regex::new(&format!(r"{braille}\s*-\s*{braille}")).expect("lint expression compiles");
    for spelling in [
        r"[\x{2800}-\x{28FF}]",
        r"[\x{2801}-\x{28ff}]",
        concat!("[", "\\", "u2800-", "\\", "u28FF]"),
        r"[\u{2800}-\u{28FF}]",
        "[\u{2801}-\u{28FF}]",
    ] {
        assert!(raw_braille_range.is_match(spelling), "{spelling}");
    }

    for agent in Agent::all() {
        let Some(source) = bundled_manifest_source(agent) else {
            continue;
        };
        assert!(
            !raw_braille_range.is_match(source),
            "{} manifest contains a raw Braille range",
            agent.label()
        );
    }
}

fn collect_osc_progress_regexes<'a>(
    gate: &'a CompiledGate,
    regions: &[CompiledRegion],
    patterns: &mut Vec<(&'a str, &'a Regex)>,
) {
    if regions[gate.region].spec == RegionSpec::OscProgress {
        patterns.extend(gate.regex.iter().map(|regex| ("regex", regex)));
        patterns.extend(gate.line_regex.iter().map(|regex| ("line_regex", regex)));
    }
    for nested in gate.all.iter().chain(&gate.any).chain(&gate.not_gate) {
        collect_osc_progress_regexes(nested, regions, patterns);
    }
}

#[test]
fn bundled_osc_progress_regexes_match_progress_display_output() {
    let mut progress_spellings = Vec::new();
    for state in [
        ProgressState::Remove,
        ProgressState::Normal,
        ProgressState::Error,
        ProgressState::Indeterminate,
        ProgressState::Paused,
    ] {
        for percent in [None, Some(0)] {
            progress_spellings.push(Progress { state, percent }.to_string());
        }
    }

    let mut checked = 0;
    for agent in Agent::all() {
        let Some(source) = bundled_manifest_source(agent) else {
            continue;
        };
        let manifest = parse_bundled_manifest(agent.label(), source)
            .unwrap_or_else(|error| panic!("bundled {} manifest: {error}", agent.label()));
        for rule in &manifest.rules {
            let mut patterns = Vec::new();
            collect_osc_progress_regexes(&rule.gate, &manifest.regions, &mut patterns);
            for (kind, regex) in patterns {
                checked += 1;
                assert!(
                    progress_spellings
                        .iter()
                        .any(|spelling| regex.is_match(spelling)),
                    "{} rule {} {kind} {:?} matches no shepr-vt Progress display output",
                    agent.label(),
                    rule.id,
                    regex.as_str()
                );
            }
        }
    }
    assert!(checked > 0, "bundled OSC progress matchers are present");
}

#[test]
fn claude_title_spinner_stands_down_while_a_permission_dialog_is_live() {
    let claude = bundled_loaded(Agent::Claude);
    let spinner = DetectionInput {
        screen: "some output\n* Thinking… (3s · esc to interrupt)\n",
        osc_title: Some("\u{2810} Claude Code"),
        osc_progress: None,
    };
    let working = explain_loaded_manifest(Agent::Claude, spinner, &claude);
    assert_eq!(working.verdict.state(), AgentState::Working);
    assert_eq!(
        working.matched_rule.map(|rule| rule.id).as_deref(),
        Some("osc_title_working")
    );

    let dialog = DetectionInput {
        screen: "Bash command\n  rm -rf build\nDo you want to proceed?\n 1. Yes\n  2. No\nEsc to cancel\n",
        osc_title: Some("\u{2810} Claude Code"),
        osc_progress: None,
    };
    let blocked = explain_loaded_manifest(Agent::Claude, dialog, &claude);
    assert_eq!(blocked.verdict.state(), AgentState::Blocked, "{blocked:?}");
}

#[test]
fn opencode_permission_header_needs_live_dialog_controls() {
    for agent in [Agent::OpenCode, Agent::Kilo] {
        let loaded = bundled_loaded(agent);
        let stale = explain_loaded_manifest(
            agent,
            screen_input("△ Permission required\nearlier text only\n"),
            &loaded,
        );
        assert_ne!(stale.verdict.state(), AgentState::Blocked);
        let live = explain_loaded_manifest(
            agent,
            screen_input(
                "△ Permission required\n$ rm -rf build\nAllow once   Allow always   Reject\n",
            ),
            &loaded,
        );
        assert_eq!(live.verdict.state(), AgentState::Blocked);
    }
}

/// "Reject" and "Allow always" lead to follow-up screens of the same pending
/// request that drop the "Permission required" header; each blocks only with
/// its own controls live.
#[test]
fn opencode_permission_follow_up_screens_need_their_own_controls() {
    // Each agent's own wording of the dialog bodies, which the gates do not read.
    for (agent, rule, name, always_until) in [
        (
            Agent::OpenCode,
            "permission_required",
            "OpenCode",
            "until OpenCode is restarted",
        ),
        (Agent::Kilo, "opencode_permission", "Kilo", "permanently"),
    ] {
        let loaded = bundled_loaded(agent);
        for live in [
            format!(
                "△ Reject permission\nTell {name} what to do differently\n\
                 \n  enter confirm  esc cancel\n"
            ),
            format!(
                "△ Always allow\nThis will allow bash {always_until}.\n\
                 \n  Confirm   Cancel                    ⇆ select  enter confirm\n"
            ),
        ] {
            let blocked = explain_loaded_manifest(agent, screen_input(&live), &loaded);
            assert_eq!(
                blocked.verdict.state(),
                AgentState::Blocked,
                "{agent:?} {live:?}"
            );
            assert_eq!(
                blocked.matched_rule.map(|matched| matched.id).as_deref(),
                Some(rule)
            );
        }
        for stale in [
            "△ Reject permission\nearlier text only\n",
            "△ Reject permission\nlater output\n  enter confirm\n",
            "△ Always allow\nearlier text only\n",
            "△ Always allow\nConfirm\nCancel\n",
            "△ Always allow\nlater output\n  enter confirm\n",
        ] {
            let explain = explain_loaded_manifest(agent, screen_input(stale), &loaded);
            assert_ne!(
                explain.verdict.state(),
                AgentState::Blocked,
                "{agent:?} {stale:?}"
            );
        }
    }
}

#[test]
fn codex_no_match_is_unknown_without_changing_other_agents() {
    let manifests = TestManifests::new(&local_manifest("working", "active-marker"));
    let explain = manifests.explain(Agent::Codex, "unmatched-marker");

    assert_eq!(explain.verdict.state(), AgentState::Unknown);
    assert!(!explain.verdict.visible_idle());
    assert_eq!(
        explain.fallback_reason,
        Some(FallbackReason::ManifestUnknownFallback)
    );
    let pi = bundled_loaded(Agent::Pi);
    let other = fallback_explain(Agent::Pi, Some((&pi, Vec::new())));
    assert_eq!(other.verdict.state(), AgentState::Idle);
    assert_eq!(
        other.fallback_reason,
        Some(FallbackReason::DefaultKnownAgentIdleFallback)
    );
}

#[test]
fn agents_without_a_screen_manifest_are_unknown_not_idle() {
    for agent in [Agent::Omp, Agent::Mastracode] {
        assert!(bundled_manifest_source(agent).is_none());
        assert!(screen_unknown_is_stable(agent));
        let detection = detect_with_manifest(screen_input(" \n"), None);
        assert_eq!(detection.state(), AgentState::Unknown);
        assert!(!detection.visible_idle());
        let explain = fallback_explain(agent, None);
        assert_eq!(explain.verdict.state(), AgentState::Unknown);
        assert_eq!(
            explain.fallback_reason,
            Some(FallbackReason::NoScreenManifest)
        );
    }
    assert!(screen_unknown_is_stable(Agent::Codex));
    assert!(screen_unknown_is_stable(Agent::Letta));
    assert!(!screen_unknown_is_stable(Agent::Gemini));
}

#[test]
fn explain_for_label_evaluates_the_bundled_manifest_and_names_an_unknown_label() {
    let screen =
        "Bash command\n  rm -rf build\nDo you want to proceed?\n 1. Yes\n  2. No\nEsc to cancel\n";
    let by_label = explain_for_label("claude", screen_input(screen));
    let direct = explain_with_input(Agent::Claude, screen_input(screen));
    assert_eq!(by_label, direct);
    assert_eq!(by_label.verdict.state(), AgentState::Blocked);

    let unknown = explain_for_label("no-such-agent", screen_input(screen));
    assert_eq!(unknown.verdict.state(), AgentState::Unknown);
    assert_eq!(unknown.fallback_reason, Some(FallbackReason::UnknownAgent));
}

#[test]
fn rule_semantics_apply_gates_priority_and_line_regex() {
    {
        let manifests = TestManifests::new(&rules_manifest(
            r#"
[[rules]]
id = "low_contains"
state = "idle"
priority = 1
contains = ["match"]

[[rules]]
id = "high_nested_gates"
state = "working"
priority = 10
contains = ["match"]
all = [
  { any = [{ regex = ["w[io]n"] }, { contains = ["fallback"] }] },
]
not = [
  { contains = ["blocked"] },
]

[[rules]]
id = "line_regex"
state = "blocked"
priority = 20
line_regex = ["^exact line$", "^before$"]
"#,
        ));

        let high = manifests.explain(Agent::Codex, "match win");
        assert_eq!(high.verdict.state(), AgentState::Working);
        assert_eq!(
            high.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("high_nested_gates")
        );

        let not_gate = manifests.explain(Agent::Codex, "match win blocked");
        assert_eq!(not_gate.verdict.state(), AgentState::Idle);
        assert_eq!(
            not_gate.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("low_contains")
        );

        let line = manifests.explain(Agent::Codex, "before\nexact line\nafter");
        assert_eq!(line.verdict.state(), AgentState::Blocked);
        assert_eq!(
            line.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("line_regex")
        );
    }
}

#[test]
fn bundled_manifests_compile_once_and_are_shared_across_threads() {
    let first = loaded(Agent::Claude).expect("claude has a bundled manifest");
    assert!(!first.rules.is_empty());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(move || {
                let again = loaded(Agent::Claude).expect("claude has a bundled manifest");
                assert_eq!(
                    first.rules.as_ptr(),
                    again.rules.as_ptr(),
                    "every caller must share one compiled rule set and its regex search caches"
                );
            });
        }
    });
}

#[test]
fn osc_regions_use_separate_inputs_and_share_rule_priority() {
    {
        let manifests = TestManifests::new(&rules_manifest(
            r#"
[[rules]]
id = "screen"
state = "idle"
priority = 10
region = "whole_recent"
visible_idle = true
contains = ["screen-marker"]

[[rules]]
id = "title"
state = "working"
priority = 20
region = "osc_title"
visible_working = true
regex = ['^title-marker$']

[[rules]]
id = "progress"
state = "blocked"
priority = 30
region = "osc_progress"
visible_blocker = true
regex = ['^progress-marker$']
"#,
        ));
        for (screen, title, progress, state, rule) in [
            ("screen-marker", None, None, AgentState::Idle, "screen"),
            (
                "screen-marker",
                Some("title-marker"),
                None,
                AgentState::Working,
                "title",
            ),
            (
                "screen-marker",
                Some("title-marker"),
                Some("progress-marker"),
                AgentState::Blocked,
                "progress",
            ),
            (
                "screen-marker title-marker progress-marker",
                None,
                None,
                AgentState::Idle,
                "screen",
            ),
        ] {
            let input = DetectionInput {
                screen,
                osc_title: title,
                osc_progress: progress,
            };
            let result = manifests.explain_input(Agent::Codex, input);
            assert_eq!(result.verdict.state(), state);
            assert_eq!(
                result
                    .matched_rule
                    .as_ref()
                    .map(|matched| matched.id.as_str()),
                Some(rule)
            );
            let detection = manifests.detect_input(input);
            assert_eq!(detection.state(), state);
            assert_eq!(detection.visible_idle(), state == AgentState::Idle);
            assert_eq!(detection.visible_working(), state == AgentState::Working);
            assert_eq!(detection.visible_blocker(), state == AgentState::Blocked);
        }
        let swapped = manifests.explain_input(
            Agent::Codex,
            DetectionInput {
                screen: "",
                osc_title: Some("progress-marker"),
                osc_progress: Some("title-marker"),
            },
        );
        assert!(swapped.matched_rule.is_none());
    }
}

#[test]
fn skip_rule_suppresses_state_update_without_visible_state_evidence() {
    {
        let manifests = TestManifests::new(&rules_manifest(
            r#"
[[rules]]
id = "activity"
state = "working"
priority = 10
visible_working = true
contains = ["activity-marker"]

[[rules]]
id = "overlay"
state = "unknown"
priority = 20
skip_state_update = true
contains = ["overlay-marker"]
"#,
        ));
        let screen = "activity-marker overlay-marker";
        let result = manifests.explain(Agent::Codex, screen);
        assert_eq!(result.verdict.state(), AgentState::Unknown);
        assert!(result.verdict.skip_state_update());
        assert_eq!(
            result.skipped_update_reason,
            Some(SkippedUpdateReason::MatchedRule {
                rule_id: "overlay".into()
            })
        );
        assert!(!result.verdict.visible_idle());
        assert!(!result.verdict.visible_working());
        assert!(!result.verdict.visible_blocker());
        assert!(manifests.detect(screen).skip_state_update());
    }
}

#[test]
fn screen_regions_extract_structure_without_classifying_agent_state() {
    for (screen, spec, expected) in [
        (
            "before\n› input\nafter\n",
            "codex_after_last_prompt_marker",
            "after\n",
        ),
        (
            "before\n› input\nafter\n",
            "codex_before_current_prompt_marker",
            "before\n",
        ),
        (
            "before\n› input\nafter\n",
            "codex_whole_recent_without_current_prompt_marker",
            "",
        ),
        (
            "no marker\n",
            "codex_whole_recent_without_current_prompt_marker",
            "no marker\n",
        ),
        (
            "above\n\n───\nbody\n───\nfooter\n",
            "claude_last_non_empty_above_prompt_box",
            "above",
        ),
        (
            "above\n───\nbody\n───\nfooter\n",
            "claude_prompt_box_body",
            "body\n",
        ),
        (
            "above\n───\nbody\n───\nfooter\n",
            "after_last_horizontal_rule",
            "footer\n",
        ),
    ] {
        assert_eq!(
            region(
                DetectionInput {
                    screen,
                    osc_title: None,
                    osc_progress: None
                },
                spec
            ),
            expected,
            "region={spec}"
        );
    }
}

// Enforcement: every bundled manifest parses and compiles.
#[test]
fn all_bundled_manifests_parse_validate_and_compile() {
    for agent in Agent::all() {
        let Some(content) = bundled_manifest_source(agent) else {
            assert!(screen_manifest_agents().all(|candidate| candidate != agent));
            continue;
        };
        assert!(
            bundled_manifest(agent).is_some(),
            "missing compiled manifest for {}",
            agent.label()
        );
        let manifest = parse_bundled_manifest(agent.label(), content)
            .unwrap_or_else(|error| panic!("bundled {} manifest: {error}", agent.label()));
        assert!(
            !manifest.rules.is_empty(),
            "bundled {} has no rules",
            agent.label()
        );
    }
    assert!(parse_bundled_manifest("claude", &local_manifest("idle", "x")).is_err());
}

#[test]
fn manifest_validation_rejects_unknown_fields_empty_rules_invalid_regions_and_regexes() {
    assert!(
        parse_manifest(
            r#"
id = "codex"

[[rules]]
id = "typo"
state = "working"
contain = ["Working"]
"#
        )
        .is_err()
    );
    assert!(
        parse_manifest(
            r#"
id = "codex"

[[rules]]
id = "empty"
state = "working"
"#
        )
        .is_err()
    );
    assert!(
        parse_manifest(
            r#"
id = "codex"

[[rules]]
id = "bad_region"
state = "working"
region = "after_last_promt_marker"
contains = ["Working"]
"#
        )
        .is_err()
    );
    assert!(
        parse_manifest(
            r#"
id = "codex"

[[rules]]
id = "bad_regex"
state = "working"
regex = ["["]
"#
        )
        .is_err()
    );
    assert!(
        parse_manifest(
            r#"
id = "codex"

[[rules]]
id = "bad_nested_regex"
state = "working"
any = [{ line_regex = ["["] }]
"#
        )
        .is_err()
    );
}

#[test]
fn manifest_validation_keeps_skip_rules_neutral() {
    assert!(
        parse_manifest(
            r#"
id = "codex"

[[rules]]
id = "bad_skip_state"
state = "idle"
skip_state_update = true
contains = ["menu"]
"#
        )
        .is_err()
    );
    assert!(
        parse_manifest(
            r#"
id = "codex"

[[rules]]
id = "bad_skip_visible"
state = "unknown"
skip_state_update = true
visible_blocker = true
contains = ["menu"]
"#
        )
        .is_err()
    );
}

#[test]
fn manifest_validation_rejects_excessive_rule_count() {
    let mut manifest = String::from(
        r#"
id = "codex"
"#,
    );
    for index in 0..129 {
        manifest.push_str(&format!(
            r#"
[[rules]]
id = "rule_{index}"
state = "idle"
contains = ["ready"]
"#
        ));
    }
    assert!(parse_manifest(&manifest).is_err());
}

#[test]
fn manifest_validation_caps_gate_depth_at_eight_levels() {
    fn manifest_with_nested_gates(operator: &str, nested_levels: usize) -> String {
        let mut nested = r#"{ contains = ["leaf"] }"#.to_string();
        for _ in 1..nested_levels {
            nested = format!(r#"{{ contains = ["nested"], {operator} = [{nested}] }}"#);
        }
        format!(
            r#"
id = "codex"

[[rules]]
id = "deep"
state = "idle"
contains = ["ready"]
{operator} = [{nested}]
"#
        )
    }

    for operator in ["all", "not"] {
        let at_limit = manifest_with_nested_gates(operator, MAX_GATE_DEPTH - 1);
        assert!(parse_manifest(&at_limit).is_ok(), "operator={operator}");

        let over_limit = manifest_with_nested_gates(operator, MAX_GATE_DEPTH);
        assert!(parse_manifest(&over_limit).is_err(), "operator={operator}");
    }
}

#[test]
fn manifest_validation_rejects_excessive_matchers() {
    let matchers = (0..33)
        .map(|index| format!(r#""m{index}""#))
        .collect::<Vec<_>>()
        .join(", ");
    let manifest = format!(
        r#"
id = "codex"

[[rules]]
id = "many"
state = "idle"
contains = [{matchers}]
"#
    );
    assert!(parse_manifest(&manifest).is_err());
}

#[test]
fn bottom_non_empty_lines_uses_bottom_occurrence_for_repeated_text() {
    let content = "marker\nold\n\nmiddle\nmarker\nnew\n";
    assert_eq!(
        region(
            DetectionInput {
                screen: content,
                osc_title: None,
                osc_progress: None
            },
            "bottom_non_empty_lines(2)"
        ),
        "marker\nnew\n"
    );
}

#[test]
fn top_non_empty_lines_uses_top_occurrence_for_repeated_text() {
    let content = "\nmarker\nold\n\nmiddle\nmarker\nnew\n";
    assert_eq!(
        region(
            DetectionInput {
                screen: content,
                osc_title: None,
                osc_progress: None
            },
            "top_non_empty_lines(2)"
        ),
        "\nmarker\nold\n"
    );
}

#[test]
fn manifest_validation_rejects_invalid_counted_line_regions() {
    for name in ["bottom_non_empty_lines", "top_non_empty_lines"] {
        assert!(RegionSpec::parse(&format!("{name}(1)")).is_some());
        assert!(RegionSpec::parse(&format!("{name}({})", u16::MAX)).is_some());
        for count in ["0", "00", "01", "+1", "65536", "999999999999999999999999"] {
            assert!(
                RegionSpec::parse(&format!("{name}({count})")).is_none(),
                "{name} accepted invalid count {count}"
            );
        }
    }
}

#[test]
fn explained_regions_use_the_parsed_canonical_spelling() {
    let mut source = local_manifest("working", "active-marker");
    source.push_str("\nregion = \"  whole_recent  \"\n");
    let manifests = TestManifests::new(&source);
    let explain = manifests.explain(Agent::Codex, "active-marker");
    assert_eq!(
        explain
            .matched_rule
            .expect("matched rule")
            .region
            .to_string(),
        "whole_recent"
    );
    assert!(
        explain
            .evaluated_rules
            .iter()
            .all(|rule| rule.region.to_string() == "whole_recent")
    );
}
