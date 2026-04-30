use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};

use crate::sim::{Category, SimulatedComponent, Telemetry, World};

pub struct Grid {
    id: u64,
    name: String,
    pub rated_fuse_current: u32,
    pub stream_jitter_pct: f32,
}

impl Grid {
    pub fn new(id: u64, rated_fuse_current: u32, stream_jitter_pct: f32) -> Self {
        Self {
            id,
            name: format!("grid-{id}"),
            rated_fuse_current,
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
    fn tick(&self, _world: &World, _now: DateTime<Utc>, _dt: Duration) {}
    fn telemetry(&self, _world: &World) -> Telemetry {
        Telemetry {
            id: self.id,
            category: Some(Category::Grid),
            ..Default::default()
        }
    }

    fn rated_fuse_current(&self) -> Option<u32> {
        Some(self.rated_fuse_current)
    }

    fn stream_jitter_pct(&self) -> f32 {
        self.stream_jitter_pct
    }
}
