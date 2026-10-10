//! How hard the kernel presses each model endpoint.
//!
//! An endpoint is a base URL's origin: scheme, host, and port. A backend
//! with no base URL is its own endpoint. Every outbound model request takes
//! a [`Slot`] from its endpoint first, and the slot is released when the
//! request ends. A streamed reply holds its slot until the stream ends.
//!
//! Each endpoint has a concurrency limit, the smallest `max_concurrent` of
//! the backends that name it, and none when no backend sets one. A 429 or 503
//! answer starts a cooldown for the whole endpoint: `Retry-After` when the
//! answer carries one, else [`FIRST_COOLDOWN`] doubled on each busy answer in
//! a row up to [`MAX_COOLDOWN`]. A 2xx answer resets the doubling. No new
//! request starts during a cooldown.
//!
//! A caller waits for a slot until its own deadline. The limiter never
//! retries a request. See `docs/retries-and-ratelimits.md`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use super::config::BackendConfig;
use super::{LlmError, LlmResult};

/// The cooldown after the first busy answer in a row with no `Retry-After`.
pub const FIRST_COOLDOWN: Duration = Duration::from_secs(5);

/// The longest cooldown a run of busy answers with no `Retry-After` reaches.
pub const MAX_COOLDOWN: Duration = Duration::from_secs(60);

/// The endpoint a backend's requests go to: its base URL's origin, or
/// `hosted backend <name>` when it has no base URL.
pub fn endpoint_key(name: &str, base_url: Option<&str>) -> Result<String, String> {
    match base_url.map(str::trim).filter(|u| !u.is_empty()) {
        Some(url) => origin(url),
        None => Ok(format!("hosted backend {name}")),
    }
}

/// `scheme://host:port` for `url`, with the scheme's default port written
/// out, so two spellings of one server name one endpoint.
pub fn origin(url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("{url} is not a URL: {e}"))?;
    let host = parsed.host_str().ok_or_else(|| format!("{url} names no host"))?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| format!("{url} names no port, and {}: has no default port", parsed.scheme()))?;
    Ok(format!("{}://{host}:{port}", parsed.scheme()))
}

/// The `Retry-After` header, when it holds whole seconds.
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Takes a slot at `endpoint` for one model request, waiting up to `wait`,
/// and records the wait on the current span as `llm.slot_wait_ms`. No
/// endpoint takes no slot. A wait that runs out fails with `RateLimited`,
/// naming the endpoint and why it had no slot.
pub(crate) async fn take_for_request(endpoint: Option<&Arc<Endpoint>>, wait: Duration) -> LlmResult<Option<Slot>> {
    let Some(endpoint) = endpoint else { return Ok(None) };
    match endpoint.acquire(Instant::now() + wait).await {
        Ok(slot) => {
            tracing::Span::current().record("llm.slot_wait_ms", slot.waited().as_millis() as u64);
            Ok(Some(slot))
        }
        Err(no_slot) => Err(LlmError::RateLimited(no_slot.to_string())),
    }
}

/// One megakernel call through `endpoint`: takes a slot, noting the wait in
/// `waits`, makes the call, and tells the endpoint how it answered. A 429 is
/// refused before any work starts, so the same call goes out again once the
/// cooldown it started ends (`docs/mk-admission.md`). Any other answer,
/// a 503 included, is returned with the slot it was made under. Every wait
/// ends at `deadline`.
pub async fn mk_call<T, F, Fut>(
    endpoint: &Arc<Endpoint>,
    deadline: Instant,
    waits: &SlotWaits,
    mut call: F,
) -> Result<(Slot, Result<T, kaijutsu_mk::MkError>), NoSlot>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, kaijutsu_mk::MkError>>,
{
    use kaijutsu_mk::MkError;
    loop {
        let slot = endpoint.acquire_noting(deadline, waits).await?;
        let result = call().await;
        slot.answered_mk(&result);
        match &result {
            Err(MkError::Status { status: 429, .. } | MkError::Service { status: 429, .. }) => {
                tracing::debug!(endpoint = %endpoint.key, "megakernel refused the call with 429; sending it again after the cooldown");
            }
            _ => return Ok((slot, result)),
        }
    }
}

/// Tells `slot`'s endpoint how it answered `response`.
pub(crate) fn observe(slot: Option<&Slot>, response: &reqwest::Response) {
    if let Some(slot) = slot {
        slot.answered_with(response.status().as_u16(), retry_after(response.headers()));
    }
}

/// The cooldown for the `n`th busy answer in a row with no `Retry-After`.
fn backoff(n: u32) -> Duration {
    let doublings = n.saturating_sub(1).min(16);
    FIRST_COOLDOWN.saturating_mul(1 << doublings).min(MAX_COOLDOWN)
}

/// Every endpoint the kernel has sent to or been configured for. One per
/// kernel; it outlives registry rebuilds, so a slot taken before a `kj
/// backend set` is released to the same endpoint after it.
#[derive(Default)]
pub struct Endpoints {
    by_key: parking_lot::Mutex<HashMap<String, Arc<Endpoint>>>,
}

impl Endpoints {
    /// The endpoint named `key`, created unlimited when it is new.
    pub fn get(&self, key: &str) -> Arc<Endpoint> {
        self.by_key
            .lock()
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Endpoint::new(key)))
            .clone()
    }

    /// The endpoint `url` addresses.
    pub fn for_url(&self, url: &str) -> Result<Arc<Endpoint>, String> {
        Ok(self.get(&origin(url)?))
    }

    /// The endpoint `backend`'s requests go to.
    pub fn for_backend(&self, backend: &BackendConfig) -> Result<Arc<Endpoint>, String> {
        Ok(self.get(&endpoint_key(&backend.name, backend.base_url.as_deref())?))
    }

    /// Sets each endpoint's limit from the backends that name it: the
    /// smallest `max_concurrent` among them, or none. An endpoint no backend
    /// names becomes unlimited. In-flight slots and cooldowns carry over.
    pub fn configure(&self, backends: &[BackendConfig]) {
        let mut named: HashMap<String, Vec<(String, Option<u32>)>> = HashMap::new();
        for backend in backends {
            match endpoint_key(&backend.name, backend.base_url.as_deref()) {
                Ok(key) => named.entry(key).or_default().push((backend.name.clone(), backend.max_concurrent)),
                Err(e) => {
                    tracing::warn!(backend = %backend.name, error = %e, "backend has no endpoint; its limit is not applied")
                }
            }
        }
        let endpoints: Vec<Arc<Endpoint>> = {
            let mut by_key = self.by_key.lock();
            for key in named.keys() {
                by_key.entry(key.clone()).or_insert_with(|| Arc::new(Endpoint::new(key)));
            }
            by_key.values().cloned().collect()
        };
        for endpoint in endpoints {
            let mut backends = named.remove(&endpoint.key).unwrap_or_default();
            backends.sort();
            endpoint.set_backends(backends);
        }
    }

    /// Every endpoint, by key.
    pub fn statuses(&self) -> Vec<EndpointStatus> {
        let endpoints: Vec<Arc<Endpoint>> = self.by_key.lock().values().cloned().collect();
        let mut out: Vec<EndpointStatus> = endpoints.iter().map(|e| e.status()).collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }
}

#[derive(Clone, Copy, Debug)]
struct Cooldown {
    until: Instant,
    status: u16,
    length: Duration,
}

#[derive(Default)]
struct State {
    limit: Option<u32>,
    /// Each backend that names this endpoint, with its own limit.
    backends: Vec<(String, Option<u32>)>,
    in_flight: u32,
    cooldown: Option<Cooldown>,
    busy_in_a_row: u32,
}

impl State {
    fn cooling(&self, now: Instant) -> Option<Cooldown> {
        self.cooldown.filter(|c| c.until > now)
    }
}

/// One endpoint's limit, in-flight count, and cooldown.
pub struct Endpoint {
    key: String,
    state: parking_lot::Mutex<State>,
    /// Fires when a slot is released, the limit changes, or a cooldown
    /// starts or ends.
    changed: Notify,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("key", &self.key).finish_non_exhaustive()
    }
}

/// What `kj backend show` reports about an endpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct EndpointStatus {
    pub key: String,
    /// `None` is unlimited.
    pub limit: Option<u32>,
    /// Each backend that names this endpoint, with its own limit, by name.
    pub backends: Vec<(String, Option<u32>)>,
    pub in_flight: u32,
    /// The time left and the status that started it, while cooling down.
    pub cooldown: Option<(Duration, u16)>,
}

/// A caller could not get a slot before its deadline.
#[derive(Clone, Debug)]
pub struct NoSlot {
    pub endpoint: String,
    pub waited: Duration,
    /// Why the endpoint had no slot when the wait ended.
    pub why: String,
}

impl std::fmt::Display for NoSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no slot at {} after waiting {} ms: {}", self.endpoint, self.waited.as_millis(), self.why)
    }
}

impl Endpoint {
    fn new(key: &str) -> Self {
        Endpoint { key: key.to_string(), state: parking_lot::Mutex::default(), changed: Notify::new() }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    fn set_backends(&self, backends: Vec<(String, Option<u32>)>) {
        let limit = backends.iter().filter_map(|(_, l)| *l).min();
        {
            let mut state = self.state.lock();
            state.limit = limit;
            state.backends = backends;
        }
        self.changed.notify_waiters();
    }

    /// Takes a slot, waiting while the endpoint is at its limit or cooling
    /// down, until `deadline`.
    pub async fn acquire(self: &Arc<Self>, deadline: Instant) -> Result<Slot, NoSlot> {
        self.acquire_noting(deadline, &SlotWaits::default()).await
    }

    /// [`Self::acquire`], adding the wait to `waits`. A wait cut short by the
    /// caller's own timeout stays visible in `waits`.
    pub async fn acquire_noting(self: &Arc<Self>, deadline: Instant, waits: &SlotWaits) -> Result<Slot, NoSlot> {
        let started = Instant::now();
        waits.begin(started, &self.key);
        let result = self.wait_for_slot(started, deadline).await;
        waits.end(started.elapsed());
        result
    }

    async fn wait_for_slot(self: &Arc<Self>, started: Instant, deadline: Instant) -> Result<Slot, NoSlot> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let now = Instant::now();
            let wake = {
                let mut state = self.state.lock();
                match state.cooling(now) {
                    Some(cooldown) => Some(cooldown.until),
                    None if state.limit.is_none_or(|limit| state.in_flight < limit) => {
                        state.in_flight += 1;
                        return Ok(Slot { endpoint: self.clone(), waited: now - started });
                    }
                    None => None,
                }
            };
            if now >= deadline {
                return Err(NoSlot { endpoint: self.key.clone(), waited: now - started, why: self.why_waiting(now) });
            }
            let until = wake.map_or(deadline, |w| w.min(deadline));
            tokio::select! {
                _ = changed.as_mut() => {}
                _ = tokio::time::sleep_until(until) => {}
            }
        }
    }

    fn why_waiting(&self, now: Instant) -> String {
        let state = self.state.lock();
        match state.cooling(now) {
            Some(c) => format!(
                "cooling down for {} ms more after a {} answer",
                (c.until - now).as_millis(),
                c.status
            ),
            None => format!(
                "{} of {} slots in flight",
                state.in_flight,
                state.limit.map_or_else(|| "unlimited".to_string(), |l| l.to_string())
            ),
        }
    }

    pub fn status(&self) -> EndpointStatus {
        let now = Instant::now();
        let state = self.state.lock();
        EndpointStatus {
            key: self.key.clone(),
            limit: state.limit,
            backends: state.backends.clone(),
            in_flight: state.in_flight,
            cooldown: state.cooling(now).map(|c| (c.until - now, c.status)),
        }
    }

    fn release(&self) {
        {
            let mut state = self.state.lock();
            state.in_flight = state.in_flight.checked_sub(1).expect("a slot is released once");
        }
        self.changed.notify_waiters();
    }

    fn answered(&self) {
        self.state.lock().busy_in_a_row = 0;
    }

    fn busy(self: &Arc<Self>, status: u16, retry_after: Option<Duration>) {
        let now = Instant::now();
        let cooldown = {
            let mut state = self.state.lock();
            state.busy_in_a_row = state.busy_in_a_row.saturating_add(1);
            let length = retry_after.unwrap_or_else(|| backoff(state.busy_in_a_row));
            let until = now + length;
            if state.cooling(now).is_some_and(|c| c.until >= until) {
                return;
            }
            let cooldown = Cooldown { until, status, length };
            state.cooldown = Some(cooldown);
            cooldown
        };
        tracing::info!(
            endpoint = %self.key, status, cooldown_ms = cooldown.length.as_millis() as u64,
            "endpoint {} answered {status}; no new request starts for {} ms",
            self.key, cooldown.length.as_millis()
        );
        self.changed.notify_waiters();
        let endpoint = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(cooldown.until).await;
            let ended = {
                let mut state = endpoint.state.lock();
                let current = state.cooldown.is_some_and(|c| c.until == cooldown.until);
                if current {
                    state.cooldown = None;
                }
                current
            };
            if ended {
                tracing::info!(
                    endpoint = %endpoint.key, status = cooldown.status, cooldown_ms = cooldown.length.as_millis() as u64,
                    "endpoint {} cooldown after a {} answer ended after {} ms",
                    endpoint.key, cooldown.status, cooldown.length.as_millis()
                );
                endpoint.changed.notify_waiters();
            }
        });
    }
}

/// One request's place at an endpoint. Dropping it releases the slot.
pub struct Slot {
    endpoint: Arc<Endpoint>,
    waited: Duration,
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot").field("endpoint", &self.endpoint.key).field("waited", &self.waited).finish()
    }
}

impl Slot {
    /// How long the caller waited for this slot.
    pub fn waited(&self) -> Duration {
        self.waited
    }

    /// The endpoint answered with HTTP `status`: 429 and 503 start a
    /// cooldown, honoring `retry_after`; a 2xx resets the doubling.
    pub fn answered_with(&self, status: u16, retry_after: Option<Duration>) {
        match status {
            429 | 503 => self.endpoint.busy(status, retry_after),
            200..=299 => self.endpoint.answered(),
            _ => {}
        }
    }

    /// The endpoint answered a megakernel call with `result`.
    pub fn answered_mk<T>(&self, result: &Result<T, kaijutsu_mk::MkError>) {
        use kaijutsu_mk::MkError;
        match result {
            Ok(_) => self.endpoint.answered(),
            Err(e @ (MkError::Status { status, .. } | MkError::Service { status, .. })) => {
                self.answered_with(*status, e.retry_after())
            }
            Err(_) => {}
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.endpoint.release();
    }
}

/// The slot waits of one council decision or observation, so its record can
/// say how long it queued, including a wait its deadline cut short.
#[derive(Default)]
pub struct SlotWaits(parking_lot::Mutex<WaitState>);

#[derive(Default)]
struct WaitState {
    done: Duration,
    waiting: Option<(Instant, String)>,
}

impl SlotWaits {
    fn begin(&self, at: Instant, endpoint: &str) {
        self.0.lock().waiting = Some((at, endpoint.to_string()));
    }

    fn end(&self, waited: Duration) {
        let mut state = self.0.lock();
        state.done += waited;
        state.waiting = None;
    }

    /// Every wait so far, a wait still open included.
    pub fn total(&self) -> Duration {
        let state = self.0.lock();
        state.done + state.waiting.as_ref().map_or(Duration::ZERO, |(at, _)| at.elapsed())
    }

    /// The wait still open, as a miss cause names it, when one is.
    pub fn open(&self) -> Option<String> {
        let state = self.0.lock();
        state
            .waiting
            .as_ref()
            .map(|(at, endpoint)| format!("waiting {} ms for a slot at {endpoint}", at.elapsed().as_millis()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(name: &str, base_url: Option<&str>, max: Option<u32>) -> BackendConfig {
        let mut b = BackendConfig::new(name, crate::llm::BackendKind::OpenAi);
        b.base_url = base_url.map(str::to_string);
        b.max_concurrent = max;
        b
    }

    #[test]
    fn an_origin_is_scheme_host_and_port() {
        assert_eq!(origin("http://zorak:8090/v1").unwrap(), "http://zorak:8090");
        assert_eq!(origin("http://Zorak:8090").unwrap(), "http://zorak:8090");
        assert_eq!(origin("https://api.anthropic.com").unwrap(), "https://api.anthropic.com:443");
        assert_eq!(origin("https://api.anthropic.com:443/v1").unwrap(), "https://api.anthropic.com:443");
        assert_ne!(origin("http://zorak:8090").unwrap(), origin("http://zorak:8091").unwrap());
        assert_ne!(origin("http://zorak:8090").unwrap(), origin("https://zorak:8090").unwrap());
        assert!(origin("zorak").is_err());
        assert_eq!(endpoint_key("claude", None).unwrap(), "hosted backend claude");
        assert_eq!(endpoint_key("zorak", Some("http://zorak:8090/v1")).unwrap(), "http://zorak:8090");
    }

    #[test]
    fn backoff_doubles_from_five_seconds_to_sixty() {
        let secs: Vec<u64> = (1..=7).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [5, 10, 20, 40, 60, 60, 60]);
        assert_eq!(backoff(u32::MAX), MAX_COOLDOWN);
    }

    #[tokio::test(start_paused = true)]
    async fn a_limit_of_one_holds_a_second_request_until_the_first_ends() {
        let endpoints = Endpoints::default();
        endpoints.configure(&[backend("mk", Some("http://zorak:8090"), Some(1))]);
        let endpoint = endpoints.for_url("http://zorak:8090").unwrap();
        let first = endpoint.acquire(Instant::now()).await.unwrap();
        let waiter = endpoint.clone();
        let second = tokio::spawn(async move {
            let slot = waiter.acquire(Instant::now() + Duration::from_secs(60)).await.unwrap();
            slot.waited()
        });
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!second.is_finished(), "the second request started beside the first");
        assert_eq!(endpoint.status().in_flight, 1);
        drop(first);
        let waited = second.await.unwrap();
        assert!(waited >= Duration::from_secs(3), "waited {waited:?}");
        assert_eq!(endpoint.status().in_flight, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_past_its_deadline_names_the_endpoint_and_the_limit() {
        let endpoints = Endpoints::default();
        endpoints.configure(&[backend("mk", Some("http://zorak:8090"), Some(1))]);
        let endpoint = endpoints.for_url("http://zorak:8090").unwrap();
        let _held = endpoint.acquire(Instant::now()).await.unwrap();
        let waits = SlotWaits::default();
        let miss = endpoint.acquire_noting(Instant::now() + Duration::from_millis(700), &waits).await.unwrap_err();
        assert_eq!(miss.waited, Duration::from_millis(700));
        assert_eq!(miss.to_string(), "no slot at http://zorak:8090 after waiting 700 ms: 1 of 1 slots in flight");
        assert_eq!(waits.total(), Duration::from_millis(700));
        assert!(waits.open().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_cut_short_by_its_caller_stays_open_in_its_waits() {
        let endpoints = Endpoints::default();
        endpoints.configure(&[backend("mk", Some("http://zorak:8090"), Some(1))]);
        let endpoint = endpoints.for_url("http://zorak:8090").unwrap();
        let _held = endpoint.acquire(Instant::now()).await.unwrap();
        let waits = SlotWaits::default();
        let far = Instant::now() + Duration::from_secs(60);
        assert!(tokio::time::timeout(Duration::from_millis(400), endpoint.acquire_noting(far, &waits)).await.is_err());
        assert_eq!(waits.total(), Duration::from_millis(400));
        assert_eq!(waits.open().as_deref(), Some("waiting 400 ms for a slot at http://zorak:8090"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_after_delays_the_next_request_to_that_endpoint_only() {
        let endpoints = Endpoints::default();
        let busy = endpoints.for_url("http://zorak:8090").unwrap();
        let other = endpoints.for_url("http://zorak:8091").unwrap();
        busy.acquire(Instant::now()).await.unwrap().answered_with(429, Some(Duration::from_secs(2)));
        assert_eq!(busy.status().cooldown, Some((Duration::from_secs(2), 429)));

        let started = Instant::now();
        let slot = other.acquire(started + Duration::from_secs(10)).await.unwrap();
        assert_eq!(slot.waited(), Duration::ZERO, "another endpoint is not delayed");
        drop(slot);
        let slot = busy.acquire(started + Duration::from_secs(10)).await.unwrap();
        assert_eq!(slot.waited(), Duration::from_secs(2));
        assert_eq!(busy.status().cooldown, None);
    }

    #[tokio::test(start_paused = true)]
    async fn busy_answers_in_a_row_double_the_cooldown_and_a_success_resets_it() {
        let endpoint = Endpoints::default().get("http://zorak:8090");
        let far = || Instant::now() + Duration::from_secs(600);
        for (n, want) in [(1, 5), (2, 10), (3, 20)] {
            let slot = endpoint.acquire(far()).await.unwrap();
            slot.answered_with(503, None);
            assert_eq!(endpoint.status().cooldown, Some((Duration::from_secs(want), 503)), "busy answer {n}");
            drop(slot);
            tokio::time::sleep(Duration::from_secs(want)).await;
        }
        endpoint.acquire(far()).await.unwrap().answered_with(200, None);
        endpoint.acquire(far()).await.unwrap().answered_with(503, None);
        assert_eq!(endpoint.status().cooldown, Some((FIRST_COOLDOWN, 503)));
    }

    #[tokio::test(start_paused = true)]
    async fn two_backends_at_one_origin_share_one_limiter_at_the_smaller_limit() {
        let endpoints = Endpoints::default();
        let a = backend("mk-zorak", Some("http://zorak:8090"), Some(4));
        let b = backend("tenchi", Some("http://zorak:8090/v1"), Some(2));
        endpoints.configure(&[a.clone(), b.clone(), backend("local", Some("http://localhost:11434/v1"), None)]);
        let (ea, eb) = (endpoints.for_backend(&a).unwrap(), endpoints.for_backend(&b).unwrap());
        assert!(Arc::ptr_eq(&ea, &eb));
        let status = ea.status();
        assert_eq!(status.limit, Some(2));
        assert_eq!(status.backends, vec![("mk-zorak".to_string(), Some(4)), ("tenchi".to_string(), Some(2))]);
        let _x = ea.acquire(Instant::now()).await.unwrap();
        let _y = eb.acquire(Instant::now()).await.unwrap();
        assert!(ea.acquire(Instant::now()).await.is_err(), "a third slot at a limit of 2");
        assert_eq!(endpoints.for_url("http://localhost:11434").unwrap().status().limit, None);

        // A rebuild that drops both keeps the slots and lifts the limit.
        endpoints.configure(&[]);
        assert_eq!(ea.status().limit, None);
        assert_eq!(ea.status().in_flight, 2);
    }
}
