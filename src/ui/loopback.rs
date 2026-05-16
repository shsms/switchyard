//! gRPC loopback supervisor that mirrors switchyard's own gRPC
//! service back through `frequenz-microgrid`'s client + logical-
//! meter actors. Every dashboard formula tile reads from there, so
//! the SPA exercises exactly the same path a downstream EMS would.
//!
//! `spawn_microgrid_loopback` kicks the supervisor task; the
//! supervisor watches `MicrogridSite` events and rebuilds the
//! `Microgrid` handle every time the topology changes (which also
//! resubscribes every forwarder against the new graph).

use std::time::Duration;

use frequenz_microgrid::{
    LogicalMeterConfig, LogicalMeterHandle, Microgrid, MicrogridClientHandle, Sample, metric,
    quantity::Power,
};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::sim::{MicrogridSite, events::SiteEvent};

use super::state::{
    HistorySample, MICROGRID_HISTORY_CAP, MicrogridSampleSnapshot, SharedMicrogrid,
};

/// Spawn a tokio task that constructs a [`Microgrid`] pointed at
/// `grpc_url`, kicks off forwarders for the aggregated streams the
/// Dashboard cares about, and stores the handle in `slot` once the
/// connection succeeds. `Microgrid::try_new` already retries lazily
/// until the gRPC server is reachable; this wrapper exists so the
/// UI's `serve` doesn't block on the gRPC server coming up — UI
/// startup proceeds, and dashboard endpoints return 503 until the
/// slot fills.
///
/// `site` is the sink the forwarders publish to via
/// [`MicrogridSite::broadcast_microgrid_sample`]; the existing `/ws/events`
/// stream then carries the samples to the SPA without any extra
/// wiring — they ride the same `SiteEvent` discriminator the
/// per-component samples already use.
pub fn spawn_microgrid_loopback(grpc_url: String, slot: SharedMicrogrid, site: MicrogridSite) {
    tokio::spawn(async move {
        if !build_microgrid(&grpc_url, &slot, &site).await {
            return;
        }
        log::info!("microgrid loopback: connected + graph built + forwarders running");
        // Watch for topology mutations and rebuild on each. The
        // graph crate's ComponentGraph is snapshotted at try_new
        // time so formulas + subscriptions go stale once the site
        // mutates; rebuilding picks up the new shape.
        run_supervisor(grpc_url, slot, site).await;
    });
}

/// Build a fresh `Microgrid` and wire up its forwarders. Same
/// code path for the initial boot and every subsequent rebuild:
/// `slot.client` is lazily initialised on first call via
/// `MicrogridClientHandle::try_new(grpc_url)`, then reused
/// forever. Each call builds a fresh `LogicalMeterHandle` against
/// the current topology and assembles the `Microgrid` via
/// `new_from_handles`. The old `Microgrid` (replaced in `slot`)
/// drops normally; its `LogicalMeterActor` exits cleanly because
/// it handles a closed instructions channel by breaking out.
///
/// Forwarder subscriptions are awaited synchronously **before** the
/// slot swap. The shared `MicrogridClientActor` caches a
/// `broadcast::Sender` per component and its backing tonic stream
/// task exits the moment it sees `receiver_count == 0` between
/// upstream samples (see
/// <https://github.com/frequenz-floss/frequenz-microgrid-rs/issues/…>).
/// Subscribing the new LM first keeps that count ≥ 1 across the
/// handoff, so the stream task survives and samples reach the new
/// forwarders without a multi-second silence.
///
/// Returns false if the gRPC connect or graph build fails outright
/// (which the crate normally retries through; a hard failure means
/// something like a malformed URL).
async fn build_microgrid(grpc_url: &str, slot: &SharedMicrogrid, site: &MicrogridSite) -> bool {
    // Lazy client init. `MicrogridClientHandle::try_new` doesn't
    // contact the server — the connection is established lazily on
    // the first RPC — so this is cheap to call. It does validate
    // the URL though, hence the Result.
    let client = match slot
        .client
        .get_or_try_init(|| MicrogridClientHandle::try_new(grpc_url.to_owned()))
        .await
    {
        Ok(c) => c.clone(),
        Err(e) => {
            log::error!("microgrid loopback: client try_new failed: {e}");
            return false;
        }
    };
    // 1 Hz sample cadence matches the existing history sampler;
    // dashboard tiles refresh at this rate. LogicalMeterHandle's
    // try_new internally loops on the graph build until it
    // succeeds, so a topology mid-mutation just delays this call
    // rather than returning Err.
    let config = LogicalMeterConfig::new(chrono::TimeDelta::seconds(1));
    let lm = match LogicalMeterHandle::try_new(client.clone(), config).await {
        Ok(lm) => lm,
        Err(e) => {
            log::error!("microgrid loopback: logical-meter setup failed: {e}");
            return false;
        }
    };
    let mut mg = Microgrid::new_from_handles(client, lm);
    let handles = subscribe_power_forwarders(&mut mg, site, slot.clone()).await;
    // Atomic swap. Aborting the old forwarders + dropping the old
    // Microgrid happens AFTER the new LM has subscribed to every
    // component it cares about (above), so the shared client's
    // per-component broadcast Senders never see receiver_count drop
    // to zero between LM generations.
    for h in slot.forwarders.lock().drain(..) {
        h.abort();
    }
    slot.latest.write().clear();
    *slot.forwarders.lock() = handles;
    *slot.microgrid.write() = Some(mg);
    true
}

/// Subscribe to MicrogridSite events and rebuild the Microgrid handle on
/// every TopologyChanged. Lagged-receiver and dropped-sender
/// events also trigger a rebuild (defensive — a missed event
/// might have been a topology change).
async fn run_supervisor(grpc_url: String, slot: SharedMicrogrid, site: MicrogridSite) {
    let mut events = site.subscribe_events();
    loop {
        match events.recv().await {
            Ok(SiteEvent::TopologyChanged { .. }) => {
                debounce_topology_burst(&mut events).await;
                rebuild(&grpc_url, &slot, &site).await;
            }
            Ok(_) => continue,
            Err(RecvError::Lagged(n)) => {
                log::warn!(
                    "microgrid loopback supervisor: lagged {n} events, rebuilding defensively"
                );
                debounce_topology_burst(&mut events).await;
                rebuild(&grpc_url, &slot, &site).await;
            }
            Err(RecvError::Closed) => {
                log::info!("microgrid loopback supervisor: site events closed, exiting");
                return;
            }
        }
    }
}

/// After seeing the first TopologyChanged, swallow any further
/// events that arrive within `DEBOUNCE` so a hot-reload that
/// registers 12 components in rapid succession only triggers one
/// rebuild instead of 12.
async fn debounce_topology_burst(events: &mut tokio::sync::broadcast::Receiver<SiteEvent>) {
    const DEBOUNCE: Duration = Duration::from_millis(300);
    let deadline = tokio::time::Instant::now() + DEBOUNCE;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(_)) => continue, // keep collecting
            Ok(Err(_)) => return,  // broadcast error; supervisor's main loop deals with it
            Err(_) => return,      // deadline; we're done
        }
    }
}

/// Rebuild the `LogicalMeterHandle` so its graph snapshot reflects
/// the new topology. `build_microgrid` does the work — it
/// subscribes the new forwarders first, then atomically aborts the
/// old ones and swaps the slot. The old `Microgrid` stays in the
/// slot until then so the shared client's per-component broadcast
/// Senders keep at least one live receiver across the handoff.
///
/// Only the `LogicalMeterHandle` inside the new Microgrid is
/// rebuilt; the `MicrogridClientHandle` cached in `slot.client` is
/// reused. See the field doc for why the client is long-lived.
async fn rebuild(grpc_url: &str, slot: &SharedMicrogrid, site: &MicrogridSite) {
    log::info!("microgrid loopback: topology changed — rebuilding handle");
    build_microgrid(grpc_url, slot, site).await;
}

/// Build subscriptions for the active-power streams the Dashboard
/// tier-1 (grid), tier-2 (battery pool), tier-3 (PV), and tier-4
/// (consumer + producer aggregates) read from, and spawn one tokio
/// task per surviving subscription to forward samples onto the
/// MicrogridSite event bus.
///
/// Each `formula.subscribe().await` is run on the caller's task so
/// that, when this function returns, the new LM has already
/// subscribed all its required components through the shared
/// client. That keeps `build_microgrid`'s swap step safe: the old
/// `Microgrid` can drop without ever taking the shared client's
/// per-component broadcast receiver count to zero.
///
/// Streams whose underlying category is absent (no PV in the
/// topology, etc.) emit a single `log::info!` and are silently
/// dropped — the Dashboard's matching tile renders as "data
/// unavailable" until that category appears.
async fn subscribe_power_forwarders(
    microgrid: &mut Microgrid,
    site: &MicrogridSite,
    state: SharedMicrogrid,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    let lm = microgrid.logical_meter();
    let metered: [(&'static str, _); 4] = [
        ("grid_power", lm.grid::<metric::AcPowerActive>()),
        ("consumer_power", lm.consumer::<metric::AcPowerActive>()),
        ("producer_power", lm.producer::<metric::AcPowerActive>()),
        ("pv_power", lm.pv::<metric::AcPowerActive>(None)),
    ];
    for (stream, formula) in metered {
        if let Some(h) = subscribe_power_forwarder(stream, formula, site, state.clone()).await {
            handles.push(h);
        }
    }
    // Grid frequency via `lm.grid::<metric::AcFrequency>()` would
    // be the natural way to feed a "Grid frequency" tile, but
    // frequenz-microgrid 0.4.1's LogicalMeterActor's
    // `TypedFormulaResponseSender` branches only on Power /
    // Voltage / ReactivePower / Current — calling `.subscribe()`
    // on the Frequency formula returns `Internal: Can't create
    // TypedFormulaResponseSender for ...Frequency`. See
    // /vagrant/upstream-frequency-formula.md. Until that lands
    // upstream, frequency stays on the per-component
    // /api/history?metric=frequency_hz path.
    // BatteryPool takes &mut self for power() / power_bounds() (it
    // caches subscriber refs); build it once and let it go out of
    // scope after both subscriptions resolve.
    match microgrid.battery_pool(None) {
        Ok(mut pool) => {
            if let Some(h) =
                subscribe_power_forwarder("battery_pool_power", pool.power(), site, state.clone())
                    .await
            {
                handles.push(h);
            }
            // power_bounds returns a Vec<Bounds<Power>>; the
            // forwarder flattens the first envelope into two
            // separate streams so the existing point-sample
            // infrastructure (cache + sparkline) renders both
            // halves without an envelope-shaped payload variant.
            handles.push(spawn_bounds_forwarder(pool.power_bounds(), site, state));
        }
        Err(e) => log::info!("microgrid loopback: battery pool absent — skipping: {e}"),
    }
    handles
}

/// Forward a `Vec<Bounds<Power>>` stream as two point streams
/// `battery_pool_bounds_lower` + `battery_pool_bounds_upper`. The
/// upstream tracker emits a fresh Vec on every telemetry snapshot,
/// so the cadence matches the power forwarders' 1 Hz; sparklines
/// alongside the pool power tile track the same time axis.
///
/// When the Vec is empty (no batteries in the pool) both halves
/// publish `None`. When it has multiple disjoint regions we keep
/// only the outermost envelope — single-region is by far the
/// common case and a multi-region split is a niche signal that the
/// developer-facing dashboard isn't designed around.
fn spawn_bounds_forwarder(
    mut rx: tokio::sync::broadcast::Receiver<Vec<frequenz_microgrid::Bounds<Power>>>,
    site: &MicrogridSite,
    state: SharedMicrogrid,
) -> JoinHandle<()> {
    let site = site.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(envelopes) => {
                    let lower = outer_bound(&envelopes, |b| b.lower(), f32::min);
                    let upper = outer_bound(&envelopes, |b| b.upper(), f32::max);
                    let ts_ms = chrono::Utc::now().timestamp_millis();
                    publish_scalar(
                        "battery_pool_bounds_lower",
                        "Power",
                        "W",
                        lower,
                        ts_ms,
                        &site,
                        &state,
                    );
                    publish_scalar(
                        "battery_pool_bounds_upper",
                        "Power",
                        "W",
                        upper,
                        ts_ms,
                        &site,
                        &state,
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("microgrid loopback: battery_pool_bounds lagged {n} samples");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    log::info!("microgrid loopback: battery_pool_bounds closed; forwarder exiting");
                    return;
                }
            }
        }
    })
}

fn outer_bound(
    envelopes: &[frequenz_microgrid::Bounds<Power>],
    pick: impl Fn(&frequenz_microgrid::Bounds<Power>) -> Option<Power>,
    fold: fn(f32, f32) -> f32,
) -> Option<f32> {
    envelopes
        .iter()
        .filter_map(|b| pick(b).map(|p| p.as_watts()))
        .reduce(fold)
}

/// Subscribe to one Power-valued formula and spawn a forwarder that
/// pushes each `Sample<Power>` onto the MicrogridSite event bus as a
/// `MicrogridSample { stream, quantity: "Power", unit: "W", ... }`
/// event. The `formula.subscribe().await` runs on the caller's task
/// so the LM has actually registered for the component samples by
/// the time we return — see `build_microgrid` for why that ordering
/// matters across rebuilds. Returns `None` (no spawn) if the formula
/// errored at construction (typical for absent categories) or the
/// initial subscribe failed.
async fn subscribe_power_forwarder(
    stream: &'static str,
    formula: Result<frequenz_microgrid::Formula<Power>, frequenz_microgrid::Error>,
    site: &MicrogridSite,
    state: SharedMicrogrid,
) -> Option<JoinHandle<()>> {
    let formula = match formula {
        Ok(f) => f,
        Err(e) => {
            log::info!("microgrid loopback: skip {stream} ({e})");
            return None;
        }
    };
    let mut rx = match formula.subscribe().await {
        Ok(rx) => rx,
        Err(e) => {
            log::warn!("microgrid loopback: subscribe {stream} failed: {e}");
            return None;
        }
    };
    let site = site.clone();
    Some(tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(sample) => publish_power(stream, sample, &site, &state),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("microgrid loopback: {stream} lagged {n} samples");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    log::info!("microgrid loopback: {stream} closed; forwarder exiting");
                    return;
                }
            }
        }
    }))
}

fn publish_power(
    stream: &'static str,
    sample: Sample<Power>,
    site: &MicrogridSite,
    state: &SharedMicrogrid,
) {
    let value = sample.value().map(|p| p.as_watts());
    let ts_ms = sample.timestamp().timestamp_millis();
    publish_scalar(stream, "Power", "W", value, ts_ms, site, state);
}

/// Push a typed scalar onto both the per-stream `latest` cache and
/// the WS event bus. The `quantity` + `unit` pair travels with the
/// sample so the SPA picks the right autoscale family (Power
/// W→kW→MW, Frequency Hz, etc.) without pattern-matching on the
/// stream name.
fn publish_scalar(
    stream: &'static str,
    quantity: &'static str,
    unit: &'static str,
    value: Option<f32>,
    ts_ms: i64,
    site: &MicrogridSite,
    state: &SharedMicrogrid,
) {
    let snapshot = MicrogridSampleSnapshot {
        quantity,
        unit,
        ts_ms,
        value,
    };
    state.latest.write().insert(stream, snapshot);
    // Append to the rolling history ring so the Dashboard tile
    // sparklines have past data to backfill from on page load.
    // Drop the oldest entry when the ring is full.
    {
        let mut history = state.history.write();
        let ring = history.entry(stream).or_default();
        if ring.len() == MICROGRID_HISTORY_CAP {
            ring.pop_front();
        }
        ring.push_back(HistorySample { ts_ms, value });
    }
    site.broadcast_microgrid_sample(stream, quantity, unit, ts_ms, value);
}
