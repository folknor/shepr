use shepr_test_fixtures::ValidatedConfigFixture as _;

const MAXIMAL_CONFIG: &str = r##"
[theme]
name = "tokyo-night"

[theme.custom]
accent = "#89b4fa"
panel_bg = "#111111"
sidebar_bg = "#222222"
active_row_bg = "#333333"
selection_bg = "#444444"
surface0 = "#555555"
surface1 = "#666666"
surface_dim = "#777777"
overlay0 = "#888888"
overlay1 = "#999999"
text = "#aaaaaa"
subtext0 = "#bbbbbb"
mauve = "#cccccc"
green = "#dddddd"
yellow = "#eeeeee"
red = "#123456"
blue = "#234567"
teal = "#345678"
peach = "#456789"

[terminal]
default_shell = "/bin/sh"
login_shell = true
new_cwd = "__NEW_CWD__"

[session]
resume_agents_on_restore = false
startup_per_agent_delay_ms = 17

[server]
headless_cols = 132
headless_rows = 42

[keys]
prefix = "ctrl+a"
help = ["prefix+?", "ctrl+h"]

[ui]
sidebar_width = 30
sidebar_min_width = 20
sidebar_max_width = 40
sidebar_start_collapsed = true
sidebar_collapsed_mode = "__COLLAPSED_MODE__"
mouse_capture = false
copy_on_select = false
host_cursor = "__HOST_CURSOR__"
right_click_passthrough_modifier = "__RIGHT_CLICK__"
redraw_on_focus_gained = false
mouse_scroll_lines = 7
confirm_close = false
prompt_new_tab_name = false
prompt_new_workspace_name = true
pane_borders = "__PANE_BORDERS__"
pane_outer_borders = false
pane_scrollbars = false
pane_gaps = false
show_agent_labels_on_pane_borders = true
hide_tab_bar_when_single_tab = true
tab_bar_position = "__TAB_BAR_POSITION__"
tab_bar_right = [
    { type = "zoom" },
    { type = "hostname" },
    { type = "datetime", format = "%Y-%m-%d" },
    { type = "text", text = "builder" },
    { type = "command", command = "printf ready", interval_seconds = 7, timeout_seconds = 3 },
]
tab_bar_right_separator = " | "
window_title = "{hostname}: {workspace} {tab} {pane} {terminal_title}"
agent_panel_sort = "__AGENT_SORT__"
status_indicators = "__STATUS_INDICATORS__"
accent = "#f5c2e7"

[ui.sidebar.agents]
rows = [["state_icon", "state_text", "machine", "workspace", "tab", "pane", "agent", "terminal_title", "terminal_title_stripped"], [{ token = "workspace", fg = "#112233", bold = false, dim = true, rules = [{ equals = "main", fg = "#abcdef", bold = true }, { contains = "dev", ignore_case = true, hide = false }, { starts_with = "prod", fg = "#123456" }, { gt = 2.5, dim = true }, { lt = 10.0, hide = true }] }]]
row_gap = 2

[ui.sidebar.agents.rows_by_agent]
claude = [[{ token = "agent", fg = "#abcdef", dim = false }]]

[ui.sidebar.spaces]
rows = [["state_icon", "state_text", "workspace", "branch", "git_status"], [{ token = "branch", fg = "#abcdef", bold = true }]]
row_gap = 3

[advanced]
scrollback_limit_bytes = 123456

[experimental]
allow_nested = true
pane_history = true
reveal_hidden_cursor_for_cjk_ime = true
cjk_ime_agents = ["claude", "codex"]
cjk_ime_cursor_shape = "__IME_CURSOR_SHAPE__"

[remote]
manage_ssh_config = false
"##;

#[test]
fn maximal_validated_config_codec_round_trips_wire_variants() {
    let env = shepr_test_support::IsolatedEnv::new();
    let new_cwd_policies = ["follow", "home", "current", "."];
    let right_click_modifiers = ["off", "ctrl", "alt", "ctrl+alt"];
    let ime_cursor_shapes = [
        "block",
        "steady_block",
        "underline",
        "steady_underline",
        "bar",
        "steady_bar",
    ];
    let host_cursor_modes = ["native", "drawn"];
    let collapsed_modes = ["compact", "hidden"];
    let pane_border_modes = ["auto", "always", "off"];
    let tab_bar_positions = ["top", "bottom"];
    let agent_sorts = ["spaces", "priority"];
    let status_indicators = ["dots", "symbols"];
    let default_paths = shepr_config::AppPaths::resolve().expect("isolated default paths resolve");

    env.set(
        shepr_core::env::EnvVar::SheprSocketPath,
        env.path().join("codec-api.sock"),
    );
    let api_override_paths =
        shepr_config::AppPaths::resolve().expect("API socket override resolves");
    env.remove(shepr_core::env::EnvVar::SheprSocketPath);

    env.set(
        shepr_core::env::EnvVar::SheprClientSocketPath,
        env.path().join("codec-client.sock"),
    );
    let client_override_paths =
        shepr_config::AppPaths::resolve().expect("client socket override resolves");
    env.remove(shepr_core::env::EnvVar::SheprClientSocketPath);
    let path_variants = [default_paths, api_override_paths, client_override_paths];

    for index in 0..ime_cursor_shapes.len() {
        let source = MAXIMAL_CONFIG
            .replace(
                "__NEW_CWD__",
                new_cwd_policies[index % new_cwd_policies.len()],
            )
            .replace(
                "__RIGHT_CLICK__",
                right_click_modifiers[index % right_click_modifiers.len()],
            )
            .replace("__IME_CURSOR_SHAPE__", ime_cursor_shapes[index])
            .replace(
                "__HOST_CURSOR__",
                host_cursor_modes[index % host_cursor_modes.len()],
            )
            .replace(
                "__COLLAPSED_MODE__",
                collapsed_modes[index % collapsed_modes.len()],
            )
            .replace(
                "__PANE_BORDERS__",
                pane_border_modes[index % pane_border_modes.len()],
            )
            .replace(
                "__TAB_BAR_POSITION__",
                tab_bar_positions[index % tab_bar_positions.len()],
            )
            .replace("__AGENT_SORT__", agent_sorts[index % agent_sorts.len()])
            .replace(
                "__STATUS_INDICATORS__",
                status_indicators[index % status_indicators.len()],
            );
        let config = toml::from_str::<shepr_config::Config>(&source)
            .expect("maximal test configuration parses");
        let validated = shepr_config::ValidatedConfig::test_from_config_with_paths(
            config,
            Some(&source),
            path_variants[index % path_variants.len()].clone(),
        );
        assert!(
            validated
                .provenance()
                .values()
                .iter()
                .any(|origin| { origin.source == shepr_config::ConfigSource::Default })
        );
        assert!(
            validated
                .provenance()
                .values()
                .iter()
                .any(|origin| { origin.source == shepr_config::ConfigSource::ConfigFileKey })
        );
        assert!(matches!(
            &validated.paths().provenance().home_dir,
            shepr_config::ConfigSource::EnvironmentVariable(_)
        ));

        let encoded = encode(&validated);
        let decoded =
            shepr_protocol::codec::from_slice_exact::<shepr_config::ValidatedConfig>(&encoded)
                .expect("config decodes");
        assert_eq!(decoded, validated);
        assert_eq!(encode(&decoded), encoded);
    }
}

fn encode(config: &shepr_config::ValidatedConfig) -> Vec<u8> {
    let mut encoded = Vec::new();
    shepr_protocol::codec::encode_into(&mut encoded, config).expect("config encodes");
    encoded
}
