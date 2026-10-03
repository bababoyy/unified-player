# ADR 0006: Playback Control Plane Ownership

Status: Proposed maintenance reference

Date: 2026-08-30

Provider composition, identity, accounts, capabilities, and cross-domain action
ownership are governed by ADR 0007. This ADR governs only the playback control
plane within that provider-platform contract.

## Purpose

This is the current reference for how playback intent, scheduling, provider
activation, endpoint choice, queue continuation, state publication, and UI
feedback should fit together. ADR 0001's safety invariants remain valid. ADR
0002 established the coordinator as the owner of provider transitions, but the
runtime subsequently accumulated queue and endpoint decisions outside that
boundary. This ADR refines that ownership model; it does not claim the target
has already been implemented.

The maintenance objective is not a new playback framework. It is fewer owners,
fewer state mirrors, and fewer provider-specific branches at call sites.

## Evidence Snapshot

The 2026-08-30 18:42:11 diagnostics run confirms:

- a YouTube Music request used the native VISIONOS route; source resolution
  took 1.296 seconds, decoding took 4.391 seconds, and request completion took
  5.764 seconds;
- both recorded YouTube seeks completed successfully in 10 ms or less;
- YouTube playback ended at 68.501 seconds, the visible provider became Spotify
  50 ms later, and Spotify player events arrived 606 ms after the end event;
- the successful queue handoff had no request envelope, provider-neutral trace,
  route stage, command acknowledgement, or playback-ready terminal event; and
- YouTube search logged an internal provider failure while the scheduler still
  reported the request as successful.

The immediately preceding 18:41:01 run confirms a different failure mode:

- Spotify API retries were active;
- a pause toggle occupied playback coordination for 5.265 seconds before being
  superseded;
- the next toggle waited 5.064 seconds, then was rejected because a transition
  was in progress; and
- ten Spotify play attempts failed in 62-73 ms with only the broad
  `unavailable` classification.

The latest handoff therefore succeeded, but the logs cannot prove which
Spotify endpoint accepted it or when audio became ready. Endpoint choice is an
inference from the checked-out code, not a verified log fact.

## Current Runtime Shape

```text
terminal / media key / socket / provider event
                    |
             ClientRequest scheduler --------------+
                    |                               |
          foreground request task                   | background events bypass it
                    |                               |
          PlaybackCoordinator                       |
      transition lock + activation generation       |
                    |                               |
       caller-supplied start closure                 |
          /                     \                    |
 Spotify SPIRC/Web API       YouTube resolver        |
          \                     /                    |
     PlayerState projections + provider sessions <--+
                    |
       separately mutable UI active_provider
```

The pieces are individually useful. The bloat comes from overlapping policy:

| Concern | Current owners | Consequence |
| --- | --- | --- |
| Request ordering | `request_scheduler.rs`; background provider monitors | Queue continuation can bypass delivery policy and request cancellation. |
| Provider transition | `playback_coordinator.rs`; caller-supplied start closures | The coordinator serializes activation but does not fully own endpoint execution or readiness. |
| Queue advancement | YouTube monitor in `playback_actions.rs`; Spotify event loop in `streaming.rs`; foreground next/previous handlers | The same queue transition has several entry points and asymmetric tracing. |
| Spotify endpoint choice | `AppClient` SPIRC helpers; coordinator adapter; legacy player request/Web API path | Local versus remote authority is rediscovered at multiple call sites. |
| Playback truth | coordinator state; Spotify API snapshot; buffered snapshot; YouTube snapshot/phase; provider sessions; UI `active_provider` | A command can be accepted and displayed before audible readiness, while stale projections still influence controls. |
| Failure policy | `anyhow` chains; broad diagnostic category; resilient queue loop | A provider/control failure can be treated as an unavailable media item and skipped. |
| UI completion | generic scheduler descriptors plus provider-side state writes | `NoOp`, semantic provider failure, rejection, command acceptance, and playback readiness do not share one lifecycle. |

The approximate size of the central path is also a maintenance signal, not a
defect by itself: `playback_actions.rs`, `playback_coordinator.rs`, and
`request_scheduler.rs` together contain about 5,900 lines and 221 functions
across the playback/scheduling modules reviewed here. A new abstraction is
valuable only if it deletes ownership decisions from these paths.

## Confirmed Contract Violations

### Queue continuation has no single owner

The YouTube monitor advances `UnifiedQueue` and calls
`play_unified_item_resilient`. The librespot `EndOfTrack` handler independently
does the same. Foreground next/previous handlers form a third entry path. The
queue data structure is shared, but the transition operation is not.

### Visible provider is not a playback-ready fact

`prepare_activation` writes `active_provider` before the target start future
runs. The integrated Spotify path returns after SPIRC `activate` and `load`
accept the command; it does not await a matching `Playing` event. In the latest
run the UI changed provider 606 ms before the first subsequent Spotify player
events. This conflicts with ADR 0001's ordering language that publishes the new
active-provider snapshot after incoming activation.

### Cancellation has three scopes

The scheduler cancels or supersedes foreground request tasks. The coordinator
owns an activation token. YouTube prefetch/monitor work and librespot event-loop
continuation have their own lifetimes. Background queue continuation can call
the coordinator without passing through a scheduler envelope, and the Spotify
path currently passes no reserved activation permit.

### Retry and skip policy is untyped

`play_unified_item_resilient` skips to another queue item after every error,
defaulting unknown errors to `Unavailable`. Only an actual item-unavailable
result justifies automatic skipping. Rate limiting, endpoint loss, transition
conflict, cancellation, authentication, and stale work require different
outcomes.

### Request terminal outcomes are not end-to-end outcomes

The scheduler correctly distinguishes success, rejection, supersession, and
failure for work that it owns. Provider handlers can still absorb an internal
failure and return success, while a background handoff may have no scheduler
terminal event at all. `NoOp` is mapped to rejection without a reason, and only
active play/pause control rejections currently receive specific UI guidance.

## Target Ownership

Keep the current major building blocks and make their boundaries narrower:

1. **The scheduler owns ingress and delivery policy.** Every foreground intent
   and every provider completion that may advance the application queue enters
   one contextual request envelope. Event sources emit facts; they do not
   execute the next item.
2. **The coordinator owns one playback generation.** It serializes provider
   transition, cancellation, queue handoff, and state publication for that
   generation. No event loop calls provider start directly.
3. **`UnifiedQueue` owns queue data, not async execution.** Exactly one queue
   driver operation may mutate position for a completion generation. Duplicate
   or stale completion facts are rejected before mutation.
4. **A provider adapter owns its endpoints.** The Spotify adapter chooses local
   integrated SPIRC or remote Connect/Web API from an explicit target policy.
   Callers ask Spotify to start an item; they do not implement fallback.
   YouTube resolution remains inside the YouTube adapter.
5. **Command acceptance and playback readiness are different facts.** Provider
   start returns an acknowledgement, then a generation-matched provider event
   confirms ready, failed, or timed out. UI may show switching after command
   acceptance, but may show running only after readiness.
6. **One coordinator snapshot is the UI authority.** Provider API, buffered,
   YouTube, and persisted-session values remain provider projections. They do
   not independently select the active owner.
7. **Failures are typed at the provider boundary.** At minimum distinguish
   item unavailable, endpoint unavailable, rate limited, authentication,
   cancelled, stale, and internal failure. Only item unavailable is
   automatically skippable.

Conceptually:

```text
PlaybackIntent or ProviderEvent
            |
   contextual scheduler envelope
            |
  PlaybackCoordinator / QueueDriver
            |
 ProviderAdapter::start(item, target, generation, cancellation)
            |
        CommandAccepted
            |
 generation-matched Ready | Failed | Ended
            |
 coordinator snapshot -> UI and persisted projections
```

`QueueDriver` names a responsibility, not necessarily a new long-lived object
or task. It may begin as coordinator-owned methods. Do not add a second channel
or state machine merely to match the diagram.

## Required Invariants

1. At most one playback generation may own audible output.
2. Exactly one owner advances the unified queue for a generation.
3. A provider event must identify and match the current provider, media item,
   and generation before it can advance the queue or publish readiness.
4. Scheduler cancellation and coordinator cancellation must reach the same
   nested provider work.
5. Provider selection and endpoint selection must not be duplicated at call
   sites.
6. `running` means playback-ready; `switching` and `command accepted` are not
   aliases for it.
7. Only `ItemUnavailable` may trigger automatic queue skip.
8. Every logical transition has one operation context and one terminal outcome,
   including background handoffs.
9. A stale or rejected operation cannot mutate queue position, playback owner,
   provider projections, persistence, or UI success state.
10. A migration step must remove an old execution path or state-writing owner;
    adding a parallel path is not completion.

## Migration Order

### 1. Provider-neutral queue handoff

Replace direct continuation from both provider event loops with one contextual
queue-advance request owned by the coordinator. Carry one generation through
queue mutation and nested provider start. Preserve current queue data and
provider playback implementations.

Deletion gate: the YouTube monitor and librespot event loop no longer call
`play_unified_item_resilient` or mutate `UnifiedQueue` position directly.

### 2. Typed start failure

Make provider start return a bounded failure class and allow the queue driver
to skip only `ItemUnavailable`. Surface transient endpoint/rate-limit failures
as retryable playback failure without consuming another queue item.

Deletion gate: the resilient loop no longer defaults arbitrary errors to
`Unavailable`.

### 3. Readiness and visible state

Separate command acceptance from generation-matched playback readiness and
derive the UI provider state from the coordinator snapshot.

Deletion gate: `prepare_activation` no longer publishes `running`, and event
handlers no longer use a separately mutable UI provider as playback truth.

### 4. Endpoint ownership consolidation

Move Spotify local/remote route choice behind the Spotify adapter and remove
the duplicate SPIRC/Web API choice from caller-supplied start closures and
legacy control paths where the coordinator now owns policy.

Deletion gate: one provider adapter is the only module selecting a Spotify
playback endpoint.

Each step is a separate maintenance goal with focused tests and no adjacent
timeout, resolver, rate-limit, or broad UI rewrite.

## First Implementation Goal

`unified-queue-single-handoff-owner-01`

Implementation status (2026-08-30): implemented; manual mixed-provider
acceptance pending. Both provider event sources now publish one queue-occurrence
completion intent to the client scheduler. The deletion gate in step 1 is met;
the remaining migration steps are unchanged.

Observable outcome: both YouTube and Spotify end-of-track facts submit the same
provider-neutral, generation-aware queue continuation operation; the operation
has one trace and terminal result, and a duplicate/stale completion cannot
advance the queue twice.

In scope:

- one provider-neutral handoff request and operation context;
- coordinator-owned generation validation and queue mutation;
- cancellation propagation into the nested provider start;
- regression tests using fake provider events and engines; and
- deletion of direct continuation from both event loops.

Out of scope:

- timeout tuning, Spotify rate-limit changes, endpoint fallback changes,
  readiness/UI redesign, EJS/YouTube resolver work, and broad error taxonomy.

This slice is first because it removes duplicate execution ownership. Failure
typing and readiness become smaller changes once every handoff uses one path.

## Consequences

- Some current behavior will remain imperfect between migration steps, but
  each step reduces the number of owners.
- Existing provider projections and queues can remain while their authority is
  narrowed; a risky state-model rewrite is not a prerequisite.
- Diagnostics become simpler because background and foreground transitions use
  the same operation envelope.
- Manual acceptance remains necessary for audible readiness and phone/remote
  Spotify behavior, but fake-engine tests can prove ordering, cancellation,
  stale rejection, and single advancement without audio output.
