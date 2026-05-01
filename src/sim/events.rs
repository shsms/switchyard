//! World-level event stream — fans out telemetry samples and topology
//! version bumps to UI subscribers (the WebSocket endpoint, future
//! command-line monitors, anything else that wants live notifications).
//!
//! `tokio::sync::broadcast` is multi-producer / multi-consumer with a
//! bounded ring; if a subscriber falls behind by more than the ring
//! capacity it gets a `RecvError::Lagged` and skips ahead. That's the
//! right tradeoff for a UI: the chart is stale anyway when the tab
//! has been backgrounded for an hour, no value in queueing every
//! missed sample.

use serde::Serialize;

/// Capacity of the broadcast ring. Sized to absorb a full second of
/// telemetry samples (~10 metrics × ~50 components) plus headroom
/// for eval bursts. Subscribers that fall further behind than this
/// will see `Lagged` errors and re-sync.
pub const EVENT_BUS_CAPACITY: usize = 4096;

/// A single broadcast event the UI can react to. The discriminator
/// is `kind`; per-variant fields are inlined alongside it (serde
/// `tag` + `flatten`-equivalent via per-variant struct shape).
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorldEvent {
    /// Mutation occurred (eval, reload, …). Subscribers should
    /// refetch /api/topology if they care about structure or
    /// metadata changes. Cheap signal — sent on every accepted eval
    /// regardless of whether the eval actually mutated state.
    TopologyChanged { version: u64 },
    /// Single telemetry sample, emitted by the history sampler.
    /// Wired in the next commit.
    Sample {
        id: u64,
        metric: &'static str,
        ts_ms: i64,
        value: f32,
    },
}
