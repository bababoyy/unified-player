#!/usr/bin/env python3
"""Run cargo-deny while emitting only bounded diagnostic categories."""

from __future__ import annotations

import argparse
import collections
import datetime
import json
import os
import re
import subprocess
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python 3.10 local maintenance environments.
    import tomli as tomllib


SAFE_CODE = re.compile(r"[a-z0-9-]+")
SAFE_SEVERITIES = {"error", "warning", "note", "help"}
CHECKS = ("advisories", "bans", "licenses", "sources")
ADVISORY_ID = re.compile(r"RUSTSEC-[0-9]{4}-[0-9]{4,8}")
EXPIRY = re.compile(r"owner accepted (?:until|through) ([0-9]{4}-[0-9]{2}-[0-9]{2}):")


def advisory_ids(output: str) -> list[str]:
    """Only emit public IDs from cargo-deny's structured advisory notes."""
    found: set[str] = set()
    for line in output.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        fields = record.get("fields") if isinstance(record, dict) else None
        notes = fields.get("notes") if isinstance(fields, dict) else None
        if not isinstance(notes, list):
            continue
        for note in notes:
            if isinstance(note, str) and note.startswith("ID: "):
                value = note[4:]
                if ADVISORY_ID.fullmatch(value):
                    found.add(value)
    return sorted(found)


def exception_issues(policy: dict, today: datetime.date) -> list[str]:
    issues = []
    for entry in policy.get("advisories", {}).get("ignore", []):
        if not isinstance(entry, dict):
            issues.append("undated-exception")
            continue
        reason = entry.get("reason", "")
        match = EXPIRY.match(reason) if isinstance(reason, str) else None
        try:
            expires = datetime.date.fromisoformat(match[1]) if match else None
        except ValueError:
            expires = None
        if expires is None:
            issues.append("undated-exception")
        elif today > expires:
            issues.append("expired-exception")
    return issues


def check_exceptions() -> bool:
    try:
        policy = tomllib.loads(Path("deny.toml").read_text(encoding="utf-8"))
        issues = exception_issues(policy, datetime.datetime.now(datetime.timezone.utc).date())
    except (OSError, ValueError, TypeError, AttributeError):
        print("dependency exception policy could not be read")
        return False
    for category, count in sorted(collections.Counter(issues).items()):
        print(f"severity=error; code={category}; count={count}")
    if not issues:
        print("dependency exception dates: ok")
    return not issues


def diagnostic_counts(output: str) -> collections.Counter[tuple[str, str]]:
    counts: collections.Counter[tuple[str, str]] = collections.Counter()
    for line in output.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        fields = record.get("fields") if isinstance(record, dict) else None
        if not isinstance(fields, dict):
            continue
        if "severity" not in fields and "code" not in fields:
            continue
        severity = fields.get("severity")
        code = fields.get("code")
        if severity not in SAFE_SEVERITIES:
            severity = "unknown"
        if not isinstance(code, str) or SAFE_CODE.fullmatch(code) is None:
            code = "unknown"
        counts[(severity, code)] += 1
    return counts


def render_summary(counts: collections.Counter[tuple[str, str]]) -> str:
    return "\n".join(
        f"severity={severity}; code={code}; count={count}"
        for (severity, code), count in sorted(counts.items())
    )


def self_test() -> None:
    private_value = "private_fixture_value_that_must_never_render"
    record = json.dumps(
        {
            "fields": {
                "severity": "error",
                "code": "unknown-git",
                "message": private_value,
                "labels": [{"message": private_value}],
            }
        }
    )
    summary = render_summary(diagnostic_counts(record))
    if summary != "severity=error; code=unknown-git; count=1":
        raise RuntimeError("dependency-policy-self-test-mismatch")
    if private_value in summary:
        raise RuntimeError("dependency-policy-self-test-leak")
    ids = advisory_ids(json.dumps({"fields": {"notes": [
        "ID: RUSTSEC-2026-0194", "ID: RUSTSEC-2026-0194",
        f"ID: RUSTSEC-2026-0195 {private_value}", private_value,
    ]}}) + "\n[]\nnull\ninvalid")
    if ids != ["RUSTSEC-2026-0194"]:
        raise RuntimeError("dependency-policy-advisory-id-mismatch")
    today = datetime.date(2026, 11, 22)
    for reason, expected in [
        ("owner accepted until 2026-11-22: reviewed", []),
        ("owner accepted through 2026-11-21: reviewed", ["expired-exception"]),
        ("owner accepted until 2026-13-22: reviewed", ["undated-exception"]),
        ("unbounded exception", ["undated-exception"]),
    ]:
        if exception_issues({"advisories": {"ignore": [{"reason": reason}]}}, today) != expected:
            raise RuntimeError("dependency-policy-exception-date-mismatch")
    print("dependency policy self-test: ok (raw fields suppressed)")


def run_policy() -> int:
    if not check_exceptions():
        return 1
    executable = os.environ.get("CARGO_DENY", "cargo-deny")
    try:
        result = subprocess.run(
            [
                executable,
                "--format",
                "json",
                "--log-level",
                "error",
                "check",
                *CHECKS,
            ],
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
    except OSError:
        print("dependency policy failed before producing a safe summary")
        return 1

    counts = diagnostic_counts(f"{result.stdout}\n{result.stderr}")
    summary = render_summary(counts)
    if summary:
        print(summary)
    for advisory in advisory_ids(f"{result.stdout}\n{result.stderr}"):
        print(f"advisory={advisory}; url=https://rustsec.org/advisories/{advisory}.html")
    if result.returncode != 0:
        print("dependency policy rejected the resolved graph; raw diagnostics suppressed")
        return 1
    print("dependency policy: ok")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--check-exceptions", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        try:
            self_test()
        except (RuntimeError, TypeError, ValueError):
            print("dependency policy self-test failed without exposing fixture data")
            return 1
        return 0
    if args.check_exceptions:
        return 0 if check_exceptions() else 1
    return run_policy()


if __name__ == "__main__":
    sys.exit(main())
