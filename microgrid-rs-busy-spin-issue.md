# `MicrogridClientActor` busy-spins at 100 % CPU after its instructions channel closes

**Crate**: `frequenz-microgrid` 0.4.1
**Source**: `src/client/microgrid_client_actor.rs`

## Summary

When all `MicrogridClientHandle` clones for a given `Microgrid` are dropped,
the `MicrogridClientActor`'s `instructions_rx` mpsc receiver enters the
closed state. The actor's `run` loop's `select!` then polls the channel
via `recv()` which returns `Ready(None)` immediately. The instruction
handler matches `None` with a no-op arm:

```rust
// src/client/microgrid_client_actor.rs:233
None => {}
```

`handle_instruction` returns `Ok(())`; the supervisor's `select!`-loop
goes back around; `recv()` again returns `Ready(None)` immediately; the
handler again no-ops. The future never suspends, so the task pins the
tokio worker thread it lands on at 100 % CPU until the process exits.

The sibling `stream_status_rx` arm handles its own `None` correctly:

```rust
// src/client/microgrid_client_actor.rs:86
None => {
    tracing::error!("MicrogridClientActor: Stream status channel closed, exiting.");
    return;
}
```

A user-facing symptom — observed in a downstream project that holds two
`Microgrid` instances against the same in-process gRPC server and drops
the older one whenever the topology changes — is that each topology
change permanently adds one core's worth of busy-spin work. A 4-edit
session is enough to peg 4 cores.

## Reproduction

```rust
use frequenz_microgrid::{LogicalMeterConfig, Microgrid};

#[tokio::main]
async fn main() {
    // Build, drop, observe. Replace the URL with any reachable
    // Microgrid API server.
    let mg = Microgrid::try_new(
        "http://[::1]:8800",
        LogicalMeterConfig::new(chrono::TimeDelta::seconds(1)),
    )
    .await
    .unwrap();

    // Drop the handle. The actor's instructions_rx closes when the
    // last MicrogridClientHandle clone goes away (a fresh Microgrid
    // holds two clones — one in the outer struct, one carried by
    // the LogicalMeterHandle).
    drop(mg);

    // Hang the test so `top` can show what's burning. After ~1 s
    // (or however long the LogicalMeterActor's resampler takes to
    // notice it has no subscribers) you should see one tokio worker
    // thread pegged at 100 % CPU until ctrl-C.
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
}
```

Adding a `dbg!` at the top of `handle_instruction` shows it firing tens
of thousands of times per second after the drop.

`perf top -p <pid>` after the drop attributes the cycles to
`MicrogridClientActor::run` /
`tokio::sync::mpsc::Receiver::poll_recv` /
`async_block::poll`.

## Suggested fix

Two natural shapes:

**A.** Mirror `stream_status_rx`'s handling — break the loop when the
instructions channel closes:

```rust
None => {
    tracing::warn!(
        "MicrogridClientActor: instructions channel closed, exiting."
    );
    return Err(...);  // or break out of run() from the caller
}
```

The caller's `if let Err(e) = handle_instruction(...).await` already
logs errors; a sentinel `ErrorKind::ChannelClosed` (or simply returning
`Ok(())` from the inner match and breaking from `run`'s loop) keeps the
shape consistent.

**B.** Move the `None`-on-close handling up one level into `run`'s
`select!` arm itself:

```rust
loop {
    select! {
        instruction = self.instructions_rx.recv() => {
            let Some(instruction) = instruction else {
                tracing::warn!(
                    "MicrogridClientActor: instructions channel closed, exiting."
                );
                return;
            };
            if let Err(e) = handle_instruction(
                &mut self.client,
                &mut component_streams,
                instruction,
                stream_status_tx.clone(),
            ).await {
                tracing::error!(...);
            }
        }
        stream_status = stream_status_rx.recv() => { /* unchanged */ }
        now = retry_timer.tick() => { /* unchanged */ }
    }
}
```

Then `handle_instruction` only needs to accept `Instruction` (not
`Option<Instruction>`), which is the cleaner signature.

Either approach restores the cooperative-yield invariant the rest of
the actor maintains.

## Workaround for downstream callers

Keep at least one `MicrogridClientHandle` clone alive for the lifetime
of the process. The cleanest shape is to construct the client once
directly via `MicrogridClientHandle::try_new(url)`, stash it in
shared state, and assemble each fresh `Microgrid` via
`Microgrid::new_from_handles(client.clone(), lm)` — where `lm` is a
new `LogicalMeterHandle::try_new(client.clone(), config)` built
whenever the topology changes. The actor's `recv()` then parks on a
genuinely open channel and the busy-spin doesn't trigger.

```rust
use frequenz_microgrid::{
    LogicalMeterConfig, LogicalMeterHandle, Microgrid, MicrogridClientHandle,
};

// Once, at boot:
let client = MicrogridClientHandle::try_new("http://[::1]:8800").await?;

// On every topology change:
let lm = LogicalMeterHandle::try_new(
    client.clone(),
    LogicalMeterConfig::new(chrono::TimeDelta::seconds(1)),
).await?;
let mg = Microgrid::new_from_handles(client.clone(), lm);
// …subscribe, drop the previous mg, etc. The `client` outlives
// every mg, so its actor's channel never closes.
```

## Environment

- `frequenz-microgrid = 0.4.1`
- `frequenz-microgrid-component-graph = 0.5.0`
- Tokio 1.43.x, multi-threaded runtime.
- Observed on Linux x86_64.
