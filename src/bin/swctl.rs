//! Convenience CLI for poking at a running switchyard server.
//!
//! gRPC commands (default --addr http://[::1]:8800):
//!   swctl info
//!   swctl list
//!   swctl list --category battery
//!   swctl tree
//!   swctl stream 1001
//!   swctl stream 1001 --samples 5 --json
//!   swctl set-power 1001 8000
//!   swctl set-power 1001 -- -5000 --lifetime 30   # negative → discharge
//!   swctl augment-bounds 1001 --lower -15000 --upper 15000 --lifetime 60
//!
//! Scenario commands — HTTP (default --ui-addr http://127.0.0.1:8801):
//!   swctl scenario start "demo"
//!   swctl scenario event outage "bat-1003"
//!   swctl scenario load scenarios/example.lisp
//!   swctl scenario report
//!   swctl scenario events --since 0 --limit 20
//!   swctl scenario summary
//!   swctl scenario stop

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

    /// HTTP endpoint of the simulator's UI server. Used by the
    /// `scenario` subcommand — the scenario lifecycle isn't on
    /// gRPC.
    #[arg(long, default_value = "http://127.0.0.1:8801", global = true)]
    ui_addr: String,

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
        /// Subscribe only to specific metrics. Repeatable. Names are
        /// case-insensitive and match the proto enum either with or
        /// without the `METRIC_` prefix (e.g. `dc_power` or
        /// `METRIC_DC_POWER`). Omitting the flag streams every metric.
        #[arg(long = "metric")]
        metrics: Vec<String>,
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

    /// Drive the running scenario: start / stop / event / load,
    /// and read back report + events. Talks to the UI's HTTP
    /// surface, not gRPC.
    #[command(subcommand)]
    Scenario(ScenarioCmd),
}

#[derive(Subcommand, Debug)]
enum ScenarioCmd {
    /// Begin a fresh scenario. Resets the journal + reporters.
    Start {
        /// Scenario name. Lands as `(scenario-start NAME)`.
        name: String,
    },

    /// Mark the running scenario as ended. Freezes elapsed +
    /// metrics; flushes any active CSV sinks.
    Stop,

    /// Append a journal event. KIND becomes a Lisp symbol; PAYLOAD
    /// is rendered as a string.
    Event {
        /// Event kind (e.g. `outage`, `note`).
        kind: String,
        /// Free-form payload string.
        payload: String,
    },

    /// Load a hand-written scenario file via `(load PATH)`. Path
    /// is resolved against the running config.lisp's load
    /// directory, same as `(load …)` from the REPL.
    Load {
        /// Path to the scenario file (e.g. `scenarios/example.lisp`).
        path: String,
    },

    /// Show lifecycle summary — name, started_at, elapsed, event
    /// count.
    Summary,

    /// Show aggregate metrics: peak / charge / discharge / SoC
    /// stats / 15-min averages.
    Report,

    /// Show recent events in the journal.
    Events {
        /// Cursor: only events with id >= SINCE. Default 0.
        #[arg(long, default_value_t = 0)]
        since: u64,
        /// Cap on returned entries.
        #[arg(long, default_value_t = 50)]
        limit: usize,
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
    // Dispatch scenario commands first — they only need the HTTP
    // client, not a live gRPC channel. Avoids paying for a
    // failing gRPC connect when the user only wants /api/scenario.
    if let Cmd::Scenario(s) = cli.cmd {
        return run_scenario(s, &cli.ui_addr, cli.json).await;
    }
    let mut client = MicrogridClient::connect(cli.addr.clone()).await?;
    match cli.cmd {
        Cmd::Info => cmd_info(&mut client, cli.json).await,
        Cmd::List { category, ids } => cmd_list(&mut client, category, ids, cli.json).await,
        Cmd::Connections { from, to } => cmd_connections(&mut client, from, to, cli.json).await,
        Cmd::Tree => cmd_tree(&mut client).await,
        Cmd::Stream {
            id,
            samples,
            metrics,
        } => cmd_stream(&mut client, id, samples, metrics, cli.json).await,
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
        // Scenario handled before the gRPC connect above.
        Cmd::Scenario(_) => unreachable!(),
    }
}

async fn run_scenario(
    cmd: ScenarioCmd,
    ui_addr: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = reqwest::Client::new();
    match cmd {
        ScenarioCmd::Start { name } => {
            // Quote the name as a Lisp string. We don't accept
            // arbitrary expressions here on purpose — names are
            // labels, not code.
            eval(
                &http,
                ui_addr,
                &format!("(scenario-start {})", lisp_string(&name)),
            )
            .await?;
            println!("scenario-start {name}");
        }
        ScenarioCmd::Stop => {
            eval(&http, ui_addr, "(scenario-stop)").await?;
            println!("scenario-stopped");
        }
        ScenarioCmd::Event { kind, payload } => {
            // KIND → unquoted symbol (matches the (scenario-event
            // 'symbol …) idiom in scenario scripts). PAYLOAD →
            // Lisp string.
            let id = eval(
                &http,
                ui_addr,
                &format!("(scenario-event '{kind} {})", lisp_string(&payload)),
            )
            .await?;
            println!("event id={id}");
        }
        ScenarioCmd::Load { path } => {
            eval(&http, ui_addr, &format!("(load {})", lisp_string(&path))).await?;
            println!("loaded {path}");
        }
        ScenarioCmd::Summary => {
            let s: serde_json::Value = http
                .get(format!("{ui_addr}/api/scenario"))
                .send()
                .await?
                .json()
                .await?;
            print_summary(&s, json);
        }
        ScenarioCmd::Report => {
            let r: serde_json::Value = http
                .get(format!("{ui_addr}/api/scenario/report"))
                .send()
                .await?
                .json()
                .await?;
            print_report(&r, json);
        }
        ScenarioCmd::Events { since, limit } => {
            let e: serde_json::Value = http
                .get(format!("{ui_addr}/api/scenario/events"))
                .query(&[("since", since.to_string()), ("limit", limit.to_string())])
                .send()
                .await?
                .json()
                .await?;
            print_events(&e, json);
        }
    }
    Ok(())
}

/// POST a Lisp expression to /api/eval. Returns the rendered
/// result string on success, or surfaces the error message.
async fn eval(
    http: &reqwest::Client,
    ui_addr: &str,
    expr: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let body: serde_json::Value = http
        .post(format!("{ui_addr}/api/eval"))
        .body(expr.to_string())
        .send()
        .await?
        .json()
        .await?;
    if body["ok"] == true {
        Ok(body["value"].as_str().unwrap_or("").to_string())
    } else {
        let msg = body["error"].as_str().unwrap_or("(unknown)");
        Err(format!("eval failed: {msg}").into())
    }
}

/// Backslash-escape a string for embedding inside Lisp source.
/// Handles the two characters that break a `"…"` literal: `"`
/// and `\`.
fn lisp_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn print_summary(s: &serde_json::Value, json: bool) {
    if json {
        println!("{s:#}");
        return;
    }
    let name = s["name"].as_str().unwrap_or("(none)");
    let started = s["started_at"].as_str().unwrap_or("—");
    let ended = s["ended_at"].as_str().unwrap_or("running");
    let elapsed = s["elapsed_s"].as_f64().unwrap_or(0.0);
    let n = s["event_count"].as_u64().unwrap_or(0);
    println!("name        {name}");
    println!("started_at  {started}");
    println!("ended_at    {ended}");
    println!("elapsed     {elapsed:.1} s");
    println!("events      {n}");
}

fn print_report(r: &serde_json::Value, json: bool) {
    if json {
        println!("{r:#}");
        return;
    }
    fn kw(v: f64) -> String {
        format!("{:.2} kW", v / 1000.0)
    }
    fn kwh(v: f64) -> String {
        format!("{:.2} kWh", v / 1000.0)
    }
    let elapsed = r["scenario_elapsed_s"].as_f64().unwrap_or(0.0);
    let peak = r["peak_main_meter_w"].as_f64().unwrap_or(0.0);
    let chg = r["total_battery_charged_wh"].as_f64().unwrap_or(0.0);
    let dchg = r["total_battery_discharged_wh"].as_f64().unwrap_or(0.0);
    let pv = r["total_pv_produced_wh"].as_f64().unwrap_or(0.0);
    println!("elapsed              {elapsed:.1} s");
    println!("main meter peak      {}", kw(peak));
    println!("battery charged      {}", kwh(chg));
    println!("battery discharged   {}", kwh(dchg));
    println!("PV produced          {}", kwh(pv));
    if let Some(soc) = r["soc_stats"].as_object() {
        let mean = soc["mean_pct"].as_f64().unwrap_or(0.0);
        let median = soc["median_pct"].as_f64().unwrap_or(0.0);
        let mode = soc["mode_pct"]
            .as_u64()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".into());
        println!("SoC mean / median / mode  {mean:.1}% / {median:.1}% / {mode}%");
    }
    if let Some(arr) = r["main_meter_window_averages"].as_array()
        && !arr.is_empty()
    {
        println!("\n15-min main-meter averages (last 6):");
        for w in arr.iter().rev().take(6).collect::<Vec<_>>().iter().rev() {
            let ts = w["window_start"].as_str().unwrap_or("?");
            let avg = w["avg_w"].as_f64().unwrap_or(0.0);
            println!("  {ts}  {}", kw(avg));
        }
    }
    if let Some(arr) = r["per_battery"].as_array()
        && !arr.is_empty()
    {
        println!("\nper battery:");
        for b in arr {
            let id = b["id"].as_u64().unwrap_or(0);
            let c = b["charge_wh"].as_f64().unwrap_or(0.0);
            let d = b["discharge_wh"].as_f64().unwrap_or(0.0);
            println!("  {id}  charge {}  discharge {}", kwh(c), kwh(d));
        }
    }
}

fn print_events(e: &serde_json::Value, json: bool) {
    if json {
        println!("{e:#}");
        return;
    }
    let next = e["next_event_id"].as_u64().unwrap_or(0);
    if let Some(arr) = e["events"].as_array() {
        if arr.is_empty() {
            println!("(no events)");
        } else {
            println!("{:>5}  {:<24}  {:<14}  payload", "id", "ts", "kind");
            for ev in arr {
                let id = ev["id"].as_u64().unwrap_or(0);
                let ts = ev["ts"].as_str().unwrap_or("?");
                let kind = ev["kind"].as_str().unwrap_or("?");
                let payload = ev["payload"].as_str().unwrap_or("");
                println!("{id:>5}  {ts:<24}  {kind:<14}  {payload}");
            }
        }
    }
    println!("\nnext_event_id: {next}");
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
    metric_names: Vec<String>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let filter = if metric_names.is_empty() {
        None
    } else {
        let mut metrics = Vec::with_capacity(metric_names.len());
        for name in &metric_names {
            metrics.push(parse_metric_name(name)? as i32);
        }
        Some(
            switchyard::proto::microgrid::receive_electrical_component_telemetry_stream_request::ComponentTelemetryStreamFilter {
                metrics,
            },
        )
    };
    let req = ReceiveElectricalComponentTelemetryStreamRequest {
        electrical_component_id: id,
        filter,
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
            && got >= n
        {
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
            && let Some(cb) = &b.config_bounds
        {
            return (cb.lower, cb.upper);
        }
    }
    (None, None)
}

fn short_metric(m: Metric) -> String {
    m.as_str_name().trim_start_matches("METRIC_").to_string()
}

/// Resolve a CLI string to the proto `Metric` enum. Accepts both
/// `METRIC_AC_POWER_ACTIVE` and the lowercase `ac_power_active` /
/// `ac-power-active` shorthands; the lookup is case-insensitive and
/// tolerant of `-` vs `_` so users don't have to fight tab-completion.
fn parse_metric_name(s: &str) -> Result<Metric, Box<dyn std::error::Error>> {
    let normalized = s.replace('-', "_").to_ascii_uppercase();
    let with_prefix = if normalized.starts_with("METRIC_") {
        normalized
    } else {
        format!("METRIC_{normalized}")
    };
    Metric::from_str_name(&with_prefix).ok_or_else(|| format!("unknown metric '{s}'").into())
}
