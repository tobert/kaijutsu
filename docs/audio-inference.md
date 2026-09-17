# Offline audio inference: cost and placement

Evaluation as of September 17, 2026. Execution still lives in the kernel;
placement in lfm2d or another process is undecided. Amy's scope: "for now
we'll evaluate kaijutsu's tradeoffs, and decide later about the new home."

## Current implementation

`kj audio beats <host-path>` loads a mel spectrogram graph and a beat graph
for each invocation. `beat-this` 1.0.0 uses RTen 0.24.0 for CPU inference,
Symphonia for decoding, and Rubato for resampling. These are unconditional
kernel dependencies. There is no `ort`/`ort-sys` dependency or external
ONNX Runtime library. Semantic embeddings already use the lfm2d service.

The graphs are not loaded at kernel startup and there is no resident model
cache. The installed full beat graph is 83,162,650 bytes (79.3 MiB); the
small graph is 10,555,592 bytes (10.1 MiB); the mel graph is 270,742 bytes
(0.26 MiB). The command prefers full, falls back to small when full is
absent, and reports the selection. It has no model-selection option.

`crates/kaijutsu-kernel/src/kj/audio.rs` runs loading and analysis through
`spawn_blocking`. It has no analysis-specific concurrency limit, decoded
sample limit, memory budget, or inference cancellation mechanism. Once
started, this synchronous work survives dropping its async waiter; normal
Tokio runtime shutdown waits for it. This follows the locked Tokio 1.52.3
source and the documented [blocking-task contract](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html).

Inputs are host paths, not VFS paths or CAS references. Results contain the
path, model family, BPM, and beat/downbeat timestamps in seconds. They do
not identify the audio content hash or the model weight digest.

## Measurements

Optimized standalone Rust probes linked the existing release build of the
locked `beat-this` dependency on an AMD Ryzen AI MAX+ 395 (16 physical,
32 logical cores). Fixtures were synthetic 120 BPM kick/hat/bass loops,
44.1 kHz stereo PCM16 WAV, lasting 10 or 180 seconds. These measure cost,
not beat/downbeat accuracy on music.

Each process loaded graphs, decoded/resampled the file, analyzed it, then
analyzed the same decoded samples again with the same graphs. The table
shows medians of three separate processes per case. First analysis includes
graph loading and decoding; warm analysis excludes both. Peak RSS covers
the whole process, including both analyses. OS page cache was not flushed.

| Beat model | Audio duration | First analysis | Warm analysis | Peak RSS |
|---|---:|---:|---:|---:|
| Full | 10 s | 174 ms | 100 ms | 177 MiB |
| Small | 10 s | 97 ms | 56 ms | 108 MiB |
| Full | 180 s | 2.32 s | 2.10 s | 564 MiB |
| Small | 180 s | 1.60 s | 1.43 s | 495 MiB |

RTen's default global pool created 16 worker threads. Setting
`RTEN_NUM_THREADS=4` only in the probe processes changed full-model 180 s
first analysis to 3.00 s, warm analysis to 2.85 s, and peak RSS to 549 MiB.
Median process CPU time (user plus system, **both** analyses) fell from
32.01 to 16.66 CPU-seconds. Full-model 10 s first analysis improved to
141 ms. This small sample supports testing a CPU budget, not selecting a
universal thread count. No server configuration was changed.

A separate probe used `BeatThis::new` followed by `analyze_file`, once per
job, sharing RTen's global pool in one process. One 180 s full-model job
took 2.36 s and peaked at 564 MiB. Two simultaneous jobs completed in
3.49 s and peaked at 1,407 MiB (1.37 GiB). Each case ran once; this shows
the cost of overlap, not a concurrency scaling curve.

Memory is more than model weights. The library decodes the whole file,
converts it to mono, resamples it, and computes the whole mel spectrogram
before the predictor processes approximately 30-second chunks. Smaller
weights do not eliminate those buffers. After dropping graphs and audio,
the sequential 180 s probes still had about 152 MiB RSS; after the two-job
probe, about 627 MiB. Allocator and global thread-pool retention can keep
memory resident. These snapshots do not establish a leak or the eventual
steady state of a long-lived kernel.

Graph loading alone took roughly 34–41 ms for full and 17–19 ms for small
at the default thread count. Caching graphs could help repeated short
requests, but would address little of a full track's inference time.

Raw probes, logs, summaries, and input/model hashes are retained in the
project's private working memory under
`~/exomemory/kaijutsu/measurements/2026-09-17-beat-this/`.
These are library measurements, not RPC latency or deployed-server load
measurements. GPU execution, real music accuracy, kernel responsiveness,
audio deadline misses, and binary/build-size differences were not measured.

## Placement tradeoffs

Occasional offline analysis is fast on this host. Keeping it in process
currently avoids another service, transfer protocol, and availability
dependency. The cost is shared memory and CPU pressure, a larger kernel
dependency graph, and an inference lifetime that command cancellation does
not bound. Read-only effect classification does not make this work cheap.

A separate analysis process could isolate memory exhaustion and inference
process crashes, enforce CPU/memory limits, and provide a replaceable model cache
or accelerator backend. It also needs media transfer or resolution, queue
and failure semantics, and result validation. It does not remove the cost
of inference; it changes which process bears it. A dedicated thread alone
would not provide that fault or memory isolation.

The proposed boundary separates three responsibilities:

| Owner | Responsibility |
|---|---|
| Kernel | Accept intent and identity; retain media references and analysis provenance; sequence accepted music state; map analysis timestamps into clips, tracks, and transport decisions. |
| Analysis executor | Decode and resample; choose and load models; bound queued/running work and input size; own compute budgets, caching, cancellation, and failure reporting. |
| Audio daemon | Own devices, capture buffers, sample timing, and scheduled physical playback. |

This is a proposed analysis interface, not an implemented service contract.
`kj audio beats` can remain the public verb under either implementation.
An executor could initially remain in process; a process boundary would be
needed for hard termination of inference that cannot cooperate with cancel.
Whether lfm2d should implement that interface is a separate decision.

Before expanding music inference, define admission limits, oversized-input
refusal, and what cancellation promises. Before relocating it, evaluate
representative music, repeated calls, overlapping kernel work, and immutable
media/model identities. A live audio model would need a separate latency
and buffering contract; this offline tracker does not establish one.
