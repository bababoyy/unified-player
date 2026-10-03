# ADR 0007: Provider Platform Composition

Status: Proposed canonical target; not implemented

Date: 2026-08-30

## Authority and Scope

This ADR is the canonical target for provider composition across accounts,
authentication, catalog/search, libraries, collections and playlists,
playback, actions, scheduling, state application, and shared UI lifecycle.

Provider-specific transport, API, authentication, resolution, decoding,
matching, and mutation details remain inside provider modules. Domain-specific
architecture documents refine this target:

- ADR 0006 owns playback transition, queue-handoff, readiness, and cancellation
  rules.
- `docs/playlist-platform-architecture.md` owns playlist identity, occurrence,
  mutation, projection, and conflict rules.

If either document implies that Spotify and YouTube Music are the complete set
of possible providers, this ADR supersedes that implication. It does not
supersede their domain invariants.

## Context

The application has provider-neutral nouns such as `MediaId`, capability
values, operation contexts, selection generations, and Unified queues. Its
verbs remain provider-specific across the application:

- `ActiveProvider`, `Provider`, and `PlayableMedia` are closed over Spotify and
  YouTube Music;
- coordinator APIs receive separate Spotify and YouTube engine values;
- account management, search, library reads, playlist mutation, playback
  commands, media controls, diagnostics, state projections, and UI rendering
  contain repeated provider matches; and
- provider handlers can write application or UI state directly.

Consequently, adding a third provider requires editing common orchestration
even when that provider's API implementation is excluded. A playback-only
registry would not solve this. It would create another provider registry beside
the library, playlist, and action routers.

## Decision

Model a provider as a stable identity with a registered set of optional domain
capabilities. Do not model it as one playback engine and do not require every
provider to implement one large interface.

```text
ProviderRegistry
  |
  +-- ProviderDescriptor
  +-- AccountService?      authentication and account lifecycle
  +-- CatalogService?      search and entity lookup
  +-- LibraryService?      saved/followed/liked collection reads
  +-- CollectionService?   playlist and collection mutations
  +-- PlaybackService?     start, control, events, and endpoint policy
  +-- ActionProvider?      generic capability contributions and native actions
```

The registry is application composition, not dynamic plugin discovery. The
initial implementation remains statically linked and registers built-in
providers during startup.

## Identity Vocabulary

### Provider identity

Provider identity has separate runtime and persisted forms. The runtime remains
a closed set because providers are statically linked; persistence remains
forward-readable because disabled or unknown IDs must not destroy user data.

```rust
enum ProviderId {
    Spotify,
    YouTubeMusic,
    // A new built-in provider adds one variant here and one registration.
}

struct PersistedProviderId(String);
```

Every runtime variant has one stable, versioned serialization slug. Conversion
from `PersistedProviderId` returns either a supported `ProviderId` or an inert
unknown-provider value that can be displayed, retained, exported, or removed
but never executed. Renaming a slug requires an explicit alias migration.

This closed-identity/open-behavior split preserves compiler checking without
requiring common orchestration to match on provider variants. Adding a provider
may edit the identity definition and composition root. A compile error in
scheduler, coordinator, queue, common action planning, or shared library flow
is evidence that provider-specific behavior escaped its adapter boundary.

If dynamic third-party plugins are authorized in the future, revisit identity
and trust boundaries in a new ADR. Do not pre-pay that complexity with arbitrary
runtime strings today.

### Account identity

Every remote operation is scoped to an explicit provider account and account
epoch:

```rust
struct ProviderAccountRef {
    provider: ProviderId,
    account_id: String,
    epoch: u64,
}
```

The account ID is the application's stable, non-secret account key. Tokens,
cookies, signed URLs, proof tokens, and provider session material never become
generic identity or persistence fields.

### Media and collection identity

Keep `MediaId` as the provider-neutral playable identity, replacing its closed
provider enum only when the identity migration is a bounded goal. Collection
identity must likewise carry provider, account, kind, and provider-native ID:

```rust
struct CollectionRef {
    provider: ProviderId,
    account: Option<ProviderAccountRef>,
    kind: CollectionKind,
    raw_id: String,
}
```

Occurrence and mutation tokens remain scoped to a collection revision and are
owned by its collection adapter. They are not interchangeable with media IDs.

## Provider Descriptor and Capabilities

Each registration supplies immutable presentation and availability metadata:

```rust
struct ProviderDescriptor {
    id: ProviderId,
    label: String,
    capabilities: ProviderCapabilitySet,
}
```

Labels and icons are presentation metadata. Operational support comes from
registered services and instance-specific capability snapshots, not from the
descriptor alone. A configured service may still report an operation
unavailable because authentication, account scope, media kind, collection
revision, playback target, or current state does not permit it.

Capabilities are positive and granular. Absence is supported behavior, not an
error. Examples:

- a local Unified repository provides collection storage but no auth, catalog,
  or playback;
- a metadata-only provider may expose catalog search with no playback;
- a provider may support playback and likes but not editable playlists; and
- device transfer may exist for one playback target without becoming a generic
  requirement.

## Domain Service Boundaries

### Account service

Owns sign-in, sign-out, validation, account switching, credential refresh, and
provider account health. It returns typed account outcomes and never writes UI
state. Interactive browser/device-code requirements are explicit outcomes
rather than hidden side effects.

### Catalog service

Accepts provider-neutral search/entity queries and projects results to shared
media/entity summaries. Provider-native detail may remain an opaque typed
payload held inside the provider module; the common UI must not depend on it to
render identity, title, artwork, availability, or supported actions.

### Library service

Owns provider library reads and pagination. It projects them into a common
lifecycle and section shell:

```text
LibraryQuery
  -> LibrarySnapshot
       provider/account/generation
       sections
       entries
       continuation
       capability snapshot
```

The shell standardizes loading, empty, error, retry, stale generation,
selection, and pagination behavior. It does not force Spotify saved albums,
YouTube uploads, and SoundCloud likes or reposts into identical semantics.
Provider-specific detail pages may remain separate where the data and workflow
are genuinely different.

### Collection service

Owns playlist and collection creation, reads, exact-occurrence mutation,
provider-native mutation tokens, revision checks, and refresh receipts. It
implements the intent/result contracts in the playlist architecture document.
Provider adapters do not mutate page state or decide how a result is presented.

### Playback service

Owns provider start/control/deactivation mechanics, endpoint selection,
provider event normalization, and provider-local cancellation. The playback
coordinator consumes this capability through the rules in ADR 0006. It does
not know provider-specific endpoint names, auth material, or resolver routes.

### Action provider

Contributes actions using descriptors rather than page-specific hard-coded
lists. It supports both common and provider-native behavior.

## Action Context and Planning

Every action is planned from a stable context:

```rust
struct ActionContext {
    provider: ProviderId,
    account: Option<ProviderAccountRef>,
    scope: ActionScope,
    entities: Vec<EntityHandle>,
    dataset_generation: u64,
    capabilities: CapabilitySnapshot,
}
```

Common actions use common intent IDs such as play, queue, save, add to
collection, remove occurrence, move occurrence, or open related entity. They
are visible only when the current capability snapshot permits them.

Provider-native actions use namespaced IDs such as
`soundcloud:repost` or `spotify:transfer_device`. They still produce a typed
intent and pass through the same planner, contextual scheduler envelope,
operation lifecycle, cancellation, result classification, and UI feedback.
An action callback cannot call a provider client or mutate UI state directly.

The action menu is therefore extensible without pretending all providers have
feature parity. Unsupported actions are absent or carry an explicit bounded
reason according to the shared UX contract.

## Application Ownership

```text
input / provider fact / user action
              |
        typed application intent
              |
 contextual scheduler envelope
              |
 domain application service
              |
 ProviderRegistry -> optional provider capability
              |
 typed result or provider fact
              |
 centralized state application
              |
 shared lifecycle UI + provider-specific detail projection
```

Ownership rules:

| Layer | Owns | Must not own |
| --- | --- | --- |
| UI/page state | focus, selection, viewport, visible lifecycle, action context | provider calls, retry loops, auth tokens |
| Planner | capability and freshness validation, intent formation | network execution, UI mutation |
| Scheduler | ordering, deduplication, supersession, operation context, task cancellation | provider semantics, queue policy |
| Domain application service | one logical operation, provider selection, normalized result | provider transport, terminal rendering |
| Provider adapter | API/transport/auth mechanics and provider-native tokens | global UI, cross-provider policy |
| State application | cache/projection updates and stale-generation rejection | network calls, provider fallback |

Playback, library, and collection services remain separate application
services. They share identity, registry, scheduling, outcomes, and state
application conventions, not a generic coordinator base class.

## State and UI Contract

Provider projections are keyed by provider/account rather than stored as an
expanding set of `spotify_*`, `youtube_*`, and future-provider fields. Do not
replace those fields with one universal `ProviderRuntimeState` bag or one lock.
Each domain owns typed storage keyed by shared identity:

```text
AccountState[ProviderId]
CatalogCache[ProviderAccountRef, QueryKey]
LibrarySnapshots[ProviderAccountRef, LibraryViewKey]
CollectionSnapshots[CollectionRef]
PlaybackSessions[ProviderAccountRef]
ProviderHealth[ProviderId]
```

The maps are projections, not owners of behavior, and they need not share an
implementation type or mutex. Domain state application is the only writer for
provider results. UI reads normalized snapshots and owns interaction state.
Provider modules may supply presentation data but never write terminal or page
state.

All domain operations use a shared lifecycle vocabulary where applicable:
accepted, running, applied, no-op with reason, unsupported with reason,
retryable failure, terminal failure, cancelled, superseded, and stale. Domain
receipts retain richer facts without collapsing them into `success` or
`unavailable`.

## Provider Addition Contract

Adding a statically linked third provider may require:

1. a provider module implementing only its supported domain services;
2. one descriptor and composition-root registration;
3. provider-specific configuration and credential persistence;
4. provider-native projections or detail pages where justified; and
5. focused adapter, contract, and manual acceptance tests.

It must not require provider-specific branches in:

- the request scheduler;
- the playback coordinator state machine;
- UnifiedQueue advancement;
- generic media-control routing;
- shared library lifecycle and selection;
- shared collection operation planning;
- generic action-menu construction; or
- operation outcome and UI lifecycle handling.

Changing a durable provider ID or schema is a migration, not ordinary provider
registration.

## Architecture Tests

Before claiming provider neutrality, test the common layers with providers that
do not match the Spotify/YouTube capability shape:

### Fake search and playback provider

- catalog and local playback capabilities;
- no account service, library, or editable collections;
- common play, pause, seek, end-of-track, cancellation, and queue handoff; and
- one provider-native playback action.

### Fake library and collection provider

- account, library, and editable collection capabilities;
- no playback capability;
- loading, pagination, retry, exact-occurrence mutation, and stale result
  rejection; and
- one provider-native collection action.

The test succeeds only if these providers can be registered without editing
production scheduler, coordinator, queue, common action planner, or shared
library UI control flow. Editing the closed identity definition and composition
root is allowed. Compile errors or behavior branches elsewhere are evidence
that provider-specific policy escaped its adapter boundary.

## Failure-Mode Review

The architecture is useful only if its easiest implementation is also a safe
implementation. The following risks are not hypothetical abstractions; each is
adjacent to an existing project pattern such as the broad `AppClient` facade,
static capability matches, provider-shaped `ActionContext`, provider-specific
caches, partially propagated account epochs, detached provider work, or direct
state application from request handlers.

Severity means architectural impact:

- **P0:** can violate account isolation, single playback ownership, data
  integrity, credential safety, or deterministic shutdown;
- **P1:** can recreate the current cross-product, produce incorrect UX, or make
  provider addition unsafe; and
- **P2:** can cause performance, diagnostics, compatibility, or maintenance
  degradation without immediately corrupting ownership.

### P0: Registry becomes a global service locator

Failure: UI, event handlers, provider adapters, and helpers fetch arbitrary
services from a globally accessible registry. Dependencies become invisible,
providers call one another recursively, tests require the entire application,
and the registry replaces `AppClient` with a larger indirection layer.

Guardrails:

- construct and validate the registry once at the composition root;
- keep it immutable for the process lifetime;
- inject a domain-specific resolver into each application service, never the
  complete registry into UI or provider code;
- forbid provider adapters from resolving other providers; and
- keep cross-provider orchestration in application planners/services.

Proof: a catalog service test constructs only a catalog resolver and fake
catalog adapter. It does not construct playback, UI, runtime, or another
provider domain.

### P0: Hidden current account leaks work across accounts

Failure: a registered adapter closes over a mutable "current account". A
request begins for account A, the user switches to account B, and the late
result populates B's cache, mutates B's playlist, or starts playback using A's
credentials.

Guardrails:

- every remote intent, cache key, provider event, and result envelope carries
  `ProviderAccountRef` and account epoch;
- adapters receive account scope explicitly instead of reading global current
  account state during execution;
- account switching cancels account-scoped work and increments the epoch; and
- state application rejects a mismatched provider, account, or epoch even when
  cancellation was ignored.

Proof: switch accounts while delayed search, playlist mutation, and playback
resolution fakes are in flight. None may update the new account's projection or
publish success.

### P0: Provider events are attributed to the wrong activation

Failure: many provider SDK events contain a media ID but no application
generation. A delayed `Playing` or `Ended` event from an old activation can mark
the new provider ready, advance UnifiedQueue twice, or stop the current engine.

Guardrails:

- the playback adapter binds each event subscription to an activation lease;
- the adapter envelope adds provider, account, expected media identity, and
  application generation at the boundary;
- the coordinator validates every field before readiness, queue advancement,
  persistence, or UI publication; and
- duplicate completion is idempotent for one generation.

Proof: deliver old-provider, wrong-media, duplicate-ended, and out-of-order
ready/ended events after a switch. All stale facts are observable but inert.

### P0: Cooperative cancellation is mistaken for completion

Failure: a provider future ignores its cancellation token or is blocked in
native/network work. The request task is cancelled, but the provider later
publishes a result, holds a device, starts audio, or mutates a remote
collection.

Guardrails:

- cancellation is required at every async provider boundary;
- state application always performs generation and epoch checks independently
  of cancellation;
- coordinator ownership can deactivate late playback acknowledgements;
- mutations distinguish cancelled-before-dispatch from outcome-unknown after
  dispatch; and
- bounded shutdown reports workers that exceed their deadline.

Proof: contract fakes deliberately ignore cancellation and complete late. They
cannot change local state; remote mutation reports `OutcomeUnknown` rather than
lying about cancellation.

### P0: Provider-owned workers escape `AppRuntime`

Failure: a provider starts browser, websocket, polling, decoder, or event tasks
inside registry construction and drops their handles. Shutdown completes while
workers retain credentials, locks, browser profiles, audio devices, or file
writers.

Guardrails:

- registry construction is side-effect free;
- background work is returned as an explicit provider runtime contribution;
- `AppRuntime` owns every task/thread and supplies ingress/work cancellation;
- provider shutdown is idempotent and bounded; and
- worker failure has declared critical or auxiliary policy.

Proof: a fake provider owns one active async worker and one native thread; both
must appear in lifecycle diagnostics and join during normal shutdown.

### P0: Untyped escape hatches carry secrets or invalid state

Failure: common interfaces use `Any`, `serde_json::Value`, arbitrary maps,
closures, or debug strings to accommodate provider differences. Downcasts fail
at runtime, provider payloads leak into UI/persistence, and tokens or signed
URLs enter logs and operation contexts.

Guardrails:

- no `Any` or generic JSON payload in provider-platform public contracts;
- provider-native mutation tokens use bounded, domain-owned envelopes with
  explicit safe serialization policy;
- action descriptors contain IDs and presentation metadata, never executable
  callbacks or credentials;
- safe diagnostic fields are derived separately from domain errors; and
- ephemeral transport material never enters shared state.

Proof: serialization and diagnostic tests scan populated provider envelopes for
forbidden credential and transport categories.

### P0: Migration creates two executable paths

Failure: a registry adapter wraps the old handler while direct UI/request/event
paths remain active. Both can execute, advance queues, refresh caches, or apply
the same provider event. The abstraction increases bloat and races instead of
removing them.

Guardrails:

- one write owner per vertical slice;
- route one characterized behavior through the new boundary;
- delete the old direct call and state write in the same goal;
- make duplicate execution visible in tests and diagnostics; and
- keep compatibility bridges read-only or give them an explicit removal goal.

Proof: each migration commit reports removed callers/branches and demonstrates
one terminal operation for one intent.

### P1: Optional services form a provider mega-object anyway

Failure: `ProviderServices` accumulates dozens of optional traits, shared
mutable state, lifecycle callbacks, caches, and helper methods. Every domain
depends on the full bag and capability combinations become impossible to
reason about.

Guardrails:

- `ProviderServices` exists only as composition metadata;
- domain registries/resolvers expose one service family at a time;
- provider-internal clients may share a private session/runtime, but common
  domains receive narrow trait handles; and
- adding a domain requires its own intent/result/state contract, not methods on
  a base provider trait.

Proof: the partial-capability fake providers compile without dummy or panic
implementations for unsupported domains.

### P1: Capability flags become another exhaustive match table

Failure: static booleans say SoundCloud supports `like`, `playlist`, or `seek`,
but actual support varies by account, media kind, ownership, target revision,
region, playback endpoint, or current state. Large flag structs duplicate
provider branching and drift from execution.

Guardrails:

- static descriptors advertise only broad service presence;
- operation-specific capability queries return a snapshot with provider,
  account epoch, dataset generation, target revision, and bounded reason;
- execution revalidates the operation against current state; and
- UI never treats a cached capability snapshot as authorization.

Proof: capability changes after a menu opens cause a typed rejection with a
useful next action, not an invalid provider request or silent no-op.

### P1: Capability-driven UI becomes unstable

Failure: action lists reorder or disappear during rendering as health and
capabilities refresh. Numeric shortcuts target a different action, selection is
lost, or the user sees controls flicker between enabled and absent.

Guardrails:

- action descriptors have stable IDs and deterministic registry order;
- a popup freezes its `ActionContext` and descriptor order for its lifetime;
- execution revalidates by action ID, not displayed index; and
- changed capability produces a bounded rejected/superseded result.

Proof: mutate capability state while a popup is open and confirm the chosen ID
cannot execute a newly shifted action.

### P1: Provider-native actions bypass application policy

Failure: `soundcloud:repost` is registered as a callback that calls the client
directly. It bypasses scheduler ordering, account epoch, cancellation, operation
feedback, privacy, and state refresh policy.

Guardrails:

- descriptors are inert data;
- native action IDs are namespaced, validated, versioned, and collision-free;
- planners convert IDs to typed domain intents; and
- only domain application services dispatch provider execution.

Proof: a native action produces the same accepted/running/terminal lifecycle
and stale-account rejection as a common action.

### P1: Shared models erase meaningful provider semantics

Failure: one universal library or collection entry uses many optional fields,
misrepresents uploads/reposts/episodes, invents reorder semantics, or loses
provider mutation tokens. The UI looks uniform but sends invalid operations.

Guardrails:

- normalize identity, presentation minimums, lifecycle, selection, and
  capabilities, not every provider field;
- keep provider-native detail projections inside provider modules;
- distinguish media identity, collection identity, and occurrence identity;
- express unsupported behavior explicitly; and
- never manufacture provider tokens to keep an action enabled.

Proof: fixtures with duplicates, provider-only entity kinds, missing metadata,
and non-editable collections remain readable while invalid actions stay absent.

### P1: Cross-provider operations pick the wrong owner

Failure: adding a Spotify item to a YouTube or SoundCloud collection is assigned
to the source adapter, target adapter, or action provider inconsistently.
Matching/resolution, target capability, idempotency, and partial outcome become
hidden provider behavior.

Guardrails:

- the application planner owns source-to-target planning;
- the source adapter supplies identity/metadata, a resolver performs explicit
  conversion when required, and the target collection adapter owns mutation;
- target account/revision is fixed before execution; and
- partial or unresolved results remain typed.

Proof: source, destination, and resolver failures are independently injected
and never mutate the wrong provider or consume an occurrence silently.

### P1: A universal outcome taxonomy becomes meaningless

Failure: every domain returns `success`, `unavailable`, or `retryable`, losing
the difference between empty library, unsupported action, stale revision,
rate limit, command accepted, playback ready, partial remote mutation, and
unknown remote outcome.

Guardrails:

- share only terminal lifecycle axes;
- retain domain-specific typed receipts and failure categories;
- separate command acknowledgement from observable readiness;
- require every no-op/rejection to carry a bounded reason; and
- let UI map typed facts to domain-appropriate guidance.

Proof: contract tests assert that semantically distinct outcomes cannot collapse
to the same success/no-op path.

### P1: Browsing provider, playback owner, and mutation target are conflated

Failure: one `active_provider` drives navigation, media controls, account scope,
and destination actions. Browsing SoundCloud can redirect controls from a
playing Spotify session or send a mutation to the wrong account.

Guardrails:

- represent browsing context, playback owner/transition, action source, and
  mutation destination as distinct semantic values containing `ProviderId`;
- never infer playback owner from the visible page; and
- require explicit destination/account in mutation intents.

Proof: browse provider B, play provider A, and mutate a collection at provider C
without any owner changing implicitly.

### P1: Scheduler generalization causes head-of-line blocking or event floods

Failure: every provider fact enters one ordered lane, so a slow library request
blocks pause/seek, or high-frequency progress events exhaust the request queue.
Conversely, overly broad latest-wins keys cause unrelated accounts/providers to
supersede one another.

Guardrails:

- scheduler owns mechanics while domain intent metadata selects bounded
  delivery policy;
- deduplication keys include domain, provider, account, and resource scope where
  required;
- high-frequency projections are sampled/coalesced before ingress;
- playback control and shutdown retain bounded latency; and
- ordered mutation lanes do not serialize unrelated accounts globally.

Proof: mixed-provider load tests cover queue depth, control latency, fair
progress, deduplication scope, and shutdown priority.

### P1: Lock ordering and callbacks deadlock the application

Failure: registry, account, state, coordinator, provider-client, and UI locks are
held across await points or acquired in inconsistent order. A provider event
callback re-enters a service while the same service lock is held.

Guardrails:

- registry reads are immutable and lock-free after startup;
- never hold application/UI/state mutex guards across provider awaits;
- provider events are data sent through an owned boundary, not synchronous
  callbacks into application state;
- document the remaining lock order; and
- use bounded channels rather than re-entrant callbacks.

Proof: concurrency tests pause provider futures while account switch, shutdown,
and event delivery proceed; no required owner waits on itself.

### P1: Unknown, disabled, or removed providers corrupt persistence

Failure: a feature-disabled provider makes sessions, Unified rows, playlists,
history, or config fail to deserialize. The application drops unknown entries
on save or attempts to execute them through the default provider.

Guardrails:

- persisted provider IDs are forward-readable and retained verbatim;
- unknown provider rows are inert placeholders with explicit unavailable
  reason;
- save preserves unknown envelopes unless the user removes them;
- no fallback provider is inferred; and
- provider-specific schema versions migrate independently.

Proof: load, display, save, export, and remove data for an unregistered provider
without executing it or losing it.

### P1: One provider's initialization failure blocks the application

Failure: browser/auth/cache initialization for an optional provider fails and
registry construction aborts Spotify playback or the entire TUI. Alternatively,
the provider appears healthy because its descriptor registered before runtime
initialization failed.

Guardrails:

- distinguish descriptor registration, configured availability, authenticated
  health, and runtime readiness;
- isolate optional provider initialization failures;
- validate the configured default/browsing provider with an explicit fallback
  selection policy; and
- expose retryable provider health without claiming service capability ready.

Proof: one fake provider fails registration validation, another fails runtime
initialization, and unrelated provider behavior remains usable.

### P2: Generic projection increases memory and render churn

Failure: adapters clone complete libraries into normalized snapshots, retain
provider-native payloads twice, or publish a new generation for every progress
tick. Trait dispatch is blamed even though allocation, locks, and rendering are
the actual cost.

Guardrails:

- snapshots are paged and bounded;
- large immutable data uses shared ownership where appropriate;
- presentation summaries exclude unused native payloads;
- high-frequency playback progress does not invalidate unrelated views; and
- performance budgets measure projection, state application, and render cost.

Proof: large-library and rapid-progress fixtures verify bounded allocation,
stable selection, and sampled UI updates.

### P2: Registry order and identifiers destabilize UX or diagnostics

Failure: `HashMap` iteration changes provider/action order between runs;
provider-native IDs create unbounded diagnostic cardinality; renamed action IDs
break key bindings or stored state.

Guardrails:

- registry and action presentation order are explicit and deterministic;
- IDs use a validated bounded grammar and versioning/alias policy;
- diagnostic operation names come from a registered bounded vocabulary; and
- user-visible labels are not persistence or correlation keys.

Proof: repeated construction produces identical navigation/action order and
safe bounded diagnostic fields.

### P2: Fake providers are too polite

Failure: architecture tests use immediate, perfectly cancellable, single-page
providers. They prove trait compatibility but miss the concurrency and semantic
failures that motivated the architecture.

Guardrails:

- fakes support delayed, duplicated, out-of-order, partial, stale, unauthorized,
  rate-limited, and cancellation-ignoring behavior;
- test both providers simultaneously and across account epochs;
- use unsupported capability combinations; and
- require terminal outcome and state invariants, not only returned values.

Proof: the stress scenarios below are mandatory contract fixtures rather than
optional integration tests.

## Mandatory Stress Scenarios

Before ADR 0007 can become implemented, common-layer tests must cover:

1. **Account switch during work:** delayed library, mutation, and playback
   results for account A arrive after switching to B and remain inert.
2. **Delayed playback events:** old `Ready` and duplicate `Ended` facts arrive
   after a provider switch and cannot publish or advance twice.
3. **Ignored cancellation:** a provider completes after cancellation; local
   state rejects it and remote mutation reports an honest unknown outcome.
4. **Partial capability provider:** search/playback with no library and
   library/collection with no playback require no dummy methods or shared-layer
   provider branches.
5. **Capability drift:** an action popup remains stable while capability changes;
   execution revalidates the stable action ID and explains rejection.
6. **Cross-provider mutation:** source, resolver, and destination accounts are
   distinct and individually validated.
7. **Unknown provider persistence:** disabled-provider rows survive load/save and
   remain readable but inert.
8. **Degraded startup:** one optional provider fails initialization without
   disabling unrelated providers or claiming false readiness.
9. **Scheduler pressure:** provider events, library pagination, mutation, pause,
   seek, and shutdown coexist without unbounded queue growth or control
   starvation.
10. **Owned shutdown:** provider async tasks, native threads, browser resources,
    and audio resources are cancelled and joined through `AppRuntime`.

## Design Rejection Criteria

Reject an implementation, even if its happy-path tests pass, when any of these
are true:

- UI, renderer, or event code obtains the full provider registry;
- a remote provider operation lacks explicit provider-account epoch;
- a provider public contract contains `Any`, arbitrary JSON, executable action
  callbacks, credentials, signed URLs, or raw response bodies;
- a provider starts an unowned background task or native thread;
- a provider adapter writes `UIState`, global page state, or another provider's
  projection;
- common scheduler, coordinator, queue, library shell, or action planner matches
  a provider variant to execute provider behavior;
- registry construction performs auth, browser, network, playback, or mutation
  side effects;
- registry or descriptor order depends on hash iteration;
- old and new paths can execute the same intent or provider fact;
- cancellation alone is trusted to prevent stale state publication;
- unknown persisted providers are dropped, remapped, or executed through a
  default provider; or
- an abstraction adds more provider branches/owners than it removes in the
  bounded goal.

Provider identity serialization, composition-root registration, presentation
descriptor selection, and explicit migrations are the allowed places to name a
concrete built-in provider.

## Decisions Required Before Accepted

ADR 0007 remains proposed until owner review resolves these choices:

1. Confirm the closed runtime `ProviderId` plus forward-readable
   `PersistedProviderId` split.
2. Confirm that the complete registry is composition-root-only and domain
   services receive narrow resolvers.
3. Confirm that provider-native actions are inert descriptors converted to
   typed domain intents, never callbacks.
4. Confirm separate semantic owners for browsing context, playback owner,
   action source, and mutation destination.
5. Confirm provider workers must be contributed to and owned by `AppRuntime`.
6. Confirm unknown provider data is retained inert rather than rejected or
   silently migrated to another provider.
7. Confirm domain-owned typed projection maps instead of one universal provider
   state bag.
8. Confirm the mandatory stress scenarios are milestone gates, while each
   bounded migration goal runs only its relevant subset.

## Incremental Migration Rules

1. Do not build the whole registry before a maintained behavior needs a slice.
2. Every slice begins with current behavior characterization and a fake adapter
   contract.
3. Introduce a provider-neutral boundary only while moving one existing path
   behind it.
4. Delete the replaced provider match, direct UI write, or parallel execution
   path in the same goal.
5. Do not combine identity migration, playback handoff, library UI, collection
   mutation, and auth refactors in one goal.
6. Preserve readable persisted data for disabled or unknown providers.
7. Count reduced common-layer provider matches and removed owners as the
   maintenance payoff; registry code alone is not completion.

The next playback implementation may remain
`unified-queue-single-handoff-owner-01`, but its event and handoff contract must
use provider identity and capabilities without assuming the only other provider
is YouTube Music or Spotify. It does not need to implement the complete registry
or migrate all provider identities.

## Non-Goals and Rejected Approaches

- No SoundCloud implementation is authorized by this ADR.
- No dynamic library, plugin ABI, provider marketplace, or runtime code loading.
- No single `Provider` mega-trait requiring unsupported dummy methods.
- No universal data model that erases meaningful provider semantics.
- No provider-specific API payloads in shared UI, queue, or scheduler types.
- No application service that combines auth, library, playlists, and playback
  merely because they share a registry.
- No broad rewrite solely to reduce match counts.

## Acceptance and Finality

This target becomes `Accepted` after owner review confirms the eight decisions
above, the domain split, and the provider-addition contract. Acceptance approves
direction and guardrails, not a big-bang rewrite. Implementation remains
incremental after acceptance. It becomes implemented only when the architecture
and stress tests pass and common orchestration no longer requires
provider-specific behavior branches for a fake third provider.
