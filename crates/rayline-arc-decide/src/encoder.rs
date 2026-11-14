//! A [`Readouts`] source backed by a local ARC encoder server.
//!
//! The server answers `POST /encode` with the encoder's final-layer state
//! at each requested token offset, as raw little-endian float32 rows
//! (`x-rows` by `x-dim`). Each call here is a one-shot encode that keeps no
//! session on the server. The client speaks plain HTTP/1.1 to a host and
//! port and is meant for a server on this machine.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::decide::{ReadoutRequest, Readouts};

/// The largest response body accepted (rows x width x 4 bytes).
const MAX_BODY_BYTES: usize = 1 << 30;

/// A response's status, headers (names lowercased) and body.
type Response = (u16, Vec<(String, String)>, Vec<u8>);

/// What one episode's retained encoder session holds: the tokens encoded so
/// far and the rows already read from them.
#[derive(Debug, Default)]
struct Held {
    tokens: Vec<u32>,
    rows: HashMap<usize, Vec<f32>>,
}

/// The most episodes whose sessions this client tracks; the encoder holds
/// fewer and saves the rest to disk.
const MAX_EPISODES: usize = 32;

#[derive(Debug, Clone)]
pub struct LocalEncoder {
    /// `host:port`, shared by every clone so a restarted encoder on a new
    /// port is reached by all of them ([`Self::set_url`]).
    authority: Arc<RwLock<String>>,
    timeout: Duration,
    /// Called when the encoder refuses an input longer than its context.
    on_context_full: Option<Hook>,
    /// Per episode, so a request that extends the last one appends only the
    /// new tokens to the encoder's retained session. Each episode has its own
    /// lock, so one episode's encode never waits on another's.
    held: Arc<Mutex<HashMap<String, Arc<Mutex<Held>>>>>,
    /// Timings of each `/encode` call, kept only after [`Self::with_stats`].
    stats: Option<Arc<Mutex<Vec<EncodeStats>>>>,
}

/// One `/encode` call as the client saw it.
#[derive(Debug, Clone, Default)]
pub struct EncodeStats {
    /// Tokens sent (the appended tail on an append).
    pub tokens: usize,
    /// Rows asked for.
    pub rows: usize,
    pub request_bytes: usize,
    pub response_bytes: usize,
    /// Building the JSON body.
    pub serialize_ms: f64,
    /// Connect, send, and read the whole response.
    pub http_ms: f64,
    /// Turning the response bytes into rows.
    pub parse_ms: f64,
    /// The server's own encode time (`x-timing-ms`).
    pub server_ms: Option<f64>,
    /// Tokens the server restored from a saved session (`x-restored-tokens`).
    pub restored_tokens: Option<usize>,
}

impl LocalEncoder {
    /// Keep a timing record per `/encode` call, read with
    /// [`Self::take_stats`].
    pub fn with_stats(mut self) -> Self {
        self.stats = Some(Arc::default());
        self
    }

    /// The timing records since the last call.
    pub fn take_stats(&self) -> Vec<EncodeStats> {
        self.stats
            .as_ref()
            .and_then(|stats| stats.lock().ok().map(|mut s| std::mem::take(&mut *s)))
            .unwrap_or_default()
    }
}

/// A callback, shown in `Debug` output by name only.
#[derive(Clone)]
struct Hook(Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hook")
    }
}

/// The `host:port` of an `http://host:port` URL with no path.
fn authority_of(url: &str) -> Result<String, String> {
    url.strip_prefix("http://")
        .map(|rest| rest.trim_end_matches('/'))
        .filter(|rest| !rest.is_empty() && !rest.contains('/'))
        .map(str::to_owned)
        .ok_or_else(|| format!("not an http://host:port URL: {url}"))
}

impl LocalEncoder {
    /// `url` is `http://host:port`, with no path.
    pub fn new(url: &str) -> Result<Self, String> {
        Ok(Self {
            authority: Arc::new(RwLock::new(authority_of(url)?)),
            timeout: Duration::from_secs(600),
            on_context_full: None,
            held: Arc::default(),
            stats: None,
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Call `hook` whenever the encoder refuses an input as longer than its
    /// context (`exceeds_kv_budget`). The request still fails.
    pub fn on_context_full(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_context_full = Some(Hook(Arc::new(hook)));
        self
    }

    /// Send this and every clone's requests to `url` from now on (an encoder
    /// restarted on another port). Episodes it still holds resume from their
    /// saved state there.
    pub fn set_url(&self, url: &str) -> Result<(), String> {
        let authority = authority_of(url)?;
        *self
            .authority
            .write()
            .map_err(|_| "the encoder address is poisoned".to_owned())? = authority;
        Ok(())
    }

    fn post(&self, path: &str, body: &[u8]) -> Result<Response, String> {
        let authority = self
            .authority
            .read()
            .map_err(|_| "the encoder address is poisoned".to_owned())?
            .clone();
        let io = |e: std::io::Error| format!("encoder at {authority}: {e}");
        let mut stream = TcpStream::connect(&authority).map_err(io)?;
        stream.set_read_timeout(Some(self.timeout)).map_err(io)?;
        stream.set_write_timeout(Some(self.timeout)).map_err(io)?;
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).map_err(io)?;
        stream.write_all(body).map_err(io)?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).map_err(io)?;
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| "the encoder sent no HTTP status".to_owned())?;
        let mut headers = Vec::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).map_err(io)? == 0 {
                break;
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
            }
        }
        let header = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
        };
        if header("transfer-encoding").is_some_and(|v| !v.eq_ignore_ascii_case("identity")) {
            return Err(
                "the encoder answered with a transfer encoding this client does not read".into(),
            );
        }
        let mut payload = Vec::new();
        match header("content-length").map(str::parse::<usize>) {
            Some(Ok(length)) if length <= MAX_BODY_BYTES => {
                payload.resize(length, 0);
                reader.read_exact(&mut payload).map_err(io)?;
            }
            Some(_) => return Err("the encoder's response length is unusable".into()),
            None => {
                reader
                    .take(MAX_BODY_BYTES as u64)
                    .read_to_end(&mut payload)
                    .map_err(io)?;
            }
        }
        Ok((status, headers, payload))
    }
}

impl LocalEncoder {
    /// The rows at `offsets`, encoding `tokens[base..]` (the rest is the
    /// session's already). `session` `None` is a one-shot encode.
    fn encode(
        &self,
        session: Option<&str>,
        base: usize,
        tokens: &[u32],
        offsets: &[usize],
    ) -> Result<Result<Vec<Vec<f32>>, u16>, String> {
        let started = std::time::Instant::now();
        let ms = |since: std::time::Instant| since.elapsed().as_secs_f64() * 1e3;
        let mut body = serde_json::json!({
            "base": base,
            "tokens": &tokens[base..],
            "offsets": offsets,
        });
        match session {
            Some(session) => body["session"] = serde_json::json!(session),
            None => body["retain"] = serde_json::json!(false),
        }
        let body = body.to_string();
        let serialize_ms = ms(started);
        let sent = std::time::Instant::now();
        let (status, headers, payload) = self.post("/encode", body.as_bytes())?;
        let http_ms = ms(sent);
        if status == 409 {
            return Ok(Err(status));
        }
        if status != 200 {
            // The body of a refusal is a short JSON code, never request text.
            let detail = String::from_utf8_lossy(&payload[..payload.len().min(200)]).into_owned();
            if status == 422 && detail.contains("exceeds_kv_budget") {
                if let Some(Hook(hook)) = &self.on_context_full {
                    hook();
                }
            }
            return Err(format!("the encoder answered {status}: {detail}"));
        }
        let count = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n == name)
                .and_then(|(_, v)| v.parse::<usize>().ok())
                .ok_or_else(|| format!("the encoder's response has no {name}"))
        };
        let (rows, dim) = (count("x-rows")?, count("x-dim")?);
        let stride = dim.checked_mul(4).filter(|stride| *stride > 0);
        let Some(stride) = stride.filter(|stride| {
            rows == offsets.len() && rows.checked_mul(*stride) == Some(payload.len())
        }) else {
            return Err("the encoder's rows do not match the request".into());
        };
        let parsed = std::time::Instant::now();
        let rows: Vec<Vec<f32>> = payload
            .chunks_exact(stride)
            .map(|row| {
                row.chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect()
            })
            .collect();
        if let Some(stats) = &self.stats {
            let header = |name: &str| {
                headers
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.as_str())
            };
            let record = EncodeStats {
                tokens: tokens.len() - base,
                rows: rows.len(),
                request_bytes: body.len(),
                response_bytes: payload.len(),
                serialize_ms,
                http_ms,
                parse_ms: ms(parsed),
                server_ms: header("x-timing-ms").and_then(|v| v.parse().ok()),
                restored_tokens: header("x-restored-tokens").and_then(|v| v.parse().ok()),
            };
            if let Ok(mut stats) = stats.lock() {
                stats.push(record);
            }
        }
        Ok(Ok(rows))
    }
}

impl Readouts for LocalEncoder {
    fn readouts(&self, request: &ReadoutRequest<'_>) -> Result<Vec<Vec<f32>>, String> {
        let (tokens, offsets) = (request.tokens, request.offsets);
        let Some(episode) = request.episode.filter(|e| !e.is_empty()) else {
            return self
                .encode(None, 0, tokens, offsets)?
                .map_err(|status| format!("the encoder answered {status}"));
        };
        // The table's lock is held only to find the episode. The episode's
        // own lock is held across its encode, so a concurrent request for
        // the same episode cannot interleave.
        let episode_state = {
            let mut held = self
                .held
                .lock()
                .map_err(|_| "the encoder session table is poisoned".to_owned())?;
            if !held.contains_key(episode) && held.len() >= MAX_EPISODES {
                held.clear();
            }
            held.entry(episode.to_owned()).or_default().clone()
        };
        let mut state = episode_state
            .lock()
            .map_err(|_| "the encoder session is poisoned".to_owned())?;
        let base = state.tokens.len();
        // The same input again (a retry, or another decision on one turn):
        // its rows are already here.
        if base > 0 && state.tokens == tokens {
            let rows: Option<Vec<Vec<f32>>> = offsets
                .iter()
                .map(|offset| state.rows.get(offset).cloned())
                .collect();
            if let Some(rows) = rows {
                return Ok(rows);
            }
        }
        let appends = base > 0
            && base < tokens.len()
            && tokens.starts_with(&state.tokens)
            && offsets
                .iter()
                .all(|offset| *offset >= base || state.rows.contains_key(offset));
        if appends {
            let fresh: Vec<usize> = offsets.iter().copied().filter(|o| *o >= base).collect();
            let answered = if fresh.is_empty() {
                Ok(Vec::new())
            } else {
                self.encode(Some(episode), base, tokens, &fresh)?
            };
            if let Ok(rows) = answered {
                state.tokens = tokens.to_vec();
                state.rows.extend(fresh.into_iter().zip(rows));
                return offsets
                    .iter()
                    .map(|offset| {
                        state
                            .rows
                            .get(offset)
                            .cloned()
                            .ok_or_else(|| "the encoder session lost a row".to_owned())
                    })
                    .collect();
            }
            // 409: the encoder no longer holds the session; start it again.
        }
        *state = Held::default();
        let rows = self
            .encode(Some(episode), 0, tokens, offsets)?
            .map_err(|status| format!("the encoder answered {status}"))?;
        state.tokens = tokens.to_vec();
        state.rows = offsets.iter().copied().zip(rows.iter().cloned()).collect();
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn reads_rows_from_a_local_server() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let address = listener.local_addr().expect("a bound address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one connection");
            let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
            let mut length = 0;
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).expect("a header line");
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().expect("a length");
                }
                if line.trim_end().is_empty() {
                    break;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("the body");
            let sent: serde_json::Value = serde_json::from_slice(&body).expect("a JSON body");
            assert_eq!(sent["offsets"], serde_json::json!([1, 2]));
            assert_eq!(sent["retain"], false);
            let rows: Vec<u8> = [1.0f32, 2.0, 3.0, 4.0]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nx-rows: 2\r\nX-Dim: 2\r\nContent-Length: {}\r\n\r\n",
                rows.len()
            );
            stream.write_all(head.as_bytes()).expect("write the head");
            stream.write_all(&rows).expect("write the rows");
        });
        let encoder = LocalEncoder::new(&format!("http://{address}")).expect("a valid URL");
        let rows = encoder
            .readouts(&ReadoutRequest {
                episode: None,
                tokens: &[5, 6, 7],
                offsets: &[1, 2],
            })
            .expect("rows");
        assert_eq!(rows, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
        server.join().expect("the server thread");
    }

    #[test]
    fn a_repeated_input_is_answered_without_the_encoder() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let address = listener.local_addr().expect("a bound address");
        // Serves exactly one encode; a second connection would hang the test
        // on its read timeout and fail it.
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one connection");
            let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
            let mut length = 0;
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).expect("a header line");
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().expect("a length");
                }
                if line.trim_end().is_empty() {
                    break;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("the body");
            let rows: Vec<u8> = [1.0f32, 2.0].iter().flat_map(|v| v.to_le_bytes()).collect();
            let head = format!(
                "HTTP/1.1 200 OK\r\nx-rows: 1\r\nx-dim: 2\r\nContent-Length: {}\r\n\r\n",
                rows.len()
            );
            stream.write_all(head.as_bytes()).expect("write the head");
            stream.write_all(&rows).expect("write the rows");
        });
        let encoder = LocalEncoder::new(&format!("http://{address}"))
            .expect("a valid URL")
            .with_timeout(Duration::from_secs(2));
        let request = ReadoutRequest {
            episode: Some("e"),
            tokens: &[5, 6, 7],
            offsets: &[2],
        };
        let first = encoder.readouts(&request).expect("rows");
        server.join().expect("the server thread");
        assert_eq!(encoder.readouts(&request).expect("cached rows"), first);
    }

    #[test]
    fn refuses_urls_it_cannot_dial() {
        for url in [
            "https://127.0.0.1:1",
            "http://",
            "http://127.0.0.1:1/encode",
            "127.0.0.1:1",
        ] {
            assert!(LocalEncoder::new(url).is_err(), "{url}");
        }
    }
}
