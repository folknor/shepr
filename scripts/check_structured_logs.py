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
"""
from __future__ import annotations

import re
import sys

from _brokkr_config import ROOT, repository_sources


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
        ('shepr_platform::structured_log!(WARN, event = a.b, outcome = "error", "ok");', []),
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
    if failures:
        print('\n'.join(failures), file=sys.stderr)
        return 1
    print('structured logs ok')
    return 0


if __name__ == '__main__':
    sys.exit(main())
