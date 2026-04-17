# DataPipe Architecture Vision

## Core idea

Replace the current ad-hoc wiring (DataLoop + DataLoopHandle + event_txs_snapshot + set_desired method calls) with a composable pipeline where a single `DataPipe` struct owns the event loop and dispatches typed messages to each stage.

## Shape

```
┌─────────────────────────────────────────────────────┐
│  DataPipe  (owns the loop, owns the channel)        │
│                                                     │
│  on message:                                        │
│    ProcessPoolMsg::SetDesired(specs)  → ProcessPool │
│    ProcessPoolMsg::SendEvent(key, v)  → ProcessPool │
│    StreamValuesMsg::SetDesired(specs) → StreamValues│
│    …                                                │
│                                                     │
│  back-edge (only allowed feedback arc):             │
│    ProcessPool stdout → DataPipe → consumers        │
└─────────────────────────────────────────────────────┘
```

## Key properties

- **DataPipe owns the loop.** No external thread drives it; callers send messages through a channel.
- **All configuration is a message.** `set_desired` becomes `ProcessPoolMsg::SetDesired(specs)` — no separate handle type needed.
- **Stages are equal.** Each stage (ProcessPool, StreamValues, …) receives typed messages and emits outputs. None calls methods on another directly.
- **Single back-edge rule.** Process stdout lines flow back up to DataPipe for routing. All other data flow is top-down. This prevents feedback cycles and mirrors the Elm architecture constraint.
- **`DataLoopHandle` disappears.** Main thread holds a `DataPipeSender` (thin wrapper around the channel); DataPipe fans messages out internally.

## Migration path

1. Define `DataPipeMsg` enum with variants for each stage.
2. Give DataPipe a `Sender<DataPipeMsg>` and run the loop internally.
3. Replace `set_desired` call-sites with `pipe_tx.send(DataPipeMsg::ProcessPool(SetDesired(specs)))`.
4. Replace `event_txs_snapshot` Arc with `DataPipeMsg::ProcessPool(SendEvent(key, value))`.
5. Delete `DataLoopHandle`.

## What this does NOT cover yet

- StreamValues as a managed stage (they're currently just process stdout parsed as JSON — may not need separate treatment).
- Backpressure between stages.
- Per-stage restart/error policy.
