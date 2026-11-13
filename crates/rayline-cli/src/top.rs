//! `raydock top`: live request metrics from the local hybrid process.

use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use serde_json::Value;

const TOP_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopRequest {
    pub json: bool,
}

pub async fn render_top(request: &TopRequest) -> io::Result<String> {
    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    render_top_from_home(&home, request).await
}

async fn render_top_from_home(home: &Path, request: &TopRequest) -> io::Result<String> {
    let source = TopSource::connect(&crate::hybrid::state_dir(home)).await?;

    if request.json {
        let snapshot = source.fetch_snapshot().await?;
        return serde_json::to_string_pretty(&snapshot)
            .map(|mut json| {
                json.push('\n');
                json
            })
            .map_err(io::Error::other);
    }

    if !io::stdout().is_terminal() {
        let snapshot = source.fetch_snapshot().await?;
        return Ok(format_top_snapshot(&snapshot));
    }

    run_top_tui(&source).await?;
    Ok(String::new())
}

/// The authenticated snapshot endpoint of one live local process, and the
/// identity every snapshot must carry.
struct TopSource {
    client: reqwest::Client,
    url: String,
    identity: Value,
}

impl TopSource {
    async fn connect(state_dir: &Path) -> io::Result<Self> {
        let (client, url, identity) =
            crate::hybrid::top_connection(state_dir)
                .await
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        format!("no local process is running: {error}"),
                    )
                })?;
        Ok(Self {
            client,
            url,
            identity,
        })
    }

    async fn fetch_snapshot(&self) -> io::Result<Value> {
        let snapshot = fetch_top_snapshot(&self.client, &self.url).await?;
        if snapshot.get("runtime").and_then(Value::as_str) != Some("rayline-local")
            || ["pid", "core_pid", "instance_id"]
                .iter()
                .any(|key| snapshot.get(key) != self.identity.get(key))
        {
            return Err(io::Error::other(
                "local process snapshot/process identity mismatch; restart top",
            ));
        }
        Ok(snapshot)
    }
}

async fn fetch_top_snapshot(client: &reqwest::Client, url: &str) -> io::Result<Value> {
    client
        .get(url)
        .send()
        .await
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("local process metrics endpoint is not available: {error}"),
            )
        })?
        .error_for_status()
        .map_err(io::Error::other)?
        .json::<Value>()
        .await
        .map_err(io::Error::other)
        .and_then(|snapshot| {
            validate_top_snapshot(&snapshot)?;
            Ok(snapshot)
        })
}

/// Reject a snapshot whose shape the renderers cannot safely index. Every top
/// path fetches through `fetch_top_snapshot`, so this is the one gate. The
/// diagnostic never echoes the endpoint's body.
fn validate_top_snapshot(snapshot: &Value) -> io::Result<()> {
    let invalid = |what: &str| {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("local process metrics endpoint returned a malformed snapshot: {what}"),
        ))
    };
    let Some(object) = snapshot.as_object() else {
        return invalid("not a JSON object");
    };
    for key in ["active", "recent"] {
        match object.get(key) {
            Some(Value::Array(rows)) if rows.iter().all(Value::is_object) => {}
            _ => return invalid(&format!("`{key}` is not an array of objects")),
        }
    }
    let Some(Value::Object(totals)) = object.get("totals") else {
        return invalid("`totals` is not an object");
    };
    for key in TOTALS {
        if !totals.get(*key).is_some_and(Value::is_u64) {
            return invalid(&format!("`totals.{key}` is not a count"));
        }
    }
    Ok(())
}

/// The `totals` counts top shows.
const TOTALS: &[&str] = &[
    "completed_requests",
    "errored_requests",
    "local_requests",
    "remote_requests",
    "input_tokens",
    "output_tokens",
];

/// The `key` rows of a snapshot `validate_top_snapshot` accepted.
fn rows<'a>(snapshot: &'a Value, key: &str) -> &'a [Value] {
    snapshot[key]
        .as_array()
        .expect("validate_top_snapshot checked the rows")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TopSort {
    Started,
    Ttft,
    Throughput,
}

impl TopSort {
    fn next(self) -> Self {
        match self {
            Self::Started => Self::Ttft,
            Self::Ttft => Self::Throughput,
            Self::Throughput => Self::Started,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Ttft => "ttft",
            Self::Throughput => "tg",
        }
    }
}

struct TopTerminalGuard;

impl TopTerminalGuard {
    fn enter(stdout: &mut io::Stdout) -> io::Result<Self> {
        terminal::enable_raw_mode().map_err(io::Error::other)?;
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TopTerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

async fn run_top_tui(source: &TopSource) -> io::Result<()> {
    let mut stdout = io::stdout();
    let _guard = TopTerminalGuard::enter(&mut stdout)?;
    let mut snapshot = Value::Null;
    let mut last_error: Option<String> = None;
    let mut last_fetch: Option<Instant> = None;
    let mut paused = false;
    let mut force_refresh = true;
    let mut needs_draw = true;
    let mut sort = TopSort::Started;

    loop {
        let should_refresh = force_refresh
            || (!paused
                && last_fetch
                    .map(|last| last.elapsed() >= TOP_REFRESH_INTERVAL)
                    .unwrap_or(true));
        if should_refresh {
            match source.fetch_snapshot().await {
                Ok(next_snapshot) => {
                    snapshot = next_snapshot;
                    last_error = None;
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                }
            }
            last_fetch = Some(Instant::now());
            force_refresh = false;
            needs_draw = true;
        }

        if needs_draw {
            draw_top(&mut stdout, &snapshot, last_error.as_deref(), paused, sort)?;
            needs_draw = false;
        }

        if event::poll(Duration::from_millis(100)).map_err(io::Error::other)? {
            match event::read().map_err(io::Error::other)? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('p') => {
                        paused = !paused;
                        needs_draw = true;
                    }
                    KeyCode::Char('r') => force_refresh = true,
                    KeyCode::Char('s') => {
                        sort = sort.next();
                        needs_draw = true;
                    }
                    _ => {}
                },
                Event::Resize(_, _) => needs_draw = true,
                _ => {}
            }
        }
    }

    Ok(())
}

fn draw_top(
    stdout: &mut io::Stdout,
    snapshot: &Value,
    last_error: Option<&str>,
    paused: bool,
    sort: TopSort,
) -> io::Result<()> {
    let (width, height) = terminal::size().map_err(io::Error::other)?;
    queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;

    let mut y = 0;
    y = draw_title(stdout, y, width, "Raydock Top")?;
    y = draw_top_summary(stdout, y, width, snapshot, paused, sort)?;
    if let Some(error) = last_error {
        y = draw_colored_line(
            stdout,
            y,
            width,
            Color::Red,
            &format!("last fetch error: {error}"),
        )?;
    }
    y = draw_line(stdout, y, width, "")?;

    let active = sorted_top_rows(snapshot, "active", sort);
    let recent = sorted_top_rows(snapshot, "recent", sort);
    let remaining = height.saturating_sub(y).saturating_sub(1);
    let active_budget = if remaining > 12 {
        remaining / 2
    } else {
        remaining
    };
    y = draw_table_section(
        stdout,
        y,
        width,
        y.saturating_add(active_budget),
        "Active",
        &active,
    )?;
    if y < height.saturating_sub(1) {
        y = draw_line(stdout, y, width, "")?;
    }
    let footer_y = height.saturating_sub(1);
    y = draw_table_section(stdout, y, width, footer_y, "Recent", &recent)?;

    let _ = y;
    let footer = "q/Esc quit  p pause  r refresh  s sort  | --json for scripts";
    queue!(
        stdout,
        MoveTo(0, footer_y),
        Clear(ClearType::CurrentLine),
        SetForegroundColor(Color::DarkGrey),
        Print(fit_width(footer, width as usize)),
        ResetColor
    )?;
    stdout.flush()
}

fn draw_top_summary(
    stdout: &mut io::Stdout,
    mut y: u16,
    width: u16,
    snapshot: &Value,
    paused: bool,
    sort: TopSort,
) -> io::Result<u16> {
    let totals = &snapshot["totals"];
    y = draw_line(
        stdout,
        y,
        width,
        &format!(
            "active={}  recent={}  mode={}  sort={}  refresh={}ms",
            row_count(snapshot, "active"),
            row_count(snapshot, "recent"),
            if paused { "paused" } else { "live" },
            sort.label(),
            TOP_REFRESH_INTERVAL.as_millis(),
        ),
    )?;
    draw_line(
        stdout,
        y,
        width,
        &format!(
            "overall completed={}  errors={}  local={}  remote={}  input={}  output={}",
            totals_u64(totals, "completed_requests"),
            totals_u64(totals, "errored_requests"),
            totals_u64(totals, "local_requests"),
            totals_u64(totals, "remote_requests"),
            totals_u64(totals, "input_tokens"),
            totals_u64(totals, "output_tokens"),
        ),
    )
}

fn draw_table_section(
    stdout: &mut io::Stdout,
    mut y: u16,
    width: u16,
    bottom_exclusive: u16,
    title: &str,
    rows: &[Value],
) -> io::Result<u16> {
    if y >= bottom_exclusive {
        return Ok(y);
    }

    y = draw_heading(
        stdout,
        y,
        width,
        &format!("{title} requests ({})", rows.len()),
    )?;
    if y >= bottom_exclusive {
        return Ok(y);
    }

    y = draw_colored_line(stdout, y, width, Color::DarkGrey, &top_table_header(width))?;
    if rows.is_empty() {
        if y < bottom_exclusive {
            y = draw_line(stdout, y, width, "no requests")?;
        }
        return Ok(y);
    }

    let available = bottom_exclusive.saturating_sub(y) as usize;
    let rendered_rows = rows.len().min(available);
    for row in rows.iter().take(rendered_rows) {
        let rendered = top_table_row(row, width);
        y = match top_row_color(row) {
            Some(color) => draw_colored_line(stdout, y, width, color, &rendered)?,
            None => draw_line(stdout, y, width, &rendered)?,
        };
    }
    if rows.len() > rendered_rows && y < bottom_exclusive {
        y = draw_colored_line(
            stdout,
            y,
            width,
            Color::DarkGrey,
            &format!("... {} more", rows.len() - rendered_rows),
        )?;
    }
    Ok(y)
}

fn draw_title(stdout: &mut io::Stdout, y: u16, width: u16, text: &str) -> io::Result<u16> {
    queue!(
        stdout,
        MoveTo(0, y),
        SetAttribute(Attribute::Bold),
        SetForegroundColor(Color::Cyan),
        Print(fit_width(text, width as usize)),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    Ok(y.saturating_add(1))
}

fn draw_heading(stdout: &mut io::Stdout, y: u16, width: u16, text: &str) -> io::Result<u16> {
    queue!(
        stdout,
        MoveTo(0, y),
        SetAttribute(Attribute::Bold),
        Print(fit_width(text, width as usize)),
        SetAttribute(Attribute::Reset)
    )?;
    Ok(y.saturating_add(1))
}

fn draw_colored_line(
    stdout: &mut io::Stdout,
    y: u16,
    width: u16,
    color: Color,
    text: &str,
) -> io::Result<u16> {
    queue!(
        stdout,
        MoveTo(0, y),
        SetForegroundColor(color),
        Print(fit_width(text, width as usize)),
        ResetColor
    )?;
    Ok(y.saturating_add(1))
}

fn draw_line(stdout: &mut io::Stdout, y: u16, width: u16, text: &str) -> io::Result<u16> {
    queue!(stdout, MoveTo(0, y), Print(fit_width(text, width as usize)))?;
    Ok(y.saturating_add(1))
}

fn top_row_color(row: &Value) -> Option<Color> {
    (value_str(row, "target") == "local").then_some(Color::DarkCyan)
}

fn row_count(snapshot: &Value, key: &str) -> usize {
    rows(snapshot, key).len()
}

fn sorted_top_rows(snapshot: &Value, key: &str, sort: TopSort) -> Vec<Value> {
    let mut rows = rows(snapshot, key).to_vec();
    rows.sort_by(|left, right| {
        top_sort_value(right, sort)
            .partial_cmp(&top_sort_value(left, sort))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

fn top_sort_value(row: &Value, sort: TopSort) -> f64 {
    match sort {
        TopSort::Started => row
            .get("started_at_unix_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0) as f64,
        TopSort::Ttft => row.get("ttft_ms").and_then(Value::as_u64).unwrap_or(0) as f64,
        TopSort::Throughput => row.get("output_tps").and_then(Value::as_f64).unwrap_or(0.0),
    }
}

fn top_table_header(width: u16) -> String {
    if width < 120 {
        format!(
            "{:<14} {:<10} {:<7} {:<10} {:>7} {:>7} {:>7} {:>6} {:>6} MODEL",
            "REQ", "AGENT", "TARGET", "STATE", "OUT", "CACHE %", "TTFT", "PF", "TG"
        )
    } else {
        format!(
            "{:<18} {:<16} {:<12} {:<8} {:<10} {:>7} {:>7} {:>7} {:>8} {:>8} {:>6} {:>6} MODEL / POLICY",
            "REQ",
            "UPSTREAM",
            "AGENT",
            "TARGET",
            "STATE",
            "IN",
            "OUT",
            "CACHE %",
            "AGE",
            "TTFT",
            "PF",
            "TG"
        )
    }
}

fn top_table_row(row: &Value, width: u16) -> String {
    let request_id = truncate_cell(
        value_str(row, "request_id"),
        if width < 120 { 14 } else { 18 },
    );
    let agent = truncate_cell(
        value_str(row, "agent_type"),
        if width < 120 { 10 } else { 12 },
    );
    let target = truncate_cell(top_target_cell(row), 8);
    let state = truncate_cell(value_str(row, "state"), 10);
    let output_tokens = count_cell(row, "output_tokens");
    let ttft = ms_cell(row, "ttft_ms");
    let prefill_tps = rate_cell(row, "prefill_tps");
    let generation_tps = rate_cell(row, "output_tps");
    let cache_hit = percent_cell(row, "cache_hit_ratio");
    let model = top_model_cell(row);

    if width < 120 {
        return format!(
            "{request_id:<14} {agent:<10} {target:<7} {state:<10} {output_tokens:>7} {cache_hit:>7} {ttft:>7} {prefill_tps:>6} {generation_tps:>6} {model}"
        );
    }

    let upstream = truncate_cell(value_str(row, "upstream_request_id"), 16);
    let input_tokens = count_cell(row, "input_tokens");
    let age = ms_cell(row, "duration_ms");
    format!(
        "{request_id:<18} {upstream:<16} {agent:<12} {target:<8} {state:<10} {input_tokens:>7} {output_tokens:>7} {cache_hit:>7} {age:>8} {ttft:>8} {prefill_tps:>6} {generation_tps:>6} {model}"
    )
}

fn format_top_snapshot(snapshot: &Value) -> String {
    let active = row_count(snapshot, "active");
    let recent = row_count(snapshot, "recent");
    let totals = &snapshot["totals"];
    let mut output = format!(
        "Raydock top snapshot\nactive: {active}  recent: {recent}  completed: {}  errors: {}\n",
        totals_u64(totals, "completed_requests"),
        totals_u64(totals, "errored_requests"),
    );
    for row in rows(snapshot, "active") {
        output.push_str(&format_top_row(row));
    }
    if active == 0 {
        output.push_str("no active requests\n");
    }
    output
}

fn format_top_row(row: &Value) -> String {
    let request_id = value_str(row, "request_id");
    let upstream = value_str(row, "upstream_request_id");
    let agent = value_str(row, "agent_type");
    let target = top_target_cell(row);
    let model = value_str(row, "selected_model");
    let state = value_str(row, "state");
    let ttft = ms_cell(row, "ttft_ms");
    let output_tokens = count_cell(row, "output_tokens");
    let prefill_tps = rate_cell(row, "prefill_tps");
    let generation_tps = rate_cell(row, "output_tps");
    let cache_hit = percent_cell(row, "cache_hit_ratio");
    format!(
        "{request_id:<22} upstream={upstream:<16} {agent:<12} {target:<10} {state:<10} out={output_tokens:<7} cache%={cache_hit:<7} ttft={ttft:<8} pf={prefill_tps:<6} tg={generation_tps:<6} {model}\n"
    )
}

fn value_str<'a>(row: &'a Value, key: &str) -> &'a str {
    row.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("-")
}

fn totals_u64(totals: &Value, key: &str) -> u64 {
    totals[key]
        .as_u64()
        .expect("validate_top_snapshot checked the totals")
}

fn count_cell(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_u64)
        .map(|value| format_compact_number(value as f64, 0))
        .unwrap_or_else(|| "-".to_owned())
}

fn ms_cell(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_u64)
        .map(format_ms)
        .unwrap_or_else(|| "-".to_owned())
}

fn format_ms(value: u64) -> String {
    if value < 1000 {
        format!("{value}ms")
    } else {
        format!("{}s", format_compact_number(value as f64 / 1000.0, 1))
    }
}

fn rate_cell(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_f64)
        .map(|value| format_compact_number(value, 1))
        .unwrap_or_else(|| "-".to_owned())
}

fn percent_cell(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_f64)
        .map(|value| format!("{}%", format_compact_number(value * 100.0, 0)))
        .unwrap_or_else(|| "-".to_owned())
}

/// The row's execution target; the hosted router is shown as `cloud`.
fn top_target_cell(row: &Value) -> &str {
    match value_str(row, "target") {
        "remote" => "cloud",
        target => target,
    }
}

/// The model that served the row, then the routing preset when there is one.
/// A row whose execution model is unknown shows `-`, never the requested model.
fn top_model_cell(row: &Value) -> String {
    let model = value_str(row, "selected_model");
    match value_str(row, "policy") {
        "-" => model.to_owned(),
        policy => format!("{model} / {policy}"),
    }
}

fn format_compact_number(value: f64, plain_decimals: usize) -> String {
    if !value.is_finite() {
        return "-".to_owned();
    }

    let units = [
        (1.0, ""),
        (1_000.0, "k"),
        (1_000_000.0, "M"),
        (1_000_000_000.0, "B"),
        (1_000_000_000_000.0, "T"),
    ];
    let abs = value.abs();
    let mut unit_index = 0;
    while unit_index + 1 < units.len() && abs >= units[unit_index + 1].0 {
        unit_index += 1;
    }
    while unit_index + 1 < units.len() && abs / units[unit_index].0 >= 999.95 {
        unit_index += 1;
    }

    let (scale, suffix) = units[unit_index];
    if unit_index == 0 {
        format!("{value:.plain_decimals$}")
    } else {
        format!("{:.1}{suffix}", value / scale)
    }
}

fn truncate_cell(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        value.to_owned()
    } else {
        fit_width(value, width)
    }
}

fn fit_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut chars = text.chars();
    let mut output = String::new();
    for _ in 0..width {
        let Some(ch) = chars.next() else {
            return output;
        };
        output.push(ch);
    }
    if chars.next().is_none() {
        return output;
    }
    output.pop();
    output.push('~');
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{name}-{}-{nanos}", std::process::id()))
    }

    const JSON: TopRequest = TopRequest { json: true };

    async fn hybrid_top_fixture(
        home: &Path,
        health_override: Value,
    ) -> tokio::task::JoinHandle<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state_dir = crate::hybrid::state_dir(home);
        fs::create_dir_all(&state_dir).unwrap();
        let mut state = serde_json::json!({
            "contract_version": 1, "ready": true,
            "pid": std::process::id(), "core_pid": std::process::id(),
            "instance_id": "test-core", "credential_env": [],
            "capabilities": {"context_window": 98304, "max_output_tokens": 4096, "input_modalities": ["text"]},
            "listen_url": format!("http://{}", listener.local_addr().unwrap())
        });
        fs::write(
            state_dir.join("ready.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        fs::write(state_dir.join("gateway-token"), "test-private-token").unwrap();
        for (key, value) in health_override.as_object().unwrap() {
            state[key] = value.clone();
        }
        tokio::spawn(async move {
            let mut snapshots = 0;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buffer = [0u8; 4096];
                let size = stream.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..size]).to_lowercase();
                let authenticated =
                    request.contains("authorization: bearer test-private-token\r\n");
                let health_request = request.starts_with("get /healthz ");
                let mut body = if health_request {
                    state.clone()
                } else if let Some(body) = state.get("snapshot_body") {
                    body.clone()
                } else {
                    serde_json::json!({"runtime": "rayline-local", "instance_id": "test-core",
                        "pid": std::process::id(), "core_pid": std::process::id(),
                        "totals": {"completed_requests": 1, "errored_requests": 0, "local_requests": 0,
                            "remote_requests": 1, "input_tokens": 0, "output_tokens": 12}, "active": [],
                        "recent": [{"request_id": "hybrid_1", "target": "remote", "state": "completed",
                            "selected_model": null, "requested_model": "virtual-model", "output_tokens": 12}]})
                };
                if !health_request {
                    snapshots += 1;
                    if state.get("replace_snapshot_identity") == Some(&Value::Bool(true))
                        && snapshots > 1
                    {
                        body["instance_id"] = Value::from("replacement-core");
                    }
                }
                let body = serde_json::to_vec(&body).unwrap();
                let status = if authenticated {
                    "200 OK"
                } else {
                    "401 Unauthorized"
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
            }
        })
    }

    #[tokio::test]
    async fn top_authenticates_and_preserves_unknown_execution() {
        let home = unique_test_dir("hybrid-top");
        let server = hybrid_top_fixture(&home, serde_json::json!({})).await;
        let rendered = render_top_from_home(&home, &JSON).await.unwrap();
        let snapshot: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(snapshot["runtime"], "rayline-local");
        assert_eq!(snapshot["recent"][0]["selected_model"], Value::Null);
        assert_eq!(snapshot["recent"][0]["output_tokens"], 12);
        assert_eq!(top_target_cell(&snapshot["recent"][0]), "cloud");
        assert!(!top_model_cell(&snapshot["recent"][0]).contains("virtual-model"));

        fs::write(
            crate::hybrid::state_dir(&home).join("gateway-token"),
            "wrong-private-token",
        )
        .unwrap();
        let error = render_top_from_home(&home, &JSON)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("health/authentication"));
        assert!(!error.contains("private-token"));
        server.abort();

        let server =
            hybrid_top_fixture(&home, serde_json::json!({"instance_id": "different-core"})).await;
        assert!(
            render_top_from_home(&home, &JSON)
                .await
                .unwrap_err()
                .to_string()
                .contains("identity")
        );
        server.abort();

        let server = hybrid_top_fixture(
            &home,
            serde_json::json!({"replace_snapshot_identity": true}),
        )
        .await;
        let source = TopSource::connect(&crate::hybrid::state_dir(&home))
            .await
            .unwrap();
        assert!(source.fetch_snapshot().await.is_ok());
        assert!(
            source
                .fetch_snapshot()
                .await
                .unwrap_err()
                .to_string()
                .contains("snapshot/process identity mismatch")
        );
        server.abort();

        let ready_path = crate::hybrid::state_dir(&home).join("ready.json");
        let original: Value = serde_json::from_slice(&fs::read(&ready_path).unwrap()).unwrap();
        for patch in [
            serde_json::json!({"ready": false}),
            serde_json::json!({"pid": 0}),
            serde_json::json!({"core_pid": 0}),
            serde_json::json!({"listen_url": "http://private-token@127.0.0.1:1234"}),
            serde_json::json!({"listen_url": "http://example.com:1234"}),
        ] {
            let mut state = original.clone();
            for (key, value) in patch.as_object().unwrap() {
                state[key] = value.clone();
            }
            fs::write(&ready_path, serde_json::to_vec(&state).unwrap()).unwrap();
            let error = render_top_from_home(&home, &JSON)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("no local process is running"), "{error}");
            assert!(error.contains("readiness") || error.contains("not live"));
            assert!(!error.contains("private-token"));
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn top_without_a_local_process_is_an_error() {
        let home = unique_test_dir("top-no-process");
        let error = render_top_from_home(&home, &JSON).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert!(error.to_string().contains("no local process is running"));
    }

    #[tokio::test]
    async fn malformed_snapshots_are_errors_never_panics() {
        let totals = serde_json::json!({"completed_requests": 0, "errored_requests": 0,
            "local_requests": 0, "remote_requests": 0, "input_tokens": 0, "output_tokens": 0});
        let valid = serde_json::json!({"runtime": "rayline-local", "pid": std::process::id(),
            "core_pid": std::process::id(), "instance_id": "test-core",
            "active": [], "recent": [], "totals": totals});
        validate_top_snapshot(&valid).unwrap();
        let with = |key: &str, value: Value| {
            let mut body = valid.clone();
            body[key] = value;
            body
        };
        let without = |key: &str| {
            let mut body = valid.clone();
            body.as_object_mut().unwrap().remove(key);
            body
        };
        let mut partial_totals = totals.clone();
        partial_totals
            .as_object_mut()
            .unwrap()
            .remove("errored_requests");
        for body in [
            serde_json::json!([]),
            serde_json::json!(0),
            serde_json::json!("secret-body-marker"),
            with("active", serde_json::json!([1])),
            with("recent", serde_json::json!("x")),
            with("totals", serde_json::json!(3)),
            without("active"),
            without("recent"),
            without("totals"),
            with("totals", partial_totals),
            with("totals", serde_json::json!({"completed_requests": -1})),
        ] {
            let home = unique_test_dir("top-malformed");
            let server =
                hybrid_top_fixture(&home, serde_json::json!({ "snapshot_body": body })).await;
            let error = render_top_from_home(&home, &JSON).await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{body}");
            assert!(!error.to_string().contains("secret-body-marker"));
            server.abort();
            fs::remove_dir_all(&home).ok();
        }
    }

    #[test]
    fn top_numeric_cells_use_compact_suffixes() {
        let row = serde_json::json!({
            "input_tokens": 10_200,
            "output_tokens": 1_200_000,
            "duration_ms": 10_200,
            "ttft_ms": 999,
            "prefill_tps": 12_000.2,
            "output_tps": 1_200_000.0,
            "cache_hit_ratio": 0.876,
        });

        assert_eq!(count_cell(&row, "input_tokens"), "10.2k");
        assert_eq!(count_cell(&row, "output_tokens"), "1.2M");
        assert_eq!(ms_cell(&row, "duration_ms"), "10.2s");
        assert_eq!(ms_cell(&row, "ttft_ms"), "999ms");
        assert_eq!(rate_cell(&row, "prefill_tps"), "12.0k");
        assert_eq!(rate_cell(&row, "output_tps"), "1.2M");
        assert_eq!(percent_cell(&row, "cache_hit_ratio"), "88%");
    }

    #[test]
    fn top_table_places_cache_percent_after_token_columns() {
        let header = top_table_header(140);
        let input = header.find("IN").expect("header includes input column");
        let output = header.find("OUT").expect("header includes output column");
        let cache = header
            .find("CACHE %")
            .expect("header includes cache percent column");
        let age = header.rfind("AGE").expect("header includes age column");
        assert!(input < output);
        assert!(output < cache);
        assert!(cache < age);

        let row = serde_json::json!({
            "request_id": "req",
            "agent_type": "main",
            "target": "local",
            "state": "done",
            "selected_model": "model",
            "input_tokens": 10_200,
            "output_tokens": 1_200_000,
            "duration_ms": 10_200,
            "ttft_ms": 999,
            "prefill_tps": 12_000.2,
            "output_tps": 1_200_000.0,
            "cache_hit_ratio": 0.876,
        });

        let cells = top_table_row(&row, 140)
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            &cells[5..12],
            ["10.2k", "1.2M", "88%", "10.2s", "999ms", "12.0k", "1.2M"]
        );
    }

    #[test]
    fn top_table_shows_the_upstream_request_id_after_the_request_id() {
        let header = top_table_header(140);
        let req = header.find("REQ").expect("header includes request column");
        let upstream = header
            .find("UPSTREAM")
            .expect("header includes upstream column");
        let agent = header.find("AGENT").expect("header includes agent column");
        assert!(req < upstream);
        assert!(upstream < agent);

        let row = serde_json::json!({
            "request_id": "req_1",
            "upstream_request_id": "rt_0123456789abcdefghij",
            "agent_type": "main",
            "target": "remote",
            "state": "completed",
            "selected_model": "model",
        });
        let cells = top_table_row(&row, 140)
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(cells[0], "req_1");
        assert_eq!(cells[1].chars().count(), 16);
        assert!(cells[1].starts_with("rt_0123456789"));
        assert_eq!(cells[2], "main");
        assert_eq!(cells[3], "cloud");

        let local = serde_json::json!({"request_id": "req_2", "target": "local"});
        let cells = top_table_row(&local, 140)
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(&cells[..2], ["req_2", "-"]);
        assert!(format_top_row(&local).contains("upstream=-"));
        assert!(format_top_row(&row).contains("upstream=rt_0123456789abcdefghij"));
    }

    #[test]
    fn top_model_cell_shows_the_routing_preset() {
        let row = serde_json::json!({"selected_model": "model", "policy": "balanced"});
        assert_eq!(top_model_cell(&row), "model / balanced");
        let row = serde_json::json!({"selected_model": "model"});
        assert_eq!(top_model_cell(&row), "model");
    }

    #[test]
    fn top_row_color_highlights_local_requests() {
        let local = serde_json::json!({ "target": "local" });
        let remote = serde_json::json!({ "target": "remote" });

        assert_eq!(top_row_color(&local), Some(Color::DarkCyan));
        assert_eq!(top_row_color(&remote), None);
    }
}
