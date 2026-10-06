//! `shepr man [TOPIC]`: the bundled user manuals.
//!
//! Every end-user manual in `docs/` is compiled into the binary with
//! `include_str!` and rendered to the terminal through the markdown-to-ANSI
//! renderer in `render`. With no topic, the command lists what is available;
//! with a topic, it renders that manual, colour off when stdout is not a
//! terminal or `NO_COLOR` is set. The manuals travel inside the binary, so an
//! installed `shepr` reads them with no source tree.

mod render;

use clap::builder::{PossibleValue, PossibleValuesParser, TypedValueParser as _};
use clap::{Arg, ArgMatches};
use shepr_launch::invocation::{COMMAND_MAN, PROGRAM_NAME};

/// The clap id of the optional topic argument.
const TOPIC_ARG: &str = "topic";

/// `shepr man [TOPIC]`: render one bundled manual, or list the topics when
/// `topic` is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Command {
    pub(crate) topic: Option<Topic>,
}

/// One bundled manual. Each is an end-user document in `docs/`; the
/// contributor references in `reference/` are not topics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Topic {
    /// Copying and pasting (docs/clipboard.md).
    Clipboard,
    /// The two config files and every setting (docs/config.md).
    Config,
}

impl Topic {
    /// Every topic, in listing order.
    const ALL: &'static [Self] = &[Self::Clipboard, Self::Config];

    /// The name typed on the command line and shown in the listing. The value
    /// parser is built from these names, so the listing and the parser cannot
    /// disagree.
    fn name(self) -> &'static str {
        match self {
            Self::Clipboard => "clipboard",
            Self::Config => "config",
        }
    }

    /// One-line summary shown in the topic listing and in `--help`.
    fn summary(self) -> &'static str {
        match self {
            Self::Clipboard => {
                "copying from panes, both selections, and how text reaches the clipboard"
            }
            Self::Config => {
                "client.toml and server.toml: where they live, who reads them, every setting"
            }
        }
    }

    /// The bundled markdown, compiled into the binary so `shepr man` needs no
    /// source tree at runtime. A new end-user manual in `docs/` gets a variant
    /// above and an arm here; the test below holds the two together.
    fn content(self) -> &'static str {
        match self {
            Self::Clipboard => include_str!("../../docs/clipboard.md"),
            Self::Config => include_str!("../../docs/config.md"),
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|topic| topic.name() == name)
    }
}

/// The optional `TOPIC` positional of `shepr man`. Its possible values are the
/// topic names, so an unknown topic is a usage error (exit 2) and `--help`
/// lists every topic with its summary.
pub(super) fn topic_argument() -> Arg {
    let names = PossibleValuesParser::new(
        Topic::ALL
            .iter()
            .map(|topic| PossibleValue::new(topic.name()).help(topic.summary())),
    );
    Arg::new(TOPIC_ARG)
        .value_name("TOPIC")
        .help("Manual to display; omit to list the available topics")
        .value_parser(names.try_map(|name: String| {
            Topic::from_name(&name).ok_or_else(|| format!("{name:?} is not a manual topic"))
        }))
}

pub(super) fn parse(matches: &ArgMatches) -> Option<Command> {
    let topic = super::matches::try_value::<Topic>(matches, TOPIC_ARG).ok()?;
    Some(Command { topic })
}

/// Renders a topic, or lists the topics when none is given. Output goes
/// through the CLI's `print!`, which restores the default SIGPIPE disposition
/// first, so a pager or `head` that closes the pipe early ends the process
/// quietly instead of failing a write.
///
/// Colour is decided only when a topic is rendered, so listing the topics never
/// reads the environment.
pub(super) fn run(command: &Command) -> super::CliResult<i32> {
    let out = match command.topic {
        Some(topic) => render::render(topic.content(), !super::color_enabled(&std::io::stdout())?),
        None => list_topics(),
    };
    print!("{out}");
    Ok(0)
}

fn list_topics() -> String {
    let width = Topic::ALL
        .iter()
        .map(|topic| topic.name().len())
        .max()
        .unwrap_or(0);
    let mut out =
        format!("Bundled docs. Run `{PROGRAM_NAME} {COMMAND_MAN} <topic>` to read one.\n\n");
    for topic in Topic::ALL {
        out.push_str(&format!(
            "  {name:width$}  {summary}\n",
            name = topic.name(),
            summary = topic.summary()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{CliCommand, Launch};

    impl Topic {
        /// The repository path of the manual a topic bundles. Kept exhaustive
        /// beside [`Topic::content`] so the bundling test can prove every manual
        /// in `docs/` has exactly one topic.
        fn document_path(self) -> &'static str {
            match self {
                Self::Clipboard => "docs/clipboard.md",
                Self::Config => "docs/config.md",
            }
        }
    }

    /// Every end-user manual in `docs/` is bundled by exactly one topic, the
    /// contributor-only references in `reference/` by none, and each topic's
    /// `include_str!` names the document its topic declares.
    #[test]
    fn every_user_manual_is_available_as_a_man_topic() {
        let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut manuals = ["docs", "reference"]
            .into_iter()
            .flat_map(|folder| {
                let directory = repository.join(folder);
                std::fs::read_dir(&directory)
                    .unwrap_or_else(|error| panic!("reading {}: {error}", directory.display()))
                    .filter_map(move |entry| {
                        let path = entry.expect("manual entry").path();
                        path.extension()
                            .is_some_and(|extension| extension == "md")
                            .then(|| {
                                format!(
                                    "{folder}/{}",
                                    path.file_name().expect("manual filename").to_string_lossy()
                                )
                            })
                    })
            })
            .collect::<Vec<_>>();
        const CONTRIBUTOR_ONLY: [&str; 2] = [
            "reference/session-save-shutdown.md",
            "reference/technical-implementation-spec.md",
        ];
        for excluded in CONTRIBUTOR_ONLY {
            assert!(
                manuals.iter().any(|manual| manual.as_str() == excluded),
                "expected contributor-only reference {excluded} to exist"
            );
        }
        manuals.retain(|manual| !CONTRIBUTOR_ONLY.contains(&manual.as_str()));
        let mut topics = Topic::ALL
            .iter()
            .map(|topic| topic.document_path().to_owned())
            .collect::<Vec<_>>();
        manuals.sort();
        topics.sort();
        assert_eq!(
            topics, manuals,
            "every manual in docs/ and reference/ must be bundled by exactly one Topic, except the contributor-only references"
        );

        let source_path = repository.join("src/cli/man.rs");
        let source = std::fs::read_to_string(&source_path)
            .unwrap_or_else(|error| panic!("reading {}: {error}", source_path.display()));
        for topic in Topic::ALL {
            let arm = format!(
                "Self::{topic:?} => include_str!(\"../../{}\")",
                topic.document_path()
            );
            assert!(
                source.contains(&arm),
                "{} must render the document named by its topic: expected `{arm}` in content()",
                topic.name()
            );
        }
    }

    /// The listing and the parser are one authority: every name `shepr man`
    /// lists parses, through the real command line, back to its own topic.
    #[test]
    fn every_listed_topic_name_parses_back_to_its_topic() {
        let listing = list_topics();
        for topic in Topic::ALL.iter().copied() {
            let name = topic.name();
            let launch = crate::cli::tests::parse(&["man", name]);
            assert!(
                matches!(
                    &launch,
                    Launch::Cli(command)
                        if matches!(
                            &**command,
                            CliCommand::Man(Command { topic: Some(parsed) }) if *parsed == topic
                        )
                ),
                "`shepr man {name}` does not parse back to {topic:?}"
            );
            assert!(
                listing.contains(name) && listing.contains(topic.summary()),
                "the listing omits {name}"
            );
        }
        let launch = crate::cli::tests::parse(&["man"]);
        assert!(
            matches!(
                &launch,
                Launch::Cli(command)
                    if matches!(&**command, CliCommand::Man(Command { topic: None }))
            ),
            "a bare `shepr man` lists the topics"
        );
    }
}
