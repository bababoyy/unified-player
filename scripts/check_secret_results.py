#!/usr/bin/env python3
"""Reduce TruffleHog JSON lines to a secret-safe policy summary."""

from __future__ import annotations

import argparse
import collections
import json
import re
import sys
from pathlib import Path


ALLOWLIST_PATH = Path("secret-scan-allowlist.json")
SAFE_DETECTOR = re.compile(r"[A-Za-z0-9_-]+")
ALLOWLIST_KEYS = {"detector", "commit", "file", "line", "reason"}


class SecretResultError(RuntimeError):
    pass


def load_allowlist(path: Path) -> set[tuple[str, str, str, int]]:
    try:
        records = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SecretResultError("allowlist-read-error") from error
    if not isinstance(records, list):
        raise SecretResultError("allowlist-shape-error")

    allowed: set[tuple[str, str, str, int]] = set()
    for record in records:
        if not isinstance(record, dict) or set(record) != ALLOWLIST_KEYS:
            raise SecretResultError("allowlist-record-error")
        detector = record["detector"]
        commit = record["commit"]
        file = record["file"]
        line = record["line"]
        reason = record["reason"]
        text_values = (detector, commit, file, reason)
        if not all(isinstance(value, str) and value for value in text_values):
            raise SecretResultError("allowlist-value-error")
        if not isinstance(line, int) or line < 1:
            raise SecretResultError("allowlist-line-error")
        allowed.add((detector, commit, file, line))
    return allowed


def finding_identity(record: dict[str, object]) -> tuple[str, str, str, int] | None:
    try:
        detector = record["DetectorName"]
        git = record["SourceMetadata"]["Data"]["Git"]  # type: ignore[index]
        commit = git["commit"]
        file = git["file"]
        line = git["line"]
    except (KeyError, TypeError):
        return None
    if not all(isinstance(value, str) for value in (detector, commit, file)):
        return None
    if not isinstance(line, int):
        return None
    return detector, commit, file, line


def summarize(
    lines: list[str],
    allowed: set[tuple[str, str, str, int]],
) -> tuple[collections.Counter[str], int]:
    active: collections.Counter[str] = collections.Counter()
    accepted = 0
    for raw_line in lines:
        if not raw_line.strip():
            continue
        try:
            record = json.loads(raw_line)
        except json.JSONDecodeError as error:
            raise SecretResultError("scanner-json-error") from error
        if not isinstance(record, dict):
            raise SecretResultError("scanner-record-error")
        identity = finding_identity(record)
        if identity is not None and identity in allowed:
            accepted += 1
            continue
        detector = record.get("DetectorName")
        if not isinstance(detector, str) or SAFE_DETECTOR.fullmatch(detector) is None:
            detector = "unknown"
        active[detector] += 1
    return active, accepted


def render(active: collections.Counter[str], accepted: int) -> str:
    lines = [
        f"detector={detector}; count={count}"
        for detector, count in sorted(active.items())
    ]
    lines.append(f"Secret scan accepted {accepted} reviewed baseline candidate(s).")
    if active:
        lines.append(
            f"Secret scan found {sum(active.values())} new candidate(s); "
            "matched material is intentionally suppressed."
        )
        lines.append("status=findings")
    else:
        lines.append("Secret scan found no new candidates.")
        lines.append("status=clean")
    return "\n".join(lines)


def self_test() -> None:
    private_value = "private-fixture/value-never-render"
    baseline = {
        "DetectorName": "KnownDetector",
        "SourceMetadata": {
            "Data": {"Git": {"commit": "abc", "file": "src/a.rs", "line": 7}}
        },
        "Raw": private_value,
    }
    active = {
        "DetectorName": private_value,
        "SourceMetadata": {
            "Data": {"Git": {"commit": "def", "file": "src/b.rs", "line": 8}}
        },
        "Raw": private_value,
    }
    counts, accepted = summarize(
        [json.dumps(baseline), json.dumps(active)],
        {("KnownDetector", "abc", "src/a.rs", 7)},
    )
    summary = render(counts, accepted)
    if counts != {"unknown": 1} or accepted != 1 or private_value in summary:
        raise SecretResultError("secret-summary-self-test-mismatch")
    print("secret summary self-test: ok (raw fields suppressed)")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    try:
        if args.self_test:
            self_test()
            return 0
        allowed = load_allowlist(ALLOWLIST_PATH)
        active, accepted = summarize(sys.stdin.readlines(), allowed)
        print(render(active, accepted))
        return 0
    except (SecretResultError, TypeError, ValueError):
        print("Secret scan summary failed without exposing matched material.")
        return 1


if __name__ == "__main__":
    sys.exit(main())
