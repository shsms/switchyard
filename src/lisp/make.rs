//! `(make-grid)`, `(make-meter)`, `(make-battery)`, … — the lisp DSL
//! for building the microgrid topology. Each constructor takes its
//! arguments as a typed plist via tulisp's `AsPlist!` macro and
//! returns a `ComponentHandle` (an opaque `Shared<dyn TulispAny>` on
//! the lisp side).

use std::time::Duration;

use tulisp::{AsPlist, Error, Plist, TulispContext};

use crate::sim::{
    Battery, BatteryInverter, Chp, ComponentHandle, EvCharger, Grid, Meter, SolarInverter, World,
    battery::BatteryConfig, ev_charger::EvChargerConfig, inverter::battery_inverter::BatteryInverterConfig,
    inverter::solar_inverter::SolarInverterConfig,
};

// -----------------------------------------------------------------------------
// make-grid
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct GridArgs {
        id: Option<i64> {= None},
        rated_fuse_current<":rated-fuse-current">: Option<i64> {= None},
        successors: Option<Vec<ComponentHandle>> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-meter
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct MeterArgs {
        id: Option<i64> {= None},
        interval: Option<i64> {= None},
        power: Option<f64> {= None},
        successors: Option<Vec<ComponentHandle>> {= None},
        hidden: Option<bool> {= None},
        reactive_power<":reactive-power">: Option<f64> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-battery
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct BatteryArgs {
        id: Option<i64> {= None},
        interval: Option<i64> {= None},
        capacity_wh<":capacity">: Option<f64> {= None},
        initial_soc<":initial-soc">: Option<f64> {= None},
        soc_lower<":soc-lower">: Option<f64> {= None},
        soc_upper<":soc-upper">: Option<f64> {= None},
        voltage: Option<f64> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-battery-inverter
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct BatteryInverterArgs {
        id: Option<i64> {= None},
        interval: Option<i64> {= None},
        successors: Option<Vec<ComponentHandle>> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        command_delay_ms<":command-delay-ms">: Option<i64> {= None},
        ramp_rate<":ramp-rate">: Option<f64> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-solar-inverter
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct SolarInverterArgs {
        id: Option<i64> {= None},
        interval: Option<i64> {= None},
        sunlight_pct<":sunlight%">: Option<f64> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        command_delay_ms<":command-delay-ms">: Option<i64> {= None},
        ramp_rate<":ramp-rate">: Option<f64> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-ev-charger
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct EvChargerArgs {
        id: Option<i64> {= None},
        interval: Option<i64> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        initial_soc<":initial-soc">: Option<f64> {= None},
        capacity_wh<":capacity">: Option<f64> {= None},
        command_delay_ms<":command-delay-ms">: Option<i64> {= None},
        ramp_rate<":ramp-rate">: Option<f64> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-chp
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct ChpArgs {
        id: Option<i64> {= None},
    }
}

// -----------------------------------------------------------------------------
// Registration
// -----------------------------------------------------------------------------

pub fn register(ctx: &mut TulispContext, world: World) {
    let w = world.clone();
    ctx.defun("make-grid", move |args: Plist<GridArgs>| {
        let a = args.into_inner();
        let id = a.id.map(|x| x as u64).unwrap_or_else(|| w.next_id());
        let grid = Grid::new(id, a.rated_fuse_current.unwrap_or(0) as u32);
        let h = w.register(grid);
        connect_successors(&w, id, &a.successors);
        Ok::<_, Error>(h)
    });

    let w = world.clone();
    ctx.defun("make-meter", move |args: Plist<MeterArgs>| {
        let a = args.into_inner();
        let id = a.id.map(|x| x as u64).unwrap_or_else(|| w.next_id());
        let interval = ms_to_duration(a.interval, 1000);
        let succ_ids: Vec<u64> = a
            .successors
            .as_ref()
            .map(|v| v.iter().map(|h| h.id()).collect())
            .unwrap_or_default();
        let fixed = a.power.map(|p| p as f32);
        let meter = Meter::new(id, interval, succ_ids, fixed);
        let h = w.register(meter);
        if !a.hidden.unwrap_or(false) {
            connect_successors(&w, id, &a.successors);
        }
        Ok::<_, Error>(h)
    });

    let w = world.clone();
    ctx.defun("make-battery", move |args: Plist<BatteryArgs>| {
        let a = args.into_inner();
        let id = a.id.map(|x| x as u64).unwrap_or_else(|| w.next_id());
        let interval = ms_to_duration(a.interval, 1000);
        let mut cfg = BatteryConfig::default();
        if let Some(v) = a.capacity_wh {
            cfg.capacity_wh = v as f32;
        }
        if let Some(v) = a.initial_soc {
            cfg.initial_soc_pct = v as f32;
        }
        if let Some(v) = a.soc_lower {
            cfg.soc_lower_pct = v as f32;
        }
        if let Some(v) = a.soc_upper {
            cfg.soc_upper_pct = v as f32;
        }
        if let Some(v) = a.voltage {
            cfg.voltage_v = v as f32;
        }
        if let Some(v) = a.rated_lower {
            cfg.rated_lower_w = v as f32;
        }
        if let Some(v) = a.rated_upper {
            cfg.rated_upper_w = v as f32;
        }
        Ok::<_, Error>(w.register(Battery::new(id, interval, cfg)))
    });

    let w = world.clone();
    ctx.defun(
        "make-battery-inverter",
        move |args: Plist<BatteryInverterArgs>| {
            let a = args.into_inner();
            let id = a.id.map(|x| x as u64).unwrap_or_else(|| w.next_id());
            let interval = ms_to_duration(a.interval, 1000);
            let mut cfg = BatteryInverterConfig::default();
            if let Some(v) = a.rated_lower {
                cfg.rated_lower_w = v as f32;
            }
            if let Some(v) = a.rated_upper {
                cfg.rated_upper_w = v as f32;
            }
            if let Some(v) = a.command_delay_ms {
                cfg.command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.ramp_rate {
                cfg.ramp_rate_w_per_s = v as f32;
            }
            let succ_ids: Vec<u64> = a
                .successors
                .as_ref()
                .map(|v| v.iter().map(|h| h.id()).collect())
                .unwrap_or_default();
            let inv = BatteryInverter::new(id, interval, cfg, succ_ids);
            let h = w.register(inv);
            connect_successors(&w, id, &a.successors);
            Ok::<_, Error>(h)
        },
    );

    let w = world.clone();
    ctx.defun(
        "make-solar-inverter",
        move |args: Plist<SolarInverterArgs>| {
            let a = args.into_inner();
            let id = a.id.map(|x| x as u64).unwrap_or_else(|| w.next_id());
            let interval = ms_to_duration(a.interval, 1000);
            let mut cfg = SolarInverterConfig::default();
            if let Some(v) = a.sunlight_pct {
                cfg.sunlight_pct = v as f32;
            }
            if let Some(v) = a.rated_lower {
                cfg.rated_lower_w = v as f32;
            }
            if let Some(v) = a.rated_upper {
                cfg.rated_upper_w = v as f32;
            }
            if let Some(v) = a.command_delay_ms {
                cfg.command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.ramp_rate {
                cfg.ramp_rate_w_per_s = v as f32;
            }
            Ok::<_, Error>(w.register(SolarInverter::new(id, interval, cfg)))
        },
    );

    let w = world.clone();
    ctx.defun("make-ev-charger", move |args: Plist<EvChargerArgs>| {
        let a = args.into_inner();
        let id = a.id.map(|x| x as u64).unwrap_or_else(|| w.next_id());
        let interval = ms_to_duration(a.interval, 1000);
        let mut cfg = EvChargerConfig::default();
        if let Some(v) = a.rated_lower {
            cfg.rated_lower_w = v as f32;
        }
        if let Some(v) = a.rated_upper {
            cfg.rated_upper_w = v as f32;
        }
        if let Some(v) = a.initial_soc {
            cfg.initial_soc_pct = v as f32;
        }
        if let Some(v) = a.capacity_wh {
            cfg.capacity_wh = v as f32;
        }
        if let Some(v) = a.command_delay_ms {
            cfg.command_delay = Duration::from_millis(v.max(0) as u64);
        }
        if let Some(v) = a.ramp_rate {
            cfg.ramp_rate_w_per_s = v as f32;
        }
        Ok::<_, Error>(w.register(EvCharger::new(id, interval, cfg)))
    });

    let w = world;
    ctx.defun("make-chp", move |args: Plist<ChpArgs>| {
        let a = args.into_inner();
        let id = a.id.map(|x| x as u64).unwrap_or_else(|| w.next_id());
        Ok::<_, Error>(w.register(Chp::new(id)))
    });
}

fn connect_successors(world: &World, parent: u64, successors: &Option<Vec<ComponentHandle>>) {
    if let Some(list) = successors {
        for child in list {
            world.connect(parent, child.id());
        }
    }
}

fn ms_to_duration(ms: Option<i64>, default_ms: u64) -> Duration {
    Duration::from_millis(ms.map(|x| x.max(0) as u64).unwrap_or(default_ms))
}
