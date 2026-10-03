# Diagnostics and Privacy

`unified-player` diagnostics are local, bounded, and designed for review before
sharing. The application does not send telemetry or export diagnostic data.

## What Users See

The Diagnostics view shows:

- component and worker health;
- whether an operation is queued, running, completed, superseded, or failed;
- the most recent operation outcome and a short random reference;
- diagnostic-writer state and dropped-event count;
- bounded UI mode; and
- recent incident summaries with impact, cause class, retryability, one next
  action, event code, incident reference, and component health.

Move through the rows with the normal list and page navigation keys. On a
selected row, `g a` or `Ctrl+Space` opens only the actions valid for that row.
Diagnostic action labels start at `[1]`; pressing the displayed digit invokes
that action, while arrow/page keys and Enter remain available.
The popup can show bounded incident details, correlated operation timing,
component or worker transition history, logging/filter state, support review,
and local performance evidence. Acknowledging an incident affects only its
appearance for the current run; it does not delete evidence.

The operation section shows the eight most recent operations and keeps an
explicitly followed operation visible through unrelated completions until its
terminal outcome can be inspected. Diagnostic text is normalized before it
enters the UI model, bounded again for display, and allowlisted separately for
clipboard output.

Loading or empty component, worker, operation, and incident sections appear as
selectable state rows with a bounded explanation. A failed local diagnostic
action shows its safe incident reference, impact, cause class, retryability,
next action, and Support health without showing the source error.

Diagnostics is not a playback or lifecycle controller. It cannot retry or
cancel requests, restart workers, kill a browser, reset audio, switch a
provider, authenticate, or send a playback command.

Routine success, cancellation, and supersession do not create incidents. Raw
provider responses and Rust error chains are never shown in the view. A
handled request's specific safe failure and terminal outcome share one short
reference and appear as one incident rather than duplicate summaries.

## Local Reports

`unified-player diagnostics` prints static build and configuration-mode facts.
`unified-player diagnostics --live` adds bounded state from an already-running
application. Neither command authenticates a hidden client or contacts a
provider solely for diagnostics.

Create and verify a local support bundle with:

```text
unified-player diagnostics --bundle <empty-folder>
unified-player diagnostics --review-bundle <folder>
```

The review command verifies the manifest policy, each SHA-256 hash declared by
the manifest, the standalone checksum file, file size limits, and the
forbidden-data scan. Review the four text files before sharing them. Backtraces
are excluded. Bundles are never uploaded automatically.

For a bounded reproduction session, an already-running application can enable
verbose trace-level logging for 1-300 seconds:

```text
unified-player diagnostics --live --verbose-seconds 30
```

The previous verbose-tracing threshold is restored automatically. This filter
applies only to generic events admitted through the `tracing` compatibility
layer. Registered typed causality, lifecycle, health, incident, and performance
events remain on at every filter level so an operation timeline cannot acquire
gaps when verbosity changes. The Diagnostics view and live report label these
two streams separately, show whether verbose tracing is temporary, and show its
remaining time. The interactive Filter row offers fixed 15, 30, and 60 second
windows and an immediate stop action.
If diagnostics were disabled at startup with `RUST_LOG=off`, verbose tracing is
reported as unavailable instead of claiming that a compatibility layer was
enabled. Core typed in-memory causality remains available for that run.

An isolated performance-budget sample remains under the Performance row. It is
promoted to an incident only after three breaches of the same component budget
within five minutes, or for one material breach lasting at least both four
times its budget and five seconds. Promotion occurs once per bounded window;
budget evidence never cancels playback or application work.

A handled warning with a safe failure category is not, by itself, an incident.
It must carry an explicit failed terminal outcome (or be emitted at error
severity). This prevents a recovered fallback, such as skipping one unavailable
queue candidate and successfully playing the next, from remaining visible as a
false operation failure.

`unified-player diagnostics --trend` compares p95 timings in the newer and
older halves of retained local samples. It prints only stable codes, counts,
durations, and deltas. It does not contact a service or create a remote metric.

## Data Boundary

Allowed evidence includes build revision/dirty state, platform, run and random
correlation references, registered event codes, component/provider kinds,
bounded lifecycle states, outcomes, durations, counters, and safe cause
categories.

The following are excluded from JSONL, the UI, incident summaries, and support
bundles:

- credentials, cookies, authorization material, and browser state;
- URLs, headers, request/response bodies, and provider payloads;
- paths and configuration values;
- queries, titles, artists, lyrics, playlist names, and media identifiers;
- clipboard/key content; and
- raw errors, panic payloads, and backtraces.

Panic evidence contains only a stable fingerprint derived from the source
location, a short incident reference, and a fixed privacy notice.

Clipboard actions never read the clipboard. They construct a new allowlisted
incident, health, short-reference, bundle-review, or bounded-trend string and
write only that value. Folder opening keeps the generated path in private
runtime state; the path is not rendered, copied, logged, or included in the
bundle.

## Private Developer Capture

The default-off `private-capture` build feature is a separate trust product,
not a more verbose diagnostics mode. It supports explicit, one-shot provider
investigations where a developer needs exact request and response evidence.
Its models, queue, encrypted writer, vault, replay, comparison, derivative, and
passphrase handling live outside `observability`. Normal JSONL, incidents,
clipboard actions, panic reports, and ordinary support bundles cannot read its
storage. The feature adds no raw diagnostic event.

A private capture is never armed automatically. One accepted warning and one
in-memory passphrase can authorize only the next matching manual foreground
operation for 60 seconds. Background work, prefetch, resume, probes, replay,
Spotify work, and authentication refresh cannot consume that arm. Queue,
encryption, permission, quota, or writer failure marks the capture incomplete
without changing playback.

Artifacts use an age-compatible passphrase-encrypted binary stream, random
filenames, an authenticated manifest committed last, current-user-only
permissions, atomic no-overwrite publication, a five-artifact/100 MiB quota,
and 24-hour retention. No plaintext temporary file is created. Media body bytes
are never eligible for capture. The manifest records build revision and dirty
state, platform, effective limits, completeness, dropped records, terminal
category, and a record-stream checksum.

Raw captures can contain private activity, provider payloads, signed URLs,
headers, and credential-class values. They are therefore never shareable as an
ordinary support artifact. A sanitized derivative is a separate, canonical,
allowlist-built report, not redacted raw text. It contains only registered
enums, coarse HTTP classes, bounded counts and timing buckets, fixed findings,
schema/revision/completeness facts, and checksum/scan results. It excludes
capture references, paths, identifiers, URL/query/header values, provider
prose, payloads, credentials, titles, artists, lyrics, and body hashes.

Derivative creation fails closed if typed input is unsupported or incomplete
beyond its contract, output already exists, the destination overlaps protected
application roots, publication cannot be validated, checksums fail, an unknown
field appears, or the seeded forbidden-data scanner cannot prove its result.
The scanner checks bounded private values and their raw, JSON-escaped,
percent-encoded, standard Base64, and URL-safe Base64 representations. The
three-file derivative remains separate from ordinary support bundles in
version 1.

### Private Capture In The TUI

In an enabled build, open Diagnostics with `g o`, select the `Private capture`
row, and use `g a` or `Ctrl+Space`. The row shows only safe state,
completeness, bounded counts/size, selected short reference, last action,
registered comparison categories, replay outcome, and derivative review
status.

Arming requires a sensitivity explanation and a masked in-memory passphrase.
Fresh replay requires two `y` confirmations before passphrase entry. Numeric
action shortcuts cannot confirm fresh replay, folder opening, or deletion.
Opening the encrypted folder and deleting a capture require their own
confirmations. Offline replay sends no network request; fresh replay makes one
allowlisted current-credential request and produces no audio or playback-state
change.

Preparing a derivative retains one bounded, already-scanned three-file preview
in memory. `View derivative preview` renders those exact file contents without
re-reading a destination directory. `Copy derivative review` constructs a new
allowlisted Tier 3 summary and writes only that text; it never reads or
transforms existing clipboard content. Neither action exposes the private
capture.

### Private Capture CLI

Enabled builds expose `unified-player youtube debug-capture`. `status` and
`list` do not decrypt artifacts. `review`, `replay`, `compare`,
`sanitize-preview`, `sanitize`, and `inspect` prompt for the passphrase through
an interactive terminal and reject redirected input. Fresh replay additionally
requires `--acknowledge-network`; folder opening and private inspection require
`--acknowledge-sensitive`; deletion requires `--acknowledge-delete`.

Normal CLI output contains only fixed labels, short random references, counts,
buckets, registered categories/outcomes, review results, or the scanned Tier 3
derivative. It never prints paths, private provider evidence, or error chains.
The sole private-output exception is `inspect`. It checks interactive stdout
before artifact decryption and renders a bounded catalog or one masked Tier 2
record directly to the terminal. Credential and signed URL values are masked,
JSON bodies are explicitly normalized, and opaque bodies are withheld. There
is no redirected, file, clipboard, UI, log, socket, raw, or byte-exact output
mode. Inspector text remains private and must never be pasted into an issue or
acceptance report. See `docs/private-capture-operator-guide.md` for the complete
command and operator workflow.

The feature remains default-off after the Phase 7H review. This preserves an
explicit sensitivity boundary while live cross-platform evidence and the
documented same-user filesystem namespace race remain. Exact credential replay
and automatic derivative inclusion in ordinary support bundles remain out of
scope.

## Retention

Structured JSONL files rotate at 10 MiB or a UTC day boundary. Files in the
diagnostics-owned namespace are retained for at most seven days and 50 MiB.
Legacy logs and panic files pre-dating this policy may contain sensitive data;
do not attach them to reports or include them in support bundles.

The separate encrypted private-capture vault retains at most five artifacts,
100 MiB total, and 24 hours of age, including encrypted replay/comparison child
artifacts. Sanitized derivative directories are outside the vault and are
operator-owned after creation; they are not automatically purged. Review and
delete them deliberately after the investigation.
