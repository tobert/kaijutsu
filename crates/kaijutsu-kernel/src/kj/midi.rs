//! `kj midi` — read the CRDT-owned MIDI device profile library.
//!
//! Device profiles live at `/etc/midi/devices/<name>` on the same CRDT-native
//! backend that owns `/etc/rc` and `/etc/config`
//! (`docs/config-crdt-ownership.md`): the kernel is the sole owner, no host
//! file, no write-through. Embedded seeds (`assets/defaults/midi/devices/*.md`,
//! `crate::midi_seed`) bootstrap a fresh kernel once — see `docs/midi-next.md`
//! "Storage and identity" (slice 1 step 2).
//!
//! `list` enumerates the devices tree, `show` prints one profile document,
//! `send`/`panic` emit raw control cues at a named device (slice 1 step 4,
//! below). Profile-resolved control *names* (`kj midi cc subh vco1-level
//! -25%`), `identify`, and `pull` are later slices in `docs/midi-next.md` —
//! this noun exists so building them doesn't fragment `kj` discovery later
//! (the `kj audio`/`kj transport` precedent this file follows).
//!
//! ## Emit (slice 1 step 4): `send` and `panic`
//!
//! **The kernel never touches hardware** (`docs/midi.md` "Render is a wire
//! cue; the sink owns the hardware"). `kj midi send` composes MIDI bytes,
//! wraps them in a [`MidiControl`] envelope naming the *device*, and
//! publishes a play-now [`RenderCue`] (`lead == 0`) on the SAME FlowBus
//! `kj play` uses — same path, different payload. Every attached sink
//! receives it; the one that has that device matched resolves the name to a
//! real port and emits. This is what retires the whipped-up-python pattern:
//! a kaish script anywhere on the rig can play gear attached to a laptop,
//! with zero ALSA in the script.
//!
//! Three deliberate stances:
//!
//! - **The profile is the gate; presence is not.** An unknown `<device>` (no
//!   `/etc/midi/devices/<name>`) is a loud error — there is nothing to route
//!   and no honest guess. A device the presence store says is *absent* or
//!   *unknown* is still sent, with the warning in the result: presence is a
//!   sink's report, a sink may have connected since, and the kernel does not
//!   gate players (`CLAUDE.md` "crosstalk-as-feature"). The sink drops what
//!   it can't route, loudly, in its own logs.
//! - **Channels are 1–16 at the surface**, `channel − 1` on the wire — the
//!   profile documents' convention, and what a device's front panel says.
//! - **Raw only.** `send` takes numbers a human or a manual already decided
//!   on. No profile-resolved names, no relative values, no `/run/midi`
//!   provenance write — all slice 2/3.
//!
//! ## Presence (slice 1 step 3)
//!
//! Both verbs carry the sink-fed presence column
//! (`docs/midi-next.md` "Presence is sink-fed"). Three states, and the third
//! is load-bearing:
//!
//! - **live** — some sink reported this profile matched a plugged-in device.
//! - **absent** — some sink reported it *gone* (an unplug is a first-class
//!   report, not an omission).
//! - **unknown** — nobody has told this kernel anything. A fresh kernel says
//!   this about everything, because presence is ephemeral: a restart with no
//!   sinks connected genuinely knows nothing. Stale presence that lies is
//!   worse than no presence, so we never soften unknown into absent.
//!
//! The column reads [`crate::midi_presence::MidiPresenceStore`] — the same
//! state the read-only `/run/midi/<device>` view renders for kai and kaish.
//! Devices with no profile are never invented: an unmatched port simply has
//! no presence entry (the ear still captures from it regardless).

use clap::{Parser, Subcommand};
use kaijutsu_audio::{CuePayload, MIDI_CONTROL_MIME, MidiControl, MidiControlEvent, RenderCue};
use kaijutsu_types::ContentType;
use kaijutsu_types::paths::{MIDI_ROOT, midi_device_path};

use super::refs;
use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};
use crate::flows::BlockFlow;

#[derive(Parser, Debug)]
#[command(
    name = "midi",
    about = "MIDI device profiles (/etc/midi/devices/<name>) and raw emit at a named device",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct MidiArgs {
    #[command(subcommand)]
    command: MidiCommand,
}

#[derive(Subcommand, Debug)]
enum MidiCommand {
    /// List known device profiles (name + title pulled from the doc).
    #[command(alias = "ls")]
    List {
        /// Emit a JSON array of {name, title} objects instead of a labelled view
        #[arg(long)]
        json: bool,
    },
    /// Print one device's profile document.
    #[command(alias = "cat")]
    Show {
        /// Device name (e.g. minibrute) or full /etc/midi/devices path
        name: String,
        /// Emit a JSON object instead of a labelled view
        #[arg(long)]
        json: bool,
        /// Emit exactly the stored document — no path/length header
        #[arg(long, conflicts_with = "json")]
        raw: bool,
    },
    /// Emit raw MIDI at a named device (the sink resolves the port).
    Send {
        /// Device name — a profile key under /etc/midi/devices
        device: String,
        #[command(subcommand)]
        message: SendMessage,
        /// Target context: . (default) | <label> | <hex prefix>. Mirrors
        /// `kj play`: every attached sink receives the cue regardless.
        #[arg(long, short = 'c', global = true)]
        context: Option<String>,
    },
    /// All-notes-off + all-sound-off on all 16 channels of <device>, or of
    /// every device the rig currently reports live when omitted.
    Panic {
        /// Device name; omitted = every live device
        device: Option<String>,
        /// Target context: . (default) | <label> | <hex prefix>
        #[arg(long, short = 'c')]
        context: Option<String>,
    },
}

/// The raw message grammar of `kj midi send <device> …`. Every value is a
/// literal a human read off a manual or a front panel — profile-resolved
/// names are slice 2 (`docs/midi-next.md`).
#[derive(Subcommand, Debug)]
enum SendMessage {
    /// Note On, optionally gated by a scheduled Note Off.
    Note {
        /// MIDI channel, 1-16 (front-panel numbering)
        channel: u8,
        /// Note number, 0-127 (60 = middle C)
        note: u8,
        /// Velocity, 0-127
        velocity: u8,
        /// Also send the matching Note Off this many ms later. Omitted = a
        /// bare Note On that sustains until something else stops it (drone,
        /// or a later `kj midi panic`).
        #[arg(long)]
        off_ms: Option<u64>,
    },
    /// Control Change.
    Cc {
        /// MIDI channel, 1-16
        channel: u8,
        /// Controller number, 0-127
        controller: u8,
        /// Value, 0-127
        value: u8,
    },
    /// Program Change.
    Pc {
        /// MIDI channel, 1-16
        channel: u8,
        /// Program number, 0-127 (raw MIDI numbering — gear that prints
        /// 1-128 on its panel is one higher than this)
        program: u8,
    },
    /// Fire-and-forget System Exclusive bytes — the day-one escape hatch for
    /// everything the other verbs don't cover (`docs/midi-next.md` "SysEx:
    /// the exchange pattern"). No reply is collected; that's `kj midi
    /// identify`/`pull`, a later slice.
    Sysex {
        /// Hex bytes (`f07e7f0601f7`; spaces, commas and 0x prefixes are
        /// tolerated) or `@<path>` to read a raw `.syx` file. Multiple
        /// concatenated F0…F7 messages are split and sent in order.
        data: String,
    },
}

// ── message composition (pure) ───────────────────────────────────────────────

/// Surface channel (1-16, what the front panel says) → wire nibble (0-15).
/// A loud error rather than a silent clamp: channel 0 is a typo for 1 often
/// enough that guessing would send a whole phrase to the wrong instrument.
fn wire_channel(channel: u8) -> Result<u8, String> {
    match channel {
        1..=16 => Ok(channel - 1),
        _ => Err(format!(
            "channel {channel} out of range — MIDI channels are 1-16 at this surface \
             (the wire byte is channel-1)"
        )),
    }
}

/// A 7-bit MIDI data byte. Anything ≥ 0x80 IS a status byte on the wire, so an
/// out-of-range value doesn't merely mis-sound — it injects a rogue message
/// into the stream. Refuse, name the field.
fn data_byte(name: &str, value: u8) -> Result<u8, String> {
    match value {
        0..=127 => Ok(value),
        _ => Err(format!("{name} {value} out of range (0-127)")),
    }
}

/// Compose one `send` message into wire events. `Note` yields two events when
/// gated (the Off carries the offset — one cue, not two, so a dropped second
/// cue can never strand a sounding note).
fn compose_send(message: &SendMessage) -> Result<Vec<MidiControlEvent>, String> {
    Ok(match message {
        SendMessage::Note { channel, note, velocity, off_ms } => {
            let ch = wire_channel(*channel)?;
            let note = data_byte("note", *note)?;
            let velocity = data_byte("velocity", *velocity)?;
            let mut events = vec![MidiControlEvent::now(&[0x90 | ch, note, velocity])];
            if let Some(ms) = off_ms {
                events.push(MidiControlEvent::new(*ms, &[0x80 | ch, note, 0]));
            }
            events
        }
        SendMessage::Cc { channel, controller, value } => {
            let ch = wire_channel(*channel)?;
            let controller = data_byte("controller", *controller)?;
            let value = data_byte("value", *value)?;
            vec![MidiControlEvent::now(&[0xB0 | ch, controller, value])]
        }
        SendMessage::Pc { channel, program } => {
            let ch = wire_channel(*channel)?;
            let program = data_byte("program", *program)?;
            vec![MidiControlEvent::now(&[0xC0 | ch, program])]
        }
        SendMessage::Sysex { data } => sysex_messages(data)?
            .iter()
            .map(|m| MidiControlEvent::now(m))
            .collect(),
    })
}

/// A human-readable one-liner for what a `send` is about to do — the result
/// message's subject. Built from the parsed args, not from the composed
/// bytes, so it reads the way the operator typed it.
fn describe_send(message: &SendMessage) -> String {
    match message {
        SendMessage::Note { channel, note, velocity, off_ms } => match off_ms {
            Some(ms) => format!("note {note} vel {velocity} ch {channel} ({ms}ms gate)"),
            None => format!("note {note} vel {velocity} ch {channel} (no note-off)"),
        },
        SendMessage::Cc { channel, controller, value } => {
            format!("cc {controller} = {value} ch {channel}")
        }
        SendMessage::Pc { channel, program } => format!("program change {program} ch {channel}"),
        SendMessage::Sysex { data } => match data.strip_prefix('@') {
            Some(path) => format!("sysex from {path}"),
            None => format!("sysex ({} hex chars)", data.len()),
        },
    }
}

/// The panic sequence for one device: **all-notes-off (CC 123) then
/// all-sound-off (CC 120) on every one of the 16 channels**. Both, in that
/// order, deliberately: 123 releases held notes so envelopes finish naturally
/// (the musical stop), 120 then kills anything still sounding (the honest
/// stop, for gear that ignores 123 or is stuck mid-release). 32 messages, all
/// at offset 0.
fn panic_messages() -> Vec<MidiControlEvent> {
    let mut events = Vec::with_capacity(32);
    for ch in 0..16u8 {
        events.push(MidiControlEvent::now(&[0xB0 | ch, 123, 0]));
        events.push(MidiControlEvent::now(&[0xB0 | ch, 120, 0]));
    }
    events
}

/// Parse the `sysex` argument into complete F0…F7 messages.
///
/// Two forms, distinguished at the grammar (never sniffed): `@<path>` reads a
/// file as **raw bytes** (the universal `.syx` format), anything else is hex
/// text with human separators tolerated (`"F0 7E 7F 06 01 F7"`, `0xF0,0x7E`).
/// The byte stream is then split into individual messages and validated —
/// leading F0, terminating F7, and 7-bit payload throughout. A malformed
/// SysEx is refused here rather than shipped: a truncated dialogue can leave
/// real gear waiting mid-transfer, which is worse than not sending.
fn sysex_messages(spec: &str) -> Result<Vec<Vec<u8>>, String> {
    let bytes = match spec.strip_prefix('@') {
        Some(path) => std::fs::read(path).map_err(|e| format!("sysex file '{path}': {e}"))?,
        None => {
            let normalized: String = spec
                .replace("0x", "")
                .replace("0X", "")
                .chars()
                .filter(|c| !c.is_whitespace() && *c != ',' && *c != ':')
                .collect();
            kaijutsu_audio::midi_control::from_hex(&normalized)
                .map_err(|e| format!("sysex hex: {e}"))?
        }
    };
    split_sysex(&bytes)
}

/// Split a raw byte stream into complete SysEx messages, refusing anything
/// malformed. Separate from [`sysex_messages`] so the validation is testable
/// without touching the filesystem.
fn split_sysex(bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    if bytes.is_empty() {
        return Err("sysex payload is empty".to_string());
    }
    let mut messages = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match (b, current.as_mut()) {
            (0xF0, None) => current = Some(vec![0xF0]),
            (0xF0, Some(_)) => {
                return Err(format!("sysex: unterminated message — a second F0 at byte {i}"));
            }
            (_, None) => {
                return Err(format!(
                    "sysex: byte {i} is 0x{b:02x}, but a SysEx message must start with F0"
                ));
            }
            (0xF7, Some(msg)) => {
                msg.push(0xF7);
                messages.push(current.take().expect("just matched Some"));
            }
            (b, Some(_)) if b >= 0x80 => {
                return Err(format!(
                    "sysex: byte {i} is 0x{b:02x} — SysEx payload bytes must be 7-bit (00-7F)"
                ));
            }
            (b, Some(msg)) => msg.push(b),
        }
    }
    if current.is_some() {
        return Err("sysex: unterminated message — the stream never reached F7".to_string());
    }
    Ok(messages)
}

/// The shared result shape for `send`/`panic`: what went where, how many
/// sinks heard it, and every warning that didn't stop it.
///
/// **Zero listeners is itself a warning.** A kernel with no sink attached
/// publishes happily into nothing, and a player who just turned a knob with
/// no sound deserves to be told that up front rather than debugging their
/// cable. It is not an error — the cue is genuinely emitted, and a sink can
/// be attached next second (it just won't get *this* cue: control cues are
/// fire-and-forget, never replayed).
fn emit_result(
    device: &str,
    subject: &str,
    messages: usize,
    receivers: usize,
    mut warnings: Vec<String>,
) -> KjResult {
    if receivers == 0 {
        warnings.push(
            "no render sink is attached to this kernel — the cue was published to 0 listeners, \
             so nothing will sound"
                .to_string(),
        );
    }
    let data = serde_json::json!({
        "device": device,
        "subject": subject,
        "messages": messages,
        "receivers": receivers,
        "warnings": warnings,
    });
    let mut message = format!("{device}: {subject} — {messages} message(s), {receivers} sink(s)");
    for w in &warnings {
        message.push_str("\nwarning: ");
        message.push_str(w);
    }
    // NOT ephemeral, unlike `kj play`'s "playing …": the `kj midi` verbs ARE
    // a device context's tools (`docs/midi-next.md` "one emit surface"), so
    // the model turning the knob has to see whether its send actually landed
    // — an ephemeral warning is one no model ever reads.
    KjResult::ok_typed_with_data(message, ContentType::Plain, data)
}

/// The `/etc/midi/devices` directory path.
fn devices_dir() -> String {
    format!("{MIDI_ROOT}/devices")
}

/// Canonicalize a user-supplied device arg to `/etc/midi/devices/<name>`.
/// Accepts a bare name (`minibrute`) or an already-full path. Rejects nested
/// paths and parent escapes — the devices namespace is flat, one document per
/// device (a future rc-style bucket widens the *reader*, not this grammar,
/// since a bucket's files still hang directly under `/etc/midi/devices/<name>/`,
/// not under a per-device leaf this canonicalizer would need to parse).
fn midi_device_canonical(name: &str) -> Result<String, String> {
    let dir = devices_dir();
    let trimmed = name.trim();
    let bare = trimmed
        .strip_prefix(&format!("{dir}/"))
        .unwrap_or(trimmed)
        .trim_matches('/');
    if bare.is_empty() {
        return Err("missing device name (e.g. minibrute)".to_string());
    }
    if bare.contains('/') || bare == ".." || bare == "." {
        return Err(format!(
            "invalid device name '{name}': {dir} is a flat namespace (one document per device)"
        ));
    }
    Ok(midi_device_path(bare))
}

/// The three presence states, rendered. `unknown` is never softened into
/// `absent`: "nobody told us" and "a sink watched it leave" are different
/// facts, and collapsing them would make a restarted kernel lie.
fn presence_label(record: Option<&crate::midi_presence::MidiPresenceRecord>) -> &'static str {
    match record {
        Some(r) if r.present => "live",
        Some(_) => "absent",
        None => "unknown",
    }
}

/// Placeholder title rendered for a directory entry under
/// `/etc/midi/devices` — the shape a future rc-style bucket device will take
/// (`docs/midi-next.md` "The core split"). Today's reader only understands a
/// single-file leaf profile; a bucket directory must render a visible row
/// instead of silently vanishing from an `is_file()` filter, so a future
/// bucket device is a "not built yet" row, never an invisible one.
const BUCKET_PLACEHOLDER_TITLE: &str = "(bucket device — kj midi bucket support not built yet)";

/// The device's display title: its first non-empty line, with a leading
/// markdown `#`/`##`/etc. stripped (every shipped profile opens with a `# …
/// device profile` heading — `docs/midi-next.md`'s prose+JSON hybrid). Falls
/// back to `(untitled)` rather than an empty string so `list` never silently
/// drops a row for a malformed document.
fn doc_title(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(untitled)".to_string())
}

impl KjDispatcher {
    pub(crate) async fn dispatch_midi(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<MidiArgs>();
        }
        let parsed = match MidiArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj midi: {e}"));
            }
        };
        match parsed.command {
            MidiCommand::List { json } => self.midi_list(json).await,
            MidiCommand::Show { name, json, raw } => self.midi_show(&name, json, raw).await,
            MidiCommand::Send { device, message, context } => {
                self.midi_send(&device, &message, context.as_deref(), caller).await
            }
            MidiCommand::Panic { device, context } => {
                self.midi_panic(device.as_deref(), context.as_deref(), caller).await
            }
        }
    }

    /// A `<device>` must name a real profile before anything is emitted at
    /// it. There is no honest fallback: an unknown name has no port to
    /// resolve at any sink, so guessing would mean either silence dressed as
    /// success, or bytes at the wrong instrument.
    async fn midi_device_must_exist(&self, verb: &str, name: &str) -> Result<String, String> {
        use crate::vfs::{VfsError, VfsOps};
        let canonical = midi_device_canonical(name).map_err(|e| format!("kj midi {verb}: {e}"))?;
        match self.kernel().vfs().read_all(std::path::Path::new(&canonical)).await {
            Ok(_) => Ok(canonical),
            Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => Err(format!(
                "kj midi {verb}: unknown device '{name}' (no profile at {canonical}) \
                 — `kj midi list` shows what this kernel knows"
            )),
            Err(e) => Err(format!("kj midi {verb}: '{canonical}': {e}")),
        }
    }

    /// What the presence store has to say about `device`, as a warning — or
    /// `None` when it is live. **Never a gate.** A device the rig hasn't
    /// reported may well be plugged into a sink that connected a moment ago,
    /// and the kernel doesn't referee between players
    /// (`CLAUDE.md` "crosstalk-as-feature"). The cue goes out either way; the
    /// sink drops what it can't route, loudly, in its own logs.
    fn presence_warning(&self, device: &str) -> Option<String> {
        match self.kernel().midi_presence().get(device) {
            Some(r) if r.present => None,
            Some(_) => Some(format!(
                "'{device}' was last reported ABSENT — sending anyway; \
                 a sink that can't route it will drop the cue"
            )),
            None => Some(format!(
                "presence of '{device}' is UNKNOWN to this kernel (no sink has reported it) \
                 — sending anyway; a sink that can't route it will drop the cue"
            )),
        }
    }

    /// Wrap composed messages in a [`MidiControl`] envelope and push it down
    /// the existing render-cue wire as a play-now [`RenderCue`] — the same
    /// FlowBus, the same bridge, the same sinks as `kj play`. Returns the
    /// listener count so the caller can tell a player "0 listeners" instead of
    /// letting a sinkless rig look like a successful send.
    fn publish_control(
        &self,
        context_id: kaijutsu_types::ContextId,
        device: &str,
        events: Vec<MidiControlEvent>,
    ) -> usize {
        let envelope = MidiControl::new(device, events);
        let cue = RenderCue {
            mime: MIDI_CONTROL_MIME.to_string(),
            payload: CuePayload::Inline(envelope.to_bytes()),
            // Onset now: a control send has no musical placement to hit, and
            // a lead would only add latency between a knob-turn and the gear.
            lead: std::time::Duration::ZERO,
            // Unstamped, like `kj play`'s play-now: there is no phrase
            // boundary to back-date against, and the sink must never DROP a
            // panic for staleness.
            epoch_ns: 0,
            // No track/beat association — dispatches immediately regardless
            // of a sink's clock mode.
            onset_beat: None,
        };
        self.kernel()
            .block_flows()
            .publish(BlockFlow::RenderCue { context_id, cue })
    }

    /// `kj midi send <device> note|cc|pc|sysex …`
    async fn midi_send(
        &self,
        device: &str,
        message: &SendMessage,
        context: Option<&str>,
        caller: &KjCaller,
    ) -> KjResult {
        if let Err(e) = self.midi_device_must_exist("send", device).await {
            return KjResult::Err(e);
        }
        let events = match compose_send(message) {
            Ok(e) => e,
            Err(e) => return KjResult::Err(format!("kj midi send: {e}")),
        };
        let context_id = {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(context, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj midi send: {e}")),
            }
        };
        let warning = self.presence_warning(device);
        let count = events.len();
        let receivers = self.publish_control(context_id, device, events);
        let subject = describe_send(message);
        emit_result(device, &subject, count, receivers, warning.into_iter().collect())
    }

    /// `kj midi panic [device]` — all-notes-off + all-sound-off on every
    /// channel. With no `<device>`, every device the rig currently reports
    /// **live** gets its own cue: panic is the "stop everything" reflex, so
    /// it fans out rather than making a player name the offender.
    async fn midi_panic(
        &self,
        device: Option<&str>,
        context: Option<&str>,
        caller: &KjCaller,
    ) -> KjResult {
        let devices: Vec<String> = match device {
            Some(name) => {
                if let Err(e) = self.midi_device_must_exist("panic", name).await {
                    return KjResult::Err(e);
                }
                vec![name.to_string()]
            }
            None => {
                let live: Vec<String> = self
                    .kernel()
                    .midi_presence()
                    .snapshot()
                    .into_iter()
                    .filter(|r| r.present)
                    .map(|r| r.device)
                    .collect();
                if live.is_empty() {
                    // Loud, not a cheerful no-op: the player asked for
                    // everything to stop and nothing was told to. Naming a
                    // device explicitly still works (presence never gates).
                    return KjResult::Err(
                        "kj midi panic: no device is reported live on this rig — \
                         name one explicitly (`kj midi panic <device>`) to send anyway, \
                         or check `kj midi list`"
                            .to_string(),
                    );
                }
                live
            }
        };
        let context_id = {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(context, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj midi panic: {e}")),
            }
        };

        let mut warnings = Vec::new();
        let mut receivers = 0usize;
        let count = panic_messages().len();
        for name in &devices {
            warnings.extend(self.presence_warning(name));
            receivers = receivers.max(self.publish_control(context_id, name, panic_messages()));
        }
        emit_result(
            &devices.join(", "),
            "panic (all-notes-off + all-sound-off, all 16 channels)",
            count * devices.len(),
            receivers,
            warnings,
        )
    }

    async fn midi_list(&self, json: bool) -> KjResult {
        use crate::vfs::{VfsError, VfsOps};
        let vfs = self.kernel().vfs();
        let dir = devices_dir();
        let entries = match vfs.readdir(std::path::Path::new(&dir)).await {
            Ok(e) => e,
            // Absent (no mount, nothing seeded yet) reads as an empty listing,
            // not an error — a kernel that never mounted /etc/midi still
            // answers `kj midi list` truthfully with nothing.
            Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => Vec::new(),
            Err(e) => return KjResult::Err(format!("kj midi list: readdir {dir}: {e}")),
        };

        let presence = self.kernel().midi_presence();
        // (name, title, presence label, record, kind) — presence is a
        // *separate* lookup per device, never derived from the document: the
        // profile is durable knowledge, presence is a live report about it.
        // `kind` distinguishes a real single-file profile ("profile") from a
        // directory entry this reader can't parse yet ("bucket") — a future
        // rc-style bucket device must show up as a visible, labelled row, not
        // vanish behind a file-only filter.
        let mut rows: Vec<(String, String, &'static str, Option<_>, &'static str)> = Vec::new();
        for entry in entries {
            use crate::vfs::FileType;
            let (title, kind) = match entry.kind {
                FileType::File => {
                    let path = midi_device_path(&entry.name);
                    let title = match vfs.read_all(std::path::Path::new(&path)).await {
                        Ok(bytes) => match String::from_utf8(bytes) {
                            Ok(s) => doc_title(&s),
                            Err(e) => {
                                return KjResult::Err(format!(
                                    "kj midi list: '{path}' is not valid UTF-8: {e}"
                                ));
                            }
                        },
                        Err(e) => return KjResult::Err(format!("kj midi list: read {path}: {e}")),
                    };
                    (title, "profile")
                }
                FileType::Directory => (BUCKET_PLACEHOLDER_TITLE.to_string(), "bucket"),
                // Not a shape any current or planned seed uses under this
                // tree; skip rather than guess at a title.
                FileType::Symlink => continue,
            };
            let record = presence.get(&entry.name);
            let label = presence_label(record.as_ref());
            rows.push((entry.name, title, label, record, kind));
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));

        let data = serde_json::Value::Array(
            rows.iter()
                .map(|(name, title, label, record, kind)| {
                    serde_json::json!({
                        "name": name,
                        "title": title,
                        "presence": label,
                        // Null rather than 0 for unknown: a missing observation
                        // has no timestamp, and an invented one would read as
                        // "observed at the epoch".
                        "at": record.as_ref().map(|r| r.at_ns),
                        "backend": record.as_ref().map(|r| r.backend.clone()),
                        // WHERE the report came from — the whole point of a
                        // rig-wide store. Null for unknown (nobody reported)
                        // and for a sink too old to say; never a placeholder
                        // hostname, which would read as a real machine.
                        "host": record
                            .as_ref()
                            .map(|r| r.sink.host.clone())
                            .filter(|h| !h.is_empty()),
                        "kind": kind,
                    })
                })
                .collect(),
        );
        if json {
            return KjResult::ok_with_data(data.to_string(), data);
        }
        if rows.is_empty() {
            return KjResult::ok_with_data("(no device profiles)".to_string(), data);
        }
        let width = rows.iter().map(|(n, ..)| n.len()).max().unwrap_or(0);
        let pwidth = rows.iter().map(|(_, _, p, ..)| p.len()).max().unwrap_or(0);
        // The "where" column, and only when somebody actually answered it: on
        // a rig nobody has reported into (or one whose sinks are too old to
        // name themselves) the column is pure blank padding, so it isn't
        // drawn at all.
        let host_of = |record: &Option<crate::midi_presence::MidiPresenceRecord>| -> String {
            record
                .as_ref()
                .map(|r| r.sink.host.clone())
                .filter(|h| !h.is_empty())
                .unwrap_or_default()
        };
        let hwidth = rows
            .iter()
            .map(|(_, _, _, record, _)| host_of(record).len())
            .max()
            .unwrap_or(0);
        let lines: Vec<String> = rows
            .iter()
            .map(|(name, title, label, record, _)| {
                if hwidth == 0 {
                    return format!("  {name:<width$}  {label:<pwidth$}  {title}");
                }
                let host = host_of(record);
                format!("  {name:<width$}  {label:<pwidth$}  {host:<hwidth$}  {title}")
            })
            .collect();
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    async fn midi_show(&self, name: &str, json: bool, raw: bool) -> KjResult {
        use crate::vfs::{VfsError, VfsOps};
        let canonical = match midi_device_canonical(name) {
            Ok(c) => c,
            Err(e) => return KjResult::Err(format!("kj midi show: {e}")),
        };
        let vfs = self.kernel().vfs();
        let content = match vfs.read_all(std::path::Path::new(&canonical)).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(e) => {
                    return KjResult::Err(format!(
                        "kj midi show: '{canonical}' is not valid UTF-8: {e}"
                    ));
                }
            },
            Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => {
                return KjResult::Err(format!(
                    "kj midi show: unknown device '{name}' (no profile at {canonical})"
                ));
            }
            Err(e) => return KjResult::Err(format!("kj midi show: '{canonical}': {e}")),
        };

        if raw {
            // Exactly the stored document — no header — so it round-trips
            // through a future `kj midi set`/`edit` the way `kj config show
            // --raw` does today.
            return KjResult::ok(content);
        }

        let bare = canonical.rsplit('/').next().unwrap_or(&canonical);
        // Presence rides alongside the document, never inside it: the profile
        // is durable knowledge, the presence record is an ephemeral live
        // report keyed by the same device name (`/run/midi/<device>`).
        let presence = self.kernel().midi_presence().get(bare);
        let label = presence_label(presence.as_ref());
        let record = serde_json::json!({
            "path": canonical,
            "name": bare,
            "content_length": content.len(),
            "content": content,
            "presence": label,
            "presence_record": presence.as_ref().map(|r| r.to_json()),
        });
        if json {
            return KjResult::ok_with_data(record.to_string(), record);
        }
        let ports = presence
            .as_ref()
            .filter(|r| r.present)
            .map(|r| {
                r.ports
                    .iter()
                    .map(|p| format!("{} ({})", p.name, p.address))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty())
            .map(|s| format!("ports:   {s}\n"))
            .unwrap_or_default();
        // Which sink saw it — the answer to "live, but WHERE?" for a rig
        // spread across machines. Omitted rather than faked when unknown.
        let host = presence
            .as_ref()
            .map(|r| r.sink.host.clone())
            .filter(|h| !h.is_empty())
            .map(|h| format!("host:    {h}\n"))
            .unwrap_or_default();
        let out = format!(
            "path:    {canonical}\nlength:  {} bytes\npresent: {label}\n{host}{ports}\n{content}\n",
            content.len(),
        );
        KjResult::ok_typed_with_data(out, ContentType::Markdown, record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::KjResult;
    use crate::kj::test_helpers::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn canonical_accepts_bare_and_full_rejects_nesting() {
        assert_eq!(
            midi_device_canonical("minibrute").unwrap(),
            "/etc/midi/devices/minibrute"
        );
        assert_eq!(
            midi_device_canonical("/etc/midi/devices/timidity").unwrap(),
            "/etc/midi/devices/timidity"
        );
        assert!(midi_device_canonical("sub/device").is_err());
        assert!(midi_device_canonical("/etc/midi/devices/a/b").is_err());
        assert!(midi_device_canonical("").is_err());
        assert!(midi_device_canonical("..").is_err());
    }

    #[test]
    fn doc_title_strips_heading_and_falls_back() {
        assert_eq!(
            doc_title("# Arturia MiniBrute (original) — device profile\n\nbody"),
            "Arturia MiniBrute (original) — device profile"
        );
        assert_eq!(doc_title("\n\n   \n"), "(untitled)");
        assert_eq!(doc_title("plain first line\nmore"), "plain first line");
    }

    /// A fresh kernel (the real CRDT-native `/etc/midi` mount, seeded from
    /// embedded defaults) already carries the shipped device profiles — no
    /// separate bootstrap step needed by callers.
    #[tokio::test]
    async fn fresh_kernel_seeds_midi_devices_into_the_vfs() {
        let d = test_dispatcher_crdt_rc().await;
        use crate::vfs::VfsOps;
        let names: Vec<_> = d
            .kernel()
            .vfs()
            .readdir(std::path::Path::new("/etc/midi/devices"))
            .await
            .expect("readdir /etc/midi/devices")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.contains(&"minibrute".to_string()), "names: {names:?}");
        assert!(names.contains(&"timidity".to_string()), "names: {names:?}");
        assert!(names.contains(&"keystep-pro".to_string()), "names: {names:?}");
        assert!(
            names.contains(&"keylab-88-mkii".to_string()),
            "names: {names:?}"
        );
    }

    #[tokio::test]
    async fn list_shows_all_four_shipped_devices_with_titles() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let arr = v.as_array().expect("array");
                let title_contains = |name: &str, needle: &str| {
                    let row = arr
                        .iter()
                        .find(|row| row["name"] == name)
                        .unwrap_or_else(|| panic!("{name} row present"));
                    assert!(
                        row["title"].as_str().is_some_and(|t| t.contains(needle)),
                        "{name} title: {row:?}"
                    );
                    assert_eq!(row["kind"], "profile", "{name} kind: {row:?}");
                };
                title_contains("minibrute", "MiniBrute");
                title_contains("timidity", "TiMidity");
                title_contains("keystep-pro", "KeyStep Pro");
                title_contains("keylab-88-mkii", "KeyLab mkII 88");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_non_json_renders_a_labelled_line_per_device() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d.dispatch(&[s("midi"), s("list")], &c).await;
        match result {
            KjResult::Ok { message, .. } => {
                assert!(message.contains("minibrute"), "message: {message}");
                assert!(message.contains("timidity"), "message: {message}");
                assert!(message.contains("keystep-pro"), "message: {message}");
                assert!(message.contains("keylab-88-mkii"), "message: {message}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn show_returns_the_seeded_document_content() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("midi"), s("show"), s("minibrute"), s("--json")], &c)
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["path"].as_str(), Some("/etc/midi/devices/minibrute"));
                let content = v["content"].as_str().expect("content present");
                assert!(content.contains("MiniBrute"), "content: {content}");
                assert!(content.contains("\"device\": \"minibrute\""), "content: {content}");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn show_raw_emits_exactly_the_stored_document() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("midi"), s("show"), s("timidity"), s("--raw")], &c)
            .await;
        match result {
            KjResult::Ok { message, .. } => {
                assert!(message.starts_with("# TiMidity"), "message: {message}");
                // No decoration header leaked into the raw body.
                assert!(!message.starts_with("path:"), "message: {message}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// `kj midi show` of an unknown device fails loud, not silently — no
    /// empty-content fallback (the house crash-over-corruption stance).
    #[tokio::test]
    async fn show_unknown_device_errors_loudly() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("midi"), s("show"), s("nonesuch")], &c)
            .await;
        match result {
            KjResult::Err(msg) => {
                assert!(msg.contains("unknown device"), "msg: {msg}");
                assert!(msg.contains("nonesuch"), "msg: {msg}");
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// A device name that tries to escape the flat namespace is rejected at
    /// the grammar, not turned into a confusing VFS error.
    #[tokio::test]
    async fn show_rejects_nested_or_escaping_names() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("midi"), s("show"), s("../../etc/passwd")], &c)
            .await;
        match result {
            KjResult::Err(msg) => assert!(msg.contains("flat namespace"), "msg: {msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// A directory under `/etc/midi/devices` (the shape a future rc-style
    /// bucket device will take, `docs/midi-next.md` "The core split") must
    /// render as a visible, clearly-labelled row — not silently vanish behind
    /// the old `is_file()` filter. Built through the real CRDT-native
    /// `/etc/midi` mount (same fixture `fresh_kernel_seeds_midi_devices_into_the_vfs`
    /// uses): `ConfigCrdtFs` synthesizes directories from descendant paths
    /// (see `readdir_synthesizes_virtual_directories` in
    /// `runtime::config_crdt_fs`), so writing a leaf file under
    /// `/etc/midi/devices/<bucket>/...` is enough to make `<bucket>` itself
    /// appear as a real `FileType::Directory` readdir entry — no fixture
    /// workaround needed.
    #[tokio::test]
    async fn list_renders_a_placeholder_row_for_a_bucket_directory() {
        use crate::vfs::VfsOps;
        let d = test_dispatcher_crdt_rc().await;
        d.kernel()
            .vfs()
            .write_all(
                std::path::Path::new("/etc/midi/devices/future-bucket/S00-notes.md"),
                b"stub rc-style bucket file, not a seed",
            )
            .await
            .expect("write nested bucket file");

        let c = test_caller();
        let result = d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let arr = v.as_array().expect("array");
                let bucket = arr
                    .iter()
                    .find(|row| row["name"] == "future-bucket")
                    .expect("future-bucket row present");
                assert_eq!(bucket["kind"], "bucket", "row: {bucket:?}");
                assert_eq!(
                    bucket["title"],
                    BUCKET_PLACEHOLDER_TITLE,
                    "row: {bucket:?}"
                );
                // The real profiles are still there alongside it.
                assert!(
                    arr.iter().any(|row| row["name"] == "minibrute"),
                    "arr: {arr:?}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// The human (non-JSON) view carries the same placeholder — a bucket
    /// device is visible there too, not just in `--json`.
    #[tokio::test]
    async fn list_non_json_renders_the_bucket_placeholder_too() {
        use crate::vfs::VfsOps;
        let d = test_dispatcher_crdt_rc().await;
        d.kernel()
            .vfs()
            .write_all(
                std::path::Path::new("/etc/midi/devices/future-bucket/S00-notes.md"),
                b"stub rc-style bucket file, not a seed",
            )
            .await
            .expect("write nested bucket file");

        let c = test_caller();
        let result = d.dispatch(&[s("midi"), s("list")], &c).await;
        match result {
            KjResult::Ok { message, .. } => {
                let line = message
                    .lines()
                    .find(|l| l.contains("future-bucket"))
                    .expect("future-bucket row");
                assert!(
                    line.contains("bucket device — kj midi bucket support not built yet"),
                    "line: {line}"
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    // ── presence (docs/midi-next.md "Presence is sink-fed") ───────────────

    use crate::midi_presence::{MidiPortFact, MidiPresenceRecord, SinkAttribution};

    fn sink_report(device: &str, present: bool, at_ns: u64) -> MidiPresenceRecord {
        sink_report_from(device, present, at_ns, "moltar")
    }

    fn sink_report_from(
        device: &str,
        present: bool,
        at_ns: u64,
        host: &str,
    ) -> MidiPresenceRecord {
        MidiPresenceRecord::from_sink(
            device,
            present,
            "alsa",
            if present {
                vec![MidiPortFact {
                    name: "MiniBrute MIDI 1".into(),
                    address: "24:0".into(),
                }]
            } else {
                vec![]
            },
            at_ns,
            SinkAttribution::new(kaijutsu_types::SessionId::new(), host),
        )
    }

    /// A kernel nobody has reported to says **unknown** for every profile —
    /// never "absent". This is the fresh-kernel/restart case: presence is
    /// ephemeral, so silence means we don't know, not that nothing is there.
    #[tokio::test]
    async fn list_presence_is_unknown_until_a_sink_reports() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                for row in v.as_array().expect("array") {
                    assert_eq!(row["presence"], "unknown", "row: {row}");
                    assert!(row["at"].is_null(), "row: {row}");
                }
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// The whole point of step 3: a sink report turns the column live, and an
    /// unplug turns it absent. Both are visible to `kj midi list` on any node.
    #[tokio::test]
    async fn list_presence_follows_sink_reports_through_plug_and_unplug() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let presence = d.kernel().midi_presence().clone();

        presence.record(sink_report("minibrute", true, 1_000)).unwrap();
        let row = |v: &serde_json::Value, name: &str| -> serde_json::Value {
            v.as_array()
                .expect("array")
                .iter()
                .find(|r| r["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("row {name} present"))
        };

        let result = d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await;
        let KjResult::Ok { data: Some(v), .. } = result else {
            panic!("expected Ok with data");
        };
        assert_eq!(row(&v, "minibrute")["presence"], "live");
        assert_eq!(row(&v, "minibrute")["at"], 1_000);
        assert_eq!(row(&v, "minibrute")["backend"], "alsa");
        // An un-reported neighbour stays unknown — presence is per device.
        assert_eq!(row(&v, "timidity")["presence"], "unknown");

        // Unplug: the report flips the column, it does not vanish.
        presence.record(sink_report("minibrute", false, 2_000)).unwrap();
        let result = d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await;
        let KjResult::Ok { data: Some(v), .. } = result else {
            panic!("expected Ok with data");
        };
        assert_eq!(row(&v, "minibrute")["presence"], "absent");
        assert_eq!(row(&v, "minibrute")["at"], 2_000);
    }

    /// The human view carries the column too (this is what a player reads).
    #[tokio::test]
    async fn list_non_json_renders_the_presence_column() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        d.kernel()
            .midi_presence()
            .record(sink_report("minibrute", true, 1))
            .unwrap();

        let result = d.dispatch(&[s("midi"), s("list")], &c).await;
        match result {
            KjResult::Ok { message, .. } => {
                let line = message
                    .lines()
                    .find(|l| l.contains("minibrute"))
                    .expect("minibrute row");
                assert!(line.contains("live"), "line: {line}");
                let other = message
                    .lines()
                    .find(|l| l.contains("timidity"))
                    .expect("timidity row");
                assert!(other.contains("unknown"), "line: {other}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// "Live" is only half an answer on a rig spread across machines — the
    /// other half is WHERE, and it rides the same row.
    #[tokio::test]
    async fn list_says_which_host_a_device_is_live_on() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        d.kernel()
            .midi_presence()
            .record(sink_report_from("minibrute", true, 1, "moltar"))
            .unwrap();

        let KjResult::Ok { data: Some(v), .. } =
            d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await
        else {
            panic!("expected Ok with data");
        };
        let row = |name: &str| -> serde_json::Value {
            v.as_array()
                .expect("array")
                .iter()
                .find(|r| r["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("row {name} present"))
        };
        assert_eq!(row("minibrute")["host"], "moltar");
        // Unknown presence has no host to report — null, never a guess at
        // "this machine" (the reporting sink need not be local at all).
        assert!(row("timidity")["host"].is_null());
    }

    /// The human table grows a where-column once somebody has answered it.
    #[tokio::test]
    async fn list_non_json_renders_the_host_column() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        d.kernel()
            .midi_presence()
            .record(sink_report_from("minibrute", true, 1, "moltar"))
            .unwrap();

        let KjResult::Ok { message, .. } = d.dispatch(&[s("midi"), s("list")], &c).await else {
            panic!("expected Ok");
        };
        let line = message
            .lines()
            .find(|l| l.contains("minibrute"))
            .expect("minibrute row");
        assert!(line.contains("live"), "line: {line}");
        assert!(line.contains("moltar"), "line: {line}");
    }

    /// With nothing reported the column is pure padding — don't draw it.
    #[tokio::test]
    async fn list_non_json_omits_the_host_column_when_nobody_reported() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let KjResult::Ok { message, .. } = d.dispatch(&[s("midi"), s("list")], &c).await else {
            panic!("expected Ok");
        };
        let line = message
            .lines()
            .find(|l| l.contains("minibrute"))
            .expect("minibrute row");
        let after = line.split("unknown").nth(1).expect("presence label");
        assert!(
            after.starts_with("  ") && !after.starts_with("   "),
            "the title must follow the label directly, with no blank \
             where-column between them: {line:?}"
        );
    }

    /// `kj midi show` carries the live record (including the port facts the
    /// sink reported) alongside the durable document.
    #[tokio::test]
    async fn show_includes_the_presence_record() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        d.kernel()
            .midi_presence()
            .record(sink_report("minibrute", true, 7))
            .unwrap();

        let result = d
            .dispatch(&[s("midi"), s("show"), s("minibrute"), s("--json")], &c)
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["presence"], "live");
                assert_eq!(v["presence_record"]["present"]["value"], true);
                assert_eq!(v["presence_record"]["present"]["source"], "sink");
                assert_eq!(v["presence_record"]["ports"]["value"][0]["address"], "24:0");
                assert_eq!(v["presence_record"]["host"]["value"], "moltar");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// The human view of `show` answers "where" too.
    #[tokio::test]
    async fn show_non_json_names_the_reporting_host() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        d.kernel()
            .midi_presence()
            .record(sink_report_from("minibrute", true, 7, "zorak"))
            .unwrap();

        let KjResult::Ok { message, .. } = d
            .dispatch(&[s("midi"), s("show"), s("minibrute")], &c)
            .await
        else {
            panic!("expected Ok");
        };
        assert!(message.contains("host:    zorak"), "message: {message}");
    }

    /// A sink's disconnect un-knows what it told us, everywhere it shows: the
    /// row goes back to `unknown` and `/run/midi/<device>` stops existing.
    /// This is the crashed-sink case — no unplug report is ever coming.
    #[tokio::test]
    async fn a_reaped_connection_takes_its_rows_and_run_files_with_it() {
        use crate::vfs::{VfsError, VfsOps};
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let record = sink_report_from("minibrute", true, 1, "moltar");
        let connection = record.sink.connection;
        d.kernel().midi_presence().record(record).unwrap();

        d.kernel().midi_presence().reap_connection(connection);

        let KjResult::Ok { data: Some(v), .. } =
            d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await
        else {
            panic!("expected Ok with data");
        };
        let row = v
            .as_array()
            .expect("array")
            .iter()
            .find(|r| r["name"] == "minibrute")
            .cloned()
            .expect("row minibrute present");
        assert_eq!(row["presence"], "unknown", "reaped reads as unknown");
        assert!(row["host"].is_null());
        assert!(row["at"].is_null());

        assert!(
            matches!(
                d.kernel()
                    .vfs()
                    .read_all(std::path::Path::new("/run/midi/minibrute"))
                    .await,
                Err(VfsError::NotFound(_))
            ),
            "the /run entry must be gone, not a lingering present=true file"
        );
    }

    /// `kj midi list` reads the same state the `/run/midi/<device>` view
    /// serves — one store, two readers (kai/kaish read the path, kj reads the
    /// store). If these ever disagree, presence is lying somewhere.
    #[tokio::test]
    async fn the_run_midi_view_agrees_with_the_list_column() {
        use crate::vfs::VfsOps;
        let d = test_dispatcher_crdt_rc().await;
        d.kernel()
            .midi_presence()
            .record(sink_report("minibrute", true, 5))
            .unwrap();

        let names: Vec<String> = d
            .kernel()
            .vfs()
            .readdir(std::path::Path::new("/run/midi"))
            .await
            .expect("readdir /run/midi")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["minibrute".to_string()]);

        let bytes = d
            .kernel()
            .vfs()
            .read_all(std::path::Path::new("/run/midi/minibrute"))
            .await
            .expect("read /run/midi/minibrute");
        let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(doc["present"]["value"], true);
        assert_eq!(doc["present"]["source"], "sink");
        assert_eq!(doc["present"]["at"], 5);
    }

    // ── emit: send / panic (docs/midi-next.md slice 1 step 4) ─────────────

    use kaijutsu_audio::{CuePayload, MIDI_CONTROL_MIME, MidiControl};

    /// Subscribe to the render-cue topic and return the receiver. The FlowBus
    /// is a live broadcast, not a queue — subscribe BEFORE dispatching.
    fn cue_sub(d: &crate::kj::KjDispatcher) -> crate::flows::Subscription<crate::flows::BlockFlow> {
        d.kernel().block_flows().subscribe("block.render_cue")
    }

    /// Pull the one published control cue off the bus and decode its
    /// envelope. Panics loudly if nothing was published — a silent
    /// no-publish is exactly the failure these tests exist to catch.
    fn expect_control_cue(sub: &mut crate::flows::Subscription<crate::flows::BlockFlow>) -> (MidiControl, RenderCue) {
        let msg = sub.try_recv().expect("a RenderCue should have been published");
        match msg.payload {
            crate::flows::BlockFlow::RenderCue { cue, .. } => {
                assert_eq!(cue.mime, MIDI_CONTROL_MIME, "control cues get their own mime");
                assert_eq!(
                    cue.lead,
                    std::time::Duration::ZERO,
                    "a control send is onset-now (lead 0)"
                );
                let CuePayload::Inline(bytes) = &cue.payload else {
                    panic!("control cues are always inline, never CAS: {:?}", cue.payload);
                };
                let envelope = MidiControl::parse(std::str::from_utf8(bytes).expect("utf8"))
                    .expect("the published envelope must parse");
                (envelope, cue)
            }
            other => panic!("expected RenderCue, got {other:?}"),
        }
    }

    /// The core of step 4: a `send` puts a device-ADDRESSED cue on the same
    /// wire `kj play` uses, carrying the device name (the sink's only routing
    /// key) and the composed bytes. The gated note's Note Off rides the SAME
    /// cue at an offset — one cue, so a lost second cue can never strand a
    /// sounding note.
    #[tokio::test]
    async fn send_note_publishes_a_device_addressed_control_cue() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let mut sub = cue_sub(&d);

        let result = d
            .dispatch(
                &[
                    s("midi"), s("send"), s("minibrute"), s("note"),
                    s("1"), s("60"), s("100"), s("--off-ms"), s("250"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "send failed: {result:?}");

        let (envelope, _cue) = expect_control_cue(&mut sub);
        assert_eq!(envelope.device, "minibrute", "the cue names the DEVICE, not a port");
        assert_eq!(
            envelope.decoded().unwrap(),
            vec![(0, vec![0x90, 60, 100]), (250, vec![0x80, 60, 0])],
        );
    }

    /// Channel 1-16 at the surface, `channel - 1` on the wire (the profile
    /// documents' convention). Channel 16 is the interesting end: it must be
    /// nibble 0xF, not a wrapped 0x0.
    #[tokio::test]
    async fn send_uses_front_panel_channel_numbering_on_the_wire() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let mut sub = cue_sub(&d);
        let result = d
            .dispatch(
                &[s("midi"), s("send"), s("minibrute"), s("cc"), s("16"), s("74"), s("64")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "send failed: {result:?}");
        let (envelope, _) = expect_control_cue(&mut sub);
        assert_eq!(envelope.decoded().unwrap(), vec![(0, vec![0xBF, 74, 64])]);
    }

    #[tokio::test]
    async fn send_pc_is_a_two_byte_program_change() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let mut sub = cue_sub(&d);
        let result = d
            .dispatch(&[s("midi"), s("send"), s("timidity"), s("pc"), s("2"), s("40")], &c)
            .await;
        assert!(result.is_ok(), "send failed: {result:?}");
        let (envelope, _) = expect_control_cue(&mut sub);
        assert_eq!(envelope.device, "timidity");
        assert_eq!(envelope.decoded().unwrap(), vec![(0, vec![0xC1, 40])]);
    }

    /// A note with no `--off-ms` is a bare Note On that sustains — one event,
    /// no invented note-off. Making one up would be a different instruction
    /// than the one typed.
    #[tokio::test]
    async fn an_ungated_note_sends_exactly_one_message() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let mut sub = cue_sub(&d);
        let result = d
            .dispatch(
                &[s("midi"), s("send"), s("minibrute"), s("note"), s("1"), s("60"), s("100")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "send failed: {result:?}");
        let (envelope, _) = expect_control_cue(&mut sub);
        assert_eq!(envelope.decoded().unwrap(), vec![(0, vec![0x90, 60, 100])]);
    }

    /// The profile IS the gate: an unknown device is a loud error and NOTHING
    /// reaches the wire. There is no port to resolve at any sink, so a
    /// cheerful "sent!" would be a lie.
    #[tokio::test]
    async fn send_to_an_unknown_device_errors_and_publishes_nothing() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let mut sub = cue_sub(&d);
        let result = d
            .dispatch(
                &[s("midi"), s("send"), s("nonesuch"), s("cc"), s("1"), s("74"), s("64")],
                &c,
            )
            .await;
        match result {
            KjResult::Err(msg) => {
                assert!(msg.contains("unknown device"), "msg: {msg}");
                assert!(msg.contains("nonesuch"), "msg: {msg}");
            }
            other => panic!("expected Err, got {other:?}"),
        }
        assert!(sub.try_recv().is_none(), "an unknown device must publish no cue");
    }

    /// Presence is NOT a gate (`CLAUDE.md` "crosstalk-as-feature"): a device
    /// nobody has reported still gets its cue, with the warning riding the
    /// result. The sink drops what it can't route, loudly, in its own logs.
    #[tokio::test]
    async fn send_to_an_unreported_device_warns_but_still_sends() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let mut sub = cue_sub(&d);
        let result = d
            .dispatch(
                &[s("midi"), s("send"), s("minibrute"), s("cc"), s("1"), s("74"), s("64")],
                &c,
            )
            .await;
        match &result {
            KjResult::Ok { message, data: Some(v), .. } => {
                assert!(message.contains("warning"), "message: {message}");
                let warnings = v["warnings"].as_array().expect("warnings array");
                assert!(
                    warnings.iter().any(|w| w.as_str().is_some_and(|w| w.contains("UNKNOWN"))),
                    "warnings: {warnings:?}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
        let (envelope, _) = expect_control_cue(&mut sub);
        assert_eq!(envelope.device, "minibrute", "the cue went out regardless");
    }

    /// A live device carries no presence warning — only the genuine ones, so
    /// the warning stays worth reading.
    #[tokio::test]
    async fn send_to_a_live_device_carries_no_presence_warning() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        d.kernel()
            .midi_presence()
            .record(sink_report("minibrute", true, 1))
            .unwrap();
        let _sub = cue_sub(&d);
        let result = d
            .dispatch(
                &[s("midi"), s("send"), s("minibrute"), s("cc"), s("1"), s("74"), s("64")],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let warnings = v["warnings"].as_array().expect("warnings array");
                assert!(
                    !warnings.iter().any(|w| w.as_str().is_some_and(|w| w.contains("UNKNOWN")
                        || w.contains("ABSENT"))),
                    "a live device needs no presence warning: {warnings:?}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// A device a sink watched leave says so specifically — "absent" and
    /// "unknown" are different facts all the way to the CLI.
    #[tokio::test]
    async fn send_to_an_absent_device_says_absent_not_unknown() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        d.kernel()
            .midi_presence()
            .record(sink_report("minibrute", false, 1))
            .unwrap();
        let _sub = cue_sub(&d);
        let result = d
            .dispatch(
                &[s("midi"), s("send"), s("minibrute"), s("cc"), s("1"), s("74"), s("64")],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let warnings = v["warnings"].as_array().expect("warnings array");
                assert!(
                    warnings.iter().any(|w| w.as_str().is_some_and(|w| w.contains("ABSENT"))),
                    "warnings: {warnings:?}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// The `kj midi` verbs are a device context's TOOLS — a model turning a
    /// knob must be able to read whether the send landed, so the result (and
    /// especially its warnings) is never ephemeral.
    #[tokio::test]
    async fn a_send_result_is_visible_to_the_model_not_ephemeral() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(
                &[s("midi"), s("send"), s("minibrute"), s("cc"), s("1"), s("74"), s("64")],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { ephemeral, .. } => {
                assert!(!ephemeral, "a device context's tool result must reach the model")
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// A kernel nobody's sink is attached to publishes into nothing — say so,
    /// rather than let a player debug a cable that's fine.
    #[tokio::test]
    async fn a_send_with_no_attached_sink_warns_about_zero_listeners() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        // Deliberately NO subscriber.
        let result = d
            .dispatch(
                &[s("midi"), s("send"), s("minibrute"), s("cc"), s("1"), s("74"), s("64")],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["receivers"], 0);
                let warnings = v["warnings"].as_array().expect("warnings array");
                assert!(
                    warnings.iter().any(|w| w.as_str().is_some_and(|w| w.contains("0 listeners"))),
                    "warnings: {warnings:?}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// `kj midi panic <device>` = CC 123 then CC 120 on all 16 channels.
    #[tokio::test]
    async fn panic_sends_all_notes_off_then_all_sound_off_on_every_channel() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let mut sub = cue_sub(&d);
        let result = d.dispatch(&[s("midi"), s("panic"), s("minibrute")], &c).await;
        assert!(result.is_ok(), "panic failed: {result:?}");

        let (envelope, _) = expect_control_cue(&mut sub);
        assert_eq!(envelope.device, "minibrute");
        let messages = envelope.decoded().unwrap();
        assert_eq!(messages.len(), 32, "16 channels x 2 controllers");
        for ch in 0..16u8 {
            assert_eq!(messages[ch as usize * 2], (0, vec![0xB0 | ch, 123, 0]));
            assert_eq!(messages[ch as usize * 2 + 1], (0, vec![0xB0 | ch, 120, 0]));
        }
    }

    /// Bare `kj midi panic` is the stop-everything reflex: it fans out to
    /// every device the rig reports LIVE, one cue each, without making the
    /// player name the offender.
    #[tokio::test]
    async fn panic_with_no_device_fans_out_to_every_live_device() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let presence = d.kernel().midi_presence();
        presence.record(sink_report("minibrute", true, 1)).unwrap();
        presence.record(sink_report("keystep-pro", true, 1)).unwrap();
        // An absent device is not panicked at: nothing there is sounding.
        presence.record(sink_report("timidity", false, 1)).unwrap();
        let mut sub = cue_sub(&d);

        let result = d.dispatch(&[s("midi"), s("panic")], &c).await;
        assert!(result.is_ok(), "panic failed: {result:?}");

        let mut devices = vec![expect_control_cue(&mut sub).0.device, expect_control_cue(&mut sub).0.device];
        devices.sort();
        assert_eq!(devices, vec!["keystep-pro".to_string(), "minibrute".to_string()]);
        assert!(sub.try_recv().is_none(), "exactly one cue per live device");
    }

    /// Nothing live means nothing was told to stop — a loud error, not a
    /// cheerful no-op that leaves a droning synth droning.
    #[tokio::test]
    async fn panic_with_nothing_live_is_a_loud_error() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let mut sub = cue_sub(&d);
        match d.dispatch(&[s("midi"), s("panic")], &c).await {
            KjResult::Err(msg) => {
                assert!(msg.contains("no device is reported live"), "msg: {msg}");
                assert!(msg.contains("kj midi panic <device>"), "names the escape hatch: {msg}");
            }
            other => panic!("expected Err, got {other:?}"),
        }
        assert!(sub.try_recv().is_none());
    }

    /// A NAMED panic works even when presence says nothing — presence never
    /// gates, it only warns (the same rule `send` follows).
    #[tokio::test]
    async fn a_named_panic_works_on_an_unreported_device() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let mut sub = cue_sub(&d);
        let result = d.dispatch(&[s("midi"), s("panic"), s("timidity")], &c).await;
        assert!(result.is_ok(), "panic failed: {result:?}");
        assert_eq!(expect_control_cue(&mut sub).0.device, "timidity");
    }

    #[tokio::test]
    async fn panic_on_an_unknown_device_errors_and_publishes_nothing() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let mut sub = cue_sub(&d);
        match d.dispatch(&[s("midi"), s("panic"), s("nonesuch")], &c).await {
            KjResult::Err(msg) => assert!(msg.contains("unknown device"), "msg: {msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
        assert!(sub.try_recv().is_none());
    }

    /// `--context` is `global` on `send` so it can be written where a player
    /// naturally types it — after the message, not wedged between the device
    /// and the verb. Pinned by a test because "global" is the kind of clap
    /// detail that silently stops working.
    #[test]
    fn the_context_flag_is_accepted_after_the_message_verb() {
        // `no_binary_name` — the dispatcher hands the noun's ARGV, not "midi".
        let args =
            MidiArgs::try_parse_from(["send", "minibrute", "cc", "1", "74", "64", "-c", "scratch"])
                .expect("parse");
        match args.command {
            MidiCommand::Send { device, context, message } => {
                assert_eq!(device, "minibrute");
                assert_eq!(context.as_deref(), Some("scratch"));
                assert!(matches!(message, SendMessage::Cc { channel: 1, controller: 74, value: 64 }));
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    // ── range validation (pure) ───────────────────────────────────────────

    /// Channel 0 is the classic off-by-one typo. Refuse rather than clamp: a
    /// clamped channel sends a whole phrase to the wrong instrument.
    #[test]
    fn channel_numbering_refuses_zero_and_seventeen() {
        assert_eq!(wire_channel(1), Ok(0));
        assert_eq!(wire_channel(16), Ok(15));
        assert!(wire_channel(0).is_err());
        assert!(wire_channel(17).is_err());
    }

    /// A data byte ≥ 0x80 IS a status byte on the wire — an unmasked typo
    /// wouldn't just mis-sound, it would inject a rogue message. Refuse it,
    /// naming the field.
    #[test]
    fn a_data_byte_above_seven_bits_is_refused_by_name() {
        assert_eq!(data_byte("velocity", 127), Ok(127));
        let err = data_byte("velocity", 200).unwrap_err();
        assert!(err.contains("velocity"), "err: {err}");
        assert!(err.contains("0-127"), "err: {err}");
    }

    #[tokio::test]
    async fn an_out_of_range_value_is_refused_before_anything_is_published() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let mut sub = cue_sub(&d);
        match d
            .dispatch(
                &[s("midi"), s("send"), s("minibrute"), s("note"), s("0"), s("60"), s("100")],
                &c,
            )
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("channel 0 out of range"), "msg: {msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
        assert!(sub.try_recv().is_none(), "a rejected message publishes nothing");
    }

    // ── sysex (pure + wired) ──────────────────────────────────────────────

    #[test]
    fn sysex_splits_a_stream_into_complete_messages() {
        let stream = [0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7, 0xF0, 0x00, 0x20, 0x6B, 0xF7];
        assert_eq!(
            split_sysex(&stream).unwrap(),
            vec![
                vec![0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7],
                vec![0xF0, 0x00, 0x20, 0x6B, 0xF7],
            ]
        );
    }

    /// A truncated SysEx can leave real gear waiting mid-transfer — worse
    /// than not sending at all. Refuse every malformed shape by name.
    #[test]
    fn malformed_sysex_is_refused_by_name() {
        assert!(split_sysex(&[]).unwrap_err().contains("empty"));
        assert!(split_sysex(&[0xF0, 0x7E]).unwrap_err().contains("never reached F7"));
        assert!(split_sysex(&[0x7E, 0xF7]).unwrap_err().contains("must start with F0"));
        assert!(split_sysex(&[0xF0, 0xF0, 0xF7]).unwrap_err().contains("second F0"));
        // A status byte inside the payload corrupts the dialogue.
        assert!(split_sysex(&[0xF0, 0x90, 0xF7]).unwrap_err().contains("7-bit"));
    }

    /// Manuals print SysEx as "F0 7E 7F 06 01 F7" — accept that verbatim,
    /// plus the comma/0x forms code tends to produce.
    #[test]
    fn sysex_hex_tolerates_the_separators_manuals_use() {
        let want = vec![vec![0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7]];
        assert_eq!(sysex_messages("F0 7E 7F 06 01 F7").unwrap(), want);
        assert_eq!(sysex_messages("0xF0,0x7E,0x7F,0x06,0x01,0xF7").unwrap(), want);
        assert_eq!(sysex_messages("f07e7f0601f7").unwrap(), want);
    }

    #[tokio::test]
    async fn send_sysex_publishes_the_bytes_verbatim() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let mut sub = cue_sub(&d);
        let result = d
            .dispatch(
                &[s("midi"), s("send"), s("keystep-pro"), s("sysex"), s("F0 7E 7F 06 01 F7")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "sysex send failed: {result:?}");
        let (envelope, _) = expect_control_cue(&mut sub);
        assert_eq!(
            envelope.decoded().unwrap(),
            vec![(0, vec![0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7])]
        );
    }

    /// `@<path>` reads a raw `.syx` file — the universal interchange format,
    /// and the whole point of the day-one escape hatch.
    #[tokio::test]
    async fn send_sysex_reads_a_raw_syx_file() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("identity.syx");
        std::fs::write(&path, [0xF0u8, 0x7E, 0x7F, 0x06, 0x01, 0xF7]).expect("write syx");
        let mut sub = cue_sub(&d);
        let result = d
            .dispatch(
                &[
                    s("midi"), s("send"), s("keystep-pro"), s("sysex"),
                    format!("@{}", path.display()),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "sysex file send failed: {result:?}");
        let (envelope, _) = expect_control_cue(&mut sub);
        assert_eq!(
            envelope.decoded().unwrap(),
            vec![(0, vec![0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7])]
        );
    }

    #[tokio::test]
    async fn a_missing_sysex_file_is_a_loud_error() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        match d
            .dispatch(
                &[s("midi"), s("send"), s("keystep-pro"), s("sysex"), s("@/nope/missing.syx")],
                &c,
            )
            .await
        {
            KjResult::Err(msg) => assert!(msg.contains("sysex file"), "msg: {msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// `kj midi list` on a kernel with no `/etc/midi` mount at all answers
    /// truthfully with an empty listing rather than erroring — mirrors `kj
    /// config list`'s "absent mount reads as empty" contract.
    #[tokio::test]
    async fn list_with_no_midi_mount_is_an_empty_listing_not_an_error() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v.as_array().map(|a| a.len()), Some(0));
            }
            other => panic!("expected Ok with an empty array, got {other:?}"),
        }
    }
}
