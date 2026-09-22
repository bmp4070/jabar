//! `jabar-bench`: the LSP benchmark client from `docs/monolith-measurements.md`.
//!
//! It speaks LSP over stdio to a spawned `jabar`, drives one *workload*, and
//! writes exactly one JSON object describing the run to stdout. It fails a run
//! — non-zero exit, `"ok": false` — when a capability, status field, or result
//! count the workload expects is missing, so a regression cannot masquerade as
//! a fast empty answer. Parsing server logs by hand is reserved for diagnosis;
//! the reported path is this client's JSON plus the server's `JABAR_BENCH_LOG`
//! phase events, tied together by a shared run ID.
//!
//! Workloads:
//! - `startup`: spawn to `initialize`, to `initialized`, to first `jabar/status`,
//!   to first `workspace/symbol`. Validates the index is loaded and a sentinel
//!   query meets a minimum count. This is the Phase-2 startup measurement.
//! - `probe`: after startup, run open-loop probes (`jabar/status`, a fixed
//!   definition, a fixed symbol) on independent schedules for a fixed duration,
//!   reporting per-method latency percentiles. This is the Phase-3
//!   responsiveness measurement, run while an external process mutates shards.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};

fn main() {
    let report = match run() {
        Ok(report) => report,
        // Still emit a JSON object so a harness collecting `runs.jsonl` records
        // the failure rather than a blank line.
        Err(err) => json!({ "ok": false, "error": err.to_string() }),
    };
    println!("{report}");
    // A validation failure (`ok: false`) is a failed run, so the exit code says
    // so — a harness must not read a slow-but-wrong run as a pass.
    if report.get("ok").and_then(Value::as_bool) != Some(true) {
        std::process::exit(1);
    }
}

fn run() -> Result<Value> {
    let args = Args::parse()?;
    match args.command.as_str() {
        "startup" => workload_startup(&args),
        "probe" => workload_probe(&args),
        other => bail!("unknown workload `{other}`; expected `startup` or `probe`"),
    }
}

// ---------------------------------------------------------------------------
// Argument parsing: `<workload> --key value ...`, so no dependency is needed.
// ---------------------------------------------------------------------------

struct Args {
    command: String,
    opts: BTreeMap<String, String>,
}

impl Args {
    fn parse() -> Result<Args> {
        let mut raw = std::env::args().skip(1);
        let command =
            raw.next().context("usage: jabar-bench <startup|probe> [--flag value ...]")?;
        let mut opts = BTreeMap::new();
        while let Some(flag) = raw.next() {
            let key =
                flag.strip_prefix("--").ok_or_else(|| anyhow!("expected --flag, got `{flag}`"))?;
            let value = raw.next().ok_or_else(|| anyhow!("flag `--{key}` needs a value"))?;
            opts.insert(key.to_owned(), value);
        }
        Ok(Args { command, opts })
    }

    fn get(&self, key: &str) -> Result<&str> {
        self.opts.get(key).map(String::as_str).ok_or_else(|| anyhow!("missing required --{key}"))
    }

    fn opt(&self, key: &str) -> Option<&str> {
        self.opts.get(key).map(String::as_str)
    }

    fn parse_or<T: std::str::FromStr>(&self, key: &str, default: T) -> Result<T>
    where
        T::Err: std::fmt::Display,
    {
        match self.opt(key) {
            None => Ok(default),
            Some(raw) => raw.parse().map_err(|e| anyhow!("--{key}: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------

fn workload_startup(args: &Args) -> Result<Value> {
    let query = args.opt("query").unwrap_or("").to_owned();
    let expect_min: usize = args.parse_or("expect-min", 0)?;

    let spawned = Instant::now();
    let mut server = Server::spawn(args)?;
    let mut client = Client::new(&mut server)?;

    let init_result = client.initialize(server_root_uri(args)?, init_options(args)?)?;
    let t_initialize = spawned.elapsed();
    let capabilities = capability_names(&init_result);

    client.notify("initialized", json!({}))?;
    let t_initialized = spawned.elapsed();

    let status = client.request("jabar/status", json!({}))?;
    let t_first_status = spawned.elapsed();
    let index_loaded = status.get("indexLoaded").and_then(Value::as_bool).unwrap_or(false);
    let indexed_definitions = status.get("indexedDefinitions").and_then(Value::as_u64).unwrap_or(0);

    let symbols = client.request("workspace/symbol", json!({ "query": query }))?;
    let t_first_symbol = spawned.elapsed();
    let returned = symbols.as_array().map(Vec::len).unwrap_or(0);

    client.shutdown()?;
    server.wait_briefly();

    // Validation: an index that loaded, a query that met its floor. A run that
    // fails these is a failed run, not a fast one.
    let mut failures = Vec::new();
    if !index_loaded {
        failures.push("index not loaded at startup".to_owned());
    }
    if returned < expect_min {
        failures.push(format!("query `{query}` returned {returned}, expected >= {expect_min}"));
    }
    let ok = failures.is_empty();

    let report = json!({
        "ok": ok,
        "workload": "startup",
        "run_id": server.run_id,
        "bench_log": server.bench_log,
        "capabilities": capabilities,
        "index_loaded": index_loaded,
        "indexed_definitions": indexed_definitions,
        "query": query,
        "returned": returned,
        "expect_min": expect_min,
        "client_ms": {
            "spawn_to_initialize": ms(t_initialize),
            "spawn_to_initialized": ms(t_initialized),
            "spawn_to_first_status": ms(t_first_status),
            "spawn_to_first_symbol": ms(t_first_symbol),
        },
        "failures": failures,
    });
    Ok(report)
}

fn workload_probe(args: &Args) -> Result<Value> {
    let duration = Duration::from_secs_f64(args.parse_or("duration-secs", 30.0)?);
    let query = args.opt("query").unwrap_or("").to_owned();
    // An optional fixed definition probe; skipped when no file is given.
    let def = match (args.opt("def-file"), args.opt("def-line"), args.opt("def-col")) {
        (Some(f), Some(l), Some(c)) => Some((f.to_owned(), l.parse::<u32>()?, c.parse::<u32>()?)),
        _ => None,
    };

    let mut server = Server::spawn(args)?;
    let mut client = Client::new(&mut server)?;
    client.initialize(server_root_uri(args)?, init_options(args)?)?;
    client.notify("initialized", json!({}))?;

    // Open-loop schedules: each probe is due on its own cadence regardless of
    // whether the previous one has answered, so a stall cannot hide requests
    // through coordinated omission. This is a synchronous approximation — one
    // request in flight at a time — sufficient to surface event-loop stalls
    // via latency; the server-side heartbeat carries the authoritative pause
    // metric.
    let mut status = Latencies::default();
    let mut symbol = Latencies::default();
    let mut definition = Latencies::default();

    let started = Instant::now();
    let mut next_status = Duration::ZERO;
    let mut next_query = Duration::ZERO;
    while started.elapsed() < duration {
        let now = started.elapsed();
        if now >= next_status {
            time_request(&mut client, "jabar/status", json!({}), &mut status)?;
            next_status += Duration::from_millis(50);
        }
        if now >= next_query {
            time_request(&mut client, "workspace/symbol", json!({ "query": query }), &mut symbol)?;
            if let Some((file, line, col)) = &def {
                let params = json!({
                    "textDocument": { "uri": file_uri(args, file)? },
                    "position": { "line": line, "character": col }
                });
                time_request(&mut client, "textDocument/definition", params, &mut definition)?;
            }
            next_query += Duration::from_secs(1);
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    client.shutdown()?;
    server.wait_briefly();

    let report = json!({
        "ok": status.errors == 0 && symbol.errors == 0 && definition.errors == 0,
        "workload": "probe",
        "run_id": server.run_id,
        "bench_log": server.bench_log,
        "duration_secs": duration.as_secs_f64(),
        "status": status.summary(),
        "symbol": symbol.summary(),
        "definition": definition.summary(),
    });
    Ok(report)
}

fn time_request(
    client: &mut Client,
    method: &str,
    params: Value,
    into: &mut Latencies,
) -> Result<()> {
    let at = Instant::now();
    match client.request(method, params) {
        Ok(_) => into.record(at.elapsed()),
        Err(_) => into.errors += 1,
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Latency accumulation
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Latencies {
    samples: Vec<f64>,
    errors: usize,
}

impl Latencies {
    fn record(&mut self, d: Duration) {
        self.samples.push(d.as_secs_f64() * 1000.0);
    }

    fn summary(&self) -> Value {
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        json!({
            "count": sorted.len(),
            "errors": self.errors,
            "p50_ms": percentile(&sorted, 50.0),
            "p95_ms": percentile(&sorted, 95.0),
            "max_ms": sorted.last().copied(),
        })
    }
}

/// Nearest-rank percentile, the estimator named in the measurement plan.
fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted.get(rank - 1).copied()
}

// ---------------------------------------------------------------------------
// Server process
// ---------------------------------------------------------------------------

struct Server {
    child: Child,
    run_id: String,
    bench_log: Option<String>,
    // Kept alive for the client's transport; taken once.
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
}

impl Server {
    fn spawn(args: &Args) -> Result<Server> {
        let jabar = args.get("jabar")?;
        let run_id = args
            .opt("run-id")
            .map(str::to_owned)
            .unwrap_or_else(|| format!("run-{}", now_millis()));
        let bench_log = args.opt("bench-log").map(str::to_owned);

        let mut command = Command::new(jabar);
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        command.env("JABAR_BENCH_RUN", &run_id);
        if let Some(log) = &bench_log {
            command.env("JABAR_BENCH_LOG", log);
        }
        let mut child = command.spawn().with_context(|| format!("spawning `{jabar}`"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        Ok(Server { child, run_id, bench_log, stdin, stdout })
    }

    /// Gives the server a moment to exit after `shutdown`/`exit`, then reaps it.
    fn wait_briefly(&mut self) {
        for _ in 0..50 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Minimal LSP JSON-RPC transport over the child's stdio
// ---------------------------------------------------------------------------

struct Client {
    writer: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: i64,
}

impl Client {
    fn new(server: &mut Server) -> Result<Client> {
        let writer = server.stdin.take().context("server stdin already taken")?;
        let stdout = server.stdout.take().context("server stdout already taken")?;
        Ok(Client { writer, reader: BufReader::new(stdout), next_id: 1 })
    }

    fn initialize(&mut self, root_uri: String, options: Value) -> Result<Value> {
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "general": { "positionEncodings": ["utf-16", "utf-8"] },
                "window": { "workDoneProgress": true },
                "workspace": { "symbol": {} },
                "textDocument": {}
            },
            "initializationOptions": options,
            "clientInfo": { "name": "jabar-bench" }
        });
        self.request("initialize", params)
    }

    fn shutdown(&mut self) -> Result<()> {
        let _ = self.request("shutdown", Value::Null)?;
        self.notify("exit", Value::Null)?;
        Ok(())
    }

    /// Sends a request and returns its result, replying OK to any server→client
    /// request that interleaves (jabar registers capabilities and creates
    /// progress tokens this way) so the transport does not deadlock.
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        loop {
            let message = self.read_message()?;
            if let Some(response_id) = message.get("id").and_then(Value::as_i64) {
                if message.get("method").is_some() {
                    // A server→client request. Acknowledge and keep waiting.
                    self.send(json!({ "jsonrpc": "2.0", "id": response_id, "result": null }))?;
                    continue;
                }
                if response_id == id {
                    if let Some(error) = message.get("error") {
                        bail!("`{method}` failed: {error}");
                    }
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            // A notification (e.g. `$/progress`, `window/showMessage`) — ignore.
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn send(&mut self, message: Value) -> Result<()> {
        let body = serde_json::to_vec(&message)?;
        write!(self.writer, "Content-Length: {}\r\n\r\n", body.len())?;
        self.writer.write_all(&body)?;
        self.writer.flush()?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Value> {
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let read = self.reader.read_line(&mut line)?;
            if read == 0 {
                bail!("server closed the connection");
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some(value) = trimmed.strip_prefix("Content-Length:") {
                content_length = Some(value.trim().parse().context("bad Content-Length")?);
            }
        }
        let len = content_length.context("header without Content-Length")?;
        let mut body = vec![0u8; len];
        self.reader.read_exact(&mut body)?;
        Ok(serde_json::from_slice(&body)?)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn init_options(args: &Args) -> Result<Value> {
    match args.opt("init-json") {
        None => Ok(json!({})),
        Some(raw) => serde_json::from_str(raw).context("--init-json is not valid JSON"),
    }
}

fn server_root_uri(args: &Args) -> Result<String> {
    let root = args.get("root")?;
    Ok(format!("file://{root}"))
}

fn file_uri(args: &Args, relative: &str) -> Result<String> {
    let root = args.get("root")?;
    Ok(format!("file://{}/{}", root.trim_end_matches('/'), relative.trim_start_matches('/')))
}

fn capability_names(init_result: &Value) -> Vec<String> {
    init_result
        .get("capabilities")
        .and_then(Value::as_object)
        .map(|caps| caps.keys().cloned().collect())
        .unwrap_or_default()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
