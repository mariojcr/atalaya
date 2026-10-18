// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Syslog receiver: network devices (a UniFi gateway, a NAS, a switch) send
//! their logs here over UDP or TCP and they land in the logs table.
//!
//! - RFC 3164 (`<PRI>Mmm dd hh:mm:ss host tag: msg`) and RFC 5424
//!   (`<PRI>1 timestamp host app procid msgid [sd] msg`).
//! - CEF messages (`CEF:0|Vendor|Product|Version|SignatureId|Name|Severity|k=v …`,
//!   the SIEM format of UniFi and many others): header and extension fields
//!   become searchable attributes and the CEF severity sets the level.
//! - Firewall lines with `KEY=value` pairs (`SRC=… DST=… PROTO=… SPT=… DPT=…`,
//!   the netfilter log format) get those pairs as lower-case attributes.
//!
//! The time is the reception time: RFC 3164 timestamps have no year or zone.
//!
//! Anyone who reaches the port can send, so TCP is bounded: a line longer than
//! `MAX_LINE`, or a connection silent for `IDLE`, is closed, and at most
//! `MAX_CONNECTIONS` are open at once, `MAX_PER_PEER` of them from one address
//! (one /64 in IPv6), so a single sender cannot take every slot.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Semaphore;

use crate::sink::Sink;

const MAX_BODY: usize = 64 << 10;
/// Longest TCP line: a UDP datagram's size, so both carry the same messages
const MAX_LINE: usize = 64 << 10;
/// A TCP sender silent for this long is disconnected (it reconnects to send again)
const IDLE: Duration = Duration::from_secs(15 * 60);
/// TCP connections open at once; more are refused until one closes
const MAX_CONNECTIONS: usize = 256;
/// TCP connections open at once from one address
const MAX_PER_PEER: usize = 16;

/// Open TCP connections per sender: an IPv4 address, or the /64 of an IPv6 one
/// (whoever holds one has all of its addresses)
#[derive(Clone, Default)]
struct PerPeer(Arc<std::sync::Mutex<HashMap<IpAddr, usize>>>);

/// One connection counted for its sender, until it is dropped
struct PeerSlot(PerPeer, IpAddr);

impl PerPeer {
    fn key(ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => IpAddr::V6((u128::from(v6) & !((1u128 << 64) - 1)).into()),
            },
            v4 => v4,
        }
    }

    fn take(&self, ip: IpAddr) -> Option<PeerSlot> {
        let key = Self::key(ip);
        let mut open = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let n = open.entry(key).or_default();
        if *n >= MAX_PER_PEER {
            return None;
        }
        *n += 1;
        Some(PeerSlot(self.clone(), key))
    }
}

impl Drop for PeerSlot {
    fn drop(&mut self) {
        let mut open = self.0.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = open.get_mut(&self.1) {
            *n -= 1;
            if *n == 0 {
                open.remove(&self.1);
            }
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub(crate) struct SyslogConfig {
    /// Address to listen on, UDP and TCP (default 0.0.0.0:5514)
    #[serde(default = "default_listen")]
    pub(crate) listen: String,
    /// Service name of the received lines (default syslog)
    #[serde(default = "default_service")]
    pub(crate) service: String,
}

fn default_listen() -> String {
    "0.0.0.0:5514".into()
}
fn default_service() -> String {
    "syslog".into()
}

/// One parsed message, before it becomes a row
#[derive(Debug, Default, PartialEq)]
struct Message {
    host: String,
    app: String,
    level: &'static str,
    body: String,
    attributes: Map<String, Value>,
}

/// Starts the UDP and TCP listeners; counts received lines in `lines`
pub(crate) async fn run(cfg: SyslogConfig, sink: Arc<Sink>, lines: Arc<AtomicU64>) -> anyhow::Result<()> {
    let udp = UdpSocket::bind(&cfg.listen).await?;
    let tcp = TcpListener::bind(&cfg.listen).await?;
    tracing::info!("syslog: listening on {} (udp and tcp), service {}", cfg.listen, cfg.service);
    let cfg = Arc::new(cfg);
    {
        let (cfg, sink, lines) = (cfg.clone(), sink.clone(), lines.clone());
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                match udp.recv_from(&mut buf).await {
                    Ok((n, peer)) => {
                        for line in String::from_utf8_lossy(&buf[..n]).lines() {
                            ingest(&cfg, &sink, &lines, line, peer);
                        }
                    }
                    Err(e) => tracing::warn!("syslog: udp: {e}"),
                }
            }
        });
    }
    tokio::spawn(async move {
        let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let peers = PerPeer::default();
        let mut refused: u64 = 0;
        loop {
            let (stream, peer) = match tcp.accept().await {
                Ok(c) => c,
                Err(e) => {
                    // Out of descriptors, for one: wait instead of spinning
                    tracing::warn!("syslog: tcp: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Some(peer_slot) = peers.take(peer.ip()) else {
                refused += 1;
                if refused.is_power_of_two() {
                    tracing::warn!("syslog: {MAX_PER_PEER} tcp connections open from one address: refused {peer} ({refused} so far)");
                }
                continue;
            };
            let Ok(slot) = slots.clone().try_acquire_owned() else {
                refused += 1;
                // At powers of two: a flood of connections does not flood the log
                if refused.is_power_of_two() {
                    tracing::warn!("syslog: {MAX_CONNECTIONS} tcp connections open: refused {peer} ({refused} so far)");
                }
                continue;
            };
            let (cfg, sink, lines) = (cfg.clone(), sink.clone(), lines.clone());
            tokio::spawn(async move {
                let _slots = (slot, peer_slot);
                serve_tcp(stream, peer, &cfg, &sink, &lines).await;
            });
        }
    });
    Ok(())
}

/// One TCP sender, one message per line (the common framing; octet counting is
/// not supported), until it closes, sends a line over `MAX_LINE` or stays
/// silent for `IDLE`
async fn serve_tcp(stream: TcpStream, peer: SocketAddr, cfg: &SyslogConfig, sink: &Sink, lines: &AtomicU64) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        match tokio::time::timeout(IDLE, read_line(&mut reader, &mut line, MAX_LINE)).await {
            Ok(Ok(Line::Done)) => ingest(cfg, sink, lines, &String::from_utf8_lossy(&line), peer),
            Ok(Ok(Line::TooLong)) => {
                tracing::warn!("syslog: tcp {peer}: a line over {} KiB, disconnected", MAX_LINE >> 10);
                return;
            }
            Ok(Err(e)) => {
                tracing::warn!("syslog: tcp {peer}: {e}");
                return;
            }
            Ok(Ok(Line::Closed)) | Err(_) => return,
        }
    }
}

#[derive(Debug, PartialEq)]
enum Line {
    Done,
    Closed,
    TooLong,
}

/// Reads up to a newline (left out) into `buf`, giving up past `max` bytes;
/// the last line before the end needs no newline
async fn read_line<R: AsyncBufRead + Unpin>(r: &mut R, buf: &mut Vec<u8>, max: usize) -> std::io::Result<Line> {
    loop {
        let available = r.fill_buf().await?;
        if available.is_empty() {
            return Ok(if buf.is_empty() { Line::Closed } else { Line::Done });
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let chunk = &available[..newline.unwrap_or(available.len())];
        if buf.len() + chunk.len() > max {
            return Ok(Line::TooLong);
        }
        buf.extend_from_slice(chunk);
        let used = newline.map_or(chunk.len(), |i| i + 1);
        r.consume(used);
        if newline.is_some() {
            return Ok(Line::Done);
        }
    }
}

fn ingest(cfg: &SyslogConfig, sink: &Sink, lines: &AtomicU64, line: &str, peer: SocketAddr) {
    let line = line.trim_end_matches(['\r', '\n', '\0']);
    if line.trim().is_empty() {
        return;
    }
    let m = parse(line);
    let host = if m.host.is_empty() || m.host == "-" {
        peer.ip().to_string()
    } else {
        m.host.clone()
    };
    let mut body = m.body;
    if body.len() > MAX_BODY {
        let mut cut = MAX_BODY;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
    }
    sink.push(
        "logs",
        json!({
            "Time": Utc::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            "Service": cfg.service,
            "Host": host,
            "Stream": "syslog",
            "Level": m.level,
            "Body": body,
            "Resource": { "source.ip": peer.ip().to_string(), "syslog.app": m.app },
            "Attributes": m.attributes,
        }),
    );
    lines.fetch_add(1, Ordering::Relaxed);
}

/// Syslog severity (0 emergency … 7 debug) as a log level
fn syslog_level(severity: u8) -> &'static str {
    match severity {
        0..=2 => "FATAL",
        3 => "ERROR",
        4 => "WARN",
        5 | 6 => "INFO",
        _ => "DEBUG",
    }
}

/// CEF severity (0–10, or Low/Medium/High/Very-High) as a log level
fn cef_level(severity: &str) -> &'static str {
    match severity.trim().to_ascii_lowercase().as_str() {
        "low" => "INFO",
        "medium" => "WARN",
        "high" => "ERROR",
        "very-high" | "veryhigh" => "FATAL",
        s => match s.parse::<u8>() {
            Ok(0..=3) => "INFO",
            Ok(4..=6) => "WARN",
            Ok(7 | 8) => "ERROR",
            Ok(_) => "FATAL",
            Err(_) => "",
        },
    }
}

fn parse(line: &str) -> Message {
    let mut m = Message::default();
    let mut rest = line;
    // <PRI>
    let mut severity = None;
    if let Some(r) = rest.strip_prefix('<')
        && let Some((pri, after)) = r.split_once('>')
        && let Ok(pri) = pri.parse::<u16>()
    {
        severity = Some((pri % 8) as u8);
        m.attributes.insert("syslog.facility".into(), json!((pri / 8).to_string()));
        m.attributes.insert("syslog.severity".into(), json!((pri % 8).to_string()));
        rest = after;
    }
    if let Some(r) = rest.strip_prefix("1 ") {
        // RFC 5424: TIMESTAMP HOSTNAME APP-NAME PROCID MSGID STRUCTURED-DATA MSG
        let mut parts = r.splitn(6, ' ');
        let _ts = parts.next();
        m.host = parts.next().unwrap_or_default().to_owned();
        m.app = parts.next().unwrap_or_default().trim_matches('-').to_owned();
        let _procid = parts.next();
        let _msgid = parts.next();
        let tail = parts.next().unwrap_or_default();
        rest = skip_structured_data(tail);
    } else if rest.len() > 16
        && rest.as_bytes()[3] == b' '
        && rest[..3].chars().all(|c| c.is_ascii_alphabetic())
        // The timestamp is ASCII; slicing at 15 inside a multi-byte character would panic
        && rest.is_char_boundary(15)
    {
        // RFC 3164: "Mmm dd hh:mm:ss HOST TAG: MSG" (day padded with a space)
        let after_ts = &rest[15..];
        let after_ts = after_ts.trim_start();
        let (host, msg) = after_ts.split_once(' ').unwrap_or((after_ts, ""));
        m.host = host.to_owned();
        rest = msg;
        // TAG: app[pid]: or app:
        if let Some((tag, msg)) = rest.split_once(": ")
            && !tag.contains(' ')
            && !tag.is_empty()
        {
            m.app = tag.split('[').next().unwrap_or(tag).to_owned();
            rest = msg;
        }
    }
    m.level = severity.map(syslog_level).unwrap_or("");
    let rest = rest.trim();
    if let Some(i) = rest.find("CEF:") {
        parse_cef(&rest[i..], &mut m);
    } else {
        m.body = rest.to_owned();
        parse_kv(rest, &mut m.attributes);
    }
    m
}

/// Skips RFC 5424 structured data: "-" or one or more "[…]" blocks
fn skip_structured_data(s: &str) -> &str {
    if let Some(r) = s.strip_prefix("- ") {
        return r;
    }
    if s == "-" {
        return "";
    }
    let mut rest = s;
    while let Some(r) = rest.strip_prefix('[') {
        let mut escaped = false;
        let mut end = None;
        for (i, c) in r.char_indices() {
            match c {
                '\\' if !escaped => escaped = true,
                ']' if !escaped => {
                    end = Some(i);
                    break;
                }
                _ => escaped = false,
            }
        }
        match end {
            Some(i) => rest = &r[i + 1..],
            None => return "",
        }
    }
    rest.trim_start()
}

/// `CEF:Version|Vendor|Product|Version|SignatureId|Name|Severity|Extension`
fn parse_cef(s: &str, m: &mut Message) {
    let s = &s[4..];
    let mut fields: Vec<String> = Vec::with_capacity(8);
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if fields.len() == 7 {
            cur.push(c);
            continue;
        }
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '|' => fields.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    fields.push(cur);
    let names = [
        "cef.version",
        "cef.vendor",
        "cef.product",
        "cef.device_version",
        "cef.signature_id",
        "cef.name",
        "cef.severity",
    ];
    for (k, v) in names.iter().zip(&fields) {
        m.attributes.insert((*k).into(), json!(v.trim()));
    }
    if let Some(sev) = fields.get(6) {
        let l = cef_level(sev);
        if !l.is_empty() {
            m.level = l;
        }
    }
    let ext = fields.get(7).map(String::as_str).unwrap_or("");
    let ext = parse_cef_extension(ext);
    let name = fields.get(5).cloned().unwrap_or_default();
    m.body = match ext.get("msg").and_then(Value::as_str) {
        Some(msg) if !msg.is_empty() && msg != name => format!("{name}: {msg}"),
        _ => name,
    };
    m.attributes.extend(ext);
}

/// CEF extension: `key=value` pairs where a value runs until the next ` key=`;
/// `\=`, `\\`, `\n` and `\r` are escapes
fn parse_cef_extension(s: &str) -> Map<String, Value> {
    let mut out = Map::new();
    let bytes = s.as_bytes();
    // Positions of unescaped '=' preceded by a key
    let mut keys: Vec<(usize, usize)> = Vec::new(); // (key start, '=' index)
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' && (i == 0 || bytes[i - 1] != b'\\') {
            let mut k = i;
            while k > 0 && (bytes[k - 1].is_ascii_alphanumeric() || bytes[k - 1] == b'_' || bytes[k - 1] == b'.') {
                k -= 1;
            }
            if k < i && (k == 0 || bytes[k - 1] == b' ') {
                keys.push((k, i));
            }
        }
        i += 1;
    }
    for (n, &(k, eq)) in keys.iter().enumerate() {
        let end = keys.get(n + 1).map(|&(next, _)| next).unwrap_or(bytes.len());
        let key = &s[k..eq];
        let raw = s[eq + 1..end].trim_end();
        let value = raw.replace("\\=", "=").replace("\\n", "\n").replace("\\r", "\r").replace("\\\\", "\\");
        out.insert(key.to_owned(), json!(value));
    }
    out
}

/// `KEY=value` tokens (netfilter: IN= OUT= SRC= DST= PROTO= SPT= DPT=…) and a
/// leading `[rule-name]`
fn parse_kv(s: &str, attrs: &mut Map<String, Value>) {
    let mut found = 0;
    for tok in s.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=')
            && !k.is_empty()
            && k.len() <= 16
            && k.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            attrs.insert(k.to_ascii_lowercase(), json!(v));
            found += 1;
        }
    }
    if found >= 2
        && let Some(r) = s.strip_prefix('[')
        && let Some((rule, _)) = r.split_once(']')
    {
        attrs.insert("rule".into(), json!(rule));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3164_with_cef() {
        let m = parse(
            "<14>Oct  5 22:10:01 UDM-Pro-SE CEF:0|Ubiquiti|UniFi Network|10.6.106|544|Admin Accessed UniFi Network|1|UNIFIcategory=System UNIFIsubCategory=Admin src=192.168.0.10 msg=admin accessed UniFi Network using the web",
        );
        assert_eq!(m.host, "UDM-Pro-SE");
        assert_eq!(m.level, "INFO");
        assert_eq!(m.attributes["cef.vendor"], "Ubiquiti");
        assert_eq!(m.attributes["cef.signature_id"], "544");
        assert_eq!(m.attributes["src"], "192.168.0.10");
        assert_eq!(m.attributes["UNIFIcategory"], "System");
        assert_eq!(m.attributes["msg"], "admin accessed UniFi Network using the web");
        assert_eq!(m.body, "Admin Accessed UniFi Network: admin accessed UniFi Network using the web");
    }

    #[test]
    fn cef_escapes_and_severity() {
        let m = parse("<12>Oct 15 01:02:03 gw CEF:0|Vendor|Pro\\|duct|1|100|Threat|8|act=blocked cs1=a\\=b c msg=line\\nnext");
        assert_eq!(m.attributes["cef.product"], "Pro|duct");
        assert_eq!(m.level, "ERROR");
        assert_eq!(m.attributes["cs1"], "a=b c");
        assert_eq!(m.attributes["msg"], "line\nnext");
        assert_eq!(m.attributes["act"], "blocked");
    }

    #[test]
    fn rfc5424() {
        let m = parse("<165>1 2026-10-05T22:10:01.003Z nas app 1234 ID47 [exampleSDID@32473 iut=\"3\"] disk ready");
        assert_eq!(m.host, "nas");
        assert_eq!(m.app, "app");
        assert_eq!(m.level, "INFO");
        assert_eq!(m.body, "disk ready");
        let m = parse("<11>1 2026-10-05T22:10:01Z host - - - - plain message");
        assert_eq!((m.app.as_str(), m.body.as_str(), m.level), ("", "plain message", "ERROR"));
    }

    #[test]
    fn netfilter_line() {
        let m = parse(
            "<4>Oct  5 22:11:12 UDM-Pro-SE kernel: [WAN_LOCAL-D-2147483647] IN=eth8 OUT= MAC=aa:bb SRC=203.0.113.9 DST=198.51.100.2 LEN=40 PROTO=TCP SPT=51234 DPT=22",
        );
        assert_eq!(m.app, "kernel");
        assert_eq!(m.level, "WARN");
        assert_eq!(m.attributes["src"], "203.0.113.9");
        assert_eq!(m.attributes["dpt"], "22");
        assert_eq!(m.attributes["proto"], "TCP");
        assert_eq!(m.attributes["rule"], "WAN_LOCAL-D-2147483647");
    }

    #[test]
    fn a_multibyte_character_where_the_timestamp_ends_does_not_panic() {
        // 'é' takes bytes 14 and 15 of what looks like an RFC 3164 header: one such
        // datagram used to panic and stop the UDP receiver for good
        let m = parse("<13>Oct 14 12:00:0é host app: hello");
        assert_eq!(m.body, "Oct 14 12:00:0é host app: hello");
        assert_eq!(m.host, "");
    }

    #[tokio::test]
    async fn tcp_lines_are_bounded() {
        let input: &[u8] = b"one\r\ntwo\nlast without newline";
        let mut r = BufReader::new(input);
        let mut got = Vec::new();
        loop {
            let mut buf = Vec::new();
            match read_line(&mut r, &mut buf, 64).await.unwrap() {
                Line::Done => got.push(String::from_utf8(buf).unwrap()),
                other => {
                    assert_eq!(other, Line::Closed);
                    break;
                }
            }
        }
        assert_eq!(got, ["one\r", "two", "last without newline"]);
        // A line longer than the limit, whether or not its newline ever comes
        for input in [&b"0123456789abcdef\nok\n"[..], &[b'x'; 1000][..]] {
            let mut r = BufReader::with_capacity(4, input);
            let mut buf = Vec::new();
            assert_eq!(read_line(&mut r, &mut buf, 8).await.unwrap(), Line::TooLong);
            assert!(buf.len() <= 8);
        }
    }

    #[tokio::test]
    async fn the_tcp_listener_refuses_past_its_connection_limit_and_drops_long_lines() {
        let sink = Arc::new(
            Sink::new(
                "http://127.0.0.1:1/".into(),
                "u".into(),
                crate::sink::test_password(),
                "atalaya".into(),
                std::env::temp_dir().join(format!("atalaya-syslog-{}", std::process::id())),
                1 << 20,
            )
            .unwrap(),
        );
        // A free port for UDP and TCP alike
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let cfg = SyslogConfig {
            listen: format!("127.0.0.1:{port}"),
            service: "syslog".into(),
        };
        let lines = Arc::new(AtomicU64::new(0));
        run(cfg, sink, lines.clone()).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let addr = format!("127.0.0.1:{port}");
        // A line over the limit closes that connection: what follows is not read
        // (the server may close before every byte is written: the writes can fail)
        let mut s = TcpStream::connect(&addr).await.unwrap();
        let _ = s.write_all(&vec![b'x'; MAX_LINE + 1]).await;
        let _ = s.write_all(b"\n<14>after the long line\n").await;
        let mut rest = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut rest))
            .await
            .expect("closed by the server");
        assert_eq!(lines.load(Ordering::Relaxed), 0);
        // Its slot is freed right after the socket closes
        tokio::time::sleep(Duration::from_millis(100)).await;
        // From one address, a connection past MAX_PER_PEER is closed straight away
        // (the whole of 127.0.0.0/8 is loopback: each sender gets its own address)
        let from = |i: usize| async move {
            let s = tokio::net::TcpSocket::new_v4().unwrap();
            s.bind(format!("127.0.0.{}:0", 2 + i).parse().unwrap()).unwrap();
            s.connect(format!("127.0.0.1:{port}").parse().unwrap()).await.unwrap()
        };
        let mut one = Vec::new();
        for _ in 0..MAX_PER_PEER {
            one.push(from(0).await);
        }
        one.last_mut().unwrap().write_all(b"<14>last of one address\n").await.unwrap();
        for _ in 0..50 {
            if lines.load(Ordering::Relaxed) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(lines.load(Ordering::Relaxed), 1);
        let mut extra = from(0).await;
        let mut byte = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(5), extra.read(&mut byte))
            .await
            .expect("closed by the server");
        assert!(matches!(n, Ok(0) | Err(_)), "refused past the per-address limit");
        drop(one);
        tokio::time::sleep(Duration::from_millis(100)).await;
        lines.store(0, Ordering::Relaxed);
        // Every slot taken, from several addresses: one more connection is closed straight away
        let mut open = Vec::new();
        for i in 0..MAX_CONNECTIONS {
            open.push(from(i / MAX_PER_PEER).await);
        }
        // The listener takes the connections in order: once one line has gone
        // through the last of them, all the slots are taken
        open.last_mut().unwrap().write_all(b"<14>a line\n").await.unwrap();
        for _ in 0..50 {
            if lines.load(Ordering::Relaxed) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(lines.load(Ordering::Relaxed), 1);
        let mut extra = from(MAX_CONNECTIONS / MAX_PER_PEER).await;
        let n = tokio::time::timeout(Duration::from_secs(5), extra.read(&mut byte))
            .await
            .expect("closed by the server");
        assert!(matches!(n, Ok(0) | Err(_)), "refused");
        // Closing one frees its slot
        drop(open.pop());
        let mut again = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut s = TcpStream::connect(&addr).await.unwrap();
            s.write_all(b"<14>accepted again\n").await.unwrap();
            for _ in 0..10 {
                if lines.load(Ordering::Relaxed) == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if lines.load(Ordering::Relaxed) == 2 {
                again = Some(s);
                break;
            }
        }
        assert!(again.is_some(), "a freed slot takes a new connection");
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("atalaya-syslog-{}", std::process::id())));
    }

    #[test]
    fn plain_text_without_pri() {
        let m = parse("just a line");
        assert_eq!(m.body, "just a line");
        assert_eq!(m.level, "");
        assert!(m.attributes.is_empty());
    }
}
