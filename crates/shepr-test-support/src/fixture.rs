//! `shepr-fixture`: the one program tests spawn when what they need is a
//! process, rather than a program whose own behaviour is under test.
//!
//! Tests used to borrow those processes off the host: `sh`, `bash`, `sleep`,
//! `printf`, `cat`, `yes`, `head`, `tr`, `ps`, and authored shell scripts
//! standing in for clipboard helpers or a remote shepr. That made each
//! fixture's behaviour the host's to define (fractional `sleep`, `printf` as an
//! executable rather than a builtin, `ps -o sid=`), and a machine missing one
//! of them failed a test about something else. This program is built by the
//! workspace, so a fixture depends only on what the repository builds.
//!
//! The program is a `[[bin]]` of this dev-only crate rather than of each
//! crate that spawns processes, so no production crate carries test code.
//! Cargo builds it whenever this package is among the tested packages, which
//! `brokkr check` always is, and `brokkr.toml` names it in `build_packages` so
//! a narrower `brokkr test` builds it too. Tests find it beside their own
//! executable through [`path`], which refuses, naming how to build it, when it
//! is absent.
//!
//! # The grammar
//!
//! The whole grammar is this module: [`Step`] is what a test writes, its
//! encoding into argv tokens and the parse back are here, and so is the
//! interpreter the binary runs. A script is a sequence of steps run in order;
//! the process exits 0 after the last one unless a step ended it first.
//!
//! | token form | step |
//! |---|---|
//! | `sleep <seconds>` | [`Step::Sleep`] |
//! | `print <text>` | [`Step::Print`] |
//! | `print-err <text>` | [`Step::PrintErr`] |
//! | `print-args` | [`Step::PrintArgs`] |
//! | `print-arg <n>` | [`Step::PrintArg`] |
//! | `print-pid` | [`Step::PrintPid`] |
//! | `print-sid` | [`Step::PrintSid`] |
//! | `print-env <name>` | [`Step::PrintEnv`] |
//! | `fill <byte> <count>` | [`Step::Fill`] |
//! | `cat` | [`Step::Cat`] |
//! | `drain` | [`Step::Drain`] |
//! | `read-line` | [`Step::ReadLine`] |
//! | `to <path>` | [`Step::To`] |
//! | `close-stdout` | [`Step::CloseStdout`] |
//! | `cd <path>` | [`Step::Cd`] |
//! | `ignore <signal>` | [`Step::Ignore`] |
//! | `raise <signal>` | [`Step::Raise`] |
//! | `limit-file-size <bytes>` | [`Step::LimitFileSize`] |
//! | `spawn <argv0> <seconds> <held>` | [`Step::Spawn`] |
//! | `wait` | [`Step::Wait`] |
//! | `exit <code>` | [`Step::Exit`] |
//! | `exec <count> <program> <arg>...` | [`Step::Exec`] |
//! | `when <count> <operand>... <step>... end` | [`Step::When`] |
//!
//! Every form has a fixed arity or states its own count, so a script needs no
//! quoting and no delimiter beyond `when`'s `end`.
//!
//! # Direct and stand-in invocation
//!
//! Invoked directly, `shepr-fixture <script> [-- <operand>...]` runs the
//! script, and the tokens after `--` are its operands.
//!
//! Some production code launches a program by a name or path it chooses and
//! appends argv of its own: a clipboard helper, a remote `shepr`, a pane
//! shell, an agent found through `PATH`. For those, [`stand_in`] links this
//! program into a directory under the wanted name and writes the script beside
//! it, at `<name>.fixture`. A process whose own executable has such a file
//! beside it runs that script, and its whole argv after the program name is
//! the operands. The link is a hard link, so the process's name, its
//! `/proc/<pid>/exe` and its `argv[0]` are all the stand-in's name, and the
//! script is found however the stand-in was reached, `PATH` included.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

/// The fixture program's file name, and so its process name: fifteen bytes or
/// fewer, so the kernel's process name is not truncated.
pub const FIXTURE_NAME: &str = "shepr-fixture";

/// The extension of the script file beside a stand-in.
const STAND_IN_SCRIPT_EXTENSION: &str = "fixture";

/// The exit code for a script the fixture could not parse or run: distinct
/// from the codes scripts choose, so a broken fixture is not read as a
/// scripted outcome.
pub const FIXTURE_FAILURE_EXIT: i32 = 125;

/// One thing the fixture does. See the module docs for the token forms.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Sleep this long. Fractional seconds are fine.
    Sleep(Duration),
    /// Write the text to stdout, with no newline added.
    Print(String),
    /// Write the text to stderr, with no newline added.
    PrintErr(String),
    /// Write each operand to stdout followed by a newline.
    PrintArgs,
    /// Write operand `n`, counted from 1, to stdout with no newline; nothing
    /// when there is no such operand.
    PrintArg(usize),
    /// Write this process's pid to stdout, with no newline.
    PrintPid,
    /// Write this process's session id to stdout, with no newline.
    PrintSid,
    /// Write the variable's value and a newline to stdout; an unset variable
    /// writes the newline alone.
    PrintEnv(String),
    /// Write `count` copies of `byte` to stdout.
    Fill { byte: u8, count: usize },
    /// Copy stdin to stdout until end of input or a read error.
    Cat,
    /// Read stdin until end of input or a read error, discarding it.
    Drain,
    /// Read stdin up to and including one newline, or to end of input,
    /// taking nothing beyond it.
    ReadLine,
    /// Point stdout at this file, created or truncated, for every later step
    /// and every process spawned or executed afterwards.
    To(PathBuf),
    /// Close stdout, so a reader sees end of output while this process lives.
    CloseStdout,
    /// Change the working directory.
    Cd(PathBuf),
    /// Ignore the signal. An ignored disposition survives `exec` and is
    /// inherited by children, so it reaches later spawns and executions too.
    Ignore(Signal),
    /// Send the signal to this process.
    Raise(Signal),
    /// Set both file-size limits to this many bytes.
    LimitFileSize(u64),
    /// Start a child that sleeps for the given time, named `argv0`, holding
    /// the standard streams `held` names; the others are `/dev/null`. It is
    /// neither waited for nor killed, unless a later [`Step::Wait`] waits.
    Spawn {
        argv0: String,
        sleep: Duration,
        held: Held,
    },
    /// Wait for every child [`Step::Spawn`] started.
    Wait,
    /// Exit now with this code.
    Exit(i32),
    /// Replace this process: the first element is the program, the rest its
    /// arguments.
    Exec(Vec<OsString>),
    /// Run the steps only when the operands are exactly these.
    When {
        operands: Vec<String>,
        steps: Vec<Step>,
    },
}

/// A signal a script can ignore or raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Hup,
    Term,
    Kill,
    Xfsz,
}

impl Signal {
    const ALL: [Self; 4] = [Self::Hup, Self::Term, Self::Kill, Self::Xfsz];

    fn token(self) -> &'static str {
        match self {
            Self::Hup => "hup",
            Self::Term => "term",
            Self::Kill => "kill",
            Self::Xfsz => "xfsz",
        }
    }

    fn number(self) -> libc::c_int {
        match self {
            Self::Hup => libc::SIGHUP,
            Self::Term => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
            Self::Xfsz => libc::SIGXFSZ,
        }
    }
}

/// Which of its parent's standard streams a [`Step::Spawn`] child holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    Nothing,
    Stdout,
    Stderr,
    All,
}

impl Held {
    const ALL: [Self; 4] = [Self::Nothing, Self::Stdout, Self::Stderr, Self::All];

    fn token(self) -> &'static str {
        match self {
            Self::Nothing => "none",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::All => "all",
        }
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// The argv tokens a script is spelled as, without the program name.
#[must_use]
pub fn args(steps: &[Step]) -> Vec<OsString> {
    let mut tokens = Vec::new();
    encode(steps, &mut tokens);
    tokens
}

fn seconds(duration: Duration) -> OsString {
    duration.as_secs_f64().to_string().into()
}

fn encode(steps: &[Step], out: &mut Vec<OsString>) {
    for step in steps {
        match step {
            Step::Sleep(duration) => out.extend(["sleep".into(), seconds(*duration)]),
            Step::Print(text) => out.extend(["print".into(), text.into()]),
            Step::PrintErr(text) => out.extend(["print-err".into(), text.into()]),
            Step::PrintArgs => out.push("print-args".into()),
            Step::PrintArg(index) => out.extend(["print-arg".into(), index.to_string().into()]),
            Step::PrintPid => out.push("print-pid".into()),
            Step::PrintSid => out.push("print-sid".into()),
            Step::PrintEnv(name) => out.extend(["print-env".into(), name.into()]),
            Step::Fill { byte, count } => out.extend([
                "fill".into(),
                OsString::from_vec(vec![*byte]),
                count.to_string().into(),
            ]),
            Step::Cat => out.push("cat".into()),
            Step::Drain => out.push("drain".into()),
            Step::ReadLine => out.push("read-line".into()),
            Step::To(path) => out.extend(["to".into(), path.into()]),
            Step::CloseStdout => out.push("close-stdout".into()),
            Step::Cd(path) => out.extend(["cd".into(), path.into()]),
            Step::Ignore(signal) => out.extend(["ignore".into(), signal.token().into()]),
            Step::Raise(signal) => out.extend(["raise".into(), signal.token().into()]),
            Step::LimitFileSize(bytes) => {
                out.extend(["limit-file-size".into(), bytes.to_string().into()]);
            }
            Step::Spawn { argv0, sleep, held } => out.extend([
                "spawn".into(),
                argv0.into(),
                seconds(*sleep),
                held.token().into(),
            ]),
            Step::Wait => out.push("wait".into()),
            Step::Exit(code) => out.extend(["exit".into(), code.to_string().into()]),
            Step::Exec(argv) => {
                out.extend(["exec".into(), argv.len().to_string().into()]);
                out.extend(argv.iter().cloned());
            }
            Step::When { operands, steps } => {
                out.extend(["when".into(), operands.len().to_string().into()]);
                out.extend(operands.iter().map(OsString::from));
                encode(steps, out);
                out.push("end".into());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct Tokens<'a> {
    tokens: &'a [OsString],
    next: usize,
}

impl Tokens<'_> {
    fn take(&mut self, what: &str) -> Result<&OsStr, String> {
        let token = self
            .tokens
            .get(self.next)
            .ok_or_else(|| format!("the script ends where {what} was expected"))?;
        self.next += 1;
        Ok(token)
    }

    fn text(&mut self, what: &str) -> Result<String, String> {
        let token = self.take(what)?;
        token
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("{what} {token:?} is not UTF-8"))
    }

    fn number<T: std::str::FromStr>(&mut self, what: &str) -> Result<T, String> {
        let text = self.text(what)?;
        text.parse()
            .map_err(|_| format!("{what} {text:?} is not a number"))
    }

    fn duration(&mut self, what: &str) -> Result<Duration, String> {
        let seconds: f64 = self.number(what)?;
        Duration::try_from_secs_f64(seconds)
            .map_err(|_| format!("{what} {seconds} is not a duration in seconds"))
    }

    fn signal(&mut self) -> Result<Signal, String> {
        let text = self.text("a signal")?;
        Signal::ALL
            .into_iter()
            .find(|signal| signal.token() == text)
            .ok_or_else(|| format!("unknown signal {text:?}"))
    }

    fn held(&mut self) -> Result<Held, String> {
        let text = self.text("the streams a child holds")?;
        Held::ALL
            .into_iter()
            .find(|held| held.token() == text)
            .ok_or_else(|| format!("unknown held streams {text:?}"))
    }
}

/// Parses a whole script: `tokens` must be steps and nothing else.
fn parse(tokens: &[OsString]) -> Result<Vec<Step>, String> {
    let mut cursor = Tokens { tokens, next: 0 };
    let steps = parse_steps(&mut cursor, false)?;
    Ok(steps)
}

/// Parses steps until the end of the tokens or, inside a `when`, its `end`.
fn parse_steps(cursor: &mut Tokens<'_>, in_when: bool) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    loop {
        let Some(token) = cursor.tokens.get(cursor.next) else {
            if in_when {
                return Err("a `when` has no `end`".into());
            }
            return Ok(steps);
        };
        cursor.next += 1;
        let keyword = token
            .to_str()
            .ok_or_else(|| format!("step {token:?} is not UTF-8"))?;
        let step = match keyword {
            "end" if in_when => return Ok(steps),
            "sleep" => Step::Sleep(cursor.duration("a sleep")?),
            "print" => Step::Print(cursor.text("the text to print")?),
            "print-err" => Step::PrintErr(cursor.text("the text to print")?),
            "print-args" => Step::PrintArgs,
            "print-arg" => Step::PrintArg(cursor.number("an operand number")?),
            "print-pid" => Step::PrintPid,
            "print-sid" => Step::PrintSid,
            "print-env" => Step::PrintEnv(cursor.text("a variable name")?),
            "fill" => {
                let byte = match cursor.take("the byte to fill with")?.as_bytes() {
                    [byte] => *byte,
                    other => return Err(format!("fill takes one byte, not {other:?}")),
                };
                Step::Fill {
                    byte,
                    count: cursor.number("a byte count")?,
                }
            }
            "cat" => Step::Cat,
            "drain" => Step::Drain,
            "read-line" => Step::ReadLine,
            "to" => Step::To(cursor.take("a path")?.into()),
            "close-stdout" => Step::CloseStdout,
            "cd" => Step::Cd(cursor.take("a directory")?.into()),
            "ignore" => Step::Ignore(cursor.signal()?),
            "raise" => Step::Raise(cursor.signal()?),
            "limit-file-size" => Step::LimitFileSize(cursor.number("a file-size limit")?),
            "spawn" => Step::Spawn {
                argv0: cursor.text("a child's argv[0]")?,
                sleep: cursor.duration("a child's sleep")?,
                held: cursor.held()?,
            },
            "wait" => Step::Wait,
            "exit" => Step::Exit(cursor.number("an exit code")?),
            "exec" => {
                let count: usize = cursor.number("an argv length")?;
                if count == 0 {
                    return Err("exec needs a program".into());
                }
                let argv = (0..count)
                    .map(|_| cursor.take("an exec argument").map(OsStr::to_owned))
                    .collect::<Result<_, _>>()?;
                Step::Exec(argv)
            }
            "when" => {
                let count: usize = cursor.number("an operand count")?;
                let operands = (0..count)
                    .map(|_| cursor.text("an operand to match"))
                    .collect::<Result<_, _>>()?;
                Step::When {
                    operands,
                    steps: parse_steps(cursor, true)?,
                }
            }
            other => return Err(format!("unknown step {other:?}")),
        };
        steps.push(step);
    }
}

// ---------------------------------------------------------------------------
// The interpreter, run by the `shepr-fixture` binary
// ---------------------------------------------------------------------------

struct Run {
    /// The fixture program itself, never a stand-in link: what a spawned child
    /// runs, so the child takes its script from argv.
    fixture: PathBuf,
    operands: Vec<OsString>,
    children: Vec<Child>,
}

/// Whether a script goes on to its next step or ends the process.
enum Flow {
    Continue,
    /// A [`Step::Exit`] ran: stop here with this code.
    Exit(i32),
}

/// The exit status the kernel reports for `exit(code)`: its low byte.
fn exit_code(code: i32) -> std::process::ExitCode {
    std::process::ExitCode::from(code.to_le_bytes()[0])
}

/// The `shepr-fixture` binary's whole body: work out how this process was
/// invoked, then run its script.
pub fn main() -> std::process::ExitCode {
    match run_invocation() {
        Ok(code) => exit_code(code),
        Err(error) => {
            // With stderr unwritable there is nowhere left to report to; the
            // failure exit code below still tells the test what happened.
            writeln!(io::stderr(), "{FIXTURE_NAME}: {error}").ok();
            exit_code(FIXTURE_FAILURE_EXIT)
        }
    }
}

/// Runs the script and returns the code the process exits with.
fn run_invocation() -> Result<i32, String> {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let own = std::env::current_exe()
        .map_err(|error| format!("finding this program's own path: {error}"))?;
    let after_name = argv.get(1..).unwrap_or_default();
    let (fixture, script, operands) = match read_stand_in_script(&own)? {
        Some(mut tokens) => {
            if tokens.is_empty() {
                return Err(format!("{} is empty", script_path(&own).display()));
            }
            let fixture = PathBuf::from(tokens.remove(0));
            (fixture, tokens, after_name.to_vec())
        }
        None => {
            let split = after_name
                .iter()
                .position(|token| token == "--")
                .unwrap_or(after_name.len());
            let operands = after_name.get(split + 1..).unwrap_or_default().to_vec();
            (own, after_name[..split].to_vec(), operands)
        }
    };
    let steps = parse(&script)?;
    let mut run = Run {
        fixture,
        operands,
        children: Vec::new(),
    };
    let code = match run.steps(&steps)? {
        Flow::Continue => 0,
        Flow::Exit(code) => code,
    };
    flush_stdout()?;
    Ok(code)
}

fn script_path(program: &Path) -> PathBuf {
    let mut path = program.as_os_str().to_owned();
    path.push(".");
    path.push(STAND_IN_SCRIPT_EXTENSION);
    PathBuf::from(path)
}

/// The tokens of the script beside `program`, or `None` when it is not a
/// stand-in. The first token is the fixture program's own path.
fn read_stand_in_script(program: &Path) -> Result<Option<Vec<OsString>>, String> {
    let path = script_path(program);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(
            bytes
                .split(|byte| *byte == 0)
                .map(|token| OsString::from_vec(token.to_vec()))
                .collect::<Vec<_>>()
                .split_last()
                .map_or_default(|(_, tokens)| tokens.to_vec()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("reading {}: {error}", path.display())),
    }
}

fn flush_stdout() -> Result<(), String> {
    io::stdout()
        .flush()
        .map_err(|error| format!("flushing stdout: {error}"))
}

fn write_stdout(bytes: &[u8]) -> Result<(), String> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(bytes)
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("writing stdout: {error}"))
}

/// Reads stdin one byte at a time, so nothing past what a step consumes is
/// taken from a stream a later `exec` hands on.
fn read_stdin_byte() -> Option<u8> {
    let mut byte = 0u8;
    loop {
        // SAFETY: reads at most one byte into a live local.
        let read = unsafe { libc::read(0, (&raw mut byte).cast(), 1) };
        match read {
            1 => return Some(byte),
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            // End of input, or a read error such as a hung-up terminal's EIO.
            _ => return None,
        }
    }
}

impl Run {
    fn steps(&mut self, steps: &[Step]) -> Result<Flow, String> {
        for step in steps {
            if let Flow::Exit(code) = self.step(step)? {
                return Ok(Flow::Exit(code));
            }
        }
        Ok(Flow::Continue)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "print-env reports a pane child's raw environment, which is what the fixture is asked to show"
    )]
    fn print_env(name: &str) -> Result<(), String> {
        let mut bytes = std::env::var_os(name).map_or_default(OsString::into_vec);
        bytes.push(b'\n');
        write_stdout(&bytes)
    }

    /// A spawned child runs where the script is running: it is part of this
    /// fixture's own process tree, and a `cd` step before the spawn sets it.
    #[expect(
        clippy::disallowed_methods,
        reason = "a Spawn child inherits the fixture's working directory, which the script controls"
    )]
    fn spawn_command(&self) -> std::process::Command {
        std::process::Command::new(&self.fixture)
    }

    fn step(&mut self, step: &Step) -> Result<Flow, String> {
        match step {
            Step::Sleep(duration) => std::thread::sleep(*duration),
            Step::Print(text) => write_stdout(text.as_bytes())?,
            Step::PrintErr(text) => io::stderr()
                .write_all(text.as_bytes())
                .map_err(|error| format!("writing stderr: {error}"))?,
            Step::PrintArgs => {
                let mut bytes = Vec::new();
                for operand in &self.operands {
                    bytes.extend_from_slice(operand.as_bytes());
                    bytes.push(b'\n');
                }
                write_stdout(&bytes)?;
            }
            Step::PrintArg(index) => {
                if let Some(operand) = index
                    .checked_sub(1)
                    .and_then(|index| self.operands.get(index))
                {
                    write_stdout(operand.as_bytes())?;
                }
            }
            Step::PrintPid => write_stdout(std::process::id().to_string().as_bytes())?,
            Step::PrintSid => {
                // SAFETY: getsid(0) takes no pointer and reads this process's session.
                let session = unsafe { libc::getsid(0) };
                write_stdout(session.to_string().as_bytes())?;
            }
            Step::PrintEnv(name) => Self::print_env(name)?,
            Step::Fill { byte, count } => {
                let mut stdout = io::BufWriter::new(io::stdout().lock());
                let chunk = [*byte; 4096];
                let mut left = *count;
                while left > 0 {
                    let take = left.min(chunk.len());
                    stdout
                        .write_all(&chunk[..take])
                        .map_err(|error| format!("writing stdout: {error}"))?;
                    left -= take;
                }
                stdout
                    .flush()
                    .map_err(|error| format!("writing stdout: {error}"))?;
            }
            Step::Cat => {
                let mut buffer = [0u8; 4096];
                let mut stdin = io::stdin().lock();
                loop {
                    match stdin.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => write_stdout(&buffer[..read])?,
                    }
                }
            }
            Step::Drain => while read_stdin_byte().is_some() {},
            Step::ReadLine => {
                while let Some(byte) = read_stdin_byte() {
                    if byte == b'\n' {
                        break;
                    }
                }
            }
            Step::To(path) => {
                flush_stdout()?;
                let file = std::fs::File::create(path)
                    .map_err(|error| format!("creating {}: {error}", path.display()))?;
                redirect_stdout(&file)?;
            }
            Step::CloseStdout => {
                flush_stdout()?;
                // SAFETY: closes this process's own stdout; later writes to it
                // fail rather than touch another descriptor, since nothing is
                // opened in between by a step that follows.
                unsafe { libc::close(1) };
            }
            Step::Cd(path) => std::env::set_current_dir(path)
                .map_err(|error| format!("changing directory to {}: {error}", path.display()))?,
            Step::Ignore(signal) => {
                // SAFETY: installs SIG_IGN; no handler code runs.
                unsafe { libc::signal(signal.number(), libc::SIG_IGN) };
            }
            Step::Raise(signal) => {
                flush_stdout()?;
                // SAFETY: raise(3) signals this process and touches no memory.
                unsafe { libc::raise(signal.number()) };
            }
            Step::LimitFileSize(bytes) => {
                let limit = libc::rlimit {
                    rlim_cur: *bytes,
                    rlim_max: *bytes,
                };
                // SAFETY: setrlimit reads one live rlimit value.
                if unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &raw const limit) } != 0 {
                    return Err(format!(
                        "setting the file-size limit: {}",
                        io::Error::last_os_error()
                    ));
                }
            }
            Step::Spawn { argv0, sleep, held } => {
                flush_stdout()?;
                let (stdout, stderr) = match held {
                    Held::Nothing => (Stdio::null(), Stdio::null()),
                    Held::Stdout => (Stdio::inherit(), Stdio::null()),
                    Held::Stderr => (Stdio::null(), Stdio::inherit()),
                    Held::All => (Stdio::inherit(), Stdio::inherit()),
                };
                let stdin = if matches!(held, Held::All) {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                };
                let child = self
                    .spawn_command()
                    .arg0(argv0)
                    .args(args(&[Step::Sleep(*sleep)]))
                    .stdin(stdin)
                    .stdout(stdout)
                    .stderr(stderr)
                    .spawn()
                    .map_err(|error| format!("spawning a child: {error}"))?;
                self.children.push(child);
            }
            Step::Wait => {
                for mut child in self.children.drain(..) {
                    child
                        .wait()
                        .map_err(|error| format!("waiting for a child: {error}"))?;
                }
            }
            Step::Exit(code) => return Ok(Flow::Exit(*code)),
            Step::Exec(argv) => {
                flush_stdout()?;
                let Some((program, rest)) = argv.split_first() else {
                    return Err("exec needs a program".into());
                };
                let error = exec(program, rest);
                return Err(format!(
                    "executing {}: {error}",
                    Path::new(program).display()
                ));
            }
            Step::When { operands, steps } => {
                if self
                    .operands
                    .iter()
                    .map(|operand| operand.to_str())
                    .eq(operands.iter().map(|operand| Some(operand.as_str())))
                {
                    return self.steps(steps);
                }
            }
        }
        Ok(Flow::Continue)
    }
}

/// Replaces this process with `program`, returning only the error when that
/// fails.
#[expect(
    clippy::disallowed_methods,
    reason = "exec replaces this process in place, so the program runs in the fixture's working \
              directory, which the script controls with `cd`"
)]
fn exec(program: &OsStr, rest: &[OsString]) -> io::Error {
    std::process::Command::new(program).args(rest).exec()
}

fn redirect_stdout(file: &std::fs::File) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    // SAFETY: dup2 onto this process's own stdout; both descriptors are live.
    if unsafe { libc::dup2(file.as_raw_fd(), 1) } < 0 {
        return Err(format!(
            "pointing stdout at a file: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The test side
// ---------------------------------------------------------------------------

/// The fixture program, found in the profile directory the running test
/// executable was built under.
///
/// Cargo writes binaries to the profile directory and test executables below
/// it (`deps`, or a per-package build directory, depending on the layout), so
/// the fixture is the nearest `shepr-fixture` among the executable's
/// ancestors; a binary running from the profile directory itself finds it
/// alongside.
///
/// # Panics
///
/// When the program is not there, naming how to build it.
#[must_use]
pub fn path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let exe = std::env::current_exe()
            .unwrap_or_else(|error| panic!("finding the test executable's own path: {error}"));
        exe.ancestors()
            .skip(1)
            .map(|dir| dir.join(FIXTURE_NAME))
            .find(|candidate| match std::fs::metadata(candidate) {
                Ok(metadata) => metadata.is_file(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => panic!("inspecting {}: {error}", candidate.display()),
            })
            .unwrap_or_else(|| {
                panic!(
                    "the test fixture program `{FIXTURE_NAME}` is missing from every directory \
                     above {}. It is a binary of the shepr-test-support package, which cargo \
                     builds whenever that package is among the tested packages; `brokkr check` \
                     and `brokkr test` both build it, since brokkr.toml names the package in \
                     `build_packages`.",
                    exe.display()
                )
            })
    })
}

/// [`path`] as a `&'static str`, for fields that take one.
///
/// # Panics
///
/// As [`path`], or when the path is not UTF-8.
#[must_use]
pub fn path_str() -> &'static str {
    path()
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path {} is not UTF-8", path().display()))
}

/// A command running the fixture with this script, in a fresh scratch
/// directory ([`crate::command_in_scratch`]).
///
/// # Panics
///
/// As [`path`] and [`crate::ScratchDir::new`].
#[must_use]
pub fn command(steps: &[Step]) -> std::process::Command {
    let mut command = crate::command_in_scratch(path(), "fixture-command");
    command.args(args(steps));
    command
}

/// The fixture program and this script as one argv of strings, for code that
/// takes a whole command line.
///
/// # Panics
///
/// When a token is not UTF-8.
#[must_use]
pub fn argv(steps: &[Step]) -> Vec<String> {
    std::iter::once(path().as_os_str().to_owned())
        .chain(args(steps))
        .map(|token| {
            token
                .into_string()
                .unwrap_or_else(|token| panic!("fixture token {token:?} is not UTF-8"))
        })
        .collect()
}

/// The fixture program and this script as one POSIX shell command line, each
/// token single-quoted, for production code that runs a command string under
/// the shell.
#[must_use]
pub fn shell_line(steps: &[Step]) -> String {
    argv(steps)
        .iter()
        .map(|token| shepr_core::shell_quote::quote_always(token))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Installs the fixture as `dir/name`, running `steps` whatever argv it is
/// started with; that argv is the script's operands. Returns the stand-in's
/// path.
///
/// # Panics
///
/// When the link or its script cannot be written.
#[must_use]
pub fn stand_in(dir: &Path, name: &str, steps: &[Step]) -> PathBuf {
    let program = dir.join(name);
    // A hard link rather than a copy: nothing is written to the executable,
    // so no descriptor open for writing can leak into a child another test
    // thread forks and make the exec fail with ETXTBSY. A copy is the fallback
    // only when the scratch tree is on another filesystem.
    if let Err(link_error) = std::fs::hard_link(path(), &program) {
        std::fs::copy(path(), &program).unwrap_or_else(|copy_error| {
            panic!(
                "installing the fixture as {}: linking failed ({link_error}), copying failed \
                 ({copy_error})",
                program.display()
            )
        });
    }
    let mut bytes = Vec::new();
    for token in std::iter::once(path().as_os_str().to_owned()).chain(args(steps)) {
        bytes.extend_from_slice(token.as_bytes());
        bytes.push(0);
    }
    let script = script_path(&program);
    std::fs::write(&script, bytes)
        .unwrap_or_else(|error| panic!("writing {}: {error}", script.display()));
    program
}

/// `path` as a pane shell, for tests that build a PTY command or launch a pane
/// without going through config validation, which is what mints one in
/// production. No shell-name or executable check runs.
///
/// # Panics
///
/// When `path` is not absolute.
#[must_use]
pub fn resolved_shell(path: impl AsRef<Path>) -> shepr_core::shell::ResolvedShell {
    let path = path.as_ref();
    shepr_core::shell::ResolvedShell::validate(path.to_path_buf(), |_| Ok(()))
        .unwrap_or_else(|error| panic!("test shell {}: {error}", path.display()))
}

/// A stand-in pane shell named `sh` that drains its terminal without
/// interpreting commands and prints nothing, for tests that need a pane to
/// stay live without depending on the host's shell. One per test process.
///
/// # Panics
///
/// As [`stand_in`].
#[must_use]
pub fn idle_shell() -> &'static str {
    static SHELL: OnceLock<String> = OnceLock::new();
    SHELL.get_or_init(|| {
        let dir = crate::ScratchDir::new("fixture-idle-shell");
        stand_in(&dir, "sh", &[Step::Drain])
            .into_os_string()
            .into_string()
            .unwrap_or_else(|path| panic!("the idle shell path {path:?} is not UTF-8"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_step() -> Vec<Step> {
        vec![
            Step::Sleep(Duration::from_millis(1500)),
            Step::Print("two words\nand a line".into()),
            Step::PrintErr(String::new()),
            Step::PrintArgs,
            Step::PrintArg(2),
            Step::PrintPid,
            Step::PrintSid,
            Step::PrintEnv("TERM".into()),
            Step::Fill {
                byte: b'x',
                count: 5000,
            },
            Step::Cat,
            Step::Drain,
            Step::ReadLine,
            Step::To("/nonexistent/out".into()),
            Step::CloseStdout,
            Step::Cd("/nonexistent/dir".into()),
            Step::Ignore(Signal::Hup),
            Step::Raise(Signal::Kill),
            Step::LimitFileSize(0),
            Step::Spawn {
                argv0: "codex".into(),
                sleep: Duration::from_secs(30),
                held: Held::Stderr,
            },
            Step::Wait,
            Step::Exit(7),
            Step::Exec(vec!["/nonexistent/program".into(), "end".into()]),
            Step::When {
                operands: vec!["status".into(), "--json".into()],
                steps: vec![
                    Step::Print("{}".into()),
                    Step::When {
                        operands: Vec::new(),
                        steps: vec![Step::Exit(0)],
                    },
                ],
            },
            Step::Exit(64),
        ]
    }

    /// Every step survives its encoding, including an `exec` argument and a
    /// nested `when` that spell a keyword.
    #[test]
    fn scripts_round_trip_through_their_tokens() {
        let steps = every_step();
        assert_eq!(parse(&args(&steps)), Ok(steps));
    }

    #[test]
    fn malformed_scripts_are_refused_with_a_reason() {
        let refused = |tokens: &[&str]| {
            let tokens: Vec<OsString> = tokens.iter().map(OsString::from).collect();
            parse(&tokens).expect_err("a malformed script is refused")
        };
        assert!(refused(&["sleep"]).contains("ends where"));
        assert!(refused(&["sleep", "-1"]).contains("not a duration"));
        assert!(refused(&["fill", "xy", "3"]).contains("one byte"));
        assert!(refused(&["when", "0", "print", "x"]).contains("no `end`"));
        assert!(refused(&["end"]).contains("unknown step"));
        assert!(refused(&["exec", "0"]).contains("needs a program"));
        assert!(refused(&["ignore", "usr1"]).contains("unknown signal"));
    }

    #[test]
    fn shell_lines_quote_every_token() {
        let line = shell_line(&[Step::Print("it's | here".into())]);
        assert!(line.ends_with(r#"'print' 'it'\''s | here'"#), "{line}");
    }
}
