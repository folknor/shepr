#!/usr/bin/env python3
"""Route every info, warn and error event through the shared log macro.

`shepr_platform::structured_log!` supplies the `event`, `subsystem` and
`outcome` fields and constrains event names to `subsystem.operation`, so a
direct tracing call at those levels is refused outright: an `event =` field
written by hand would still miss `subsystem` and `outcome`. That covers the
level macros however they are reached (`tracing::warn!`, an imported `warn!`,
`log::warn!`), spread over several lines or nested in another macro, and
`event!` with an INFO, WARN or ERROR level. Comments and literals are blanked
first, so text that only looks like a call is not one. Debug and trace are
outside this operator-diagnostic contract. The macro itself passes its level
as `Level::$level`, which names no level and so is not refused.

Two more rules hold the vocabulary of every `structured_log!` call, at any
level. The event's subsystem must be one of `SUBSYSTEMS` below, each of which
means one thing; a new subsystem is added there with its meaning, never reused
for a second one. And a failure's cause is the field `error`: a field keyed
`err`, `e`, `failure`, `reason`, `rejection`, `diagnostic`, `detail`, `panic`
and the like is refused, whether written `key = value` or as a `?key` or `%key`
shorthand. The check is textual, so it knows only the key, not what the value
means: a field that holds something else under one of those names takes a
different name. Outcomes are held by the macro itself (`shepr_platform::Outcome`),
not by this script.
"""
from __future__ import annotations

import re
import sys

from _brokkr_config import ROOT, repository_sources


SUBSYSTEMS = {
    'agent': 'agent identity, detection, state and session reports for a pane',
    'api': 'the JSON API listener and its requests',
    'child': 'supervised host-utility children: admission, deadlines and reaping',
    'cli': 'the command line process',
    'client': 'the client process (TUI launch, startup, shutdown, its own files)',
    'clipboard': 'clipboard copy, paste, OSC 52 and the helper processes',
    'connection': 'one TUI connection as the server sees it, and its transport',
    'daemon': 'daemonizing and reaping the server process',
    'endpoint': 'the client side of a server endpoint, local or remote',
    'environment': 'reading process environment values',
    'git': 'Git status refresh and its worker',
    'host_terminal': 'the terminal the client runs in: modes, queries, writes, frames',
    'input': 'parsing raw host terminal input',
    'integration': 'agent integration installation and reports',
    'ipc': 'sockets, listeners and their locks',
    'launch': 'launching, probing and stopping a server from outside it',
    'logging': 'the log sink itself',
    'pane': 'a pane, its process launch, exit and teardown',
    'persist': 'session save, restore, checkpoints and their files',
    'process': 'process handle plumbing',
    'pty': 'the PTY layer: reads, writes, launch handshake',
    'publish_file': 'atomic file publication',
    'remote': 'SSH machines and the remote host side of the bridge',
    'runtime': 'the owned runtime directory',
    'server': 'the server process: startup, shutdown, event loop, pane teardown',
    'shutdown': 'the logind host shutdown warning and freeze',
    'surface': 'pane surface projection, encoding, patches and receipt',
    'terminal': 'a pane terminal core: its state, cwd and theme',
    'workspace': 'workspace create, focus, close, rename, move and startup',
}
# A failure's cause is the field `error`; these keys say the same thing otherwise.
FAILURE_KEYS = {
    'err', 'e', 'fail', 'failure', 'reason', 'rejection', 'diagnostic', 'detail',
    'details', 'panic', 'exception', 'fault', 'problem', 'why', 'error_message',
    'error_msg', 'errmsg',
}

STRUCTURED_CALL = re.compile(r'\bstructured_log\s*!\s*[({\[]')
EVENT_ARGUMENT = re.compile(r'event\s*=\s*(\w+)\s*\.\s*(\w+)$')
FIELD_KEY = re.compile(r'([A-Za-z_][\w.]*)\s*=(?!=)')
FIELD_SHORTHAND = re.compile(r'[?%]?\s*([A-Za-z_][\w.]*)$')
LEVEL_CALL = re.compile(r"\b(?:info|warn|error)\s*!\s*[({\[]")
EVENT_CALL = re.compile(r"\bevent\s*!\s*[({\[]")
OPERATOR_LEVEL = re.compile(r"\bLevel\s*::\s*(?:INFO|WARN|ERROR)\b")
RAW_STRING = re.compile(r'(?:br|cr|r)(#*)"')
CHAR = re.compile(r"'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'")


def code_mask(source: str) -> str:
    """Blank comments and literals, retaining offsets and line breaks."""
    result = list(source)
    index = 0
    while index < len(source):
        start = index
        if source.startswith('//', index):
            end = source.find('\n', index)
            index = len(source) if end < 0 else end
        elif source.startswith('/*', index):
            index += 2
            depth = 1
            while index < len(source) and depth:
                if source.startswith('/*', index):
                    depth += 1
                    index += 2
                elif source.startswith('*/', index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
        else:
            raw = RAW_STRING.match(source, index)
            if raw:
                closing = '"' + raw[1]
                end = source.find(closing, raw.end())
                index = len(source) if end < 0 else end + len(closing)
            elif source[index] == '"':
                index += 1
                while index < len(source):
                    if source[index] == '\\':
                        index += 2
                    elif source[index] == '"':
                        index += 1
                        break
                    else:
                        index += 1
            elif source[index] == "'" and (char := CHAR.match(source, index)):
                index = char.end()
            else:
                index += 1
                continue
        for offset in range(start, min(index, len(source))):
            if result[offset] != '\n':
                result[offset] = ' '
    return ''.join(result)


def first_argument(masked: str, start: int) -> str:
    """The text of a macro invocation's first top-level argument."""
    depth = 0
    argument = []
    for char in masked[start:]:
        if char in ')}]':
            if depth == 0:
                break
            depth -= 1
        elif char in '({[':
            depth += 1
        elif char == ',' and depth == 0:
            break
        argument.append(char)
    return ''.join(argument)


def top_level_arguments(masked: str, start: int) -> list[str]:
    """The top-level comma-separated arguments of a macro invocation."""
    depth = 0
    arguments = []
    current = []
    for char in masked[start:]:
        if char in ')}]' and depth == 0:
            break
        if char in '({[':
            depth += 1
        elif char in ')}]':
            depth -= 1
        if char == ',' and depth == 0:
            arguments.append(''.join(current).strip())
            current = []
        else:
            current.append(char)
    arguments.append(''.join(current).strip())
    return arguments


def vocabulary_problems(source: str) -> list[tuple[int, str]]:
    """Unlisted subsystems and failure fields not keyed `error`, by line."""
    masked = code_mask(source)
    problems = []
    for call in STRUCTURED_CALL.finditer(masked):
        line = source.count('\n', 0, call.start()) + 1
        arguments = top_level_arguments(masked, call.end())
        if len(arguments) < 3:
            continue
        event = EVENT_ARGUMENT.match(arguments[1])
        if event and event[1] not in SUBSYSTEMS:
            problems.append((
                line,
                f'subsystem `{event[1]}` is not in SUBSYSTEMS in scripts/check_structured_logs.py; '
                'pick the one that means this, or add it there with its meaning',
            ))
        for argument in arguments[3:]:
            keyed = FIELD_KEY.match(argument)
            if keyed:
                key = keyed[1]
            else:
                shorthand = FIELD_SHORTHAND.match(argument)
                if not shorthand:
                    continue
                key = shorthand[1]
            if key.split('.')[0] in FAILURE_KEYS:
                problems.append((line, f'a failure is the field `error`, not `{key}`'))
    return problems


def direct_calls(source: str) -> list[int]:
    masked = code_mask(source)
    starts = [call.start() for call in LEVEL_CALL.finditer(masked)]
    starts += [
        call.start()
        for call in EVENT_CALL.finditer(masked)
        if OPERATOR_LEVEL.search(first_argument(masked, call.end()))
    ]
    return sorted(source.count('\n', 0, start) + 1 for start in starts)


def self_check() -> None:
    vocabulary_cases = [
        ('structured_log!(WARN, event = pane.exit, outcome = Error, error = %e, "x");', []),
        ('structured_log!(WARN, event = pane.exit, outcome = Error, %error, "x");', []),
        ('structured_log!(WARN, event = pane.exit, outcome = Error, error_kind = k, "x");', []),
        ('structured_log!(WARN, event = pane.exit, outcome = Error, %failure, "x");', [1]),
        ('structured_log!(WARN, event = pane.exit, outcome = Error, ?reason, "x");', [1]),
        ('structured_log!(WARN, event = pane.exit, outcome = Error, reason = ?r, "x");', [1]),
        ('structured_log!(WARN, event = pane.exit, outcome = Error, err, "x");', [1]),
        ('structured_log!(\n    WARN, event = pane.exit, outcome = Error,\n    pane = %id,\n    %e,\n    "x"\n);', [1]),
        ('structured_log!(WARN, event = pane.exit, outcome = Error, failure.stage, "x");', [1]),
        ('structured_log!(WARN, event = pane.exit, outcome = Error, stage = failure.stage, "x");', []),
        ('structured_log!(WARN, event = pane.exit, outcome = Error, setup_failure_status, "x");', []),
        ('structured_log!(WARN, event = nowhere.exit, outcome = Error, "x");', [1]),
        ('structured_log!(WARN, event = pane.exit, outcome = Error, "failure, reason = 1");', []),
        ('structured_log!($level, event = $subsystem.$operation, outcome = (x), $($fields)+)', []),
    ]
    for source, expected in vocabulary_cases:
        actual = [line for line, _ in vocabulary_problems(source)]
        if actual != expected:
            raise RuntimeError(f'log check self-test failed: {source!r}: {actual} != {expected}')

    cases = [
        ('tracing::warn!(event = "ipc.accept", "failed");', [1]),
        ('info! { "ready" }', [1]),
        ('log::error!("x");', [1]),
        ('let text = "tracing::error!(x)";', []),
        ('warn!(field = { tracing::info!("nested"); });', [1, 1]),
        ('// tracing::warn!("comment");\nerror!("real");', [2]),
        ('/* /* nested */ warn!("comment"); */ ok();', []),
        ('let text = r##"tracing::error!("text");"##;\nwarn!("real");', [2]),
        ('let text = "escaped \\" warn!(text)"; debug!("ok");', []),
        ("let c = ')'; tracing::debug!(\"fine\");", []),
        ('compile_error!("x"); my_info!(x); tracing::debug!("a"); tracing::trace!("b");', []),
        ('tracing::info!(\n    target: "sink",\n    "ok"\n);', [1]),
        ('warn\n    !(\n    "spread"\n);', [1]),
        ('macro_rules! m { () => { tracing::warn!("in a macro") } }', [1]),
        ('tracing::event!(tracing::Level::WARN, "raw");', [1]),
        ('event!(\n    Level::ERROR,\n    "raw"\n);', [1]),
        ('tracing::event!(Level::DEBUG, "fine");', []),
        ('$crate::tracing_backend::event!($crate::tracing_backend::Level::$level, x)', []),
        ('shepr_platform::structured_log!(WARN, event = a.b, outcome = Error, "ok");', []),
    ]
    for source, expected in cases:
        actual = direct_calls(source)
        if actual != expected:
            raise RuntimeError(f'log check self-test failed: {source!r}: {actual} != {expected}')


def main() -> int:
    self_check()
    failures = []
    for path in repository_sources({'.rs'}):
        for line in direct_calls(path.read_text()):
            failures.append(
                f'{path.relative_to(ROOT)}:{line}: log info, warn and error events '
                'through shepr_platform::structured_log!'
            )
        for line, problem in vocabulary_problems(path.read_text()):
            failures.append(f'{path.relative_to(ROOT)}:{line}: {problem}')
    if failures:
        print('\n'.join(failures), file=sys.stderr)
        return 1
    print('structured logs ok')
    return 0


if __name__ == '__main__':
    sys.exit(main())
