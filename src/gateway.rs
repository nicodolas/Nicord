//! Minimal Discord gateway client — only to obtain a `session_id`
//! so slash commands can be invoked (`POST /interactions` requires
//! one). Nothing else uses the gateway: no event handling, no message
//! receiving, no intents beyond the minimum.
//!
//! Lazy: it connects only when a slash command actually fires, and
//! never when the user only sends plain messages — REST-only operation
//! is unchanged. A failed gateway never breaks message sending: slash
//! sends fail with a clear log message while prefix commands keep
//! working.
//!
//! The gateway runs as a background tokio task owned by the engine.
//! The UI "Test" button runs in its own short-lived runtime (a
//! background task would die with it), so it uses `connect_once`
//! instead: one connection, one READY, then close.

use crate::discord::DiscordError;
use futures_util::{SinkExt, StreamExt};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

pub const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// How long the send path waits for a gateway session before giving up.
pub const SESSION_WAIT_SECS: u64 = 20;

/// Shared gateway state, owned by the engine.
pub struct Gateway {
    session_id: Arc<RwLock<Option<String>>>,
    /// A connection task is alive right now. Set by a Drop guard held
    /// by the task itself, so a task killed with its runtime clears it
    /// and the next slash send can spawn a fresh one. Claimed with
    /// compare-exchange so two concurrent slash sends never spawn
    /// two tasks.
    task_alive: Arc<AtomicBool>,
    /// Stop requested (engine shutdown).
    stop: Arc<AtomicBool>,
}

/// Drop guard: clears `task_alive` when the connection task ends for
/// any reason, including its runtime being dropped.
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Default for Gateway {
    fn default() -> Self {
        Self::new()
    }
}

impl Gateway {
    pub fn new() -> Self {
        Self {
            session_id: Arc::new(RwLock::new(None)),
            task_alive: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Current session id, if the gateway is connected.
    pub async fn session_id(&self) -> Option<String> {
        self.session_id.read().await.clone()
    }

    /// Spawn the background connection task if none is alive. Must be
    /// called from inside a tokio runtime (the engine loop / test
    /// path). Clearing the stop flag here lets the engine restart the
    /// gateway after an Emergency Stop.
    pub fn ensure_started(&self, token: String) {
        self.stop.store(false, Ordering::SeqCst);
        // Claim the spawn: exactly one task, even if two slash sends
        // race. The guard held by the task clears the flag when it
        // ends, so a later send can spawn a fresh one.
        if self
            .task_alive
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let session = self.session_id.clone();
        let alive = self.task_alive.clone();
        let stop = self.stop.clone();
        tokio::spawn(async move {
            let _guard = AliveGuard(alive);
            run_connection_loop(token, session, stop).await;
        });
    }

    /// Ask the background task to stop (engine shutdown / Emergency
    /// Stop). The session id is cleared so a later slash send
    /// reconnects from scratch.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// One-shot connect: identify and wait for READY (up to `timeout`),
    /// then close and return the session id. Used by the UI "Test"
    /// button, which runs in its own short-lived runtime.
    pub async fn connect_once(token: &str, timeout: Duration) -> Result<String, DiscordError> {
        let deadline = Instant::now() + timeout;
        let (mut ws, _) = tokio::time::timeout(timeout, connect_async(GATEWAY_URL))
            .await
            .map_err(|_| DiscordError::Other("kết nối gateway hết thời gian".into()))?
            .map_err(|e| DiscordError::Other(format!("kết nối gateway thất bại: {e}")))?;
        let result = async {
            // Wait for HELLO, then Identify.
            let hello = read_until(&mut ws, deadline, |t| hello_interval(t).is_some()).await?;
            let _ = hello_interval(&hello);
            let payload = identify_payload(token);
            ws.send(Message::Text(payload.to_string().into()))
                .await
                .map_err(|e| DiscordError::Other(format!("gửi qua gateway thất bại: {e}")))?;
            // Wait for READY, answering heartbeats on the way.
            let mut interval: Option<u64> = hello_interval(&hello);
            let mut last_beat = Instant::now();
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(DiscordError::Other(
                        "chờ READY của gateway hết thời gian".into(),
                    ));
                }
                let beat_in =
                    interval.map(|i| Duration::from_millis(i).saturating_sub(last_beat.elapsed()));
                let text = match beat_in {
                    Some(d) if !d.is_zero() => {
                        match tokio::time::timeout(d, ws.next()).await {
                            Ok(Some(Ok(Message::Text(t)))) => t.to_string(),
                            Ok(Some(Ok(_))) => continue,
                            Ok(Some(Err(e))) => {
                                return Err(DiscordError::Other(format!(
                                    "đọc gateway thất bại: {e}"
                                )));
                            }
                            Ok(None) => {
                                return Err(DiscordError::Other("gateway đã đóng".into()));
                            }
                            Err(_) => {
                                // Heartbeat due.
                                let hb = heartbeat_payload(None);
                                ws.send(Message::Text(hb.to_string().into()))
                                    .await
                                    .map_err(|e| {
                                        DiscordError::Other(format!(
                                            "gửi qua gateway thất bại: {e}"
                                        ))
                                    })?;
                                last_beat = Instant::now();
                                continue;
                            }
                        }
                    }
                    _ => match tokio::time::timeout(remaining, ws.next()).await {
                        Ok(Some(Ok(Message::Text(t)))) => t.to_string(),
                        Ok(Some(Ok(_))) => continue,
                        Ok(Some(Err(e))) => {
                            return Err(DiscordError::Other(format!("đọc gateway thất bại: {e}")));
                        }
                        Ok(None) => {
                            return Err(DiscordError::Other("gateway đã đóng".into()));
                        }
                        Err(_) => {
                            return Err(DiscordError::Other(
                                "chờ READY của gateway hết thời gian".into(),
                            ));
                        }
                    },
                };
                if let Some(op) = gateway_op(&text) {
                    if op == 10 {
                        interval = hello_interval(&text);
                    } else if op == 1 {
                        let hb = heartbeat_payload(dispatch_seq(&text));
                        ws.send(Message::Text(hb.to_string().into()))
                            .await
                            .map_err(|e| {
                                DiscordError::Other(format!("gửi qua gateway thất bại: {e}"))
                            })?;
                        last_beat = Instant::now();
                    }
                }
                if let Some(sid) = ready_session_id(&text) {
                    return Ok(sid);
                }
            }
        }
        .await;
        let _ = ws.close(None).await;
        result
    }
}

/// Reconnect loop: one connection attempt per `connect_and_run`,
/// with exponential backoff (2s … 60s) until `stop` is set.
async fn run_connection_loop(
    token: String,
    session: Arc<RwLock<Option<String>>>,
    stop: Arc<AtomicBool>,
) {
    let mut backoff = Duration::from_secs(2);
    while !stop.load(Ordering::SeqCst) {
        match connect_and_run(&token, &session, &stop).await {
            Ok(()) => {
                // Clean end (stop requested or orderly close): reset
                // the backoff; the loop condition exits on stop.
                backoff = Duration::from_secs(2);
                if stop.load(Ordering::SeqCst) {
                    break;
                }
            }
            Err(()) => {
                *session.write().await = None;
                if sleep_or_stop(backoff, &stop).await {
                    break;
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
    *session.write().await = None;
}

/// One connection: HELLO → Identify → heartbeat loop until the socket
/// closes, errors, or `stop` is set. Ok(()) = orderly end.
async fn connect_and_run(
    token: &str,
    session: &Arc<RwLock<Option<String>>>,
    stop: &Arc<AtomicBool>,
) -> Result<(), ()> {
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(15), connect_async(GATEWAY_URL))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
    // HELLO carries the heartbeat interval; without it, don't bother.
    let deadline = Instant::now() + Duration::from_secs(15);
    let hello = read_until(&mut ws, deadline, |t| hello_interval(t).is_some())
        .await
        .map_err(|_| ())?;
    let interval_ms = hello_interval(&hello).ok_or(())?;
    let interval = Duration::from_millis(interval_ms);
    ws.send(Message::Text(identify_payload(token).to_string().into()))
        .await
        .map_err(|_| ())?;
    let mut last_seq: Option<u64> = None;
    let mut last_beat = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = ws.close(None).await;
            return Ok(());
        }
        let until_beat = interval.saturating_sub(last_beat.elapsed());
        let text = match tokio::time::timeout(until_beat, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => t.to_string(),
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => return Err(()),
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) => return Err(()),
            Err(_) => {
                // Heartbeat due.
                let hb = heartbeat_payload(last_seq);
                ws.send(Message::Text(hb.to_string().into()))
                    .await
                    .map_err(|_| ())?;
                last_beat = Instant::now();
                continue;
            }
        };
        let Some(op) = gateway_op(&text) else {
            continue;
        };
        match op {
            0 => {
                if let Some(s) = dispatch_seq(&text) {
                    last_seq = Some(s);
                }
                if let Some(sid) = ready_session_id(&text) {
                    *session.write().await = Some(sid);
                }
            }
            1 => {
                // Heartbeat requested: answer immediately.
                let hb = heartbeat_payload(last_seq);
                ws.send(Message::Text(hb.to_string().into()))
                    .await
                    .map_err(|_| ())?;
                last_beat = Instant::now();
            }
            7 => return Err(()), // Reconnect requested.
            9 => return Err(()), // Invalid session — reconnect (backoff applies).
            10 => { /* HELLO again: ignore, interval already known. */ }
            11 => { /* Heartbeat ACK. */ }
            _ => {}
        }
    }
}

/// Read text messages until `pred` matches one (or the deadline
/// passes). Non-text frames are skipped.
async fn read_until<F>(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    deadline: Instant,
    pred: F,
) -> Result<String, DiscordError>
where
    F: Fn(&str) -> bool,
{
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(DiscordError::Other("chờ gateway hết thời gian".into()));
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let s = t.to_string();
                if pred(&s) {
                    return Ok(s);
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => {
                return Err(DiscordError::Other(format!("đọc gateway thất bại: {e}")));
            }
            Ok(None) => {
                return Err(DiscordError::Other("gateway đã đóng".into()));
            }
            Err(_) => {
                return Err(DiscordError::Other("chờ gateway hết thời gian".into()));
            }
        }
    }
}

/// Sleep that aborts early when `stop` is set. True = interrupted.
async fn sleep_or_stop(dur: Duration, stop: &Arc<AtomicBool>) -> bool {
    let deadline = Instant::now() + dur;
    loop {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        tokio::time::sleep(left.min(Duration::from_millis(200))).await;
    }
}

/// Identify payload: intents 0 — the gateway is only used for a
/// session id, so no events are requested.
pub fn identify_payload(token: &str) -> serde_json::Value {
    serde_json::json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": 0,
            "properties": {
                "os": "windows",
                "browser": "Nicord",
                "device": "Nicord"
            }
        }
    })
}

/// Heartbeat payload (op 1) with the last dispatch sequence number
/// (null when none seen yet).
pub fn heartbeat_payload(seq: Option<u64>) -> serde_json::Value {
    serde_json::json!({ "op": 1, "d": seq })
}

/// Gateway opcode of a raw message, if it parses.
pub fn gateway_op(text: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .get("op")?
        .as_i64()
}

/// Heartbeat interval (ms) from a HELLO (op 10) message.
pub fn hello_interval(text: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("op")?.as_i64() != Some(10) {
        return None;
    }
    v.get("d")?.get("heartbeat_interval")?.as_u64()
}

/// Session id from a READY dispatch (op 0, t = "READY").
pub fn ready_session_id(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("op")?.as_i64() != Some(0) {
        return None;
    }
    if v.get("t")?.as_str() != Some("READY") {
        return None;
    }
    v.get("d")?
        .get("session_id")?
        .as_str()
        .map(|s| s.to_string())
}

/// Dispatch sequence number (`s` field) of a gateway message.
pub fn dispatch_seq(text: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .get("s")?
        .as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_payload_has_intents_zero() {
        let p = identify_payload("tok123");
        assert_eq!(p["op"], 2);
        assert_eq!(p["d"]["token"], "tok123");
        assert_eq!(p["d"]["intents"], 0);
        assert_eq!(p["d"]["properties"]["browser"], "Nicord");
    }

    #[test]
    fn heartbeat_payload_carries_seq() {
        assert_eq!(heartbeat_payload(Some(42))["d"], 42);
        assert!(heartbeat_payload(None)["d"].is_null());
        assert_eq!(heartbeat_payload(None)["op"], 1);
    }

    #[test]
    fn hello_interval_parses_op_10_only() {
        let hello = r#"{"op":10,"d":{"heartbeat_interval":41250}}"#;
        assert_eq!(hello_interval(hello), Some(41250));
        // A dispatch is not a HELLO.
        let dispatch = r#"{"op":0,"d":{"heartbeat_interval":41250}}"#;
        assert_eq!(hello_interval(dispatch), None);
        assert_eq!(hello_interval("not json"), None);
    }

    #[test]
    fn ready_session_id_parses_ready_only() {
        let ready = r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc123","user":{}}}"#;
        assert_eq!(ready_session_id(ready).as_deref(), Some("abc123"));
        // Same shape but a different event is not READY.
        let other = r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"session_id":"zzz"}}"#;
        assert_eq!(ready_session_id(other), None);
        // Op 11 (heartbeat ACK) is not a dispatch.
        assert_eq!(ready_session_id(r#"{"op":11}"#), None);
    }

    #[test]
    fn gateway_op_and_seq_parse() {
        assert_eq!(gateway_op(r#"{"op":1,"d":null}"#), Some(1));
        assert_eq!(gateway_op(r#"{"op":11}"#), Some(11));
        assert_eq!(gateway_op("garbage"), None);
        assert_eq!(
            dispatch_seq(r#"{"op":0,"s":99,"t":"READY","d":{}}"#),
            Some(99)
        );
        assert_eq!(dispatch_seq(r#"{"op":11}"#), None);
    }
}
