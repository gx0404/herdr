#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
import subprocess
from pathlib import Path

ALLOWED_TYPES = {
    "feat",
    "fix",
    "perf",
    "docs",
    "ci",
    "test",
    "refactor",
    "chore",
    "release",
}
SUBJECT_RE = re.compile(r"^(?P<kind>[a-z]+)(?:\([^)]+\))?!?:\s+\S")
# Approved for the 2026-10-18 fork sync: retain this existing master commit
# without rewriting history. Only Git range inspection can use this exact pair;
# new subjects and commit-message files still have to satisfy valid_subject().
HISTORICAL_SUBJECT_EXCEPTIONS = {
    "5382ba3a18de6a6f21cdbdcf50016ae958528771": "ignore",
}


def git_subjects(rev_range: str) -> list[str]:
    output = subprocess.check_output(
        ["git", "log", "--encoding=UTF-8", "--pretty=format:%H%x00%s", rev_range],
        text=True,
        encoding="utf-8",
    )
    subjects = []
    for line in output.splitlines():
        commit, subject = line.split("\0", 1)
        if HISTORICAL_SUBJECT_EXCEPTIONS.get(commit) == subject:
            continue
        subjects.append(subject)
    return subjects


def valid_subject(subject: str) -> bool:
    match = SUBJECT_RE.match(subject)
    return bool(match and match.group("kind") in ALLOWED_TYPES)


def commit_message_subject(path: Path) -> str | None:
    for line in path.read_text(encoding="utf-8").splitlines():
        subject = line.strip()
        if subject and not subject.startswith("#"):
            return subject
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description="Validate conventional commit subjects")
    parser.add_argument("subjects", nargs="*")
    parser.add_argument("--range", dest="rev_range")
    parser.add_argument("--message-file")
    args = parser.parse_args()

    subjects = list(args.subjects)
    if args.rev_range:
        subjects.extend(git_subjects(args.rev_range))
    if args.message_file:
        subject = commit_message_subject(Path(args.message_file))
        if subject:
            subjects.append(subject)

    invalid = [subject for subject in subjects if not valid_subject(subject)]
    if invalid:
        print("invalid commit subject(s):")
        for subject in invalid:
            print(f"  {subject}")
        print(
            "commit subjects must use conventional commits because preview notes are generated from them."
        )
        print("example: fix(update): install selected channel")
        print("expected: type(optional-scope): subject")
        print("allowed types: " + ", ".join(sorted(ALLOWED_TYPES)))
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
