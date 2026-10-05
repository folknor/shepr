//! A development tool that previews the colours the shepr client derives from
//! a terminal theme (`shepr_term::host_tint`): the UI palette and every host
//! hue's sidebar colours, drawn on the surfaces they are used on with their
//! contrasts, so the derivation and the contrast targets in
//! `crates/shepr-term/src/limits.rs` can be judged by eye.
//!
//! With no theme arguments it asks the terminal it runs in, as the client
//! does. Any of `--background`, `--foreground` or `--ansi` previews that theme
//! instead, without asking, so other themes can be tried in one terminal.
//! It is not installed: run it with `brokkr run shepr-palette-preview -- ARGS`.

mod limits;
mod query;
mod render;

use std::process::ExitCode;

use shepr_term::RgbColor;
use shepr_term::host::{DefaultColorKind, TerminalTheme};
use shepr_term::host_tint::HostHue;

const USAGE: &str = "\
usage: shepr-palette-preview [OPTIONS]

Previews the colours the shepr client derives from a terminal theme.
With no theme option the terminal this runs in is asked for its colours.

theme options (any of them skips the query):
  --background COLOR    the terminal background (needed to derive anything)
  --foreground COLOR    the terminal foreground
  --ansi INDEX=COLOR    a palette slot, 0 to 255; repeatable

other options:
  --accent HUE          the local server's hue, the client's accent (default blue)
  --hues HUE,HUE,...    the hues derived together for the sidebar, as the
                        palettes in client.toml (default: all of them)
  -h, --help            this text

COLOR is #rrggbb, #rgb or rgb:rrrr/gggg/bbbb.
HUE is red, orange, yellow, green, cyan, blue, purple or magenta.
";

fn main() -> ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("shepr-palette-preview: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let (theme, source) = match options.theme {
        Some(theme) => (theme, "from the arguments"),
        None => match query::query_terminal_theme() {
            Ok(theme) => (theme, "reported by this terminal"),
            Err(error) => {
                eprintln!("shepr-palette-preview: {error}");
                return ExitCode::FAILURE;
            }
        },
    };
    let preview = render::Preview {
        theme: &theme,
        source,
        accent: options.accent,
        hues: &options.hues,
    };
    match preview.render() {
        Some(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        None => {
            eprintln!(
                "shepr-palette-preview: no background ({source}); the client derives nothing \
                 and draws with the terminal's own ANSI colours"
            );
            ExitCode::FAILURE
        }
    }
}

struct Options {
    /// The theme given on the command line, or `None` to ask the terminal.
    theme: Option<TerminalTheme>,
    accent: HostHue,
    hues: Vec<HostHue>,
}

impl Options {
    /// The options in `args`, or `None` when help was asked for.
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Self>, String> {
        let mut theme: Option<TerminalTheme> = None;
        let mut accent = HostHue::Blue;
        let mut hues = HostHue::ALL.to_vec();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "-h" | "--help" => return Ok(None),
                "--background" | "--foreground" => {
                    let kind = if arg == "--background" {
                        DefaultColorKind::Background
                    } else {
                        DefaultColorKind::Foreground
                    };
                    let color = parse_color(&value()?)?;
                    theme = Some(theme.unwrap_or_default().with_color(kind, color));
                }
                "--ansi" => {
                    let value = value()?;
                    let (index, color) = value
                        .split_once('=')
                        .ok_or_else(|| format!("--ansi takes INDEX=COLOR, not {value}"))?;
                    let index = index
                        .parse::<u8>()
                        .map_err(|_| format!("{index} is not a palette index 0 to 255"))?;
                    let color = parse_color(color)?;
                    theme = Some(theme.unwrap_or_default().with_palette_color(index, color));
                }
                "--accent" => accent = parse_hue(&value()?)?,
                "--hues" => {
                    hues = value()?
                        .split(',')
                        .map(parse_hue)
                        .collect::<Result<_, _>>()?;
                }
                _ => return Err(format!("unknown argument {arg}")),
            }
        }
        // The client derives the local hue's sidebar colours with the rest,
        // and the focused pane border takes its accent.
        if !hues.contains(&accent) {
            hues.push(accent);
        }
        hues.sort_by_key(|hue| HostHue::ALL.iter().position(|each| each == hue));
        hues.dedup();
        Ok(Some(Self {
            theme,
            accent,
            hues,
        }))
    }
}

fn parse_color(value: &str) -> Result<RgbColor, String> {
    let spelled = if value.starts_with("rgb:") || value.starts_with('#') {
        value.to_owned()
    } else {
        format!("#{value}")
    };
    shepr_term::seq::parse_rgb_color(&spelled)
        .ok_or_else(|| format!("{value} is not a colour (#rrggbb, #rgb or rgb:rrrr/gggg/bbbb)"))
}

fn parse_hue(value: &str) -> Result<HostHue, String> {
    HostHue::ALL
        .into_iter()
        .find(|hue| hue.name() == value)
        .ok_or_else(|| format!("{value} is not a hue"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Options>, String> {
        Options::parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn no_theme_argument_asks_the_terminal() {
        let options = parse(&[]).expect("valid").expect("not help");
        assert!(options.theme.is_none());
        assert_eq!(options.accent, HostHue::Blue);
        assert_eq!(options.hues, HostHue::ALL);
    }

    #[test]
    fn theme_arguments_build_the_theme() {
        let options = parse(&[
            "--background",
            "1e1e2e",
            "--foreground",
            "#cdd6f4",
            "--ansi",
            "2=rgb:a6a6/e3e3/a1a1",
            "--accent",
            "green",
            "--hues",
            "purple,red",
        ])
        .expect("valid")
        .expect("not help");
        let theme = options.theme.expect("a theme was given");
        assert_eq!(
            theme.background,
            Some(RgbColor {
                r: 0x1e,
                g: 0x1e,
                b: 0x2e
            })
        );
        assert_eq!(
            theme.palette[2],
            Some(RgbColor {
                r: 0xa6,
                g: 0xe3,
                b: 0xa1
            })
        );
        // The accent hue joins the sidebar hues, in the config's order.
        assert_eq!(
            options.hues,
            [HostHue::Red, HostHue::Green, HostHue::Purple]
        );
    }

    #[test]
    fn bad_arguments_are_refused() {
        assert!(parse(&["--background"]).is_err());
        assert!(parse(&["--background", "nope"]).is_err());
        assert!(parse(&["--ansi", "300=#000000"]).is_err());
        assert!(parse(&["--accent", "teal"]).is_err());
        assert!(parse(&["--frobnicate"]).is_err());
        assert!(matches!(parse(&["--help"]), Ok(None)));
    }
}
