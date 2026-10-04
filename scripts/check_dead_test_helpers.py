#!/usr/bin/env python3
"""Refuse a test helper that nothing names.

The workspace denies `unused` and clippy lints `--all-targets`, so rustc's
dead-code pass runs over every test build. It still misses a whole class of
dead test code, because it treats an item as used whenever the item is
reachable from outside its crate:

  - a `pub` helper under `#[cfg(test)]` in a library whose path to it is
    public (a `#[cfg(test)] impl` on a public type, a gated `pub fn` in a
    `pub mod`). `unreachable_pub` makes such an item `pub` exactly when it is
    reachable, so rustc counts it as exported API and never as dead, although
    the item exists only in that crate's own test build and no other crate can
    ever call it;
  - every `pub` item of a dev-only crate (`shepr-test-support`,
    `shepr-test-fixtures`): their whole API is exported, and whether another
    crate's tests still call it is a question no single compilation asks;
  - a private helper whose only callers are such exported dead helpers: the
    exported one is a liveness root, so its callees look live too.

This check answers the question by name instead. A test helper is a
definition (a `fn`, `const`, `static`, `struct`, `enum`, `union`, `type`,
`trait` or `macro_rules!`) in test-only code: an item under a test-only cfg,
everything inside it, every module file reached through a test-gated `mod`,
and every item of a dev-only crate (a member that some member takes as a
dev-dependency and none takes as a normal or build dependency). Not
helpers, because something other than a name calls them: `main`, the members
of trait impls, and any item or `impl` carrying an attribute outside a short
inert list (`cfg`, `doc`, `derive`, lint levels and the like). That last rule
covers `#[test]` and `#[tokio::test]` functions, attribute macros that drive
an impl's methods (`#[zbus::interface]`), and `#[global_allocator]`.

A helper is live when its name appears, outside its own body, in code that
can see it: the module subtree its visibility names (`pub(crate)` or a plain
`pub` the whole crate; a dev-only crate's `pub` items every crate). Comments
do not count, string literals do (a serde attribute names a function in one),
and a `use` naming the helper does not, unless it renames it with `as`. An
occurrence inside a helper already found dead does not count either, so the
pass repeats until nothing changes, which catches helpers used only by dead
helpers.

Matching by name errs toward silence: a dead helper that shares its name
with something live in its scope is not reported. That is the trade for no
false positives. Mutually recursive dead helpers are not reported either.

The module tree is walked from each crate root, following `mod` declarations
(and `#[path]`), so a file is test-only exactly when rustc compiles it only
for tests. A file several crates load (the workspace build script, also a
test module of shepr-protocol) is walked once per crate, and its helper is
dead only if it is dead in each. A `mod` declaration whose file cannot be
found fails the check rather than shrinking what it covers. The lexer
handles comments (nested), strings, raw strings and char literals; item
boundaries are found by bracket depth, which suits rustfmt-formatted code.
The test-only cfg judgement is
`check_skip_after_scopes.py`'s, so the two checks agree on what a test item
is.
"""

from __future__ import annotations

import pathlib
import re
import sys
import tomllib
from dataclasses import dataclass, field

from _brokkr_config import ROOT, workspace_member_manifests
from check_skip_after_scopes import test_only_cfg_attribute

SPECIAL = re.compile(r'(?<![\w])(?:b|c)?r#*"|//|/\*|"|\'')
RAW_OPEN = re.compile(r'(?:b|c)?r(#*)"')
CHAR_LITERAL = re.compile(r"'(?:\\u\{[0-9A-Fa-f]{1,6}\}|\\x[0-9A-Fa-f]{2}|\\.|[^\\'\n])'")
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
ALIASED = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]*)\s+as\b")

VIS = r"(?P<vis>pub(?:\s*\([^)]*\))?\s+)?"
MOD = re.compile(rf"^{VIS}mod\s+(?P<name>\w+)\s*(?P<end>[;{{])")
IMPL = re.compile(r"^(?:unsafe\s+)?impl\b")
USE = re.compile(rf"^{VIS}use\b")
TRAIT = re.compile(rf"^{VIS}(?:unsafe\s+)?(?:auto\s+)?trait\s+(?P<name>\w+)")
DEFINITIONS = [
    ("fn", re.compile(rf'^{VIS}(?:(?:const|async|unsafe|safe|default)\s+|extern\s+"[^"]*"\s+)*fn\s+(?P<name>\w+)')),
    ("const", re.compile(rf"^{VIS}const\s+(?P<name>\w+)\s*:")),
    ("static", re.compile(rf"^{VIS}static\s+(?:mut\s+)?(?P<name>\w+)\s*:")),
    ("struct", re.compile(rf"^{VIS}struct\s+(?P<name>\w+)")),
    ("enum", re.compile(rf"^{VIS}enum\s+(?P<name>\w+)")),
    ("union", re.compile(rf"^{VIS}union\s+(?P<name>\w+)")),
    ("type", re.compile(rf"^{VIS}type\s+(?P<name>\w+)")),
    ("macro", re.compile(r"^macro_rules!\s*(?P<name>\w+)")),
]
PATH_ATTRIBUTE = re.compile(r'^#\[\s*path\s*=\s*"([^"]*)"\s*\]$')
ATTRIBUTE_NAME = re.compile(r"^#\[\s*((?:\w+::)*\w+)")
# Attributes that change neither who calls an item nor whether it is called.
# Any other attribute (an attribute macro such as `#[zbus::interface]`, a
# derive helper, `#[global_allocator]`, `#[no_mangle]`) may hand the item to
# generated code or the linker, so the item is a root, and an `impl` carrying
# one is not walked.
INERT_ATTRIBUTES = {
    "allow",
    "cfg",
    "cfg_attr",
    "cold",
    "deny",
    "deprecated",
    "derive",
    "doc",
    "expect",
    "forbid",
    "inline",
    "must_use",
    "non_exhaustive",
    "path",
    "repr",
    "rustfmt::skip",
    "track_caller",
    "warn",
}


def inert(attributes: list[str]) -> bool:
    """Whether every attribute is one that hands the item to no generated code."""
    for text in attributes:
        name = ATTRIBUTE_NAME.match(text)
        if name is None or name.group(1) not in INERT_ATTRIBUTES:
            return False
    return True


NOT_HELPERS = {"main", "_"}


def mask(text: str) -> tuple[str, str]:
    """The text with comments blanked, and with string and char contents blanked too.

    Both keep every newline and every offset, so line numbers carry across.
    """
    nocomment = list(text)
    bare = list(text)

    def blank(buffer: list[str], begin: int, end: int) -> None:
        for index in range(begin, end):
            if buffer[index] != "\n":
                buffer[index] = " "

    index = 0
    length = len(text)
    while True:
        match = SPECIAL.search(text, index)
        if match is None:
            break
        start = match.start()
        token = match.group()
        if token == "//":
            end = text.find("\n", start)
            end = length if end < 0 else end
            blank(nocomment, start, end)
            blank(bare, start, end)
            index = end
        elif token == "/*":
            depth, end = 1, start + 2
            while end < length and depth:
                if text.startswith("/*", end):
                    depth, end = depth + 1, end + 2
                elif text.startswith("*/", end):
                    depth, end = depth - 1, end + 2
                else:
                    end += 1
            blank(nocomment, start, end)
            blank(bare, start, end)
            index = end
        elif token == '"':
            end = start + 1
            while end < length and text[end] != '"':
                end += 2 if text[end] == "\\" else 1
            blank(bare, start + 1, min(end, length))
            index = end + 1
        elif token == "'":
            literal = CHAR_LITERAL.match(text, start)
            if literal:
                blank(bare, start + 1, literal.end() - 1)
                index = literal.end()
            else:
                index = start + 1
        else:
            hashes = RAW_OPEN.match(token).group(1)
            closer = '"' + hashes
            end = text.find(closer, match.end())
            end = length if end < 0 else end
            blank(bare, match.end(), end)
            index = end + len(closer)
    return "".join(nocomment), "".join(bare)


@dataclass
class Item:
    attributes: list[str]
    first: int
    last: int
    opener: int | None
    head: str


def attribute_end(line: str, start: int) -> int | None:
    """Offset just past the attribute opening at `start`, if it closes on this line."""
    depth = 0
    for index in range(start, len(line)):
        if line[index] == "[":
            depth += 1
        elif line[index] == "]":
            depth -= 1
            if depth == 0:
                return index + 1
    return None


def scan_items(bare: list[str], nocomment: list[str], start: int, stop: int) -> list[Item]:
    """Items at the bracket depth of `start` in `[start, stop)`, with their attributes."""
    items: list[Item] = []
    depth = 0
    attributes: list[str] = []
    pending: list[str] | None = None
    pending_depth = 0
    item_start: int | None = None
    head_column = 0
    opener: int | None = None

    def emit(last: int) -> None:
        nonlocal item_start, attributes, opener
        assert item_start is not None
        upto = opener if opener is not None else last
        head = " ".join(
            bare[number][head_column if number == item_start else 0 :].strip()
            for number in range(item_start, upto + 1)
        )
        items.append(Item(attributes, item_start, last, opener, head))
        item_start, attributes, opener = None, [], None

    for number in range(start, stop):
        line = bare[number]
        if pending is not None:
            pending.append(nocomment[number].strip())
            pending_depth += line.count("[") - line.count("]")
            if pending_depth <= 0:
                attributes.append(" ".join(pending))
                pending = None
            continue
        column = 0
        if item_start is None and depth == 0:
            stripped = line.strip()
            if not stripped:
                continue
            column = len(line) - len(line.lstrip())
            while line.startswith("#", column):
                end = attribute_end(line, column)
                if end is None:
                    pending = [nocomment[number][column:].strip()]
                    pending_depth = line[column:].count("[") - line[column:].count("]")
                    break
                text = nocomment[number][column:end].strip()
                if not text.startswith("#!"):
                    attributes.append(text)
                column = end + len(line[end:]) - len(line[end:].lstrip())
            if pending is not None or column >= len(line):
                continue
            item_start, head_column = number, column
        for index in range(column, len(line)):
            char = line[index]
            if char in "([{":
                if char == "{" and depth == 0 and opener is None:
                    opener = number
                depth += 1
            elif char in ")]}":
                depth -= 1
                if char == "}" and depth == 0 and item_start is not None:
                    rest = line[index + 1 :].lstrip()
                    if not rest.startswith((";", ",", ".", ")", "?")):
                        emit(number)
            elif char == ";" and depth == 0 and item_start is not None:
                emit(number)
    if item_start is not None:
        emit(stop - 1)
    return items


@dataclass
class Crate:
    package: str
    root: pathlib.Path
    dev_only: bool


@dataclass
class SourceFile:
    path: pathlib.Path
    crate: Crate
    nocomment: list[str]
    bare: list[str]
    modules: list[tuple[str, ...]]
    use_lines: set[int] = field(default_factory=set)


@dataclass
class Helper:
    name: str
    kind: str
    visibility: str
    source: SourceFile
    first: int
    last: int
    scope: tuple[str, ...] | None  # module prefix inside the crate; None for every crate

    def where(self) -> str:
        return f"{self.source.path.relative_to(ROOT).as_posix()}:{self.first + 1}"


def scope_of(visibility: str, module: tuple[str, ...], dev_only: bool) -> tuple[str, ...] | None:
    """The module subtree a definition with this visibility is named in."""
    spelled = re.sub(r"\s+", "", visibility)
    if not spelled or spelled == "pub(self)":
        return module
    if spelled == "pub":
        return None if dev_only else ()
    if spelled == "pub(crate)":
        return ()
    if spelled == "pub(super)":
        return module[:-1]
    target = re.fullmatch(r"pub\(in(.*)\)", spelled)
    if target:
        parts = target.group(1).split("::")
        if parts[0] == "crate":
            return tuple(parts[1:])
        resolved = list(module)
        for part in parts:
            if part == "super":
                resolved.pop()
            elif part != "self":
                resolved.append(part)
        return tuple(resolved)
    return ()


def trait_impl(head: str) -> bool:
    """Whether an `impl` head implements a trait (`impl<..> Trait for Type`)."""
    text = head.split("{", 1)[0].replace("->", "  ")
    text = re.sub(r"^(?:unsafe\s+)?impl\s*", "", text)
    if text.startswith("<"):
        depth = 0
        for index, char in enumerate(text):
            depth += char == "<"
            depth -= char == ">"
            if depth == 0:
                text = text[index + 1 :]
                break
    text = re.split(r"\bwhere\b", text, maxsplit=1)[0]
    return re.search(r"\bfor\b(?!\s*<)", text) is not None


class Walker:
    def __init__(self) -> None:
        self.files: dict[tuple[pathlib.Path, pathlib.Path], SourceFile] = {}
        self.masked: dict[pathlib.Path, tuple[str, str]] = {}
        self.helpers: list[Helper] = []
        self.problems: list[str] = []

    def walk_file(
        self, path: pathlib.Path, crate: Crate, test: bool, module: tuple[str, ...], directory: pathlib.Path
    ) -> None:
        # A file can be a module of several crates (the workspace build script
        # is also a test module of shepr-protocol), so it is walked once per
        # crate, each walk with that crate's test context.
        key = (path, crate.root)
        if key in self.files:
            return
        if path not in self.masked:
            self.masked[path] = mask(path.read_text())
        nocomment, bare = self.masked[path]
        source = SourceFile(path, crate, nocomment.split("\n"), bare.split("\n"), [])
        source.modules = [module] * len(source.bare)
        self.files[key] = source
        self.walk_block(source, 0, len(source.bare), test, module, directory)

    def walk_block(
        self,
        source: SourceFile,
        start: int,
        stop: int,
        test: bool,
        module: tuple[str, ...],
        directory: pathlib.Path,
    ) -> None:
        for item in scan_items(source.bare, source.nocomment, start, stop):
            gated = test or any(test_only_cfg_attribute([text]) for text in item.attributes)
            head = item.head
            declared = MOD.match(head)
            if declared:
                name = declared.group("name")
                inner = module + (name,)
                if declared.group("end") == "{":
                    if item.opener is not None and item.opener < item.last:
                        for number in range(item.opener + 1, item.last):
                            source.modules[number] = inner
                        self.walk_block(source, item.opener + 1, item.last, gated, inner, directory / name)
                    continue
                self.walk_module_file(source, item, name, gated, inner, directory)
                continue
            if USE.match(head):
                source.use_lines.update(range(item.first, item.last + 1))
                continue
            if IMPL.match(head):
                walked = not trait_impl(head) and inert(item.attributes)
                if walked and item.opener is not None and item.opener < item.last:
                    self.walk_block(source, item.opener + 1, item.last, gated, module, directory)
                continue
            if not gated or not inert(item.attributes):
                continue
            kind, defined = "trait", TRAIT.match(head)
            for candidate, pattern in DEFINITIONS:
                if defined:
                    break
                kind, defined = candidate, pattern.match(head)
            if not defined or defined.group("name") in NOT_HELPERS:
                continue
            visibility = (defined.groupdict().get("vis") or "").strip()
            self.helpers.append(
                Helper(
                    defined.group("name"),
                    kind,
                    visibility or "private",
                    source,
                    item.first,
                    item.last,
                    scope_of(visibility, module, source.crate.dev_only),
                )
            )

    def walk_module_file(
        self,
        source: SourceFile,
        item: Item,
        name: str,
        test: bool,
        module: tuple[str, ...],
        directory: pathlib.Path,
    ) -> None:
        """Walk the file an out-of-line `mod name;` declaration loads."""
        explicit = next(
            (match.group(1) for text in item.attributes if (match := PATH_ATTRIBUTE.match(text))),
            None,
        )
        if explicit is not None:
            candidates = [(source.path.parent / explicit).resolve()]
        else:
            candidates = [directory / f"{name}.rs", directory / name / "mod.rs"]
        found = next((path for path in candidates if path.is_file()), None)
        if found is None:
            relative = source.path.relative_to(ROOT).as_posix()
            looked = ", ".join(path.relative_to(ROOT).as_posix() for path in candidates)
            self.problems.append(f"{relative}:{item.first + 1}: cannot find the file of `mod {name};` (looked for {looked})")
            return
        if found.name == "mod.rs" or explicit is not None:
            child_directory = found.parent
        else:
            child_directory = found.parent / found.stem
        self.walk_file(found, source.crate, test, module, child_directory)


def dependency_names(manifest: dict, sections: tuple[str, ...]) -> set[str]:
    names: set[str] = set()
    tables = [manifest] + list(manifest.get("target", {}).values())
    for table in tables:
        for section in sections:
            for key, value in table.get(section, {}).items():
                names.add(value.get("package", key) if isinstance(value, dict) else key)
    return names


def crates() -> list[Crate]:
    manifests = {path: tomllib.loads(path.read_text()) for path in workspace_member_manifests()}
    shipped: set[str] = set()
    dev: set[str] = set()
    for manifest in manifests.values():
        shipped |= dependency_names(manifest, ("dependencies", "build-dependencies"))
        dev |= dependency_names(manifest, ("dev-dependencies",))
    found: list[Crate] = []
    for path, manifest in manifests.items():
        package = manifest["package"]["name"]
        directory = path.parent
        dev_only = package in dev and package not in shipped
        roots: list[pathlib.Path] = []
        for default in ("src/lib.rs", "src/main.rs", "build.rs"):
            roots.append(directory / default)
        for folder in ("src/bin", "tests", "benches", "examples"):
            roots.extend(sorted((directory / folder).glob("*.rs")))
        explicit = [manifest.get("lib", {})] + [
            target for kind in ("bin", "test", "bench", "example") for target in manifest.get(kind, [])
        ]
        roots.extend(directory / target["path"] for target in explicit if "path" in target)
        build = manifest["package"].get("build")
        if isinstance(build, str):
            roots.append(directory / build)
        seen: set[pathlib.Path] = set()
        for root in roots:
            root = root.resolve()
            if root.is_file() and root not in seen:
                seen.add(root)
                found.append(Crate(package, root, dev_only))
    return found


def main() -> int:
    walker = Walker()
    for crate in crates():
        walker.walk_file(crate.root, crate, crate.dev_only, (), crate.root.parent)

    # Every identifier occurrence that counts as naming something: comments
    # are already blanked, and a `use` counts only for the name it renames.
    occurrences: dict[str, list[tuple[SourceFile, int]]] = {}
    for source in walker.files.values():
        for number, line in enumerate(source.nocomment):
            tokens = ALIASED.findall(line) if number in source.use_lines else IDENT.findall(line)
            for token in tokens:
                occurrences.setdefault(token, []).append((source, number))

    dead: dict[int, set[int]] = {}

    def named_elsewhere(helper: Helper) -> bool:
        for source, number in occurrences.get(helper.name, []):
            if source is helper.source and helper.first <= number <= helper.last:
                continue
            if number in dead.get(id(source), ()):
                continue
            if helper.scope is None:
                return True
            if source.crate.root != helper.source.crate.root:
                continue
            if source.modules[number][: len(helper.scope)] == helper.scope:
                return True
        return False

    found: list[Helper] = []
    remaining = list(walker.helpers)
    while True:
        newly = [helper for helper in remaining if not named_elsewhere(helper)]
        if not newly:
            break
        for helper in newly:
            dead.setdefault(id(helper.source), set()).update(range(helper.first, helper.last + 1))
        found.extend(newly)
        remaining = [helper for helper in remaining if helper not in newly]

    # A definition compiled into several crates is dead only if it is dead in
    # every one of them.
    live_somewhere = {(helper.source.path, helper.first) for helper in remaining}
    reported: dict[tuple[pathlib.Path, int], Helper] = {}
    for helper in found:
        key = (helper.source.path, helper.first)
        if key not in live_somewhere:
            reported.setdefault(key, helper)

    problems = list(walker.problems)
    for helper in sorted(reported.values(), key=lambda item: (item.where(), item.name)):
        scope = "any crate" if helper.scope is None else "::".join(("crate",) + helper.scope)
        problems.append(
            f"{helper.where()}: test helper `{helper.name}` ({helper.kind}, {helper.visibility}) "
            f"is named nowhere in {scope} outside its own body and other dead helpers"
        )
    if problems:
        print("\n".join(problems))
        print(f"{len(problems)} problem(s); delete the dead test helpers, or call them")
        return 1
    print(f"{len(walker.helpers)} test helpers in {len(walker.files)} files, each named by live code")
    print("dead test helpers ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
