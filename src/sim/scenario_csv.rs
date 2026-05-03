//! Per-component CSV telemetry sinks for scenario recording.
//!
//! `(scenario-record-csv DIR)` walks every registered component and
//! opens one CSV file per id under `DIR`. The header is identical
//! across categories — a uniform set of telemetry columns with
//! empty cells where a component doesn't publish that field — so a
//! downstream loader doesn't need per-category dispatch.
//!
//! Each row is written from inside `World::record_history_snapshot`
//! so the cadence matches the existing history sampler (1 Hz). The
//! sink is a `BufWriter<File>`; `(scenario-stop-csv)` and
//! `(scenario-stop)` drop the writers, which flushes and closes
//! the underlying file.

use std::{
    collections::HashMap,
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

use chrono::{DateTime, Utc};

use crate::sim::component::{Category, Telemetry};

const CSV_HEADER: &str = "ts_iso,active_power_w,reactive_power_var,dc_power_w,soc_pct\n";

/// One CSV sink — owns the file handle until dropped.
pub(crate) struct CsvSink {
    writer: BufWriter<File>,
}

impl CsvSink {
    /// Open `dir/<id>-<category>.csv`, write the header, return
    /// the sink. Errors if the directory doesn't exist or isn't
    /// writable.
    pub(crate) fn open(dir: &Path, id: u64, category: Category) -> std::io::Result<Self> {
        let path = dir.join(format!("{id}-{}.csv", category_slug(category)));
        let mut writer = BufWriter::new(File::create(&path)?);
        writer.write_all(CSV_HEADER.as_bytes())?;
        Ok(Self { writer })
    }

    /// Append one row from a telemetry snapshot. Empty cells for
    /// fields the component doesn't publish.
    pub(crate) fn write_row(&mut self, ts: DateTime<Utc>, snap: &Telemetry) -> std::io::Result<()> {
        fn cell(v: Option<f32>) -> String {
            v.map(|x| x.to_string()).unwrap_or_default()
        }
        writeln!(
            self.writer,
            "{},{},{},{},{}",
            ts.to_rfc3339(),
            cell(snap.active_power_w),
            cell(snap.reactive_power_var),
            cell(snap.dc_power_w),
            cell(snap.soc_pct),
        )
    }
}

fn category_slug(c: Category) -> &'static str {
    match c {
        Category::Grid => "grid",
        Category::Meter => "meter",
        Category::Inverter => "inverter",
        Category::Battery => "battery",
        Category::EvCharger => "ev-charger",
        Category::Chp => "chp",
    }
}

/// Bundle of active CSV sinks keyed by component id. Mutated under
/// `World::scenario_csv` lock; the lock pattern is "open for the
/// lifetime of the scenario, drop on stop".
pub(crate) type CsvSinks = HashMap<u64, CsvSink>;
