//! Convenience CLI for poking at a running switchyard server.
//!
//! Examples:
//!   swctl info
//!   swctl list
//!   swctl list --category battery
//!   swctl tree
//!   swctl stream 1001
//!   swctl stream 1001 --samples 5 --json
//!   swctl set-power 1001 8000
//!   swctl set-power 1001 -- -5000 --lifetime 30   # negative → discharge
//!   swctl augment-bounds 1001 --lower -15000 --upper 15000 --lifetime 60

use std::collections::{BTreeMap, BTreeSet};

use clap::{Parser, Subcommand, ValueEnum};
use tonic::transport::Channel;

use switchyard::proto::common::metrics::Metric;
use switchyard::proto::common::microgrid::electrical_components::{
    ElectricalComponent, ElectricalComponentCategory,
    electrical_component_category_specific_info::Kind,
};
use switchyard::proto::microgrid::microgrid_client::MicrogridClient;
use switchyard::proto::microgrid::{
    AugmentElectricalComponentBoundsRequest, ListElectricalComponentConnectionsRequest,
    ListElectricalComponentsRequest, PowerType, ReceiveElectricalComponentTelemetryStreamRequest,
    SetElectricalComponentPowerRequest,
};

#[derive(Parser, Debug)]
#[command(
    name = "swctl",
    about = "Switchyard microgrid client",
    version,
    propagate_version = true
)]
struct Cli {
    /// gRPC endpoint of the simulator.
    #[arg(long, default_value = "http://[::1]:8800", global = true)]
    addr: String,

    /// Emit JSON instead of human-friendly output where applicable.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Show microgrid metadata.
    Info,

    /// List electrical components.
    List {
        /// Filter by category.
        #[arg(long)]
        category: Option<Category>,
        /// Filter by component ID (repeatable).
        #[arg(long = "id")]
        ids: Vec<u64>,
    },

    /// List electrical-component connections.
    Connections {
        /// Filter by source ID (repeatable).
        #[arg(long = "from")]
        from: Vec<u64>,
        /// Filter by destination ID (repeatable).
        #[arg(long = "to")]
        to: Vec<u64>,
    },

    /// Pretty-print the topology as a tree.
    Tree,

    /// Stream telemetry samples for a component.
    Stream {
        /// Component ID.
        id: u64,
        /// Stop after N samples (default: stream forever, Ctrl+C to exit).
        #[arg(long)]
        samples: Option<usize>,
    },

    /// Set the active or reactive power set-point on a component.
    SetPower {
        /// Component ID.
        id: u64,
        /// Power in watts (or VARs with --reactive). Negative = discharge.
        #[arg(allow_hyphen_values = true)]
        power: f32,
        /// Treat power as reactive (VAR) rather than active (W).
        #[arg(long)]
        reactive: bool,
        /// Request lifetime in seconds (10..=900).
        #[arg(long)]
        lifetime: Option<u64>,
    },

    /// Augment a component's active-power bounds for a limited time.
    AugmentBounds {
        /// Component ID.
        id: u64,
        /// New lower bound (W).
        #[arg(long, allow_hyphen_values = true)]
        lower: f32,
        /// New upper bound (W).
        #[arg(long, allow_hyphen_values = true)]
        upper: f32,
        /// Request lifetime in seconds (5..=900).
        #[arg(long, default_value_t = 60)]
        lifetime: u64,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum Category {
    Grid,
    Meter,
    Inverter,
    Battery,
    EvCharger,
    Chp,
}

impl Category {
    fn to_proto(self) -> ElectricalComponentCategory {
        match self {
            Self::Grid => ElectricalComponentCategory::GridConnectionPoint,
            Self::Meter => ElectricalComponentCategory::Meter,
            Self::Inverter => ElectricalComponentCategory::Inverter,
            Self::Battery => ElectricalComponentCategory::Battery,
            Self::EvCharger => ElectricalComponentCategory::EvCharger,
            Self::Chp => ElectricalComponentCategory::Chp,
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = MicrogridClient::connect(cli.addr.clone()).await?;
    match cli.cmd {
        Cmd::Info => cmd_info(&mut client, cli.json).await,
        Cmd::List { category, ids } => cmd_list(&mut client, category, ids, cli.json).await,
        Cmd::Connections { from, to } => cmd_connections(&mut client, from, to, cli.json).await,
        Cmd::Tree => cmd_tree(&mut client).await,
        Cmd::Stream { id, samples } => cmd_stream(&mut client, id, samples, cli.json).await,
        Cmd::SetPower {
            id,
            power,
            reactive,
            lifetime,
        } => cmd_set_power(&mut client, id, power, reactive, lifetime).await,
        Cmd::AugmentBounds {
            id,
            lower,
            upper,
            lifetime,
        } => cmd_augment(&mut client, id, lower, upper, lifetime).await,
    }
}

async fn cmd_info(
    client: &mut MicrogridClient<Channel>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = client.get_microgrid(()).await?.into_inner();
    if json {
        println!("{:#?}", resp);
        return Ok(());
    }
    if let Some(mg) = resp.microgrid {
        println!("microgrid_id   = {}", mg.id);
        println!("enterprise_id  = {}", mg.enterprise_id);
        println!("name           = {}", mg.name);
        println!("status         = {}", mg.status);
        if let Some(t) = mg.create_timestamp {
            println!("created_at     = {}", format_ts(&t));
        }
    } else {
        println!("(no microgrid)");
    }
    Ok(())
}

async fn cmd_list(
    client: &mut MicrogridClient<Channel>,
    category: Option<Category>,
    ids: Vec<u64>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let req = ListElectricalComponentsRequest {
        electrical_component_categories: category
            .map(|c| vec![c.to_proto() as i32])
            .unwrap_or_default(),
        electrical_component_ids: ids,
    };
    let resp = client.list_electrical_components(req).await?.into_inner();
    if json {
        println!("{:#?}", resp.electrical_components);
        return Ok(());
    }
    println!(
        "{:>5}  {:<24}  {:<10}  {:<10}  {:>12}  {:>12}",
        "id", "name", "category", "subtype", "rated_lower", "rated_upper"
    );
    for c in &resp.electrical_components {
        let (lo, hi) = active_bounds(c);
        println!(
            "{:>5}  {:<24}  {:<10}  {:<10}  {:>12}  {:>12}",
            c.id,
            c.name,
            short_category(c.category),
            short_subtype(c),
            fmt_opt(lo),
            fmt_opt(hi),
        );
    }
    Ok(())
}

async fn cmd_connections(
    client: &mut MicrogridClient<Channel>,
    from: Vec<u64>,
    to: Vec<u64>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let req = ListElectricalComponentConnectionsRequest {
        source_electrical_component_ids: from,
        destination_electrical_component_ids: to,
    };
    let resp = client
        .list_electrical_component_connections(req)
        .await?
        .into_inner();
    if json {
        println!("{:#?}", resp.electrical_component_connections);
        return Ok(());
    }
    for c in &resp.electrical_component_connections {
        println!(
            "{} -> {}",
            c.source_electrical_component_id, c.destination_electrical_component_id
        );
    }
    Ok(())
}

async fn cmd_tree(client: &mut MicrogridClient<Channel>) -> Result<(), Box<dyn std::error::Error>> {
    let comps = client
        .list_electrical_components(ListElectricalComponentsRequest::default())
        .await?
        .into_inner()
        .electrical_components;
    let conns = client
        .list_electrical_component_connections(ListElectricalComponentConnectionsRequest::default())
        .await?
        .into_inner()
        .electrical_component_connections;

    let by_id: BTreeMap<u64, &ElectricalComponent> = comps.iter().map(|c| (c.id, c)).collect();
    let mut children: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    let mut has_parent: BTreeSet<u64> = BTreeSet::new();
    for c in &conns {
        children
            .entry(c.source_electrical_component_id)
            .or_default()
            .push(c.destination_electrical_component_id);
        has_parent.insert(c.destination_electrical_component_id);
    }

    let roots: Vec<u64> = by_id
        .keys()
        .copied()
        .filter(|id| !has_parent.contains(id))
        .collect();
    for (i, r) in roots.iter().enumerate() {
        print_tree(*r, &by_id, &children, "", i == roots.len() - 1, true);
    }
    Ok(())
}

fn print_tree(
    id: u64,
    by_id: &BTreeMap<u64, &ElectricalComponent>,
    children: &BTreeMap<u64, Vec<u64>>,
    prefix: &str,
    is_last: bool,
    is_root: bool,
) {
    let connector = if is_root {
        ""
    } else if is_last {
        "└── "
    } else {
        "├── "
    };
    let label = match by_id.get(&id) {
        Some(c) => format!("[{}] {} ({})", c.id, c.name, short_category(c.category)),
        None => format!("[{id}] <unknown>"),
    };
    println!("{prefix}{connector}{label}");
    if let Some(kids) = children.get(&id) {
        let new_prefix = if is_root {
            String::new()
        } else if is_last {
            format!("{prefix}    ")
        } else {
            format!("{prefix}│   ")
        };
        for (i, k) in kids.iter().enumerate() {
            print_tree(*k, by_id, children, &new_prefix, i == kids.len() - 1, false);
        }
    }
}

async fn cmd_stream(
    client: &mut MicrogridClient<Channel>,
    id: u64,
    samples: Option<usize>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let req = ReceiveElectricalComponentTelemetryStreamRequest {
        electrical_component_id: id,
        ..Default::default()
    };
    let mut stream = client
        .receive_electrical_component_telemetry_stream(req)
        .await?
        .into_inner();

    if !json {
        println!(
            "{:<24}  {:<26}  {:>14}  {:<22}",
            "time", "metric", "value", "bounds"
        );
    }

    let mut got = 0usize;
    while let Some(msg) = stream.message().await? {
        let Some(t) = msg.telemetry else { continue };
        if json {
            println!("{:#?}", t);
        } else {
            for s in &t.metric_samples {
                let metric = Metric::try_from(s.metric)
                    .map(short_metric)
                    .unwrap_or_else(|_| format!("METRIC_{}", s.metric));
                let value = s
                    .value
                    .as_ref()
                    .and_then(|v| v.metric_value_variant.as_ref())
                    .and_then(|v| match v {
                        switchyard::proto::common::metrics::metric_value_variant::MetricValueVariant::SimpleMetric(sv) => Some(sv.value),
                        _ => None,
                    });
                let bounds = if s.bounds.is_empty() {
                    String::new()
                } else {
                    s.bounds
                        .iter()
                        .map(|b| format!("[{}, {}]", fmt_opt(b.lower), fmt_opt(b.upper)))
                        .collect::<Vec<_>>()
                        .join(",")
                };
                let ts = s
                    .sample_time
                    .as_ref()
                    .map(format_ts)
                    .unwrap_or_else(|| "-".into());
                let val_str = match value {
                    Some(v) => format!("{:>14.2}", v),
                    None => format!("{:>14}", "-"),
                };
                println!("{ts:<24}  {metric:<26}  {val_str}  {bounds}");
            }
        }
        got += 1;
        if let Some(n) = samples
            && got >= n {
                break;
            }
    }
    Ok(())
}

async fn cmd_set_power(
    client: &mut MicrogridClient<Channel>,
    id: u64,
    power: f32,
    reactive: bool,
    lifetime: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let req = SetElectricalComponentPowerRequest {
        electrical_component_id: id,
        power,
        power_type: if reactive {
            PowerType::Reactive as i32
        } else {
            PowerType::Active as i32
        },
        request_lifetime: lifetime,
    };
    let mut stream = client
        .set_electrical_component_power(req)
        .await?
        .into_inner();
    while let Some(msg) = stream.message().await? {
        let name =
            switchyard::proto::microgrid::SetElectricalComponentPowerRequestStatus::try_from(
                msg.status,
            )
            .map(|s| {
                s.as_str_name()
                    .trim_start_matches("SET_ELECTRICAL_COMPONENT_POWER_REQUEST_STATUS_")
                    .to_string()
            })
            .unwrap_or_else(|_| msg.status.to_string());
        println!("status: {name}");
        if let Some(t) = msg.valid_until_time {
            println!("valid_until: {}", format_ts(&t));
        }
    }
    Ok(())
}

async fn cmd_augment(
    client: &mut MicrogridClient<Channel>,
    id: u64,
    lower: f32,
    upper: f32,
    lifetime: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    use switchyard::proto::common::metrics::Bounds;
    let req = AugmentElectricalComponentBoundsRequest {
        electrical_component_id: id,
        target_metric: Metric::AcPowerActive as i32,
        bounds: vec![Bounds {
            lower: Some(lower),
            upper: Some(upper),
        }],
        request_lifetime: Some(lifetime),
    };
    let resp = client
        .augment_electrical_component_bounds(req)
        .await?
        .into_inner();
    if let Some(t) = resp.valid_until_time {
        println!("valid_until: {}", format_ts(&t));
    } else {
        println!("(no expiry returned)");
    }
    Ok(())
}

// ---------- formatting helpers --------------------------------------------

fn fmt_opt(v: Option<f32>) -> String {
    v.map(|x| format!("{x}")).unwrap_or_else(|| "*".into())
}

fn format_ts(t: &prost_types::Timestamp) -> String {
    use chrono::{DateTime, Utc};
    let secs = t.seconds;
    let nanos = t.nanos.max(0) as u32;
    DateTime::<Utc>::from_timestamp(secs, nanos)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| format!("{secs}.{nanos:09}"))
}

fn short_category(cat: i32) -> &'static str {
    match ElectricalComponentCategory::try_from(cat) {
        Ok(ElectricalComponentCategory::GridConnectionPoint) => "grid",
        Ok(ElectricalComponentCategory::Meter) => "meter",
        Ok(ElectricalComponentCategory::Inverter) => "inverter",
        Ok(ElectricalComponentCategory::Battery) => "battery",
        Ok(ElectricalComponentCategory::EvCharger) => "ev",
        Ok(ElectricalComponentCategory::Chp) => "chp",
        _ => "?",
    }
}

fn short_subtype(c: &ElectricalComponent) -> String {
    let Some(info) = c.category_specific_info.as_ref() else {
        return String::new();
    };
    let Some(kind) = info.kind.as_ref() else {
        return String::new();
    };
    match kind {
        Kind::Inverter(i) => format!("{:?}", i.r#type()),
        Kind::Battery(b) => format!("{:?}", b.r#type()),
        Kind::EvCharger(e) => format!("{:?}", e.r#type()),
        Kind::GridConnectionPoint(g) => format!("fuse={}", g.rated_fuse_current),
        _ => String::new(),
    }
}

fn active_bounds(c: &ElectricalComponent) -> (Option<f32>, Option<f32>) {
    for b in &c.metric_config_bounds {
        let metric = Metric::try_from(b.metric).ok();
        if matches!(metric, Some(Metric::AcPowerActive) | Some(Metric::DcPower))
            && let Some(cb) = &b.config_bounds {
                return (cb.lower, cb.upper);
            }
    }
    (None, None)
}

fn short_metric(m: Metric) -> String {
    m.as_str_name().trim_start_matches("METRIC_").to_string()
}
