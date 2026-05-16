use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};

use crate::sim::{Category, SimulatedComponent, Telemetry, MicrogridSite};

pub struct Grid {
    id: u64,
    name: String,
    pub rated_fuse_current: u32,
    pub rated_active_bounds: Option<(f32, f32)>,
    pub stream_jitter_pct: f32,
}

impl Grid {
    pub fn new(
        id: u64,
        rated_fuse_current: u32,
        rated_active_bounds: Option<(f32, f32)>,
        stream_jitter_pct: f32,
    ) -> Self {
        Self {
            id,
            name: format!("grid-{id}"),
            rated_fuse_current,
            rated_active_bounds,
            stream_jitter_pct,
        }
    }
}

impl fmt::Display for Grid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for Grid {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::Grid
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn stream_interval(&self) -> Duration {
        Duration::from_secs(1)
    }
    fn tick(&self, _world: &MicrogridSite, _now: DateTime<Utc>, _dt: Duration) {}
    /// Grid is a topology root; per-phase voltage / frequency reads
    /// belong on the meter directly downstream of it (those are the
    /// fields a real control app subscribes to). Returning only
    /// id + category here keeps the stream lean and matches how
    /// microsim modelled the grid connection point.
    fn telemetry(&self, _world: &MicrogridSite) -> Telemetry {
        Telemetry {
            id: self.id,
            category: Some(Category::Grid),
            ..Default::default()
        }
    }

    fn rated_fuse_current(&self) -> Option<u32> {
        Some(self.rated_fuse_current)
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        self.rated_active_bounds
    }

    fn stream_jitter_pct(&self) -> f32 {
        self.stream_jitter_pct
    }
}
