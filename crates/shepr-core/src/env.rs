//! The one reader of the process environment, and the one policy it applies.
//!
//! Every fixed-name environment variable a shepr process interprets is an
//! [`EnvVar`] variant with a declared [`EnvKind`], and every read goes through
//! [`read`]
//! and its typed wrappers (or [`resolve`], the pure half over a value handed
//! in). Git's indexed `GIT_CONFIG_KEY_<n>` and `GIT_CONFIG_VALUE_<n>` families
//! are registered by [`is_registered_name`] and read by
//! [`read_git_config_parameters`]. Variables shepr only sets or removes on a
//! child's environment are the separate [`ChildEnv`] vocabulary. `PATH` and
//! `SHELL` appear in both tables:
//! the process reads them to resolve its pane shell, then writes resolved
//! values into the pane environment. The root `clippy.toml` denies
//! `std::env::var`, `var_os`,
//! `vars`, `vars_os`, `set_var` and `remove_var` everywhere else; the allowed
//! sites outside this module (building a pane child's environment from the
//! server's, the test isolation guard, test harness probes) each carry a
//! scoped `#[expect]`, and `brokkr.toml`'s `disallowed-escapes-are-allowlisted`
//! textlint makes a new one a reviewed change.
//!
//! One rule holds for every variable:
//!
//! - unset and empty are the same answer: unset. A shell `VAR=` is not a value.
//!   A selector ([`EnvKind::SelectorPath`]) refuses empty instead: unset
//!   falls back to the build's default server, so an
//!   empty override (a shell `VAR=$UNSET`, or a pane launched with an empty
//!   socket path) would otherwise silently retarget the process at a
//!   different server.
//! - surrounding whitespace is refused, naming the variable: `" 1"` is not `1`,
//!   and guessing which the user meant is how a setting silently fails to take.
//! - a value that is not valid UTF-8 is refused, naming the variable.
//! - a [`EnvKind::Flag`] accepts exactly `1`, `0`, `true` and `false`, in that
//!   case; anything else is refused.
//! - an absolute-path kind refuses a relative path, naming the variable.
//!
//! [`EnvKind::Handoff`] and [`EnvKind::Raw`] preserve OS strings byte for byte.
//! A handoff is written by one shepr process for a child. Raw values are
//! inherited `PATH` and `SHELL` inputs, where non-UTF-8 bytes and whitespace
//! can be meaningful, and Git's command-scope config variables, whose grammar
//! belongs to Git;
//! only empty reads as unset.
//!
//! What a value means beyond its kind (a log filter's
//! syntax, which directory a relative path is joined to) stays with the site
//! that owns the variable.
//!
//! Core is the home because every crate that reads the environment already
//! depends on it or sits above it, and reading the environment needs nothing
//! beyond `std`.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

/// The value shepr writes into `SHEPR_ENV` for every pane, and the only value
/// the binary's nested-launch check and the shipped hook assets treat as
/// "inside a shepr pane".
pub const SHEPR_ENV_IN_PANE: &str = "1";

/// `SHEPR_*` spellings used internally by the shipped agent assets rather
/// than read by a shepr process or written into a pane, so they stay outside
/// the environment registry. Keeping the list in core lets test isolation
/// clear these names and the agent crate's asset test check against the same
/// list, without making them process configuration.
pub const SHEPR_ASSET_INTERNAL_NAMES: &[&str] = &[
    // Header markers install and status code parse out of an asset's text;
    // they are not environment variables.
    "SHEPR_INTEGRATION_ID",
    "SHEPR_INTEGRATION_VERSION",
    // A hook script handing its arguments to the interpreter it runs.
    "SHEPR_ACTION",
    "SHEPR_HOOK_INPUT_FILE",
    "SHEPR_HOOK_SEQ",
    // Tunables only the omp extension reads.
    "SHEPR_OMP_IDLE_DEBOUNCE_MS",
    "SHEPR_OMP_RETRY_GRACE_MS",
    // The devin hook's injection seam for its own tests.
    "SHEPR_DEVIN_LIST_JSON",
];

/// Declares a closed vocabulary of environment variable names: the enum, its
/// `ALL` table in declaration order, `name()`, `Display` and `AsRef<OsStr>`,
/// so a variant can be handed straight to `Command::env` and friends.
macro_rules! env_vocabulary {
    (
        $(#[$enum_meta:meta])*
        pub enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident => $spelling:literal, )*
        }
    ) => {
        $(#[$enum_meta])*
        pub enum $name {
            $( $(#[$variant_meta])* $variant, )*
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];

            /// The variable's name as the environment spells it.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $spelling,)*
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.name())
            }
        }

        impl AsRef<OsStr> for $name {
            fn as_ref(&self) -> &OsStr {
                OsStr::new(self.name())
            }
        }
    };
}

env_vocabulary! {
    /// Every environment variable a shepr process interprets.
    ///
    /// The table test below pins the name and kind of every variant.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum EnvVar {
        /// `SHEPR_CONFIG_PATH`: the config file, explicitly; a relative value
        /// is joined to the current directory.
        SheprConfigPath => "SHEPR_CONFIG_PATH",
        /// `SHEPR_SOCKET_PATH`: the API socket of the server to target. Written
        /// into every pane as the socket of the server that owns it.
        SheprSocketPath => "SHEPR_SOCKET_PATH",
        /// `SHEPR_CLIENT_SOCKET_PATH`: the client socket to target when
        /// `SHEPR_SOCKET_PATH` is not set. Written by the remote bridge into
        /// the client it spawns.
        SheprClientSocketPath => "SHEPR_CLIENT_SOCKET_PATH",
        /// `SHEPR_PANE_ID`: the public id of the pane a process runs in,
        /// written into every managed pane.
        SheprPaneId => "SHEPR_PANE_ID",
        /// `SHEPR_ENV`: marks a process as running inside a shepr pane; the
        /// value is [`SHEPR_ENV_IN_PANE`].
        SheprEnv => "SHEPR_ENV",
        /// `SHEPR_STARTUP_CWD`: the directory the user launched `shepr` from,
        /// handed to the server daemon it spawns to seed the first workspace.
        SheprStartupCwd => "SHEPR_STARTUP_CWD",
        /// `SHEPR_LOG`: the `tracing` filter directives for the file logs.
        SheprLog => "SHEPR_LOG",
        /// `SHEPR_DEBUG_OSC_EVIDENCE`: logs selected OSC sequences each pane
        /// receives, pane content included. The payloads (window titles,
        /// progress text) are child-controlled and routinely carry paths,
        /// branch names and ticket numbers; they are truncated but not
        /// filtered. Meant for capturing evidence while writing agent
        /// manifests, not for leaving on.
        SheprDebugOscEvidence => "SHEPR_DEBUG_OSC_EVIDENCE",
        /// `HOME`: the user's home directory, the parent of every default path.
        Home => "HOME",
        /// `XDG_CONFIG_HOME`: the config tree's parent.
        XdgConfigHome => "XDG_CONFIG_HOME",
        /// `XDG_STATE_HOME`: the state tree's parent.
        XdgStateHome => "XDG_STATE_HOME",
        /// `XDG_RUNTIME_DIR`: the runtime tree's parent; it has no default.
        XdgRuntimeDir => "XDG_RUNTIME_DIR",
        /// `SHELL`: the inherited shell used when `terminal.default_shell` is
        /// empty. An unusable or unrecognized value fails the launch; unset
        /// or blank means `/bin/sh`.
        Shell => "SHELL",
        /// `PATH`: the inherited executable search path used to resolve the
        /// configured pane shell at launch.
        Path => "PATH",
        /// `SSH_AUTH_SOCK`: the SSH agent socket panes are given access to.
        SshAuthSock => "SSH_AUTH_SOCK",
        /// `SSH_CONNECTION`: set by sshd; its presence means the clipboard is
        /// on the far side of an SSH session.
        SshConnection => "SSH_CONNECTION",
        /// `SSH_TTY`: set by sshd; read like `SSH_CONNECTION`.
        SshTty => "SSH_TTY",
        /// `VSCODE_IPC_HOOK_CLI`: set in a VS Code remote terminal; its
        /// presence means the clipboard is on the editor's side.
        VscodeIpcHookCli => "VSCODE_IPC_HOOK_CLI",
        /// `TMUX`: set inside tmux; selects the host key protocol. Removed from
        /// every pane, since it names the outer terminal.
        Tmux => "TMUX",
        /// `TERM_PROGRAM`: the host terminal's name; selects the host key
        /// protocol. Written into every pane as `shepr`.
        TermProgram => "TERM_PROGRAM",
        /// `WEZTERM_PANE`: set inside WezTerm; selects the host key protocol.
        /// Removed from every pane, since it names the outer terminal.
        WeztermPane => "WEZTERM_PANE",
        /// `WAYLAND_DISPLAY`: its presence offers the Wayland clipboard helpers.
        WaylandDisplay => "WAYLAND_DISPLAY",
        /// `DISPLAY`: its presence offers the X11 clipboard helpers.
        Display => "DISPLAY",
        /// `PI_CODING_AGENT_DIR`: pi's config directory override.
        PiCodingAgentDir => "PI_CODING_AGENT_DIR",
        /// `PI_CONFIG_DIR`: omp's config directory name under `HOME`.
        PiConfigDir => "PI_CONFIG_DIR",
        /// `CLAUDE_CONFIG_DIR`: Claude Code's config directory override.
        ClaudeConfigDir => "CLAUDE_CONFIG_DIR",
        /// `CODEX_HOME`: Codex's config directory override.
        CodexHome => "CODEX_HOME",
        /// `KIMI_CODE_HOME`: Kimi Code's config directory override.
        KimiCodeHome => "KIMI_CODE_HOME",
        /// `COPILOT_HOME`: GitHub Copilot CLI's config directory override.
        CopilotHome => "COPILOT_HOME",
        /// `QODER_CONFIG_DIR`: Qoder CLI's config directory override.
        QoderConfigDir => "QODER_CONFIG_DIR",
        /// `QWEN_HOME`: Qwen Code's config directory override.
        QwenHome => "QWEN_HOME",
        /// `CURSOR_CONFIG_DIR`: Cursor CLI's config directory override.
        CursorConfigDir => "CURSOR_CONFIG_DIR",
        /// `ANTIGRAVITY_CLI_CONFIG_DIR`: Antigravity CLI's config directory
        /// override.
        AntigravityCliConfigDir => "ANTIGRAVITY_CLI_CONFIG_DIR",
        /// `GROK_HOME`: the grok CLI's config home override.
        GrokHome => "GROK_HOME",
        /// `GIT_CEILING_DIRECTORIES`: Git's colon-separated list of absolute
        /// directories repository discovery does not ascend into. shepr's own
        /// discovery honours it as Git does; Git children read it themselves.
        GitCeilingDirectories => "GIT_CEILING_DIRECTORIES",
        /// `GIT_CONFIG_GLOBAL`: replace both default global config files with
        /// this one file, as Git does.
        GitConfigGlobal => "GIT_CONFIG_GLOBAL",
        /// `GIT_CONFIG_SYSTEM`: replace Git's system config file, normally
        /// `/etc/gitconfig`.
        GitConfigSystem => "GIT_CONFIG_SYSTEM",
        /// `GIT_CONFIG_NOSYSTEM`: Git's boolean setting that skips the system
        /// config file when true. Kept as text for Git's full boolean grammar.
        GitConfigNoSystem => "GIT_CONFIG_NOSYSTEM",
        /// `GIT_CONFIG_COUNT`: the number of indexed command-scope config
        /// pairs Git reads. Its decimal grammar follows Git's parser.
        GitConfigCount => "GIT_CONFIG_COUNT",
        /// `GIT_CONFIG_PARAMETERS`: quoted `git -c` assignments inherited by
        /// Git subprocesses, applied after the indexed command-scope pairs.
        GitConfigParameters => "GIT_CONFIG_PARAMETERS",
    }
}

env_vocabulary! {
    /// Variables shepr sets or removes in a child's environment. `PATH` and
    /// `SHELL` are also in [`EnvVar`] because the process reads them when
    /// resolving its pane shell.
    ///
    /// The closed vocabulary keeps every name shepr writes into a child in one
    /// place beside the names it reads, so the pane contract and the shipped
    /// hook assets can be checked against it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum ChildEnv {
        /// `TERM`: the terminal type every pane advertises.
        Term => "TERM",
        /// `COLORTERM`: the colour support every pane advertises.
        Colorterm => "COLORTERM",
        /// `TERM_PROGRAM_VERSION`: shepr's version, beside `TERM_PROGRAM`.
        TermProgramVersion => "TERM_PROGRAM_VERSION",
        /// `SHELL`: the resolved shell a pane child sees.
        Shell => "SHELL",
        /// `PATH`: the child's executable search path.
        Path => "PATH",
        /// `SHEPR_BIN_PATH`: the shepr executable, for tab-bar status commands
        /// to call back.
        SheprBinPath => "SHEPR_BIN_PATH",
        /// `SHEPR_ACTIVE_WORKSPACE_ID`: the focused workspace, for tab-bar
        /// status commands.
        SheprActiveWorkspaceId => "SHEPR_ACTIVE_WORKSPACE_ID",
        /// `SHEPR_ACTIVE_TAB_ID`: the focused tab, for tab-bar status commands.
        SheprActiveTabId => "SHEPR_ACTIVE_TAB_ID",
        /// `SHEPR_ACTIVE_PANE_ID`: the focused pane, for tab-bar status
        /// commands.
        SheprActivePaneId => "SHEPR_ACTIVE_PANE_ID",
        /// `SHEPR_ACTIVE_PANE_CWD`: the focused pane's directory, for tab-bar
        /// status commands.
        SheprActivePaneCwd => "SHEPR_ACTIVE_PANE_CWD",
        /// `SSH_ASKPASS`: removed so interactive SSH authentication prompts on
        /// the terminal.
        SshAskpass => "SSH_ASKPASS",
        /// `SSH_ASKPASS_REQUIRE`: set to `never` for the same reason.
        SshAskpassRequire => "SSH_ASKPASS_REQUIRE",
        /// `ITERM_SESSION_ID`: an outer-terminal handle, removed from panes.
        ItermSessionId => "ITERM_SESSION_ID",
        /// `LC_TERMINAL`: an outer-terminal handle, removed from panes.
        LcTerminal => "LC_TERMINAL",
        /// `LC_TERMINAL_VERSION`: an outer-terminal handle, removed from panes.
        LcTerminalVersion => "LC_TERMINAL_VERSION",
        /// `KITTY_WINDOW_ID`: an outer-terminal handle, removed from panes.
        KittyWindowId => "KITTY_WINDOW_ID",
        /// `WT_SESSION`: an outer-terminal handle, removed from panes.
        WtSession => "WT_SESSION",
        /// `TMUX_PANE`: an outer-terminal handle, removed from panes.
        TmuxPane => "TMUX_PANE",
        /// `STY`: an outer-terminal handle (screen), removed from panes.
        Sty => "STY",
        /// `ZELLIJ`: an outer-terminal handle, removed from panes.
        Zellij => "ZELLIJ",
        /// `ZELLIJ_SESSION_NAME`: an outer-terminal handle, removed from panes.
        ZellijSessionName => "ZELLIJ_SESSION_NAME",
        /// `ZELLIJ_PANE_ID`: an outer-terminal handle, removed from panes.
        ZellijPaneId => "ZELLIJ_PANE_ID",
        /// `CLAUDECODE`: an outer Claude Code session's marker, removed from
        /// panes so a new pane is not taken for its child agent.
        ClaudeCode => "CLAUDECODE",
        /// `CLAUDE_CODE_CHILD_SESSION`: removed from panes, as `CLAUDECODE`.
        ClaudeCodeChildSession => "CLAUDE_CODE_CHILD_SESSION",
        /// `CLAUDE_CODE_SESSION_ID`: removed from panes, as `CLAUDECODE`.
        ClaudeCodeSessionId => "CLAUDE_CODE_SESSION_ID",
        /// `CLAUDE_CODE_MESSAGING_TOKEN`: removed from panes, as `CLAUDECODE`.
        ClaudeCodeMessagingToken => "CLAUDE_CODE_MESSAGING_TOKEN",
        /// `CODEX_THREAD_ID`: an outer Codex session's marker, removed from
        /// panes.
        CodexThreadId => "CODEX_THREAD_ID",
        /// `OMPCODE`: an outer omp session's marker, removed from panes.
        Ompcode => "OMPCODE",
    }
}

/// What a variable's value is, which decides how [`resolve`] reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvKind {
    /// Exactly `1`, `0`, `true` or `false`.
    Flag,
    /// Text the owning site parses further.
    Text,
    /// A filesystem path; the owning site decides what a relative one means.
    Path,
    /// A filesystem path that must be absolute.
    AbsolutePath,
    /// An absolute path that selects which shepr server a process talks to:
    /// [`AbsolutePath`](Self::AbsolutePath) that refuses empty rather than
    /// reading it as unset (the module doc, first rule).
    SelectorPath,
    /// Presence alone is the answer; the value is never interpreted.
    Presence,
    /// A path one shepr process hands a shepr child byte for byte: only empty
    /// reads as unset, and nothing is refused.
    Handoff,
    /// An inherited OS string such as `PATH` or `SHELL`: only empty reads as
    /// unset, and nothing is refused.
    Raw,
}

impl EnvVar {
    /// The variable's declared kind.
    #[must_use]
    pub const fn kind(self) -> EnvKind {
        match self {
            Self::SheprDebugOscEvidence => EnvKind::Flag,
            Self::SheprPaneId
            | Self::SheprEnv
            | Self::SheprLog
            | Self::TermProgram
            | Self::GitCeilingDirectories
            | Self::GitConfigNoSystem => EnvKind::Text,
            Self::SheprConfigPath
            | Self::SshAuthSock
            | Self::PiCodingAgentDir
            | Self::PiConfigDir
            | Self::ClaudeConfigDir
            | Self::CodexHome
            | Self::KimiCodeHome
            | Self::CopilotHome
            | Self::QoderConfigDir
            | Self::QwenHome
            | Self::CursorConfigDir
            | Self::AntigravityCliConfigDir
            | Self::GrokHome
            | Self::GitConfigGlobal
            | Self::GitConfigSystem => EnvKind::Path,
            Self::Home | Self::XdgConfigHome | Self::XdgStateHome | Self::XdgRuntimeDir => {
                EnvKind::AbsolutePath
            }
            Self::SheprSocketPath | Self::SheprClientSocketPath => EnvKind::SelectorPath,
            Self::SshConnection
            | Self::SshTty
            | Self::VscodeIpcHookCli
            | Self::Tmux
            | Self::WeztermPane
            | Self::WaylandDisplay
            | Self::Display => EnvKind::Presence,
            Self::SheprStartupCwd => EnvKind::Handoff,
            Self::Shell | Self::Path | Self::GitConfigCount | Self::GitConfigParameters => {
                EnvKind::Raw
            }
        }
    }
}

/// Whether `name` belongs to the fixed environment vocabulary or to one of
/// Git's indexed command-scope config variable families. Indexed names use
/// the canonical decimal spelling Git generates (`_0`, `_1`, ...).
#[must_use]
pub fn is_registered_name(name: &OsStr) -> bool {
    EnvVar::ALL.iter().any(|var| OsStr::new(var.name()) == name)
        || name.to_str().is_some_and(|name| {
            is_indexed_git_config_name(name, "GIT_CONFIG_KEY_")
                || is_indexed_git_config_name(name, "GIT_CONFIG_VALUE_")
        })
}

fn is_indexed_git_config_name(name: &str, prefix: &str) -> bool {
    let Some(index) = name.strip_prefix(prefix) else {
        return false;
    };
    !index.is_empty()
        && index.bytes().all(|byte| byte.is_ascii_digit())
        && (index == "0" || !index.starts_with('0'))
}

/// Why a present value was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvRefusal {
    /// The bytes are not valid UTF-8.
    NotUtf8,
    /// A selector is set and empty.
    EmptySelector,
    /// The value has leading or trailing whitespace.
    SurroundingWhitespace(String),
    /// A flag that is not exactly `1`, `0`, `true` or `false`.
    NotAFlag(String),
    /// An absolute-path kind holding a relative path.
    NotAbsolute(String),
}

/// A refused environment value, naming the variable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvError {
    /// The variable refused.
    pub var: EnvVar,
    /// Why.
    pub refusal: EnvRefusal,
}

impl std::fmt::Display for EnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = self.var.name();
        match &self.refusal {
            EnvRefusal::NotUtf8 => write!(
                f,
                "{name} is not valid UTF-8; set it to a text value or unset it"
            ),
            EnvRefusal::EmptySelector => write!(
                f,
                "{name} is set but empty; it selects which shepr server to use, and an empty \
                 value would silently fall back to the default one. Set it to a value, or unset \
                 {name} to use the default on purpose"
            ),
            EnvRefusal::SurroundingWhitespace(value) => write!(
                f,
                "{name} is {value:?}, which has surrounding whitespace; remove it (an empty \
                 value is the same as unset)"
            ),
            EnvRefusal::NotAFlag(value) => write!(
                f,
                "{name} is {value:?}; expected exactly one of 1, 0, true, false (lowercase), \
                 or unset"
            ),
            EnvRefusal::NotAbsolute(value) => write!(
                f,
                "{name} is {value:?}, which is a relative path; set it to an absolute path or \
                 unset it"
            ),
        }
    }
}

impl std::error::Error for EnvError {}

impl From<EnvError> for io::Error {
    fn from(error: EnvError) -> Self {
        io::Error::new(io::ErrorKind::InvalidInput, error)
    }
}

/// A value that passed the policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvValue {
    /// A [`EnvKind::Flag`]'s answer.
    Flag(bool),
    /// A [`EnvKind::Presence`] variable that is set.
    Present,
    /// Any other UTF-8 kind's text, never empty and never padded.
    Text(String),
    /// A [`EnvKind::Handoff`] or [`EnvKind::Raw`] value, never empty.
    Raw(OsString),
}

/// The policy, over a raw value handed in: the pure half of [`read`], so every
/// arm is testable without mutating the process environment.
///
/// # Errors
///
/// Returns [`EnvError`] for a non-UTF-8 value, one with surrounding
/// whitespace, a malformed flag, a relative path where an absolute one is
/// declared, or an empty selector.
pub fn resolve(var: EnvVar, raw: Option<&OsStr>) -> Result<Option<EnvValue>, EnvError> {
    let refuse = |refusal| EnvError { var, refusal };
    let Some(raw) = raw else { return Ok(None) };
    let kind = var.kind();
    if matches!(kind, EnvKind::Handoff | EnvKind::Raw) {
        return Ok((!raw.is_empty()).then(|| EnvValue::Raw(raw.to_owned())));
    }
    let text = raw.to_str().ok_or_else(|| refuse(EnvRefusal::NotUtf8))?;
    if text.is_empty() {
        return match kind {
            EnvKind::SelectorPath => Err(refuse(EnvRefusal::EmptySelector)),
            EnvKind::Flag
            | EnvKind::Text
            | EnvKind::Path
            | EnvKind::AbsolutePath
            | EnvKind::Presence
            | EnvKind::Handoff
            | EnvKind::Raw => Ok(None),
        };
    }
    if text.trim() != text {
        return Err(refuse(EnvRefusal::SurroundingWhitespace(text.to_owned())));
    }
    match kind {
        EnvKind::Flag => match text {
            "1" | "true" => Ok(Some(EnvValue::Flag(true))),
            "0" | "false" => Ok(Some(EnvValue::Flag(false))),
            other => Err(refuse(EnvRefusal::NotAFlag(other.to_owned()))),
        },
        EnvKind::Presence => Ok(Some(EnvValue::Present)),
        EnvKind::AbsolutePath | EnvKind::SelectorPath => {
            if Path::new(text).is_absolute() {
                Ok(Some(EnvValue::Text(text.to_owned())))
            } else {
                Err(refuse(EnvRefusal::NotAbsolute(text.to_owned())))
            }
        }
        EnvKind::Text | EnvKind::Path => Ok(Some(EnvValue::Text(text.to_owned()))),
        EnvKind::Handoff | EnvKind::Raw => Ok(Some(EnvValue::Raw(raw.to_owned()))),
    }
}

/// This process's raw value of `var`, before the policy.
#[expect(
    clippy::disallowed_methods,
    reason = "the one production environment read; every other site asks here"
)]
fn raw(var: EnvVar) -> Option<OsString> {
    std::env::var_os(var.name())
}

/// The raw value of an indexed environment variable, before any text policy.
#[expect(
    clippy::disallowed_methods,
    reason = "the one production environment read for Git's indexed config family"
)]
fn raw_name(name: &OsStr) -> Option<OsString> {
    std::env::var_os(name)
}

/// Reads one variable from this process's environment under the policy.
///
/// # Errors
///
/// Every refusal [`resolve`] makes.
pub fn read(var: EnvVar) -> Result<Option<EnvValue>, EnvError> {
    resolve(var, raw(var).as_deref())
}

fn is_flag(kind: EnvKind) -> bool {
    kind == EnvKind::Flag
}

fn is_presence(kind: EnvKind) -> bool {
    kind == EnvKind::Presence
}

fn is_text(kind: EnvKind) -> bool {
    matches!(
        kind,
        EnvKind::Text | EnvKind::Path | EnvKind::AbsolutePath | EnvKind::SelectorPath
    )
}

fn is_path(kind: EnvKind) -> bool {
    matches!(
        kind,
        EnvKind::Path | EnvKind::AbsolutePath | EnvKind::SelectorPath | EnvKind::Handoff
    )
}

/// [`resolve`] for a flag, as a `bool`.
///
/// # Panics
///
/// When `var` is not a [`EnvKind::Flag`]: a caller asking the wrong question
/// of a closed table is a programming error with nothing to hand back.
///
/// # Errors
///
/// Every refusal [`resolve`] makes.
pub fn resolve_flag(var: EnvVar, raw: Option<&OsStr>) -> Result<Option<bool>, EnvError> {
    assert!(is_flag(var.kind()), "{var} is not a flag");
    Ok(resolve(var, raw)?.map(|value| match value {
        EnvValue::Flag(flag) => flag,
        EnvValue::Present | EnvValue::Text(_) | EnvValue::Raw(_) => {
            unreachable!("a flag resolves to a flag")
        }
    }))
}

/// [`resolve`] for a presence variable: whether it is set and non-empty.
///
/// # Panics
///
/// When `var` is not a [`EnvKind::Presence`], for the reason [`resolve_flag`]
/// gives.
///
/// # Errors
///
/// Every refusal [`resolve`] makes.
pub fn resolve_present(var: EnvVar, raw: Option<&OsStr>) -> Result<bool, EnvError> {
    assert!(is_presence(var.kind()), "{var} is not a presence variable");
    Ok(resolve(var, raw)?.is_some())
}

/// [`resolve`] for a UTF-8 text or path kind, as text.
///
/// # Panics
///
/// When `var` is a flag, presence or handoff variable, for the reason
/// [`resolve_flag`] gives.
///
/// # Errors
///
/// Every refusal [`resolve`] makes.
pub fn resolve_text(var: EnvVar, raw: Option<&OsStr>) -> Result<Option<String>, EnvError> {
    assert!(is_text(var.kind()), "{var} is not text-valued");
    Ok(resolve(var, raw)?.map(|value| match value {
        EnvValue::Text(text) => text,
        EnvValue::Flag(_) | EnvValue::Present | EnvValue::Raw(_) => {
            unreachable!("a text-valued kind resolves to text")
        }
    }))
}

/// [`resolve`] for a path kind, as a path.
///
/// # Panics
///
/// When `var` is not a path kind, for the reason [`resolve_flag`] gives.
///
/// # Errors
///
/// Every refusal [`resolve`] makes.
pub fn resolve_path(var: EnvVar, raw: Option<&OsStr>) -> Result<Option<PathBuf>, EnvError> {
    assert!(is_path(var.kind()), "{var} is not path-valued");
    Ok(resolve(var, raw)?.map(|value| match value {
        EnvValue::Text(text) => PathBuf::from(text),
        EnvValue::Raw(raw) => PathBuf::from(raw),
        EnvValue::Flag(_) | EnvValue::Present => {
            unreachable!("a path kind resolves to text or raw bytes")
        }
    }))
}

/// Reads a flag from this process's environment.
///
/// # Panics
///
/// As [`resolve_flag`].
///
/// # Errors
///
/// Every refusal [`resolve`] makes.
pub fn read_flag(var: EnvVar) -> Result<Option<bool>, EnvError> {
    resolve_flag(var, raw(var).as_deref())
}

/// Reads whether a presence variable is set in this process's environment.
///
/// # Panics
///
/// As [`resolve_present`].
///
/// # Errors
///
/// Every refusal [`resolve`] makes.
pub fn read_present(var: EnvVar) -> Result<bool, EnvError> {
    resolve_present(var, raw(var).as_deref())
}

/// Reads a text or path variable from this process's environment, as text.
///
/// # Panics
///
/// As [`resolve_text`].
///
/// # Errors
///
/// Every refusal [`resolve`] makes.
pub fn read_text(var: EnvVar) -> Result<Option<String>, EnvError> {
    resolve_text(var, raw(var).as_deref())
}

/// Reads a path variable from this process's environment.
///
/// # Panics
///
/// As [`resolve_path`].
///
/// # Errors
///
/// Every refusal [`resolve`] makes.
pub fn read_path(var: EnvVar) -> Result<Option<PathBuf>, EnvError> {
    resolve_path(var, raw(var).as_deref())
}

/// Resolves one byte-preserving value such as `PATH`, `SHELL` or
/// `GIT_CONFIG_COUNT`.
///
/// # Panics
///
/// When `var` is not a [`EnvKind::Raw`] or [`EnvKind::Handoff`] value.
pub fn resolve_os(var: EnvVar, raw: Option<&OsStr>) -> Result<Option<OsString>, EnvError> {
    assert!(
        matches!(var.kind(), EnvKind::Raw | EnvKind::Handoff),
        "{var} is not an OS-string variable"
    );
    Ok(resolve(var, raw)?.map(|value| match value {
        EnvValue::Raw(raw) => raw,
        EnvValue::Flag(_) | EnvValue::Present | EnvValue::Text(_) => {
            unreachable!("an OS-string variable resolves to a raw value")
        }
    }))
}

/// Reads one byte-preserving environment value such as `PATH`, `SHELL` or
/// `GIT_CONFIG_COUNT`.
///
/// # Panics
///
/// As [`resolve_os`].
pub fn read_os(var: EnvVar) -> Result<Option<OsString>, EnvError> {
    resolve_os(var, raw(var).as_deref())
}

/// Reads Git's indexed command-scope config pairs in index order, followed by
/// quoted `git -c` parameters, which override conflicting indexed pairs. Git
/// treats an unset or empty count as zero, requires both variables for every
/// index below the count, and rejects malformed counts or missing pairs. A
/// valueless `git -c` parameter is represented by `None`; its consumer decides
/// whether the key is boolean or requires a value.
///
/// # Errors
///
/// Returns an error when the count is not accepted by Git, an indexed key or
/// value is missing, a present pair is not valid UTF-8 for shepr's config
/// reader, or the quoted parameters are malformed.
pub fn read_git_config_parameters() -> io::Result<Vec<(String, Option<String>)>> {
    let count = read_os(EnvVar::GitConfigCount)?;
    let count = count
        .as_deref()
        .map(parse_git_config_count)
        .transpose()?
        .unwrap_or(0);
    let mut parameters = Vec::new();
    for index in 0..count {
        let key_name = format!("GIT_CONFIG_KEY_{index}");
        let value_name = format!("GIT_CONFIG_VALUE_{index}");
        let key = raw_name(OsStr::new(&key_name)).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("missing {key_name} for Git command-scope config"),
            )
        })?;
        let value = raw_name(OsStr::new(&value_name)).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("missing {value_name} for Git command-scope config"),
            )
        })?;
        let key = key.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{key_name} is not valid UTF-8"),
            )
        })?;
        let value = value.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{value_name} is not valid UTF-8"),
            )
        })?;
        parameters.push((key.to_owned(), Some(value.to_owned())));
    }
    if let Some(raw) = read_os(EnvVar::GitConfigParameters)? {
        let text = raw.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "GIT_CONFIG_PARAMETERS is not valid UTF-8",
            )
        })?;
        parameters.extend(parse_git_config_parameters(text)?);
    }
    Ok(parameters)
}

/// Parse Git's single-quoted command-scope assignments. Each key and value
/// is quoted separately in the current form; the older quoted `key=value`
/// form is also accepted. A quote inside a word is encoded as `'\''` and an
/// exclamation mark as `'\!'`. A valueless key (Git's implicit boolean) reads
/// as no value, leaving its meaning to the config consumer.
fn parse_git_config_parameters(text: &str) -> io::Result<Vec<(String, Option<String>)>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "bogus format in GIT_CONFIG_PARAMETERS",
        )
    };
    let mut rest = text.trim_ascii_start();
    let mut pairs = Vec::new();
    while !rest.is_empty() {
        let (key, tail) = parse_git_single_quote(rest).ok_or_else(invalid)?;
        let (name, value, tail) = if let Some(tail) = tail.strip_prefix('=') {
            let (value, tail) = if tail.starts_with('\'') {
                let (value, tail) = parse_git_single_quote(tail).ok_or_else(invalid)?;
                (Some(value), tail)
            } else if tail.is_empty() || tail.as_bytes()[0].is_ascii_whitespace() {
                (None, tail)
            } else {
                return Err(invalid());
            };
            (key, value, tail)
        } else if let Some((name, value)) = key.split_once('=') {
            (name.to_owned(), Some(value.to_owned()), tail)
        } else {
            (key, None, tail)
        };
        if name.is_empty() || (!tail.is_empty() && !tail.as_bytes()[0].is_ascii_whitespace()) {
            return Err(invalid());
        }
        pairs.push((name, value));
        rest = tail.trim_ascii_start();
    }
    Ok(pairs)
}

fn parse_git_single_quote(input: &str) -> Option<(String, &str)> {
    let mut rest = input.strip_prefix('\'')?;
    let mut value = String::new();
    loop {
        let end = rest.find('\'')?;
        value.push_str(&rest[..end]);
        rest = &rest[end + 1..];
        // Git's `sq_quote` escapes both `'` and `!` outside the quotes, and
        // its `sq_dequote_step` accepts either escape only when the quoted
        // part resumes right after it.
        let escaped = ['\'', '!'].into_iter().find_map(|quoted| {
            let tail = rest.strip_prefix('\\')?.strip_prefix(quoted)?;
            Some((quoted, tail.strip_prefix('\'')?))
        });
        if let Some((quoted, tail)) = escaped {
            value.push(quoted);
            rest = tail;
        } else {
            return Some((value, rest));
        }
    }
}

fn parse_git_config_count(raw: &OsStr) -> io::Result<usize> {
    let text = raw.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "GIT_CONFIG_COUNT is not valid UTF-8",
        )
    })?;
    if text.is_empty() {
        return Ok(0);
    }
    let bytes = text.as_bytes();
    let mut index = 0;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    let negative = match bytes.get(index) {
        Some(b'+') => {
            index += 1;
            false
        }
        Some(b'-') => {
            index += 1;
            true
        }
        _ => false,
    };
    let first_digit = index;
    let mut magnitude = 0_usize;
    while let Some(byte) = bytes.get(index).filter(|byte| byte.is_ascii_digit()) {
        magnitude = magnitude
            .checked_mul(10)
            .and_then(|count| count.checked_add(usize::from(*byte - b'0')))
            .ok_or_else(too_many_git_config_parameters)?;
        index += 1;
    }
    if index == first_digit || index != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "GIT_CONFIG_COUNT is not a valid decimal count",
        ));
    }
    let count = if negative {
        0_usize.wrapping_sub(magnitude)
    } else {
        magnitude
    };
    if count > i32::MAX as usize {
        return Err(too_many_git_config_parameters());
    }
    Ok(count)
}

fn too_many_git_config_parameters() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "GIT_CONFIG_COUNT exceeds Git's maximum entry count",
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt as _;

    use super::*;

    /// The table: every interpreted variable's name and kind, spelled out.
    #[test]
    fn every_variable_has_its_documented_name_and_kind() {
        use EnvKind::{AbsolutePath, Flag, Handoff, Path, Presence, Raw, SelectorPath, Text};
        let table: &[(EnvVar, &str, EnvKind)] = &[
            (EnvVar::SheprConfigPath, "SHEPR_CONFIG_PATH", Path),
            (EnvVar::SheprSocketPath, "SHEPR_SOCKET_PATH", SelectorPath),
            (
                EnvVar::SheprClientSocketPath,
                "SHEPR_CLIENT_SOCKET_PATH",
                SelectorPath,
            ),
            (EnvVar::SheprPaneId, "SHEPR_PANE_ID", Text),
            (EnvVar::SheprEnv, "SHEPR_ENV", Text),
            (EnvVar::SheprStartupCwd, "SHEPR_STARTUP_CWD", Handoff),
            (EnvVar::SheprLog, "SHEPR_LOG", Text),
            (
                EnvVar::SheprDebugOscEvidence,
                "SHEPR_DEBUG_OSC_EVIDENCE",
                Flag,
            ),
            (EnvVar::Home, "HOME", AbsolutePath),
            (EnvVar::XdgConfigHome, "XDG_CONFIG_HOME", AbsolutePath),
            (EnvVar::XdgStateHome, "XDG_STATE_HOME", AbsolutePath),
            (EnvVar::XdgRuntimeDir, "XDG_RUNTIME_DIR", AbsolutePath),
            (EnvVar::Shell, "SHELL", Raw),
            (EnvVar::Path, "PATH", Raw),
            (EnvVar::SshAuthSock, "SSH_AUTH_SOCK", Path),
            (EnvVar::SshConnection, "SSH_CONNECTION", Presence),
            (EnvVar::SshTty, "SSH_TTY", Presence),
            (EnvVar::VscodeIpcHookCli, "VSCODE_IPC_HOOK_CLI", Presence),
            (EnvVar::Tmux, "TMUX", Presence),
            (EnvVar::TermProgram, "TERM_PROGRAM", Text),
            (EnvVar::WeztermPane, "WEZTERM_PANE", Presence),
            (EnvVar::WaylandDisplay, "WAYLAND_DISPLAY", Presence),
            (EnvVar::Display, "DISPLAY", Presence),
            (EnvVar::PiCodingAgentDir, "PI_CODING_AGENT_DIR", Path),
            (EnvVar::PiConfigDir, "PI_CONFIG_DIR", Path),
            (EnvVar::ClaudeConfigDir, "CLAUDE_CONFIG_DIR", Path),
            (EnvVar::CodexHome, "CODEX_HOME", Path),
            (EnvVar::KimiCodeHome, "KIMI_CODE_HOME", Path),
            (EnvVar::CopilotHome, "COPILOT_HOME", Path),
            (EnvVar::QoderConfigDir, "QODER_CONFIG_DIR", Path),
            (EnvVar::QwenHome, "QWEN_HOME", Path),
            (EnvVar::CursorConfigDir, "CURSOR_CONFIG_DIR", Path),
            (
                EnvVar::AntigravityCliConfigDir,
                "ANTIGRAVITY_CLI_CONFIG_DIR",
                Path,
            ),
            (EnvVar::GrokHome, "GROK_HOME", Path),
            (
                EnvVar::GitCeilingDirectories,
                "GIT_CEILING_DIRECTORIES",
                Text,
            ),
            (EnvVar::GitConfigGlobal, "GIT_CONFIG_GLOBAL", Path),
            (EnvVar::GitConfigSystem, "GIT_CONFIG_SYSTEM", Path),
            (EnvVar::GitConfigNoSystem, "GIT_CONFIG_NOSYSTEM", Text),
            (EnvVar::GitConfigCount, "GIT_CONFIG_COUNT", Raw),
            (EnvVar::GitConfigParameters, "GIT_CONFIG_PARAMETERS", Raw),
        ];
        assert_eq!(
            table.iter().map(|(var, _, _)| *var).collect::<Vec<_>>(),
            EnvVar::ALL,
            "the table covers every variant, in order"
        );
        for (var, name, kind) in table {
            assert_eq!(var.name(), *name);
            assert_eq!(var.kind(), *kind, "{name}");
            assert_eq!(var.to_string(), *name);
            assert_eq!(AsRef::<OsStr>::as_ref(var), OsStr::new(name));
        }
    }

    #[test]
    fn registry_covers_only_canonical_indexed_git_config_names() {
        for name in [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_KEY_12",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_VALUE_12",
        ] {
            assert!(is_registered_name(OsStr::new(name)), "{name}");
        }
        for name in [
            "GIT_CONFIG_KEY_",
            "GIT_CONFIG_KEY_00",
            "GIT_CONFIG_KEY_x",
            "GIT_CONFIG_VALUE_00",
            "GIT_CONFIG_VALUE_1_EXTRA",
            "OTHER_GIT_CONFIG_KEY_0",
        ] {
            assert!(!is_registered_name(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn git_config_parameters_accept_git_quoted_assignments() {
        let parsed = parse_git_config_parameters(
            "'branch.main.remote'='team '\t'branch.main.merge'='refs/heads/it'\\''s' 'core.bare=false'",
        )
        .expect("valid Git quoted parameters");
        assert_eq!(
            parsed,
            [
                ("branch.main.remote".into(), Some("team ".into())),
                ("branch.main.merge".into(), Some("refs/heads/it's".into())),
                ("core.bare".into(), Some("false".into())),
            ]
        );
        // Verbatim from `git -c 'alias.x=!printenv GIT_CONFIG_PARAMETERS'
        // -c "a.b=it's" -c c.d -c e.f= -c 'g.h=x y' x`.
        let parsed = parse_git_config_parameters(
            "'alias.x'=''\\!'printenv GIT_CONFIG_PARAMETERS' 'a.b'='it'\\''s' 'c.d'= 'e.f'='' 'g.h'='x y'",
        )
        .expect("Git's own encoding parses");
        assert_eq!(
            parsed,
            [
                (
                    "alias.x".into(),
                    Some("!printenv GIT_CONFIG_PARAMETERS".into())
                ),
                ("a.b".into(), Some("it's".into())),
                ("c.d".into(), None),
                ("e.f".into(), Some(String::new())),
                ("g.h".into(), Some("x y".into())),
            ]
        );
        for input in [
            "'unterminated",
            "'key'=value",
            "'key'='value'x",
            "''='value'",
            "'key'='a'\\x'b'",
        ] {
            assert!(parse_git_config_parameters(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn git_config_count_uses_git_decimal_count_rules() {
        assert_eq!(
            parse_git_config_count(OsStr::new("")).expect("empty count"),
            0
        );
        assert_eq!(
            parse_git_config_count(OsStr::new("0")).expect("zero count"),
            0
        );
        assert_eq!(
            parse_git_config_count(OsStr::new("+2")).expect("signed count"),
            2
        );
        assert_eq!(
            parse_git_config_count(OsStr::new(" 2")).expect("leading space"),
            2
        );
        assert_eq!(
            parse_git_config_count(OsStr::new("-0")).expect("negative zero"),
            0
        );
        let max_count = usize::MAX;
        let negative_unsigned_max = format!("-{max_count}");
        assert_eq!(
            parse_git_config_count(OsStr::new(&negative_unsigned_max))
                .expect("strtoul wraps a negative unsigned count"),
            1
        );
        for count in ["  ", "2 ", "+", "-1", "2147483648"] {
            assert!(
                parse_git_config_count(OsStr::new(count)).is_err(),
                "{count:?}"
            );
        }
    }

    #[test]
    fn the_child_vocabulary_only_overlaps_on_inherited_shell_inputs() {
        let names: Vec<&str> = ChildEnv::ALL.iter().copied().map(ChildEnv::name).collect();
        assert_eq!(
            names,
            [
                "TERM",
                "COLORTERM",
                "TERM_PROGRAM_VERSION",
                "SHELL",
                "PATH",
                "SHEPR_BIN_PATH",
                "SHEPR_ACTIVE_WORKSPACE_ID",
                "SHEPR_ACTIVE_TAB_ID",
                "SHEPR_ACTIVE_PANE_ID",
                "SHEPR_ACTIVE_PANE_CWD",
                "SSH_ASKPASS",
                "SSH_ASKPASS_REQUIRE",
                "ITERM_SESSION_ID",
                "LC_TERMINAL",
                "LC_TERMINAL_VERSION",
                "KITTY_WINDOW_ID",
                "WT_SESSION",
                "TMUX_PANE",
                "STY",
                "ZELLIJ",
                "ZELLIJ_SESSION_NAME",
                "ZELLIJ_PANE_ID",
                "CLAUDECODE",
                "CLAUDE_CODE_CHILD_SESSION",
                "CLAUDE_CODE_SESSION_ID",
                "CLAUDE_CODE_MESSAGING_TOKEN",
                "CODEX_THREAD_ID",
                "OMPCODE",
            ]
        );
        let interpreted: Vec<&str> = EnvVar::ALL.iter().copied().map(EnvVar::name).collect();
        let shared: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| interpreted.contains(name))
            .collect();
        assert_eq!(shared, ["SHELL", "PATH"]);
        let mut all: Vec<&str> = EnvVar::ALL
            .iter()
            .copied()
            .map(EnvVar::name)
            .chain(names.iter().copied())
            .collect();
        let total = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), total - shared.len(), "only shell inputs overlap");
    }

    /// The shared policy, over every variable: unset is unset and so is empty
    /// (except for selectors, below), padding and non-UTF-8 refuse by name.
    #[test]
    fn the_policy_holds_for_every_variable() {
        let non_utf8 = OsStr::from_bytes(&[b'/', b'a', 0xff]);
        for &var in EnvVar::ALL {
            assert_eq!(resolve(var, None), Ok(None), "{var} unset");
            let kind = var.kind();
            if kind != EnvKind::SelectorPath {
                assert_eq!(resolve(var, Some(OsStr::new(""))), Ok(None), "{var} empty");
            }
            if matches!(kind, EnvKind::Handoff | EnvKind::Raw) {
                // Preserved byte for byte: nothing but empty is special.
                for raw in [non_utf8, OsStr::new(" /a b ")] {
                    let resolved = if kind == EnvKind::Handoff {
                        resolve_path(var, Some(raw)).map(|path| path.map(PathBuf::into_os_string))
                    } else {
                        resolve_os(var, Some(raw))
                    };
                    assert_eq!(resolved, Ok(Some(raw.to_os_string())), "{var}");
                }
                continue;
            }
            let error = resolve(var, Some(non_utf8)).expect_err("non-UTF-8 refuses");
            assert_eq!(error.refusal, EnvRefusal::NotUtf8);
            assert!(error.to_string().contains(var.name()), "{error}");
            for padded in [" 1", "1 ", "\t1", "  ", " /abs"] {
                let error = resolve(var, Some(OsStr::new(padded)))
                    .expect_err("surrounding whitespace refuses");
                assert_eq!(
                    error.refusal,
                    EnvRefusal::SurroundingWhitespace(padded.to_owned())
                );
                assert!(error.to_string().contains(var.name()), "{error}");
            }
            let good = match kind {
                EnvKind::Flag => "1",
                EnvKind::AbsolutePath | EnvKind::SelectorPath => "/abs/value",
                EnvKind::Text
                | EnvKind::Path
                | EnvKind::Presence
                | EnvKind::Handoff
                | EnvKind::Raw => "value",
            };
            assert!(
                resolve(var, Some(OsStr::new(good)))
                    .expect("clean")
                    .is_some(),
                "{var}"
            );
        }
    }

    /// A selector is the one kind whose empty value is refused, naming the
    /// variable and the unset spelling.
    #[test]
    fn an_empty_selector_is_refused_not_unset() {
        let selectors: Vec<EnvVar> = EnvVar::ALL
            .iter()
            .copied()
            .filter(|var| var.kind() == EnvKind::SelectorPath)
            .collect();
        assert_eq!(
            selectors,
            [EnvVar::SheprSocketPath, EnvVar::SheprClientSocketPath]
        );
        for var in selectors {
            let error = resolve(var, Some(OsStr::new(""))).expect_err("an empty selector refuses");
            assert_eq!(error.refusal, EnvRefusal::EmptySelector);
            let rendered = error.to_string();
            assert!(
                rendered.contains(&format!("unset {}", var.name())),
                "{rendered}"
            );
        }
    }

    /// A flag accepts exactly four spellings.
    #[test]
    fn a_flag_accepts_exactly_one_zero_true_false() {
        for &var in EnvVar::ALL.iter().filter(|var| var.kind() == EnvKind::Flag) {
            for (raw, want) in [("1", true), ("true", true), ("0", false), ("false", false)] {
                assert_eq!(resolve_flag(var, Some(OsStr::new(raw))), Ok(Some(want)));
            }
            for bad in ["TRUE", "True", "yes", "on", "2", "no", "FALSE"] {
                let error = resolve_flag(var, Some(OsStr::new(bad))).expect_err(bad);
                assert_eq!(error.refusal, EnvRefusal::NotAFlag(bad.to_owned()));
            }
        }
    }

    #[test]
    fn an_absolute_path_kind_refuses_a_relative_path() {
        for &var in EnvVar::ALL
            .iter()
            .filter(|var| matches!(var.kind(), EnvKind::AbsolutePath | EnvKind::SelectorPath))
        {
            let error = resolve_path(var, Some(OsStr::new("relative/dir")))
                .expect_err("a relative path refuses");
            assert_eq!(
                error.refusal,
                EnvRefusal::NotAbsolute("relative/dir".to_owned())
            );
            assert!(error.to_string().contains(var.name()), "{error}");
            assert_eq!(
                resolve_path(var, Some(OsStr::new("/abs/dir"))),
                Ok(Some(PathBuf::from("/abs/dir")))
            );
        }
        // A plain path kind leaves relative paths to its owning site.
        assert_eq!(
            resolve_path(EnvVar::SheprConfigPath, Some(OsStr::new("rel.toml"))),
            Ok(Some(PathBuf::from("rel.toml")))
        );
    }

    #[test]
    fn presence_is_set_and_non_empty() {
        assert_eq!(resolve_present(EnvVar::Tmux, None), Ok(false));
        assert_eq!(
            resolve_present(EnvVar::Tmux, Some(OsStr::new(""))),
            Ok(false)
        );
        assert_eq!(
            resolve_present(EnvVar::Tmux, Some(OsStr::new("/tmp/tmux-1/default,1,0"))),
            Ok(true)
        );
    }

    #[test]
    fn a_refusal_converts_to_an_invalid_input_io_error_naming_the_variable() {
        let error = resolve(EnvVar::Home, Some(OsStr::new("relative")))
            .expect_err("a relative HOME refuses");
        let io_error = io::Error::from(error);
        assert_eq!(io_error.kind(), io::ErrorKind::InvalidInput);
        assert!(io_error.to_string().contains("HOME"), "{io_error}");
    }
}
