#!/usr/bin/env python3
"""Fail when runtime Rust code writes to the terminal outside owned surfaces.

This is intentionally a small source guard, not a Rust parser. It catches the
common direct-I/O forms that caused backend output to bypass the TUI. Existing
test-only output is ignored.
"""

from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
SOURCE_ROOT = ROOT / "unified-player" / "src"

UI_OWNED = {
    "unified-player/src/ui/mod.rs",
    "unified-player/src/ui/cover_image.rs",
    "unified-player/src/main.rs",
    "unified-player/src/developer_capture/private_view.rs",
}
CLI_OWNED_PREFIX = "unified-player/src/cli/"
TEST_OUTPUT_FILES = {
    "unified-player/src/client/youtube/javascript.rs",
    "unified-player/src/client/youtube/playback/tests/forensics.rs",
    "unified-player/src/developer_capture/analysis.rs",
    "unified-player/src/developer_capture/recorder.rs",
    "unified-player/src/developer_capture/replay.rs",
    "unified-player/src/developer_capture/sanitize.rs",
}

DIRECT_IO = re.compile(
    r"\b(?:e?println|e?print)!\s*\(|"
    r"\bstd::io::(?:stdin|stdout|stderr)\s*\(|"
    r"\b(?:stdin|stdout|stderr)\s*\(\s*\)"
)


def is_test_line(line: str, brace_depth: int, test_depth: int | None) -> tuple[bool, int | None]:
    """Track cfg(test) blocks well enough to ignore benchmark diagnostics."""
    if test_depth is None and "#[cfg(test)]" in line:
        test_depth = brace_depth
    in_test = test_depth is not None
    next_depth = brace_depth + line.count("{") - line.count("}")
    if test_depth is not None and next_depth <= test_depth:
        test_depth = None
    return in_test, test_depth


def main() -> int:
    violations: list[tuple[str, int, str]] = []

    for path in sorted(SOURCE_ROOT.rglob("*.rs")):
        relative = path.relative_to(ROOT).as_posix()
        brace_depth = 0
        test_depth: int | None = None
        for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            in_test, test_depth = is_test_line(line, brace_depth, test_depth)
            match = DIRECT_IO.search(line)
            brace_depth += line.count("{") - line.count("}")
            if not match or in_test:
                continue
            if relative in TEST_OUTPUT_FILES:
                continue
            if relative in UI_OWNED or relative.startswith(CLI_OWNED_PREFIX):
                continue
            violations.append((relative, line_number, line.strip()))

    if violations:
        print("terminal ownership violations:")
        for relative, line_number, line in violations:
            print(f"  {relative}:{line_number}: {line}")
        return 1
    print("terminal ownership check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
