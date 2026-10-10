# mk admission: 429 busy and the listen backlog

The megakernel service (`http://100.83.138.103:8090/mk/v1`, zorak) refuses work it can't take now, instead of
letting it queue silently. Code: `service/http.py` (`Server.admit`, `Server.release`, `request_queue_size`),
commit `71bca56`, deployed 2026-10-10 13:53.

## What a client sees

- **Limit:** 16 POST/PUT requests in progress at once (`Server.max_requests`). Requests past that get:
  - status `429`
  - header `Retry-After: 1`
  - body `{"error": {"code": 429, "message": "...", "type": "busy"}}`
  - the connection closes after the answer (`Connection: close`)
- **Nothing ran.** A 429 is refused before any work starts, so it is always safe to retry the same request.
- **Not gated:** GET routes (health, status, openapi) and DELETE. A 429 never comes from them.
- **Held for the whole request.** A streamed `/generate` keeps its slot until its stream ends. So 16 open streams
  means the 17th POST gets a 429, even if none of the 16 is doing anything at that moment.
- **Errors close the connection.** Every error the front sends with `Connection: close` (400, 411, 413, 429) tells
  the client to open a new connection for its next request. Keep-alive clients should honor this.

## What to do

- Retry a 429 after `Retry-After` seconds (1 s). Back off by a few seconds per retry if it keeps happening; do not
  retry in a tight loop.
- A burst of up to 16 in-flight POSTs is served normally. Those requests still take their turn at the GPU, so
  latency rises under load; that is the expected path, not a refusal.
- Don't count 429 as a failure of the request: the request never ran, and the same request can be sent again.

## The listen backlog

The listener's accept queue is 128 (it was Python's default, 5). A burst of connections that arrives while the
accept loop is busy now waits in the kernel instead of being dropped. Dropped connections showed up as stalls on the
client with no error on either side. Clients may still see a stall if the service is restarting.

## Restart and the reattach

`make restart` on zorak reattaches to the weights systemd keeps (a few seconds). A request in flight during the
restart fails with a connection error; retry it.

## Open

- The limit is 16. It is a knob, not part of the API; the 429 is the contract. Tell us if real bursts need more.
- Not yet measured under real load from kaijutsu. The kaijutsu-side rate limit and its 429 handling are designed
  separately (see kaijutsu's council contract, `council-openapi.json`, which already lists 429 `busy`).
