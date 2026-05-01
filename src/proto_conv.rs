//! Convert switchyard's Rust-side `Telemetry` and `Category` into the
//! proto messages that the Microgrid gRPC service emits.
//!
//! Lives in its own module so the server code stays focused on RPC
//! plumbing.

use std::collections::HashSet;

use prost_types::Timestamp;

use crate::{
    proto::common::{
        metrics::{
            Bounds, Metric, MetricSample, MetricValueVariant, SimpleMetricValue,
            metric_value_variant,
        },
        microgrid::electrical_components::{
            Battery, BatteryType, ElectricalComponent, ElectricalComponentCategory,
            ElectricalComponentCategorySpecificInfo, ElectricalComponentStateCode,
            ElectricalComponentStateSnapshot, ElectricalComponentTelemetry, EvCharger,
            EvChargerType, GridConnectionPoint, Inverter, InverterType, MetricConfigBounds,
            electrical_component_category_specific_info::Kind,
        },
    },
    proto::microgrid::ReceiveElectricalComponentTelemetryStreamResponse,
    sim::{Category, SimulatedComponent, Telemetry},
};

/// Subscriber's metric allowlist. `None` means "all metrics"; `Some`
/// is the set of `Metric as i32` values the client asked for.
pub type MetricFilter<'a> = Option<&'a HashSet<i32>>;

#[inline]
fn allowed(filter: MetricFilter<'_>, metric: Metric) -> bool {
    match filter {
        None => true,
        Some(set) => set.contains(&(metric as i32)),
    }
}

pub fn category_to_proto(c: Category) -> ElectricalComponentCategory {
    match c {
        Category::Grid => ElectricalComponentCategory::GridConnectionPoint,
        Category::Meter => ElectricalComponentCategory::Meter,
        Category::Inverter => ElectricalComponentCategory::Inverter,
        Category::Battery => ElectricalComponentCategory::Battery,
        Category::EvCharger => ElectricalComponentCategory::EvCharger,
        Category::Chp => ElectricalComponentCategory::Chp,
    }
}

/// Build the static, type-defining `ElectricalComponent` for a
/// component (used by `ListElectricalComponents`).
pub fn make_component_proto(c: &dyn SimulatedComponent) -> ElectricalComponent {
    let cat = category_to_proto(c.category());
    let kind = match cat {
        ElectricalComponentCategory::Inverter => Some(Kind::Inverter(Inverter {
            r#type: match c.subtype() {
                Some("solar") | Some("pv") => InverterType::Pv as i32,
                Some("hybrid") => InverterType::Hybrid as i32,
                _ => InverterType::Battery as i32,
            },
        })),
        ElectricalComponentCategory::Battery => Some(Kind::Battery(Battery {
            r#type: match c.subtype() {
                Some("li-ion") => BatteryType::LiIon as i32,
                Some("naion") => BatteryType::NaIon as i32,
                _ => BatteryType::Unspecified as i32,
            },
        })),
        ElectricalComponentCategory::EvCharger => Some(Kind::EvCharger(EvCharger {
            r#type: match c.subtype() {
                Some("ac") => EvChargerType::Ac as i32,
                Some("dc") => EvChargerType::Dc as i32,
                Some("hybrid") => EvChargerType::Hybrid as i32,
                _ => EvChargerType::Unspecified as i32,
            },
        })),
        ElectricalComponentCategory::GridConnectionPoint => {
            Some(Kind::GridConnectionPoint(GridConnectionPoint {
                rated_fuse_current: c.rated_fuse_current().unwrap_or(0),
            }))
        }
        _ => None,
    };

    let mut bounds = Vec::new();
    if let Some((lower, upper)) = c.rated_active_bounds() {
        let metric = if cat == ElectricalComponentCategory::Battery {
            Metric::DcPower
        } else {
            Metric::AcPowerActive
        };
        bounds.push(MetricConfigBounds {
            metric: metric as i32,
            config_bounds: Some(Bounds {
                lower: Some(lower),
                upper: Some(upper),
            }),
        });
        // Reactive config bounds: prefer the trait's reactive_bounds()
        // sampled at full rated active P (the worst-case-most-restrictive
        // headroom). Falls back to the ±max-rated edge for components
        // that don't implement reactive_bounds yet.
        if cat != ElectricalComponentCategory::Battery {
            let p_max = lower.abs().max(upper.abs());
            let (rlo, rhi) = c.reactive_bounds().unwrap_or((-p_max, p_max));
            bounds.push(MetricConfigBounds {
                metric: Metric::AcPowerReactive as i32,
                config_bounds: Some(Bounds {
                    lower: Some(rlo),
                    upper: Some(rhi),
                }),
            });
        }
    }

    ElectricalComponent {
        id: c.id(),
        name: c.name().to_string(),
        category: cat as i32,
        microgrid_id: 0,
        category_specific_info: Some(ElectricalComponentCategorySpecificInfo { kind }),
        metric_config_bounds: bounds,
        ..Default::default()
    }
}

/// Build a streaming telemetry response for a component, optionally
/// limited to a subset of metrics chosen by the subscriber.
pub fn telemetry_to_proto(
    c: &dyn SimulatedComponent,
    t: &Telemetry,
    filter: MetricFilter<'_>,
) -> ReceiveElectricalComponentTelemetryStreamResponse {
    let now = Some(Timestamp::from(std::time::SystemTime::now()));
    let cat = c.category();

    let mut samples = Vec::new();
    let mut states = Vec::new();

    if let Some(s) = t.frequency_hz
        && allowed(filter, Metric::AcFrequency)
    {
        samples.push(simple_sample(now, Metric::AcFrequency, s));
    }
    if let Some((v1, v2, v3)) = t.per_phase_voltage_v {
        if allowed(filter, Metric::AcVoltagePhase1N) {
            samples.push(simple_sample(now, Metric::AcVoltagePhase1N, v1));
        }
        if allowed(filter, Metric::AcVoltagePhase2N) {
            samples.push(simple_sample(now, Metric::AcVoltagePhase2N, v2));
        }
        if allowed(filter, Metric::AcVoltagePhase3N) {
            samples.push(simple_sample(now, Metric::AcVoltagePhase3N, v3));
        }
    }
    if let Some((p1, p2, p3)) = t.per_phase_current_a {
        if allowed(filter, Metric::AcCurrentPhase1) {
            samples.push(simple_sample(now, Metric::AcCurrentPhase1, p1));
        }
        if allowed(filter, Metric::AcCurrentPhase2) {
            samples.push(simple_sample(now, Metric::AcCurrentPhase2, p2));
        }
        if allowed(filter, Metric::AcCurrentPhase3) {
            samples.push(simple_sample(now, Metric::AcCurrentPhase3, p3));
        }
    }
    if let Some((p1, p2, p3)) = t.per_phase_active_w {
        if allowed(filter, Metric::AcPowerActivePhase1) {
            samples.push(simple_sample(now, Metric::AcPowerActivePhase1, p1));
        }
        if allowed(filter, Metric::AcPowerActivePhase2) {
            samples.push(simple_sample(now, Metric::AcPowerActivePhase2, p2));
        }
        if allowed(filter, Metric::AcPowerActivePhase3) {
            samples.push(simple_sample(now, Metric::AcPowerActivePhase3, p3));
        }
    }
    if let Some((q1, q2, q3)) = t.per_phase_reactive_var {
        if allowed(filter, Metric::AcPowerReactivePhase1) {
            samples.push(simple_sample(now, Metric::AcPowerReactivePhase1, q1));
        }
        if allowed(filter, Metric::AcPowerReactivePhase2) {
            samples.push(simple_sample(now, Metric::AcPowerReactivePhase2, q2));
        }
        if allowed(filter, Metric::AcPowerReactivePhase3) {
            samples.push(simple_sample(now, Metric::AcPowerReactivePhase3, q3));
        }
    }
    if let Some(p) = t.active_power_w
        && allowed(filter, Metric::AcPowerActive)
    {
        let mut sample = simple_sample(now, Metric::AcPowerActive, p);
        if let Some(b) = &t.active_power_bounds {
            sample.bounds = b.0.clone();
        }
        samples.push(sample);
    }
    if let Some(q) = t.reactive_power_var
        && allowed(filter, Metric::AcPowerReactive)
    {
        let mut sample = simple_sample(now, Metric::AcPowerReactive, q);
        if let Some((lo, hi)) = t.reactive_power_bounds {
            sample.bounds = vec![Bounds {
                lower: Some(lo),
                upper: Some(hi),
            }];
        }
        samples.push(sample);
    }

    // DC / battery-flavoured samples
    if let Some(cap) = t.capacity_wh
        && allowed(filter, Metric::BatteryCapacity)
    {
        samples.push(simple_sample(now, Metric::BatteryCapacity, cap));
    }
    if let Some(soc) = t.soc_pct
        && allowed(filter, Metric::BatterySocPct)
    {
        let mut s = simple_sample(now, Metric::BatterySocPct, soc);
        if let (Some(l), Some(u)) = (t.soc_lower_pct, t.soc_upper_pct) {
            s.bounds = vec![Bounds {
                lower: Some(l),
                upper: Some(u),
            }];
        }
        samples.push(s);
    }
    if let Some(v) = t.dc_voltage_v
        && allowed(filter, Metric::DcVoltage)
    {
        samples.push(simple_sample(now, Metric::DcVoltage, v));
    }
    if let Some(i) = t.dc_current_a
        && allowed(filter, Metric::DcCurrent)
    {
        samples.push(simple_sample(now, Metric::DcCurrent, i));
    }
    if let Some(p) = t.dc_power_w
        && allowed(filter, Metric::DcPower)
    {
        let mut sample = simple_sample(now, Metric::DcPower, p);
        // Only attach bounds to DC for batteries — for AC components
        // they are attached above.
        if cat == Category::Battery
            && let Some(b) = &t.active_power_bounds
        {
            sample.bounds = b.0.clone();
        }
        samples.push(sample);
    }

    if let Some(s) = t.component_state
        && let Some(code) = parse_state(s)
    {
        states.push(code as i32);
    }
    if let Some(s) = t.relay_state
        && let Some(code) = parse_state(s)
    {
        states.push(code as i32);
    }
    if let Some(s) = t.cable_state
        && let Some(code) = parse_state(s)
    {
        states.push(code as i32);
    }

    ReceiveElectricalComponentTelemetryStreamResponse {
        telemetry: Some(ElectricalComponentTelemetry {
            electrical_component_id: c.id(),
            metric_samples: samples,
            state_snapshots: vec![ElectricalComponentStateSnapshot {
                origin_time: now,
                states,
                ..Default::default()
            }],
        }),
    }
}

fn simple_sample(now: Option<Timestamp>, metric: Metric, value: f32) -> MetricSample {
    MetricSample {
        sample_time: now,
        metric: metric as i32,
        value: Some(MetricValueVariant {
            metric_value_variant: Some(metric_value_variant::MetricValueVariant::SimpleMetric(
                SimpleMetricValue { value },
            )),
        }),
        ..Default::default()
    }
}

fn parse_state(s: &str) -> Option<ElectricalComponentStateCode> {
    use std::str::FromStr;
    ElectricalComponentStateCode::from_str(s).ok()
}
