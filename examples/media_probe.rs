//! Live media-datapath probe for a running rtpbridge.
//!
//! Connects to the control WebSocket, creates a WebRTC endpoint, acts as a
//! full-ICE str0m peer (ICE + DTLS-SRTP), then sends real SRTP (Opus PT 111)
//! and reports whether rtpbridge counts it as inbound media (the same signal
//! the `endpoint media timeout` watchdog keys on).
//!
//! Usage:
//!   cargo run --example media_probe -- --ws 127.0.0.1:9111 --label rtpbridge-1 --secs 9
//!
//! Control WS is reached via `kubectl port-forward` (localhost). Media flows to
//! whatever candidate the bridge advertises in its offer (its public media_ip).

use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use str0m::change::SdpOffer;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

struct Ctl {
    tx: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    resp_rx: mpsc::UnboundedReceiver<Value>,
    event_rx: mpsc::UnboundedReceiver<Value>,
    next_id: u64,
}

impl Ctl {
    async fn connect(addr: &str) -> anyhow::Result<Self> {
        let url = format!("ws://{addr}");
        let (stream, _) = connect_async(&url).await?;
        let (tx, mut rx) = stream.split();
        let (resp_tx, resp_rx) = mpsc::unbounded_channel::<Value>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = rx.next().await {
                if let Message::Text(t) = msg {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        if v.get("event").is_some() {
                            let _ = event_tx.send(v);
                        } else {
                            let _ = resp_tx.send(v);
                        }
                    }
                }
            }
        });
        Ok(Self {
            tx,
            resp_rx,
            event_rx,
            next_id: 1,
        })
    }

    async fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let req = json!({ "id": id, "method": method, "params": params });
        self.tx.send(Message::Text(req.to_string().into())).await?;
        let resp = tokio::time::timeout(Duration::from_secs(6), self.resp_rx.recv())
            .await
            .map_err(|_| anyhow::anyhow!("response timeout for {method}"))?
            .ok_or_else(|| anyhow::anyhow!("resp channel closed"))?;
        if let Some(err) = resp.get("error") {
            anyhow::bail!("{method} returned error: {err}");
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    fn drain_events(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(v) = self.event_rx.try_recv() {
            out.push(v);
        }
        out
    }
}

fn sdp_value(sdp: &str, prefix: &str) -> Option<String> {
    sdp.lines()
        .map(|l| l.trim())
        .find_map(|l| l.strip_prefix(prefix).map(|s| s.to_string()))
}

/// Parse "a=candidate:... <ip> <port> typ host ..." -> SocketAddr
fn parse_candidate_addr(sdp: &str) -> Option<SocketAddr> {
    for l in sdp.lines().map(|l| l.trim()) {
        if let Some(rest) = l.strip_prefix("a=candidate:") {
            let t: Vec<&str> = rest.split_whitespace().collect();
            // foundation comp transport priority IP PORT typ host ...
            if t.len() >= 8 && t[6] == "typ" {
                if let (Ok(ip), Ok(port)) = (t[4].parse(), t[5].parse::<u16>()) {
                    return Some(SocketAddr::new(ip, port));
                }
            }
        }
    }
    None
}

fn local_ip_towards(target: SocketAddr) -> std::net::IpAddr {
    let s = StdUdpSocket::bind("0.0.0.0:0").unwrap();
    // doesn't send anything; just selects the egress interface
    s.connect(target).ok();
    s.local_addr()
        .map(|a| a.ip())
        .unwrap_or_else(|_| "0.0.0.0".parse().unwrap())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ---- args ----
    let args: Vec<String> = std::env::args().collect();
    let mut ws = "127.0.0.1:9100".to_string();
    let mut label = "rtpbridge".to_string();
    let mut secs = 9u64;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--ws" => {
                ws = args[i + 1].clone();
                i += 2;
            }
            "--label" => {
                label = args[i + 1].clone();
                i += 2;
            }
            "--secs" => {
                secs = args[i + 1].parse().unwrap_or(9);
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }
    println!("== media_probe [{label}] via control ws://{ws} ==");

    let mut ctl = Ctl::connect(&ws).await?;
    let sess = ctl.request("session.create", json!({})).await?;
    let session_id = sess
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    println!("session_id = {session_id}");

    ctl.request("stats.subscribe", json!({"interval_ms": 1000}))
        .await?;

    let off = ctl
        .request(
            "endpoint.create_offer",
            json!({"type":"webrtc","direction":"sendrecv"}),
        )
        .await?;
    let ep_id = off["endpoint_id"].as_str().unwrap_or("?").to_string();
    let offer_sdp = off["sdp_offer"].as_str().unwrap_or("").to_string();
    let mid = sdp_value(&offer_sdp, "a=mid:").unwrap_or_else(|| "0".to_string());
    let cand = parse_candidate_addr(&offer_sdp);
    println!("endpoint_id = {ep_id}");
    println!("offer mid = {mid}");
    println!("rtpbridge media candidate = {cand:?}   <-- SRTP target");

    let target = cand.ok_or_else(|| anyhow::anyhow!("no candidate in offer"))?;

    // ---- str0m peer (full ICE) bound to the egress interface ----
    let lip = local_ip_towards(target);
    let socket = UdpSocket::bind((lip, 0)).await?;
    let peer_addr = socket.local_addr()?;
    println!(
        "local peer socket = {peer_addr} (egress toward {})",
        target.ip()
    );

    let mut rtc = RtcConfig::new().set_rtp_mode(true).build(Instant::now());
    rtc.add_local_candidate(Candidate::host(peer_addr, "udp")?);
    let answer = rtc
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&offer_sdp)?)?;
    ctl.request(
        "endpoint.webrtc.accept_answer",
        json!({"endpoint_id": ep_id, "sdp": answer.to_sdp_string()}),
    )
    .await?;

    // ---- Phase A: drive ICE (+DTLS) ----
    let mut buf = vec![0u8; 2048];
    let mut connected = false;
    let mut transmits = 0u32;
    let a_deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < a_deadline {
        match rtc.poll_output() {
            Ok(Output::Timeout(when)) => {
                let wait = when
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO)
                    .min(Duration::from_millis(10));
                match tokio::time::timeout(wait, socket.recv_from(&mut buf)).await {
                    Ok(Ok((n, src))) => {
                        if let Ok(r) = Receive::new(Protocol::Udp, src, peer_addr, &buf[..n]) {
                            let _ = rtc.handle_input(Input::Receive(Instant::now(), r));
                        }
                    }
                    _ => {
                        let _ = rtc.handle_input(Input::Timeout(Instant::now()));
                    }
                }
            }
            Ok(Output::Transmit(t)) => {
                transmits += 1;
                let _ = socket.send_to(&t.contents, t.destination).await;
            }
            Ok(Output::Event(Event::Connected)) => {
                connected = true;
                break;
            }
            Ok(Output::Event(_)) => {}
            Err(e) => {
                println!("str0m error during ICE: {e:?}");
                break;
            }
        }
    }
    println!(
        "ICE: {} (STUN transmits sent = {transmits})",
        if connected {
            "CONNECTED"
        } else {
            "NOT CONNECTED"
        }
    );
    if !connected {
        println!(
            "VERDICT [{label}]: media socket did NOT complete ICE — datapath unreachable/wedged"
        );
        let _ = ctl.request("session.destroy", json!({})).await;
        return Ok(());
    }

    // ---- Phase B: send SRTP (Opus PT 111) and watch rtpbridge's inbound stats ----
    println!("sending SRTP for {secs}s ...");
    let mid_t: str0m::media::Mid = mid.as_str().into();
    let b_deadline = Instant::now() + Duration::from_secs(secs);
    let mut seq: u64 = 0;
    let mut last_print = Instant::now();
    let mut last_stats: Option<Value> = None;
    while Instant::now() < b_deadline {
        // write one 20ms Opus frame
        {
            let mut api = rtc.direct_api();
            if let Some(tx) = api.stream_tx_by_mid(mid_t, None) {
                let _ = tx.write_rtp(
                    111.into(),
                    seq.into(),
                    (seq as u32).wrapping_mul(960),
                    Instant::now(),
                    seq == 0,
                    str0m::rtp::ExtensionValues::default(),
                    false,
                    vec![0x80u8; 160],
                );
            }
            seq += 1;
        }
        // flush + receive
        let pump_until = Instant::now() + Duration::from_millis(20);
        while Instant::now() < pump_until {
            match rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Ok(Output::Timeout(when)) => {
                    let wait = when
                        .checked_duration_since(Instant::now())
                        .unwrap_or(Duration::ZERO)
                        .min(Duration::from_millis(5));
                    match tokio::time::timeout(wait, socket.recv_from(&mut buf)).await {
                        Ok(Ok((n, src))) => {
                            if let Ok(r) = Receive::new(Protocol::Udp, src, peer_addr, &buf[..n]) {
                                let _ = rtc.handle_input(Input::Receive(Instant::now(), r));
                            }
                        }
                        _ => {
                            let _ = rtc.handle_input(Input::Timeout(Instant::now()));
                            break;
                        }
                    }
                }
                Ok(Output::Event(_)) => {}
                Err(_) => break,
            }
        }
        // collect rtpbridge stats events for our endpoint
        for ev in ctl.drain_events() {
            if ev.get("event").and_then(|v| v.as_str()) == Some("stats") {
                if let Some(eps) = ev["data"]["endpoints"].as_array() {
                    if let Some(me) = eps
                        .iter()
                        .find(|e| e["endpoint_id"].as_str() == Some(ep_id.as_str()))
                    {
                        last_stats = Some(me.clone());
                    }
                }
            }
        }
        if last_print.elapsed() >= Duration::from_millis(1500) {
            last_print = Instant::now();
            println!(
                "  sent_rtp={seq}  rtpbridge_stats_for_endpoint={}",
                last_stats
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "<none yet>".into())
            );
        }
    }

    // ---- Phase C: final state ----
    let info = ctl.request("session.info", json!({})).await?;
    let ep_state = info["endpoints"]
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|e| e["endpoint_id"].as_str() == Some(ep_id.as_str()))
        })
        .map(|e| e.to_string())
        .unwrap_or_else(|| "<endpoint gone>".into());
    println!("final session.info endpoint = {ep_state}");
    println!(
        "final stats sample = {}",
        last_stats
            .as_ref()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<none>".into())
    );
    println!(
        "VERDICT [{label}]: ICE connected; sent {seq} SRTP frames. Inspect stats above + rtpbridge logs for session {session_id} to confirm inbound media was decoded."
    );

    let _ = ctl.request("session.destroy", json!({})).await;
    Ok(())
}
