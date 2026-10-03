#!/usr/bin/env python3
"""Check publishable paths without reading or printing candidate contents."""

from __future__ import annotations

import argparse
import collections
import fnmatch
import subprocess
import sys
from pathlib import Path, PurePosixPath


MAX_TRACKED_BYTES = 5 * 1024 * 1024
PACKAGES = ("unified-player", "lyric_finder")
VENDORED_EJS_MANIFEST = Path("crates/ytdlp-ejs/Cargo.toml")
VENDORED_EJS_NOTICE = "crates/ytdlp-ejs/THIRD_PARTY_NOTICES.md"
VENDORED_EJS_PACKAGE_NOTICE = "THIRD_PARTY_NOTICES.md"
LICENSE_PATHS = (
    Path("LICENSE"),
    Path("unified-player/LICENSE"),
)
# lyric_finder is the upstream crate with only mechanical lint fixes, so it
# keeps the upstream notice: the root license without later copyright lines.
UPSTREAM_ONLY_LICENSE_PATHS = (Path("lyric_finder/LICENSE"),)
COMMON_PACKAGE_PATHS = {
    ".cargo_vcs_info.json",
    "Cargo.lock",
    "Cargo.toml",
    "Cargo.toml.orig",
    "LICENSE",
}
GENERATED_PACKAGE_PATHS = {".cargo_vcs_info.json", "Cargo.toml.orig"}
PACKAGE_BOUNDARIES = {
    "unified-player": ({"README.md", "build.rs"}, ("src/", "tests/")),
    "lyric_finder": ({"rustfmt.toml"}, ("examples/", "src/")),
}
SENSITIVE_FILENAMES = {
    ".env",
    ".envrc",
    "auth.json",
    "browser-path.txt",
    "cookie.txt",
    "cookies.txt",
    "credentials",
    "credentials.json",
    "id_ed25519",
    "id_rsa",
    "oauth.json",
    "po_token.txt",
}
SENSITIVE_SUFFIXES = {
    ".db": "local-database",
    ".har": "network-capture",
    ".key": "key-material",
    ".log": "log-file",
    ".p12": "key-material",
    ".pcap": "network-capture",
    ".pcapng": "network-capture",
    ".pem": "key-material",
    ".pfx": "key-material",
    ".sqlite": "local-database",
    ".sqlite3": "local-database",
}
ARTIFACT_SUFFIXES = {
    ".7z",
    ".crate",
    ".dll",
    ".dylib",
    ".exe",
    ".gz",
    ".rar",
    ".so",
    ".tar",
    ".tgz",
    ".zip",
}
BACKUP_SUFFIXES = {".bak", ".bk"}
BLOCKED_COMPONENTS = {
    ".git": "repository-metadata",
    "browser-profile": "browser-state",
    "chrome-user-data": "browser-state",
    "target": "build-output",
    "__pycache__": "generated-cache",
}


class PublicationCheckError(RuntimeError):
    pass


def normalized_path(value: str) -> PurePosixPath:
    return PurePosixPath(value.replace("\\", "/"))


def classify_path(value: str) -> set[str]:
    path = normalized_path(value)
    lowered_parts = tuple(part.lower() for part in path.parts)
    name = lowered_parts[-1] if lowered_parts else ""
    categories: set[str] = set()

    if path.is_absolute() or ".." in path.parts:
        categories.add("unsafe-path")
    if name in SENSITIVE_FILENAMES:
        categories.add("credential-file")
    for suffix, category in SENSITIVE_SUFFIXES.items():
        if name.endswith(suffix):
            categories.add(category)
    if any(name.endswith(suffix) for suffix in ARTIFACT_SUFFIXES):
        categories.add("binary-or-archive-artifact")
    if any(name.endswith(suffix) for suffix in BACKUP_SUFFIXES):
        categories.add("backup-file")
    for component, category in BLOCKED_COMPONENTS.items():
        if component in lowered_parts:
            categories.add(category)

    return categories


def tracked_paths() -> list[str]:
    return git_paths([])


def git_paths(arguments: list[str]) -> list[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z", *arguments],
        check=True,
        capture_output=True,
    )
    return [
        value.decode("utf-8", errors="surrogateescape")
        for value in result.stdout.split(b"\0")
        if value
    ]


def package_paths(package: str) -> list[str]:
    result = subprocess.run(
        [
            "cargo",
            "package",
            "--locked",
            "--allow-dirty",
            "--list",
            "-p",
            package,
        ],
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if result.returncode != 0:
        raise PublicationCheckError(f"cargo-package-list-error:{package}")
    return [line.strip() for line in result.stdout.splitlines() if line.strip()]


def manifest_package_paths(manifest: Path) -> list[str]:
    result = subprocess.run(
        [
            "cargo",
            "package",
            "--manifest-path",
            str(manifest),
            "--locked",
            "--allow-dirty",
            "--list",
        ],
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if result.returncode != 0:
        raise PublicationCheckError(f"cargo-package-list-error:{manifest}")
    return [line.strip() for line in result.stdout.splitlines() if line.strip()]


def violation_counts(paths: list[str]) -> collections.Counter[str]:
    counts: collections.Counter[str] = collections.Counter()
    for value in paths:
        counts.update(classify_path(value))
    return counts


def package_boundary_violations(package: str, paths: list[str]) -> int:
    exact, prefixes = PACKAGE_BOUNDARIES[package]
    allowed_exact = COMMON_PACKAGE_PATHS | exact
    return sum(
        normalized_path(value).as_posix() not in allowed_exact
        and not any(
            normalized_path(value).as_posix().startswith(prefix)
            for prefix in prefixes
        )
        for value in paths
    )


def package_source_path(package: str, value: str) -> str | None:
    path = normalized_path(value).as_posix()
    if path in GENERATED_PACKAGE_PATHS:
        return None
    if path == "Cargo.lock":
        return "Cargo.lock"
    if package == "unified-player" and path == "README.md":
        return "README.md"
    return f"{package}/{path}"


def untracked_package_violations(
    package: str,
    paths: list[str],
    tracked: set[str],
) -> int:
    return sum(
        source is not None and source not in tracked
        for source in (package_source_path(package, value) for value in paths)
    )


def docker_context_included(value: str, patterns: list[str]) -> bool:
    path = normalized_path(value).as_posix().lstrip("/")
    included = True
    for raw_pattern in patterns:
        pattern = raw_pattern.strip()
        if not pattern or pattern.startswith("#"):
            continue
        negated = pattern.startswith("!")
        if negated:
            pattern = pattern[1:]
        pattern = pattern.lstrip("/")
        if pattern.endswith("/"):
            prefix = pattern.rstrip("/")
            matches = path == prefix
        else:
            matches = fnmatch.fnmatchcase(path, pattern)
        if matches:
            included = negated
    return included


def untracked_docker_violations(
    paths: list[str],
    tracked: set[str],
    patterns: list[str],
) -> int:
    return sum(
        value not in tracked and docker_context_included(value, patterns)
        for value in paths
    )


def docker_context_policy(tracked: set[str]) -> bool:
    try:
        patterns = Path(".dockerignore").read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise PublicationCheckError("dockerignore-read-error") from error

    expected = {
        "Cargo.lock": True,
        "unified-player/src/main.rs": True,
        "lyric_finder/examples/lyric-finder.rs": True,
        "lyric_finder/src/lib.rs": True,
        ".env": False,
        "capture/session.har": False,
        "browser-profile/Default/History": False,
        "target/debug/unified-player.exe": False,
        "MIGRATION_BACKUP_NOTES.md": False,
    }
    failures = sum(
        docker_context_included(path, patterns) != should_include
        for path, should_include in expected.items()
    )
    if failures:
        print(f"Docker context: rejected {failures} policy expectation(s)")
        print(f"category=docker-context-policy; count={failures}")
        return False

    roots = [
        "Cargo.lock",
        "Cargo.toml",
        "LICENSE",
        "README.md",
        "rust-toolchain.toml",
        "unified-player",
        "lyric_finder",
    ]
    candidates = set(
        git_paths(["--cached", "--others", "--exclude-standard", "--", *roots])
    )
    candidates.update(
        git_paths(
            [
                "--others",
                "--ignored",
                "--exclude-standard",
                "--",
                "unified-player",
                "lyric_finder",
            ]
        )
    )
    untracked = untracked_docker_violations(
        sorted(candidates),
        tracked,
        patterns,
    )
    if untracked:
        print(f"Docker context: rejected {untracked} untracked item(s)")
        print(f"category=untracked-docker-context-file; count={untracked}")
        return False
    print(f"Docker context: ok ({len(expected)} policy cases; tracked inputs only)")
    return True


def upstream_only_license(text: str) -> str:
    lines = []
    seen_copyright = False
    for line in text.splitlines():
        if line.startswith("Copyright "):
            if seen_copyright:
                continue
            seen_copyright = True
        lines.append(line)
    return "\n".join(lines)


def license_copy_policy() -> bool:
    try:
        licenses = [
            "\n".join(path.read_text(encoding="utf-8").splitlines())
            for path in LICENSE_PATHS
        ]
        upstream_only = [
            "\n".join(path.read_text(encoding="utf-8").splitlines())
            for path in UPSTREAM_ONLY_LICENSE_PATHS
        ]
    except OSError as error:
        raise PublicationCheckError("license-read-error") from error
    expected_upstream = upstream_only_license(licenses[0])
    mismatches = sum(value != licenses[0] for value in licenses[1:]) + sum(
        value != expected_upstream for value in upstream_only
    )
    if mismatches:
        print(f"License copies: rejected {mismatches} divergent file(s)")
        print(f"category=license-copy-mismatch; count={mismatches}")
        return False
    print(
        f"License copies: ok ({len(licenses)} identical, "
        f"{len(upstream_only)} upstream-only)"
    )
    return True


def vendored_notice_violations(
    package_paths: list[str], tracked: set[str], notice_exists: bool
) -> collections.Counter[str]:
    counts: collections.Counter[str] = collections.Counter()
    if not notice_exists:
        counts["missing-vendored-notice"] += 1
    if VENDORED_EJS_PACKAGE_NOTICE not in {
        normalized_path(path).as_posix() for path in package_paths
    }:
        counts["vendored-notice-not-packaged"] += 1
    if VENDORED_EJS_NOTICE not in {
        normalized_path(path).as_posix() for path in tracked
    }:
        counts["untracked-vendored-notice"] += 1
    return counts


def vendored_notice_policy(tracked: set[str]) -> bool:
    try:
        package_paths_value = manifest_package_paths(VENDORED_EJS_MANIFEST)
    except PublicationCheckError:
        print("Vendored EJS package: rejected package-list-error")
        print("category=vendored-package-list-error; count=1")
        return False

    counts = vendored_notice_violations(
        package_paths_value,
        tracked,
        Path(VENDORED_EJS_NOTICE).is_file(),
    )
    if counts:
        print(f"Vendored EJS package: rejected {sum(counts.values())} notice issue(s)")
        for category, count in sorted(counts.items()):
            print(f"category={category}; count={count}")
        return False

    print("Vendored EJS package: ok (tracked notice is Cargo-packaged)")
    return True


def tracked_size_violations(paths: list[str]) -> int:
    count = 0
    for value in paths:
        path = Path(value)
        try:
            if path.lstat().st_size > MAX_TRACKED_BYTES:
                count += 1
        except OSError as error:
            raise PublicationCheckError("tracked-file-stat-error") from error
    return count


def report_surface(
    label: str,
    paths: list[str],
    package: str | None = None,
    tracked: set[str] | None = None,
) -> bool:
    counts = violation_counts(paths)
    if package is not None:
        if tracked is None:
            raise PublicationCheckError("package-tracking-state-error")
        normalized = {normalized_path(path).as_posix() for path in paths}
        if "LICENSE" not in normalized:
            counts["missing-license-file"] += 1
        unexpected = package_boundary_violations(package, paths)
        if unexpected:
            counts["unexpected-package-file"] += unexpected
        untracked = untracked_package_violations(package, paths, tracked)
        if untracked:
            counts["untracked-package-source"] += untracked

    if counts:
        print(f"{label}: rejected {sum(counts.values())} publication item(s)")
        for category, count in sorted(counts.items()):
            print(f"category={category}; count={count}")
        return False

    print(f"{label}: ok ({len(paths)} paths inspected)")
    return True


def self_test() -> None:
    owner_license = "MIT License\n\nCopyright (c) 2021 Upstream\nCopyright (c) 2026 Owner\n\nTerms"
    if upstream_only_license(owner_license) != "MIT License\n\nCopyright (c) 2021 Upstream\n\nTerms":
        raise PublicationCheckError("self-test-upstream-license")
    cases = {
        "src/main.rs": set(),
        "Cargo.toml.orig": set(),
        "local/cookie.txt": {"credential-file"},
        "capture/session.har": {"network-capture"},
        "keys/private.pem": {"key-material"},
        "dist/application.exe": {"binary-or-archive-artifact"},
        "browser-profile/Default/History": {"browser-state"},
        "target/debug/application": {"build-output"},
        "scripts/__pycache__/check.cpython-312.pyc": {"generated-cache"},
        "../outside": {"unsafe-path"},
    }
    for value, expected in cases.items():
        actual = classify_path(value)
        if actual != expected:
            raise PublicationCheckError(
                f"self-test-category-mismatch:{','.join(sorted(expected))}"
            )

    docker_patterns = ["*", "!Cargo.toml", "!app/", "!app/src/", "!app/src/**"]
    docker_cases = {
        "Cargo.toml": True,
        "app/src/main.rs": True,
        "app/private.env": False,
        "capture.har": False,
    }
    for value, expected in docker_cases.items():
        if docker_context_included(value, docker_patterns) != expected:
            raise PublicationCheckError("self-test-docker-context-mismatch")

    if untracked_docker_violations(
        ["app/src/main.rs", "Cargo.toml"],
        {"Cargo.toml"},
        docker_patterns,
    ) != 1:
        raise PublicationCheckError("self-test-docker-tracking-mismatch")

    package_cases = {
        "src/main.rs": 0,
        "build.rs": 0,
        "private/notes.txt": 1,
    }
    for value, expected in package_cases.items():
        actual = package_boundary_violations("unified-player", [value])
        if actual != expected:
            raise PublicationCheckError("self-test-package-boundary-mismatch")

    package_tracking_cases = {
        "src/main.rs": "unified-player/src/main.rs",
        "README.md": "README.md",
        "Cargo.lock": "Cargo.lock",
        ".cargo_vcs_info.json": None,
    }
    for value, expected in package_tracking_cases.items():
        if package_source_path("unified-player", value) != expected:
            raise PublicationCheckError("self-test-package-tracking-mismatch")
    if untracked_package_violations(
        "unified-player",
        ["src/main.rs"],
        set(),
    ) != 1:
        raise PublicationCheckError("self-test-package-untracked-mismatch")
    notice_path = VENDORED_EJS_NOTICE
    if vendored_notice_violations(
        [VENDORED_EJS_PACKAGE_NOTICE],
        {notice_path},
        True,
    ):
        raise PublicationCheckError("self-test-vendored-notice-match-mismatch")
    expected_notice_violations = {
        "missing-vendored-notice": 1,
        "untracked-vendored-notice": 1,
        "vendored-notice-not-packaged": 1,
    }
    if vendored_notice_violations([], set(), False) != expected_notice_violations:
        raise PublicationCheckError("self-test-vendored-notice-violation-mismatch")
    print(
        "publication policy self-test: ok "
        f"({len(cases) + len(docker_cases) + len(package_cases) + 8} cases)"
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check tracked and Cargo-packaged paths without printing names"
    )
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument(
        "--tracked-only", action="store_true",
        help="Check tracked paths, sizes, licenses and Docker exclusions without Cargo",
    )
    args = parser.parse_args()

    try:
        if args.self_test:
            self_test()
            return 0

        tracked = tracked_paths()
        tracked_set = set(tracked)
        tracked_oversized = tracked_size_violations(tracked)
        ok = report_surface("tracked source", tracked)
        if tracked_oversized:
            print(
                "tracked source: rejected "
                f"{tracked_oversized} oversized publication item(s)"
            )
            print(f"category=oversized-tracked-file; count={tracked_oversized}")
            ok = False

        ok = docker_context_policy(tracked_set) and ok
        ok = license_copy_policy() and ok
        if args.tracked_only:
            return 0 if ok else 1
        ok = vendored_notice_policy(tracked_set) and ok

        for package in PACKAGES:
            ok = report_surface(
                f"Cargo package {package}",
                package_paths(package),
                package=package,
                tracked=tracked_set,
            ) and ok
        return 0 if ok else 1
    except (OSError, subprocess.SubprocessError, PublicationCheckError):
        print("publication policy check failed before producing a safe summary")
        return 1


if __name__ == "__main__":
    sys.exit(main())
