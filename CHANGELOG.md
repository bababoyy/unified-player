# Changelog

This project is in pre-release development. Entries describe repository and
user-facing changes that are ready to be evaluated; they are not a promise of
binary compatibility or a published release cadence.

## Unreleased

- Clarified the provider-neutral product description and support boundaries.
- Replaced inherited upstream installation instructions with source-build
  guidance for this checkout.
- Linked the support, configuration, contribution, and security documentation
  from the first-visit README flow.
- Kept YouTube Music behavior explicitly experimental where it depends on
  unofficial APIs or browser-session playback.
- Kept the integrated Spotify device usable while the Spotify Web API is rate
  limited: it stays in the device list, and playback controls on it go through
  Spotify Connect instead of the Web API.
- Reduced Spotify Web API reads: controls on the integrated device no longer
  trigger a playback read, the periodic read while it plays slowed to 60 s, and
  reopening Liked Songs reads one page instead of the whole library when it
  has not changed.
- Track changes on the integrated device are shown from the player's own
  metadata instead of a Web API playback read, and the Spotify queue is only
  read automatically while it is visible.
- Started YouTube Music tracks faster: tracks up to 32 MiB are fetched whole
  and decoded from memory instead of one range request per MP4 fragment, and
  the public client material a first playback needs is fetched at startup.

## Release policy

Release binaries, package-manager formulas, container images, and crates.io
packages will be listed here only after project identity, ownership, signing,
and provenance are approved. See [publication readiness](docs/publication-readiness.md)
for the current gate.
