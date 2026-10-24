// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! Time synchronization from chronyd: what `chronyc tracking`, `sources`,
//! `sourcestats` and `serverstats` show, read over chronyd's command protocol
//! and turned into `host` service metrics.
//!
//! chronyd answers commands on UDP 127.0.0.1:323, which the collector reaches
//! from the host's network. tracking, sources and sourcestats are open to
//! localhost by default; serverstats only from chrony 4.7, when chrony.conf
//! lists it in `opencommands` (the directive replaces the default list, so it
//! must repeat the default commands). A command chronyd refuses is skipped. A
//! host without chronyd gets host_chrony_up = 0.
//!
//! The packet layouts are those of chrony's candm.h, protocol version 6
//! (chrony 4.x). A request is padded to the length of its reply: chronyd never
//! answers with more bytes than it received.
//!
//! Signs follow chronyc: the system time offset is positive when the clock is
//! ahead ("fast"), and the frequency is positive when the clock would run fast
//! without correction.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::prom::{Kind, Labels, Scrape};

const PROTO_VERSION: u8 = 6;
const PKT_TYPE_REQUEST: u8 = 1;
const PKT_TYPE_REPLY: u8 = 2;

const REQ_N_SOURCES: u16 = 14;
const REQ_SOURCE_DATA: u16 = 15;
const REQ_TRACKING: u16 = 33;
const REQ_SOURCESTATS: u16 = 34;
const REQ_SERVER_STATS: u16 = 54;

const RPY_N_SOURCES: u16 = 2;
const RPY_SOURCE_DATA: u16 = 3;
const RPY_TRACKING: u16 = 5;
const RPY_SOURCESTATS: u16 = 6;
const RPY_SERVER_STATS4: u16 = 25;

const STT_SUCCESS: u16 = 0;

/// Where the command data starts in a request and in a reply
const REQUEST_HEADER: usize = 20;
const REPLY_HEADER: usize = 28;

/// Lengths of the reply data (up to the EOR field of each RPY_ struct)
const N_SOURCES_LEN: usize = 4;
const SOURCE_DATA_LEN: usize = 48;
const TRACKING_LEN: usize = 76;
const SOURCESTATS_LEN: usize = 56;
const SERVER_STATS_LEN: usize = 168;

/// IPAddr families
const FAMILY_UNSPEC: u16 = 0;
const FAMILY_INET4: u16 = 1;
const FAMILY_INET6: u16 = 2;

/// Source mode of a reference clock (RPY_SD_MD_REF)
const MODE_REFCLOCK: u16 = 2;

/// Reference ID of the local clock (`local` directive, orphan mode)
const LOCAL_REFID: u32 = 0x7F7F_0101;

/// chronyd is on the same host
const TIMEOUT: Duration = Duration::from_millis(500);

/// A whole reading, whatever answers on the command port
const BUDGET: Duration = Duration::from_secs(3);

/// Upper bound on the sources read in one pass
const MAX_SOURCES: u32 = 256;

#[derive(Debug)]
enum Fail {
    Io(io::Error),
    /// chronyd answered with this status instead of the data
    Status(u16),
    Bad(&'static str),
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Io(e) => write!(f, "{e}"),
            Fail::Status(s) => write!(f, "chronyd answered status {s}"),
            Fail::Bad(why) => write!(f, "bad reply: {why}"),
        }
    }
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn int64(b: &[u8], at: usize) -> u64 {
    u64::from(be32(b, at)) << 32 | u64::from(be32(b, at + 4))
}

/// chrony's Float: 7-bit signed exponent and 25-bit signed coefficient,
/// coef · 2^(exp − 25) (util.c, UTI_FloatNetworkToHost)
fn float(b: &[u8], at: usize) -> f64 {
    let x = be32(b, at);
    let mut exp = (x >> 25) as i32;
    if exp >= 1 << 6 {
        exp -= 1 << 7;
    }
    let mut coef = (x & ((1 << 25) - 1)) as i32;
    if coef >= 1 << 24 {
        coef -= 1 << 25;
    }
    f64::from(coef) * 2f64.powi(exp - 25)
}

/// Seconds of a Timespec (sec high, sec low, nsec); high is 0x7fffffff for a
/// 32-bit timestamp
fn timespec(b: &[u8], at: usize) -> f64 {
    let (high, low, nsec) = (be32(b, at), be32(b, at + 4), be32(b, at + 8));
    let secs = if high == 0x7fff_ffff {
        u64::from(low)
    } else {
        u64::from(high) << 32 | u64::from(low)
    };
    secs as f64 + f64::from(nsec) / 1e9
}

/// A reference ID as chronyc prints it (UTI_RefidToString): its printable
/// characters ("GPS", "PPS")
fn refid_name(id: u32) -> String {
    id.to_be_bytes().iter().filter(|b| (0x20..0x7f).contains(*b)).map(|b| char::from(*b)).collect()
}

/// The address in an IPAddr (16 address bytes, then the family)
fn address(b: &[u8], at: usize) -> Option<String> {
    match be16(b, at + 16) {
        FAMILY_INET4 => Some(Ipv4Addr::from(be32(b, at)).to_string()),
        FAMILY_INET6 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&b[at..at + 16]);
            Some(Ipv6Addr::from(a).to_string())
        }
        _ => None,
    }
}

/// A request: the header, the command's data, and zeros up to the length of
/// the reply
fn request(command: u16, sequence: u32, data: &[u8], reply_len: usize) -> Vec<u8> {
    let mut p = Vec::with_capacity(REPLY_HEADER + reply_len);
    p.extend_from_slice(&[PROTO_VERSION, PKT_TYPE_REQUEST, 0, 0]);
    p.extend_from_slice(&command.to_be_bytes());
    // attempt: each request is sent once
    p.extend_from_slice(&0u16.to_be_bytes());
    p.extend_from_slice(&sequence.to_be_bytes());
    p.extend_from_slice(&[0; 8]);
    debug_assert_eq!(p.len(), REQUEST_HEADER);
    p.extend_from_slice(data);
    p.resize(p.len().max(REPLY_HEADER + reply_len), 0);
    p
}

/// Checks a reply's header and returns its data
fn parse_reply(b: &[u8], command: u16, reply: u16, reply_len: usize) -> Result<&[u8], Fail> {
    if b.len() < REPLY_HEADER {
        return Err(Fail::Bad("shorter than a header"));
    }
    if b[..4] != [PROTO_VERSION, PKT_TYPE_REPLY, 0, 0] {
        return Err(Fail::Bad("not a protocol 6 reply"));
    }
    if be16(b, 4) != command {
        return Err(Fail::Bad("reply to another command"));
    }
    let status = be16(b, 8);
    if status != STT_SUCCESS {
        return Err(Fail::Status(status));
    }
    if be16(b, 6) != reply {
        return Err(Fail::Bad("unexpected reply format"));
    }
    b.get(REPLY_HEADER..REPLY_HEADER + reply_len).ok_or(Fail::Bad("reply too short"))
}

struct Client {
    sock: UdpSocket,
    sequence: u32,
    /// End of the reading's budget
    deadline: Instant,
}

impl Client {
    fn connect(addr: SocketAddr) -> io::Result<Self> {
        let local: SocketAddr = if addr.is_ipv4() {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let sock = UdpSocket::bind(local)?;
        // Connected: datagrams from anyone else are not read, and a closed
        // port fails at once with ConnectionRefused
        sock.connect(addr)?;
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos();
        Ok(Self {
            sock,
            sequence: seed ^ std::process::id(),
            deadline: Instant::now() + BUDGET,
        })
    }

    /// Sends a command and returns the data of its reply
    fn call(&mut self, command: u16, data: &[u8], reply: u16, reply_len: usize) -> Result<Vec<u8>, Fail> {
        // Datagrams that are not the reply do not extend the wait, and nothing
        // is sent once the reading's budget is spent
        let until = (Instant::now() + TIMEOUT).min(self.deadline);
        let left = || until.saturating_duration_since(Instant::now());
        let timed_out = || Fail::Io(io::ErrorKind::TimedOut.into());
        if left().is_zero() {
            return Err(timed_out());
        }
        self.sequence = self.sequence.wrapping_add(1);
        let seq = self.sequence;
        self.sock.send(&request(command, seq, data, reply_len)).map_err(Fail::Io)?;
        let mut buf = [0u8; 1024];
        loop {
            let wait = left();
            if wait.is_zero() {
                return Err(timed_out());
            }
            self.sock.set_read_timeout(Some(wait)).map_err(Fail::Io)?;
            let n = self.sock.recv(&mut buf).map_err(Fail::Io)?;
            let b = &buf[..n];
            // A late reply to an earlier request that timed out
            if n >= REPLY_HEADER && be32(b, 16) != seq {
                continue;
            }
            return parse_reply(b, command, reply, reply_len).map(<[u8]>::to_vec);
        }
    }
}

/// tracking: the state of the system clock
fn tracking(o: &mut Scrape, d: &[u8], now: f64) {
    let correction = float(d, 40);
    let ref_time = timespec(d, 28);
    let mut g = |metric: &str, unit: &str, help: &str, value: f64| {
        o.put(metric, Kind::Gauge, unit, help, Labels::new(), value);
    };
    g(
        "host_chrony_synchronized",
        "1",
        "1 when chronyd considers the clock synchronized.",
        if be16(d, 26) == 3 { 0.0 } else { 1.0 },
    );
    g("host_chrony_stratum", "1", "Stratum of the system clock.", f64::from(be16(d, 24)));
    g(
        "host_chrony_local_reference",
        "1",
        "1 when the reference is the local clock (orphan mode): no real source is followed.",
        if be32(d, 0) == LOCAL_REFID { 1.0 } else { 0.0 },
    );
    if ref_time > 0.0 {
        g(
            "host_chrony_reference_age_seconds",
            "s",
            "Time since the last measurement from the reference was processed.",
            (now - ref_time).max(0.0),
        );
    }
    g(
        "host_chrony_system_time_offset_seconds",
        "s",
        "System clock minus NTP time: positive when the clock is ahead.",
        -correction,
    );
    g("host_chrony_rms_offset_seconds", "s", "Long-term average of the offset.", float(d, 48));
    g(
        "host_chrony_frequency_ppm",
        "ppm",
        "Rate error of the clock without correction: positive when it would run fast.",
        float(d, 52),
    );
    // chronyc's documented bound: |offset| + root dispersion + root delay / 2
    g(
        "host_chrony_max_error_seconds",
        "s",
        "Bound on the clock's error, assuming the stratum 1 source is right.",
        correction.abs() + float(d, 68) + float(d, 64) / 2.0,
    );
}

/// source_data: one source as `chronyc sources` shows it. Returns its labels
fn source(o: &mut Scrape, d: &[u8]) -> Option<Labels> {
    let mode = be16(d, 26);
    // chronyd sends a reference clock with its refid as an IPv4 address
    let name = if mode == MODE_REFCLOCK { refid_name(be32(d, 0)) } else { address(d, 0)? };
    let mode = match mode {
        0 => "server",
        1 => "peer",
        MODE_REFCLOCK => "refclock",
        _ => "unknown",
    };
    let l: Labels = [("source".to_owned(), name), ("mode".to_owned(), mode.to_owned())].into();
    let mut g = |metric: &str, unit: &str, help: &str, value: f64| {
        o.put(metric, Kind::Gauge, unit, help, l.clone(), value);
    };
    g(
        "host_chrony_source_state",
        "1",
        "Selection state as chronyc marks it: 0 selected (*), 1 not selectable (?), 2 falseticker (x), 3 jittery (~), 4 combined (+), 5 not combined (-).",
        f64::from(be16(d, 24)),
    );
    g("host_chrony_source_stratum", "1", "Stratum of the source.", f64::from(be16(d, 22)));
    g(
        "host_chrony_source_reachability_ratio",
        "1",
        "Share of the last 8 polls the source answered.",
        f64::from((be16(d, 30) & 0xff).count_ones()) / 8.0,
    );
    let since = be32(d, 32);
    // u32::MAX: no sample yet
    if since != u32::MAX {
        g(
            "host_chrony_source_last_sample_age_seconds",
            "s",
            "Time since the last good sample from the source.",
            f64::from(since),
        );
        g(
            "host_chrony_source_offset_seconds",
            "s",
            "Local clock minus the source at the last sample.",
            float(d, 40),
        );
        g("host_chrony_source_offset_error_seconds", "s", "Error bound of the last sample.", float(d, 44));
    }
    Some(l)
}

/// sourcestats: the jitter of the source with labels `l`
fn sourcestats(o: &mut Scrape, d: &[u8], l: &Labels) {
    // A reference clock has no address: its refid names it
    let name = if be16(d, 4 + 16) == FAMILY_UNSPEC {
        Some(refid_name(be32(d, 0)))
    } else {
        address(d, 4)
    };
    // The list may have changed between the two requests
    if name.as_deref() != l.get("source").map(String::as_str) {
        return;
    }
    o.put(
        "host_chrony_source_std_dev_seconds",
        Kind::Gauge,
        "s",
        "Estimated standard deviation of the source's samples.",
        l.clone(),
        float(d, 36),
    );
}

/// serverstats: what chronyd served as an NTP server
fn server_stats(o: &mut Scrape, d: &[u8]) {
    for (metric, at, help) in [
        ("host_chrony_server_ntp_requests_total", 0, "Valid NTP requests received."),
        ("host_chrony_server_ntp_dropped_total", 24, "NTP requests dropped by rate limiting."),
    ] {
        o.put(metric, Kind::Cumulative, "1", help, Labels::new(), int64(d, at) as f64);
    }
}

/// One reading of chronyd: tracking, every source and, when chrony.conf opens
/// them, the server statistics
pub fn collect(addr: SocketAddr) -> Scrape {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    let mut o = Scrape::default();
    let up = |o: &mut Scrape, v: f64| {
        o.put(
            "host_chrony_up",
            Kind::Gauge,
            "1",
            "1 when chronyd answered on its command port.",
            Labels::new(),
            v,
        );
    };
    let mut c = match Client::connect(addr) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("chronyd at {addr}: {e}");
            up(&mut o, 0.0);
            return o;
        }
    };
    match c.call(REQ_TRACKING, &[], RPY_TRACKING, TRACKING_LEN) {
        Ok(d) => {
            up(&mut o, 1.0);
            tracking(&mut o, &d, now);
        }
        // Refused (left out of opencommands), but chronyd is there
        Err(e @ Fail::Status(_)) => {
            tracing::debug!("chronyd at {addr}: tracking: {e}");
            up(&mut o, 1.0);
        }
        // A host without chronyd is not news
        Err(e) => {
            tracing::debug!("chronyd at {addr}: {e}");
            up(&mut o, 0.0);
            return o;
        }
    }
    match c.call(REQ_N_SOURCES, &[], RPY_N_SOURCES, N_SOURCES_LEN) {
        Ok(d) => {
            for i in 0..be32(&d, 0).min(MAX_SOURCES) {
                let index = i.to_be_bytes();
                let l = match c.call(REQ_SOURCE_DATA, &index, RPY_SOURCE_DATA, SOURCE_DATA_LEN) {
                    Ok(d) => source(&mut o, &d),
                    // Gone in between, or refused
                    Err(Fail::Status(_)) => continue,
                    Err(e) => {
                        tracing::warn!("chronyd at {addr}: source {i}: {e}");
                        break;
                    }
                };
                let Some(l) = l else { continue };
                match c.call(REQ_SOURCESTATS, &index, RPY_SOURCESTATS, SOURCESTATS_LEN) {
                    Ok(d) => sourcestats(&mut o, &d, &l),
                    Err(Fail::Status(_)) => {}
                    Err(e) => {
                        tracing::warn!("chronyd at {addr}: sourcestats {i}: {e}");
                        break;
                    }
                }
            }
        }
        Err(e @ Fail::Status(_)) => tracing::debug!("chronyd at {addr}: sources: {e}"),
        Err(e) => tracing::warn!("chronyd at {addr}: sources: {e}"),
    }
    match c.call(REQ_SERVER_STATS, &[], RPY_SERVER_STATS4, SERVER_STATS_LEN) {
        Ok(d) => server_stats(&mut o, &d),
        // Not opened in chrony.conf, as on a client
        Err(e) => tracing::debug!("chronyd at {addr}: serverstats: {e}"),
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prom::Scalar;

    const STT_UNAUTH: u16 = 2;

    /// UTI_FloatHostToNetwork, to build replies
    fn float_bytes(x: f64) -> [u8; 4] {
        let (neg, x) = if x < 0.0 { (true, -x) } else { (false, x) };
        let (mut exp, mut coef): (i32, i32) = if x < 1e-100 {
            (0, 0)
        } else {
            let mut exp = (x.ln() / 2f64.ln()) as i32 + 1;
            let mut coef = (x * 2f64.powi(-exp + 25) + 0.5) as i32;
            while coef > (1 << 24) - 1 + i32::from(neg) {
                coef >>= 1;
                exp += 1;
            }
            (exp, coef)
        };
        if neg {
            coef = ((-coef as u32) << 7 >> 7) as i32;
        }
        exp &= 0x7f;
        ((exp as u32) << 25 | coef as u32).to_be_bytes()
    }

    fn ip4(a: [u8; 4]) -> Vec<u8> {
        let mut v = a.to_vec();
        v.resize(16, 0);
        v.extend_from_slice(&FAMILY_INET4.to_be_bytes());
        v.extend_from_slice(&[0, 0]);
        v
    }

    fn reply(command: u16, reply: u16, status: u16, seq: u32, data: &[u8]) -> Vec<u8> {
        let mut p = vec![PROTO_VERSION, PKT_TYPE_REPLY, 0, 0];
        p.extend_from_slice(&command.to_be_bytes());
        p.extend_from_slice(&reply.to_be_bytes());
        p.extend_from_slice(&status.to_be_bytes());
        p.extend_from_slice(&[0; 6]);
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&[0; 8]);
        p.extend_from_slice(data);
        p
    }

    fn tracking_data(ref_id: u32, stratum: u16, leap: u16, correction: f64) -> Vec<u8> {
        let mut d = ref_id.to_be_bytes().to_vec();
        d.extend(ip4([192, 0, 2, 1]));
        d.extend_from_slice(&stratum.to_be_bytes());
        d.extend_from_slice(&leap.to_be_bytes());
        // ref_time, 32-bit form
        d.extend_from_slice(&0x7fff_ffffu32.to_be_bytes());
        d.extend_from_slice(&1_800_000_000u32.to_be_bytes());
        d.extend_from_slice(&500_000_000u32.to_be_bytes());
        // correction, last and RMS offset, frequency, residual frequency,
        // skew, root delay, root dispersion, update interval
        for x in [correction, 1e-4, 2e-4, -13.0, 0.01, 0.05, 0.03, 0.002, 64.0] {
            d.extend_from_slice(&float_bytes(x));
        }
        assert_eq!(d.len(), TRACKING_LEN);
        d
    }

    fn source_data(addr: [u8; 4], mode: u16, state: u16, reach: u16, since: u32, offset: f64) -> Vec<u8> {
        let mut d = ip4(addr);
        // poll, stratum, state, mode, flags, reachability
        for x in [6, 1, state, mode, 0, reach] {
            d.extend_from_slice(&x.to_be_bytes());
        }
        d.extend_from_slice(&since.to_be_bytes());
        for x in [offset, offset, 0.016] {
            d.extend_from_slice(&float_bytes(x));
        }
        assert_eq!(d.len(), SOURCE_DATA_LEN);
        d
    }

    /// As chronyd sends it: an address, or for a reference clock only the refid
    fn sourcestats_data(ref_id: u32, addr: Option<[u8; 4]>, std_dev: f64) -> Vec<u8> {
        let mut d = vec![0u8; SOURCESTATS_LEN];
        d[..4].copy_from_slice(&ref_id.to_be_bytes());
        if let Some(a) = addr {
            d[4..24].copy_from_slice(&ip4(a));
        }
        d[36..40].copy_from_slice(&float_bytes(std_dev));
        d
    }

    fn get<'a>(s: &'a Scrape, metric: &str, source: Option<&str>) -> Option<&'a Scalar> {
        s.scalars
            .iter()
            .find(|x| x.metric == metric && source.is_none_or(|v| x.labels.get("source").map(String::as_str) == Some(v)))
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= b.abs() * 1e-6 + 1e-12
    }

    /// A fake chronyd for `n` requests: checks each request's header and its
    /// padding, sends the datagrams `answer` returns, and gives back the
    /// commands it saw
    fn serve(n: usize, answer: impl Fn(u16, u32, &[u8]) -> Vec<Vec<u8>> + Send + 'static) -> (SocketAddr, std::thread::JoinHandle<Vec<u16>>) {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        // a client that stops halfway fails the test instead of hanging it
        server.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let mut seen = Vec::new();
            for _ in 0..n {
                let (len, from) = server.recv_from(&mut buf).unwrap();
                let b = &buf[..len];
                assert_eq!(&b[..4], &[PROTO_VERSION, PKT_TYPE_REQUEST, 0, 0]);
                let (cmd, seq) = (be16(b, 4), be32(b, 8));
                let padded = match cmd {
                    REQ_TRACKING => 104,
                    REQ_N_SOURCES => 32,
                    REQ_SOURCE_DATA => 76,
                    REQ_SOURCESTATS => 84,
                    REQ_SERVER_STATS => 196,
                    _ => panic!("unexpected command {cmd}"),
                };
                assert_eq!(len, padded);
                seen.push(cmd);
                for r in answer(cmd, seq, b) {
                    server.send_to(&r, from).unwrap();
                }
            }
            seen
        });
        (addr, handle)
    }

    #[test]
    fn floats_decode_like_chrony() {
        for x in [0.0, 1.0, -1.0, 0.000_123_456, -0.004_25, 13.083, -2.5e-9, 64.0, 1e6] {
            let y = float(&float_bytes(x), 0);
            assert!(close(y, x), "{x} → {y}");
        }
        // exponent 1, coefficient 2^23: 2^23 · 2^(1 − 25) = 0.5
        assert_eq!(float(&(1u32 << 25 | 1 << 23).to_be_bytes(), 0), 0.5);
    }

    #[test]
    fn refids_print_like_chronyc() {
        assert_eq!(refid_name(u32::from_be_bytes(*b"GPS\0")), "GPS");
        // isprint: spaces stay, anything else is skipped, NUL included
        assert_eq!(refid_name(u32::from_be_bytes([b'A', 0, b' ', 0x7f])), "A ");
    }

    #[test]
    fn replies_are_checked() {
        let ok = reply(REQ_TRACKING, RPY_TRACKING, STT_SUCCESS, 9, &tracking_data(1, 2, 0, 0.0));
        assert!(parse_reply(&ok, REQ_TRACKING, RPY_TRACKING, TRACKING_LEN).is_ok());
        assert!(matches!(parse_reply(&ok, REQ_N_SOURCES, RPY_TRACKING, TRACKING_LEN), Err(Fail::Bad(_))));
        assert!(matches!(parse_reply(&ok[..60], REQ_TRACKING, RPY_TRACKING, TRACKING_LEN), Err(Fail::Bad(_))));
        let unauth = reply(REQ_SERVER_STATS, 1, STT_UNAUTH, 9, &[]);
        assert!(matches!(
            parse_reply(&unauth, REQ_SERVER_STATS, RPY_SERVER_STATS4, SERVER_STATS_LEN),
            Err(Fail::Status(STT_UNAUTH))
        ));
    }

    #[test]
    fn tracking_follows_chronyc() {
        let mut s = Scrape::default();
        // correction > 0: chronyc prints "slow", the clock is behind
        tracking(&mut s, &tracking_data(0xC000_0201, 2, 0, 0.0012), 1_800_000_010.5);
        assert!(close(get(&s, "host_chrony_system_time_offset_seconds", None).unwrap().value, -0.0012));
        assert_eq!(get(&s, "host_chrony_synchronized", None).unwrap().value, 1.0);
        assert_eq!(get(&s, "host_chrony_local_reference", None).unwrap().value, 0.0);
        assert_eq!(get(&s, "host_chrony_stratum", None).unwrap().value, 2.0);
        assert!(close(get(&s, "host_chrony_reference_age_seconds", None).unwrap().value, 10.0));
        assert!(close(get(&s, "host_chrony_frequency_ppm", None).unwrap().value, -13.0));
        assert!(close(get(&s, "host_chrony_max_error_seconds", None).unwrap().value, 0.0012 + 0.002 + 0.015));

        let mut s = Scrape::default();
        tracking(&mut s, &tracking_data(LOCAL_REFID, 10, 0, 0.0), 0.0);
        assert_eq!(get(&s, "host_chrony_local_reference", None).unwrap().value, 1.0);
        let mut s = Scrape::default();
        tracking(&mut s, &tracking_data(0, 0, 3, 0.0), 0.0);
        assert_eq!(get(&s, "host_chrony_synchronized", None).unwrap().value, 0.0);
    }

    #[test]
    fn sources_and_refclocks() {
        let mut s = Scrape::default();
        let l = source(&mut s, &source_data([192, 0, 2, 1], 0, 0, 0o377, 33, -0.000274)).unwrap();
        sourcestats(&mut s, &sourcestats_data(0, Some([192, 0, 2, 1]), 1e-4), &l);
        // the list changed in between: no jitter under the wrong source
        sourcestats(&mut s, &sourcestats_data(0, Some([192, 0, 2, 9]), 1e-4), &l);
        // no sample yet: no offset
        source(&mut s, &source_data([198, 51, 100, 7], 0, 1, 0o42, u32::MAX, 0.0));
        // a reference clock: chronyd sends its refid as an IPv4 address
        let l = source(&mut s, &source_data(*b"GPS\0", MODE_REFCLOCK, 5, 0o17, 1, 1e-7)).unwrap();
        sourcestats(&mut s, &sourcestats_data(u32::from_be_bytes(*b"GPS\0"), None, 2e-7), &l);

        assert_eq!(get(&s, "host_chrony_source_state", Some("192.0.2.1")).unwrap().value, 0.0);
        assert_eq!(get(&s, "host_chrony_source_reachability_ratio", Some("192.0.2.1")).unwrap().value, 1.0);
        assert!(close(get(&s, "host_chrony_source_offset_seconds", Some("192.0.2.1")).unwrap().value, -0.000274));
        assert_eq!(get(&s, "host_chrony_source_reachability_ratio", Some("198.51.100.7")).unwrap().value, 0.25);
        assert!(get(&s, "host_chrony_source_offset_seconds", Some("198.51.100.7")).is_none());
        let g = get(&s, "host_chrony_source_state", Some("GPS")).unwrap();
        assert_eq!((g.labels["mode"].as_str(), g.value), ("refclock", 5.0));
        // jitter carries the same labels as the rest of the source
        let jitter: Vec<_> = s.scalars.iter().filter(|x| x.metric == "host_chrony_source_std_dev_seconds").collect();
        assert_eq!(jitter.len(), 2);
        assert_eq!(jitter[0].labels, source_labels("192.0.2.1", "server"));
        assert_eq!(jitter[1].labels, source_labels("GPS", "refclock"));
    }

    fn source_labels(source: &str, mode: &str) -> Labels {
        [("source".to_owned(), source.to_owned()), ("mode".to_owned(), mode.to_owned())].into()
    }

    #[test]
    fn server_stats_are_counters() {
        let mut d = vec![0u8; SERVER_STATS_LEN];
        d[0..8].copy_from_slice(&(5u64 << 32 | 7).to_be_bytes());
        d[24..32].copy_from_slice(&3u64.to_be_bytes());
        let mut s = Scrape::default();
        server_stats(&mut s, &d);
        let r = get(&s, "host_chrony_server_ntp_requests_total", None).unwrap();
        assert!(r.counter);
        assert_eq!(r.value, (5u64 << 32 | 7) as f64);
        assert_eq!(get(&s, "host_chrony_server_ntp_dropped_total", None).unwrap().value, 3.0);
    }

    #[test]
    fn talks_to_chronyd() {
        let (addr, server) = serve(5, |cmd, seq, b| match cmd {
            REQ_TRACKING => vec![reply(cmd, RPY_TRACKING, STT_SUCCESS, seq, &tracking_data(0xC000_0201, 3, 0, -0.0001))],
            REQ_N_SOURCES => vec![reply(cmd, RPY_N_SOURCES, STT_SUCCESS, seq, &[0, 0, 0, 1])],
            REQ_SOURCE_DATA => {
                assert_eq!(be32(b, REQUEST_HEADER), 0);
                // a stale reply first: it must be skipped
                vec![
                    reply(cmd, RPY_SOURCE_DATA, STT_SUCCESS, seq.wrapping_sub(1), &[0; SOURCE_DATA_LEN]),
                    reply(cmd, RPY_SOURCE_DATA, STT_SUCCESS, seq, &source_data([192, 0, 2, 1], 0, 0, 0o377, 5, 0.0002)),
                ]
            }
            REQ_SOURCESTATS => {
                let d = sourcestats_data(0, Some([192, 0, 2, 1]), 3e-5);
                vec![reply(cmd, RPY_SOURCESTATS, STT_SUCCESS, seq, &d)]
            }
            // serverstats, not opened
            _ => vec![reply(cmd, 1, STT_UNAUTH, seq, &[])],
        });
        let s = collect(addr);
        assert_eq!(
            server.join().unwrap(),
            [REQ_TRACKING, REQ_N_SOURCES, REQ_SOURCE_DATA, REQ_SOURCESTATS, REQ_SERVER_STATS]
        );
        assert_eq!(get(&s, "host_chrony_up", None).unwrap().value, 1.0);
        assert!(close(get(&s, "host_chrony_system_time_offset_seconds", None).unwrap().value, 0.0001));
        assert_eq!(get(&s, "host_chrony_source_state", Some("192.0.2.1")).unwrap().value, 0.0);
        assert!(close(get(&s, "host_chrony_source_std_dev_seconds", Some("192.0.2.1")).unwrap().value, 3e-5));
        assert!(get(&s, "host_chrony_server_ntp_requests_total", None).is_none());
    }

    #[test]
    fn refusing_everything_is_still_up() {
        // opencommands without the defaults
        let (addr, server) = serve(3, |cmd, seq, _| vec![reply(cmd, 1, STT_UNAUTH, seq, &[])]);
        let s = collect(addr);
        assert_eq!(server.join().unwrap(), [REQ_TRACKING, REQ_N_SOURCES, REQ_SERVER_STATS]);
        assert_eq!(get(&s, "host_chrony_up", None).unwrap().value, 1.0);
        assert_eq!(s.scalars.len(), 1);
    }

    #[test]
    fn stale_replies_cannot_stall() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let (_, from) = server.recv_from(&mut buf).unwrap();
            let seq = be32(&buf, 8);
            // a wrong sequence number every 100 ms, for longer than a reading may take
            let stale = reply(REQ_TRACKING, RPY_TRACKING, STT_SUCCESS, seq ^ 1, &[0; TRACKING_LEN]);
            for _ in 0..50 {
                let _ = server.send_to(&stale, from);
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let t = Instant::now();
        let s = collect(addr);
        assert!(t.elapsed() < TIMEOUT * 2, "{:?}", t.elapsed());
        assert_eq!(get(&s, "host_chrony_up", None).unwrap().value, 0.0);
    }

    #[test]
    fn no_chronyd_is_down() {
        // a port nobody listens on: refused at once
        let free = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = free.local_addr().unwrap();
        drop(free);
        let s = collect(addr);
        assert_eq!(get(&s, "host_chrony_up", None).unwrap().value, 0.0);
        assert_eq!(s.scalars.len(), 1);
    }
}
