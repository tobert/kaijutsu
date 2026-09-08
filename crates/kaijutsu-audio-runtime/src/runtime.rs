//! Connection-driven device runtime. The kernel remains the sole sequencer.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::sync::atomic::Ordering;
use std::time::Duration;

use kaijutsu_audio::{CaptureLimits, CaptureRing, Tracker, MIDI_CAPTURE_MIME};
use kaijutsu_client::{ActorHandle, ConnectionStatus, SshConfig};
use kaijutsu_types::ContextId;
use tokio::sync::watch;

use crate::dj::thread::{DjCtl, DjHandle};
use crate::inventory_report::{ReportInputs, build_report, report_due};
use crate::midi_in::EarEvent;
use crate::observer::{Observation, Observer};
use crate::patch_graph::PatchGraphReader;
use crate::midi_match::{DeviceMatch, match_ports, parse_profile};
use crate::midi_presence::{MidiPortTopology, BACKEND, DEVICES_DIR, MAX_PROFILES, routes_from_match, diff_presence, sink_host};

#[derive(Clone, Debug)]
pub struct Options {
    pub audio: bool,
    pub midi: bool,
    pub output: Option<String>,
    pub rt_priority: u8,
    /// Optional client config layer before the shared metronome default.
    pub config_client: Option<String>,
    /// This node's peer nick (e.g. `"audio/moltar"`) — the `node` field in
    /// every `reportAudioInventory` call (`docs/audio-daemon.md` "One
    /// inventory owner"). Empty disables inventory reporting entirely (no
    /// node name to report under), same as an absent `context` disables
    /// capture.
    pub node: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            audio: true,
            midi: cfg!(target_os = "linux"),
            output: None,
            rt_priority: 0,
            config_client: None,
            node: String::new(),
        }
    }
}

/// A local hardware owner. Dropping it stops and joins its workers.
pub struct Engine {
    stop: watch::Sender<bool>,
    join: Option<std::thread::JoinHandle<Result<(), String>>>,
    pub pulses: crossbeam_channel::Receiver<crate::dj::DjPulse>,
    pub(crate) observation: Option<Arc<std::sync::Mutex<Observation>>>,
    /// The node's model of the kernel's clock, for whatever stamps or reports
    /// outside the runtime thread (`docs/midi.md` "The one timebase").
    pub(crate) clock: kaijutsu_client::KernelClockHandle,
}

impl Engine {
    /// Start local I/O. With no context, capture and external clock reports
    /// are disabled; playback and device presence remain active.
    pub fn start(actor: ActorHandle, ssh: SshConfig, context: Option<ContextId>, options: Options) -> Result<Self, String> {
        if !options.audio && !options.midi {
            return Err("enable audio or MIDI".into());
        }
        if options.output.is_some() && !options.audio {
            return Err("an output device requires audio playback".into());
        }
        if options.rt_priority > 99 {
            return Err("RT priority must be 0–99".into());
        }
        let (stop, stop_rx) = watch::channel(false);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        // Taken before `actor` moves into the runtime thread; clones share
        // one model, so this reads whatever the pinger has learned.
        let clock = actor.clock_handle();
        let join = std::thread::Builder::new().name("kaijutsu-audio-node".into()).spawn(move || {
            let result = (|| {
                let _ownership = Ownership::acquire(&lock_path())?;
                let mut dj = DjHandle::start(&options)?;
                let ear = if options.midi { Some(Observer::start(crate::midi_in::spawn_capture_thread(options.rt_priority, actor.clock_handle())?, context.is_some())?) } else { None };
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
                dj.ctl_tx.send(DjCtl::ActorReady { handle: actor.clone(), ssh_config: ssh, generation: 0 })
                    .map_err(|e| e.to_string())?;
                let _ = ready_tx.send(Ok((dj.pulse_rx.clone(), ear.as_ref().map(|ear| ear.shared.clone()))));
                let result = rt.block_on(serve(&actor, context, ear, &mut dj, stop_rx, &options));
                actor.midi_exchange().clear();
                let stopped = dj.shutdown();
                result.and(stopped)
            })();
            if let Err(e) = &result {
                let _ = ready_tx.send(Err(e.clone()));
                tracing::error!("audio runtime stopped: {e}");
            }
            result
        }).map_err(|e| e.to_string())?;
        let (pulses, observation) = ready_rx.recv().map_err(|e| e.to_string())??;
        Ok(Self { stop, join: Some(join), pulses, observation, clock })
    }

    pub fn is_finished(&self) -> bool {
        self.join.as_ref().is_none_or(|join| join.is_finished())
    }

    pub fn shutdown(&mut self) -> Result<(), String> {
        let _ = self.stop.send(true);
        if let Some(join) = self.join.take() {
            join.join().map_err(|_| "audio runtime panicked".to_string())??;
        }
        Ok(())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() { tracing::error!("{e}"); }
    }
}

struct EarState {
    ring: CaptureRing,
    tracker: Tracker,
    topology: MidiPortTopology,
    clocks: BTreeMap<String, kaijutsu_audio::ClockEstimate>,
    rejected: u64,
}

impl EarState {
    /// `at_epoch_ns` is the kernel-domain "now" the ring's tracker starts
    /// from — every capture stamp is minted on the one timebase
    /// (`docs/midi.md` "The one timebase").
    fn new(at_epoch_ns: u64) -> Self {
        let ring = CaptureRing::with_limits(CaptureLimits { max_events: 16_384, max_bytes: 8 * 1024 * 1024, max_message_bytes: 64 * 1024 })
            .expect("valid recording history limits");
        let tracker = ring.tracker_at(at_epoch_ns);
        Self { ring, tracker, topology: Default::default(), clocks: Default::default(), rejected: 0 }
    }

    fn ingest(&mut self, event: EarEvent) {
        match event {
            EarEvent::Capture { event, .. } => {
                if kaijutsu_audio::keep_at_ingest(&event.bytes)
                    && let Err(error) = self.ring.try_push(event) {
                    self.rejected += 1;
                    tracing::warn!("MIDI recording history rejected event: {error}");
                }
            }
            EarEvent::Inventory { ports, .. } => {
                let mut topology = MidiPortTopology::default();
                for port in ports { topology.port_up(port.facts); }
                for previous in self.topology.ports() {
                    if !topology.ports().iter().any(|p| p.address == previous.address) { self.topology.port_down(&previous.address); }
                }
                for port in topology.ports() { self.topology.port_up(port); }
            }
            EarEvent::InventoryError { error } => { tracing::warn!("MIDI inventory unavailable: {error}"); }
            EarEvent::Watermark { .. } => {}
            EarEvent::Clock { source, estimate, discontinuities } => {
                if discontinuities > 0 { tracing::debug!(source, discontinuities, "MIDI clock discontinuities"); }
                self.clocks.insert(source, estimate);
            }
            EarEvent::PortUp(facts) => self.topology.port_up(facts),
            EarEvent::PortDown { address } => { self.topology.port_down(&address); self.clocks.remove(&address); }
            EarEvent::ClientDown { client_id } => {
                self.topology.client_down(client_id);
                self.clocks.retain(|source, _| !source.starts_with(&format!("{client_id}:")));
            }
        }
    }

    fn cut(&mut self, connected: bool, target: Option<ContextId>, now: u64) -> Option<kaijutsu_audio::CaptureBatch> {
        if !connected || target.is_none() { return None; }
        let mut batch = self.ring.cut(&mut self.tracker, now);
        batch.lost = batch.lost.saturating_add(std::mem::take(&mut self.rejected));
        (!batch.is_empty() || batch.lost > 0).then_some(batch)
    }
}

async fn fetch_profiles(actor: &ActorHandle) -> Result<Vec<DeviceMatch>, String> {
    let listing = actor.vfs_snapshot(&DEVICES_DIR, 1, MAX_PROFILES).await.map_err(|e| e.to_string())?;
    let mut profiles = Vec::new();
    for child in &listing.root.children {
        if child.kind != kaijutsu_client::VfsFileType::File { continue; }
        let path = format!("{}/{}", *DEVICES_DIR, child.name);
        let bytes = actor.vfs_read_all(path.clone()).await.map_err(|e| format!("{path}: {e}"))?;
        let body = String::from_utf8(bytes).map_err(|e| format!("{path}: {e}"))?;
        if let Some(profile) = parse_profile(&child.name, &body)? { profiles.push(profile); }
    }
    Ok(profiles)
}

async fn serve(actor: &ActorHandle, context: Option<ContextId>, ear: Option<Observer>, dj: &mut DjHandle, mut stop: watch::Receiver<bool>, options: &Options) -> Result<(), String> {
    let midi = options.midi;
    let routes = Arc::new(RwLock::new(BTreeMap::new()));
    let _exchange = if midi {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let join = crate::midi_exchange::spawn_exchange_thread(rx, routes.clone(), stop.clone())?;
        actor.midi_exchange().install(tx);
        Some(ExchangeWorker { slot: actor.midi_exchange(), stop, join: Some(join) })
    } else { None };
    // Presence, clock estimates and capture cuts are all stamped in the
    // kernel's domain: the kernel is the sole sequencer, so its clock is the
    // timebase every receiver ages these against.
    let clock = actor.clock_handle();
    let mut state = EarState::new(clock.now_ns());
    let mut status = actor.watch_status();
    let mut connected = false;
    let mut profiles = Vec::new();
    let mut reported = BTreeMap::new();
    let mut revision = None;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_cut = std::time::Instant::now();
    let mut legacy_seen = 0u64;
    // Sink-fed audio inventory (`docs/audio-daemon.md` "One inventory
    // owner"). A second, dedicated ALSA client so the report reflects the
    // WHOLE local seq graph, not just what the ear or the DJ's render port
    // happen to see — opened once, best-effort: a box with no sequencer at
    // all gets no inventory reporting rather than a fatal error, the same
    // stance `fetch_profiles` takes toward a missing config tree.
    let inventory_reader = if midi {
        match PatchGraphReader::open() {
            Ok(reader) => Some(reader),
            Err(e) => {
                tracing::warn!("audio inventory reporting unavailable: {e}");
                None
            }
        }
    } else {
        None
    };
    let inventory_own_client = inventory_reader.as_ref().and_then(|r| r.client_id().ok());
    let mut inventory_revision = 0u64;
    let mut last_inventory_report: Option<std::time::Instant> = None;
    let mut last_inventory_snapshot: Option<crate::patch_graph::PatchGraphSnapshot> = None;
    let mut last_inventory_render_events = 0u64;
    let mut last_inventory_input_events: BTreeMap<String, u64> = BTreeMap::new();
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => return Ok(()),
            result = status.changed() => {
                result.map_err(|e| e.to_string())?;
                connected = false;
                reported.clear();
                revision = None;
                state.clocks.clear();
                routes.write().expect("MIDI routes lock poisoned").clear();
                dj.ctl_tx.send(DjCtl::MidiRoutes(BTreeMap::new())).map_err(|e| e.to_string())?;
                // A reconnect is a new connection to the kernel's audio
                // inventory store, which enforces its own ordering per
                // connection — re-send a full snapshot rather than relying
                // on stale cadence state from the old one.
                last_inventory_report = None;
                last_inventory_snapshot = None;
            }
            _ = tick.tick() => {}
        }
        if dj.is_finished() { return Err("DJ worker stopped unexpectedly".into()); }
        if let Some(ear) = &ear {
            let lost = ear.shared.lock().expect("MIDI observation lock poisoned").legacy_lost;
            state.rejected = state.rejected.saturating_add(lost.saturating_sub(legacy_seen));
            legacy_seen = lost;
            for _ in 0..16_384 {
                match ear.rx.try_recv() {
                    Ok(event) => state.ingest(event),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return Err("MIDI capture worker stopped".into()),
                }
            }
        }
        let current = status.borrow().clone();
        if let ConnectionStatus::Terminal { reason } = current { return Err(reason); }
        if !matches!(current, ConnectionStatus::Connected { .. }) { continue; }
        // RPC work can wait on reconnect. Shutdown cancels the wait and
        // device teardown still runs on the owning thread.
        let work = async {
            if !connected {
                if midi { profiles = fetch_profiles(actor).await?; }
                let mut loaded = false;
                let layers = options.config_client.as_deref().map(Some).into_iter().chain(std::iter::once(None));
                for client in layers {
                    let path = kaijutsu_types::paths::client_config_path(client, "metronome.toml");
                    match actor.get_config(path.clone()).await {
                    Ok(body) if !body.trim().is_empty() => {
                        let config = toml::from_str(&body).map_err(|e| format!("invalid metronome config: {e}"))?;
                        dj.ctl_tx.send(DjCtl::MetronomeConfig(config)).map_err(|e| e.to_string())?;
                        loaded = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!("metronome config {path} unavailable: {e}"),
                    }
                }
                if !loaded { tracing::warn!("metronome config unavailable; keeping the current runtime settings"); }
                connected = true;
            }
            if midi && revision != Some(state.topology.revision()) {
                let matched = match_ports(&profiles, &state.topology.ports());
                for (port, devices) in &matched.ambiguous {
                    tracing::warn!(port = port.port_name, ?devices, "ambiguous MIDI profile match; device is not routed");
                }
                let next_routes = routes_from_match(&matched.devices);
                *routes.write().expect("MIDI routes lock poisoned") = next_routes.clone();
                dj.ctl_tx.send(DjCtl::MidiRoutes(next_routes)).map_err(|e| e.to_string())?;
                let mut previous = reported.clone();
                // A device may stay present while its port address changes.
                // Re-state live ports whenever topology changes.
                for device in matched.devices.keys() { previous.remove(device); }
                for report in diff_presence(&previous, &matched.devices) {
                    actor.report_midi_presence(report.device.clone(), report.present, BACKEND, report.ports, clock.now_ns(), sink_host())
                        .await.map_err(|e| e.to_string())?;
                    reported.insert(report.device, report.present);
                }
                revision = Some(state.topology.revision());
            }
            if let Some(reader) = &inventory_reader
                && !options.node.is_empty()
            {
                match reader.snapshot() {
                    Ok(snapshot) => {
                        // Named, not addressed: the render port's ALSA
                        // client id is assigned dynamically and lives on a
                        // different thread (the DJ's), but its client/port
                        // NAME is fixed — the graph already enumerates it
                        // like any other endpoint.
                        let render_addr = snapshot
                            .endpoints
                            .iter()
                            .find(|e| e.client_name == "kaijutsu-audio" && e.port_name == "render")
                            .map(|e| (e.client_id, e.port_id));
                        let render_events =
                            crate::dj::midi::RENDER_EVENTS_SENT.load(Ordering::Relaxed);
                        let render = render_addr.map(|addr| (addr, render_events));

                        let (observed_ports, input_events, inventory_state) = match &ear {
                            Some(ear) => {
                                let o = ear.shared.lock().expect("MIDI observation lock poisoned");
                                let state = if o.error.is_some() { "error" } else if o.ready { "ready" } else { "pending" };
                                (o.ports.clone(), o.event_counts.clone(), state)
                            }
                            None => (Vec::new(), BTreeMap::new(), "ready"),
                        };

                        let changed = Some(&snapshot) != last_inventory_snapshot.as_ref()
                            || render_events != last_inventory_render_events
                            || input_events != last_inventory_input_events;
                        let elapsed = last_inventory_report.map(|at: std::time::Instant| at.elapsed());

                        if report_due(changed, elapsed) {
                            inventory_revision += 1;
                            let own_clients = [
                                crate::midi_in::EAR_CLIENT_ID.load(Ordering::Relaxed),
                                crate::midi_exchange::EXCHANGE_CLIENT_ID.load(Ordering::Relaxed),
                                inventory_own_client.unwrap_or(-1),
                            ];
                            let report = build_report(&ReportInputs {
                                node: &options.node,
                                revision: inventory_revision,
                                observed_epoch_ns: clock.now_ns(),
                                backend: BACKEND,
                                state: inventory_state,
                                graph: &snapshot,
                                own_clients: &own_clients,
                                observed_ports: &observed_ports,
                                input_events: &input_events,
                                render,
                            });
                            let bytes = serde_json::to_vec(&report).map_err(|e| e.to_string())?;
                            actor
                                .report_audio_inventory(options.node.clone(), inventory_revision, clock.now_ns(), bytes)
                                .await
                                .map_err(|e| e.to_string())?;
                            last_inventory_report = Some(std::time::Instant::now());
                            last_inventory_render_events = render_events;
                            last_inventory_input_events = input_events;
                            last_inventory_snapshot = Some(snapshot);
                        }
                    }
                    Err(e) => tracing::warn!("audio inventory snapshot failed: {e}"),
                }
            }
            if let Some(target) = context {
                for (source, estimate) in std::mem::take(&mut state.clocks) {
                    actor.report_clock_estimate(target, estimate.reference.beat, estimate.reference.tempo_bps, estimate.epoch_ns, source)
                        .await.map_err(|e| e.to_string())?;
                }
                if last_cut.elapsed() >= Duration::from_secs(4) {
                    last_cut = std::time::Instant::now();
                    if let Some(batch) = state.cut(connected, context, clock.now_ns()) {
                        if batch.lost > 0 { tracing::warn!(lost = batch.lost, "MIDI capture ring overran"); }
                        let payload = batch.to_json_bytes().map_err(|e| e.to_string())?;
                        if let Err(e) = actor.commit_capture(target, MIDI_CAPTURE_MIME, payload).await {
                            tracing::warn!("MIDI capture batch refused: {e}; attach the capture context to a track");
                        }
                    }
                }
            } else { state.clocks.clear(); }
            Ok::<_, String>(())
        };
        tokio::select! {
            biased;
            _ = stop.changed() => return Ok(()),
            result = tokio::time::timeout(Duration::from_secs(10), work) => {
                if let Err(error) = result.unwrap_or_else(|_| Err("audio node RPC timed out".into())) {
                    tracing::warn!("{error}; will reconcile after reconnect or retry");
                    connected = false;
                    reported.clear();
                    revision = None;
                    tokio::select! {
                        _ = stop.changed() => return Ok(()),
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                }
            }
        }
    }
}

struct ExchangeWorker {
    slot: Arc<kaijutsu_client::MidiExchangeSlot>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ExchangeWorker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.slot.clear();
        if let Some(join) = self.join.take()
            && join.join().is_err() {
            tracing::error!("MIDI exchange worker panicked");
        }
    }
}

fn lock_path() -> PathBuf {
    #[cfg(unix)]
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = whoami::username();
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
        .join(format!("kaijutsu-audio-{uid}.lock"))
}

struct Ownership { _file: std::fs::File }

impl Ownership {
    fn acquire(path: &std::path::Path) -> Result<Self, String> {
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)
            .map_err(|e| format!("cannot open audio ownership lock {}: {e}", path.display()))?;
        file.try_lock().map_err(|e| format!("cannot own local audio: {e}; stop the other kaijutsu-audiod instance"))?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_survives_disconnection_and_missing_target() {
        let mut state = EarState::new(kaijutsu_client::local_epoch_ns());
        let now = kaijutsu_client::local_epoch_ns();
        state.ingest(EarEvent::Capture { event: kaijutsu_audio::CaptureEvent { epoch_ns: now, source: "24:0".into(), bytes: vec![0x90, 60, 100] }, observed_at: std::time::Instant::now() });
        assert!(state.cut(false, Some(ContextId::new()), now + 10).is_none());
        assert!(state.cut(true, None, now + 10).is_none());
        let batch = state.cut(true, Some(ContextId::new()), now + 10).unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.lost, 0);
    }

    #[test]
    fn unplug_removes_the_route_and_clock() {
        let mut state = EarState::new(kaijutsu_client::local_epoch_ns());
        state.ingest(EarEvent::PortUp(crate::midi_match::PortFacts { client_name: "synth".into(), port_name: "synth".into(), address: "24:0".into(), usb_id: None }));
        state.ingest(EarEvent::Clock { source: "24:0".into(), estimate: kaijutsu_audio::ClockEstimate { reference: kaijutsu_audio::BeatRef::new(1.0, 2.0), epoch_ns: 1, residual_ns: 0 }, discontinuities: 0 });
        state.ingest(EarEvent::ClientDown { client_id: 24 });
        assert!(state.topology.ports().is_empty());
        assert!(state.clocks.is_empty());
    }

    #[test]
    fn ownership_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audio.lock");
        let first = Ownership::acquire(&path).unwrap();
        assert!(Ownership::acquire(&path).is_err());
        drop(first);
        assert!(Ownership::acquire(&path).is_ok());
    }
}
