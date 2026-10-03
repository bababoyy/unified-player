# ADR 0003: Local Diagnostic Protocol

Date: 2026-07-29

Status: Accepted

## Context

The application needs timelines, health facts, incident evidence, and support
artifacts that remain useful at maximum supported verbosity without exposing
credentials or private listening activity. Human formatting strings, complete
Rust values, and third-party logging contracts cannot provide that guarantee.

The protocol also needs stable semantics across the terminal UI, JSONL files,
live CLI diagnostics, incident records, and future support bundles.

## Decision

1. Diagnostic schema version 1 is the first compatibility surface.
2. Application events use a registered name/code pair, bounded component and
   severity enums, random run/trace/span identifiers, monotonic durations, and
   explicitly typed optional fields.
3. Event constructors accept static registered names/codes. The tracing bridge
   is a narrow compatibility adapter that admits only allowlisted fields and
   converts all other fields to absence.
4. Credentials, cookies, request/response bodies, URLs, queries, titles,
   artists, lyrics, playlist names, media IDs, clipboard content, full
   configuration, paths, and arbitrary error prose are forbidden.
5. JSONL is the machine-readable persistent format. Each line is one complete
   schema-versioned JSON object.
6. The UI ring stores structured entries and renders a separate concise human
   line. Human rendering is not the serialization contract.
7. Files rotate at 10 MiB or a UTC-day boundary and are retained for at most
   seven days and 50 MiB total. Only files owned by the diagnostics filename
   namespace participate in retention.
8. The non-blocking writer is loss-tolerant. Queue saturation or write failure
   increments a visible dropped-event counter and degrades writer health;
   diagnostics failure must not block playback or the UI.
9. No diagnostic data is exported automatically. Support artifacts require an
   explicit local command and review step.
10. Backtraces are excluded from support bundles.
11. Registered typed causality, lifecycle, health, incident, and performance
    events are always admitted. The dynamic severity filter applies only to
    generic events entering through the `tracing` compatibility adapter; UI
    and CLI wording must identify that scope.

## Compatibility

- Adding an optional field is backward compatible within schema version 1.
- Adding a registered event is backward compatible.
- Removing or renaming a field, event, outcome, or enum value requires either a
  compatibility reader or a schema-version increment.
- A code's meaning, component ownership, privacy class, and required fields
  cannot change silently.
- Human UI wording may improve without a schema change when code and semantics
  remain stable.
- Consumers must ignore unknown optional fields and reject unsupported major
  schema versions.
- Support bundle manifest versioning is independent of the event schema. File
  removal, checksum-policy changes, or privacy-boundary changes require a new
  manifest version; adding an optional manifest fact does not.
- Incident prose is presentation, while its event code, cause class,
  retryability, outcome, reference, and component are compatibility fields.
- Typed interactive row IDs, action availability, acknowledgement, selection,
  follow state, and popup wording are local presentation contracts, not JSONL
  schema fields. They may evolve without a schema increment while preserving
  privacy, stable event meaning, and the no-playback-control boundary.
- Operation timelines and health transition histories must be built from typed
  in-memory events. Reconstructing them by scraping JSONL is not compatible
  with the interactive console contract.
- The default-off private provider forensic protocol accepted in ADR 0004 is
  not an extension of schema version 1. Its encrypted record, replay,
  comparison, and derivative schemas are independently versioned. A safe
  in-memory private-capture status row adds no JSONL event or support-bundle
  field.

## Consequences

Diagnostics become locally inspectable and machine-testable without making
private activity part of the protocol. Some upstream error detail is
deliberately unavailable; provider adapters must classify it at the boundary
instead of preserving raw prose. The event registry and golden schema tests now
require deliberate review when the protocol evolves.
