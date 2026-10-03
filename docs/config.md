# Configuration Documentation

## Table of Contents

- [General](#general)
  - [Notes](#notes)
  - [Diagnostic logging](#diagnostic-logging)
  - [Media control](#media-control)
  - [Player event hook command](#player-event-hook-command)
  - [Client id command](#client-id-command)
  - [Device configuration](#device-configuration)
  - [Layout configuration](#layout-configuration)
- [Themes](#themes)
  - [Use script to add theme](#use-script-to-add-theme)
  - [Palette](#palette)
  - [Component Styles](#component-styles)
- [Keymaps](#keymaps)

Configuration files are located in the application's configuration directory, which defaults to `$HOME/.config/unified-player`.

Offline `demo` commands use temporary configuration and cache directories,
even when `-c` or `-C` is supplied. They do not load or modify your settings,
accounts, keymaps, or caches; temporary demo files are removed on exit.

## General

A sample `app.toml` is available at [examples/app.toml](../examples/app.toml).

`unified-player` uses `app.toml` for application settings. Available options:
`unified-player` also supports cli config overriding using -o / --config-override flag. Example:

```bash
unified-player -o device.volume=80 -o theme=dracula
```

| Option                            | Description                                                                                          | Default                                                                |
| --------------------------------- | ---------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| `active_provider`                 | Startup provider mode: `Spotify` or `YouTubeMusic`.                                                  | `Spotify`                                                              |
| `client_id`                       | Spotify client ID for API access. **Leave unset unless you know you need a custom one** (see notes). | See code (default: ncspot's client ID)                                 |
| `client_id_command`               | Shell command that outputs client ID to stdout (overrides `client_id`).                              | `None`                                                                 |
| `login_redirect_uri`              | Redirect URI for authentication.                                                                     | `http://127.0.0.1:8989/login`                                          |
| `client_port`                     | Port for the application's client to handle CLI commands.                                            | `8080`                                                                 |
| `log_folder`                      | Path to store log files.                                                                             | `None`                                                                 |
| `tracks_playback_limit`           | Maximum number of tracks in a playback session.                                                      | `50`                                                                   |
| `playback_format`                 | Format string for the playback window.                                                               | `{status} {track} • {artists} {liked}\n{album} • {genres}\n{metadata}` |
| `playback_metadata_fields`        | Ordered list of metadata fields displayed in the playback UI `{metadata}` placeholder.               | `["repeat", "shuffle", "volume", "device"]`                            |
| `terminal_title`                  | Terminal window title while something plays; an empty string leaves the title untouched.             | `{status} {track} · {artists} — Unified Player`                        |
| `terminal_title_idle`             | Terminal window title while nothing plays.                                                           | `{page} — Unified Player`                                              |
| `notify_format`                   | Notification format (if `notify` feature enabled).                                                   | `{ summary = "{track} • {artists}", body = "{album}" }`                |
| `notify_timeout_in_secs`          | Notification timeout in seconds (if `notify` feature enabled).                                       | `0`                                                                    |
| `notify_transient`                | Send transient notifications (Linux only, if `notify` feature enabled).                              | `false`                                                                |
| `player_event_hook_command`       | Command to execute on player events.                                                                 | `None`                                                                 |
| `ap_port`                         | Spotify session connection port.                                                                     | `None`                                                                 |
| `proxy`                           | Spotify session connection proxy.                                                                    | `None`                                                                 |
| `theme`                           | Name of the theme to use.                                                                            | `default`                                                              |
| `app_refresh_duration_in_ms`      | Minimum interval (ms) between UI redraws. The UI redraws only when its content changes, so lower values make input more responsive without raising idle CPU; `16` is about 60 FPS. Values below `8` use `8`. Editable live in Settings as "UI frame interval". | `32`                                                                   |
| `playback_refresh_duration_in_ms` | Interval (ms) between playback refreshes and external device-state checks. While the integrated player owns playback, refreshes happen at most every 20 s. | `4000`                                                                 |
| `page_size_in_rows`               | Number of rows per page for navigation.                                                              | `20`                                                                   |
| `enable_media_control`            | Enable media control support (requires `media-control` feature).                                     | `true` (Linux), `false` (macOS/Windows)                                |
| `enable_streaming`                | Enable streaming (`Always`, `Never`, or `DaemonOnly`).                                               | `Always`                                                               |
| `enable_audio_visualization`      | Show a real-time frequency bar chart above the playback controls (requires `streaming` feature).     | `false`                                                                |
| `enable_notify`                   | Enable notifications (requires `notify` feature).                                                    | `true`                                                                 |
| `enable_cover_image_cache`        | Cache album cover images.                                                                            | `true`                                                                 |
| `notify_streaming_only`           | Send notifications only when streaming is active (requires `streaming` and `notify` features).       | `false`                                                                |
| `play_icon`                       | Icon for playing state.                                                                              | `▶`                                                                    |
| `pause_icon`                      | Icon for paused state.                                                                               | `▌▌`                                                                   |
| `liked_icon`                      | Icon for liked songs.                                                                                | `♥`                                                                    |
| `explicit_icon`                   | Icon for explicit songs.                                                                             | `(E)`                                                                  |
| `volume_icon`                     | Symbol shown instead of the volume slider on narrow playback rows; click it to open the volume popup. | `🔊`                                                                   |
| `border_type`                     | Border style: `Hidden`, `Plain`, `Rounded`, `Double`, or `Thick`.                                    | `Plain`                                                                |
| `progress_bar_type`               | Progress bar style: `Rectangle` or `Line`.                                                           | `Rectangle`                                                            |
| `progress_bar_position`           | Progress bar position: `Bottom` or `Right`.                                                          | `Bottom`                                                               |
| `layout`                          | Layout configuration (see below).                                                                    | See below                                                              |
| `genre_num`                       | Max number of genres to display in playback text.                                                    | `2`                                                                    |
| `cover_img_length`                | Cover image length in terminal columns (requires `image` feature).                                   | `0` (auto, see notes)                                                  |
| `cover_img_width`                 | Cover image height in terminal rows, up to 5 (requires `image` feature).                             | `5`                                                                    |
| `cover_img_pixels`                | Pixels per side for cover image (requires `pixelate` feature).                                       | `16`                                                                   |
| `seek_duration_secs`              | Seek duration in seconds for seek commands.                                                          | `5`                                                                    |
| `sort_artist_albums_by_type`      | Sort albums by type on artist pages.                                                                 | `false`                                                                |
| `volume_scroll_step`              | Volume change step when using mouse scroll.                                                          | `5`                                                                    |
| `enable_mouse_scroll_volume`      | Enable volume control via mouse scroll over the volume slider.                                       | `true`                                                                 |
| `enable_mouse_navigation`         | Allow mouse navigation in page lists; playback volume remains independently configurable.            | `true`                                                                 |
| `custom_queue`                    | Enable app-managed queue for custom playback integration (requires `streaming` feature).             | `true`                                                                 |
| `pause_on_startup`                | Start with playback paused instead of resuming the previous session (requires `streaming` feature).  | `false`                                                                |
| `enable_relative_line_number`     | Enable Vim-style relative line numbers for lists and popups.                                         | `false`                                                                |
| `presentation`                    | Presentation preferences for compact metadata and journal row indicators (see below).                | See below                                                              |
| `session_history`                 | Bounded local listening history used by the history view and generated Unified playlists (see below). | See below                                                              |
| `listenbrainz`                    | Privacy-first controls for optional ListenBrainz integrations (see below).                           | Disabled                                                               |
| `youtube`                         | YouTube Music configuration (see below).                                                             | See below                                                              |
| `device`                          | Device configuration (see below).                                                                    | See below                                                              |

### Notes

- By default, `unified-player` uses [ncspot](https://github.com/hrkfdn/ncspot)'s client ID for compatibility with Spotify's API. It is registered in [extended quota mode](https://developer.spotify.com/documentation/web-api/concepts/quota-modes) and predates Spotify's [November 2024 Web API changes](https://developer.spotify.com/blog/2024-11-27-changes-to-the-web-api), so it has higher rate limits and broader endpoint access than a newly-registered app. **Avoid setting a custom `client_id`**: clients registered today start in the restricted default quota mode and commonly hit `429 Too Many Requests` / `403 Forbidden` errors. `unified-player` logs a warning at startup if a custom `client_id` is detected. See [this issue](https://github.com/aome510/spotify-player/issues/890) and the [sign-in notes in the README](../README.md#sign-in) for details.
- `ap_port` and `proxy` are passed to Librespot for session configuration. Librespot uses its defaults if unset.
- Setting a positive `playback_refresh_duration_in_ms` increases API usage and may trigger rate limits. The interval applies while playback is on another device or no device is active; while the integrated player owns playback, its events keep the state current and a refresh happens at most every 20 s. No Spotify refresh runs while YouTube Music owns playback. The default `4000` ms keeps external device changes visible; set it to `0` to use event/command-only refresh.
- Integrated Spotify playback consumes librespot's Connect `VolumeChanged` event, so changing the active device volume from another Spotify client is reflected in the playback UI without waiting for a REST poll.
- `enable_streaming` accepts `Always`, `Never`, or `DaemonOnly`. For backward compatibility, `true`/`false` are also accepted.
- `border_type`, `progress_bar_type`, and `progress_bar_position` accept only the values listed in the table above.
- `explicit_icon` can be set to any Unicode character or an empty string to disable explicit markers.
- `terminal_title` and `terminal_title_idle` accept `{status}`, `{track}`, `{artists}`, `{album}`, `{provider}` and `{page}` (track fields are empty while idle). The previous title is restored on exit in terminals that support the xterm title stack. Both apply immediately when changed from Settings.
- A custom Spotify `client_id` application must list `login_redirect_uri` exactly in its Redirect URIs. Prefer an `http://127.0.0.1` or `http://[::1]` address so the redirect is captured automatically; other addresses must use HTTPS and require pasting the redirect URL. Spotify does not accept `localhost`.
- `cover_img_length = 0` (the default) auto-derives the cover's column count from the terminal's cell aspect ratio. Set a non-zero `cover_img_length` to size the box manually.
- The cover image is drawn only in the full seven-row playback area (terminals with 32 or more rows), so `cover_img_width` is capped at its 5 content rows.

### Presentation

The `[presentation]` section customizes frame chrome and secondary metadata without changing
selection behavior, provider boundaries, or action availability:

| Key | Description | Default |
| --- | --- | --- |
| `layout_preset` | Live frame chrome: `Current` preserves `border_type`; `Borderless` hides borders while preserving spacing. | `Current` |
| `profile` | Named presentation preset: `Minimal`, `Balanced`, `Detailed`, or `Custom`. | `Custom` |
| `compact_metadata` | Compact row detail level when `profile = 'Custom'`. | `Minimal` |
| `journal_indicators` | Ordered tokens shown in playlist journal capsules: `listened`, `listen_later`, `rating`, and `note`. | `['listened', 'listen_later', 'rating', 'note']` |
| `focused_row_overflow` | Focused-row text behavior: `Truncate`, `Marquee` (scrolls on its own), or `Manual` (Left/Right scroll it one character while it overflows; keymap bindings for Left/Right take precedence). | `Truncate` |

`Minimal`, `Balanced`, and `Detailed` profiles set compact metadata and journal
indicator defaults centrally. Choose `Custom` to make the two explicit fields
authoritative. An empty `journal_indicators` array intentionally hides the
journal capsule contents and removes the journal column from compact track
tables.

Presentation changes apply immediately in the running UI. They are still
persisted to `app.toml` so the same presentation is used on the next launch.
`layout_preset` works with every theme and metadata profile. Borderless keeps
Hidden border padding and titles; switching back restores the configured
`border_type`. Existing configurations use Current. Settings changes to
`border_type`, `layout.playback_window_position`, and
`layout.playback_window_height` also apply immediately without restarting
playback or resetting page history, focus, selection, or scroll state.

The Settings page marks these presentation and frame values as `On apply`.
Other persisted configuration values are marked `On restart` because the
running provider, playback, and UI services read them from the startup
configuration snapshot; pressing Apply saves them but does not hot-reload
those services. Account selectors are explicit account-switch actions and
take effect when selected.

`Marquee` applies only to overflowing text in the currently focused row. It
keeps the row highlight and column boundaries fixed while gently revealing the
rest of the label or value.

### Session history

The `[session_history]` section keeps a small, local-only record of playback
transitions. It stores provider-tagged media IDs plus bounded display metadata;
it is not uploaded, included in diagnostics, or used to alter playback. Open
the read-only history popup with `g h`, or use `g H` to name a local Unified
playlist generated from the captured entries:

```toml
[session_history]
enabled = true
max_entries = 200
```

Set `enabled = false` to opt out, or set `max_entries = 0` to retain nothing.
The implementation applies a hard safety cap of 10,000 entries even if a
larger value is configured.

Provider playback snapshots are separate from this history. Spotify playback
is refreshed from Spotify on startup; persisted YouTube transport snapshots
are intentionally not restored because their browser/player session is not
valid across processes.

### ListenBrainz

ListenBrainz requests are opt-in. The master switch defaults to `false`; each
implemented integration also has its own switch:

```toml
[listenbrainz]
enabled = false
read_only_checking = true
artist_enrichment = true
```

With both values enabled, a Spotify artist page may show source-labelled,
read-only ListenBrainz popular recordings when Spotify top tracks are
unavailable, and release-group metadata when Spotify returns no artist albums.
Successful Spotify sections stay authoritative. A ListenBrainz release-group
row resolves only when selected with Enter or the action-menu command. The
resolver accepts an explicit Spotify album relation from a MusicBrainz release;
it does not guess from the artist or album title. A matched row opens the native
Spotify album page or menu, while resolving and unavailable states remain
visible through the shared row styling. Explicit CLI commands such as
`listenbrainz probe` remain available regardless of the in-app integration
switches because running the command is itself consent. Restart the application
after changing these settings.

Welcome includes an optional ListenBrainz step. Copy the **User Token** from
ListenBrainz settings into the masked token editor and choose Validate. A
successful check saves it to `listenbrainz/token.txt` in the configuration
folder; an invalid token or failed check leaves the previous token intact.
Check saved token validates the currently configured credential. These explicit
checks work with `listenbrainz.enabled = false` and do not enable background
integrations or send listens. Missing credentials never block setup completion.
The saved token takes precedence over `LISTENBRAINZ_TOKEN`; reopening Welcome
can check a newly saved token without restarting.

After validation, **Fetch my playlists** reads your public/private ListenBrainz
playlist list. Choose one playlist and press Enter or click its row to import
it as a local Unified playlist. Refresh and Cancel are list actions. Listing
does not import automatically; neither listing nor import writes to
ListenBrainz. Existing local playlist IDs and links are protected from
replacement. Native playlist counts remain unknown until fetched; successful
imports report retained item and unresolved counts. Unresolved recordings stay
in order as metadata rows; this flow does not automatically match providers.
Changing the token, refreshing, closing the picker or leaving Welcome cancels
pending local application. Listing is bounded to 2000 remote entries.


`read_only_checking` controls explicit TUI sync previews separately from the
master integration switch. It permits reads of an already linked ListenBrainz
playlist and never authorizes a write or background synchronization.

When `listenbrainz.enabled` is true, a Unified playlist without a ListenBrainz
backup offers `Back up to ListenBrainz` in its context action menu. The action
requires the user token configured by `unified-player listenbrainz auth`,
creates one private remote backup, and reports loading, completion, failure, or
a partial remote outcome in the shared status footer. It is intentionally
manual: opening or editing a Unified playlist never starts a ListenBrainz write.

### Diagnostic logging

Application logs accept `RUST_LOG` filtering, but persistent and in-app sinks
record only events whose target belongs to `unified-player`. Third-party crate
logs are deliberately excluded because their debug and trace contracts are not
under this project's privacy control. The highest supported application
verbosity is:

```text
RUST_LOG=unified_player=trace
```

Current application events use bounded operation facts and registered error
codes. They omit tokens, cookies, authorization material, request and response
bodies, complete URLs, proxy values, filesystem locations, clipboard and key
content, media and library identities, full request objects, arbitrary error
chains, and panic payloads/backtraces.

New diagnostic files use schema-versioned JSON Lines and the filename prefix
unified-player-diagnostics-. They rotate at 10 MiB or a UTC day boundary and
are retained for at most seven days and 50 MiB total. File writes are
non-blocking; saturation or a write failure appears as writer health and a
dropped-event count in the live diagnostics report rather than interrupting
playback.

The live report also includes the schema version, source revision and tracked
dirty-state marker, run ID, uptime, writer state, and bounded writer counters.
Event compatibility rules are recorded in ADR 0003.

Log and backtrace files created before Phase 6A on 2026-07-29 are legacy
sensitive artifacts. They may contain authentication tokens or private activity
and must not be attached to an issue, copied into a support bundle, committed,
or published. The application does not migrate, read, or sanitize those files.
After stopping the application, owners may delete old `unified-player-*.log`
and `unified-player-*.backtrace` files from the configured `log_folder` when
they are no longer needed locally. If an old file was shared, revoke the
affected provider session and rotate any proxy credential that may have been
recorded.

### YouTube Music configuration

YouTube Music options are configured in the `[youtube]` section:

| Option                       | Description                                                                                   | Default                      |
| ---------------------------- | --------------------------------------------------------------------------------------------- | ---------------------------- |
| `auth_type`                  | YouTube Music auth mode: `Browser`, `OAuth`, or `Unauthenticated`. YouTube mode requires login. | `Browser`                    |
| `cookie_file`                | Browser-cookie auth file. If unset, the app reads from the default config path.                | `$CONFIG/youtube/cookie.txt` |
| `oauth_file`                 | OAuth token JSON file. If unset, the app reads from the default config path.                   | `$CONFIG/youtube/oauth.json` |
| `po_token_file`              | Optional proof-of-origin token for the native playback resolver when required.                 | unset                        |
| `playback_quality`           | Native audio selection policy: `High` or `DataSaver`.                                         | `High`                       |
| `native_audio_cache_size_mb` | Maximum temporary ranged-stream cache size, clamped to 4-128 MiB.                              | `16`                         |
| `javascript_runtime`         | JavaScript challenge runtime: `Auto`, `Node`, or `QuickJs`.                                    | `Auto`                      |

YouTube playback resolves an audio-only source in process, streams it through
ranged HTTPS requests, and decodes it directly with rodio/Symphonia. No
external downloader, transcoder, or separate updater is required.

`javascript_runtime` controls only player challenge solving. `Auto` and
`QuickJs` use the embedded QuickJS engine (the `youtube-quickjs` build feature,
part of the default `standard` profile) and fall back to Node.js if it fails;
a build without QuickJS uses Node.js. `Node` always uses Node.js, which must
then be installed. The setting is applied after restarting the application.
After a YouTube track resolves, the playback route line shows the runtime that
actually solved the latest player challenge as `js: QuickJS` or `js: Node`. If
no `js:` label is shown, that resolution did not need JavaScript deciphering.

For browser auth, save the YouTube Music `Cookie` request header to `youtube/cookie.txt` in the app config directory.

`po_token_file` remains optional and is never read from command arguments or
printed in diagnostics. A legacy file containing one token is treated as a
player token. For client-specific proof material, use a JSON file such as:

```json
{
  "player": "<player-token>",
  "gvs": "<gvs-token>",
  "clients": {
    "WEB_REMIX": {
      "player": "<web-remix-player-token>",
      "gvs": "<web-remix-gvs-token>"
    },
    "ANDROID_VR": {
      "player": "<android-vr-player-token>",
      "gvs": "<android-vr-gvs-token>"
    }
  }
}
```

Values stay in the configured file. Android VR public fallback requests never
receive browser credentials or inherit legacy/global proof tokens. Automatic
Android VR playback is enabled only by an explicit client-scoped player or GVS
token; forced CLI probes remain available without one.

The CLI includes guided setup, one-time Google device login, and diagnostics:

```text
unified-player youtube auth
unified-player youtube auth --type oauth
unified-player youtube browser-login
unified-player youtube login
unified-player youtube status
unified-player youtube status --check
unified-player youtube status --check --video-id <video-id>
unified-player youtube status --check --video-id <video-id> --transport-diagnostic
unified-player youtube status --check --video-id <video-id> --audio-output
unified-player youtube debug inspect --video-id <video-id> --acknowledge-sensitive
unified-player youtube debug compare --config-a <folder> --cache-a <folder> \
  --config-b <folder> --cache-b <folder> --video-id <video-id> \
  --acknowledge-sensitive
unified-player youtube like --video-id <video-id>
unified-player youtube like --video-id <video-id> --unlike
unified-player youtube playlist create "My playlist"
unified-player youtube playlist add --playlist-id <playlist-id> --video-id <video-id>
unified-player youtube playlist remove --playlist-id <playlist-id> --set-video-id <set-video-id>
unified-player youtube playlist delete --playlist-id <playlist-id>
```

`youtube login` uses a Google OAuth client for TVs and Limited Input devices.
Provide `UNIFIED_PLAYER_YOUTUBE_OAUTH_CLIENT_ID` and
`UNIFIED_PLAYER_YOUTUBE_OAUTH_CLIENT_SECRET`; the command opens Google's device
verification page, stores the resulting refresh token atomically, and selects
OAuth mode. See [YouTube Music authentication](youtube-auth.md) for the supported
approaches and their playback limitations.

`youtube browser-login` opens an isolated Chrome, Chromium, or Edge profile and
automatically imports its YouTube cookies through the browser's local debugging
protocol once sign-in completes. The
profile is never the browser's normal user profile. After the first interactive
sign-in, the app launches that isolated profile headlessly when the saved cookie
snapshot is older than 12 hours, refreshes the snapshot, and closes the browser.
Use `--browser <path>` when the executable is not discovered automatically.

The setup and status commands print the configured credential path without
printing credential contents. Without `--check`, `youtube status` only checks
whether the configured credential file exists.
Inside the TUI, use Welcome to sign in through the browser or import a cookie
file; see [YouTube Music authentication](youtube-auth.md#sign-in-from-welcome-without-a-detected-chromium-browser).
`ImportYouTubeAuthFromClipboard` remains available for custom keybindings, but
has no default binding.
The YouTube Music Settings section also exposes the dedicated browser sign-in
as a two-step action: the first activation opens the isolated profile and the
second captures the session after Google sign-in completes.
With `--check`, the command makes authenticated library requests and resolves a
native playback probe. It prints the number of playlists, albums, and artists
returned, any provider/parser warnings, and a redacted playback-format summary.
Metadata success alone is not reported as complete authentication success.
Pass `--video-id` with `--check` to exercise the exact song or video that failed;
this distinguishes a real login failure from the authenticated-TV/dedicated-
browser transport path and verifies both a continuation range and the decoder.
Add `--transport-diagnostic` to compare the browser response, an exact in-memory
request replay, and the current sanitized continuation replay. Its output is
limited to status codes and safe booleans; signed URLs, cookies, tokens, and
captured header values are not printed or persisted.
Add `--audio-output` to send a short zero-volume source through the default
Rodio output stream and sink as well. This verifies the device path without
playing audible music.

Builds compiled with the default-off `private-capture` feature also expose
`youtube debug inspect` and `youtube debug compare`. These are private,
interactive-terminal-only developer reports. They show the selected account,
player client, playability status/reason, format inventory, and optional
browser-transport facts without printing credentials, authorization headers,
signed URL values, media bodies, or browser profile data. `compare` runs the
same probe against two isolated config/cache roots and does not switch the
running application's account. `youtube debug-capture live` runs one matching
UI-independent playback attempt and stores its bounded evidence in the
encrypted private vault; it prints only a safe capture reference. Use
`youtube debug-capture inspect` for the existing encrypted capture artifacts
when exact bounded response evidence is needed.
The like and playlist commands are explicit account mutations and require an
authenticated YouTube Music session. Playlist removal takes the playlist
entry's `set-video-id`, not merely the public video ID.
The context action menu also exposes a YouTube-specific playlist selector when
the authenticated library is available; it does not reuse Spotify playlist IDs.

### Lyrics

The `[lyrics]` section controls which external sources are allowed after
provider-native lyrics are unavailable:

```toml
[lyrics]
providers = ["simpmusic", "lrclib", "musixmatch"]
```

The fallback order is fixed and deterministic: SimpMusic, LRCLIB, Lyrics.ovh,
then Musixmatch. The list is an allowlist, so removing a provider disables its
network request. Lyrics.ovh is available as an opt-in plain-lyrics fallback;
add `"lyricsovh"` when you explicitly accept another public third-party
request. `lyrics.providers = []` keeps native Spotify lyrics but disables all
external lookups. The settings page exposes this as a multi-choice setting
under Services.

The Lyrics page uses Spotify's native lyrics when available. YouTube Music
tracks and Spotify tracks without native lyrics fall back to SimpMusic (using
the YouTube video ID when available), preferring its rich word-timed response,
then LRCLIB when album and duration metadata are available, then Lyrics.ovh
when enabled, and finally Musixmatch. Results are cached for the normal lyrics
cache duration. External providers are best-effort and may be unavailable or
return a different sync quality; no lyric body is written to diagnostics.

### Unified playlist storage

Unified playlists are stored locally as provider-tagged JSON under
`unified_playlists.json` in the config directory. Writes retain the previous
file as `unified_playlists.json.bak`; use `unified-player unified export --output
<path>` for an explicit copy, or `unified export --format jspf --playlist-id
<id>` for a JSPF-shaped portability export. The `unified link` command records optional
Spotify, YouTube Music, and ListenBrainz projection IDs without contacting those
services.

ListenBrainz is an explicit portability target, not an automatic sync service.
Its playlist API requires a user token and JSPF payloads identify recordings by
MusicBrainz recording MBID, so provider-only Spotify and YouTube IDs remain in
the local sidecar. YouTube playback and metadata use unofficial endpoints;
availability and service behavior can change independently of this application.
The native resolver keeps that contract typed and isolated. The opt-in
`listenbrainz backup --playlist-id <id>` command creates a private, empty remote
playlist, stores the provider identity and occurrence order in its versioned
description, and records the returned MBID in `PlaylistLink` only after both
remote steps succeed. It refuses descriptions over 9,000 characters before
creating anything and reports the remote MBID if creation succeeds but the
description edit fails. It does not modify an already linked remote playlist.
The opt-in TUI context action uses this same transport and persistence contract.
`listenbrainz restore --playlist-id <mbid>` only previews by default and requires
`--apply` to write a local unified playlist. A valid unified-player description
manifest is authoritative for provider identity and occurrence order; ordinary
ListenBrainz playlists continue to use their JSPF recording rows.
`listenbrainz diff --playlist-id <mbid> --unified-id <id>` compares order and
membership without mutating either side. Entries without a provider extension or
recognizable Spotify/YouTube identifier are reported as unresolved.
`listenbrainz plan --playlist-id <mbid> --unified-id <id>` produces a read-only,
manifest-first three-way plan. The versioned description manifest is the
lossless base anchor; JSPF rows are treated as an interoperability projection,
not as a replacement for provider and occurrence identity. A missing, invalid,
foreign, or snapshot-mismatched manifest produces a typed `cannot_plan` result.
Duplicate rows remain distinct by playlist entry ID, incompatible order changes
and unlinked JSPF rows are conflicts, and
`--expected-remote-fingerprint <sha256>` can reject a stale preview. Add
`--format json` for deterministic tooling output. A `cannot_plan` result is
printed before the command exits non-zero. This command does not generate apply
operations or mutate local or remote state.

`listenbrainz probe [--spotify-artist-id <id>] [--unified-id <id>] [--json]`
is a non-mutating capability check. It validates the configured token without
printing it, optionally measures Spotify-artist enrichment and Unified item to
MusicBrainz recording coverage, assesses a versioned provider-identity envelope
against the playlist-description budget, and reports scrobble readiness. It
also uses the validated token for ListenBrainz popularity reads and evaluates
direct Spotify album identity for at most five release groups. Album matching
browses MusicBrainz releases at one request per second, retries one HTTP 503,
and accepts only explicit Spotify album URL/URI relations; it does not infer an
album from artist or title similarity. The report contains bounded match,
unmatched, unavailable, retry, and duration facts without relation URLs or
credentials. The command does not create or edit playlists and never submits a
listen.

To attach a YouTube target for ongoing additions, record its native playlist ID:

```text
unified-player unified link --playlist-id <local-id> --youtube-id <youtube-playlist-id>
```

After this link exists, additions made through the TUI are appended to the
YouTube target as well. Spotify-origin items are sent only when the shared
metadata matcher finds one unambiguous YouTube result; unresolved items remain
local and report a bounded failure. Use the projection command below for an
initial full sync or for repair after a partial failure.
From the linked Unified playlist page, `g P` also performs that initial/repair
sync in the TUI, appending only missing occurrences and preserving remote rows.

Unified playlists can be projected to an existing YouTube Music playlist with an
explicit dry run first:

```text
unified-player unified project --playlist-id <local-id> --provider youtube \
  --target-playlist-id <youtube-playlist-id>
unified-player unified project --playlist-id <local-id> --provider youtube \
  --target-playlist-id <youtube-playlist-id> --apply
unified-player unified project --playlist-id <local-id> --provider youtube \
  --target-playlist-id <youtube-playlist-id> --check-remote
unified-player unified project --playlist-id <local-id> --provider youtube \
  --target-playlist-id <youtube-playlist-id> --check-remote \
  --resolution merge --apply
unified-player unified project --playlist-id <local-id> --provider youtube \
  --target-playlist-id <youtube-playlist-id> --resolution match
```

Projection appends YouTube items in local order, preserves duplicates when the
provider accepts them, reports Spotify/unresolved items, and never removes
existing target items. Partial failures are reported and the target link is kept
in the local `PlaylistLink` sidecar. The sidecar also stores a local snapshot
hash so later dry runs can flag edits made since the previous projection.
`--check-remote` also reads the target playlist and reports remote-only, missing,
ordering, and remote-snapshot changes before any apply.
When a recorded local or remote snapshot conflicts, `--apply` stops safely;
choose `--resolution local` to keep the local playlist authoritative, or
`--resolution merge` to append remote-only YouTube items to the local playlist.
`--force` remains available as an explicit override.
Merge keeps existing Spotify entries and local ordering, then appends remote-only
YouTube entries before recalculating the local snapshot.
`--resolution match` is an explicit, opt-in metadata lookup for Spotify-origin
items. It searches YouTube using title, artist, and duration evidence, accepts
only a high-confidence result with a clear margin over the next candidate, and
reports ambiguous or unresolved items. It is dry-run by default; `--apply`
refuses to write anything while any item remains unresolved.

#### Media control

Media control (`enable_media_control`) is enabled by default on Linux but disabled on macOS and Windows. On these platforms, the OS requires an open window to receive media events, which may cause the terminal to lose focus on startup.

### Player event hook command

`player_event_hook_command` is an object with `command` and `args` fields. On each player event, the command executes with the event data passed as arguments.

A player event is represented as a list of arguments with either of the following values:

- `"Changed" NEW_TRACK_ID`
- `"Playing" TRACK_ID POSITION_MS`
- `"Paused" TRACK_ID POSITION_MS`
- `"EndOfTrack" TRACK_ID`

**Note**: If `args` is specified, these arguments precede the event arguments.

For example, with `player_event_hook_command = { command = "a.sh", args = ["-b", "c", "-d"] }`, a `Changed` event with `NEW_TRACK_ID=id` executes:

```shell
a.sh -b c -d Changed id
```

Example script that reads event data from arguments and logs to a file:

```sh
#!/bin/bash

set -euo pipefail

case "$1" in
    "Changed") echo "command: $1, new_track_id: $2" >> /tmp/log.txt ;;
    "Playing") echo "command: $1, track_id: $2, position_ms: $3" >> /tmp/log.txt ;;
    "Paused") echo "command: $1, track_id: $2, position_ms: $3" >> /tmp/log.txt ;;
    "EndOfTrack") echo "command: $1, track_id: $2" >> /tmp/log.txt ;;
esac
```

### Client id command

To securely store your `client_id`, use `client_id_command` with a `command` and optional `args`. Example:

```toml
client_id_command = { command = "cat", args = ["/full/path/to/file"] }
```

**Note**: Use absolute paths; `~` is not expanded.

### Device configuration

Device options are configured in the `[device]` section:

| Option          | Description                              | Default          |
| --------------- | ---------------------------------------- | ---------------- |
| `name`          | Device name.                             | `unified-player` |
| `device_type`   | Device type.                             | `speaker`        |
| `volume`        | Initial volume (percent).                | `70`             |
| `bitrate`       | Bitrate in kbps (`96`, `160`, or `320`). | `320`            |
| `audio_cache`   | Enable audio file caching.               | `false`          |
| `normalization` | Enable audio normalization.              | `false`          |
| `autoplay`      | Enable autoplay of similar songs.        | `false`          |

See the [Librespot wiki](https://github.com/librespot-org/librespot/wiki/Options) for more details on these options.

### Layout configuration

The `[layout]` section configures the UI layout:

| Option                     | Description                                          | Default |
| -------------------------- | ---------------------------------------------------- | ------- |
| `library.album_percent`    | Percentage of the album window in the library.       | `40`    |
| `library.playlist_percent` | Percentage of the playlist window in the library.    | `40`    |
| `playback_window_position` | Position of the playback window (`Top` or `Bottom`). | `Top`   |
| `playback_window_height`   | Height of the playback window.                       | `6`     |

Example:

```toml

[layout]
library = { album_percent = 40, playlist_percent = 40 }
playback_window_position = "Top"

```

## Themes

`unified-player` uses `theme.toml` for custom themes.

Sample themes are available at [examples/theme.toml](../examples/theme.toml).
The `unified-player` example contains the design-v1 RGB palette and workspace
semantic roles; the built-in default workspace uses the same palette when no
custom theme overrides it.

Select a theme by setting `theme` in `app.toml` or using the `-t <THEME>` / `--theme <THEME>` CLI flag.

Besides `default`, the application ships themes adapted from
[OpenCode](https://github.com/anomalyco/opencode)'s TUI themes: `aura`, `ayu`, `carbonfox`, `catppuccin`, `catppuccin-frappe`, `catppuccin-macchiato`, `cobalt2`, `dracula`, `everforest`, `flexoki`, `gruvbox`, `kanagawa`, `material`, `matrix`, `mercury`, `monokai`, `nightowl`, `nord`, `one-dark`, `osaka-jade`, `palenight`, `rosepine`, `solarized`, `synthwave84`, `tokyonight`, `vesper`, `zenburn`.
Most of them also have a light variant named `<theme>-light` (for example
`gruvbox-light`). Where an original color is too faint to read, its lightness
is adjusted so every built-in theme meets WCAG AA contrast (4.5:1 for text); hues
are kept. A theme in `theme.toml` with the same name as a built-in
theme replaces it. See
[`OPENCODE_NOTICE.md`](../unified-player/src/config/themes/OPENCODE_NOTICE.md)
for attribution and license.

A theme consists of:

- `name` (required): Theme name.
- `palette` (optional): Color palette.
- `component_style` (optional): Styles for UI components.

Omitted `palette` values use terminal colors. Omitted `component_style` values use default styles.

### Component Styles

The `component_style` table customizes UI component appearance. All fields are optional:

| Field                            | Description                                               |
| -------------------------------- | --------------------------------------------------------- |
| `block_title`                    | Style for block titles                                    |
| `border`                         | Style for borders                                         |
| `playback_status`                | Style for the playback status indicator                   |
| `playback_track`                 | Style for the currently playing track name                |
| `playback_artists`               | Style for the artist(s) of the current track              |
| `playback_album`                 | Style for the album name of the current track             |
| `playback_genres`                | Style for the genres of the current track                 |
| `playback_metadata`              | Style for the metadata section in playback                |
| `playback_progress_bar`          | Style for the filled portion of the playback progress bar |
| `playback_progress_bar_unfilled` | Style for the unfilled portion (only for `Line` type)     |
| `current_playing`                | Style for the currently playing item in lists             |
| `page_desc`                      | Style for the page description                            |
| `playlist_desc`                  | Style for the playlist description                        |
| `table_header`                   | Style for table headers                                   |
| `selection`                      | Style for selected items                                  |
| `secondary_row`                  | Style for secondary rows in tables/lists                  |
| `like`                           | Style for the like indicator                              |
| `lyrics_played`                  | Style for played lyrics lines                             |
| `lyrics_playing`                 | Style for the currently playing lyrics line               |
| `sync_clean`                     | Style for clean/in-sync sync states                       |
| `sync_changed`                   | Style for changed (unapplied) sync states                 |
| `sync_conflict`                  | Style for conflicted/failed sync states                   |
| `sync_neutral`                   | Style for inactive sync states                            |
| `base`                           | Base workspace surface and readable foreground            |
| `panel`                          | Library/navigation and inspector surface                  |
| `elevated_surface`               | Grouped action surface                                    |
| `playback_surface`               | Bottom playback surface                                   |
| `secondary_text`                 | Shared metadata and hint text                             |
| `navigation_active`              | Active route in workspace navigation                      |
| `selection_inactive`             | Selected row in an unfocused pane                         |
| `focus_indicator`                | Keyboard-focus rail or indicator                          |
| `selected_indicator`             | Marker styling on an active selected row                  |
| `multiselect`                    | Multi-selection marker                                    |
| `hint_key`                       | Footer shortcut key                                       |
| `hint_text`                      | Footer shortcut description                               |
| `disabled`                       | Disabled-but-explained controls                           |
| `status_warning`                 | Warning status                                            |
| `status_error`                   | Error status                                              |
| `scrollbar_track`                | Scrollbar track                                           |
| `scrollbar_thumb`                | Scrollbar thumb                                           |
| `playback_progress_remaining`    | Unfilled bottom progress cells                            |

Each style accepts optional fields:

- `fg`: Foreground color
- `bg`: Background color
- `modifiers`: List of style modifiers

Defaults use palette values or remain unset if not specified.

#### Example

```toml
[[themes]]
name = "my_theme"
[themes.component_style]
block_title = { fg = "Magenta", modifiers = ["Bold"] }
border = { fg = "White" }
selection = { modifiers = ["Reversed", "Bold"] }
```

#### Default Component Styles

```toml
block_title = { fg = "Magenta"  }
border = {}
playback_status = { fg = "Cyan", modifiers = ["Bold"] }
playback_track = { fg = "Cyan", modifiers = ["Bold"] }
playback_artists = { fg = "Cyan", modifiers = ["Bold"] }
playback_album = { fg = "Yellow" }
playback_genres = { fg = "BrightBlack", modifiers = ["Italic"] }
playback_metadata = { fg = "BrightBlack" }
playback_progress_bar = { bg = "BrightBlack", fg = "Green" }
playback_progress_bar_unfilled = { bg = "BrightBlack" }
current_playing = { fg = "Green", modifiers = ["Bold"] }
page_desc = { fg = "Cyan", modifiers = ["Bold"] }
playlist_desc = { fg = "BrightBlack", modifiers = ["Dim"] }
table_header = { fg = "Blue" }
selection = { modifiers = ["Reversed", "Bold"] }
secondary_row = {}
like = {}
lyrics_played = { modifiers = ["Dim"] }
lyrics_playing = { fg = "Green", modifiers = ["Bold"] }
sync_clean = { fg = "Green", modifiers = ["Bold"] }
sync_changed = { fg = "Yellow" }
sync_conflict = { fg = "Red", modifiers = ["Bold"] }
sync_neutral = { fg = "BrightBlack", modifiers = ["Italic"] }
```

#### Accepted Colors

Colors can be:

- Black, Blue, Cyan, Green, Magenta, Red, White, Yellow
- BrightBlack, BrightWhite, BrightRed, BrightMagenta, BrightGreen, BrightCyan, BrightBlue, BrightYellow
- Hex codes: `#RRGGBB` (e.g., `#ff0000`)

#### Style Modifiers

Supported modifiers:

- Bold
- Dim
- Italic
- Underlined
- RapidBlink
- Reversed
- Hidden
- CrossedOut

Specify multiple modifiers as a list: `modifiers = ["Bold", "Underlined"]`.

## Diagnostics

The Settings diagnostics action opens the live Diagnostics view. It contains
only bounded component, operation, logging, UI, and incident facts. Support
bundle creation/review and the complete privacy boundary are documented in
[`diagnostics-and-privacy.md`](diagnostics-and-privacy.md).

A build compiled with the default-off `private-capture` feature adds a separate
safe operator row and the provider-scoped `youtube debug-capture` CLI. It is
not configured through TOML and is not a verbose logging option.

### Use script to add theme

The [`theme_parse`](../scripts/theme_parse) Python script (requires `toml` and `requests`) converts [iTerm2/alacritty color schemes](https://github.com/mbadolato/iTerm2-Color-Schemes/tree/master/alacritty) to compatible theme format.

Example:

```
./theme_parse "iTerm2 Solarized Dark" "solarized_dark"  >> ~/.config/unified-player/theme.toml
```

This converts the [iTerm2 Solarized Dark](https://github.com/mbadolato/iTerm2-Color-Schemes/blob/master/alacritty/iTerm2%20Solarized%20Dark.toml) color scheme to a theme named `solarized_dark`.

### Palette

A theme's `palette` table can include:

- `background`
- `foreground`
- `black`
- `blue`
- `cyan`
- `green`
- `magenta`
- `red`
- `white`
- `yellow`
- `bright_black`
- `bright_blue`
- `bright_cyan`
- `bright_green`
- `bright_magenta`
- `bright_red`
- `bright_white`
- `bright_yellow`

Omitted fields use terminal defaults. Values can be color names or hex codes. See [ANSI color reference](https://en.wikipedia.org/wiki/ANSI_escape_code#3-bit_and_4-bit).

## Keymaps

`unified-player` uses `keymap.toml` to add or override [default key mappings](commands.md#keys). Add a `keymaps` entry to define a new mapping, or set the command to `None` to remove one. Example:

```toml
[[keymaps]]
command = "NextTrack"
key_sequence = "g n"
[[keymaps]]
command = "PreviousTrack"
key_sequence = "g p"
[[keymaps]]
command = "Search"
key_sequence = "C-c C-x /"
[[keymaps]]
command = "ResumePause"
key_sequence = "M-enter"
[[keymaps]]
command = "None"
key_sequence = "q"
[[keymaps]]
command = { VolumeChange = { offset = 1 } }
key_sequence = "-"
[[keymaps]]
command = { SeekForward = { duration = 10 } }
key_sequence = "E"
[[keymaps]]
command = { SeekBackward = { } }
key_sequence = "Q"
```

A complete list of actions is available [here](commands.md#actions).

### Creating playlists

Press `N` to open the playlist-creation popup. The destination field starts on
the active provider and can be changed with Left/Right before entering the
playlist name:

- **Spotify** creates a native Spotify playlist with the name and description.
- **YouTube Music** creates a private native playlist and requires an
  authenticated YouTube Music session.
- **Unified (local)** creates an empty provider-neutral playlist in the local
  unified-playlist store. It remains local until explicitly linked to a native
  playlist; linked YouTube targets receive later additions after conservative
  provider matching.

Press Tab to move through destination, name, and description. Creating a
YouTube or Unified playlist is an explicit provider/local mutation and reports
its result through the normal operation feedback surface.

While viewing a Unified playlist, press `g p` to choose an existing YouTube
playlist as its linked target. The initial contents are not copied by linking;
use `unified project --check-remote` for an explicit dry run and repair.
Press `g u` on a linked Unified playlist to remove the link; this leaves both
the local playlist and the native YouTube playlist untouched.

## Actions

Actions are defined in `keymap.toml` and triggered by unbound key sequences. Actions target the selected item by default, but can be configured with `target` set to `PlayingTrack` or `SelectedItem`. See the [action list](commands.md#actions).

Example:

```toml
[[actions]]
action = "GoToArtist"
key_sequence = "g A"
[[actions]]
action = "GoToAlbum"
key_sequence = "g B"
target = "PlayingTrack"
[[actions]]
action="ToggleLiked"
key_sequence="C-l"
```

## Offline showcase

`unified-player demo screen home --scenario showcase --interactive` uses fresh,
temporary configuration and cache folders. It supplies fictional libraries and
search results for both providers. Enter simulates playback, Z adds a selected
track to the queue, n/p change tracks, and l opens original synchronized lyrics.
No provider sessions or audio workers start.

With a streaming-enabled build, add `-o enable_audio_visualization=true` before
`demo` to show animated synthetic bands. They use the demo playback clock and
stop when paused. Other preview scenarios remain static layout fixtures.
