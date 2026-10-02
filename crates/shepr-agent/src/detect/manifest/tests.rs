use super::*;

// Codex is only a registry key here; behavior tests supply synthetic rules.
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
    loaded: LoadedManifest,
}

impl TestManifests {
    fn new(content: &str) -> Self {
        let manifest = parse_manifest(content).expect("test precondition");
        Self {
            loaded: loaded_manifest(manifest).expect("test precondition"),
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
        osc_title: "",
        osc_progress: "",
    }
}

fn synthetic_loaded(rules: &str) -> LoadedManifest {
    let manifest = parse_manifest(&rules_manifest(rules)).expect("test precondition");
    loaded_manifest(manifest).expect("test precondition")
}

fn detect_loaded(loaded: &LoadedManifest, input: DetectionInput<'_>) -> Option<String> {
    let mut texts = RegionTexts::new(input);
    loaded
        .priority_order
        .iter()
        .copied()
        .find(|&index| {
            compiled_rule_matches(&loaded.compiled_rules[index], &loaded.regions, &mut texts)
        })
        .map(|index| loaded.manifest.rules[index].id.clone())
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
    assert_eq!(
        loaded.compiled_rules[0].region,
        loaded.compiled_rules[1].region
    );
    let bottom = &loaded.regions[loaded.compiled_rules[0].region];
    let whole = &loaded.regions[loaded.compiled_rules[2].region];
    assert_eq!(bottom.spec, RegionSpec::BottomNonEmptyLines(2));
    assert_eq!(whole.spec, RegionSpec::WholeRecent);
    let contains = &loaded.compiled_rules[1].gate.contains[0];
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
        osc_title: "spin task",
        osc_progress: "",
    };
    assert_eq!(
        detect_loaded(&loaded, working).as_deref(),
        Some("title_working")
    );
    let dialog = DetectionInput {
        screen: "Do you want to proceed?\nEsc to cancel",
        osc_title: "spin task",
        osc_progress: "",
    };
    assert_eq!(detect_loaded(&loaded, dialog), None);
    let stale_dialog = DetectionInput {
        screen: "Do you want to proceed?\nEsc to cancel\nlater output\nmore output",
        osc_title: "spin task",
        osc_progress: "",
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

fn bundled_loaded(agent: Agent) -> LoadedManifest {
    bundled_loaded_manifest(agent, bundled_manifest(agent).expect("test precondition"))
        .expect("test precondition")
}

#[test]
fn claude_title_spinner_stands_down_while_a_permission_dialog_is_live() {
    let claude = bundled_loaded(Agent::Claude);
    let spinner = DetectionInput {
        screen: "some output\n* Thinking… (3s · esc to interrupt)\n",
        osc_title: "\u{2810} Claude Code",
        osc_progress: "",
    };
    let working = explain_loaded_manifest(Agent::Claude, spinner, &claude);
    assert_eq!(working.state, AgentState::Working);
    assert_eq!(
        working.matched_rule.map(|rule| rule.id).as_deref(),
        Some("osc_title_working")
    );

    let dialog = DetectionInput {
        screen: "Bash command\n  rm -rf build\nDo you want to proceed?\n 1. Yes\n  2. No\nEsc to cancel\n",
        osc_title: "\u{2810} Claude Code",
        osc_progress: "",
    };
    let blocked = explain_loaded_manifest(Agent::Claude, dialog, &claude);
    assert_eq!(blocked.state, AgentState::Blocked, "{blocked:?}");
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
        assert_ne!(stale.state, AgentState::Blocked);
        let live = explain_loaded_manifest(
            agent,
            screen_input(
                "△ Permission required\n$ rm -rf build\nAllow once   Allow always   Reject\n",
            ),
            &loaded,
        );
        assert_eq!(live.state, AgentState::Blocked);
    }
}

#[test]
fn codex_no_match_is_unknown_without_changing_other_agents() {
    let manifests = TestManifests::new(&local_manifest("working", "active-marker"));
    let explain = manifests.explain(Agent::Codex, "unmatched-marker");

    assert_eq!(explain.state, AgentState::Unknown);
    assert!(!explain.visible_idle);
    assert_eq!(
        explain.fallback_reason.as_deref(),
        Some(UNKNOWN_MANIFEST_FALLBACK)
    );
    let pi = bundled_loaded(Agent::Pi);
    let other = fallback_explain(Agent::Pi, Some((&pi, Vec::new())));
    assert_eq!(other.state, AgentState::Idle);
    assert_eq!(
        other.fallback_reason.as_deref(),
        Some(DEFAULT_KNOWN_AGENT_IDLE_FALLBACK)
    );
}

#[test]
fn agents_without_a_screen_manifest_are_unknown_not_idle() {
    for agent in [Agent::Omp, Agent::Mastracode] {
        assert!(!agent.screen_manifest());
        assert!(screen_unknown_is_stable(agent));
        let detection = detect_with_manifest(screen_input(" \n"), None);
        assert_eq!(detection.state, AgentState::Unknown);
        assert!(!detection.visible_idle);
        let explain = fallback_explain(agent, None);
        assert_eq!(explain.state, AgentState::Unknown);
        assert_eq!(
            explain.fallback_reason.as_deref(),
            Some(NO_SCREEN_MANIFEST_FALLBACK)
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
    let direct = explain(Agent::Claude, screen);
    assert_eq!(by_label, direct);
    assert_eq!(by_label.state, AgentState::Blocked);

    let unknown = explain_for_label("no-such-agent", screen_input(screen));
    assert_eq!(unknown.state, AgentState::Unknown);
    assert_eq!(unknown.fallback_reason.as_deref(), Some("unknown_agent"));
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
        assert_eq!(high.state, AgentState::Working);
        assert_eq!(
            high.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("high_nested_gates")
        );

        let not_gate = manifests.explain(Agent::Codex, "match win blocked");
        assert_eq!(not_gate.state, AgentState::Idle);
        assert_eq!(
            not_gate.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("low_contains")
        );

        let line = manifests.explain(Agent::Codex, "before\nexact line\nafter");
        assert_eq!(line.state, AgentState::Blocked);
        assert_eq!(
            line.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("line_regex")
        );
    }
}

#[test]
fn bundled_manifests_compile_once_and_are_shared_across_threads() {
    let first = loaded(Agent::Claude).expect("claude has a bundled manifest");
    assert!(!first.compiled_rules.is_empty());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(move || {
                let again = loaded(Agent::Claude).expect("claude has a bundled manifest");
                assert_eq!(
                    first.compiled_rules.as_ptr(),
                    again.compiled_rules.as_ptr(),
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
            ("screen-marker", "", "", AgentState::Idle, "screen"),
            (
                "screen-marker",
                "title-marker",
                "",
                AgentState::Working,
                "title",
            ),
            (
                "screen-marker",
                "title-marker",
                "progress-marker",
                AgentState::Blocked,
                "progress",
            ),
            (
                "screen-marker title-marker progress-marker",
                "",
                "",
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
            assert_eq!(result.state, state);
            assert_eq!(
                result
                    .matched_rule
                    .as_ref()
                    .map(|matched| matched.id.as_str()),
                Some(rule)
            );
            let detection = manifests.detect_input(input);
            assert_eq!(detection.state, state);
            assert_eq!(detection.visible_idle, state == AgentState::Idle);
            assert_eq!(detection.visible_working, state == AgentState::Working);
            assert_eq!(detection.visible_blocker, state == AgentState::Blocked);
        }
        let swapped = manifests.explain_input(
            Agent::Codex,
            DetectionInput {
                screen: "",
                osc_title: "progress-marker",
                osc_progress: "title-marker",
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
        assert_eq!(result.state, AgentState::Unknown);
        assert!(result.skip_state_update);
        assert_eq!(
            result.skipped_update_reason.as_deref(),
            Some("matched_rule:overlay")
        );
        assert!(!result.visible_idle);
        assert!(!result.visible_working);
        assert!(!result.visible_blocker);
        assert!(manifests.detect(screen).skip_state_update);
    }
}

#[test]
fn screen_regions_extract_structure_without_classifying_agent_state() {
    for (screen, spec, expected) in [
        (
            "before\n› input\nafter\n",
            "after_last_prompt_marker",
            "after\n",
        ),
        (
            "before\n› input\nafter\n",
            "before_current_prompt_marker",
            "before\n",
        ),
        (
            "before\n› input\nafter\n",
            "whole_recent_without_current_prompt_marker",
            "",
        ),
        (
            "no marker\n",
            "whole_recent_without_current_prompt_marker",
            "no marker\n",
        ),
        (
            "above\n\n───\nbody\n───\nfooter\n",
            "last_non_empty_above_prompt_box",
            "above",
        ),
        (
            "above\n───\nbody\n───\nfooter\n",
            "prompt_box_body",
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
                    osc_title: "",
                    osc_progress: ""
                },
                spec
            ),
            expected,
            "region={spec}"
        );
    }
}

// Enforcement: keep the descriptor flags and bundled manifest registry aligned.
#[test]
fn all_bundled_manifests_parse_validate_and_compile() {
    for agent in Agent::screen_manifest_agents() {
        assert!(
            bundled_manifest(agent).is_some(),
            "missing bundled manifest for {}",
            agent_label(agent)
        );
    }
    for (key, content) in BUNDLED_MANIFESTS {
        assert!(
            Agent::screen_manifest_agents().any(|agent| agent.label() == *key),
            "bundled manifest {key} has no screen-detection descriptor"
        );
        let manifest = parse_bundled_manifest(key, content)
            .unwrap_or_else(|error| panic!("bundled {key} manifest: {error}"));
        if let Err(error) = loaded_manifest(manifest) {
            panic!("bundled {key} manifest could not be compiled: {error}");
        }
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
                osc_title: "",
                osc_progress: ""
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
                osc_title: "",
                osc_progress: ""
            },
            "top_non_empty_lines(2)"
        ),
        "\nmarker\nold\n"
    );
}

#[test]
fn manifest_validation_rejects_invalid_counted_line_regions() {
    for name in ["bottom_non_empty_lines", "top_non_empty_lines"] {
        assert!(validate_region_name(&format!("{name}(1)")).is_ok());
        assert!(validate_region_name(&format!("{name}({})", u16::MAX)).is_ok());
        for count in ["0", "00", "01", "+1", "65536", "999999999999999999999999"] {
            assert!(
                validate_region_name(&format!("{name}({count})")).is_err(),
                "{name} accepted invalid count {count}"
            );
        }
    }
}
