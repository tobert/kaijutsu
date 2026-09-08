# Image display over SFTP CAS — primer

**Status:** design, no code yet. Waiting for other work to settle; pick up after.
**Goal:** display image blocks in the app by pulling bytes down over SFTP `/v/cas`
and uploading them as a Bevy texture — reusing the audio CAS-fetch path — instead of
shipping image bytes over RPC.

## Why this is a small job, not a new subsystem

Images already travel content-addressed end to end. Nothing about the block/wire path
changes; we're only finishing the client-side hash → bytes → texture step that was
stubbed out before `/v/cas` existed.

- Image block wire payload = **just the 32-hex CAS hash** in `BlockSnapshot.content`,
  plus a MIME hint in `contentType`. No bytes, no thumbnail on the block.
  `kaijutsu.capnp:135` (`BlockSnapshot`), `:154` (`contentType`); block semantics
  `kaijutsu-types/src/block.rs`.
- Kernel stores bytes into CAS and keeps only the hash string as content:
  `img_block`/`img_block_from_path` MCP tools
  (`crates/kaijutsu-kernel/src/mcp/servers/block.rs:811`, `:823`); `kj cas put`
  `kj/cas.rs`.
- App already **detects** image blocks and reserves the render slot:
  `ContentType::Image` → `RichContentKind::Image { hash }`
  `crates/kaijutsu-app/src/text/rich.rs:627-631`.
- App render is a **placeholder today** — a dark rect (`IMAGE_PLACEHOLDER_COLOR`,
  `crates/kaijutsu-app/src/view/block_render.rs:155`) with no decode pipeline behind it.

## No stub comment to route around

An earlier draft of this doc warned against a stale `block_render.rs` comment
that said the pipeline "requires `RpcCommand::CasRead` and async image
loading." That comment is gone now — the current one just names the
placeholder and points at `RichContentKind::Image`. The warning still
matters as a design rule even with nothing left to correct: **do not add a
`CasRead` RPC** — that would push image bytes back onto the RPC channel,
exactly what CAS-by-hash avoids. Use the SFTP resolver instead.

## Machinery that already exists (reuse, don't rebuild)

Server side (`/v/cas`):
- `CasFs` read-only VFS backend, sharded `<ab>/<full-hash>` paths, 256 KiB read window,
  `IMMUTABLE_GENERATION = 1`: `crates/kaijutsu-kernel/src/vfs/backends/cas.rs`.
- Mounted at `/v/cas` by the server: `crates/kaijutsu-server/src/rpc.rs:2645-2646`.
- SFTP subsystem bridges russh-sftp onto the MountTable: `kaijutsu-server/src/sftp.rs`.

Client side (fetch + cache):
- `SftpClient` + `BlobResolver<F: BlobFetch>` — XDG CAS cache
  (`$XDG_CACHE_HOME/kaijutsu/cas`), single-flight per hash, self-verifying (re-hash
  fetched bytes, reject on mismatch), `NotFound` distinct from transport errors:
  `crates/kaijutsu-client/src/sftp.rs`.
- Live e2e round trip through the real mount:
  `crates/kaijutsu-server/tests/sftp_transport.rs`.

App side (the template — audio only today, and since moved crates):
- `CasPrefetch` (a DJ-thread-owned **Send** SFTP world, not a Bevy resource)
  dispatches `CuePayload::Cas(hash)` resolves off-thread; outcomes land on the
  DJ thread's `select!` via a `tokio::mpsc` channel rather than a per-frame
  drain: `crates/kaijutsu-audio-runtime/src/dj/prefetch.rs:73` — this module
  lived at `kaijutsu-app/src/dj/prefetch.rs` before the audio DJ moved into
  its own crate; re-check the path if it has moved again.
  Note **why** it owns a separate single-worker tokio runtime: SFTP futures are `Send`,
  but the DJ thread's own runtime is current-thread, so we can't resolve on it
  (same file, module doc). Lazy connect + drop-transport-on-error redial:
  `resolve_with_lazy_connect`, `dj/prefetch.rs:150`.

CAS hash facts (for the decode/cache step):
- BLAKE3 truncated to 16 bytes → 32 hex chars; `prefix()` (`hash.rs:52`) = first 2,
  `remainder()` (`hash.rs:56`) = last 30.

## Work to do (3 pieces)

1. **Generalize `CasPrefetch` from audio-only to any CAS consumer.**
   One fetch resource keyed by hash with a payload tag (`AudioCue` vs `ImageTexture`) —
   **not** a second tokio runtime for images. This is the one design decision worth
   making deliberately. Open question deferred until we have two concrete consumers:
   whether to generalize first or implement the image path against the existing
   audio-only resource and generalize after. Current lean: **implement image path
   first, generalize once two consumers exist** (bring two implementations to the
   abstraction, per our usual practice).

2. **Decode → `Handle<Image>`.**
   On the worker thread (decode is CPU work, keep it off the frame drain): bytes →
   `Image::from_buffer` (let the `image` crate sniff format; block's `image/png` is only
   a fallback — real MIME lives in the CAS sidecar) → hand back to the drain →
   `assets.add()` → attach an `ImageNode` on the block entity, replacing the placeholder
   rect (`IMAGE_PLACEHOLDER_COLOR`, `block_render.rs:155`).

3. **Cache the texture handle by hash.**
   A `HashMap<ContentHash, Handle<Image>>` so re-display is free and one image shared
   across blocks decodes once. This is trivially correct **because** `CasFs` reports
   `IMMUTABLE_GENERATION = 1` and the XDG cache never invalidates — the immutability we
   already committed to buys the cache.

## Known caveats / deferred

- Whole-object read into memory (`BlobFetch::fetch` returns `Vec<u8>`,
  `crates/kaijutsu-client/src/sftp.rs:161-166`) — fine for typical images, not for
  huge ones. Streaming decode is the same deferred item audio has.
- `SftpClient` dials its own SSH connection today, not multiplexed onto the RPC channel
  (`SftpClient::connect`, `sftp.rs:119`) — deferred optimization, unchanged by this work.
- Fetch-on-demand only (audio does fetch-on-cue); no prefetch horizon yet. Images can
  start the same way (fetch when the block scrolls into view / first render).

## Related docs

`docs/slash-v.md` (Track B, the `/v/cas` design), `docs/pcm.md`,
`docs/sftp.md`.
