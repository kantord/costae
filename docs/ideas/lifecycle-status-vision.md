# Lifecycle Status Vision

> This document covers item status, failure taxonomy, rate limiting, scheduling, and the
> `Outcome` type. It extends the Supervisor trait design (`health-vision.md`) and informs
> scheduling concerns touched on in `pipeline-vision.md`. The blanket-impl question for
> typed metadata is tracked as a spike in GitHub issue #10.

---

## The problem with a single status axis

A naive lifecycle status treats the item as moving along one linear sequence:

```
Starting → Running → Stopping → Stopped
```

This is insufficient. An item can be `Running` (alive, not crashed) while failing to apply
new configuration — an overloaded process, a rate-limited API client, a resource that
accepted old config but rejects the new one. From the outside, "Running" tells you nothing
about whether reconciliation actually succeeded.

**The convergence dimension is the only way to observe reconciliation progress from
outside.** Without it, a Supervisor can report "no errors occurred" while items are running
with stale config or quietly degraded. This is exactly why Kubernetes separates `phase` from
`Ready` conditions — `Running` does not mean ready.

---

## Two orthogonal dimensions

Item status has two independent axes:

### Dimension 1 — Lifecycle phase

Where is the item in its lifecycle?

```rust
enum LifecyclePhase {
    Starting,   // enter() called, not yet operational
    Running,    // underlying resource is alive
    Stopping,   // exit() called, cleanup in progress
    Stopped,    // fully cleaned up
}
```

### Dimension 2 — Convergence

Does the item's current actual state match its desired state?

```rust
enum Convergence {
    Converging,  // working toward desired state, not there yet
    Converged,   // actual state matches desired state
    Diverged,    // cannot currently reach desired state
}
```

Together:

```rust
struct ItemStatus {
    phase:       LifecyclePhase,
    convergence: Convergence,
}
```

These are orthogonal. All combinations are meaningful:

| phase \ convergence | Converging | Converged | Diverged |
|---|---|---|---|
| **Starting** | Normal startup | — | Stuck / crash-looping |
| **Running** | Applying new config | Healthy | Pathological (see below) |
| **Stopping** | Cleaning up | — | Zombie |
| **Stopped** | — | Clean stop | Exit failed, leaked resource |

### No `Unknown` state

There is no `Unknown` variant on either dimension. Two rules replace it:

1. **If the item has no way to check its own status**: assume `Running` + `Converged`.
   Default method implementations encode the optimistic assumption — items that don't
   override them appear healthy. This is correct: if nothing can go wrong, nothing should
   be reported as wrong.

2. **If the item CAN check but the check fails due to an error**: that IS a pathological
   state and must be reported as `Diverged`. A failed health check is not neutral; it is
   information. The item is responsible for translating check failures into `Diverged`.

This pushes ambiguity out of the framework and into the item, where it belongs.

---

## The `ReportsStatus` trait

`ReportsStatus` is an **obligatory part of `Lifecycle`** — not opt-in. As soon as this
design is implemented, every `Lifecycle` implementor also satisfies `ReportsStatus`.

```rust
trait ReportsStatus: Lifecycle {
    /// The type of extra diagnostic metadata this item can report.
    /// Use `()` if you have nothing to report.
    type Metadata: serde::Serialize;

    fn lifecycle_phase(&self, state: &Self::State) -> LifecyclePhase {
        LifecyclePhase::Running   // optimistic default: assume alive
    }

    fn convergence(&self, state: &Self::State) -> Convergence {
        Convergence::Converged    // optimistic default: assume healthy
    }

    /// Extra diagnostic data — anything informative but not actionable by the scheduler.
    /// Queue depth, memory usage, last error message, arbitrary domain data.
    /// Returns None if there is nothing to report.
    fn metadata(&self, state: &Self::State) -> Option<Self::Metadata> {
        None
    }

    /// Materialise a full snapshot. Called by the Supervisor on demand (not continuously).
    fn status_snapshot(&self, state: &Self::State) -> ItemStatus {
        ItemStatus {
            phase:       self.lifecycle_phase(state),
            convergence: self.convergence(state),
        }
    }
}
```

**Zero-cost properties:**
- No storage required. Values are computed on demand — from OS APIs (`waitpid`), from
  external state, from whatever is cheapest for that item.
- Implement only the dimensions you actually know. A process item overrides
  `lifecycle_phase` via `waitpid`. A file item overrides only `convergence`. An item with
  nothing meaningful overrides nothing and accepts the healthy defaults.
- `status_snapshot()` is called only when `health_snapshot()` is requested by an external
  consumer — not on every reconcile cycle.
- The framework converts `Option<Self::Metadata>` to `Option<serde_json::Value>` at the
  Supervisor boundary using `serde_json::to_value`. The trait itself stays decoupled from
  `serde_json`.

### Metadata: actionable vs informative

The two status dimensions (`LifecyclePhase`, `Convergence`) carry only what is **directly
useful for scheduling, monitoring, and alerting behavior**. Everything else — diagnostics,
metrics, domain-specific detail — goes into `Metadata`.

`Metadata` is `serde::Serialize`, making it:
- Storable in a database without the framework understanding its shape
- Freely definable per item — the framework never interprets it
- Accessible to external consumers who deserialize it knowing the concrete type

Implementors who want compile-time safety on their metadata type declare:
```rust
type Metadata = MyDiagnostics;  // a concrete Serialize type
```
Implementors with nothing to report declare:
```rust
type Metadata = ();
fn metadata(&self, state: &Self::State) -> Option<()> { None }
```

See GitHub issue #10 for the open question of whether a separate `ReportsTypedMetadata`
trait is worth supporting, and how to resolve the blanket impl conflict if so.

---

## Pathology taxonomy

### Zombie (Stopping + Diverged)

`exit()` was called but cleanup has not completed. The underlying resource — process, file,
X11 window — is still alive. The item reports `Stopping + Diverged`.

**Terminology**: borrowed from Unix zombie processes (a process that has exited but whose
process table entry has not been reaped by the parent). Kubernetes calls this `Terminating`.

**Current gap**: `exit()` currently returns `()` — exit failures are silently swallowed.
The item is dropped from the store regardless of whether cleanup succeeded. `exit()` must
return `Outcome` (see below) to make zombie status visible and to enable retry semantics
on cleanup failures.

### Configuration drift (Running + Diverged)

The item is alive but cannot apply the latest desired configuration. Examples:
- An overloaded process that rejected a reconfiguration request
- A subprocess still processing a config change
- A rate-limited external resource that refused the update call

Not crashed, no error surfaced through `ReconcileErrors`. Only the convergence dimension
makes this visible.

### Crash loop (oscillating Starting → Diverged → Starting)

The item repeatedly attempts `enter()`, fails, and is re-entered on the next cycle because
it is still in desired state. Without backoff in the item's own state (`RetryBudget`), this
becomes a tight retry loop. Observable over time via `health_snapshot()`.

### Stuck entry (Starting + Converging for too long)

`enter()` was called but the item has not yet become operational — slow network call,
process still initialising, dependency not yet ready. Healthy transience. An item reporting
`Starting + Converging` across many consecutive cycles signals stuck initialisation.

---

## Rate limiting is a special case of Diverged

Rate limiting — whether self-imposed or from an external resource — maps directly onto the
status model without any framework special-casing:

- An item that cannot act due to a rate limit reports `Converging` (still trying) or
  `Diverged` (gave up for now).
- Extra `enter()` calls are acceptable. The item handles its own cadence internally using
  `RetryBudget` in its `State`.
- External shared resources (PR creator, agent dispatcher, GitHub API) are plain Rust
  structs passed through `Context`. An item calls `ctx.agent_pool.try_acquire()`. If
  refused, the item returns `Outcome::Retry(Some(duration))`. No new framework primitives
  needed.
- The framework never knows a rate limit occurred. It only sees the item's reported status
  and the `Outcome` of the lifecycle call.

**Two levels of rate limiting — both handled without framework changes:**

1. **Item-level** (external resource refuses): handled via `Outcome::Retry` and
   `RetryBudget` in the item's state.
2. **Supervisor-level** (cap on how many enters per cycle): the Supervisor controls the
   drain rate of the pending queue. Items in desired state that haven't entered yet wait in
   the pending queue; the Supervisor decides how many to enter per cycle.

---

## The `Outcome` type

The current `Result<_, Error>` return from `enter()` and `reconcile_self()` conflates three
meaningfully different situations. Replace it with:

```rust
enum Outcome<E> {
    Ok,                       // succeeded — item is entered/updated
    Retry(Option<Duration>),  // transient — hold, retry after delay (or immediately)
    Fatal(E),                 // permanent — evict, do not retry until desired state changes
}
```

Mapping:
- **`Ok`**: happy path
- **`Retry`**: rate limited, temporarily unavailable, still initialising, backoff period.
  Item stays in the pending queue or store. Framework applies the specified delay.
- **`Fatal`**: misconfigured, binary missing, permission denied — something that will not
  resolve on its own. Item is evicted. The item reports `Diverged` as its final status.

This eliminates unbounded retry loops on permanent failures without requiring items to
implement their own eviction logic.

`exit()` must also return `Outcome<E>`:
- `Ok` → cleanup complete, item fully removed
- `Retry(duration)` → cleanup still in progress (zombie), try again
- `Fatal(e)` → cleanup will never complete, accept the resource leak and evict

---

## Where errors go

`ReconcileErrors` are **log events**, not framework decision inputs. The framework does not
inspect error content to make policy decisions — the item's reported `ItemStatus` carries
all structural information the Supervisor needs.

`ReconcileErrors` flows to the Supervisor's output event stream (the log stream). Callers
who want to observe failures read the log stream or query `health_snapshot()`. The
`Convergence` dimension in `ItemStatus` is what the scheduler uses, not the error value.

---

## Where `RetryBudget` lives

`RetryBudget` is a **library utility stored in the item's `State`** — not a framework
concept. The framework never touches it. An item that wants backoff-aware retry puts a
`RetryBudget` in its state and consults it during `reconcile_self()` or `enter()` to decide
whether to return `Outcome::Retry` or `Outcome::Fatal`.

Items that don't need retry tracking ignore it entirely.

---

## Priority and scheduling

When `Outcome::Retry(duration)` is returned, the Supervisor holds the item in a **pending
queue** with a wake-up time. Items in desired state that have not yet entered, or are
backing off, live here.

Priority is the ordering policy of the pending queue:
- **FIFO** by default (items entered in declaration order)
- **Weighted** by an `Ord` implementation on the item type, if priority is needed
- The Supervisor controls the drain rate (how many items it calls `enter()` on per cycle),
  giving a clean home for Supervisor-level throughput caps

---

## Open questions

- **Standardising `ItemStatus` vs generic `HealthStatus` on `Supervisor`.** Should the
  Supervisor mandate `type HealthStatus = ItemStatus`, or remain generic? Standardising
  enables generic tooling (dashboards, alerting); keeping it generic preserves flexibility
  for items with fundamentally different status semantics.

- **Blanket impl conflict for typed metadata** (GitHub issue #10). The candidate solution
  of folding `type Metadata: serde::Serialize` directly into `ReportsStatus` as an
  associated type may be sufficient — the spike will confirm.

- **Exit timeout.** How long does a `Stopping + Diverged` item stay in the Supervisor
  before it is forcibly evicted (resource leak accepted)? Should there be a configurable
  exit timeout per item, or a global Supervisor policy?

- **Pending queue location.** Should the pending queue live inside `ManagedSet` (making it
  the store's concern) or in the `Supervisor` above it (keeping `ManagedSet` simple)?

- **`Outcome::Retry` on `exit()` semantics.** If `exit()` returns `Retry`, the Supervisor
  must re-call `exit()` after the delay. This requires the item's `State` to still be
  accessible after the first `exit()` call — which means `exit()` cannot consume `State`
  on `Retry`. The ownership model for partial-exit state needs careful design.
