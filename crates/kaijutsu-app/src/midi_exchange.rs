//! The sink's exchange client — where a kernel question actually meets the
//! hardware (`docs/midi-next.md` "SysEx: the exchange pattern", slice 1
//! step 5).
//!
//! ```text
//!   kj midi identify ──► kernel registry ──► server bridge ──► BlockEvents.exchange
//!                                                                    │
//!                              MidiExchangeSlot (kaijutsu-client) ◄──┘
//!                                       │ MidiExchangeRequest
//!                                       ▼
//!                      "kaijutsu-exchange" thread — its OWN ALSA seq client
//!                          send direct ─► device port ─► temp subscription ─► reply
//! ```
//!
//! Four properties, each of them a rule from the doc rather than a
//! convenience:
//!
//! - **Its own ALSA client**, named `kaijutsu-exchange` — never the render
//!   client and never the ear. The doc's requirement is that *the ear never
//!   sees request/reply traffic*: a settings dump answering an exchange is
//!   not music, and it must not land in the capture ring. Client separation
//!   is how render and capture already stay apart (`midi_in.rs`,
//!   `dj/midi.rs`); this is the third client in that pattern.
//! - **Serialized.** One worker thread, one request at a time, so two
//!   exchanges can never interleave their replies. (The server's bridge
//!   serializes per connection too; this is the hardware-side half.)
//! - **Addressed the same way `kj midi send` is.** A device name resolves
//!   through the app's own routing table (`crate::midi_presence`'s matcher,
//!   the same `device → addresses` picture the DJ thread routes control cues
//!   with) and the request goes out **direct** to that port — no standing
//!   subscription, so asking a device a question never wires the score into
//!   it.
//! - **The reply subscription is temporary.** ALSA delivers *input* only to a
//!   port that is subscribed to the source, so hearing an answer needs one —
//!   but it is taken on the exchange client (which never feeds capture) and
//!   dropped the moment the exchange ends, so at no point does a device's
//!   ordinary playing reach anything through it.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bevy::prelude::*;
use kaijutsu_client::MidiExchangeRequest;

use crate::connection::actor_plugin::RpcActor;

/// device → its matched ports' backend addresses, in match order. The same
/// shape (and the same source) as [`crate::dj::midi::MidiRoutes`]: the app's
/// matcher computes one picture and both the control-cue path and the
/// exchange path route through it.
pub type ExchangeRoutes = BTreeMap<String, Vec<String>>;

/// How often the worker polls for a reply while waiting. Small enough that a
/// fast device's answer isn't sat on, large enough that a 2 s wait isn't a
/// spin.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Which port an exchange goes out of, or a message saying why none does.
///
/// Two accepted forms, distinguished at the grammar and never sniffed:
///
/// 1. a **device name** the matcher has routes for — slice 1's rule, the same
///    as `kj midi send`: the device's FIRST matched port (role-aware choice
///    is slice 2),
/// 2. a **backend address** (`"24:0"`) for a caller that already knows the
///    port. The kernel never sends this form — it deals in device names — but
///    the wire allows it and a local tool may use it.
///
/// A name we have no route for is a refusal, never a fallback to "some port":
/// an exchange is a dialogue with one specific instrument, and asking the
/// wrong one is worse than asking nobody.
pub(crate) fn resolve_exchange_address(
    routes: &ExchangeRoutes,
    port_or_device: &str,
) -> Result<String, String> {
    if let Some(address) = routes.get(port_or_device).and_then(|a| a.first()) {
        return Ok(address.clone());
    }
    if looks_like_alsa_address(port_or_device) {
        return Ok(port_or_device.to_string());
    }
    Err(format!(
        "no port matched '{port_or_device}' on this sink (devices here: {:?}) — \
         an exchange goes to its device or nowhere",
        routes.keys().collect::<Vec<_>>()
    ))
}

/// `"client:port"`, both halves numeric. Deliberately strict: a CoreMIDI-style
/// handle reaching this Linux path must refuse rather than be guessed at.
fn looks_like_alsa_address(s: &str) -> bool {
    match s.split_once(':') {
        Some((client, port)) => {
            client.trim().parse::<i32>().is_ok() && port.trim().parse::<i32>().is_ok()
        }
        None => false,
    }
}

/// Reassemble ALSA's SysEx fragments into complete `F0 … F7` messages.
///
/// ALSA delivers a long SysEx as several events, and a device answering a
/// dump can easily exceed one. Everything outside `F0 … F7` is dropped: an
/// exchange is looking for a message, and a device's ordinary channel-voice
/// chatter (a knob moving while we ask) must not be mistaken for one.
#[derive(Debug, Default)]
pub(crate) struct SysexAssembler {
    partial: Vec<u8>,
}

impl SysexAssembler {
    /// Feed one decoded fragment; returns every complete message it finished.
    pub(crate) fn push(&mut self, fragment: &[u8]) -> Vec<Vec<u8>> {
        let mut done = Vec::new();
        for &b in fragment {
            if b == 0xF0 {
                // A fresh start byte abandons whatever was half-collected —
                // a truncated message is not something to salvage.
                self.partial.clear();
                self.partial.push(b);
                continue;
            }
            if self.partial.is_empty() {
                continue; // not inside a SysEx: not our traffic
            }
            self.partial.push(b);
            if b == 0xF7 {
                done.push(std::mem::take(&mut self.partial));
            }
        }
        done
    }
}

/// Does this message answer the question that was asked? An empty
/// `reply_match` accepts the first complete message — the caller said it
/// didn't care.
pub(crate) fn matches_reply(message: &[u8], reply_match: &[u8]) -> bool {
    message.starts_with(reply_match)
}

// ── the Bevy side: install the sink, keep its routes fresh ─────────────────

/// The worker's shared state: the routing table Bevy keeps current, and the
/// channel the client's [`MidiExchangeSlot`](kaijutsu_client::MidiExchangeSlot)
/// hands requests to.
#[derive(Resource)]
pub struct MidiExchange {
    /// Written by [`forward_routes`], read by the worker thread per request —
    /// so an exchange always resolves against the CURRENT picture, not the
    /// one that existed when the worker started.
    routes: Arc<RwLock<ExchangeRoutes>>,
    sender: kaijutsu_client::MidiExchangeSender,
    /// Generation of the actor whose slot we last installed into, so a
    /// reconnect/respawn re-installs exactly once.
    installed_generation: Option<u64>,
}

impl MidiExchange {
    /// The routes this sink can currently reach (for debug/UI).
    pub fn routes(&self) -> ExchangeRoutes {
        self.routes
            .read()
            .expect("exchange routes lock poisoned")
            .clone()
    }
}

pub struct MidiExchangePlugin;

impl Plugin for MidiExchangePlugin {
    fn build(&self, app: &mut App) {
        let routes: Arc<RwLock<ExchangeRoutes>> = Arc::new(RwLock::new(ExchangeRoutes::new()));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<MidiExchangeRequest>();
        spawn_exchange_thread(rx, routes.clone());
        app.insert_resource(MidiExchange {
            routes,
            sender: tx,
            installed_generation: None,
        })
        .add_systems(Update, (install_sink, forward_routes));
    }
}

/// Seat this app's exchange worker in the actor's slot, once per actor
/// generation. Until this runs the kernel is told "no sink installed" —
/// loudly, which is the correct answer for the window before the app is
/// connected.
fn install_sink(actor: Option<Res<RpcActor>>, mut exchange: ResMut<MidiExchange>) {
    let Some(actor) = actor else { return };
    if exchange.installed_generation == Some(actor.generation) {
        return;
    }
    actor.handle.midi_exchange().install(exchange.sender.clone());
    exchange.installed_generation = Some(actor.generation);
    info!("MIDI exchange: sink installed for actor generation {}", actor.generation);
}

/// Keep the worker's routing table in step with the matcher — the same source
/// and the same whole-picture-replacement discipline
/// `dj::thread::forward_midi_routes_to_dj` uses for control cues. A device
/// that stopped matching must stop being askable in the same instant: its
/// address may already belong to different gear.
fn forward_routes(
    presence: Option<Res<crate::midi_presence::MidiPresenceState>>,
    exchange: Res<MidiExchange>,
) {
    let Some(presence) = presence else { return };
    if !presence.is_changed() {
        return;
    }
    *exchange
        .routes
        .write()
        .expect("exchange routes lock poisoned") = presence.routes().clone();
}

// ── the worker thread ──────────────────────────────────────────────────────

/// Spawn the dedicated exchange thread. It owns its own ALSA seq client for
/// the life of the process and answers one request at a time.
#[cfg(target_os = "linux")]
fn spawn_exchange_thread(
    mut rx: kaijutsu_client::MidiExchangeReceiver,
    routes: Arc<RwLock<ExchangeRoutes>>,
) {
    let spawned = std::thread::Builder::new()
        .name("kaijutsu-midi-exchange".into())
        .spawn(move || {
            // Opened lazily on the FIRST request, not at startup: a machine
            // with no sequencer should not log an error just for running the
            // app, and the failure belongs in the answer to whoever asked.
            let mut client: Option<ExchangeClient> = None;
            while let Some(req) = rx.blocking_recv() {
                let answer = run_one(&mut client, &routes, &req);
                let _ = req.reply.send(answer);
            }
            info!("MIDI exchange: worker thread exiting (channel closed)");
        });
    if let Err(e) = spawned {
        error!("MIDI exchange: could not spawn the worker thread: {e}");
    }
}

#[cfg(not(target_os = "linux"))]
fn spawn_exchange_thread(
    mut rx: kaijutsu_client::MidiExchangeReceiver,
    _routes: Arc<RwLock<ExchangeRoutes>>,
) {
    // No ALSA here. Answer every request with the honest reason rather than
    // leaving the kernel to time out — a Mac sink says "not this backend",
    // and some other sink on the rig may well have the device.
    std::thread::Builder::new()
        .name("kaijutsu-midi-exchange".into())
        .spawn(move || {
            while let Some(req) = rx.blocking_recv() {
                let _ = req.reply.send(Err(
                    "MIDI exchange is Linux/ALSA-only on this build; this sink cannot \
                     run a device dialogue"
                        .to_string(),
                ));
            }
        })
        .ok();
}

/// One request, start to finish: resolve the port, open the client if needed,
/// run the dialogue. Every early return is an error string the kernel renders
/// verbatim.
#[cfg(target_os = "linux")]
fn run_one(
    client: &mut Option<ExchangeClient>,
    routes: &Arc<RwLock<ExchangeRoutes>>,
    req: &MidiExchangeRequest,
) -> Result<Vec<u8>, String> {
    let address = {
        let routes = routes.read().expect("exchange routes lock poisoned");
        resolve_exchange_address(&routes, &req.port_or_device)?
    };
    let dest = parse_alsa_addr(&address).ok_or_else(|| {
        format!("'{address}' is not an ALSA sequencer address; this sink cannot address it")
    })?;
    if client.is_none() {
        *client = Some(ExchangeClient::open()?);
    }
    let client = client.as_mut().expect("just opened");
    debug!(
        "MIDI exchange: {} byte(s) → '{}' at {address}, waiting {:?}",
        req.payload.len(),
        req.port_or_device,
        req.timeout
    );
    client.exchange(dest, &req.payload, &req.reply_match, req.timeout)
}

/// Parse an ALSA sequencer address (`"client:port"`); `None` for anything
/// else. Mirrors `dj::midi::parse_alsa_addr` — same refusal, same reason (a
/// wrong guess is a dialogue with somebody else's gear).
#[cfg(target_os = "linux")]
fn parse_alsa_addr(address: &str) -> Option<(i32, i32)> {
    let (client, port) = address.split_once(':')?;
    Some((client.trim().parse().ok()?, port.trim().parse().ok()?))
}

/// The dedicated ALSA client: a duplex port used to send a request direct and
/// to hear the reply through a temporary subscription.
#[cfg(target_os = "linux")]
struct ExchangeClient {
    seq: alsa::Seq,
    port: i32,
    client_id: i32,
}

#[cfg(target_os = "linux")]
impl ExchangeClient {
    fn open() -> Result<Self, String> {
        use alsa::seq::{PortCap, PortType};
        use std::ffi::CString;

        let map = |e: alsa::Error| format!("{e}");
        // Non-blocking: the wait is a bounded poll loop, never an unbounded
        // park — a hardware read that can't time out is exactly the hang an
        // exchange promises not to be.
        let seq = alsa::Seq::open(None, None, true).map_err(|e| {
            format!("no ALSA sequencer on this sink ({e}) — it cannot run a device dialogue")
        })?;
        seq.set_client_name(&CString::new("kaijutsu-exchange").map_err(|e| e.to_string())?)
            .map_err(map)?;
        let port = seq
            .create_simple_port(
                &CString::new("exchange").map_err(|e| e.to_string())?,
                PortCap::READ | PortCap::SUBS_READ | PortCap::WRITE | PortCap::SUBS_WRITE,
                PortType::MIDI_GENERIC | PortType::APPLICATION,
            )
            .map_err(map)?;
        let client_id = seq.client_id().map_err(map)?;
        info!("kaijutsu-exchange MIDI client open on ALSA seq {client_id}:{port}");
        Ok(Self { seq, port, client_id })
    }

    fn addr(&self) -> alsa::seq::Addr {
        alsa::seq::Addr { client: self.client_id, port: self.port }
    }

    /// Send `payload` direct to `dest` and collect the first complete SysEx
    /// starting with `reply_match`, or fail by `timeout`.
    ///
    /// The subscription to the device's port is taken for the duration and
    /// dropped afterwards, **including on every error path** — a subscription
    /// that outlived a failed exchange would leave this client quietly
    /// hearing that device forever.
    fn exchange(
        &mut self,
        dest: (i32, i32),
        payload: &[u8],
        reply_match: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        let source = alsa::seq::Addr { client: dest.0, port: dest.1 };
        let subscribed = self.subscribe_from(source);
        let result = self.exchange_subscribed(dest, payload, reply_match, timeout);
        if subscribed {
            self.unsubscribe_from(source);
        }
        result
    }

    fn exchange_subscribed(
        &mut self,
        dest: (i32, i32),
        payload: &[u8],
        reply_match: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        // Anything already queued predates the question — drain it, or a
        // stale message could be mistaken for this exchange's answer.
        self.drain_pending();
        self.send_direct(dest, payload)?;

        let decoder = alsa::seq::MidiEvent::new(4096)
            .map_err(|e| format!("MIDI decoder init failed: {e}"))?;
        decoder.enable_running_status(false);
        let mut assembler = SysexAssembler::default();
        let mut buf = [0u8; 4096];
        let deadline = std::time::Instant::now() + timeout;

        while std::time::Instant::now() < deadline {
            let mut input = self.seq.input();
            if input.event_input_pending(true).unwrap_or(0) == 0 {
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
            let Ok(ev) = input.event_input() else { continue };
            let n = match decoder.decode(&mut buf, &mut ev.into_owned()) {
                Ok(n) => n,
                Err(_) => continue, // announce chatter and friends: not MIDI bytes
            };
            for message in assembler.push(&buf[..n]) {
                if matches_reply(&message, reply_match) {
                    return Ok(message);
                }
                debug!(
                    "MIDI exchange: ignoring an unmatched {}-byte message while waiting",
                    message.len()
                );
            }
        }
        Err(format!(
            "no matching reply from {}:{} within {timeout:?} — the device may not \
             answer this dialogue at all",
            dest.0, dest.1
        ))
    }

    /// Throw away whatever is already in the input queue.
    fn drain_pending(&self) {
        let mut input = self.seq.input();
        while input.event_input_pending(true).unwrap_or(0) > 0 {
            if input.event_input().is_err() {
                break;
            }
        }
    }

    /// Emit `payload` DIRECT to `dest` — no queue (there is nothing to
    /// schedule) and no standing subscription (asking a device a question
    /// must never wire the score into it).
    fn send_direct(&mut self, dest: (i32, i32), payload: &[u8]) -> Result<(), String> {
        let mut encoder = alsa::seq::MidiEvent::new(u32::try_from(payload.len()).unwrap_or(u32::MAX).max(64))
            .map_err(|e| format!("MIDI encoder init failed: {e}"))?;
        encoder.enable_running_status(false);
        encoder.init();
        match encoder.encode(payload) {
            Ok((_, Some(mut ev))) => {
                ev.set_source(self.port);
                ev.set_dest(alsa::seq::Addr { client: dest.0, port: dest.1 });
                ev.set_direct();
                self.seq
                    .event_output(&mut ev)
                    .map_err(|e| format!("sending the request failed: {e}"))?;
                self.seq
                    .drain_output()
                    .map_err(|e| format!("flushing the request failed: {e}"))?;
                Ok(())
            }
            Ok((_, None)) => Err(
                "the request encoded to no complete MIDI message (truncated payload?)".to_string(),
            ),
            Err(e) => Err(format!("encoding the request failed: {e}")),
        }
    }

    fn subscribe_from(&self, source: alsa::seq::Addr) -> bool {
        let Ok(subs) = alsa::seq::PortSubscribe::empty() else {
            return false;
        };
        subs.set_sender(source);
        subs.set_dest(self.addr());
        match self.seq.subscribe_port(&subs) {
            Ok(()) => true,
            Err(e) => {
                // Not fatal: a port that refuses a subscription may still
                // answer if something else already routes it here. We say so
                // and press on — the timeout is the honest backstop.
                debug!(
                    "MIDI exchange: could not subscribe to {}:{} ({e}); \
                     listening anyway",
                    source.client, source.port
                );
                false
            }
        }
    }

    fn unsubscribe_from(&self, source: alsa::seq::Addr) {
        if let Err(e) = self.seq.unsubscribe_port(source, self.addr()) {
            debug!(
                "MIDI exchange: could not drop the temporary subscription to {}:{}: {e}",
                source.client, source.port
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routes(pairs: &[(&str, &[&str])]) -> ExchangeRoutes {
        pairs
            .iter()
            .map(|(d, addrs)| (d.to_string(), addrs.iter().map(|a| a.to_string()).collect()))
            .collect()
    }

    // ── addressing ────────────────────────────────────────────────────────

    /// Slice 1's rule, shared with `kj midi send`: a device resolves to its
    /// FIRST matched port (role-aware choice is slice 2).
    #[test]
    fn a_device_resolves_to_its_first_matched_port() {
        let r = routes(&[("keylab-88-mkii", &["28:0", "28:1"])]);
        assert_eq!(resolve_exchange_address(&r, "keylab-88-mkii").unwrap(), "28:0");
    }

    /// A raw address is accepted for a caller that already knows the port.
    /// The kernel never sends this form — it deals in device names.
    #[test]
    fn a_bare_alsa_address_is_addressable_without_a_route() {
        assert_eq!(resolve_exchange_address(&ExchangeRoutes::new(), "24:0").unwrap(), "24:0");
    }

    /// The rule that matters most, inherited from the control-cue path: an
    /// unroutable name REFUSES. Asking the wrong instrument is worse than
    /// asking nobody.
    #[test]
    fn an_unroutable_device_refuses_and_names_what_it_knows() {
        let r = routes(&[("minibrute", &["26:0"])]);
        let err = resolve_exchange_address(&r, "subharmonicon").unwrap_err();
        assert!(err.contains("subharmonicon"), "err: {err}");
        assert!(err.contains("minibrute"), "the error lists what IS here: {err}");
        assert!(err.contains("or nowhere"), "err: {err}");
    }

    /// A CoreMIDI-shaped handle reaching this Linux path refuses rather than
    /// being guessed at.
    #[test]
    fn a_non_alsa_handle_is_not_mistaken_for_an_address() {
        assert!(resolve_exchange_address(&ExchangeRoutes::new(), "IOService:1234").is_err());
        assert!(resolve_exchange_address(&ExchangeRoutes::new(), "24").is_err());
        assert!(resolve_exchange_address(&ExchangeRoutes::new(), "").is_err());
    }

    /// A device with a route but no ports (matched, then unplugged mid-frame)
    /// is unroutable, not a panic.
    #[test]
    fn a_device_with_no_ports_is_unroutable() {
        assert!(resolve_exchange_address(&routes(&[("d", &[])]), "d").is_err());
    }

    // ── SysEx reassembly ──────────────────────────────────────────────────

    const IDENTITY_REPLY: [u8; 15] = [
        0xF0, 0x7E, 0x00, 0x06, 0x02, 0x04, 0x05, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x02, 0xF7,
    ];

    #[test]
    fn a_single_fragment_message_comes_out_whole() {
        let mut a = SysexAssembler::default();
        assert_eq!(a.push(&IDENTITY_REPLY), vec![IDENTITY_REPLY.to_vec()]);
    }

    /// The reason this exists: ALSA hands a long SysEx over in pieces, and a
    /// per-fragment reader would parse a truncated identity.
    #[test]
    fn a_message_split_across_fragments_is_reassembled() {
        let mut a = SysexAssembler::default();
        assert!(a.push(&IDENTITY_REPLY[..4]).is_empty(), "incomplete: nothing yet");
        assert!(a.push(&IDENTITY_REPLY[4..9]).is_empty());
        assert_eq!(a.push(&IDENTITY_REPLY[9..]), vec![IDENTITY_REPLY.to_vec()]);
    }

    #[test]
    fn two_messages_in_one_fragment_both_come_out_in_order() {
        let mut a = SysexAssembler::default();
        let mut both = IDENTITY_REPLY.to_vec();
        both.extend_from_slice(&[0xF0, 0x7E, 0x01, 0xF7]);
        assert_eq!(
            a.push(&both),
            vec![IDENTITY_REPLY.to_vec(), vec![0xF0, 0x7E, 0x01, 0xF7]]
        );
    }

    /// Ordinary playing while we wait (a knob, a note) is not an answer and
    /// must never be handed back as one.
    #[test]
    fn channel_voice_traffic_between_messages_is_ignored() {
        let mut a = SysexAssembler::default();
        assert!(a.push(&[0x90, 60, 100, 0x80, 60, 0]).is_empty());
        assert_eq!(a.push(&IDENTITY_REPLY), vec![IDENTITY_REPLY.to_vec()]);
    }

    /// A dropped fragment leaves a half-message; the next start byte abandons
    /// it rather than splicing two devices' bytes into one fiction.
    #[test]
    fn a_truncated_message_is_abandoned_at_the_next_start_byte() {
        let mut a = SysexAssembler::default();
        assert!(a.push(&[0xF0, 0x7E, 0x00, 0x06]).is_empty());
        assert_eq!(
            a.push(&IDENTITY_REPLY),
            vec![IDENTITY_REPLY.to_vec()],
            "the salvage-free restart yields exactly the good message"
        );
    }

    // ── reply matching ────────────────────────────────────────────────────

    #[test]
    fn a_reply_matches_on_its_leading_bytes() {
        assert!(matches_reply(&IDENTITY_REPLY, &[0xF0, 0x7E]));
        assert!(!matches_reply(&IDENTITY_REPLY, &[0xF0, 0x7D]));
        assert!(
            matches_reply(&IDENTITY_REPLY, &[]),
            "an empty filter takes the first complete message"
        );
    }

    // ── the Bevy wiring ───────────────────────────────────────────────────

    /// The plugin owns its worker from `build`, so the resource (and the
    /// sink channel behind it) exists before any connection does — an
    /// exchange that arrives on the first frame has somewhere to go.
    #[test]
    fn the_plugin_installs_the_exchange_resource() {
        let mut app = App::new();
        app.add_plugins(MidiExchangePlugin);
        assert!(app.world().get_resource::<MidiExchange>().is_some());
        assert!(
            app.world().resource::<MidiExchange>().routes().is_empty(),
            "a fresh sink can reach nothing until the matcher says otherwise"
        );
    }

    /// Routes reach the worker through the shared table, whole-picture at a
    /// time — a device that stops matching stops being askable in the same
    /// instant (its address may already belong to different gear).
    #[test]
    fn matcher_routes_reach_the_worker_and_replace_wholesale() {
        let mut app = App::new();
        app.add_plugins(MidiExchangePlugin)
            .init_resource::<crate::midi_presence::MidiPresenceState>();

        // Nothing matched yet.
        app.update();
        assert!(app.world().resource::<MidiExchange>().routes().is_empty());

        // The matcher's picture is what the worker resolves against; write it
        // the way `reconcile_presence` does and let the system carry it over.
        app.world_mut()
            .resource_mut::<crate::midi_presence::MidiPresenceState>()
            .set_routes_for_test(routes(&[("keystep-pro", &["24:0"])]));
        app.update();
        assert_eq!(
            app.world().resource::<MidiExchange>().routes()["keystep-pro"],
            vec!["24:0".to_string()]
        );

        // Unplug: the whole table is replaced, not merged.
        app.world_mut()
            .resource_mut::<crate::midi_presence::MidiPresenceState>()
            .set_routes_for_test(ExchangeRoutes::new());
        app.update();
        assert!(
            app.world().resource::<MidiExchange>().routes().is_empty(),
            "a device that stopped matching must stop being askable"
        );
    }

    // ── live ALSA (needs /dev/snd/seq) ────────────────────────────────────

    /// The platform assumption slice 1 step 5 rests on, proven end to end:
    /// **a direct-addressed request reaches a port, and the answer comes back
    /// through a temporary subscription on this client alone.** A fake device
    /// (a second seq client) answers the universal Identity Request.
    ///
    /// `#[ignore]`d like its `dj::midi` siblings; run with `--ignored` on a
    /// box with a sequencer (moltar/zorak).
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a live ALSA sequencer (/dev/snd/seq); run on the moltar/zorak runner"]
    fn a_live_exchange_reaches_a_fake_device_and_collects_its_reply() {
        use alsa::seq::{Addr, PortCap, PortType};
        use std::ffi::CString;

        // A stand-in "device": its own client with a duplex port that replies
        // to any complete SysEx it receives.
        let device = alsa::Seq::open(None, None, true).expect("open fake device");
        device
            .set_client_name(&CString::new("kj-exchange-test-device").unwrap())
            .unwrap();
        let device_port = device
            .create_simple_port(
                &CString::new("io").unwrap(),
                PortCap::READ | PortCap::SUBS_READ | PortCap::WRITE | PortCap::SUBS_WRITE,
                PortType::MIDI_GENERIC | PortType::APPLICATION,
            )
            .unwrap();
        let device_addr = (device.client_id().unwrap(), device_port);

        let answered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = answered.clone();
        let responder = std::thread::spawn(move || {
            let decoder = alsa::seq::MidiEvent::new(4096).unwrap();
            decoder.enable_running_status(false);
            let mut buf = [0u8; 4096];
            let mut asm = SysexAssembler::default();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                let mut input = device.input();
                if input.event_input_pending(true).unwrap_or(0) == 0 {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                let Ok(ev) = input.event_input() else { continue };
                let Ok(n) = decoder.decode(&mut buf, &mut ev.into_owned()) else {
                    continue;
                };
                let got = asm.push(&buf[..n]);
                if got.is_empty() {
                    continue;
                }
                // Answer whoever is subscribed to us — the exchange client
                // took a temporary subscription before asking, which is
                // exactly what this test proves is sufficient.
                let mut enc = alsa::seq::MidiEvent::new(64).unwrap();
                enc.enable_running_status(false);
                enc.init();
                if let Ok((_, Some(mut reply))) = enc.encode(&IDENTITY_REPLY) {
                    reply.set_source(device_port);
                    reply.set_subs();
                    reply.set_direct();
                    let _ = device.event_output(&mut reply);
                    let _ = device.drain_output();
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                break;
            }
        });

        // Give the fake device a moment to be listening.
        std::thread::sleep(Duration::from_millis(50));

        let mut client = ExchangeClient::open().expect("open exchange client");
        let reply = client.exchange(
            device_addr,
            &[0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7],
            &[0xF0, 0x7E],
            Duration::from_secs(3),
        );
        responder.join().unwrap();
        assert!(
            answered.load(std::sync::atomic::Ordering::SeqCst),
            "the fake device never saw the direct-addressed request"
        );
        assert_eq!(reply.expect("a reply"), IDENTITY_REPLY.to_vec());

        // The temporary subscription must be gone: this client hears nothing
        // from the device once the exchange is over. (Unsubscribing an
        // already-unsubscribed pair errors, which is the assertion.)
        let src = Addr { client: device_addr.0, port: device_addr.1 };
        assert!(
            client.seq.unsubscribe_port(src, client.addr()).is_err(),
            "the exchange must not leave a standing subscription behind"
        );
    }

    /// A device that never answers must TIME OUT — bounded, loud, and with
    /// the port named. This is the "unplugged = error, never a hang" rule at
    /// the hardware edge.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a live ALSA sequencer (/dev/snd/seq); run on the moltar/zorak runner"]
    fn a_live_exchange_with_a_silent_device_times_out_loudly() {
        use alsa::seq::{PortCap, PortType};
        use std::ffi::CString;

        let silent = alsa::Seq::open(None, None, true).expect("open silent device");
        silent
            .set_client_name(&CString::new("kj-exchange-test-silent").unwrap())
            .unwrap();
        let port = silent
            .create_simple_port(
                &CString::new("io").unwrap(),
                PortCap::READ | PortCap::SUBS_READ | PortCap::WRITE | PortCap::SUBS_WRITE,
                PortType::MIDI_GENERIC | PortType::APPLICATION,
            )
            .unwrap();
        let addr = (silent.client_id().unwrap(), port);

        let mut client = ExchangeClient::open().expect("open exchange client");
        let started = std::time::Instant::now();
        let err = client
            .exchange(
                addr,
                &[0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7],
                &[0xF0, 0x7E],
                Duration::from_millis(300),
            )
            .unwrap_err();
        assert!(err.contains("no matching reply"), "err: {err}");
        assert!(started.elapsed() < Duration::from_secs(2), "it must be BOUNDED");
    }
}
