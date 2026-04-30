//! Combined Heat and Power generator. Currently a marker — its parent
//! meter carries the negative power literal in the same way microsim
//! does.

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};

use crate::sim::{Category, SimulatedComponent, Telemetry, World};

pub struct Chp {
    id: u64,
    name: String,
}

impl Chp {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            name: format!("chp-{id}"),
        }
    }
}

impl fmt::Display for Chp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for Chp {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::Chp
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn stream_interval(&self) -> Duration {
        Duration::from_secs(1)
    }
    fn tick(&self, _w: &World, _n: DateTime<Utc>, _d: Duration) {}
    fn telemetry(&self, _w: &World) -> Telemetry {
        Telemetry {
            id: self.id,
            category: Some(Category::Chp),
            ..Default::default()
        }
    }
}
