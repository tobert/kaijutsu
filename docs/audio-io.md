# Audio and MIDI Hardware I/O in Kaijutsu

This document defines the authoritative architecture for hardware audio and MIDI input/output, daemon communication, PCM rendering, and model workload placement within the Kaijutsu system.

---

## 1. Separation of Concerns

Kaijutsu maintains a strict architectural boundary between kernel coordination and hardware FFI:
- **Kernel (`kaijutsu-kernel`)**: Owns timing, score tracks, hyoushigi anticipation scheduling, and state persistence. It never links directly to audio drivers or hardware FFI libraries.
- **Audio Daemon (`kaijutsu-audio-daemon` / `audiod`)**: A dedicated companion service that owns local ALSA/PipeWire PCM streams, audio buffer management, and physical MIDI hardware I/O. Communication between the kernel and `audiod` occurs over structured IPC.

---

## 2. Audio Daemon & Nodes

- **Node Registration**: Audio and MIDI hardware interfaces register as active nodes with `audiod` upon startup, exposing capabilities, channel counts, and sample rates.
- **Clock Offset Tracking**: `audiod` continuously measures and tracks clock drift and offset relative to the kernel's central timebase, applying micro-adjustments to prevent buffer underruns or overruns.
- **Keep Pools**: Audio buffer memory pools are pre-allocated and pinned to avoid allocation overhead during real-time audio callback execution.
- **`kj audio`**: The administrative CLI command used to inspect audio node status, routing graphs, buffer latencies, and device health.

---

## 3. PCM Samples & Clips

- **Audio/WAV Rendering**: Rendered audio payloads adhere strictly to the `audio/wav` format, delivered across the mime-keyed cue seam as CAS content hashes.
- **CAS Pre-Fetching**: Downstream audio nodes and daemons pre-fetch CAS content blocks prior to target playback ticks, ensuring zero disk or network I/O latency on the audio critical path.
- **`kj play`**: Coordinates synchronous audio clip triggering across multiple tracks, binding score timeline ticks directly to sample playback queues in `audiod`.

---

## 4. MIDI Devices & Profiles

- **Device Profiles (`/config/midi/`)**: Hardware MIDI controllers, synthesizers, and virtual ports are governed by declarative configuration profiles located in `/config/midi/`.
- **Port Roles**: Profiles define explicit port roles (e.g., clock master, performance keyboard, control surface, synth output) to automate routing upon device discovery.
- **`kj midi`**: The management interface for inspecting active MIDI connections, routing patches, monitor streams, and device profile bindings.

---

## 5. Audio Inference & Model Placement

- **Real-Time Constraints**: Real-time audio synthesis and low-latency MIDI generation demand dedicated CPU affinity and strict execution bounds.
- **Model Placement Rules**: Heavy neural audio models or speech generation tasks cannot run on the real-time audio callback thread. They operate as asynchronous producers within the hyoushigi anticipation pipeline, utilizing pre-computed lead times and CAS caching to bridge the gap between inference latency and hardware DAC deadlines.
