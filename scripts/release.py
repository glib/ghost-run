#!/usr/bin/env python3
"""Ghost Run's versions (docs/releasing.md): Semantic Versioning, worked out
from Conventional Commits.

Every PR title is a Conventional Commit subject (`feat: ...`, `fix(time): ...`,
`feat!: ...`), and squash merges make it the commit on main. After CI passes on
main, the release job runs `next`: the commits since the last `vX.Y.Z` tag
decide the next version (a breaking change bumps major, `feat` minor, `fix`,
`perf` and `revert` patch; `docs`, `test`, `ci`, `chore`, `build`, `style`
and `refactor` make no release on their own). Nothing to release: it prints
nothing. No tag yet: 1.0.0.

    scripts/release.py next [--notes FILE]   # the next version, and its notes
    scripts/release.py check-title "TITLE"   # a PR title CI accepts?

Stdlib only.
"""

import argparse
import re
import subprocess
import sys

FIRST = (1, 0, 0)
# The kinds of change, what each bumps, and its heading in the notes.
TYPES = {
    "feat": ("minor", "New"),
    "fix": ("patch", "Fixes"),
    "perf": ("patch", "Faster"),
    "revert": ("patch", "Reverted"),
    "refactor": (None, None),
    "docs": (None, None),
    "test": (None, None),
    "ci": (None, None),
    "build": (None, None),
    "chore": (None, None),
    "style": (None, None),
}
RANK = {None: 0, "patch": 1, "minor": 2, "major": 3}
SUBJECT = re.compile(
    r"^(?P<type>[a-z]+)(?:\((?P<scope>[\w./, -]+)\))?(?P<bang>!)?: (?P<desc>\S.*)$"
)
BREAKING_FOOTER = re.compile(r"^BREAKING[ -]CHANGE: ", re.MULTILINE)


def parse(subject, body=""):
    """(type, scope, breaking, description), or None if it isn't one."""
    m = SUBJECT.match(subject.strip())
    if not m or m["type"] not in TYPES:
        return None
    breaking = bool(m["bang"]) or bool(BREAKING_FOOTER.search(body or ""))
    return m["type"], m["scope"], breaking, m["desc"].strip()


def bump_of(subject, body=""):
    """What a commit bumps. One that isn't a Conventional Commit (from before
    they were adopted, or merged by hand) counts as a fix: it changed something."""
    parsed = parse(subject, body)
    if parsed is None:
        return "patch"
    kind, _, breaking, _ = parsed
    return "major" if breaking else TYPES[kind][0]


def bump(version, level):
    major, minor, patch = version
    if level == "major":
        return (major + 1, 0, 0)
    if level == "minor":
        return (major, minor + 1, 0)
    if level == "patch":
        return (major, minor, patch + 1)
    return version


def check_title(title):
    """None if `title` is fine for a PR, else what's wrong."""
    if parse(title) is not None:
        return None
    m = SUBJECT.match(title.strip())
    kinds = ", ".join(TYPES)
    if m and m["type"] not in TYPES:
        return f"'{m['type']}' isn't a kind of change. Use one of: {kinds}."
    return (
        "A PR title is a Conventional Commit subject, which picks the next version: "
        "'feat: ...' (minor), 'fix: ...' (patch), 'feat!: ...' (breaking: major), "
        f"optionally with a scope ('fix(import): ...'). Kinds: {kinds}."
    )


def git(*args, cwd=None):
    return subprocess.run(
        ["git", *args], cwd=cwd, check=True, capture_output=True, text=True
    ).stdout


def last_tag(cwd=None):
    """The latest vX.Y.Z tag reachable from HEAD and its version, or (None, None)."""
    try:
        tag = git("describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", cwd=cwd).strip()
    except subprocess.CalledProcessError:
        return None, None
    m = re.fullmatch(r"v(\d+)\.(\d+)\.(\d+)", tag)
    if not m:
        return None, None
    return tag, tuple(int(x) for x in m.groups())


def commits(since, cwd=None):
    """[(subject, body)] since the tag (all of them with none), oldest first."""
    span = [f"{since}..HEAD"] if since else ["HEAD"]
    out = git("log", "--reverse", "--format=%s%x1f%b%x1e", *span, cwd=cwd)
    found = []
    for entry in out.split("\x1e"):
        entry = entry.strip("\n")
        if entry:
            subject, _, body = entry.partition("\x1f")
            found.append((subject, body))
    return found


def notes(found):
    """Markdown release notes: the commits that changed something, by kind."""
    sections = {"Breaking changes": []}
    for _, heading in TYPES.values():
        if heading:
            sections.setdefault(heading, [])
    sections["Other changes"] = []
    for subject, body in found:
        parsed = parse(subject, body)
        if parsed is None:
            sections["Other changes"].append(subject.strip())
            continue
        kind, scope, breaking, desc = parsed
        line = f"**{scope}:** {desc}" if scope else desc
        if breaking:
            sections["Breaking changes"].append(line)
        elif TYPES[kind][1]:
            sections[TYPES[kind][1]].append(line)
    parts = []
    for heading, lines in sections.items():
        if lines:
            parts.append(f"### {heading}\n\n" + "\n".join(f"- {l}" for l in lines))
    return "\n\n".join(parts) + "\n"


def next_release(cwd=None):
    """(version string, notes), or (None, None) when nothing needs releasing."""
    tag, version = last_tag(cwd)
    found = commits(tag, cwd)
    if tag is None:
        return "%d.%d.%d" % FIRST, "The first release.\n"
    level = max((bump_of(s, b) for s, b in found), key=RANK.get, default=None)
    if level is None:
        return None, None
    return "%d.%d.%d" % bump(version, level), notes(found)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    n = sub.add_parser("next", help="print the next version (nothing: no release)")
    n.add_argument("--notes", help="write the release notes here")
    t = sub.add_parser("check-title", help="check a PR title")
    t.add_argument("title")
    args = parser.parse_args(argv)
    if args.command == "check-title":
        problem = check_title(args.title)
        if problem:
            print(problem, file=sys.stderr)
            return 1
        print(f"OK: bumps {bump_of(args.title) or 'nothing (no release)'}")
        return 0
    version, text = next_release()
    if version:
        print(version)
        if args.notes:
            with open(args.notes, "w") as f:
                f.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
