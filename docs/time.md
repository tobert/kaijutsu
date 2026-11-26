# Time and Pulse Coordination in Kaijutsu

Kaijutsu's timing and pulse coordination substrate (`hyoushigi`) unifies coding turns, musical composition, and real-time audio generation into a single continuous-time architecture. This document defines the authoritative rules for timebases, anticipation, commitment, and track score contexts.

---

## 1. The One Timebase

Kaijutsu enforces a single unified clock and beat grid across all execution tracks.

- **Scheduled-Periodic Kernel Beat Grid**: The canonical beat source is driven by the kernel beat scheduler, parameterized by `metronome.toml`. The grid produces discrete ticks at a designated tempo and meter (e.g., 120 BPM, 4/4 time).
- **Emission Wall-Clock Stamps**: Every clock emission and cue event is stamped with a precise wall-clock time (`Instant`) alongside its logical tick number. Downstream components rely on these stamps to calculate latency and drift.
- **Sink Back-Dating**: When events arrive from external hardware or asynchronous network sources, sinks apply back-dating based on measured offset and hardware buffer latency to align incoming data with the exact logical beat position.
- **Stale Event Recognition & Rejection**: Observations or inputs arriving after their scheduled target tick window have closed are unconditionally rejected. Advancing the clock across old deadlines never replays elapsed beats.
- **No Replaying Missed Beats**: If the kernel or an attached context stalls, missed beats are never replayed upon recovery. The clock jumps to the current live position, and any intervening spans fall back according to their declared policies.
- **Local Phasor Free-Running**: Local oscillators or phasor generators free-run within a tight deadband to maintain smooth sample interpolation between kernel beat ticks, preventing jitter during micro-timing fluctuations.

---

## 2. Anticipation and Commitment (*Hyoushigi* / 拍子木)

The *Hyoushigi* engine coordinates producers running at vastly different speeds on the shared pulse.

### Resolver Contract
Every producer implements the `Resolver` trait:
- `estimate_cost`: Returns the expected wall-clock duration to compute the result.
- `compute_basis`: Returns a cryptographic hash representing the contextual equivalence class of the inputs.
- `resolve`: Returns an owned, `Send`, static asynchronous future producing the content.

**Critical Rule**: Resolver start and poll execution on the beat path must **never block**. Slow work (such as model generation or CAS fetch operations) runs behind bounded admission queues.

### Basis Validity, Readiness, and Commitment
1. **Speculation**: At `speculate_at` (derived from intended start minus lead time), the resolver snapshots its input basis and starts asynchronous computation.
2. **Readiness**: Results must be ready before the commit deadline. A result observed only after its intended start is invalid and discarded.
3. **Commitment at Target Tick**: At the commit deadline, the current context basis is recomputed. If it matches the speculated basis, the cell commits.
4. **Fallback Handling**: If the basis diverges, or if the resolver fails or misses its deadline, the cell applies its declared fallback:
   - `last-good`: Repeats the latest accepted content from the same performer on the same track.
   - `skip`: Leaves the span silent.
   - `literal`: Commits a statically supplied content reference.
5. **Driver Command**: `kj drive --score-at <tick> --track <track> --fallback last-good|skip` forces manual score evaluation or fallback intervention at specific tick boundaries.

### Observable Work History
- **`kj transport work --track <name>`**: Emits a structured JSON record of current work items and the most recent 256 terminal dispositions.
- **Rolling Latency Estimates**: Each work item tracks 32-sample rolling latency estimates, capturing queue wait times and compute intervals to refine future lead times automatically.

---

## 3. Tracks and Score Contexts

Tracks serve as isolated clock domains and execution lanes within the kaijutsu engine.

- **Tracks Table & Attachments**: The `tracks` table maps track identifiers to active performer contexts, tempo multipliers, and output routing attachments.
- **Durable score contexts (`score-<track>`)** hold the track's committed phrases. Attaching the first musician creates the score. It stays empty until a cell commits. An ABC phrase writes a source block with the notation and a linked MIDI block at the same tick. Both survive stopping the track.
- **RenderCue Wire Contract**: Communication across the beat wire uses mime-keyed cues referencing Content Addressable Storage (CAS) hashes. **Raw media bytes never ride the beat wire.** CAS resolution happens out-of-band via content hash lookup.
- **Transport Control**: `kj transport` and `kj play` manage global playhead states, transport synchronization, and track-level arming for playback and recording.

To watch a musician in Bevy, run `kj transport list` and open the context
in the SCORE column with `kj context switch <score-id>`. Conversation
engraves the ABC source inline. Scripts can read the full score context ID and
playhead with `kj transport list --details --json`. The musician's own context holds its
instructions and turns. While waiting for the first staff, use
`kj transport work --track <track>` to see whether a phrase has started,
is ready, committed, or fell back.
