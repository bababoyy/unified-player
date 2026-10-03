# ADR 0001: Provider Playback Invariants

Status: Accepted as Phase 0 characterization contract
Date: 2026-07-26

## Context

Spotify and YouTube Music have different APIs, session models, and playback
engines. Provider switching currently crosses event routing, `AppClient`,
`PlayerState`, queues, persistence, async monitors, and audio resources. Before
extracting a coordinator, the required behavior must be explicit and testable.

## Decision

The following invariants define correct multi-provider coordination:

1. **Single audible engine:** at most one provider may produce audio at a time.
2. **Pause before activation:** switching pauses the outgoing engine before any
   resume/start command reaches the incoming engine.
3. **Session preservation:** switching providers does not destroy either
   provider's durable media identity, progress, queue, repeat, shuffle, or
   volume state.
4. **Stale-work rejection:** cancelled or superseded generations cannot publish
   playback snapshots, failures, or persistence updates.
5. **Paused restore:** application restart restores durable identity and
   position but does not automatically play unless an explicit startup policy
   permits it.
6. **Remote Spotify authority:** a current Spotify Connect snapshot takes
   precedence over an older locally persisted Spotify session.
7. **No durable transport secrets:** cookies, OAuth tokens, authorization
   headers, signed media URLs, proof tokens, and browser state never enter the
   provider-session store.
8. **Failure isolation:** failure of one provider cannot activate, stop, or
   mutate the other provider except through an explicit switch/stop command.

## Ordering Contract

A successful provider switch has this conceptual order:

```text
record outgoing durable state
cancel outgoing prefetch/monitor work
pause outgoing engine
confirm or snapshot paused state
select incoming provider
refresh incoming authoritative state
resume incoming engine only when saved policy allows it
publish the new active-provider snapshot
persist durable state without transport secrets
```

An implementation may combine steps, but must preserve externally observable
ordering and stale-work rejection.

## Characterization Test Plan

- Fake both engines and assert pause occurs before resume.
- Start with both provider sessions populated and assert neither is destroyed.
- Complete an old generation after switching and assert its update is ignored.
- Cancel resolve/prefetch work and assert no result is published.
- Reload a playing persisted session and assert it is restored paused.
- Provide a newer remote Spotify snapshot and assert it replaces local state.
- Serialize populated sessions and scan the schema/output for forbidden secret
  categories without embedding real secrets in fixtures.
- Force each engine operation to fail and assert the other fake engine and
  session remain unchanged.

## Consequences

- Phase 1 must create fakes and an internal harness that can prove the contract
  without live accounts or audible output.
- Phase 2 should create one coordinator that owns these transitions.
- Manual audio testing remains useful but cannot be the sole proof of ordering,
  cancellation, persistence, or failure isolation.
- Any existing behavior that contradicts an invariant is a defect to document,
  not behavior to preserve silently.
