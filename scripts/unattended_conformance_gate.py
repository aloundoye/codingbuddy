#!/usr/bin/env python3
"""Reject publication without complete native Order 3 results for this revision and lockfile."""
import argparse
import json
import pathlib
from unattended_conformance import GROUPS, TARGETS, lockfile_sha256


def validate(reports, targets, revision, lockfile):
    found = {}
    for report in reports:
        target = report.get("target")
        if target in found:
            raise ValueError(f"Duplicate target evidence: {target}")
        if report.get("version") != 1 or report.get("revision") != revision or report.get("lockfile_sha256") != lockfile:
            raise ValueError("Evidence does not match the release implementation and lockfile")
        if report.get("native") is not True or report.get("worktree_clean") is not True or report.get("status") != "passed":
            raise ValueError(f"Native conformance is incomplete: {target}")
        groups = report.get("groups", {})
        if set(groups) != set(GROUPS):
            raise ValueError(f"Missing fixture groups: {target}")
        for name, group in groups.items():
            if group.get("status") != "passed" or group.get("exit_code") != 0 or not isinstance(group.get("passed"), int) or group["passed"] <= 0:
                raise ValueError(f"Fixture group did not pass: {target}/{name}")
            digest = group.get("output_sha256", "")
            if len(digest) != 64 or any(char not in "0123456789abcdef" for char in digest):
                raise ValueError("Missing fixture output fingerprint")
        found[target] = report
    if set(found) != set(targets):
        raise ValueError("Required platform evidence is missing or unexpected")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reports", nargs="+", type=pathlib.Path)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--target", action="append", choices=sorted(TARGETS))
    args = parser.parse_args()
    lockfile = lockfile_sha256()
    validate([json.loads(path.read_text()) for path in args.reports], args.target or TARGETS, args.revision, lockfile)


if __name__ == "__main__":
    main()
