#!/usr/bin/env python3
"""Shared external dependencies have one root workspace pin.

The root `[workspace.dependencies]` table states a version once so it moves in
one place. That only holds while every member takes the name with
`workspace = true`: a member spelling `tokio = "1"` beside it is a second home
for the fact, and the two drift without either failing - a member restating
default features the workspace entry turns off silently undoes the restriction.

Two legs. The first: for every workspace member (the root `shepr` package
included), in every dependency table (normal, dev, build, and their target-cfg
variants), a name the root table declares must be taken with `workspace = true`
and must not also state `version`, `path`, `git` or `registry`. Features,
`optional` and `default-features = false` stay the member's own, since cargo
layers those onto the workspace entry.

The second closes the gap for names not in the root table yet: an external
package used by two or more workspace members must first be added there. Local
path dependencies (the `shepr-*` crates) are not external pins. Counts are by
member, so a package used as both a normal and dev dependency in one member
remains one user.
"""

from __future__ import annotations

import sys
import tomllib

from _brokkr_config import ROOT, workspace_member_manifests

DEPENDENCY_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")
SOURCE_KEYS = ("version", "path", "git", "registry")


def tables(manifest: dict):
    for name in DEPENDENCY_TABLES:
        yield name, manifest.get(name, {})
    for target, body in manifest.get("target", {}).items():
        for name in DEPENDENCY_TABLES:
            yield f"target.{target}.{name}", body.get(name, {})


def package_name(name: str, spec: object) -> str:
    if isinstance(spec, dict):
        package = spec.get("package", name)
        if isinstance(package, str):
            return package
    return name


def effective_package_name(name: str, spec: object, workspace_dependencies: dict) -> str:
    if isinstance(spec, dict) and spec.get("workspace") is True:
        inherited = workspace_dependencies.get(name, spec)
        return package_name(name, inherited)
    return package_name(name, spec)


def is_external_dependency(name: str, spec: object, workspace_dependencies: dict) -> bool:
    if isinstance(spec, dict) and "path" in spec:
        return False
    if isinstance(spec, dict) and spec.get("workspace") is True:
        inherited = workspace_dependencies.get(name)
        return not (isinstance(inherited, dict) and "path" in inherited)
    return True


def main() -> int:
    with (ROOT / "Cargo.toml").open("rb") as stream:
        workspace_dependencies = tomllib.load(stream)["workspace"].get("dependencies", {})
    pinned = set(workspace_dependencies)
    externally_pinned = {
        package_name(name, spec)
        for name, spec in workspace_dependencies.items()
        if not (isinstance(spec, dict) and "path" in spec)
    }
    if not pinned:
        print("the root manifest declares no [workspace.dependencies]; nothing to hold")
        return 1

    problems: list[str] = []
    external_users: dict[str, set[str]] = {}
    external_bypasses: dict[str, set[str]] = {}
    try:
        manifests = workspace_member_manifests()
    except RuntimeError as error:
        print(error)
        return 1
    for path in manifests:
        with path.open("rb") as stream:
            manifest = tomllib.load(stream)
        where = path.relative_to(ROOT)
        member_external: set[str] = set()
        member_bypasses: set[tuple[str, str]] = set()
        for table, deps in tables(manifest):
            for name, spec in deps.items():
                package = effective_package_name(name, spec, workspace_dependencies)
                if is_external_dependency(name, spec, workspace_dependencies):
                    member_external.add(package)
                    if name not in workspace_dependencies:
                        member_bypasses.add((package, f"{where} [{table}] as {name}"))
                if name not in pinned:
                    continue
                if not isinstance(spec, dict) or spec.get("workspace") is not True:
                    problems.append(
                        f"{where} [{table}] takes {name} without `workspace = true`; "
                        "the root manifest pins it"
                    )
                    continue
                restated = [key for key in SOURCE_KEYS if key in spec]
                if restated:
                    problems.append(
                        f"{where} [{table}] restates {', '.join(restated)} for {name}, "
                        "which the root manifest pins"
                    )
        for package in member_external:
            external_users.setdefault(package, set()).add(str(where))
        for package, dependency in member_bypasses:
            external_bypasses.setdefault(package, set()).add(dependency)

    for package, users in sorted(external_users.items()):
        if len(users) < 2:
            continue
        if package not in externally_pinned:
            named_users = ", ".join(sorted(users))
            problems.append(
                f"{package} is an external dependency in {len(users)} workspace members "
                f"but has no root workspace pin: {named_users}"
            )
        elif package in external_bypasses:
            bypasses = ", ".join(sorted(external_bypasses[package]))
            problems.append(
                f"{package} is shared by {len(users)} workspace members, but these "
                f"dependencies do not take its workspace pin: {bypasses}"
            )

    for problem in problems:
        print(problem)
    if problems:
        print(f"{len(problems)} workspace dependency violation(s)")
        return 1
    print(f"checked {len(manifests)} member manifests against {len(pinned)} workspace pins")
    print("workspace dependencies ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
