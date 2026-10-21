//! The wire between clients: turn, desync-hash and leave messages, and the link they
//! travel over.
//!
//! This is our own protocol (ShieldBattery's `netcode_v2` has its own, tied to its relay);
//! it is deliberately small and self-describing so it can be carried by CrownLink's SNP
//! path or by a relay without either side knowing what a turn *means*. The engine-facing
//! half — how a turn is produced and injected — is `crate::rollback`'s job, and the
//! scheduling is `crate::session`'s.
//!
//! Every message is `[version u8][tag u8][payload]`, little-endian, with lengths explicit.
//! `decode` is total: it returns `None` for anything short, mis-versioned, or oversized,
//! so a malformed datagram from a peer cannot make the client allocate wildly or panic.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::time::Instant;

/// Bumped when the encoding changes; a peer with a different version is rejected rather
/// than misparsed. Version 2 added the reporting slot to `Hash` (v1 was ambiguous about
/// which peer a hash came from) and the authority's `LeadReport`/`TurnsComplete`.
pub const WIRE_VERSION: u8 = 2;

/// Largest turn blob accepted, to bound allocation from a hostile or corrupt peer. A turn
/// is a frame's worth of commands; 4 KiB is far above anything the engine produces.
pub const MAX_TURN_BYTES: usize = 4096;

const TAG_TURN: u8 = 1;
const TAG_HASH: u8 = 2;
const TAG_LEAVE: u8 = 3;
const TAG_LEAD_REPORT: u8 = 4;
const TAG_TURNS_COMPLETE: u8 = 5;
const TAG_ACK: u8 = 6;

/// Slots the engine's turn scheduler can have; the authority's `TurnsComplete` reports one
/// count per slot.
pub const SLOTS: usize = 8;

/// One message between clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// The commands slot `slot` issued for the step that produces frame `frame + 1`.
    Turn { slot: u8, frame: u32, bytes: Vec<u8> },
    /// Slot `slot`'s [`crate::state_hash::desync_hash`] of frame `frame`, for the peer to
    /// compare against its own.
    Hash { slot: u8, frame: u32, hash: u64 },
    /// The relay directed `slot` to leave at `step`, with the engine's native reason.
    Leave { slot: u8, reason: u32, step: u32 },
    /// The authority's view of how late turns are, over a recent window (ADR-0001 §1.1).
    /// Broadcast by the host every 12 steps; clients use it to pace their lead.
    LeadReport {
        median_lateness: u32,
        p90_lateness: u32,
        clock_stopped_ms: u32,
    },
    /// Per-slot counts of turns the authority holds, stamped on every authority packet so a
    /// client can see how far the authority has got.
    TurnsComplete { counts: [u32; SLOTS] },
    /// `slot`'s highest turn sequence number that the sender has received **contiguously**.
    ///
    /// Turns travel over plain UDP (libjuice has no reliability API; CrownLink implements
    /// acks/resends for the engine's own stream), so a turn can be lost. Prediction and
    /// rollback only help while the turn is *in flight*; a turn lost for good would diverge
    /// the clients permanently. The receiver therefore acknowledges and the sender re-sends
    /// everything above the ack (ADR-0006).
    Ack { slot: u8, seq: u32 },
}

impl Msg {
    /// The frame this message concerns, where it has one (authority reports do not).
    pub fn frame(&self) -> u32 {
        match self {
            Msg::Turn { frame, .. } | Msg::Hash { frame, .. } => *frame,
            Msg::Leave { step, .. } => *step,
            Msg::Ack { seq, .. } => *seq,
            Msg::LeadReport { .. } | Msg::TurnsComplete { .. } => 0,
        }
    }

    /// The slot this message concerns, where it has one.
    pub fn slot(&self) -> Option<u8> {
        match self {
            Msg::Turn { slot, .. }
            | Msg::Hash { slot, .. }
            | Msg::Leave { slot, .. }
            | Msg::Ack { slot, .. } => Some(*slot),
            Msg::LeadReport { .. } | Msg::TurnsComplete { .. } => None,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        out.push(WIRE_VERSION);
        match self {
            Msg::Turn { slot, frame, bytes } => {
                out.push(TAG_TURN);
                out.push(*slot);
                out.extend_from_slice(&frame.to_le_bytes());
                out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                out.extend_from_slice(bytes);
            }
            Msg::Hash { slot, frame, hash } => {
                out.push(TAG_HASH);
                out.push(*slot);
                out.extend_from_slice(&frame.to_le_bytes());
                out.extend_from_slice(&hash.to_le_bytes());
            }
            Msg::Leave { slot, reason, step } => {
                out.push(TAG_LEAVE);
                out.push(*slot);
                out.extend_from_slice(&reason.to_le_bytes());
                out.extend_from_slice(&step.to_le_bytes());
            }
            Msg::LeadReport {
                median_lateness,
                p90_lateness,
                clock_stopped_ms,
            } => {
                out.push(TAG_LEAD_REPORT);
                out.extend_from_slice(&median_lateness.to_le_bytes());
                out.extend_from_slice(&p90_lateness.to_le_bytes());
                out.extend_from_slice(&clock_stopped_ms.to_le_bytes());
            }
            Msg::TurnsComplete { counts } => {
                out.push(TAG_TURNS_COMPLETE);
                for count in counts {
                    out.extend_from_slice(&count.to_le_bytes());
                }
            }
            Msg::Ack { slot, seq } => {
                out.push(TAG_ACK);
                out.push(*slot);
                out.extend_from_slice(&seq.to_le_bytes());
            }
        }
        out
    }

    /// Decode one message from the front of `buf`, returning it and how many bytes it
    /// used. `None` for anything that is not a well-formed message of this version.
    pub fn decode(buf: &[u8]) -> Option<(Msg, usize)> {
        if buf.len() < 2 || buf[0] != WIRE_VERSION {
            return None;
        }
        let mut p = 2usize;
        let take = |p: &mut usize, n: usize| -> Option<&[u8]> {
            let s = buf.get(*p..*p + n)?;
            *p += n;
            Some(s)
        };
        match buf[1] {
            TAG_TURN => {
                let slot = *buf.get(p)?;
                p += 1;
                let frame = u32::from_le_bytes(take(&mut p, 4)?.try_into().ok()?);
                let len = u16::from_le_bytes(take(&mut p, 2)?.try_into().ok()?) as usize;
                if len > MAX_TURN_BYTES {
                    return None;
                }
                let bytes = take(&mut p, len)?.to_vec();
                Some((Msg::Turn { slot, frame, bytes }, p))
            }
            TAG_HASH => {
                let slot = *buf.get(p)?;
                p += 1;
                let frame = u32::from_le_bytes(take(&mut p, 4)?.try_into().ok()?);
                let hash = u64::from_le_bytes(take(&mut p, 8)?.try_into().ok()?);
                Some((Msg::Hash { slot, frame, hash }, p))
            }
            TAG_LEAVE => {
                let slot = *buf.get(p)?;
                p += 1;
                let reason = u32::from_le_bytes(take(&mut p, 4)?.try_into().ok()?);
                let step = u32::from_le_bytes(take(&mut p, 4)?.try_into().ok()?);
                Some((Msg::Leave { slot, reason, step }, p))
            }
            TAG_LEAD_REPORT => {
                let median_lateness = u32::from_le_bytes(take(&mut p, 4)?.try_into().ok()?);
                let p90_lateness = u32::from_le_bytes(take(&mut p, 4)?.try_into().ok()?);
                let clock_stopped_ms = u32::from_le_bytes(take(&mut p, 4)?.try_into().ok()?);
                Some((
                    Msg::LeadReport {
                        median_lateness,
                        p90_lateness,
                        clock_stopped_ms,
                    },
                    p,
                ))
            }
            TAG_TURNS_COMPLETE => {
                let mut counts = [0u32; SLOTS];
                for count in &mut counts {
                    *count = u32::from_le_bytes(take(&mut p, 4)?.try_into().ok()?);
                }
                Some((Msg::TurnsComplete { counts }, p))
            }
            TAG_ACK => {
                let slot = *buf.get(p)?;
                p += 1;
                let seq = u32::from_le_bytes(take(&mut p, 4)?.try_into().ok()?);
                Some((Msg::Ack { slot, seq }, p))
            }
            _ => None,
        }
    }
}

/// Where turns and hashes travel. The engine-facing client only ever calls these two.
pub trait Link {
    /// Hands a message to the link for the peer.
    fn send(&mut self, msg: Msg);
    /// Takes the next message the peer has sent, if any.
    fn recv(&mut self) -> Option<Msg>;

    /// Whether the client must inject turns into the engine itself.
    ///
    /// True for carriers with no engine-side turn source: the scripted replay harness and the
    /// UDP test, where the peer's turns exist only in our input table. False for CrownLink: the
    /// engine receives the peer's turns through the `.snp` and applies them itself, so injecting
    /// the recorded copy as well applies every command twice — which crashes inside
    /// `ProcessCommands` once the match's init turns are replayed.
    fn inject_turns(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------------------
// The real link: CrownLink's rollback channel.
//
// CrownLink.snp (the fork carrying `src/snp/RollbackChannel.h`) exports a small stdcall ABI
// that the rollback DLL resolves by name from the loaded module — no link-time dependency,
// exactly like the plugin serializers. Messages travel as CrownLink custom packets with a
// dedicated sub-type, so they never enter the engine's lockstep receive queue.
//
// The exports (ADR-0001 §2.1, with the two deviations documented in RollbackChannel.h):
//   u32  __stdcall cl_rollback_abi(void);
//   void __stdcall cl_rollback_set_receive_cb(void(__stdcall*)(const NetAddress*, const void*, u32));
//   int  __stdcall cl_rollback_broadcast(const void* data, u32 len);
//   int  __stdcall cl_rollback_send_redundant(const void* data, u32 len);
// ---------------------------------------------------------------------------------------

/// `void(__stdcall*)(const NetAddress* peer, const void* data, u32 len)` — the peer address
/// is opaque here; the session cares only about the bytes.
type ReceiveCallback = unsafe extern "system" fn(*const u8, *const u8, u32);
type AbiFn = unsafe extern "system" fn() -> u32;
type SetReceiveCbFn = unsafe extern "system" fn(ReceiveCallback);
type SendFn = unsafe extern "system" fn(*const u8, u32) -> i32;

/// The ABI version this client speaks; must match `cl_rollback_abi`.
pub const CROWNLINK_ROLLBACK_ABI: u32 = 1;

#[derive(Clone, Copy)]
struct Api {
    set_receive_cb: SetReceiveCbFn,
    broadcast: SendFn,
    send_redundant: SendFn,
}

/// Messages that arrived on the CrownLink receive thread and have not been taken yet.
///
/// The callback is a plain `fn` with no user data (the ABI has none), so the queue is a
/// static. It is only ever touched from the receive thread and the game thread.
static INCOMING: once_cell::sync::Lazy<std::sync::Mutex<VecDeque<Msg>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(VecDeque::new()));

/// Channel traffic counters, logged sparsely so a stuck link can be told apart from a silent
/// one without drowning the log at 8 turns per second.
static RECV_N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static SEND_N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

unsafe extern "system" fn on_recv(_peer: *const u8, data: *const u8, len: u32) {
    if data.is_null() || len == 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(data, len as usize);
    // A malformed or mis-versioned datagram from a peer is dropped, never fatal.
    match Msg::decode(bytes) {
        Some((msg, _)) => {
            let n = RECV_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n <= 3 || n % 500 == 0 {
                crate::log::log(&format!("[cosmo-rollback] live: channel recv #{n} ({len} bytes)"));
            }
            if let Ok(mut q) = INCOMING.lock() {
                q.push_back(msg);
            }
        }
        None => {
            crate::log::log(&format!(
                "[cosmo-rollback] live: channel recv dropped undecodable {len}-byte datagram"
            ));
        }
    }
}

/// The CrownLink rollback channel, if CrownLink.snp is loaded and exports it.
pub struct CrownLink {
    api: Api,
    /// Artificial one-way delay for our own messages (`CM_ROLLBACK_SEND_DELAY_MS`).
    ///
    /// A same-machine match has ~1 ms RTT, so turns arrive before the client needs them and
    /// nothing ever mispredicts. Delaying the send is what makes the prediction and rollback
    /// path run for real without touching the host's network stack.
    delay: std::time::Duration,
    /// Messages waiting for their delay to expire.
    pending: std::collections::VecDeque<(std::time::Instant, Msg)>,
}

impl CrownLink {
    /// Resolves the channel from the loaded `CrownLink.snp`, registers the receive callback
    /// and returns the link. `None` when the module or any export is missing (e.g. the
    /// stock `.snp`), so the caller can fall back.
    pub fn connect() -> Option<Self> {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        unsafe {
            let h = GetModuleHandleA(c"crownlink.snp".as_ptr() as *const u8);
            if h.is_null() {
                return None;
            }
            let sym = |name: &std::ffi::CStr| -> usize {
                match GetProcAddress(h, name.as_ptr() as *const u8) {
                    Some(f) => f as usize,
                    None => 0,
                }
            };
            let abi = sym(c"cl_rollback_abi");
            let set_receive_cb = sym(c"cl_rollback_set_receive_cb");
            let broadcast = sym(c"cl_rollback_broadcast");
            let send_redundant = sym(c"cl_rollback_send_redundant");
            if abi == 0 || set_receive_cb == 0 || broadcast == 0 || send_redundant == 0 {
                return None;
            }
            let abi_fn: AbiFn = std::mem::transmute(abi);
            if abi_fn() != CROWNLINK_ROLLBACK_ABI {
                return None;
            }
            let api = Api {
                set_receive_cb: std::mem::transmute(set_receive_cb),
                broadcast: std::mem::transmute(broadcast),
                send_redundant: std::mem::transmute(send_redundant),
            };
            (api.set_receive_cb)(on_recv);
            let delay = std::env::var("CM_ROLLBACK_SEND_DELAY_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .map(std::time::Duration::from_millis)
                .unwrap_or_default();
            crate::log::log(&format!(
                "[cosmo-rollback] live: CrownLink rollback channel bound (abi 1, send delay {delay:?})"
            ));
            Some(CrownLink { api, delay, pending: Default::default() })
        }
    }

    /// Sends a reliability-critical message over both the direct P2P link and the relay.
    pub fn send_redundant(&self, msg: &Msg) {
        let bytes = msg.encode();
        unsafe { (self.api.send_redundant)(bytes.as_ptr(), bytes.len() as u32) };
    }
}

impl CrownLink {
    /// Hands one encoded message to the SNP.
    fn transmit(&self, msg: &Msg) {
        let bytes = msg.encode();
        let ok = unsafe { (self.api.broadcast)(bytes.as_ptr(), bytes.len() as u32) };
        let n = SEND_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n <= 3 || n % 500 == 0 {
            crate::log::log(&format!(
                "[cosmo-rollback] live: channel send #{n} ({} bytes) -> {ok}",
                bytes.len()
            ));
        }
    }

    /// Sends everything whose delay has expired.
    fn flush_due(&mut self) {
        let now = std::time::Instant::now();
        while matches!(self.pending.front(), Some((at, _)) if *at <= now) {
            if let Some((_, msg)) = self.pending.pop_front() {
                self.transmit(&msg);
            }
        }
    }
}

impl Link for CrownLink {
    fn inject_turns(&self) -> bool {
        false
    }

    fn send(&mut self, msg: Msg) {
        if self.delay.is_zero() {
            self.transmit(&msg);
            return;
        }
        self.pending.push_back((std::time::Instant::now() + self.delay, msg));
        self.flush_due();
    }

    fn recv(&mut self) -> Option<Msg> {
        self.flush_due();
        INCOMING.lock().ok()?.pop_front()
    }
}

/// A real UDP link, for running two clients on one machine (a soak/CI test that needs no
/// CrownLink session). It carries the **same** `Msg` encoding as CrownLink's channel, so it
/// exercises the same codec, queue and cadence — only the carrier differs.
///
/// `delay` holds outbound datagrams for that long, which is what makes prediction and
/// rollback actually happen on a loopback link (a zero-delay link never mispredicts).
pub struct UdpLink {
    socket: std::net::UdpSocket,
    peer: std::net::SocketAddr,
    delay: std::time::Duration,
    /// Outbound datagrams waiting for their delay to expire.
    pending: VecDeque<(Instant, Vec<u8>)>,
}

impl UdpLink {
    /// Binds `local` and sends to `127.0.0.1:peer`.
    pub fn new(local: u16, peer: u16, delay: std::time::Duration) -> Option<Self> {
        let socket = std::net::UdpSocket::bind(("127.0.0.1", local)).ok()?;
        socket.set_nonblocking(true).ok()?;
        let peer = format!("127.0.0.1:{peer}").parse().ok()?;
        Some(Self {
            socket,
            peer,
            delay,
            pending: VecDeque::new(),
        })
    }
}

impl Link for UdpLink {
    fn send(&mut self, msg: Msg) {
        let bytes = msg.encode();
        if self.delay.is_zero() {
            let _ = self.socket.send_to(&bytes, self.peer);
        } else {
            self.pending.push_back((Instant::now() + self.delay, bytes));
        }
    }

    fn recv(&mut self) -> Option<Msg> {
        // Release the datagrams whose delay has expired, then take one that arrived.
        let now = Instant::now();
        while let Some((at, _)) = self.pending.front() {
            if *at > now {
                break;
            }
            let bytes = self.pending.pop_front().expect("front").1;
            let _ = self.socket.send_to(&bytes, self.peer);
        }
        let mut buf = [0u8; 2048];
        match self.socket.recv_from(&mut buf) {
            // A malformed datagram is dropped, never fatal.
            Ok((n, _)) => Msg::decode(&buf[..n]).map(|(m, _)| m),
            Err(_) => None,
        }
    }
}

/// Resolves CrownLink's rollback channel and logs whether a match can use it.
///
/// Called lazily: storm loads the `.snp` at provider-bind time, which is *after* plugin
/// startup, so at startup the module is not in the process yet. `connect()` itself is
/// callable whenever the channel is needed; this is the diagnostic wrapper.
pub fn log_channel() {
    use crate::log::log;
    // Diagnostic (`CM_ROLLBACK_CHANNEL_LOAD=1`): load `CrownLink.snp` from the game directory
    // first. storm only loads it when a provider is bound, so in a replay this is the only way
    // to exercise the resolution path.
    if std::env::var("CM_ROLLBACK_CHANNEL_LOAD").is_ok() {
        unsafe { load_snp_for_probe() };
    }
    // The `.snp` is loaded by storm at provider-bind time, so list what is actually loaded —
    // in a replay (no provider bound) CrownLink.snp is simply absent.
    log("[cosmo-rollback] modules matching *snp*/*Crown*:");
    crate::modules::log_modules_matching(&["snp", "crown", "juice"]);
    match CrownLink::connect() {
        Some(_) => log(&format!(
            "[cosmo-rollback] CrownLink rollback channel: resolved (abi {CROWNLINK_ROLLBACK_ABI})"
        )),
        None => log(
            "[cosmo-rollback] CrownLink rollback channel: not available (stock CrownLink.snp?)",
        ),
    }
}

/// Load `CrownLink.snp` from the directory of the running executable, for the channel probe.
/// Diagnostic only (`CM_ROLLBACK_CHANNEL_LOAD=1`).
unsafe fn load_snp_for_probe() {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameA, LoadLibraryA};
    let mut buf = [0u8; 512];
    let n = GetModuleFileNameA(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32);
    if n == 0 || n as usize >= buf.len() {
        return;
    }
    let exe = std::slice::from_raw_parts(buf.as_ptr(), n as usize);
    // Strip the file name, keeping the trailing separator.
    let dir = match exe.iter().rposition(|&c| c == b'\\' || c == b'/') {
        Some(i) => &exe[..=i],
        None => return,
    };
    let mut path = dir.to_vec();
    path.extend_from_slice(b"CrownLink.snp\0");
    let h = LoadLibraryA(path.as_ptr());
    crate::log::log(&format!(
        "[cosmo-rollback] channel probe: LoadLibraryA(\"{}CrownLink.snp\") -> {}",
        String::from_utf8_lossy(dir),
        if h.is_null() { "failed" } else { "ok" }
    ));
}

/// One-shot diagnostic (`CM_ROLLBACK_CHANNEL_PROBE=1`): log whether the CrownLink channel
/// resolved, the first time a snapshot is taken (i.e. once the match is running and the
/// `.snp` is loaded). Cheap when the knob is unset: one relaxed load.
pub fn probe_channel_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.load(Ordering::Relaxed) {
        return;
    }
    if std::env::var("CM_ROLLBACK_CHANNEL_PROBE").is_err() {
        DONE.store(true, Ordering::Relaxed);
        return;
    }
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    log_channel();
}

/// A boxed link is itself a link, so the client can hold whichever carrier it was given
/// (CrownLink, UDP, or the loopback) behind one field.
impl Link for Box<dyn Link + Send> {
    fn send(&mut self, msg: Msg) {
        (**self).send(msg);
    }
    fn recv(&mut self) -> Option<Msg> {
        (**self).recv()
    }
    fn inject_turns(&self) -> bool {
        // Must forward: the default would report `true` for every boxed carrier and silently
        // turn injection back on for CrownLink.
        (**self).inject_turns()
    }
}

/// An in-process link with a fixed delivery delay, for tests and for a single-machine
/// soak run. Messages arrive exactly `delay` ticks after `send`, and never reorder.
pub struct Loopback {
    delay: u32,
    now: u32,
    queue: VecDeque<(u32, Msg)>,
}

impl Loopback {
    pub fn new(delay: u32) -> Self {
        Self {
            delay,
            now: 0,
            queue: VecDeque::new(),
        }
    }

    /// Advances the link's clock by one tick, which may make messages receivable.
    pub fn tick(&mut self) {
        self.now = self.now.wrapping_add(1);
    }

    /// The link's current tick.
    pub fn now(&self) -> u32 {
        self.now
    }

    /// Messages still in flight.
    pub fn in_flight(&self) -> usize {
        self.queue.len()
    }
}

impl Link for Loopback {
    fn send(&mut self, msg: Msg) {
        self.queue.push_back((self.now.wrapping_add(self.delay), msg));
    }

    fn recv(&mut self) -> Option<Msg> {
        match self.queue.front() {
            Some((at, _)) if *at <= self.now => self.queue.pop_front().map(|(_, m)| m),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(m: Msg) {
        let enc = m.encode();
        let (back, used) = Msg::decode(&enc).expect("decode");
        assert_eq!(back, m);
        assert_eq!(used, enc.len());
    }

    #[test]
    fn every_message_round_trips() {
        round_trip(Msg::Turn {
            slot: 1,
            frame: 0x1234_5678,
            bytes: vec![9, 8, 7],
        });
        round_trip(Msg::Turn {
            slot: 0,
            frame: 0,
            bytes: Vec::new(),
        });
        round_trip(Msg::Hash {
            slot: 1,
            frame: 42,
            hash: 0xdead_beef_cafe_f00d,
        });
        round_trip(Msg::Leave {
            slot: 2,
            reason: 7,
            step: 999,
        });
        round_trip(Msg::LeadReport {
            median_lateness: 3,
            p90_lateness: 11,
            clock_stopped_ms: 250,
        });
        round_trip(Msg::TurnsComplete {
            counts: [1, 2, 3, 4, 5, 6, 7, 8],
        });
    }

    #[test]
    fn concatenated_messages_decode_in_order() {
        let mut buf = Vec::new();
        buf.extend(Msg::Hash { slot: 1, frame: 1, hash: 2 }.encode());
        buf.extend(Msg::Leave { slot: 0, reason: 3, step: 4 }.encode());
        let (m1, n1) = Msg::decode(&buf).unwrap();
        let (m2, _) = Msg::decode(&buf[n1..]).unwrap();
        assert_eq!(m1, Msg::Hash { slot: 1, frame: 1, hash: 2 });
        assert_eq!(m2, Msg::Leave { slot: 0, reason: 3, step: 4 });
    }

    #[test]
    fn malformed_input_is_rejected_rather_than_misparsed() {
        assert!(Msg::decode(&[]).is_none());
        assert!(Msg::decode(&[WIRE_VERSION]).is_none());
        // Wrong version.
        let mut bad = Msg::Hash { slot: 1, frame: 1, hash: 2 }.encode();
        bad[0] = WIRE_VERSION + 1;
        assert!(Msg::decode(&bad).is_none());
        // Unknown tag.
        let mut bad = Msg::Hash { slot: 1, frame: 1, hash: 2 }.encode();
        bad[1] = 99;
        assert!(Msg::decode(&bad).is_none());
        // Truncated payload.
        let enc = Msg::Turn { slot: 0, frame: 5, bytes: vec![1, 2, 3] }.encode();
        for cut in 1..enc.len() {
            assert!(Msg::decode(&enc[..cut]).is_none(), "accepted {cut} bytes");
        }
    }

    #[test]
    fn an_oversized_turn_is_rejected() {
        let mut buf = vec![WIRE_VERSION, TAG_TURN, 0];
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&((MAX_TURN_BYTES + 1) as u16).to_le_bytes());
        assert!(Msg::decode(&buf).is_none());
    }

    #[test]
    fn loopback_delivers_exactly_after_its_delay() {
        let mut l = Loopback::new(3);
        l.send(Msg::Hash { slot: 1, frame: 0, hash: 1 });
        assert_eq!(l.recv(), None);
        for _ in 0..2 {
            l.tick();
            assert_eq!(l.recv(), None, "arrived early");
        }
        l.tick();
        assert_eq!(l.recv(), Some(Msg::Hash { slot: 1, frame: 0, hash: 1 }));
        assert_eq!(l.recv(), None);
        assert_eq!(l.in_flight(), 0);
    }

    #[test]
    fn loopback_preserves_order() {
        let mut l = Loopback::new(1);
        l.send(Msg::Hash { slot: 1, frame: 1, hash: 1 });
        l.send(Msg::Hash { slot: 1, frame: 2, hash: 2 });
        l.tick();
        assert_eq!(l.recv().unwrap().frame(), 1);
        assert_eq!(l.recv().unwrap().frame(), 2);
    }
}
