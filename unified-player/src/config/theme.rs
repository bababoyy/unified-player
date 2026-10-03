use std::str::FromStr;

use anyhow::Result;
use ratatui::style;
use serde::Deserialize;

/// Themes shipped with the application, generated from `OpenCode` themes by
/// `scripts/import_opencode_themes.py`.
const BUNDLED_THEMES: &str = include_str!("themes/opencode.toml");

#[derive(Clone, Debug, Deserialize)]
/// Application theme configurations.
pub struct ThemeConfig {
    #[serde(default)]
    pub themes: Vec<Theme>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Theme {
    /// Per-frame projection only; the UI session owns the preference.
    #[serde(skip)]
    runtime_border_type: Option<super::BorderType>,
    pub name: String,
    #[serde(default)]
    palette: Palette,
    #[serde(default)]
    component_style: ComponentStyle,
}

#[derive(Clone, Debug, Deserialize)]
struct Palette {
    background: Option<Color>,
    foreground: Option<Color>,

    #[serde(default = "Color::black")]
    black: Color,
    #[serde(default = "Color::blue")]
    blue: Color,
    #[serde(default = "Color::cyan")]
    cyan: Color,
    #[serde(default = "Color::green")]
    green: Color,
    #[serde(default = "Color::magenta")]
    magenta: Color,
    #[serde(default = "Color::red")]
    red: Color,
    #[serde(default = "Color::white")]
    white: Color,
    #[serde(default = "Color::yellow")]
    yellow: Color,

    #[serde(default = "Color::bright_black")]
    bright_black: Color,
    #[serde(default = "Color::bright_white")]
    bright_white: Color,
    #[serde(default = "Color::bright_red")]
    bright_red: Color,
    #[serde(default = "Color::bright_magenta")]
    bright_magenta: Color,
    #[serde(default = "Color::bright_green")]
    bright_green: Color,
    #[serde(default = "Color::bright_cyan")]
    bright_cyan: Color,
    #[serde(default = "Color::bright_blue")]
    bright_blue: Color,
    #[serde(default = "Color::bright_yellow")]
    bright_yellow: Color,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ComponentStyle {
    base: Option<Style>,
    panel: Option<Style>,
    elevated_surface: Option<Style>,
    playback_surface: Option<Style>,
    secondary_text: Option<Style>,
    navigation_active: Option<Style>,
    selection_inactive: Option<Style>,
    focus_indicator: Option<Style>,
    selected_indicator: Option<Style>,
    multiselect: Option<Style>,
    hint_key: Option<Style>,
    hint_text: Option<Style>,
    disabled: Option<Style>,
    status_warning: Option<Style>,
    status_error: Option<Style>,
    scrollbar_track: Option<Style>,
    scrollbar_thumb: Option<Style>,
    playback_progress_remaining: Option<Style>,
    block_title: Option<Style>,
    border: Option<Style>,
    playback_status: Option<Style>,
    playback_track: Option<Style>,
    playback_artists: Option<Style>,
    playback_album: Option<Style>,
    playback_genres: Option<Style>,
    playback_metadata: Option<Style>,
    playback_progress_bar: Option<Style>,
    playback_progress_bar_unfilled: Option<Style>,
    current_playing: Option<Style>,
    page_desc: Option<Style>,
    playlist_desc: Option<Style>,
    table_header: Option<Style>,
    selection: Option<Style>,
    secondary_row: Option<Style>,
    like: Option<Style>,
    lyrics_played: Option<Style>,
    lyrics_playing: Option<Style>,
    sync_clean: Option<Style>,
    sync_changed: Option<Style>,
    sync_conflict: Option<Style>,
    sync_neutral: Option<Style>,
}

#[derive(Default, Clone, Debug, Deserialize)]
struct Style {
    fg: Option<StyleColor>,
    bg: Option<StyleColor>,
    #[serde(default)]
    modifiers: Vec<StyleModifier>,
}

#[derive(Copy, Clone, Debug)]
enum StyleColor {
    Black,
    Blue,
    Cyan,
    Green,
    Magenta,
    Red,
    White,
    Yellow,
    BrightBlack,
    BrightWhite,
    BrightRed,
    BrightMagenta,
    BrightGreen,
    BrightCyan,
    BrightBlue,
    BrightYellow,
    Rgb { r: u8, g: u8, b: u8 },
}

#[derive(Copy, Clone, Debug, Deserialize)]
enum StyleModifier {
    Bold,
    Dim,
    Italic,
    Underlined,
    RapidBlink,
    Reversed,
    Hidden,
    CrossedOut,
}

#[derive(Clone, Debug)]
struct Color {
    color: style::Color,
}

impl ThemeConfig {
    /// finds a theme whose name matches a given `name`
    pub fn find_theme(&self, name: &str) -> Option<Theme> {
        self.themes.iter().find(|&t| t.name == name).cloned()
    }

    pub fn new(path: &std::path::Path) -> Result<Self> {
        let mut config = Self::default();
        config.parse_config_file(path)?;

        Ok(config)
    }

    /// parses configurations from a theme config file in `path` folder,
    /// then updates the current configurations accordingly.
    fn parse_config_file(&mut self, path: &std::path::Path) -> Result<()> {
        let file_path = path.join(super::THEME_CONFIG_FILE);
        match std::fs::read_to_string(&file_path) {
            Err(err) => {
                tracing::warn!(
                    diagnostic = %crate::observability::safe_error(
                        crate::observability::DiagnosticCode::THEME_OPEN_FAILED,
                        crate::observability::ErrorCategory::Storage,
                        &err,
                    ),
                    "Failed to open the theme configuration; using defaults"
                );
            }
            Ok(content) => {
                let config = toml::from_str::<Self>(&content)?;

                // A user theme replaces the application theme of the same name.
                for theme in config.themes {
                    match self.themes.iter_mut().find(|t| t.name == theme.name) {
                        Some(existing) => *existing = theme,
                        None => self.themes.push(theme),
                    }
                }
            }
        }
        Ok(())
    }
}

impl Theme {
    pub(crate) fn project_border_type(&mut self, border_type: super::BorderType) {
        self.runtime_border_type = Some(border_type);
    }

    pub(crate) fn border_type(&self) -> &super::BorderType {
        self.runtime_border_type
            .as_ref()
            .unwrap_or_else(|| &super::get_config().app_config.border_type)
    }

    pub fn app(&self) -> style::Style {
        let mut style = style::Style::default();
        if let Some(ref c) = self.palette.background {
            style = style.bg(c.color);
        }
        if let Some(ref c) = self.palette.foreground {
            style = style.fg(c.color);
        }
        style
    }

    fn workspace_style(&self, configured: Option<&Style>, fallback: style::Style) -> style::Style {
        configured
            .as_ref()
            .map_or(fallback, |configured| configured.style(&self.palette))
    }

    pub fn workspace_base(&self) -> style::Style {
        self.workspace_style(self.component_style.base.as_ref(), self.app())
    }

    pub fn workspace_panel(&self) -> style::Style {
        self.workspace_style(self.component_style.panel.as_ref(), self.app())
    }

    /// Raised surfaces (popups, cards, side panels). A theme that only gives
    /// them a background still gets its base text colour, rather than the
    /// terminal default, which is unreadable on light themes.
    pub fn workspace_elevated_surface(&self) -> style::Style {
        self.workspace_base()
            .patch(self.workspace_style(self.component_style.elevated_surface.as_ref(), self.app()))
    }

    pub fn workspace_playback_surface(&self) -> style::Style {
        self.workspace_style(self.component_style.playback_surface.as_ref(), self.app())
    }

    pub fn workspace_secondary_text(&self) -> style::Style {
        self.workspace_style(
            self.component_style.secondary_text.as_ref(),
            self.page_desc(),
        )
    }

    /// Semantic heading role used by workspace panels and page shells.
    pub fn workspace_heading(&self) -> style::Style {
        self.workspace_style(
            self.component_style.block_title.as_ref(),
            self.block_title(),
        )
    }

    /// Semantic divider role used by workspace panel boundaries.
    pub fn workspace_border(&self) -> style::Style {
        self.workspace_style(self.component_style.border.as_ref(), self.border())
    }

    /// Semantic accent used by workspace scope labels for each provider.
    ///
    /// The provider distinction remains part of the theme projection rather
    /// than being chosen by a page renderer, while legacy themes continue to
    /// inherit their palette's ANSI green/red values.
    pub fn workspace_provider_scope(&self, provider: super::ActiveProvider) -> style::Style {
        let color = match provider {
            super::ActiveProvider::Spotify => self.palette.green.color,
            super::ActiveProvider::YouTubeMusic => self.palette.red.color,
        };
        self.page_desc().fg(color)
    }

    pub fn workspace_navigation_active(&self) -> style::Style {
        self.workspace_style(
            self.component_style.navigation_active.as_ref(),
            self.selection(true),
        )
    }

    pub fn workspace_selection_active(&self) -> style::Style {
        self.selection(true)
    }

    /// Style for text of `role` drawn on a `selection` surface. The selection
    /// keeps its own colours, since only that pair is designed to contrast;
    /// the role contributes its emphasis.
    pub fn on_selection(selection: style::Style, role: style::Style) -> style::Style {
        selection.add_modifier(role.add_modifier)
    }

    pub fn workspace_current_playing(&self) -> style::Style {
        self.current_playing()
    }

    pub fn workspace_table_header(&self) -> style::Style {
        self.workspace_hint_key()
    }

    pub fn workspace_selection_inactive(&self) -> style::Style {
        self.workspace_style(
            self.component_style.selection_inactive.as_ref(),
            self.selection(true),
        )
    }

    /// Shared pointer-hover treatment for rows across workspace and legacy
    /// surfaces. The role intentionally uses the same design-v1 token as the
    /// workspace inactive-selection treatment so mouse navigation cannot
    /// change meaning when a route switches shell.
    pub fn hover(&self) -> style::Style {
        self.workspace_selection_inactive()
    }

    pub fn workspace_focus_indicator(&self) -> style::Style {
        self.workspace_style(
            self.component_style.focus_indicator.as_ref(),
            self.current_playing(),
        )
    }

    pub fn workspace_selected_indicator(&self) -> style::Style {
        self.workspace_style(
            self.component_style.selected_indicator.as_ref(),
            self.selection(true),
        )
    }

    pub fn workspace_multiselect(&self) -> style::Style {
        self.workspace_style(
            self.component_style.multiselect.as_ref(),
            self.current_playing(),
        )
    }

    pub fn workspace_hint_key(&self) -> style::Style {
        self.workspace_style(self.component_style.hint_key.as_ref(), self.table_header())
    }

    pub fn workspace_hint_text(&self) -> style::Style {
        self.workspace_style(self.component_style.hint_text.as_ref(), self.page_desc())
    }

    pub fn workspace_disabled(&self) -> style::Style {
        self.workspace_style(self.component_style.disabled.as_ref(), self.playlist_desc())
    }

    pub fn workspace_status_warning(&self) -> style::Style {
        self.workspace_style(
            self.component_style.status_warning.as_ref(),
            self.playback_status(),
        )
    }

    pub fn workspace_status_success(&self) -> style::Style {
        self.sync_clean()
    }

    pub fn workspace_status_info(&self) -> style::Style {
        self.sync_neutral()
    }

    pub fn workspace_status_busy(&self) -> style::Style {
        self.playback_status()
    }

    #[allow(dead_code)]
    pub fn workspace_status_error(&self) -> style::Style {
        self.workspace_style(
            self.component_style.status_error.as_ref(),
            self.sync_conflict(),
        )
    }

    #[allow(dead_code)]
    pub fn workspace_scrollbar_track(&self) -> style::Style {
        self.workspace_style(self.component_style.scrollbar_track.as_ref(), self.border())
    }

    #[allow(dead_code)]
    pub fn workspace_scrollbar_thumb(&self) -> style::Style {
        self.workspace_style(
            self.component_style.scrollbar_thumb.as_ref(),
            self.current_playing(),
        )
    }

    pub fn workspace_progress_remaining(&self) -> style::Style {
        self.workspace_style(
            self.component_style.playback_progress_remaining.as_ref(),
            self.playback_progress_bar_unfilled(),
        )
    }

    pub fn selection(&self, is_active: bool) -> style::Style {
        if is_active {
            self.component_style
                .selection
                .as_ref()
                .unwrap_or(
                    &Style::default().modifiers([StyleModifier::Reversed, StyleModifier::Bold]),
                )
                .style(&self.palette)
        } else {
            style::Style::default()
        }
    }

    pub fn block_title(&self) -> style::Style {
        self.component_style
            .block_title
            .as_ref()
            .unwrap_or(&Style::default().fg(StyleColor::Magenta))
            .style(&self.palette)
    }

    pub fn border(&self) -> style::Style {
        self.component_style
            .border
            .as_ref()
            .unwrap_or(&Style::default())
            .style(&self.palette)
    }

    pub fn playback_status(&self) -> style::Style {
        self.component_style
            .playback_status
            .as_ref()
            .unwrap_or(
                &Style::default()
                    .fg(StyleColor::Cyan)
                    .modifiers([StyleModifier::Bold]),
            )
            .style(&self.palette)
    }

    pub fn playback_track(&self) -> style::Style {
        self.component_style
            .playback_track
            .as_ref()
            .unwrap_or(
                &Style::default()
                    .fg(StyleColor::Cyan)
                    .modifiers([StyleModifier::Bold]),
            )
            .style(&self.palette)
    }

    pub fn playback_artists(&self) -> style::Style {
        self.component_style
            .playback_artists
            .as_ref()
            .unwrap_or(
                &Style::default()
                    .fg(StyleColor::Cyan)
                    .modifiers([StyleModifier::Bold]),
            )
            .style(&self.palette)
    }

    pub fn playback_album(&self) -> style::Style {
        self.component_style
            .playback_album
            .as_ref()
            .unwrap_or(&Style::default().fg(StyleColor::Yellow))
            .style(&self.palette)
    }

    pub fn playback_genres(&self) -> style::Style {
        self.component_style
            .playback_genres
            .as_ref()
            .unwrap_or(
                &Style::default()
                    .fg(StyleColor::BrightBlack)
                    .modifiers([StyleModifier::Italic]),
            )
            .style(&self.palette)
    }

    pub fn playback_metadata(&self) -> style::Style {
        self.component_style
            .playback_metadata
            .as_ref()
            .unwrap_or(&Style::default().fg(StyleColor::BrightBlack))
            .style(&self.palette)
    }

    pub fn playback_progress_bar(&self) -> style::Style {
        self.component_style
            .playback_progress_bar
            .as_ref()
            .unwrap_or(
                &Style::default()
                    .bg(StyleColor::BrightBlack)
                    .fg(StyleColor::Green),
            )
            .style(&self.palette)
    }

    pub fn playback_progress_bar_unfilled(&self) -> style::Style {
        self.component_style
            .playback_progress_bar_unfilled
            .as_ref()
            .unwrap_or(&Style::default().bg(StyleColor::BrightBlack))
            .style(&self.palette)
    }

    pub fn current_playing(&self) -> style::Style {
        self.component_style
            .current_playing
            .as_ref()
            .unwrap_or(
                &Style::default()
                    .fg(StyleColor::Green)
                    .modifiers([StyleModifier::Bold]),
            )
            .style(&self.palette)
    }

    pub fn page_desc(&self) -> style::Style {
        self.component_style
            .page_desc
            .as_ref()
            .unwrap_or(
                &Style::default()
                    .fg(StyleColor::Cyan)
                    .modifiers([StyleModifier::Bold]),
            )
            .style(&self.palette)
    }

    pub fn playlist_desc(&self) -> style::Style {
        self.component_style
            .playlist_desc
            .as_ref()
            .unwrap_or(
                &Style::default()
                    .fg(StyleColor::BrightBlack)
                    .modifiers([StyleModifier::Dim]),
            )
            .style(&self.palette)
    }

    pub fn table_header(&self) -> style::Style {
        self.component_style
            .table_header
            .as_ref()
            .unwrap_or(&Style::default().fg(StyleColor::Blue))
            .style(&self.palette)
    }

    pub fn secondary_row(&self) -> style::Style {
        self.component_style
            .secondary_row
            .as_ref()
            .unwrap_or(&Style::default())
            .style(&self.palette)
    }

    pub fn like(&self) -> style::Style {
        self.component_style
            .like
            .as_ref()
            .unwrap_or(&Style::default())
            .style(&self.palette)
    }

    pub fn lyrics_played(&self) -> style::Style {
        self.component_style
            .lyrics_played
            .as_ref()
            .unwrap_or(&Style::default().modifiers([StyleModifier::Dim]))
            .style(&self.palette)
    }

    pub fn lyrics_playing(&self) -> style::Style {
        self.component_style
            .lyrics_playing
            .as_ref()
            .unwrap_or(
                &Style::default()
                    .fg(StyleColor::Green)
                    .modifiers([StyleModifier::Bold]),
            )
            .style(&self.palette)
    }

    pub fn sync_clean(&self) -> style::Style {
        self.component_style
            .sync_clean
            .as_ref()
            .unwrap_or(
                &Style::default()
                    .fg(StyleColor::Green)
                    .modifiers([StyleModifier::Bold]),
            )
            .style(&self.palette)
    }

    pub fn sync_changed(&self) -> style::Style {
        self.component_style
            .sync_changed
            .as_ref()
            .unwrap_or(&Style::default().fg(StyleColor::Yellow))
            .style(&self.palette)
    }

    pub fn sync_conflict(&self) -> style::Style {
        self.component_style
            .sync_conflict
            .as_ref()
            .unwrap_or(
                &Style::default()
                    .fg(StyleColor::Red)
                    .modifiers([StyleModifier::Bold]),
            )
            .style(&self.palette)
    }

    pub fn sync_neutral(&self) -> style::Style {
        self.component_style
            .sync_neutral
            .as_ref()
            .unwrap_or(
                &Style::default()
                    .fg(StyleColor::BrightBlack)
                    .modifiers([StyleModifier::Italic]),
            )
            .style(&self.palette)
    }
}

impl Style {
    fn style(&self, palette: &Palette) -> style::Style {
        let mut style = style::Style::default();
        if let Some(fg) = self.fg {
            style = style.fg(fg.color(palette));
        }
        if let Some(bg) = self.bg {
            style = style.bg(bg.color(palette));
        }
        self.modifiers.iter().for_each(|&m| {
            style = style.add_modifier(m.into());
        });
        style
    }

    fn fg(mut self, fg: StyleColor) -> Self {
        self.fg = Some(fg);
        self
    }

    fn bg(mut self, bg: StyleColor) -> Self {
        self.bg = Some(bg);
        self
    }

    fn modifiers<M>(mut self, modifiers: M) -> Self
    where
        M: IntoIterator<Item = StyleModifier>,
    {
        self.modifiers = modifiers.into_iter().collect();
        self
    }
}

impl<'de> serde::de::Deserialize<'de> for StyleColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        fn rgb_from_hex(s: &str) -> Option<(u8, u8, u8)> {
            if !s.starts_with('#') || s.len() != 7 {
                None
            } else {
                Some((
                    u8::from_str_radix(&s[1..3], 16).ok()?,
                    u8::from_str_radix(&s[3..5], 16).ok()?,
                    u8::from_str_radix(&s[5..7], 16).ok()?,
                ))
            }
        }

        let str = String::deserialize(deserializer)?;
        Ok(match str.as_str() {
            "Black" => StyleColor::Black,
            "Blue" => StyleColor::Blue,
            "Cyan" => StyleColor::Cyan,
            "Green" => StyleColor::Green,
            "Magenta" => StyleColor::Magenta,
            "Red" => StyleColor::Red,
            "White" => StyleColor::White,
            "Yellow" => StyleColor::Yellow,
            "BrightBlack" => StyleColor::BrightBlack,
            "BrightWhite" => StyleColor::BrightWhite,
            "BrightRed" => StyleColor::BrightRed,
            "BrightMagenta" => StyleColor::BrightMagenta,
            "BrightGreen" => StyleColor::BrightGreen,
            "BrightCyan" => StyleColor::BrightCyan,
            "BrightBlue" => StyleColor::BrightBlue,
            "BrightYellow" => StyleColor::BrightYellow,
            s => match rgb_from_hex(s) {
                Some((r, g, b)) => StyleColor::Rgb { r, g, b },
                None => return Err(serde::de::Error::custom(format!("invalid hex color: {s}"))),
            },
        })
    }
}

impl StyleColor {
    fn color(self, palette: &Palette) -> style::Color {
        match self {
            Self::Black => palette.black.color,
            Self::Blue => palette.blue.color,
            Self::Cyan => palette.cyan.color,
            Self::Green => palette.green.color,
            Self::Magenta => palette.magenta.color,
            Self::Red => palette.red.color,
            Self::White => palette.white.color,
            Self::Yellow => palette.yellow.color,
            Self::BrightBlack => palette.bright_black.color,
            Self::BrightWhite => palette.bright_white.color,
            Self::BrightRed => palette.bright_red.color,
            Self::BrightMagenta => palette.bright_magenta.color,
            Self::BrightGreen => palette.bright_green.color,
            Self::BrightCyan => palette.bright_cyan.color,
            Self::BrightBlue => palette.bright_blue.color,
            Self::BrightYellow => palette.bright_yellow.color,
            Self::Rgb { r, g, b } => style::Color::Rgb(r, g, b),
        }
    }
}

impl From<StyleModifier> for style::Modifier {
    fn from(m: StyleModifier) -> Self {
        match m {
            StyleModifier::Bold => style::Modifier::BOLD,
            StyleModifier::Italic => style::Modifier::ITALIC,
            StyleModifier::Dim => style::Modifier::DIM,
            StyleModifier::Reversed => style::Modifier::REVERSED,
            StyleModifier::Underlined => style::Modifier::UNDERLINED,
            StyleModifier::RapidBlink => style::Modifier::RAPID_BLINK,
            StyleModifier::Hidden => style::Modifier::HIDDEN,
            StyleModifier::CrossedOut => style::Modifier::CROSSED_OUT,
        }
    }
}

impl<'de> serde::de::Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let str = String::deserialize(deserializer)?;
        match style::Color::from_str(&str) {
            Err(err) => Err(serde::de::Error::custom(format!(
                "invalid color {str}: {err:#}"
            ))),
            Ok(color) => Ok(Color { color }),
        }
    }
}

impl Color {
    fn black() -> Self {
        style::Color::Black.into()
    }
    fn red() -> Self {
        style::Color::Red.into()
    }
    fn green() -> Self {
        style::Color::Green.into()
    }
    fn yellow() -> Self {
        style::Color::Yellow.into()
    }
    fn blue() -> Self {
        style::Color::Blue.into()
    }
    fn magenta() -> Self {
        style::Color::Magenta.into()
    }
    fn cyan() -> Self {
        style::Color::Cyan.into()
    }
    fn white() -> Self {
        style::Color::Gray.into()
    }
    fn bright_black() -> Self {
        style::Color::DarkGray.into()
    }
    fn bright_red() -> Self {
        style::Color::LightRed.into()
    }
    fn bright_green() -> Self {
        style::Color::LightGreen.into()
    }
    fn bright_yellow() -> Self {
        style::Color::LightYellow.into()
    }
    fn bright_blue() -> Self {
        style::Color::LightBlue.into()
    }
    fn bright_magenta() -> Self {
        style::Color::LightMagenta.into()
    }
    fn bright_cyan() -> Self {
        style::Color::LightCyan.into()
    }
    fn bright_white() -> Self {
        style::Color::White.into()
    }
}

impl From<&str> for Color {
    fn from(s: &str) -> Self {
        Color {
            color: style::Color::from_str(s).expect("valid color"),
        }
    }
}

impl From<style::Color> for Color {
    fn from(value: style::Color) -> Self {
        Self { color: value }
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        let bundled = toml::from_str::<Self>(BUNDLED_THEMES).expect("bundled themes are valid");
        let mut themes = vec![Theme::default()];
        themes.extend(bundled.themes);
        Self { themes }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            name: "default".to_owned(),
            runtime_border_type: None,
            palette: Palette::default(),
            component_style: ComponentStyle::design_v1(),
        }
    }
}

impl ComponentStyle {
    fn design_v1() -> Self {
        Self {
            base: Some(design_style(Some((230, 230, 232)), Some((11, 11, 12)), &[])),
            panel: Some(design_style(None, Some((19, 19, 20)), &[])),
            elevated_surface: Some(design_style(None, Some((26, 26, 28)), &[])),
            playback_surface: Some(design_style(None, Some((25, 25, 27)), &[])),
            secondary_text: Some(design_style(Some((160, 160, 170)), None, &[])),
            navigation_active: Some(design_style(
                Some((230, 230, 232)),
                Some((34, 34, 36)),
                &[StyleModifier::Bold],
            )),
            selection_inactive: Some(design_style(Some((230, 230, 232)), Some((34, 34, 36)), &[])),
            focus_indicator: Some(design_style(
                Some((105, 180, 255)),
                None,
                &[StyleModifier::Bold],
            )),
            selected_indicator: Some(design_style(
                Some((19, 19, 20)),
                Some((245, 177, 131)),
                &[StyleModifier::Bold],
            )),
            multiselect: Some(design_style(
                Some((230, 230, 232)),
                None,
                &[StyleModifier::Bold],
            )),
            hint_key: Some(design_style(
                Some((230, 230, 232)),
                None,
                &[StyleModifier::Bold],
            )),
            hint_text: Some(design_style(Some((160, 160, 170)), None, &[])),
            disabled: Some(design_style(Some((133, 133, 143)), None, &[])),
            status_warning: Some(design_style(
                Some((231, 191, 122)),
                None,
                &[StyleModifier::Bold],
            )),
            status_error: Some(design_style(
                Some((242, 139, 145)),
                None,
                &[StyleModifier::Bold],
            )),
            scrollbar_track: Some(design_style(Some((48, 48, 52)), None, &[])),
            scrollbar_thumb: Some(design_style(Some((116, 116, 126)), None, &[])),
            playback_progress_remaining: Some(design_style(
                Some((91, 91, 99)),
                Some((25, 25, 27)),
                &[],
            )),
            block_title: Some(design_style(
                Some((230, 230, 232)),
                None,
                &[StyleModifier::Bold],
            )),
            border: Some(design_style(Some((48, 48, 52)), None, &[])),
            playback_status: Some(design_style(
                Some((105, 180, 255)),
                None,
                &[StyleModifier::Bold],
            )),
            playback_track: Some(design_style(
                Some((230, 230, 232)),
                None,
                &[StyleModifier::Bold],
            )),
            playback_artists: Some(design_style(Some((160, 160, 170)), None, &[])),
            playback_album: Some(design_style(Some((160, 160, 170)), None, &[])),
            playback_genres: Some(design_style(Some((160, 160, 170)), None, &[])),
            playback_metadata: Some(design_style(Some((160, 160, 170)), None, &[])),
            playback_progress_bar: Some(design_style(
                Some((105, 180, 255)),
                Some((25, 25, 27)),
                &[],
            )),
            playback_progress_bar_unfilled: Some(design_style(
                Some((91, 91, 99)),
                Some((25, 25, 27)),
                &[],
            )),
            current_playing: Some(design_style(
                Some((105, 180, 255)),
                None,
                &[StyleModifier::Bold],
            )),
            page_desc: Some(design_style(Some((160, 160, 170)), None, &[])),
            playlist_desc: Some(design_style(Some((160, 160, 170)), None, &[])),
            table_header: Some(design_style(Some((160, 160, 170)), None, &[])),
            selection: Some(design_style(
                Some((19, 19, 20)),
                Some((245, 177, 131)),
                &[StyleModifier::Bold],
            )),
            secondary_row: Some(design_style(None, Some((34, 34, 36)), &[])),
            ..Self::default()
        }
    }
}

fn design_style(
    fg: Option<(u8, u8, u8)>,
    bg: Option<(u8, u8, u8)>,
    modifiers: &[StyleModifier],
) -> Style {
    Style {
        fg: fg.map(|(r, g, b)| StyleColor::Rgb { r, g, b }),
        bg: bg.map(|(r, g, b)| StyleColor::Rgb { r, g, b }),
        modifiers: modifiers.to_vec(),
    }
}

#[cfg(test)]
mod sync_style_tests {
    use super::Theme;
    use ratatui::style::Color;

    #[test]
    fn sync_state_styles_have_distinct_version_control_defaults() {
        let theme = Theme::default();
        let clean = theme.sync_clean();
        let changed = theme.sync_changed();
        let conflict = theme.sync_conflict();
        let neutral = theme.sync_neutral();
        assert_ne!(clean, changed);
        assert_ne!(clean, conflict);
        assert_ne!(clean, neutral);
        assert_ne!(changed, conflict);
        assert_eq!(clean.fg, Some(Color::Green));
        assert_eq!(changed.fg, Some(Color::Yellow));
        assert_eq!(conflict.fg, Some(Color::Red));
        assert_eq!(neutral.fg, Some(Color::DarkGray));
    }

    #[test]
    fn default_theme_exposes_the_design_v1_rgb_roles() {
        let theme = Theme::default();

        assert_eq!(theme.workspace_base().fg, Some(Color::Rgb(230, 230, 232)));
        assert_eq!(theme.workspace_base().bg, Some(Color::Rgb(11, 11, 12)));
        assert_eq!(theme.workspace_panel().bg, Some(Color::Rgb(19, 19, 20)));
        assert_eq!(
            theme.workspace_playback_surface().bg,
            Some(Color::Rgb(25, 25, 27))
        );
        assert_eq!(
            theme.workspace_selection_inactive().bg,
            Some(Color::Rgb(34, 34, 36))
        );
        assert_eq!(theme.selection(true).bg, Some(Color::Rgb(245, 177, 131)));
        assert_eq!(
            theme.workspace_focus_indicator().fg,
            Some(Color::Rgb(105, 180, 255))
        );
        assert_eq!(
            theme.workspace_progress_remaining().fg,
            Some(Color::Rgb(91, 91, 99))
        );
        assert_eq!(
            theme
                .workspace_provider_scope(super::super::ActiveProvider::Spotify)
                .fg,
            Some(Color::Green)
        );
        assert_eq!(
            theme
                .workspace_provider_scope(super::super::ActiveProvider::YouTubeMusic)
                .fg,
            Some(Color::Red)
        );
        assert_eq!(theme.workspace_table_header(), theme.workspace_hint_key());
        assert_eq!(theme.workspace_heading(), theme.block_title());
        assert_eq!(theme.workspace_border(), theme.border());
        assert_eq!(theme.workspace_selection_active(), theme.selection(true));
        assert_eq!(theme.workspace_current_playing(), theme.current_playing());
    }

    #[test]
    fn missing_workspace_roles_fall_back_to_an_existing_theme_surface() {
        let config: super::ThemeConfig = toml::from_str(
            r##"
                [[themes]]
                name = "legacy"
                [themes.palette]
                background = "#010203"
                foreground = "#040506"
            "##,
        )
        .unwrap();
        let theme = config.themes.into_iter().next().unwrap();

        assert_eq!(theme.workspace_base(), theme.app());
        assert_eq!(theme.workspace_panel(), theme.app());
        assert_eq!(theme.workspace_secondary_text(), theme.page_desc());
    }

    #[test]
    fn raised_surfaces_carry_a_text_colour_in_every_bundled_theme() {
        // Popups and cards draw text on these surfaces without their own
        // colour; the terminal default is unreadable on light themes.
        for theme in super::ThemeConfig::default().themes {
            let base = theme.workspace_base().fg;
            if base.is_some() {
                assert_eq!(
                    theme.workspace_elevated_surface().fg,
                    base,
                    "{}",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn bundled_themes_have_unique_names_and_complete_surfaces() {
        let config = super::ThemeConfig::default();
        assert_eq!(config.themes[0].name, "default");
        assert!(config.themes.len() > 40, "bundled themes are loaded");
        let mut names = std::collections::HashSet::new();
        for theme in &config.themes {
            assert!(names.insert(theme.name.clone()), "duplicate {}", theme.name);
            assert!(theme.workspace_base().fg.is_some(), "{} text", theme.name);
            assert!(
                theme.workspace_base().bg.is_some(),
                "{} background",
                theme.name
            );
            let selection = theme.selection(true);
            assert!(
                selection.fg.is_some() && selection.bg.is_some(),
                "{}",
                theme.name
            );
            assert_ne!(selection.fg, selection.bg, "{} selection", theme.name);
        }
        assert!(config.find_theme("gruvbox").is_some());
        assert!(config.find_theme("gruvbox-light").is_some());
    }

    #[test]
    fn user_theme_replaces_the_bundled_theme_of_the_same_name() {
        let folder = std::env::temp_dir().join(format!(
            "unified-player-theme-override-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(super::super::THEME_CONFIG_FILE),
            "[[themes]]\nname = \"gruvbox\"\n[themes.palette]\nbackground = \"#010203\"\n",
        )
        .unwrap();

        let config = super::ThemeConfig::new(&folder).unwrap();
        std::fs::remove_dir_all(&folder).unwrap();

        let gruvbox = config.find_theme("gruvbox").unwrap();
        assert_eq!(gruvbox.app().bg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(
            config.themes.iter().filter(|t| t.name == "gruvbox").count(),
            1
        );
    }

    #[test]
    fn unified_player_example_theme_uses_the_existing_theme_parser() {
        let config: super::ThemeConfig =
            toml::from_str(include_str!("../../../examples/theme.toml")).unwrap();
        let theme = config
            .themes
            .into_iter()
            .find(|theme| theme.name == "unified-player")
            .expect("the design-v1 example theme is present");

        assert_eq!(theme.workspace_base().bg, Some(Color::Rgb(11, 11, 12)));
        assert_eq!(
            theme.workspace_elevated_surface().bg,
            Some(Color::Rgb(26, 26, 28))
        );
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            background: None,
            foreground: None,
            // the default theme uses the terminal's ANSI colors
            black: Color::black(),
            red: Color::red(),
            green: Color::green(),
            yellow: Color::yellow(),
            blue: Color::blue(),
            magenta: Color::magenta(),
            cyan: Color::cyan(),
            white: Color::white(),
            bright_black: Color::bright_black(),
            bright_red: Color::bright_red(),
            bright_green: Color::bright_green(),
            bright_yellow: Color::bright_yellow(),
            bright_blue: Color::bright_blue(),
            bright_magenta: Color::bright_magenta(),
            bright_cyan: Color::bright_cyan(),
            bright_white: Color::bright_white(),
        }
    }
}

#[cfg(test)]
mod contrast_tests {
    use super::{Theme, ThemeConfig};
    use ratatui::style::{Color, Modifier, Style};

    /// WCAG 2.x AA minimum for text (1.4.3).
    const TEXT: f64 = 4.5;
    /// WCAG 2.x AA minimum for graphical objects (1.4.11).
    const GRAPHIC: f64 = 3.0;

    fn relative_luminance(color: Color) -> Option<f64> {
        let Color::Rgb(r, g, b) = color else {
            // Named ANSI colours depend on the terminal palette.
            return None;
        };
        let channel = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        Some(0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b))
    }

    fn contrast_ratio(a: Color, b: Color) -> Option<f64> {
        let (a, b) = (relative_luminance(a)?, relative_luminance(b)?);
        let (light, dark) = if a > b { (a, b) } else { (b, a) };
        Some((light + 0.05) / (dark + 0.05))
    }

    /// The colours a cell ends up with, as the terminal draws them.
    fn drawn(style: Style) -> Option<(Color, Color)> {
        let (fg, bg) = (style.fg?, style.bg?);
        Some(if style.add_modifier.contains(Modifier::REVERSED) {
            (bg, fg)
        } else {
            (fg, bg)
        })
    }

    /// Pairs rendered by the workspace: `(label, style, minimum ratio)`.
    fn rendered_pairs(theme: &Theme) -> Vec<(String, Style, f64)> {
        let base = theme.workspace_base();
        let on = |surface: Style, role: Style| base.patch(surface).patch(role);
        let selection = base.patch(theme.workspace_selection_active());
        let inactive = base.patch(theme.workspace_selection_inactive());
        let mut pairs = vec![
            ("text on base".to_owned(), base, TEXT),
            (
                "text on panel".to_owned(),
                on(theme.workspace_panel(), Style::default()),
                TEXT,
            ),
            (
                "text on elevated_surface".to_owned(),
                on(theme.workspace_elevated_surface(), Style::default()),
                TEXT,
            ),
            ("selection".to_owned(), selection, TEXT),
            (
                "heading on selection".to_owned(),
                Theme::on_selection(selection, theme.workspace_heading()),
                TEXT,
            ),
            ("selection_inactive".to_owned(), inactive, TEXT),
            (
                "secondary_text on selection_inactive".to_owned(),
                theme.workspace_secondary_text().patch(inactive),
                TEXT,
            ),
            (
                "navigation_active".to_owned(),
                base.patch(theme.workspace_navigation_active()),
                TEXT,
            ),
            (
                "selected_indicator".to_owned(),
                base.patch(theme.workspace_selected_indicator()),
                GRAPHIC,
            ),
        ];
        for (surface_name, surface) in [
            ("base", Style::default()),
            ("panel", theme.workspace_panel()),
            ("elevated_surface", theme.workspace_elevated_surface()),
        ] {
            for (role_name, role, minimum) in [
                ("heading", theme.workspace_heading(), TEXT),
                ("secondary_text", theme.workspace_secondary_text(), TEXT),
                ("hint_key", theme.workspace_hint_key(), TEXT),
                ("hint_text", theme.workspace_hint_text(), TEXT),
                ("current_playing", theme.workspace_current_playing(), TEXT),
                ("status_warning", theme.workspace_status_warning(), TEXT),
                ("status_error", theme.workspace_status_error(), TEXT),
                (
                    "focus_indicator",
                    theme.workspace_focus_indicator(),
                    GRAPHIC,
                ),
            ] {
                pairs.push((
                    format!("{role_name} on {surface_name}"),
                    on(surface, role),
                    minimum,
                ));
            }
        }
        let playback = theme.workspace_playback_surface();
        for (label, role, minimum) in [
            ("playback_track", theme.playback_track(), TEXT),
            ("playback_artists", theme.playback_artists(), TEXT),
            ("playback_album", theme.playback_album(), TEXT),
            ("playback_metadata", theme.playback_metadata(), TEXT),
            ("playback_status", theme.playback_status(), TEXT),
            (
                "playback_progress_bar",
                theme.playback_progress_bar(),
                GRAPHIC,
            ),
        ] {
            pairs.push((
                format!("{label} on playback_surface"),
                on(playback, role),
                minimum,
            ));
        }
        pairs
    }

    #[test]
    fn contrast_ratio_matches_the_wcag_reference_values() {
        let ratio = |a, b| contrast_ratio(a, b).unwrap();
        assert!((ratio(Color::Rgb(0, 0, 0), Color::Rgb(255, 255, 255)) - 21.0).abs() < 0.01);
        assert!((ratio(Color::Rgb(255, 255, 255), Color::Rgb(0, 102, 204)) - 5.57).abs() < 0.01);
        assert_eq!(contrast_ratio(Color::Red, Color::Rgb(0, 0, 0)), None);
    }

    #[test]
    fn heading_on_a_selection_keeps_the_selection_colours() {
        let theme = ThemeConfig::default().find_theme("cobalt2-light").unwrap();
        let selection = theme.workspace_selection_active();
        let style = Theme::on_selection(selection, theme.workspace_heading());
        assert_eq!((style.fg, style.bg), (selection.fg, selection.bg));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn bundled_themes_meet_wcag_aa_contrast() {
        let mut failures = Vec::new();
        for theme in &ThemeConfig::default().themes {
            for (label, style, minimum) in rendered_pairs(theme) {
                let Some((fg, bg)) = drawn(style) else {
                    continue;
                };
                let Some(ratio) = contrast_ratio(fg, bg) else {
                    continue;
                };
                if ratio < minimum {
                    failures.push(format!(
                        "{}: {label} {fg} on {bg} = {ratio:.2} (< {minimum})",
                        theme.name
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} pairs below WCAG AA:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
