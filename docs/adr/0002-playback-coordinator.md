# ADR 0002: Single Playback Coordinator

Status: Accepted
Date: 2026-07-27

## Context

Phase 1 characterized provider switching through a production runtime trait,
but coordination ownership was still distributed. `AppClient` held separate
generation counters and cancellation tokens, direct playback paths paused the
opposite provider themselves, the event layer captured sessions and changed
the visible provider before the backend transition, and shutdown bypassed
playback persistence.

That arrangement could test individual orderings but could not make concurrent
activation safe by construction. Two independently spawned client requests
could each believe they were entitled to activate an engine.

## Decision

`client/playback_coordinator.rs` is the sole owner of shared playback policy.
It contains:

- one typed `PlaybackState` value: idle, one active provider, one transition
  with at most one still-active source, shutting down, or stopped;
- one serialized transition lock;
- one last-writer-wins activation generation and cancellation slot;
- activation permits reserved in client-channel receive order before request
  tasks are spawned, with the same permit guarding queue mutation;
- coordinator-owned YouTube prefetch cancellation and Spotify refresh
  generations;
- the only production calls that remember and persist provider sessions; and
- Spotify and YouTube adapters implementing the same narrow `PlaybackEngine`
  contract.

The existing `ClientRequest`, `PlayerRequest`, `YouTubePlayerRequest`, command,
and UI surfaces remain intact. They delegate activation and controls instead of
publishing transition state themselves.

## Transition Rules

For a provider switch, the coordinator:

1. supersedes older activation work;
2. remembers and persists the outgoing session;
3. deactivates the outgoing engine;
4. publishes the target provider only after outgoing deactivation;
5. resumes the target according to its saved-session policy; and
6. commits the single active state and persists the incoming snapshot.

For new playback, the coordinator performs the same outgoing deactivation and
then runs the provider-specific start future under a cancellation ticket. A
newer request cancels the older future. If an older start completes after it is
superseded, its engine is deactivated before the newer target commits.

Spotify control commands publish their acknowledged playing/paused state to
the live, buffered, and session projections immediately. Delayed API refreshes
still reconcile remote state, but an eventually consistent read cannot veto or
immediately reverse an explicit user command.

Explicit pause and resume requests are accepted only for the active provider.
Other controls are rejected while their provider is inactive, preserving the
no-overlap behavior of provider-specific media-control input.

## Shutdown

Quit sends `ShutdownPlayback` through the existing client request channel. The
UI waits for coordinator completion before restoring the terminal and exiting.
The coordinator cancels pending work, persists the active session, preserves
Spotify Connect's existing remote-playback behavior, stops local YouTube audio
and browser resources, and enters the terminal `Stopped` state. Full worker
joining remains Phase 5 runtime-supervisor work.

## Failure Behavior

- Outgoing deactivation failure leaves the source logically active and never
  resumes the target.
- Incoming resume failure leaves no engine logically active.
- A failed same-engine replacement restores the prior logical active state.
- Work completing with an obsolete generation cannot commit active state.
- Activation after shutdown is rejected.

## Consequences

- `AppClient` remains the composition/API facade and implements provider
  mechanics, but no longer owns shared transition policy or work generations.
- Provider session data structures remain in `PlayerState`; the coordinator is
  the only production owner that decides when to remember, refresh, or persist
  them.
- The event layer requests a switch but does not claim it succeeded early.
- Adding another provider requires an adapter and an explicit state-machine
  policy rather than another set of independent active flags.

## Non-Goals

- Splitting provider metadata and library services from `AppClient` is Phase 3.
- Bounding the general client request channel is Phase 3.
- Joining every runtime worker and native thread is Phase 5.
- Changing Spotify Connect remote-device semantics is not part of this phase.
