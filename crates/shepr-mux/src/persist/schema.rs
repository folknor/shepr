//! The on-disk schema of a saved session: the layout file and the history file.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::terminal::Label;
use crate::workspace::Shape;
use shepr_core::absolute_path::AbsolutePath;
use shepr_core::layout::Direction;
use shepr_core::limits::PALETTE_COLOR_COUNT;
use shepr_protocol::PanePublicNumber;

/// Current snapshot format version. Deserialization rejects every other value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SnapshotVersion(u32);

pub const SNAPSHOT_VERSION: SnapshotVersion = SnapshotVersion(1);

impl<'de> Deserialize<'de> for SnapshotVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u32::deserialize(deserializer)?;
        if value == SNAPSHOT_VERSION.0 {
            Ok(SNAPSHOT_VERSION)
        } else {
            Err(serde::de::Error::custom(format!(
                "snapshot version {value} is not supported (expected {})",
                SNAPSHOT_VERSION.0
            )))
        }
    }
}

/// Paths stay readable when they are UTF-8. Linux paths with arbitrary bytes
/// use a JSON byte sequence so one pane cannot make the whole save fail.
mod path_bytes {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    use serde::de::{SeqAccess, Visitor};
    use serde::{Deserializer, Serialize, Serializer};

    pub(crate) fn serialize<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(utf8) = path.to_str() {
            serializer.serialize_str(utf8)
        } else {
            path.as_os_str().as_bytes().serialize(serializer)
        }
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PathVisitor;

        impl<'de> Visitor<'de> for PathVisitor {
            type Value = PathBuf;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a UTF-8 path string or a sequence of path bytes")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(PathBuf::from(value))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(PathBuf::from(value))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut bytes = Vec::with_capacity(sequence.size_hint().unwrap_or_default());
                while let Some(byte) = sequence.next_element::<u8>()? {
                    bytes.push(byte);
                }
                Ok(PathBuf::from(OsString::from_vec(bytes)))
            }
        }

        deserializer.deserialize_any(PathVisitor)
    }
}

// The serde types below are the on-disk schema itself: every field is
// required, a nullable one included, so a key a save always writes cannot go
// missing unnoticed. A file that does not match fails to parse and takes the
// unusable-file path (backed up, then replaced), whole. Keeping a second,
// hand-written schema in front of lenient types, or `Option`s and defaults
// for damaged in-memory fixtures, lets the two drift; restore validates only
// what a type cannot express. The one tolerance is a pane's agent session
// (see `deserialize_agent_session`).

/// Serializable snapshot of the entire shepr session.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    /// Format version - used to detect incompatible changes.
    pub version: SnapshotVersion,
    pub host_theme: SavedHostTheme,
    pub workspaces: Vec<WorkspaceSnapshot>,
    /// The workspace the session's bookmark names: where a client with no
    /// location of its own starts.
    #[serde(deserialize_with = "required_nullable")]
    pub active: Option<usize>,
}

/// One saved layout file, including the history bytes it names. `T` is the
/// borrowed snapshot form while writing and the owned form while reading.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFile<T> {
    pub snapshot: T,
    #[serde(deserialize_with = "required_nullable")]
    pub history_digest: Option<super::history::HistoryDigest>,
}

// Serde fills a missing `Option` field with `None` unless the field has its
// own `deserialize_with`, which makes the key required while its value may
// still be `null`.
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Last observed physical terminal colours, retained for headless resumes.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedHostTheme {
    #[serde(deserialize_with = "required_nullable")]
    pub foreground: Option<shepr_term::host::RgbColor>,
    #[serde(deserialize_with = "required_nullable")]
    pub background: Option<shepr_term::host::RgbColor>,
    #[serde(deserialize_with = "deserialize_palette")]
    pub palette: Vec<Option<shepr_term::host::RgbColor>>,
}

// serde has no array impl past 32 entries, so the palette is a `Vec` whose
// length is checked here.
fn deserialize_palette<'de, D>(
    deserializer: D,
) -> Result<Vec<Option<shepr_term::host::RgbColor>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let palette = Vec::<Option<shepr_term::host::RgbColor>>::deserialize(deserializer)?;
    if palette.len() != PALETTE_COLOR_COUNT {
        return Err(serde::de::Error::custom(format!(
            "palette has {} colors, expected {PALETTE_COLOR_COUNT}",
            palette.len()
        )));
    }
    Ok(palette)
}

impl Default for SavedHostTheme {
    fn default() -> Self {
        Self {
            foreground: None,
            background: None,
            palette: vec![None; PALETTE_COLOR_COUNT],
        }
    }
}

impl From<shepr_term::host::TerminalTheme> for SavedHostTheme {
    fn from(theme: shepr_term::host::TerminalTheme) -> Self {
        Self {
            foreground: theme.foreground,
            background: theme.background,
            palette: theme.palette.into(),
        }
    }
}

impl SavedHostTheme {
    pub fn to_theme(&self) -> shepr_term::host::TerminalTheme {
        let mut theme = shepr_term::host::TerminalTheme {
            foreground: self.foreground,
            background: self.background,
            ..Default::default()
        };
        for (index, color) in self.palette.iter().take(PALETTE_COLOR_COUNT).enumerate() {
            theme.palette[index] = *color;
        }
        theme
    }
}

#[derive(Serialize, Deserialize)]
pub struct SessionHistorySnapshot {
    /// Format version follows the matching session snapshot version.
    pub version: SnapshotVersion,
    pub workspaces: Vec<WorkspaceHistorySnapshot>,
}

/// The history of each pane of one workspace, by the pane's public number.
/// The ordered map writes the same captured history as the same bytes.
#[derive(Serialize, Deserialize)]
pub struct WorkspaceHistorySnapshot {
    pub panes: BTreeMap<PanePublicNumber, PaneHistorySnapshot>,
}

/// One saved workspace. Its identity cwd is not saved: restore derives it from
/// the restored root pane's cwd. The panes are the leaves of `layout`, each
/// carrying its own record, so a leaf without a record and a record without a
/// leaf cannot be written.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    /// Canonical identity; restore assigns a fresh identity to duplicates.
    pub id: shepr_protocol::WorkspaceId,
    #[serde(deserialize_with = "required_nullable")]
    pub custom_name: Option<String>,
    /// Restore checks it against the panes' numbers.
    pub next_public_pane_number: PanePublicNumber,
    pub layout: LayoutSnapshot,
    pub zoomed: bool,
    /// The focused pane's public number.
    pub focused: PanePublicNumber,
    /// The root pane's public number.
    pub root_pane: PanePublicNumber,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneSnapshot {
    #[serde(
        serialize_with = "path_bytes::serialize",
        deserialize_with = "deserialize_cwd"
    )]
    // Absolute by type, checked when the file is parsed: shepr only ever saves
    // absolute cwds, so a relative one is a damaged file, refused whole like
    // any other schema violation. Whether the directory still exists is not a
    // schema fact: it may disappear between capture and restore, and the
    // child's required chdir owns that admission.
    pub cwd: AbsolutePath,
    /// Decoding refuses zero; restore refuses repeats within a workspace.
    pub public_number: shepr_protocol::PanePublicNumber,
    #[serde(deserialize_with = "required_nullable")]
    pub label: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_agent_session"
    )]
    pub agent_session: Option<PaneAgentSessionSnapshot>,
}

pub type PaneAgentSessionSnapshot = shepr_agent::resume::PersistedAgentSession;

fn deserialize_cwd<'de, D>(deserializer: D) -> Result<AbsolutePath, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let path = path_bytes::deserialize(deserializer)?;
    AbsolutePath::new(path).map_err(serde::de::Error::custom)
}

// Agent labels and session formats can disappear between builds. A bad saved
// session must not discard the pane or unrelated workspaces.
fn deserialize_agent_session<'de, D>(
    deserializer: D,
) -> Result<Option<PaneAgentSessionSnapshot>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| match serde_json::from_value(value) {
        Ok(session) => Some(session),
        Err(error) => {
            tracing::warn!(%error, "ignoring invalid saved agent session");
            None
        }
    }))
}

/// Saved screen history of one pane.
#[derive(Serialize, Deserialize)]
pub struct PaneHistorySnapshot {
    pub ansi: String,
}

/// Serializable BSP tree.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum LayoutSnapshot {
    Pane(PaneSnapshot),
    Split {
        direction: DirectionSnapshot,
        ratio: shepr_core::layout::SplitRatio,
        first: Box<LayoutSnapshot>,
        second: Box<LayoutSnapshot>,
    },
}

impl LayoutSnapshot {
    /// The layout a captured shape of pane records writes as.
    pub fn from_shape(shape: Shape<PaneSnapshot>) -> Self {
        match shape {
            Shape::Pane(pane) => Self::Pane(pane),
            Shape::Split {
                direction,
                ratio,
                first,
                second,
            } => Self::Split {
                direction: direction.into(),
                ratio,
                first: Box::new(Self::from_shape(*first)),
                second: Box::new(Self::from_shape(*second)),
            },
        }
    }

    /// The saved layout as a shape over its pane records, for restore.
    pub fn to_shape(&self) -> Shape<&PaneSnapshot> {
        match self {
            Self::Pane(pane) => Shape::Pane(pane),
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => Shape::Split {
                direction: (*direction).into(),
                ratio: *ratio,
                first: Box::new(first.to_shape()),
                second: Box::new(second.to_shape()),
            },
        }
    }

    /// The panes in tree order.
    pub fn panes(&self) -> Vec<&PaneSnapshot> {
        fn collect<'a>(layout: &'a LayoutSnapshot, panes: &mut Vec<&'a PaneSnapshot>) {
            match layout {
                LayoutSnapshot::Pane(pane) => panes.push(pane),
                LayoutSnapshot::Split { first, second, .. } => {
                    collect(first, panes);
                    collect(second, panes);
                }
            }
        }
        let mut panes = Vec::new();
        collect(self, &mut panes);
        panes
    }

    /// The pane saved under `number`, to refine it in place.
    pub fn pane_mut(&mut self, number: PanePublicNumber) -> Option<&mut PaneSnapshot> {
        match self {
            Self::Pane(pane) => (pane.public_number == number).then_some(pane),
            Self::Split { first, second, .. } => match first.pane_mut(number) {
                Some(pane) => Some(pane),
                None => second.pane_mut(number),
            },
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum DirectionSnapshot {
    Horizontal,
    Vertical,
}

// Keep this translation at the persistence boundary: core owns live layout
// directions, while this file owns the saved JSON tag.
impl From<Direction> for DirectionSnapshot {
    fn from(direction: Direction) -> Self {
        match direction {
            Direction::Horizontal => Self::Horizontal,
            Direction::Vertical => Self::Vertical,
        }
    }
}

impl From<DirectionSnapshot> for Direction {
    fn from(direction: DirectionSnapshot) -> Self {
        match direction {
            DirectionSnapshot::Horizontal => Self::Horizontal,
            DirectionSnapshot::Vertical => Self::Vertical,
        }
    }
}

/// Parses one on-disk session file. The serde types are the whole schema, so
/// a missing key, a wrong type or an unknown field is a parse error here.
pub fn parse_session_file(
    content: &str,
) -> Result<SessionFile<SessionSnapshot>, serde_json::Error> {
    serde_json::from_str(content)
}

pub(super) fn parse_history_snapshot(
    content: &str,
) -> Result<SessionHistorySnapshot, serde_json::Error> {
    serde_json::from_str(content)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    use shepr_protocol::PanePublicNumber;

    fn number(value: usize) -> PanePublicNumber {
        PanePublicNumber::new(value).expect("nonzero literal")
    }

    fn saved_pane(public_number: usize) -> super::PaneSnapshot {
        super::PaneSnapshot {
            cwd: super::AbsolutePath::root(),
            public_number: number(public_number),
            label: None,
            agent_session: None,
        }
    }

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Holder {
        #[serde(with = "super::path_bytes")]
        path: PathBuf,
    }

    #[test]
    fn snapshot_types_reject_wrong_version_during_deserialization() {
        for json in [
            r#"{"version":2,"workspaces":[],"active":null}"#,
            r#"{"version":2,"workspaces":[]}"#,
        ] {
            assert!(serde_json::from_str::<super::SessionSnapshot>(json).is_err());
            assert!(serde_json::from_str::<super::SessionHistorySnapshot>(json).is_err());
        }
    }

    #[test]
    fn snapshot_cwds_must_be_absolute_but_need_not_exist() {
        for relative in [
            r#"{"cwd":"relative","public_number":1,"label":null}"#,
            r#"{"cwd":"","public_number":1,"label":null}"#,
        ] {
            assert!(serde_json::from_str::<super::PaneSnapshot>(relative).is_err());
        }
        // A missing saved path remains available for a later restore attempt.
        let missing = r#"{"cwd":"/shepr-missing-saved-directory","public_number":1,"label":null}"#;
        let pane: super::PaneSnapshot = serde_json::from_str(missing).expect("absolute saved cwd");
        assert_eq!(pane.cwd, PathBuf::from("/shepr-missing-saved-directory"));
    }

    /// Every key a save writes is required, a nullable one included, and a
    /// key no save writes is refused; a pane's agent session alone may be
    /// absent (a save leaves it out when there is none).
    #[test]
    fn every_saved_key_is_required_and_no_other_is_accepted() {
        let snapshot = super::SessionSnapshot {
            version: super::SNAPSHOT_VERSION,
            host_theme: super::SavedHostTheme::default(),
            workspaces: vec![super::WorkspaceSnapshot {
                id: "w1".parse().expect("id"),
                custom_name: None,
                next_public_pane_number: shepr_protocol::PanePublicNumber::new(2)
                    .expect("nonzero literal"),
                layout: super::LayoutSnapshot::Split {
                    direction: super::DirectionSnapshot::Horizontal,
                    ratio: shepr_core::layout::SplitRatio::new(0.5)
                        .expect("test split ratio is valid"),
                    first: Box::new(super::LayoutSnapshot::Pane(saved_pane(1))),
                    second: Box::new(super::LayoutSnapshot::Pane(saved_pane(2))),
                },
                zoomed: false,
                focused: number(1),
                root_pane: number(1),
            }],
            active: None,
        };
        let saved = serde_json::to_value(super::SessionFile {
            snapshot: &snapshot,
            history_digest: None,
        })
        .expect("serialize");
        let pane = "/snapshot/workspaces/0/layout/Split/first/Pane";
        assert!(saved.pointer(&format!("{pane}/agent_session")).is_none());
        super::parse_session_file(&saved.to_string()).expect("a saved file parses");

        let workspace = "/snapshot/workspaces/0";
        for (parent, key) in [
            ("", "history_digest"),
            ("/snapshot", "version"),
            ("/snapshot", "host_theme"),
            ("/snapshot", "workspaces"),
            ("/snapshot", "active"),
            ("/snapshot/host_theme", "foreground"),
            ("/snapshot/host_theme", "background"),
            ("/snapshot/host_theme", "palette"),
            (workspace, "id"),
            (workspace, "custom_name"),
            (workspace, "next_public_pane_number"),
            (workspace, "layout"),
            (workspace, "zoomed"),
            (workspace, "focused"),
            (workspace, "root_pane"),
            (pane, "cwd"),
            (pane, "public_number"),
            (pane, "label"),
        ] {
            let mut damaged = saved.clone();
            damaged
                .pointer_mut(parent)
                .and_then(serde_json::Value::as_object_mut)
                .and_then(|object| object.remove(key))
                .expect("test precondition");
            assert!(
                super::parse_session_file(&damaged.to_string()).is_err(),
                "{parent}/{key} missing"
            );
        }

        for (pointer, invalid) in [
            ("/snapshot/workspaces/0/id", serde_json::json!("ws_1")),
            ("/snapshot/workspaces/0/id", serde_json::json!(0)),
            (
                "/snapshot/workspaces/0/next_public_pane_number",
                serde_json::json!(0),
            ),
            (
                "/snapshot/workspaces/0/layout/Split/first/Pane/public_number",
                serde_json::json!(0),
            ),
            ("/snapshot/workspaces/0/focused", serde_json::json!(0)),
            ("/snapshot/workspaces/0/root_pane", serde_json::json!(0)),
            (
                "/snapshot/workspaces/0/layout/Split/ratio",
                serde_json::json!(0.0),
            ),
            (
                "/snapshot/workspaces/0/layout/Split/ratio",
                serde_json::json!(1.0),
            ),
        ] {
            let mut damaged = saved.clone();
            *damaged.pointer_mut(pointer).expect("schema field") = invalid;
            assert!(
                super::parse_session_file(&damaged.to_string()).is_err(),
                "{pointer}"
            );
        }

        let mut unknown = saved.clone();
        unknown
            .pointer_mut(workspace)
            .and_then(serde_json::Value::as_object_mut)
            .expect("test precondition")
            .insert("identity_cwd".into(), "/".into());
        assert!(super::parse_session_file(&unknown.to_string()).is_err());

        let mut short_palette = saved;
        short_palette
            .pointer_mut("/snapshot/host_theme/palette")
            .and_then(serde_json::Value::as_array_mut)
            .expect("test precondition")
            .pop();
        assert!(super::parse_session_file(&short_palette.to_string()).is_err());
    }

    #[test]
    fn invalid_saved_agent_sessions_do_not_reject_the_pane() {
        for session in [
            serde_json::json!({"source": "shepr:codex", "agent": "removed-agent", "session_ref": {"id": "session"}}),
            serde_json::json!({"source": "invalid source", "agent": "codex", "session_ref": {"id": "session"}}),
            serde_json::json!(42),
        ] {
            let pane: super::PaneSnapshot = serde_json::from_value(serde_json::json!({
                "cwd": "/", "public_number": 1, "label": null, "agent_session": session,
            }))
            .expect("bad session stays local to this pane");
            assert!(pane.agent_session.is_none());
        }
    }

    #[test]
    fn history_panes_serialize_in_numeric_order() {
        let snapshot = super::WorkspaceHistorySnapshot {
            panes: BTreeMap::from([
                (
                    number(12),
                    super::PaneHistorySnapshot {
                        ansi: "twelve".into(),
                    },
                ),
                (number(2), super::PaneHistorySnapshot { ansi: "two".into() }),
                (
                    number(9),
                    super::PaneHistorySnapshot {
                        ansi: "nine".into(),
                    },
                ),
            ]),
        };

        assert_eq!(
            serde_json::to_string(&snapshot).expect("serialize"),
            r#"{"panes":{"2":{"ansi":"two"},"9":{"ansi":"nine"},"12":{"ansi":"twelve"}}}"#
        );
    }

    /// A pane's record lives in its layout leaf, so a file whose leaves name
    /// panes without records, or that keeps records beside the layout, is not
    /// a saved file.
    #[test]
    fn a_saved_file_without_records_in_its_leaves_fails_to_parse() {
        let workspace = |layout: serde_json::Value, extra: Option<(&str, serde_json::Value)>| {
            let mut workspace = serde_json::json!({
                "id": "w1",
                "custom_name": null,
                "next_public_pane_number": 2,
                "layout": layout,
                "zoomed": false,
                "focused": 1,
                "root_pane": 1,
            });
            if let Some((key, value)) = extra {
                workspace[key] = value;
            }
            serde_json::json!({
                "snapshot": {
                    "version": 1,
                    "host_theme": super::SavedHostTheme::default(),
                    "workspaces": [workspace],
                    "active": null,
                },
                "history_digest": null,
            })
        };
        let record = serde_json::json!({"cwd": "/", "public_number": 1, "label": null});

        super::parse_session_file(
            &workspace(serde_json::json!({"Pane": record}), None).to_string(),
        )
        .expect("a leaf with its record parses");
        // A leaf that names a pane by a bare number.
        assert!(
            super::parse_session_file(&workspace(serde_json::json!({"Pane": 1}), None).to_string())
                .is_err()
        );
        // Records kept beside the layout.
        assert!(
            super::parse_session_file(
                &workspace(
                    serde_json::json!({"Pane": record}),
                    Some(("panes", serde_json::json!({"1": record}))),
                )
                .to_string()
            )
            .is_err()
        );
    }

    #[test]
    fn utf8_paths_stay_strings_and_other_paths_round_trip_as_bytes() {
        let readable = Holder {
            path: PathBuf::from("/home/user/project"),
        };
        let json = serde_json::to_string(&readable).expect("test precondition");
        assert_eq!(json, r#"{"path":"/home/user/project"}"#);
        assert_eq!(
            serde_json::from_str::<Holder>(&json).expect("utf-8 path parses"),
            readable
        );

        let raw = Holder {
            path: PathBuf::from(OsString::from_vec(b"/tmp/caf\xe9".to_vec())),
        };
        let json = serde_json::to_string(&raw).expect("non-UTF-8 path serializes");
        assert!(json.contains('['), "{json}");
        assert_eq!(
            serde_json::from_str::<Holder>(&json).expect("byte path parses"),
            raw
        );
    }
}
