# Support and Feature Matrix

Last updated: 2026-10-04

What works today, on which service and platform, and how sure we are. This
table only lists what exists; it is not a roadmap.

## Evidence Labels

| Label | Meaning |
| --- | --- |
| Verified | Covered by tests and tried by hand with a real account on at least one platform |
| Implemented | The code is there and tested, but not yet tried by hand on every platform |
| Experimental | Relies on an unofficial or fragile service interface and may break without notice |
| Unsupported | Deliberately not offered |

## Provider Capabilities

| Capability | Spotify | YouTube Music | Evidence and boundary |
| --- | --- | --- | --- |
| Authentication | Verified: Web API PKCE; a second cached librespot session when integrated streaming is compiled | Experimental: dedicated browser session is the playback-capable path; OAuth remains useful for metadata but is provider-restricted for direct playback | Auth parser tests, live browser contracts, `docs/youtube-auth.md` |
| Library and contexts | Verified for playlists, saved albums/shows/tracks, followed artists, and normal contexts; browse categories currently return empty because Spotify removed those endpoints | Implemented for playlists, albums, artists, playlist/album/artist contexts, and liked songs/videos; liked-item media kind is recovered from the provider response, while broader live parity evidence remains pending | Provider read routes, typed liked-media parser test, and authenticated library probe |
| Search | Verified | Verified | Search tests plus manual testing of fast, repeated searches |
| Playback location | Verified remote Spotify Connect; verified local playback when `streaming` is compiled | Verified on Windows; on Linux (2026-10-03) the media request was refused with `media_forbidden` | Playback tests; manual playback on Windows and Linux |
| Pause, resume, next, previous | Verified | Verified | Command tests and manual testing on Windows |
| Seek and progress | Verified | Verified; an exhausted local sink may require a same-track source restart | Seek tests and manual testing on Windows |
| Volume and mute | Verified where the active Spotify device exposes volume | Verified for the local player | Command tests and manual testing on Windows |
| Repeat and shuffle | Verified | Verified through the unified queue | Queue tests |
| Queue and mixed-provider ordering | Verified | Verified | Queue tests and manual switching between services |
| Likes and playlist mutation | Verified Spotify library and playlist mutations | Experimental rating and playlist create/add/remove/delete through unofficial endpoints | Mutation routing tests; provider API limitations remain external |
| Lyrics | Verified native Spotify lyrics with SimpMusic/LRCLIB/Musixmatch fallback when matching metadata exists, plus opt-in Lyrics.ovh plain lyrics | Experimental SimpMusic/LRCLIB/Musixmatch fallback, plus opt-in Lyrics.ovh plain lyrics, keyed to the active YouTube item | Parser, fallback-order, source retry/cycle, and provider-allowlist tests; external providers remain best-effort |
| Cover art | Implemented; rendering requires an image feature | Implemented; rendering requires an image feature | Presentation snapshot tests |
| OS media controls | Implemented for Spotify when `media-control` is compiled and enabled | Implemented when `media-control` is compiled: metadata, play/pause, seek, and volume; media keys not yet tried by hand | Media-control tests |
| Offline playback | Unsupported | Unsupported | Both providers require network access |

## Platform and Build Support

| Surface | Windows | Linux | macOS |
| --- | --- | --- | --- |
| Interface, Spotify library, and switching services | Verified | Verified (2026-10-03) | Implemented; builds and passes tests in CI (2026-10-04), not tried by hand |
| Spotify playback on this computer | Verified with `rodio-backend` | Verified with `rodio-backend` (2026-10-03) | Implemented |
| YouTube Music playback | Verified | Failing: `media_forbidden` (2026-10-03) | Implemented |
| Media controls | Verified for Spotify when explicitly enabled; disabled by default because Winit can affect focus | Implemented through MPRIS, on by default; not yet tried by hand | Implemented for Spotify; disabled by default because Winit can affect focus |
| Dedicated YouTube browser | Verified with Chrome-family browser discovery and cleanup | Implemented | Implemented |
| Daemon | Unsupported | Implemented when the `daemon` feature is compiled | Implemented only without the incompatible media-control event-loop combination |

## Credential-Safe Diagnostics

Run the local report without contacting either provider:

```console
unified-player diagnostics
```

It reports the application version, platform, compiled features, selected
audio backends, configured startup provider, coarse authentication readiness,
and whether a dedicated browser session is configured. It omits credential
values and locations, client identifiers, URLs, track metadata, provider
responses, and log contents.

When a TUI is already running, request its coarse runtime state without
starting a client or authenticating:

```console
unified-player diagnostics --live
```

The live report adds only the active provider, playing/paused/none state, and
running/shutdown-requested lifecycle state. A missing TUI is an explicit error;
the diagnostic command never starts a hidden client as a fallback.

Provider-specific authenticated probes remain under `youtube status --check`.
Those commands are intentionally separate because they perform network and
browser work rather than producing a shareable static support report.

## Private Provider Forensics

| Surface | Status | Evidence and boundary |
| --- | --- | --- |
| Build availability | Implemented, off by default | Only in builds with the `private-capture` feature; regular builds and release binaries do not include it |
| Provider coverage | Experimental: YouTube Music manual foreground playback | Spotify, library/search mutation, generalized HTTP interception, and HAR import are out of scope |
| At-rest protection | Implemented | Passphrase-encrypted age-compatible artifacts, current-user-only permissions, bounded retention/quota, atomic no-overwrite publication |
| Replay | Implemented | Offline parser/selector replay is network-free; confirmed fresh replay makes one allowlisted current-credential request; neither produces audio or state mutation |
| Comparison | Implemented | Bounded typed working/failing and original/replay comparison; private detail stays encrypted and only registered categories reach the safe view |
| Sanitized derivative | Implemented | Canonical three-file allowlist with checksums and fail-closed forbidden-data scan; it is not an ordinary support bundle |
| Derivative preview/review copy | Implemented | Preview of the scanned three-file derivative in the app or CLI; only an allowlisted review summary can be copied |
| In-app controls | Implemented | One Diagnostics row with a masked passphrase and explicit confirmations; private capture contents never appear in the interface |
| CLI | Implemented | `youtube debug-capture`; terminal-only passphrase and explicit network/sensitive/destructive acknowledgements |
| Masked private inspector | Implemented, human-only | Separately acknowledged `inspect` authorizes interactive stdout before decryption, masks credential/signed URL values, labels normalized JSON, withholds opaque bodies, rejects redirection, and provides no raw/byte-exact mode |
| Automated tests | Implemented | Feature, security, permission, and lifecycle tests; they pass in CI on Windows, Linux, and macOS (2026-10-04) |
| Manual testing | Pending | Not yet tried by hand end to end on any platform |

The feature stays off by default. Raw encrypted
captures and terminal-inspector text are private and must never be attached to
ordinary support requests. Only a reviewed derivative with valid checksums and
a passed forbidden-data scan is eligible for deliberate sharing.

### YouTube Playback Failure Categories

| Category | Meaning and first action |
| --- | --- |
| `authentication` | The playback identity was rejected; refresh the configured sign-in method |
| `consent_age_region` | Provider policy blocks the item for this account or region; try an eligible item/account rather than retrying blindly |
| `provider_unavailable` | The item or required playback source is unavailable; try another item |
| `proof_token` | A required playback proof token is missing or rejected; refresh the dedicated browser session |
| `decipher` | The current player contract cannot decipher the media URL; update or gather a sanitized transport diagnostic |
| `rate_limited` | The provider asked the client to slow down; wait before retrying |
| `network` | Transport failed without exposing the URL; check connectivity and retry |
| `cancelled` | Newer work or shutdown intentionally superseded the request; no repair is needed |
| `contract` | A provider response no longer matches the implemented contract; update the provider adapter using sanitized evidence |
| `unsupported_format` | No compatible native audio format was offered; try another item or audio backend |
| `media_forbidden` | The playback source resolved but the provider refused its media request; try another item and gather transport evidence if it repeats |
| `media_range_contract` | The provider returned an invalid ranged-media response; retry once and review media transport diagnostics if it repeats |
