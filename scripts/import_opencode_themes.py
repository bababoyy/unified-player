#!/usr/bin/env python3
"""Generate the bundled themes from OpenCode's TUI theme files.

OpenCode (MIT, see the notice next to the output) describes each theme with
semantic colors (`background`, `text`, `primary`, ...) and dark/light
variants. This script resolves those colors for a pinned OpenCode commit and
projects them onto Unified Player's palette and component-style roles.

Usage:
    scripts/import_opencode_themes.py            # fetch the pinned commit
    scripts/import_opencode_themes.py --source-dir <folder with *.json>
"""

from __future__ import annotations

import argparse
import colorsys
import json
import sys
import urllib.request
from pathlib import Path

COMMIT = "108b988a08227df45417f27905a4d6b27ad49b6d"
SOURCE = "https://github.com/anomalyco/opencode"
RAW = "https://raw.githubusercontent.com/anomalyco/opencode/{commit}/packages/tui/src/theme/assets/{name}.json"
OUTPUT = Path(__file__).resolve().parent.parent / "unified-player/src/config/themes/opencode.toml"

# Product brand themes (opencode, orng, cursor, vercel, github) and the
# transparent lucent-orng are deliberately not imported.
THEMES = (
    "aura",
    "ayu",
    "carbonfox",
    "catppuccin",
    "catppuccin-frappe",
    "catppuccin-macchiato",
    "cobalt2",
    "dracula",
    "everforest",
    "flexoki",
    "gruvbox",
    "kanagawa",
    "material",
    "matrix",
    "mercury",
    "monokai",
    "nightowl",
    "nord",
    "one-dark",
    "osaka-jade",
    "palenight",
    "rosepine",
    "solarized",
    "synthwave84",
    "tokyonight",
    "vesper",
    "zenburn",
)

# Share of `text` mixed into `background` for the subtle selection/hover row.
SUBTLE_MIX = 0.12
# WCAG AA contrast for normal text; selection labels fall back to black or
# white when neither theme color reaches it on the selection color, and text
# colors are shifted in lightness until they reach it on every surface.
MIN_CONTRAST = 4.5
# Body text aims for WCAG AAA so it stays distinct from muted text, which may
# itself be shifted up to `MIN_CONTRAST`; low-contrast themes such as zenburn
# take the highest target their surfaces allow.
TEXT_CONTRASTS = (7.0, 6.0, 5.5, MIN_CONTRAST)
# Lightness step used when shifting a color towards legibility.
LIGHTNESS_STEP = 0.005

RGB = tuple[int, int, int]


class ThemeError(Exception):
    pass


def parse_hex(value: str) -> tuple[RGB, float]:
    digits = value[1:]
    if len(digits) in (3, 4):
        digits = "".join(c * 2 for c in digits)
    if len(digits) not in (6, 8):
        raise ThemeError(f"unsupported color {value!r}")
    rgb = tuple(int(digits[i : i + 2], 16) for i in (0, 2, 4))
    alpha = int(digits[6:8], 16) / 255 if len(digits) == 8 else 1.0
    return rgb, alpha  # type: ignore[return-value]


def mix(top: RGB, bottom: RGB, alpha: float) -> RGB:
    return tuple(round(t * alpha + b * (1 - alpha)) for t, b in zip(top, bottom))  # type: ignore[return-value]


class Resolver:
    def __init__(self, theme: dict, variant: str) -> None:
        self.defs = theme.get("defs", {})
        self.colors = theme["theme"]
        self.variant = variant
        self.background: RGB | None = None
        self.background = self.color("background")

    def raw(self, value, depth: int = 0) -> str:
        if depth > 8:
            raise ThemeError("color reference cycle")
        if isinstance(value, dict):
            value = value[self.variant]
        if not isinstance(value, str):
            raise ThemeError(f"unsupported value {value!r}")
        if value.startswith("#") or value in ("transparent", "none"):
            return value
        if value in self.defs:
            return self.raw(self.defs[value], depth + 1)
        if value in self.colors:
            return self.raw(self.colors[value], depth + 1)
        raise ThemeError(f"unknown color reference {value!r}")

    def color(self, key: str) -> RGB:
        value = self.raw(self.colors[key])
        if not value.startswith("#"):
            raise ThemeError(f"{key} is {value!r}; transparent themes are not imported")
        rgb, alpha = parse_hex(value)
        if alpha < 1.0:
            if self.background is None:
                raise ThemeError("background must be opaque")
            rgb = mix(rgb, self.background, alpha)
        return rgb

    def optional(self, key: str) -> RGB | None:
        return self.color(key) if key in self.colors else None


def luminance(rgb: RGB) -> float:
    def channel(value: int) -> float:
        c = value / 255
        return c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4

    r, g, b = (channel(v) for v in rgb)
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def contrast(a: RGB, b: RGB) -> float:
    high, low = sorted((luminance(a), luminance(b)), reverse=True)
    return (high + 0.05) / (low + 0.05)


def readable_on(surface: RGB, *candidates: RGB) -> RGB:
    """The most legible theme color on `surface`, or black/white if none is."""
    best = max(candidates, key=lambda color: contrast(color, surface))
    if contrast(best, surface) >= MIN_CONTRAST:
        return best
    return max(((0, 0, 0), (255, 255, 255)), key=lambda color: contrast(color, surface))


def legible(color: RGB, surfaces: list[RGB], minimum: float = MIN_CONTRAST) -> RGB:
    """`color` with only its lightness changed until it reaches `minimum`
    contrast on every surface; hue and saturation are kept."""

    def worst(candidate: RGB) -> float:
        return min(contrast(candidate, surface) for surface in surfaces)

    if worst(color) >= minimum:
        return color
    # Move away from the surfaces: darker on light themes, lighter on dark ones.
    darken = contrast((0, 0, 0), max(surfaces, key=luminance)) >= contrast(
        (255, 255, 255), min(surfaces, key=luminance)
    )
    h, lightness, s = colorsys.rgb_to_hls(*(v / 255 for v in color))
    step = -LIGHTNESS_STEP if darken else LIGHTNESS_STEP
    candidate = color
    while 0.0 <= lightness <= 1.0:
        lightness += step
        rgb = colorsys.hls_to_rgb(h, min(max(lightness, 0.0), 1.0), s)
        candidate = tuple(round(v * 255) for v in rgb)  # type: ignore[assignment]
        if worst(candidate) >= minimum:
            return candidate
    raise ThemeError(f"{hex_of(color)} cannot reach {minimum}:1 on {[hex_of(s) for s in surfaces]}")


def body_text(text: RGB, background: RGB, surfaces: list[RGB]) -> tuple[RGB, RGB]:
    """Legible body text and the subtle row mixed from it."""
    for minimum in TEXT_CONTRASTS:
        candidate, subtle = text, mix(text, background, SUBTLE_MIX)
        try:
            # The subtle row follows `text`, so settle both together.
            while legible(candidate, [*surfaces, subtle], minimum) != candidate:
                candidate = legible(candidate, [*surfaces, subtle], minimum)
                subtle = mix(candidate, background, SUBTLE_MIX)
        except ThemeError:
            continue
        return candidate, subtle
    raise ThemeError(f"text {hex_of(text)} cannot reach {MIN_CONTRAST}:1")


def hex_of(rgb: RGB) -> str:
    return "#{:02x}{:02x}{:02x}".format(*rgb)


def has_light_variant(theme: dict) -> bool:
    dark, light = Resolver(theme, "dark"), Resolver(theme, "light")
    return any(
        isinstance(value, dict) and dark.raw(value) != light.raw(value)
        for value in theme["theme"].values()
    )


def project(theme: dict, variant: str) -> tuple[dict[str, str], dict[str, dict]]:
    r = Resolver(theme, variant)
    background = r.color("background")
    panel = r.color("backgroundPanel")
    element = r.color("backgroundElement")
    surfaces = [background, panel, element]
    # Every role drawing these colors as text sits on one of the surfaces.
    text, subtle = body_text(r.color("text"), background, surfaces)
    muted, primary, error, warning, success, info = (
        legible(r.color(key), surfaces)
        for key in ("textMuted", "primary", "error", "warning", "success", "info")
    )
    accent = r.color("accent")
    border = r.color("border")
    preferred = r.optional("selectedListItemText")
    on_primary = readable_on(primary, *([preferred] if preferred else []), background, text)
    on_accent = readable_on(accent, background, text)

    palette = {
        "background": background,
        "foreground": text,
        "black": element,
        "red": error,
        "green": success,
        "yellow": warning,
        "blue": primary,
        "magenta": r.color("secondary"),
        "cyan": info,
        "white": text,
        "bright_black": muted,
        "bright_red": error,
        "bright_green": success,
        "bright_yellow": warning,
        "bright_blue": primary,
        "bright_magenta": accent,
        "bright_cyan": info,
        "bright_white": text,
    }

    def style(fg: RGB | None = None, bg: RGB | None = None, bold: bool = False) -> dict:
        entry: dict = {}
        if fg is not None:
            entry["fg"] = fg
        if bg is not None:
            entry["bg"] = bg
        if bold:
            entry["modifiers"] = ["Bold"]
        return entry

    roles = {
        "base": style(text, background),
        "panel": style(bg=panel),
        "elevated_surface": style(bg=element),
        "playback_surface": style(bg=panel),
        "secondary_text": style(muted),
        "navigation_active": style(text, subtle, bold=True),
        "selection_inactive": style(text, subtle),
        "selection": style(on_primary, primary, bold=True),
        "secondary_row": style(bg=subtle),
        "focus_indicator": style(primary, bold=True),
        "selected_indicator": style(on_accent, accent, bold=True),
        "multiselect": style(text, bold=True),
        "hint_key": style(text, bold=True),
        "hint_text": style(muted),
        "disabled": style(muted),
        "status_warning": style(warning, bold=True),
        "status_error": style(error, bold=True),
        "scrollbar_track": style(border),
        "scrollbar_thumb": style(muted),
        "block_title": style(text, bold=True),
        "border": style(border),
        "playback_status": style(primary, bold=True),
        "playback_track": style(text, bold=True),
        "playback_artists": style(muted),
        "playback_album": style(muted),
        "playback_genres": style(muted),
        "playback_metadata": style(muted),
        "playback_progress_bar": style(primary, panel),
        "playback_progress_bar_unfilled": style(border, panel),
        "playback_progress_remaining": style(border, panel),
        "current_playing": style(primary, bold=True),
        "page_desc": style(muted),
        "playlist_desc": style(muted),
        "table_header": style(muted),
        "like": style(accent),
        "lyrics_played": style(muted),
        "lyrics_playing": style(primary, bold=True),
        "sync_clean": style(success),
        "sync_changed": style(warning),
        "sync_conflict": style(error),
        "sync_neutral": style(muted),
    }
    return {k: hex_of(v) for k, v in palette.items()}, roles


def render(themes: list[tuple[str, dict[str, str], dict[str, dict]]]) -> str:
    lines = [
        "# Generated by scripts/import_opencode_themes.py; do not edit by hand.",
        f"# Source: {SOURCE} at {COMMIT}",
        "# (packages/tui/src/theme/assets). See OPENCODE_NOTICE.md for the license.",
    ]
    for name, palette, roles in themes:
        lines += ["", "[[themes]]", f'name = "{name}"', "", "[themes.palette]"]
        lines += [f'{key} = "{value}"' for key, value in palette.items()]
        lines += ["", "[themes.component_style]"]
        for role, entry in roles.items():
            parts = []
            if "fg" in entry:
                parts.append(f'fg = "{hex_of(entry["fg"])}"')
            if "bg" in entry:
                parts.append(f'bg = "{hex_of(entry["bg"])}"')
            if "modifiers" in entry:
                parts.append("modifiers = [" + ", ".join(f'"{m}"' for m in entry["modifiers"]) + "]")
            lines.append(f"{role} = {{ {', '.join(parts)} }}")
    return "\n".join(lines) + "\n"


def load(name: str, source_dir: Path | None) -> dict:
    if source_dir is not None:
        return json.loads((source_dir / f"{name}.json").read_text(encoding="utf-8"))
    with urllib.request.urlopen(RAW.format(commit=COMMIT, name=name), timeout=30) as response:
        return json.loads(response.read().decode("utf-8"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--source-dir", type=Path, help="read <theme>.json files from here")
    args = parser.parse_args()

    themes = []
    for name in THEMES:
        theme = load(name, args.source_dir)
        try:
            palette, roles = project(theme, "dark")
            themes.append((name, palette, roles))
            if has_light_variant(theme):
                palette, roles = project(theme, "light")
                themes.append((f"{name}-light", palette, roles))
        except (ThemeError, KeyError) as error:
            print(f"{name}: {error}", file=sys.stderr)
            return 1
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text(render(themes), encoding="utf-8")
    print(f"wrote {len(themes)} themes to {OUTPUT.relative_to(Path.cwd()) if OUTPUT.is_relative_to(Path.cwd()) else OUTPUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
