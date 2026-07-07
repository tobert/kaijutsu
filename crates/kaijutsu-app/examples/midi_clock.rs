//! Virtual MIDI clock master — the M3 dev-loop sender (`docs/midi.md`:
//! "a virtual MIDI clock on zorak — software ALSA clock source — no network
//! at all for the first slices").
//!
//! Emits `FA` (Start) then a 24-PPQN `F8` pulse train on an ALSA seq source
//! port, with optional linear tempo drift and per-pulse uniform jitter — the
//! exact signal shapes the `ClockEstimator` tests synthesize, now on a real
//! bus. The app's ear auto-subscribes via System Announce (the client name
//! is NOT one of the excluded ones), the capture-thread tap locks, and the
//! estimate stream flows kernel-ward.
//!
//! ```sh
//! cargo run -p kaijutsu-app --example midi_clock -- --bpm 120 --drift 1.0 --jitter-ms 2
//! ```
//!
//! `--drift` is BPM per minute (linear ramp, the accelerando the EMA must
//! track); `--jitter-ms` is uniform ± on each pulse's send time (the WiFi-ish
//! noise the ratio filter must reject). Runs until killed; `FC` (Stop) is
//! deliberately not sent on exit so a stall exercises the starvation path.

use std::time::{Duration, Instant};

fn arg(name: &str) -> Option<f64> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}

/// Tiny deterministic xorshift so the example needs no rand dep.
struct XorShift(u64);
impl XorShift {
    /// Uniform in [-1.0, 1.0).
    fn pm_one(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 52) as f64 * 2.0 - 1.0
    }
}

fn main() -> Result<(), String> {
    use alsa::seq::{Event, EventType, EvQueueControl, PortCap, PortType};
    use std::ffi::CString;

    let bpm = arg("--bpm").unwrap_or(120.0);
    let drift_bpm_per_min = arg("--drift").unwrap_or(0.0);
    let jitter_ms = arg("--jitter-ms").unwrap_or(0.0);
    if !(bpm > 0.0) {
        return Err("--bpm must be positive".into());
    }

    let map = |e: alsa::Error| format!("{e}");
    let seq = alsa::Seq::open(None, None, false).map_err(map)?;
    seq.set_client_name(&CString::new("kj-virtual-clock").unwrap()).map_err(map)?;
    let port = seq
        .create_simple_port(
            &CString::new("clock").unwrap(),
            PortCap::READ | PortCap::SUBS_READ,
            PortType::MIDI_GENERIC | PortType::APPLICATION,
        )
        .map_err(map)?;
    println!(
        "kj-virtual-clock on {}:{} — {bpm} BPM, drift {drift_bpm_per_min} BPM/min, jitter ±{jitter_ms} ms",
        seq.client_id().map_err(map)?,
        port
    );

    let send = |etype: EventType| -> Result<(), String> {
        let mut ev = Event::new(etype, &EvQueueControl::<()> { queue: 0, value: () });
        ev.set_source(port);
        ev.set_subs();
        ev.set_direct();
        seq.event_output(&mut ev).map_err(map)?;
        seq.drain_output().map_err(map)?;
        Ok(())
    };

    // Give the ear's announce-driven subscribe a beat to land, then Start.
    std::thread::sleep(Duration::from_millis(300));
    send(EventType::Start)?;

    let t0 = Instant::now();
    let mut rng = XorShift(0x6b61_696a_7574_7375); // "kaijutsu"
    let mut pulse: u64 = 0;
    // Ideal (jitter-free) send time of the NEXT pulse, tracked in seconds so
    // drift integrates cleanly; jitter perturbs each send, never the grid.
    let mut ideal_s = 0.0f64;
    loop {
        let minutes = ideal_s / 60.0;
        let current_bpm = (bpm + drift_bpm_per_min * minutes).max(1.0);
        ideal_s += 60.0 / current_bpm / 24.0;
        let jitter_s = rng.pm_one() * jitter_ms / 1_000.0;
        let target = t0 + Duration::from_secs_f64((ideal_s + jitter_s).max(0.0));
        if let Some(wait) = target.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        send(EventType::Clock)?;
        pulse += 1;
        if pulse % (24 * 16) == 0 {
            println!("beat {} at {current_bpm:.2} BPM", pulse / 24);
        }
    }
}
