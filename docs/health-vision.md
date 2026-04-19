# Health & Error Handling Vision

> This document traces the full design evolution, including rejected approaches and the
> reasoning behind each rejection. Preserving this history prevents re-litigating settled
> decisions in future sessions.

---

## Core distinction: reconciliation vs health

**Reconciliation** is diff-driven. Given a desired set and an actual set, compute the minimal
set of imperative operations (enter/update/exit) to close the gap. Triggered by desired
state changing.

**Health** is not diff-driven. There is no "desired health state" to diff against. It is
either a full scan, an event feedback loop, or a scheduled check. Structurally different
from reconciliation, and triggered from a completely separate place.

They share the same objects — `Lifecycle` implements both the reconcile operations and the
health check semantics — but are driven independently.

---

## Design iteration 1: HealthStatus enum + ManagedSet tracking

### What we proposed

A `HealthStatus` enum with TTL-carrying variants:

```rust
pub enum HealthStatus {
    Alive,
    Starting { valid_until: Instant },
    Stopping { valid_until: Instant },
    Dead { retry_after: Option<Instant> },
    Failed,
}

impl HealthStatus {
    pub fn needs_healing(&self, now: Instant) -> bool { ... }
}
```

`Lifecycle` would expose:
- `fn health(state: &Self::State, ctx: &Self::Context) -> HealthStatus`
- `fn unhealthy_rx(state: &Self::State) -> Option<Receiver<()>>` (push-based)
- `fn retry_limit(&self) -> Option<u32>`

`ManagedSet` would track `retry_counts: HashMap<Key, u32>` separately, call `needs_healing()`
during reconcile, and drive exit+enter on unhealthy items.

### Why rejected

**Forces a specific health strategy into the trait.** Not all lifecycle subjects are
processes. A file management system (enter = create, exit = delete, update = write) might
use biome lint as a health check — running a lint process per file on a cron schedule. A
repo compliance checker might run once at 3am. Baking `HealthStatus`, TTLs, retry counts,
and `unhealthy_rx` receivers into the trait imposes process-centric assumptions on every
implementor.

**Reduces flexibility.** Each use case needs a different health strategy: rate limiting,
backoff, event-driven monitoring, scheduled polling, or no health concept at all. Forcing
these into the trait statically removes the implementor's ability to choose.

**Adds overhead for no reason.** Items that need no health monitoring would still carry the
dead weight of the interface.

**Wrong layer for retry tracking.** Retry count in `ManagedSet` means the reconciler has to
know about retry policy — that's not its concern.

---

## Design iteration 2: NeededAction — item drives the decision

### What we proposed

Remove health from the trait entirely. Instead, give items a way to report what operation
they need from the reconciler:

```rust
pub enum NeededAction {
    Update,    // normal path
    Restart,   // exit + enter
    Remove,    // exit, don't re-enter (gave up)
}

trait Lifecycle {
    fn needed_action(state: &Self::State) -> NeededAction { NeededAction::Update }
}
```

`ManagedSet` for the update set:
```
match needed_action(state):
    Update  → update(item, state)
    Restart → exit(state) + enter(item)
    Remove  → exit(state), drop from desired
```

Item owns all health strategy internally via `State`. `ManagedSet` stays dumb.

### Why rejected

**`update()` already is this.** `update()` is called for every item in the update set
(desired ∩ existing). It has full access to `State`. It returns `Result<(), Error>`. A
failure already triggers exit + eviction. `ProcessSource::update()` already checks
`try_wait()`, restarts the process, and implements the restart logic — all within `update()`.
`NeededAction` is just a more explicit version of what `update()` already expresses.

The only case `needed_action` adds over `update()` is `Remove` — "remove me from desired
entirely." But that's a policy decision for the operator/caller (based on accumulated
`ReconcileErrors`), not for the item itself. An item should not unilaterally decide to
remove itself from the desired state.

---

## Design iteration 3: update() is where health goes (current understanding)

### What we concluded

`update()` is already the right place for all health and healing logic:

- Full access to `State` — retry count, backoff timer, `try_wait()`, whatever the item
  needs lives there
- Returns `Result<(), Error>` — failure triggers exit + eviction from store
- Called every reconcile cycle — already the right hook

Every `Lifecycle` implementor has full flexibility to implement whatever health strategy
their use case requires, entirely within `update()` and `State`. No framework needed.

The "give up" case (remove from desired) is handled by the caller based on accumulated
errors in `ReconcileErrors`, not by the item itself.

### Remaining problems

**Naming.** `update()` suggests "apply new desired state to a running thing." Nothing about
the name or type signature signals "this is also where you implement health checks, restart
logic, and backoff." A new implementor would not know to put health logic here.

**No helpers for common patterns.** Every implementor reimplements backoff, retry counting,
and TTLs from scratch. A library-quality crate should ship composable helpers for the common
case — not mandated, just available.

---

## Design iteration 4: rename + layered trait system (current best direction)

### Rename update() to reconcile_self()

`reconcile_self()` signals that the item is responsible for reconciling its own internal
state — which includes health, restart, config application, whatever it needs. This makes
the dual responsibility explicit without changing the signature.

### Layered trait system

A higher-level optional trait sits above `Lifecycle`, providing `reconcile_self()` via a
blanket impl. It splits the implementation concern into required methods the user fills in:

```rust
// Higher-level trait — Template Method pattern
trait SupervisedLifecycle {
    // Is the underlying resource still alive?
    fn is_alive(state: &Self::State) -> bool;

    // Apply new desired config to a running, healthy resource
    fn apply_update(self, state: &mut Self::State, ctx: &Self::Context) -> Result<(), Self::Error>;

    // What retry/backoff policy does this item use?
    fn retry_budget(state: &Self::State) -> &RetryBudget;
}

// Blanket impl — wires reconcile_self() automatically
impl<T: SupervisedLifecycle> Lifecycle for T {
    fn reconcile_self(self, state: &mut Self::State, ctx: &Self::Context) -> Result<(), Self::Error> {
        if !T::is_alive(state) {
            // restart logic using RetryBudget, GracePeriod, etc.
        } else {
            self.apply_update(state, ctx)
        }
    }
}
```

Helper structs (not part of the trait — composable utilities):

```rust
struct RetryBudget { limit: Option<u32>, count: u32, backoff: Duration }
struct GracePeriod { valid_until: Instant }
```

### Key properties

- **Base `Lifecycle` stays minimal.** enter, reconcile_self, exit. Anyone can implement
  directly with full control.
- **`SupervisedLifecycle` is opt-in.** Use it when your resource is process-like: has an
  alive/dead distinction and separable config-application. Skip it for file management,
  compliance checkers, or anything else with a different shape.
- **Helpers are composable, not mandated.** `RetryBudget` and `GracePeriod` are stored in
  `State` by implementors who need them. Nothing forces their use.
- **Separation of concerns is explicit.** `is_alive` = health check. `apply_update` = config
  application. The implementor thinks about these separately; the blanket impl composes them.

### Open questions

- Final name for the higher-level trait: `SupervisedLifecycle`, `RestartableLifecycle`,
  something from Erlang OTP vocabulary?
- Should `reconcile_self()` replace `update()` on the base `Lifecycle` trait, or live only
  on the higher-level trait?
- How does `DataPipe` decide *when* to call reconcile — on a tick, on a stream event, on a
  push health signal? (See `datapipe-vision.md`)
- Should `ReconcileErrors` accumulation drive the "give up" decision, or should there be an
  explicit mechanism for the caller to say "stop trying this item"?

---

## Design iteration 5: the Supervisor trait

### Background: what is OTP?

**OTP** (Open Telecom Platform) is Erlang's standard framework for building fault-tolerant
concurrent systems. Despite the name it is general-purpose — WhatsApp, Discord, and
RabbitMQ all run on it. The two relevant concepts:

- **GenServer**: a generic process that handles messages (synchronous calls and async casts).
  Equivalent in spirit to what `ProcessSource` does manually today.
- **Supervisor**: a process that *owns* a set of child processes, monitors their health, and
  applies a restart strategy when one crashes. Strategies include: *one-for-one* (restart
  only the crashed child), *one-for-all* (restart all siblings too). Supervisors can
  supervise other supervisors, forming a **supervisor tree**.

The "let it crash" philosophy comes from OTP: instead of defensive error handling inside a
process, let it die and rely on the supervisor to restart it cleanly. Fault isolation is
structural — a crash in one branch of the tree cannot corrupt another branch.

### Motivation

The current `DataLoop` / `DataLoopHandle` owns the `ManagedSet`, drives reconciliation on a
tick, and is wired manually per feature. There is no abstraction over "the thing that owns a
managed collection and decides when reconciliation runs." Once the health monitoring design
above matures, there also needs to be something that re-exposes per-item health to the
outside world.

The natural name is **Supervisor** — borrowed from Erlang OTP, where a Supervisor owns a set
of child processes, monitors their health, and applies restart strategies. In this codebase
the analogy is: Supervisor owns the `ManagedSet<T>`, drives reconcile cycles, and reports
health.

"Stage" was considered but rejected — it implies a multi-stage pipeline, whereas a Supervisor
can stand alone (a single component, not part of any pipeline).

### What a Supervisor owns and guarantees

1. **Owns the managed collection.** The `ManagedSet<T>` lives inside the Supervisor, not
   exposed directly. Reconciliation is the Supervisor's concern, not the caller's.

2. **Drives reconciliation cadence.** Whether on a tick, a stream event, or an external push,
   the Supervisor decides *when* to call reconcile. Callers send desired state in, results
   come out — they do not touch the store directly.

3. **Generic `HealthStatus` type flowing from `Lifecycle` to the external API.** The
   Supervisor is generic over a `HealthStatus` type declared by the `Lifecycle` implementor.
   This gives a compile-time guarantee that what the Lifecycle produces is what the Supervisor
   reports — no stringly-typed or `Box<dyn Any>` leakage.

4. **Forces the store to support full iteration.** To expose health to the outside world, the
   Supervisor must be able to enumerate all tracked items. The underlying store interface must
   expose `get(key)` and `iter()`. This is not currently required by `ManagedSet` and will
   need to be added.

5. **Re-exposes per-item health to external consumers.** Callers can query `health_snapshot()`
   (or equivalent) to get a `HashMap<K, HealthStatus>`. This falls naturally out of iterating
   the store and calling `is_alive` / `health()` on each item's state.

6. **Log collection falls out from stderr streams.** Because each `Lifecycle` subject manages
   a child process, its `State` holds a stderr reader. The Supervisor can map over items and
   collect logs without any additional protocol.

7. **Message-based interface from pipeline (unidirectional).** When used inside a DataPipe,
   the Supervisor receives desired-state messages inbound and pushes health/log events outbound.
   The pipeline does not call `reconcile()` directly — it sends a message, and the Supervisor
   decides whether to act.

### Rough trait sketch

```rust
trait Supervisor {
    type Item: Lifecycle;
    type HealthStatus;

    // Called by the driver (tick, event, ...) with new desired state.
    fn apply_desired(
        &mut self,
        desired: impl IntoIterator<Item = Self::Item>,
        ctx: &<Self::Item as Lifecycle>::Context,
    ) -> ReconcileErrors<<Self::Item as Lifecycle>::Key, <Self::Item as Lifecycle>::Error>;

    // Snapshot of per-item health at this moment.
    fn health_snapshot(&self) -> HashMap<<Self::Item as Lifecycle>::Key, Self::HealthStatus>;
}
```

`HealthStatus` is supplied by the implementor, not by the framework. A process supervisor
might use an enum with `Alive / Starting / Dead`. A file-management supervisor might use a
simple `bool`. The trait imposes no assumptions.

### Relationship to SupervisedLifecycle

`Supervisor` and `SupervisedLifecycle` are orthogonal:
- `SupervisedLifecycle` is an optional higher-level helper for *individual items* — it
  splits `reconcile_self()` into `is_alive` + `apply_update` + `retry_budget`.
- `Supervisor` is the owner/driver of the *collection* — it holds the `ManagedSet`,
  drives reconcile, and aggregates health.

A `Supervisor` impl can use any `Lifecycle` implementor, whether it uses
`SupervisedLifecycle` or implements `reconcile_self()` directly.

---

## Error path propagation

### The problem

`ReconcileErrors<K, E>` is currently `Vec<(K, E)>` — flat. If a widget or process three
levels deep fails, the error surfaces at the top with only its immediate key. There is no
information about which branch of the tree the failure came from.

### Option A — typed path

```rust
pub struct KeyedError<K, E> {
    pub path: Vec<K>,   // sequence of keys from root to the failing item
    pub error: E,
}
pub type ReconcileErrors<K, E> = Vec<KeyedError<K, E>>;
```

Each reconciler level wraps child errors by prepending its own key to `path`. Display
becomes `"panel 'sidebar' → process 'weather' → …"` naturally. Works when all levels share
a homogeneous key type; otherwise `path` must be `Vec<String>` (keys converted to display
form at each level boundary).

### Option B — anyhow context chaining

If the crate adopts `anyhow`, path information builds up automatically through
`.context("panel 'sidebar'")` at each level boundary. No new type needed; the error chain
IS the path. Best fit for errors that are only ever logged or displayed, never
pattern-matched.

### Decision criteria

- If errors are **only logged/displayed**: Option B (anyhow context) — zero extra type
  machinery, path comes for free.
- If errors are **pattern-matched** at any level (caller takes different action based on
  which item failed): Option A — typed `KeyedError` preserves structure for programmatic
  inspection.

The `Supervisor` trait design (iteration 5 above) will determine which applies: if the
supervisor exposes a `health_snapshot()` API that callers query, errors are likely
inspected programmatically → Option A. If errors are only routed to a log stream → Option B.

---

### Open questions

- Should `apply_desired` be the only input method, or should there be a separate
  `trigger_reconcile()` (re-run with existing desired state, e.g. on a health push signal)?
- Should `health_snapshot` be a pull API, or should the Supervisor push health events to a
  `tokio::sync::watch` channel?
- How does the Supervisor interact with `DataPipe`'s backpressure model?
- Should the `Supervisor` trait live in `managed_set` or in a higher-level module?
- **Failed item retention**: when `reconcile_self()` returns `Err` and the item exits and is
  evicted, there is no way to inspect it after the fact. Should failed items be moved to a
  separate "known-failed" map so callers can observe what died and why? Or is that the
  caller's responsibility, via accumulated `ReconcileErrors`?
- **"Give up" mechanics**: when retry budget is exhausted, what does the Supervisor do
  mechanically — call `exit()` and evict, silently drop the item from desired, or park it in
  the failed map? This decision determines whether the item's `State` is still accessible
  after failure.
