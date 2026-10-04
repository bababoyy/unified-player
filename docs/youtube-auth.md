# YouTube Music Authentication

YouTube Music has two separate authentication surfaces in this application:

1. The control plane covers search, library data, likes, and playlist mutations.
2. The playback plane obtains a playable media source from YouTube's player API.

A credential is not healthy merely because the control plane accepts it. The
`youtube status --check` command deliberately exercises both planes.

## Available approaches

### Google OAuth for installed or limited-input applications

OAuth is the best fit for the control plane. The user consents once, the app
stores a refresh token, and expired access tokens are refreshed without opening
DevTools. Google documents both the
[desktop PKCE flow](https://developers.google.com/identity/protocols/oauth2/native-app)
and the
[limited-input device flow](https://developers.google.com/identity/protocols/oauth2/limited-input-device).
The current `ytmapi-rs` integration uses the latter and persists each refreshed
token atomically.

OAuth cannot promise a permanent login. Google documents revocation, inactivity,
password/security changes, client deletion, and refresh-token limits as reasons
a refresh token can stop working. The app must report `invalid_grant` as a real
reauthentication requirement rather than silently falling back to another
identity. See Google's
[OAuth overview and token-expiration rules](https://developers.google.com/identity/protocols/oauth2#expiration).

YouTube currently restricts OAuth for direct player extraction. The maintained
yt-dlp project likewise records that
[OAuth login no longer works for YouTube playback](https://github.com/yt-dlp/yt-dlp/wiki/Extractors#logging-in-with-oauth).
OAuth success for library or playlist requests therefore does not prove playback
success.

### Browser session cookies

A signed-in browser session can authorize both YouTube Music control-plane
requests and cookie-capable player clients. Raw cookie copying is the current
compatibility path, but it is not a durable login design: browser sessions rotate,
can be revoked, and are highly sensitive bearer credentials.

Reading a user's normal Chrome profile directly is not an acceptable default.
Chrome deliberately strengthened cookie protection and, since Chrome 136,
requires a non-default profile for remote debugging. See Chrome's
[remote-debugging security change](https://developer.chrome.com/blog/remote-debugging-port).
The implemented browser-session bridge therefore owns a dedicated, isolated
profile, requires an explicit first sign-in, binds its debugging endpoint to
localhost, and never reads the user's normal browser profile or requests
administrator privileges.

### API keys, service accounts, and OpenID Connect

- An API key identifies a project, not a YouTube Music user. It cannot access a
  private library or mutate playlists on the user's behalf.
- A service account is not a consumer YouTube Music identity and cannot replace
  user authorization.
- A Sign in with Google/OpenID identity token proves who the user is, but does
  not grant the YouTube scope or a playable media source.

These are not substitutes for user OAuth or a browser session.

### PO tokens

A PO token is proof-of-origin material used by some player clients. It is not a
Google login, does not identify the user's library, can expire independently,
and may still be required alongside cookies. It must remain a separate playback
diagnostic, never be presented as account authentication.

The configured PO-token file may contain the legacy plain player token, or a
JSON object with separate `player`, `gvs`, and client-scoped entries. Player
tokens are sent only to the selected player client; GVS tokens are attached
only to that client's native media URL. Android VR never inherits legacy or
global proof material; it may receive only an explicit `clients.ANDROID_VR`
player or GVS token.

### Official IFrame playback

The official browser IFrame Player API avoids direct media-source resolution and
lets Google own the playback session. It is the most supportable playback
alternative, but it requires a visible browser player and therefore cannot be a
pure terminal-only audio engine. The architectural tradeoff is documented in
the local `.agents/docs/PLAN2.md` implementation plan.

## Implemented direction

- `unified-player youtube login` performs one Google device authorization,
  writes the refresh token to the configured OAuth file, switches the configured
  auth type to `OAuth`, and refreshes tokens automatically.
- Browser-cookie import remains available for compatibility while an isolated
  browser-session can be established with `unified-player youtube browser-login`.
- The dedicated profile is opened visibly for the first sign-in. Afterwards the
  app refreshes the cookie snapshot headlessly when it is older than 12 hours,
  so browser token rotation does not require another DevTools copy operation.
- Interactive browser login detects the signed-in YouTube session automatically,
  saves it, and closes the dedicated browser. No terminal confirmation is
  required; activating the Settings action again cancels an in-progress login.
  The Settings workflow then validates account access and native playback as
  separate stages, so a transport failure is not reported as a login failure.
- Browser playback signs the saved session with `SAPISIDHASH` and uses a
  cookie-capable TV, WEB, or WEB_REMIX player client. Sending the cookie to the
  Android VR client is intentionally avoided because that client does not
  authenticate browser sessions and can misreport a valid login as a bot-check
  failure.
- If the cookie-capable clients report no usable source, the resolver may try
  Android VR as a bounded public fallback only when explicit Android VR proof
  material is configured. It never sends browser cookies or inherits another
  client's PO token, cannot authorize private or Premium-only media, and is
  reported as a separate `ANDROID_VR` source rather than as authenticated
  account playback. Forced CLI probes may still inspect the tokenless route.
- `unified-player youtube status --check` tests library access and native player
  source resolution. A metadata-only success is not reported as full success.
- Add `--video-id <video-id> --transport-diagnostic` to compare the dedicated
  browser response, an in-memory exact replay, and the current sanitized ranged
  replay. The command prints only status codes and safe request-shape booleans;
  signed URLs, cookies, tokens, and captured header values are never printed or
  persisted.
- Add `--audio-output` to that command for a silent default-device and Rodio
  sink check after transport and decoding succeed.
- Playback never sends an authenticated identity to the public fallback or
  presents that fallback as account authorization; the client and auth scope
  remain visible in developer inspection.

Some media is returned to the authenticated TV client solely as a ciphered
source. After that authenticated request returns `OK`, the resolver opens the
same media in the muted, headless dedicated profile and captures the official
web player's per-video, playback-origin-token-bearing MP4 request. SABR-only
query parameters are removed before the URL enters the native ranged transport.
The sanitized media URL is requested with the standard HTTP `Range` header.
A track of up to 32 MiB (about 30 minutes of audio) is fetched whole as four
parallel 1 MiB ranges and decoded from memory: probing a fragmented MP4 reads
every fragment header, which over a streamed source costs one request per
fragment. Larger tracks, and any track whose whole fetch fails, are streamed
with initial, continuation, and seek range requests.
The resolver proves a nonzero byte range before publishing playback, preventing
a tokenless preview fragment from being mistaken for a complete track. The
dedicated browser-session fallback remains gated strictly on `Decipher` after
an authenticated response. Authentication, consent, and region failures do
not enter it. The bounded public Android VR fallback is considered only after
the authenticated client matrix reports provider unavailability or unsupported
formats. Diagnostics name the browser path `WEB_MUSIC_BROWSER_SESSION` and the
public path `ANDROID_VR`.

When YouTube Music credentials are ready, the app fetches the public client
versions and a guest visitor identifier at startup, even if Spotify is the
active provider. These requests send no account cookies or proof tokens; they
spare the first playback about a second.

The browser is bounded rather than permanently resident. The current-track
resolution leaves one hidden process warm just long enough for the existing
next-track prefetch to reuse it. Each temporary YouTube Music target is closed
as soon as its media request is captured; the prefetch capture closes the whole
browser immediately, while an unclaimed warm process has a 20-second idle
timeout. Switching to Spotify, quitting the TUI, and command-line diagnostics
also close it explicitly. Thus browser RAM is a short resolution-time spike,
not a steady application cost.

Interactive sign-in and hidden playback capture have exclusive ownership of
the dedicated profile. Starting sign-in first closes any playback browser owned
by the running application, and playback capture waits until sign-in finishes
or is cancelled. This prevents Chrome from reusing the hidden process while the
app waits for a new debugging port.

The intended user experience is "sign in once and refresh until Google revokes
the grant," not the impossible promise that credentials can never expire.

## Device login

Create a Google OAuth client for **TVs and Limited Input devices**, then provide
its values through environment variables so the secret is not placed in shell
history:

```text
UNIFIED_PLAYER_YOUTUBE_OAUTH_CLIENT_ID=<client-id>
UNIFIED_PLAYER_YOUTUBE_OAUTH_CLIENT_SECRET=<client-secret>
unified-player youtube login
unified-player youtube status --check
```

`YOUTUI_OAUTH_CLIENT_ID` and `YOUTUI_OAUTH_CLIENT_SECRET` are also accepted for
users migrating an existing youtui setup. Command-line flags are available for
one-off use; environment variables are preferred.

## Dedicated browser login

For playback-compatible browser authentication, run:

```text
unified-player youtube browser-login
unified-player youtube status --check
```

The login command waits for the dedicated profile to become signed in and then
imports the session automatically. There is no separate "press Enter" step.

The command opens only the profile stored under the application's YouTube
configuration folder. Closing or deleting that folder signs the application out
without changing the user's normal browser profile. The exported cookie header
and remembered browser path are written atomically with restricted permissions
where the operating system exposes Unix-style file modes.

Google Chrome is not required specifically. Discovery includes Helium alongside
Chrome, Chromium, and Microsoft Edge. Welcome shows the detected executable and
offers **Choose browser path** and **Retry browser detection**; selecting a path
applies immediately. The CLI also accepts `--browser`. A selected executable
must support the dedicated-profile DevTools login flow; being executable alone
does not establish compatibility. No browser is downloaded automatically.

### Sign in from Welcome without a detected Chromium browser

Choose **Import cookies (other sign-in option)** on the YouTube step. Its guide
and local file input work even when no compatible browser is installed:

1. In your already signed-in browser, open `music.youtube.com`. Open Developer
   Tools, select Network, reload, and select a `youtubei` request. Under request
   Headers, copy the **Cookie** value into a local UTF-8 text file. Firefox's
   [Network request details](https://firefox-source-docs.mozilla.org/devtools-user/network_monitor/request_details/)
   describe the request headers view. A Netscape-format YouTube cookie export
   is also accepted; a JSON export or a whole HAR capture is not.
2. Enter the file's path in Welcome and activate **Import** (or press Enter).
   Do not paste cookie contents into the path field. Files are limited to 64 KiB;
   import requires signed-in YouTube cookies including SAPISID and SID or
   LOGIN_INFO. Keep exported credentials private and delete the export after
   successful import if it is no longer needed.
3. Import verifies account/library access before saving the replacement and
   applies the authenticated client immediately. It does not launch a browser.
   A failed check retains the prior saved credentials and offers retry. While
   verification runs, **Cancel sign-in / import** cancels it. Browser sign-in
   uses the same cancellation action; closing its window also cancels login.

Welcome distinguishes browser availability from saved sign-in and account
verification. A failed account check blocks Continue until resolved or explicitly
skipped. **Check account & playback** checks account access and native playback
as separate results. Successful cookie import proves account access, not playable
audio: some playback paths still require the dedicated browser profile. Imported
cookies may expire; use a fresh export when account access fails.
