use std::path::Path;

use shepr_paths::AppPaths;

use super::validated::{validate_client, validate_server};
use super::{
    ClientConfig, ConfigDiagnostic, ConfigKeyPath, ConfigKeyPathSegment, ConfigProvenance,
    ServerConfig, ValidatedClientConfig, ValidatedServerConfig,
};

/// Strip the single UTF-8 byte-order mark TOML tolerates at the very start of
/// the document. A U+FEFF anywhere else is left for the TOML parser to judge,
/// so a file it rejects fails the launch.
fn normalize_utf8_bom(content: &str) -> String {
    content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .to_owned()
}

fn read_optional_config(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(normalize_utf8_bom(&content))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// One role's config file, read and deserialized but not yet validated. A
/// missing file is the empty document: the role's defaults with nothing
/// configured.
#[derive(Debug)]
struct Document<C> {
    config: C,
    provenance: ConfigProvenance,
    /// Unknown keys and sections. They fail the load like a validation
    /// problem, and are reported ahead of the role's own diagnostics.
    unknown: Vec<ConfigDiagnostic>,
}

/// One program's config file type, paired with the other program's, so a key
/// unknown here can be checked against the file that does read it.
trait Role: Default + serde::de::DeserializeOwned {
    type Other: serde::de::DeserializeOwned;
}

impl Role for ClientConfig {
    type Other = ServerConfig;
}

impl Role for ServerConfig {
    type Other = ClientConfig;
}

impl<C: Role> Document<C> {
    /// `other_file` is the other program's config file, named by an unknown
    /// key or section that program reads.
    fn read(path: &Path, other_file: &Path) -> Result<Self, Vec<ConfigDiagnostic>> {
        match read_optional_config(path) {
            Ok(Some(content)) => Self::parse(&content, other_file),
            Ok(None) => Ok(Self {
                config: C::default(),
                provenance: ConfigProvenance::from_document(None),
                unknown: Vec::new(),
            }),
            Err(error) => Err(vec![ConfigDiagnostic::read(error.to_string())]),
        }
    }

    fn parse(content: &str, other_file: &Path) -> Result<Self, Vec<ConfigDiagnostic>> {
        let table = content
            .parse::<toml::Table>()
            .map_err(|error| vec![ConfigDiagnostic::parse(error.to_string())])?;
        let document = toml::Value::Table(table);
        let (config, ignored_keys) = deserialize_with_ignored::<C, _>(document.clone())
            .map_err(|error| vec![ConfigDiagnostic::parse(error.to_string())])?;
        let provenance = ConfigProvenance::from_document(Some(&document));
        let (unknown_sections, mut unknown) = unknown_top_level_sections(&document, &ignored_keys);
        unknown.extend(unknown_config_key_diagnostics(
            ignored_keys
                .into_iter()
                .filter(|path| {
                    !matches!(path.segments(), [ConfigKeyPathSegment::Key(key)] if unknown_sections.contains(key))
                })
                .collect(),
        ));
        let unknown = unknown
            .into_iter()
            .map(|diagnostic| match diagnostic.key() {
                Some(key) if read_by::<C::Other>(&document, key) => {
                    diagnostic.belonging_in(other_file)
                }
                _ => diagnostic,
            })
            .collect();
        Ok(Self {
            config,
            provenance,
            unknown,
        })
    }
}

impl<C> Document<C> {
    /// The role's validated config, or every problem the document has: its
    /// unknown keys first, then what the role's validation reported.
    fn validate<V>(
        self,
        validate: impl FnOnce(&C, &ConfigProvenance) -> Result<V, Vec<ConfigDiagnostic>>,
    ) -> Result<V, Vec<ConfigDiagnostic>> {
        let Self {
            config,
            provenance,
            mut unknown,
        } = self;
        match validate(&config, &provenance) {
            Ok(validated) if unknown.is_empty() => Ok(validated),
            Ok(_) => Err(unknown),
            Err(mut diagnostics) => {
                unknown.append(&mut diagnostics);
                Err(unknown)
            }
        }
    }
}

impl Document<ClientConfig> {
    fn validate_client(
        self,
        paths: &AppPaths,
    ) -> Result<ValidatedClientConfig, Vec<ConfigDiagnostic>> {
        self.validate(|config, provenance| validate_client(config, provenance, paths.clone()))
    }
}

impl Document<ServerConfig> {
    fn validate_server(
        self,
        paths: &AppPaths,
    ) -> Result<ValidatedServerConfig, Vec<ConfigDiagnostic>> {
        self.validate(|config, _| validate_server(config, paths.clone()))
    }
}

fn in_file(diagnostics: Vec<ConfigDiagnostic>, path: &Path) -> Vec<ConfigDiagnostic> {
    diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.with_file(path))
        .collect()
}

/// Read and validate `client.toml` from `paths`' config directory.
pub fn load_client_validated(
    paths: &AppPaths,
) -> Result<ValidatedClientConfig, Vec<ConfigDiagnostic>> {
    let path = paths.client_config_file();
    Document::<ClientConfig>::read(&path, &paths.server_config_file())
        .and_then(|document| document.validate_client(paths))
        .map_err(|diagnostics| in_file(diagnostics, &path))
}

/// Read and validate `server.toml` from `paths`' config directory.
pub fn load_server_validated(
    paths: &AppPaths,
) -> Result<ValidatedServerConfig, Vec<ConfigDiagnostic>> {
    let path = paths.server_config_file();
    Document::<ServerConfig>::read(&path, &paths.client_config_file())
        .and_then(|document| document.validate_server(paths))
        .map_err(|diagnostics| in_file(diagnostics, &path))
}

fn unknown_top_level_sections(
    document: &toml::Value,
    ignored_paths: &[ConfigKeyPath],
) -> (std::collections::BTreeSet<String>, Vec<ConfigDiagnostic>) {
    let Some(table) = document.as_table() else {
        return (std::collections::BTreeSet::new(), Vec::new());
    };
    let mut keys = Vec::new();
    let mut diagnostics = Vec::new();
    for path in ignored_paths {
        let [ConfigKeyPathSegment::Key(key)] = path.segments() else {
            continue;
        };
        let Some(value) = table.get(key) else {
            continue;
        };
        if let Some(array_table) = unknown_top_level_section_diagnostic(value) {
            keys.push(key.clone());
            diagnostics.push(ConfigDiagnostic::unknown_section(path.clone(), array_table));
        }
    }
    (keys.into_iter().collect(), diagnostics)
}

fn unknown_top_level_section_diagnostic(value: &toml::Value) -> Option<bool> {
    if value.is_table() {
        Some(false)
    } else if value
        .as_array()
        .is_some_and(|items| !items.is_empty() && items.iter().all(toml::Value::is_table))
    {
        Some(true)
    } else {
        None
    }
}

/// Whether config `O` reads the setting at `key`, with the value it has in
/// `document`: the setting alone, deserialized as `O`, leaves nothing ignored
/// at, above or inside it, so a section is read only when every key in it
/// is. A value `O` would refuse, or an array entry missing a field `O`
/// requires, is no evidence either way and reads as not read.
fn read_by<O: serde::de::DeserializeOwned>(document: &toml::Value, key: &ConfigKeyPath) -> bool {
    let Some(isolated) = isolate(document, key.segments()) else {
        return false;
    };
    // Isolation leaves only the path to `key` and what lies under it, so
    // anything ignored is on that path or inside it.
    deserialize_with_ignored::<O, _>(isolated).is_ok_and(|(_, ignored)| ignored.is_empty())
}

/// `value` cut down to the one setting at `segments`, with every table and
/// array on the way to it holding only that path.
fn isolate(value: &toml::Value, segments: &[ConfigKeyPathSegment]) -> Option<toml::Value> {
    let Some((first, rest)) = segments.split_first() else {
        return Some(value.clone());
    };
    match first {
        ConfigKeyPathSegment::Key(key) => {
            let child = isolate(value.as_table()?.get(key)?, rest)?;
            let mut table = toml::Table::new();
            table.insert(key.clone(), child);
            Some(toml::Value::Table(table))
        }
        ConfigKeyPathSegment::Index(index) => Some(toml::Value::Array(vec![isolate(
            value.as_array()?.get(*index)?,
            rest,
        )?])),
    }
}

fn config_key_path(path: &serde_ignored::Path<'_>) -> ConfigKeyPath {
    match path {
        serde_ignored::Path::Root => ConfigKeyPath::root(),
        serde_ignored::Path::Seq { parent, index } => config_key_path(parent).index(*index),
        serde_ignored::Path::Map { parent, key } => config_key_path(parent).key(key.clone()),
        serde_ignored::Path::Some { parent }
        | serde_ignored::Path::NewtypeStruct { parent }
        | serde_ignored::Path::NewtypeVariant { parent } => config_key_path(parent),
    }
}

fn unknown_config_key_diagnostics(mut paths: Vec<ConfigKeyPath>) -> Vec<ConfigDiagnostic> {
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(ConfigDiagnostic::unknown_key)
        .collect()
}

fn deserialize_with_ignored<'de, T, D>(deserializer: D) -> Result<(T, Vec<ConfigKeyPath>), D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    let mut ignored = Vec::new();
    let value = serde_ignored::deserialize(deserializer, |path| {
        ignored.push(config_key_path(&path));
    })?;
    Ok((value, ignored))
}

#[cfg(test)]
mod tests {
    use shepr_core::env::EnvVar;

    use super::*;

    fn client_document(content: &str) -> Result<Document<ClientConfig>, Vec<ConfigDiagnostic>> {
        Document::<ClientConfig>::parse(content, &crate::test_paths().server_config_file())
    }

    fn client_from_str(content: &str) -> Result<ValidatedClientConfig, Vec<ConfigDiagnostic>> {
        client_document(content).and_then(|document| document.validate_client(&crate::test_paths()))
    }

    fn server_from_str(content: &str) -> Result<ValidatedServerConfig, Vec<ConfigDiagnostic>> {
        let paths = crate::test_paths();
        Document::<ServerConfig>::parse(content, &paths.client_config_file())
            .and_then(|document| document.validate_server(&paths))
    }

    /// The file each unknown key or section of `errors` is said to belong
    /// in, `None` for one no other program reads.
    fn belongs_in(errors: &[ConfigDiagnostic]) -> Vec<Option<std::path::PathBuf>> {
        errors
            .iter()
            .filter_map(|error| match error.kind() {
                super::super::ConfigDiagnosticKind::UnknownKey { belongs_in }
                | super::super::ConfigDiagnosticKind::UnknownSection { belongs_in, .. } => {
                    Some(belongs_in.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// A setting in the other program's file fails this launch and names the
    /// file it belongs in; a retired one fails both and names neither.
    #[test]
    fn misplaced_settings_and_retired_settings_fail_only_the_owning_launch() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let paths = crate::test_paths();
        let client_file = Some(paths.client_config_file());
        let server_file = Some(paths.server_config_file());
        for source in [
            "[terminal]\ndefault_shell = '/bin/sh'\n",
            "[session]\nresume_agents_on_restore = true\n",
            "[server]\nheadless_cols = 120\n",
            "[advanced]\nscrollback_limit_bytes = 1000\n",
            "[experimental]\nreveal_hidden_cursor_for_cjk_ime = true\n",
            "[experimental]\ncjk_ime_agents = ['codex']\n",
            "[experimental]\ncjk_ime_cursor_shape = 'bar'\n",
            "[ui]\npane_scrollbars = true\n",
            "[ui]\npane_gaps = true\n",
        ] {
            assert!(server_from_str(source).is_ok(), "{source}");
            let errors = client_from_str(source).expect_err("server setting in client file");
            assert_eq!(
                belongs_in(&errors),
                std::slice::from_ref(&server_file),
                "{source}"
            );
        }
        for source in [
            "[[machines]]\nlabel = 'build'\nssh = 'build'\npalette = 'green'\n",
            "[keys]\nprefix = 'ctrl+b'\n",
            "[local]\nlabel = 'desk'\n",
            "[ui]\nmouse_capture = false\n",
            "[ui]\nsidebar_width = 26\n",
            "[ui.sidebar.spaces]\nrows = [['workspace']]\n",
        ] {
            assert!(client_from_str(source).is_ok(), "{source}");
            let errors = server_from_str(source).expect_err("client setting in server file");
            assert_eq!(
                belongs_in(&errors),
                std::slice::from_ref(&client_file),
                "{source}"
            );
        }
        for source in [
            "[experimental]\nallow_nested = true\n",
            "[experimental]\npane_history = true\n",
            "[ui]\naccent = 'cyan'\n",
            "[ui]\nwindow_title = 'shepr'\n",
            "[ui]\nshow_agent_labels_on_pane_borders = true\n",
            "[theme]\nname = 'nord'\n",
            "[retired]\nanything = 1\n",
        ] {
            let errors = client_from_str(source).expect_err("retired in client file");
            assert_eq!(belongs_in(&errors), [None], "{source}");
            let errors = server_from_str(source).expect_err("retired in server file");
            assert_eq!(belongs_in(&errors), [None], "{source}");
        }

        // Only the misplaced key of a section is sent on; a value the other
        // program would refuse names no file.
        let errors = client_from_str("[ui]\npane_gaps = true\nmouse_captur = true\n")
            .expect_err("one misplaced and one unknown key");
        assert_eq!(belongs_in(&errors), [None, server_file.clone()]);
        let errors = client_from_str("[ui]\npane_gaps = 'sometimes'\n")
            .expect_err("a server key with a value the server refuses");
        assert_eq!(belongs_in(&errors), [None]);

        let errors = client_from_str("[terminal]\ndefault_shell = '/bin/sh'\n")
            .expect_err("a server section in the client file");
        assert_eq!(
            errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [format!(
                "unknown config section [terminal]; it belongs in {}",
                paths.server_config_file().display()
            )]
        );
    }

    #[test]
    fn each_program_reads_only_its_file_and_never_the_retired_file() {
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("role-config-load");
        let paths = AppPaths::rooted_at(scratch.path(), Some(scratch.path()), Some(scratch.path()))
            .expect("scratch roots fit a socket");
        std::fs::create_dir_all(paths.config_dir()).expect("create config directory");
        std::fs::write(paths.config_dir().join("config.toml"), "broken = [")
            .expect("retired file fixture");
        std::fs::write(paths.server_config_file(), "broken = [").expect("broken server fixture");
        env.set(EnvVar::Shell, scratch.join("missing/wrapper"));
        assert!(
            load_client_validated(&paths).is_ok(),
            "client must not read server config or SHELL"
        );
        let errors = load_server_validated(&paths).expect_err("server parses its file");
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("server.toml"))
        );
        assert!(errors.iter().all(|error| {
            error
                .to_string()
                .lines()
                .next()
                .is_some_and(|line| line.contains("server.toml"))
        }));
        std::fs::write(
            paths.server_config_file(),
            "[terminal]\ndefault_shell = '/bin/sh'\n",
        )
        .expect("valid server fixture");
        std::fs::write(paths.client_config_file(), "broken = [").expect("broken client fixture");
        assert!(
            load_server_validated(&paths).is_ok(),
            "server must not read client config"
        );
        let errors = load_client_validated(&paths).expect_err("client parses its file");
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("client.toml"))
        );
        assert!(errors.iter().all(|error| {
            error
                .to_string()
                .lines()
                .next()
                .is_some_and(|line| line.contains("client.toml"))
        }));
        std::fs::remove_file(paths.client_config_file()).expect("remove client fixture");
        assert!(load_client_validated(&paths).is_ok());
    }

    #[test]
    fn server_launch_collects_grid_and_terminal_errors() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let errors = server_from_str("[server]\nheadless_cols = 0\n[terminal]\ndefault_shell = '/missing/zsh'\nnew_cwd = 'missing'\n")
            .expect_err("invalid server settings");
        for setting in [
            "server.headless_cols",
            "terminal.default_shell",
            "terminal.new_cwd",
        ] {
            assert!(
                errors
                    .iter()
                    .any(|error| error.to_string().contains(setting)),
                "{setting}: {errors:?}"
            );
        }
    }

    /// A document that does not parse reports only its parse error: nothing
    /// validates the placeholder a failed parse would leave behind.
    #[test]
    fn load_diagnostics_keep_their_kind() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let parse = client_from_str("[keys\nprefix = 'ctrl+a'");
        assert!(matches!(parse, Err(ref errors)
            if matches!(errors.as_slice(), [diagnostic]
                if matches!(diagnostic.kind(), super::super::ConfigDiagnosticKind::Parse(_)))));

        let unknown = client_from_str("[keys]\nunknown_binding = 'ctrl+a'");
        assert!(matches!(unknown, Err(ref errors)
            if matches!(errors.as_slice(), [diagnostic]
                if matches!(diagnostic.kind(),
                    super::super::ConfigDiagnosticKind::UnknownKey { .. }
                        | super::super::ConfigDiagnosticKind::UnknownSection { .. }))));

        let invalid = client_from_str("[keys]\nprefix = 'ctrl+'");
        assert!(invalid.is_err());
    }

    #[test]
    fn config_load_reports_unreadable_path() {
        let _env = shepr_test_support::IsolatedEnv::new();
        // A directory where the config file should be cannot be read.
        let scratch = shepr_test_support::ScratchDir::new("config");
        let other = scratch.join("other.toml");
        assert!(Document::<ServerConfig>::read(scratch.path(), &other).is_err());
        assert!(
            Document::<ClientConfig>::read(scratch.path(), &other).is_err_and(|diagnostics| {
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.to_string().contains("config read error"))
            })
        );
    }

    #[test]
    fn validated_config_rejects_parse_and_semantic_errors() {
        let _env = shepr_test_support::IsolatedEnv::new();
        for (content, message) in [
            ("[keys]\nprefix = \"ctrl+\"\n", "keys.prefix"),
            ("[keys]\nzoom = \"prefix+nonsense\"\n", "keys.zoom"),
            (
                "[ui]\nwindow_title = \"shepr\"\n",
                "unknown config key ui.window_title",
            ),
            (
                "[ui]\nsidebar_min_width = 50\nsidebar_max_width = 30\n",
                "sidebar_min_width",
            ),
            ("[ui]\nsidebar_width = 17\n", "ui.sidebar_width"),
            ("[ui]\nsidebar_width = 37\n", "ui.sidebar_width"),
            ("[server]\nheadless_cols = 0\n", "headless_cols"),
            (
                "[server]\nheadless_cols = 4097\nheadless_rows = 1\n",
                "server.headless_cols",
            ),
            (
                "[server]\nheadless_cols = 4096\nheadless_rows = 1025\n",
                "server.headless_cols",
            ),
            (
                "[ui]\ntab_bar_right = []\n",
                "unknown config key ui.tab_bar_right",
            ),
            (
                "[keys]\nnext_tab = \"prefix+n\"\n",
                "unknown config key keys.next_tab",
            ),
            (
                "[ui]\nmouse_captur = true\n",
                "unknown config key ui.mouse_captur",
            ),
        ] {
            let errors = if content.contains("[server]") || content.contains("window_title") {
                server_from_str(content).expect_err("invalid server config")
            } else {
                client_from_str(content).expect_err("invalid client config")
            };
            assert!(
                errors
                    .iter()
                    .any(|diagnostic| diagnostic.to_string().contains(message)),
                "expected {message:?} in {errors:?}"
            );
        }

        assert!(server_from_str("[server]\nheadless_cols = \"wide\"\n").is_err());
    }

    #[test]
    fn the_removed_pane_border_keys_are_unknown_in_server_toml() {
        let _env = shepr_test_support::IsolatedEnv::new();
        for key in ["pane_borders", "pane_outer_borders"] {
            let errors = server_from_str(&format!("[ui]\n{key} = true\n"))
                .expect_err("a removed key fails the launch");
            let message = format!("unknown config key ui.{key}");
            assert!(
                errors
                    .iter()
                    .any(|diagnostic| diagnostic.to_string().contains(&message)),
                "expected {message:?} in {errors:?}"
            );
        }
    }

    #[test]
    fn machines_load_in_config_order() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let validated = client_from_str(
            r#"
[[machines]]
label = "build"
ssh = "dev@build"
palette = "green"

[[machines]]
label = "gpu"
ssh = "ssh://gpu.example"
palette = "cyan"
"#,
        )
        .expect("valid machines load");
        let machines: Vec<_> = validated
            .machines()
            .iter()
            .map(|machine| (machine.label.as_str(), machine.ssh.as_str()))
            .collect();
        assert_eq!(
            machines,
            [("build", "dev@build"), ("gpu", "ssh://gpu.example")]
        );

        let none = client_from_str("").expect("no machines is valid");
        assert!(none.machines().is_empty());
        assert_eq!(none.local_hue(), crate::DEFAULT_LOCAL_HUE);
    }

    /// The local server is named by `local.label`, or by this host's short
    /// hostname, which at most one machine label may match apart from ASCII
    /// case. "Local" is an ordinary name.
    #[test]
    fn the_local_server_is_named_by_its_label_or_the_hostname_and_skips_its_own_entry() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let labelled = client_from_str("[local]\nlabel = \"desk\"\n").expect("a local label loads");
        assert_eq!(labelled.local_label().as_str(), "desk");

        let hostname = shepr_platform::host_names().expect("the test host has a name");
        let unset = client_from_str("").expect("the hostname names the local server");
        assert_eq!(unset.local_label().as_str(), hostname.short());

        // One file listing every host: this host's own entry is skipped and
        // gives the local server its hue.
        let shared = client_from_str(&format!(
            "[[machines]]\nlabel = \"{}\"\nssh = \"h\"\npalette = \"green\"\n\
             [[machines]]\nlabel = \"other\"\nssh = \"o\"\npalette = \"red\"\n",
            hostname.short().to_ascii_uppercase()
        ))
        .expect("this host's own entry is skipped");
        let labels: Vec<_> = shared
            .machines()
            .iter()
            .map(|machine| machine.label.as_str())
            .collect();
        assert_eq!(labels, ["other"]);
        assert_eq!(shared.local_hue(), shepr_term::host_tint::HostHue::Green);

        client_from_str(
            "[local]\nlabel = \"desk\"\n\
             [[machines]]\nlabel = \"Local\"\nssh = \"h\"\npalette = \"red\"\n",
        )
        .expect("Local is not reserved");
    }

    #[test]
    fn every_machine_takes_a_palette() {
        use shepr_term::host_tint::HostHue;

        let _env = shepr_test_support::IsolatedEnv::new();
        let validated = client_from_str(
            r#"
[[machines]]
label = "build"
ssh = "dev@build"
palette = "green"

[[machines]]
label = "gpu"
ssh = "gpu"
palette = "purple"
"#,
        )
        .expect("palettes load");
        let palettes: Vec<_> = validated
            .machines()
            .iter()
            .map(|machine| machine.palette)
            .collect();
        assert_eq!(palettes, [HostHue::Green, HostHue::Purple]);

        for hue in HostHue::ALL {
            let source = format!(
                "[[machines]]\nlabel = \"a\"\nssh = \"h\"\npalette = \"{}\"\n",
                hue.name()
            );
            let validated = client_from_str(&source).expect("every hue name loads");
            assert_eq!(validated.machines()[0].palette, hue, "{source}");
        }

        let errors = client_from_str("[[machines]]\nlabel = \"a\"\nssh = \"h\"\n")
            .expect_err("a machine without a palette");
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("palette")),
            "{errors:?}"
        );
    }

    #[test]
    fn an_unknown_palette_or_local_key_fails_the_launch() {
        let _env = shepr_test_support::IsolatedEnv::new();
        for (content, message) in [
            (
                "[[machines]]\nlabel = \"a\"\nssh = \"h\"\npalette = \"pink\"\n",
                "pink",
            ),
            (
                "[[machines]]\nlabel = \"a\"\nssh = \"h\"\npalette = \"Green\"\n",
                "Green",
            ),
            (
                "[[machines]]\nlabel = \"a\"\nssh = \"h\"\npalette = 3\n",
                "invalid type",
            ),
            (
                "[local]\npalette = \"green\"\n",
                "unknown config key local.palette",
            ),
            (
                "[local]\nname = \"desk\"\n",
                "unknown config key local.name",
            ),
        ] {
            let errors = client_from_str(content).expect_err("invalid palette must not launch");
            assert!(
                errors
                    .iter()
                    .any(|diagnostic| diagnostic.to_string().contains(message)),
                "expected {message:?} in {errors:?}"
            );
        }
        let errors = server_from_str("[local]\nlabel = \"desk\"\n")
            .expect_err("the server file has no local table");
        assert_eq!(
            belongs_in(&errors),
            [Some(crate::test_paths().client_config_file())]
        );
    }

    #[test]
    fn invalid_machines_fail_the_launch() {
        let _env = shepr_test_support::IsolatedEnv::new();
        for (content, message) in [
            (
                "[[machines]]\nlabel = \"a\"\nssh = \"h1\"\npalette = \"red\"\n\
                 [[machines]]\nlabel = \"a\"\nssh = \"h2\"\npalette = \"red\"\n",
                "duplicates an earlier machine (related: machines[0].label)",
            ),
            (
                "[[machines]]\nlabel = \"Build\"\nssh = \"h1\"\npalette = \"red\"\n\
                 [[machines]]\nlabel = \"build\"\nssh = \"h2\"\npalette = \"red\"\n",
                "duplicates an earlier machine (related: machines[0].label)",
            ),
            (
                "[local]\nlabel = \"desk\"\n\
                 [[machines]]\nlabel = \"desk\"\nssh = \"h1\"\npalette = \"green\"\n\
                 [[machines]]\nlabel = \"DESK\"\nssh = \"h2\"\npalette = \"red\"\n",
                "duplicates an earlier machine (related: machines[0].label)",
            ),
            (
                "[[machines]]\nlabel = \"  \"\nssh = \"h\"\npalette = \"red\"\n",
                "machine label must not be blank",
            ),
            (
                "[local]\nlabel = \" desk\"\n",
                "machine label must not start or end with whitespace",
            ),
            (
                "[[machines]]\nlabel = \"a\"\nssh = \"-oProxyCommand=x\"\npalette = \"red\"\n",
                "must not start with",
            ),
            (
                "[[machines]]\nlabel = \"a\"\nssh = \"\"\npalette = \"red\"\n",
                "SSH target must not be empty",
            ),
            (
                "[[machines]]\nlabel = \"a\"\nssh = \"u:p@h\"\npalette = \"red\"\n",
                "must not contain a password",
            ),
            ("[[machines]]\nlabel = \"a\"\npalette = \"red\"\n", "ssh"),
            (
                "[[machines]]\nlabel = \"a\"\nssh = \"h\"\npalette = \"red\"\nhost = \"x\"\n",
                "unknown config key machines[0].host",
            ),
        ] {
            let errors = client_from_str(content).expect_err("invalid machines must not launch");
            assert!(
                errors
                    .iter()
                    .any(|diagnostic| diagnostic.to_string().contains(message)),
                "expected {message:?} in {errors:?}"
            );
        }
    }

    #[test]
    fn config_check_collects_all_semantic_diagnostics() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("config-diagnostics");
        let paths = crate::test_paths_at(scratch.path());
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
        std::fs::write(
            paths.client_config_file(),
            r#"
[keys]
prefix = "ctrl+"
zoom = "prefix+not-a-key"
[ui]
sidebar_width = 80
sidebar_min_width = 18
sidebar_max_width = 36
"#,
        )
        .expect("write invalid config fixture");

        let diagnostics = load_client_validated(&paths).expect_err("invalid fixture is refused");
        let messages = diagnostics
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        for expected in ["keys.prefix", "keys.zoom", "ui.sidebar_width"] {
            assert!(
                messages.iter().any(|message| message.contains(expected)),
                "missing {expected:?} from {messages:?}"
            );
        }
    }

    #[test]
    fn load_validated_rejects_bad_config_file_and_accepts_missing_file() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("config-load");
        let paths = crate::test_paths_at(scratch.path());
        let path = paths.client_config_file();
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");

        std::fs::write(&path, "[ui]\nsidebar_width = 0\n").expect("write bad config fixture");
        assert!(load_client_validated(&paths).is_err());

        std::fs::remove_file(&path).expect("remove config fixture");
        let defaults = load_client_validated(&paths).expect("missing config uses defaults");
        assert!(defaults.validated_live_keybinds().is_ok());
        assert_eq!(defaults.local_hue(), crate::DEFAULT_LOCAL_HUE);
    }

    #[test]
    fn load_validated_rejects_home_cwd_without_absolute_home() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("config-cwd");
        // Launch paths without a home directory: what resolution captures
        // when HOME is missing or relative.
        let paths = crate::test_paths_at(scratch.path());
        assert!(paths.home_dir().is_none());
        let path = paths.server_config_file();
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
        std::fs::write(&path, "[terminal]\nnew_cwd = \"home\"\n").expect("write config fixture");

        let errors = load_server_validated(&paths).expect_err("home cwd needs absolute HOME");
        assert!(
            errors.iter().any(|error| error
                .key()
                .is_some_and(|key| key.to_string() == "terminal.new_cwd")),
            "{errors:?}"
        );
    }

    #[test]
    fn new_cwd_resolves_against_the_handed_over_launch_directory() {
        let env = shepr_test_support::IsolatedEnv::new();
        let launch = env.path().join("launch");
        std::fs::create_dir_all(launch.join("project")).expect("create launch directory");
        env.set(EnvVar::SheprStartupCwd, &launch);
        let paths = AppPaths::resolve_for_server().expect("server paths resolve");

        // `current` and a relative new_cwd resolve against the launch directory.
        std::fs::create_dir_all(paths.config_dir()).expect("create config dir");
        std::fs::write(
            paths.server_config_file(),
            "[terminal]\nnew_cwd = \"project\"\n",
        )
        .expect("write config fixture");
        let config = load_server_validated(&paths).expect("relative new_cwd validates");
        assert_eq!(
            config.terminal().new_cwd,
            crate::NewTerminalCwd::Path(
                shepr_core::absolute_path::AbsolutePath::new(launch.join("project"))
                    .expect("absolute")
            )
        );
        std::fs::write(
            paths.server_config_file(),
            "[terminal]\nnew_cwd = \"current\"\n",
        )
        .expect("write config fixture");
        let config = load_server_validated(&paths).expect("current new_cwd validates");
        assert_eq!(config.terminal().new_cwd, crate::NewTerminalCwd::Current);
        assert_eq!(
            config
                .paths()
                .current_dir()
                .map(shepr_core::absolute_path::AbsolutePath::as_path),
            Some(launch.as_path())
        );
    }

    #[test]
    fn config_load_reports_unknown_keys_and_parses_known_siblings() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let document = client_document(
            r##"
plugin = []

[ui.sidebar.spaces]
row_gapp = 1

[keys]
zoom = "prefix+z"
new_workspacee = "prefix+t"

[ui]
mouse_capture = false
mouse_captur = true
"foo.bar" = true
"##,
        )
        .expect("the document parses");
        assert!(!document.config.ui.mouse_capture);

        assert_eq!(
            document
                .validate_client(&crate::test_paths())
                .expect_err("unknown keys fail the load")
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec![
                "unknown config key keys.new_workspacee",
                "unknown config key plugin",
                "unknown config key ui.\"foo.bar\"",
                "unknown config key ui.mouse_captur",
                "unknown config key ui.sidebar.spaces.row_gapp",
            ]
        );
    }

    #[test]
    fn config_load_keeps_optional_ui_values_explicit() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let document = client_document(
            r#"
[ui]
sidebar_width = 26
"#,
        )
        .expect("the document parses");
        assert_eq!(document.config.ui.sidebar_width, Some(26));
        assert_eq!(document.config.ui.sidebar_start_collapsed, None);
        assert!(document.validate_client(&crate::test_paths()).is_ok());

        let empty = client_document("").expect("the empty document parses");
        assert_eq!(empty.config.ui.sidebar_width, None);
    }

    #[test]
    fn config_provenance_queries_array_fields_by_their_parent_key() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let document: toml::Value =
            toml::from_str("[keys]\nfocus_agent = [\"prefix+1\", \"prefix+2\"]\n")
                .expect("array binding fixture parses");
        let configured = ConfigProvenance::from_document(Some(&document));
        assert!(
            configured.key_is_configured(&ConfigKeyPath::root().key("keys").key("focus_agent"))
        );

        let defaults = ConfigProvenance::from_document(None);
        assert!(!defaults.key_is_configured(&ConfigKeyPath::root().key("keys").key("focus_agent")));
    }

    #[test]
    fn config_load_reports_unknown_top_level_sections() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let errors = client_from_str(
            r#"
[[plugin]]
id = "example"
"#,
        )
        .expect_err("the unknown section fails the load");

        assert_eq!(
            errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["unknown config section [[plugin]]"]
        );
    }

    #[test]
    fn normalize_utf8_bom_removes_a_leading_bom() {
        let content = "\u{feff}[terminal]\n";
        assert_eq!(normalize_utf8_bom(content), "[terminal]\n");
    }

    #[test]
    fn normalize_utf8_bom_leaves_a_mid_file_bom_for_the_parser_to_reject() {
        let content = "[ui]\n\u{feff}[terminal]\ndefault_shell = \"zsh\"\n";
        let normalized = normalize_utf8_bom(content);
        assert_eq!(normalized, content);
        assert!(normalized.parse::<toml::Table>().is_err());
    }

    #[test]
    fn normalize_utf8_bom_preserves_boms_in_multiline_basic_strings() {
        let content = "[theme]\nname = \"\"\"\nfirst\n\u{feff}second\n\"\"\"\n";
        assert!(content.parse::<toml::Table>().is_ok());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn normalize_utf8_bom_preserves_boms_in_multiline_literal_strings() {
        let content = "[theme]\nname = '''\nfirst\n\u{feff}second\n'''\n";
        assert!(content.parse::<toml::Table>().is_ok());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn normalize_utf8_bom_preserves_string_boms_despite_other_errors() {
        let content = "[theme]\nname = \"\"\"\nfirst\n\u{feff}second\n\"\"\"\nbroken = \n";
        assert!(content.parse::<toml::Table>().is_err());
        assert_eq!(normalize_utf8_bom(content), content);
    }
}
