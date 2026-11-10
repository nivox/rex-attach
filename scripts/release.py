#!/usr/bin/env python3
"""Create a release commit/tag and resume development."""

import argparse
from pathlib import Path
import re
import shlex
import subprocess
import tomllib


class ReleaseError(Exception):
    pass


def git(repo, *args):
    return subprocess.check_output(
        ["git", "-C", str(repo), *args], text=True, stderr=subprocess.PIPE
    ).strip()


def version_tuple(value):
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value):
        raise argparse.ArgumentTypeError(f"{value!r} must be a plain X.Y.Z version")
    return tuple(map(int, value.split(".")))


def update_package_version(text, name, version):
    """Edit only this package's version, preserving other entries and comments."""
    changed = 0

    def replace_block(match):
        nonlocal changed
        block = match.group()
        package = tomllib.loads(block)["package"]
        if isinstance(package, list):
            package = package[0]
        if package.get("name") != name:
            return block
        block, count = re.subn(
            r'''(?m)^(version[ \t]*=[ \t]*)(["'])[^\n]*?\2''',
            lambda field: f"{field[1]}{field[2]}{version}{field[2]}",
            block,
        )
        if count != 1:
            raise ReleaseError(f"expected one version field for {name}")
        changed += 1
        return block

    result = re.sub(
        r"(?ms)^\[\[?package\]?\][^\n]*\n.*?(?=^\[|\Z)", replace_block, text
    )
    if changed != 1:
        raise ReleaseError(f"expected one package entry for {name}, found {changed}")
    return result


def set_version(repo, name, version):
    # Compute both edits before writing either file.
    edits = {
        repo / filename: update_package_version(
            (repo / filename).read_text(), name, version
        )
        for filename in ("Cargo.toml", "Cargo.lock")
    }
    for path, contents in edits.items():
        path.write_text(contents)


def release(args):
    repo = Path(git(Path.cwd(), "rev-parse", "--show-toplevel"))
    if git(repo, "status", "--porcelain", "--untracked-files=all"):
        raise ReleaseError("working tree is not clean; commit or stash first")
    try:
        branch = git(repo, "symbolic-ref", "--quiet", "--short", "HEAD")
    except subprocess.CalledProcessError as error:
        raise ReleaseError("HEAD is detached; check out a branch before releasing") from error
    manifest = tomllib.loads((repo / "Cargo.toml").read_text())["package"]
    name, current = manifest["name"], manifest["version"]
    base = current.removesuffix("-dev")
    current_parts = version_tuple(base)
    release_version = args.version or base
    release_parts = version_tuple(release_version)
    if release_parts < current_parts:
        raise ReleaseError("release version must not be older than the current version")
    next_version = args.next or f"{release_parts[0]}.{release_parts[1] + 1}.0"
    if version_tuple(next_version) <= release_parts:
        raise ReleaseError("next version must be newer than the release version")
    packages = tomllib.loads((repo / "Cargo.lock").read_text())["package"]
    locked = [package["version"] for package in packages if package["name"] == name]
    if locked != [current]:
        raise ReleaseError("Cargo.toml and Cargo.lock package versions do not match")
    tag = f"v{release_version}"
    if git(repo, "tag", "--list", tag):
        raise ReleaseError(f"tag {tag} already exists")
    if args.push and git(repo, "ls-remote", "--tags", "origin", f"refs/tags/{tag}"):
        raise ReleaseError(f"tag {tag} already exists on origin")
    git(repo, "var", "GIT_AUTHOR_IDENT")
    git(repo, "var", "GIT_COMMITTER_IDENT")
    next_dev = f"{next_version}-dev"
    print(f"1. Set {current} -> {release_version}, commit and tag {tag}")
    print(f"2. Set {release_version} -> {next_dev} and commit on {branch}")
    print(f"3. Atomically push {branch} and {tag} to origin" if args.push else "No push requested")
    if args.dry_run:
        return
    if not args.yes and input("Proceed? [y/N] ").lower() not in ("y", "yes"):
        raise ReleaseError("aborted")
    set_version(repo, name, release_version)
    git(repo, "add", "--", "Cargo.toml", "Cargo.lock")
    # A first release can start from an already-final Cargo version.
    git(repo, "commit", "--allow-empty", "-m", f"release: {tag}")
    git(repo, "tag", "-a", tag, "-m", f"{name} {tag}")
    set_version(repo, name, next_dev)
    git(repo, "add", "--", "Cargo.toml", "Cargo.lock")
    git(repo, "commit", "-m", f"chore: begin {next_dev}")
    print(f"Created {tag}; {branch} now carries {next_dev}.")
    if args.push:
        git(repo, "push", "--atomic", "origin", f"HEAD:refs/heads/{branch}", f"refs/tags/{tag}")
        print(f"Pushed {branch} and {tag}.")
    else:
        print("To publish, run:")
        print("  " + shlex.join([
            "git", "push", "--atomic", "origin", f"HEAD:refs/heads/{branch}", f"refs/tags/{tag}"
        ]))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", help="release X.Y.Z; default: current version without -dev")
    parser.add_argument("--next", help="next X.Y.Z before appending -dev; default: next minor")
    parser.add_argument("--push", action="store_true", help="push branch and tag atomically to origin")
    parser.add_argument("--yes", "-y", action="store_true", help="skip the confirmation prompt")
    parser.add_argument("--dry-run", action="store_true", help="validate and show the plan without edits")
    args = parser.parse_args()
    try:
        release(args)
    except (ReleaseError, argparse.ArgumentTypeError, EOFError, OSError, tomllib.TOMLDecodeError) as error:
        parser.exit(1, f"release: {error}\n")
    except subprocess.CalledProcessError as error:
        parser.exit(1, f"release: git failed: {error.stderr.strip()}\n")


if __name__ == "__main__":
    main()
