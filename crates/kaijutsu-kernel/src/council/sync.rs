//! Keeps the council server holding the council contexts and specs, and
//! hands the gate what a decision needs.
//!
//! [`CouncilSync::prepare_labels`] runs before each decision. It does only
//! the requests the server is not already believed to have answered: the
//! identity once per server, a spec once per spec id, and a context only when
//! its projected body changed. This kernel is the only writer of its council
//! contexts, so a `PUT` carries the head the server last reported as
//! `If-Match`; a head that moved anyway is a miss, not a retry.
//!
//! Every call takes a slot at the server's endpoint first
//! (`llm::endpoint::mk_call`), waiting no later than the caller's deadline,
//! and a 429 is sent again after its cooldown.
//!
//! The caller bounds `prepare_labels` with the decision's deadline and may
//! drop it part way. A `PUT` dropped before its answer may still have
//! landed, so the context is forgotten and the next `PUT` carries no
//! `If-Match`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use kaijutsu_mk::council::canon;
use kaijutsu_mk::council::wire::{Capability, ServerIdentity, SnapshotId, Spec, SpecId, SpecQuestion};
use kaijutsu_mk::{MkClient, MkError};
use kaijutsu_types::ContextId;
use sha2::{Digest, Sha256};

use super::projection::{HOUSE_RULES_BYTES_PER_TOKEN, HouseRules, project, project_house_rules, project_shadow};
use crate::kj::gate_policy::CouncilConfig;
use crate::llm::endpoint::{Endpoint, NoSlot, SlotWaits, mk_call};

/// Everything one decision needs from the server's side.
pub(crate) struct Prepared {
    /// Addresses `CouncilConfig.server`. Its timeout, `deadline_ms`, bounds
    /// each call `prepare_labels` makes; a decision passes the time its
    /// deadline leaves.
    pub(crate) client: MkClient,
    /// The server's endpoint, which each call takes a slot from.
    pub(crate) endpoint: Arc<Endpoint>,
    pub(crate) identity: ServerIdentity,
    pub(crate) spec: Spec,
    pub(crate) spec_id: SpecId,
    /// In the order the labels were given, then the house-rules context when
    /// one is read.
    pub(crate) contexts: Vec<PreparedContext>,
}

impl Prepared {
    /// The house-rules context this decision reads, when it reads one.
    pub(crate) fn house_rules(&self) -> Option<&PreparedContext> {
        self.contexts.iter().find(|c| c.house_rules)
    }
}

/// One council context as the server holds it.
pub(crate) struct PreparedContext {
    pub(crate) label: String,
    pub(crate) context_id: ContextId,
    /// The head the server reported for the body this kernel sent.
    pub(crate) head: SnapshotId,
    /// The house-rules context (`docs/council.md`, "House rules"), not a
    /// labeled council context. Its id comes from its body.
    pub(crate) house_rules: bool,
}

/// Bounds each call that primes a shadow. Priming runs off the gate's path,
/// so it waits longer than a decision's deadline.
pub(crate) const SHADOW_PRIME_TIMEOUT: Duration = Duration::from_secs(30);

/// The label the house-rules context goes by in reports and miss causes.
pub(crate) const HOUSE_RULES_LABEL: &str = "house-rules";

/// The namespace of house-rules context ids, which are UUIDv5 over the
/// sha256 of the projected body.
const HOUSE_RULES_NAMESPACE: uuid::Uuid = uuid::uuid!("5c1f0b0e-9a47-4d3b-8e62-7f1a2b3c4d5e");

/// The id the council server holds a house-rules body under: UUIDv5 of the
/// body's sha256 in [`HOUSE_RULES_NAMESPACE`]. The same body is the same id
/// for every seat, and a changed body is a new id.
fn house_rules_id(body: &kaijutsu_mk::council::wire::ContextPut) -> Result<ContextId, PrepareMiss> {
    Ok(ContextId::from(uuid::Uuid::new_v5(&HOUSE_RULES_NAMESPACE, &body_hash(body)?)))
}

/// The miss cause for a call that got no slot at its endpoint before the
/// deadline.
pub(crate) fn no_slot_cause(deadline_ms: u64, no_slot: &NoSlot) -> String {
    format!(
        "no answer within the {deadline_ms} ms deadline: the deadline passed waiting {} ms for a slot at {} ({})",
        no_slot.waited.as_millis(),
        no_slot.endpoint,
        no_slot.why
    )
}

/// The council server one decision or priming talks to: its client, its
/// endpoint, and the deadline that bounds every slot wait.
struct Link<'a> {
    client: &'a MkClient,
    server: &'a str,
    endpoint: Arc<Endpoint>,
    deadline: tokio::time::Instant,
    deadline_ms: u64,
    waits: &'a SlotWaits,
}

impl Link<'_> {
    /// One call through the endpoint. No slot before the deadline is a miss.
    async fn send<T, F, Fut>(&self, call: F) -> Result<Result<T, MkError>, PrepareMiss>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, MkError>>,
    {
        match mk_call(&self.endpoint, self.deadline, self.waits, call).await {
            Ok((_slot, result)) => Ok(result),
            Err(no_slot) => Err(PrepareMiss(no_slot_cause(self.deadline_ms, &no_slot))),
        }
    }
}

/// The endpoint `server` addresses.
fn endpoint_of(kernel: &crate::Kernel, server: &str) -> Result<Arc<Endpoint>, PrepareMiss> {
    kernel.endpoints().for_url(server).map_err(|e| PrepareMiss(format!("council server {server}: {e}")))
}

/// Why the council cannot be asked right now, in plain words; the gate
/// records it as a miss cause.
#[derive(Debug)]
pub(crate) struct PrepareMiss(pub(crate) String);

#[derive(Clone)]
struct Held {
    /// sha256 of the projected body last sent, before any `warm` is added.
    body: [u8; 32],
    head: SnapshotId,
}

#[derive(Default)]
struct State {
    identities: HashMap<String, ServerIdentity>,
    specs: HashSet<(String, SpecId)>,
    contexts: HashMap<(String, ContextId), Held>,
}

/// What this kernel believes each council server holds.
#[derive(Default)]
pub(crate) struct CouncilSync {
    /// Serializes `prepare`, so two decisions never `PUT` the same context
    /// against the same head.
    serial: tokio::sync::Mutex<()>,
    /// Serializes [`CouncilSync::prime_shadow`] apart from `prepare`: a
    /// decision never reads a shadow while shadows only record, so a slow
    /// priming `PUT` never holds up the gate.
    shadow_serial: tokio::sync::Mutex<()>,
    state: parking_lot::Mutex<State>,
}

/// Forgets a context whose `PUT` was dropped before it answered: the server
/// may hold the new body under a head this kernel never saw.
struct PutInFlight<'a> {
    state: &'a parking_lot::Mutex<State>,
    key: Option<(String, ContextId)>,
}

impl PutInFlight<'_> {
    /// The `PUT` answered; the caller records what it learned.
    fn answered(mut self) {
        self.key = None;
    }
}

impl Drop for PutInFlight<'_> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.state.lock().contexts.remove(&key);
        }
    }
}

/// The seat's house rules: the first `AGENTS.md` found walking up from its
/// working directory (the home directory when it has none), read through the
/// kernel VFS, up to one byte past the budget so the projection can mark a
/// cut. A file that cannot be read, or is not UTF-8, is skipped with a
/// warning; the decision reads no house-rules context rather than missing.
async fn house_rules(kernel: &crate::Kernel, seat: ContextId, house_rules_tokens: u64) -> Option<HouseRules> {
    use crate::vfs::VfsOps;
    let cwd = match kernel.kernel_db().lock().get_context_shell(seat) {
        Ok(row) => row.and_then(|row| row.cwd),
        Err(e) => {
            tracing::warn!(seat = %seat.short(), error = %e, "the seat's working directory cannot be read; no house rules");
            return None;
        }
    }
    .unwrap_or_else(|| kaish_kernel::home_dir().to_string_lossy().into_owned());
    let limit = u32::try_from(house_rules_tokens.saturating_mul(HOUSE_RULES_BYTES_PER_TOKEN).saturating_add(1)).unwrap_or(u32::MAX);
    for dir in std::path::Path::new(&cwd).ancestors() {
        let path = dir.join("AGENTS.md");
        if !kernel.vfs().exists(&path).await {
            continue;
        }
        let shown = path.to_string_lossy().into_owned();
        return match kernel.vfs().read(&path, 0, limit).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => Some(HouseRules { path: shown, text }),
                Err(e) => {
                    // A cut can split a character; keep the valid prefix.
                    let valid = e.utf8_error().valid_up_to();
                    if valid + 4 > limit as usize {
                        let mut bytes = e.into_bytes();
                        bytes.truncate(valid);
                        Some(HouseRules { path: shown, text: String::from_utf8(bytes).unwrap_or_default() })
                    } else {
                        tracing::warn!(path = %shown, "house rules are not UTF-8; the decision reads none");
                        None
                    }
                }
            },
            Err(e) => {
                tracing::warn!(path = %shown, error = %e, "house rules cannot be read; the decision reads none");
                None
            }
        };
    }
    None
}

impl CouncilSync {
    /// Brings the server up to date for a decision on the spec named
    /// `spec_name` (`/config/kernel/council/<spec_name>.json`) over exactly
    /// `labels`, in that order. A gate decision passes `[council] contexts`
    /// followed by its voting reviewer contexts ([`super::reviewer_contexts::ReviewerContextChain::decision_labels`]);
    /// an observation passes the one context it reads.
    ///
    /// `seat` names the proposing seat's context when `[council] house_rules`
    /// is on. Its working directory finds the house rules, whose projection
    /// ([`project_house_rules`]) is held under an id derived from its body and
    /// read after the labels. A seat with no `AGENTS.md` above its working
    /// directory adds no context.
    ///
    /// A miss, before any context is sent: a spec holding a `text` question
    /// when the server lacks the `describe` capability, a label listed twice,
    /// and more contexts, the house rules included, than the server's
    /// `contexts_per_decision`.
    ///
    /// Each call waits for a slot no later than `deadline`, adding its wait
    /// to `waits`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn prepare_labels(
        &self,
        kernel: &crate::Kernel,
        council: &CouncilConfig,
        spec_name: &str,
        labels: &[String],
        seat: Option<ContextId>,
        deadline: tokio::time::Instant,
        waits: &SlotWaits,
    ) -> Result<Prepared, PrepareMiss> {
        let _serial = self.serial.lock().await;
        let server = council.server.as_str();
        let client = MkClient::new(server, Duration::from_millis(council.deadline_ms))
            .map_err(|e| self.failed(server, "client", e))?;
        let endpoint = endpoint_of(kernel, server)?;
        let link = Link { client: &client, server, endpoint: endpoint.clone(), deadline, deadline_ms: council.deadline_ms, waits };

        let identity = self.identity(&link).await?;
        let (spec_body, spec_id) = read_spec(kernel, spec_name).await?;
        if let Some(text) = spec_body.questions.iter().find(|q| matches!(q, SpecQuestion::Text(_)))
            && !identity.capabilities.contains(&Capability::Describe)
        {
            return Err(PrepareMiss(format!(
                "spec {spec_name} holds the text question `{}`, and council server {server} lacks the \
                 `describe` capability that answers one; remove the question from \
                 /config/kernel/council/{spec_name}.json",
                text.id()
            )));
        }
        if let Some((i, label)) = labels.iter().enumerate().find(|(i, l)| labels[..*i].contains(l)) {
            return Err(PrepareMiss(format!(
                "a decision on spec {spec_name} lists council context \"{label}\" twice (position {})",
                i + 1
            )));
        }
        let house = match seat {
            None => None,
            Some(context_id) => {
                let rules = house_rules(kernel, context_id, council.house_rules_tokens).await;
                match project_house_rules(rules.as_ref(), council.house_rules_tokens) {
                    Some(body) => Some((house_rules_id(&body)?, body)),
                    None => None,
                }
            }
        };
        let reads = labels.len() + usize::from(house.is_some());
        if reads as u64 > identity.limits.contexts_per_decision {
            let mut named = labels.to_vec();
            if house.is_some() {
                named.push("the house-rules context".to_string());
            }
            return Err(PrepareMiss(format!(
                "a decision on spec {spec_name} reads {reads} contexts ({}), and council server {server} \
                 reads at most {} per decision (identity.limits.contexts_per_decision)",
                named.join(", "),
                identity.limits.contexts_per_decision
            )));
        }
        self.ensure_spec(&link, &spec_body, &spec_id).await?;

        let mut contexts = Vec::with_capacity(reads);
        for label in labels {
            let context_id = resolve_label(kernel, label)?;
            let blocks = kernel.blocks().block_snapshots(context_id).map_err(|e| {
                PrepareMiss(format!("council context \"{label}\" cannot be read: {e}"))
            })?;
            let body = project(label, &blocks, &*kernel.kernel_db().lock())
                .map_err(|e| PrepareMiss(format!("council context \"{label}\" cannot be projected: {e}")))?;
            let head = self.hold(&link, &identity, &spec_id, label, context_id, body, true).await?;
            contexts.push(PreparedContext { label: label.clone(), context_id, head, house_rules: false });
        }
        if let Some((context_id, body)) = house {
            let head = self.hold(&link, &identity, &spec_id, HOUSE_RULES_LABEL, context_id, body, false).await?;
            contexts.push(PreparedContext { label: HOUSE_RULES_LABEL.to_string(), context_id, head, house_rules: true });
        }

        let spec = spec_body;
        drop(link);
        Ok(Prepared { client, endpoint, identity, spec, spec_id, contexts })
    }

    /// Sends the shadow `shadow` of the seat labeled `seat` to `server`, so
    /// the server holds its dialogue before the next question about it.
    /// Warms the spec named `spec_name` when the server can. Sends nothing
    /// when the server already holds this body. Each call waits for a slot
    /// no later than `deadline`, adding its wait to `waits`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn prime_shadow(
        &self,
        kernel: &crate::Kernel,
        server: &str,
        spec_name: &str,
        shadow: ContextId,
        seat: &str,
        deadline: tokio::time::Instant,
        waits: &SlotWaits,
    ) -> Result<SnapshotId, PrepareMiss> {
        let _serial = self.shadow_serial.lock().await;
        let client = MkClient::new(server, SHADOW_PRIME_TIMEOUT).map_err(|e| self.failed(server, "client", e))?;
        let deadline_ms = deadline.saturating_duration_since(tokio::time::Instant::now()).as_millis() as u64;
        let link = Link { client: &client, server, endpoint: endpoint_of(kernel, server)?, deadline, deadline_ms, waits };
        let identity = self.identity(&link).await?;
        let (spec_body, spec_id) = read_spec(kernel, spec_name).await?;
        self.ensure_spec(&link, &spec_body, &spec_id).await?;
        let blocks = kernel
            .blocks()
            .block_snapshots(shadow)
            .map_err(|e| PrepareMiss(format!("shadow {shadow} cannot be read: {e}")))?;
        let body = project_shadow(seat, &blocks, &*kernel.kernel_db().lock())
            .map_err(|e| PrepareMiss(format!("shadow {shadow} cannot be projected: {e}")))?;
        let label = format!("shadow of {seat}");
        self.hold(&link, &identity, &spec_id, &label, shadow, body, true).await
    }

    /// Makes the server hold `body` as `context_id` and returns its head.
    /// Sends nothing when the server already holds this body. A context that
    /// is not `persistent` is sent with `persist: false` when the server
    /// lists the capability: the kernel sends it again after a restart, so
    /// the server need not keep it on disk.
    #[allow(clippy::too_many_arguments)]
    async fn hold(
        &self,
        link: &Link<'_>,
        identity: &ServerIdentity,
        spec_id: &SpecId,
        label: &str,
        context_id: ContextId,
        body: kaijutsu_mk::council::wire::ContextPut,
        persistent: bool,
    ) -> Result<SnapshotId, PrepareMiss> {
        let server = link.server;
        let hash = body_hash(&body)?;
        let key = (server.to_string(), context_id);
        let held = self.state.lock().contexts.get(&key).cloned();
        if let Some(h) = held.as_ref().filter(|h| h.body == hash) {
            return Ok(h.head.clone());
        }
        let mut body = body;
        if identity.capabilities.contains(&Capability::Warm) {
            body.warm = Some(vec![spec_id.clone()]);
        }
        if !persistent && identity.capabilities.contains(&Capability::Persist) {
            body.persist = Some(false);
        }
        let id = context_id.to_string();
        let mut if_match = held.as_ref().map(|h| h.head.clone());
        let in_flight = PutInFlight { state: &self.state, key: Some(key.clone()) };
        let result = loop {
            match link.send(|| link.client.put_context(&id, &body, if_match.as_ref())).await? {
                Err(e) if e.status() == Some(404) && if_match.is_some() => {
                    if_match = None;
                }
                other => break other,
            }
        };
        in_flight.answered();
        match result {
            Ok(put) => {
                self.state.lock().contexts.insert(key, Held { body: hash, head: put.head.clone() });
                Ok(put.head)
            }
            Err(e) if e.status() == Some(412) => {
                self.state.lock().contexts.remove(&key);
                Err(PrepareMiss(format!(
                    "council context \"{label}\" moved on {server} under If-Match \
                     (server head {}); the next decision sends it fresh",
                    e.head().map(|h| h.to_string()).unwrap_or_else(|| "unknown".into()),
                )))
            }
            Err(e) => Err(self.failed(server, &format!("PUT of context \"{label}\""), e)),
        }
    }

    /// Forgets what the server is believed to hold for `context_id`, so the
    /// next `prepare_labels` sends it again without `If-Match`. Call it after
    /// a 404 or 409 from a decision.
    pub(crate) fn invalidate(&self, context_id: ContextId) {
        self.state.lock().contexts.retain(|(_, id), _| *id != context_id);
    }

    /// Forgets that `spec_id` was posted, so the next `prepare_labels` for it
    /// posts it again. Call it after a decision's 404 names the spec.
    pub(crate) fn invalidate_spec(&self, spec_id: &SpecId) {
        self.state.lock().specs.retain(|(_, id)| id != spec_id);
    }

    async fn identity(&self, link: &Link<'_>) -> Result<ServerIdentity, PrepareMiss> {
        let server = link.server;
        if let Some(i) = self.state.lock().identities.get(server) {
            return Ok(i.clone());
        }
        let identity = link.send(|| link.client.identity()).await?.map_err(|e| self.failed(server, "identity", e))?;
        self.state.lock().identities.insert(server.to_string(), identity.clone());
        Ok(identity)
    }

    async fn ensure_spec(&self, link: &Link<'_>, spec: &Spec, spec_id: &SpecId) -> Result<(), PrepareMiss> {
        let server = link.server;
        let key = (server.to_string(), spec_id.clone());
        if self.state.lock().specs.contains(&key) {
            return Ok(());
        }
        link.send(|| link.client.post_spec(spec))
            .await?
            .map_err(|e| self.failed(server, &format!("POST of spec \"{}\"", spec.name), e))?;
        self.state.lock().specs.insert(key);
        Ok(())
    }

    /// A call to `server` failed: forget its identity and posted specs, so
    /// the next `prepare_labels` asks again, and name the failure.
    fn failed(&self, server: &str, what: &str, e: MkError) -> PrepareMiss {
        let mut state = self.state.lock();
        state.identities.remove(server);
        state.specs.retain(|(s, _)| s != server);
        PrepareMiss(format!("council server {server}: {what} failed: {e}"))
    }
}

fn body_hash(body: &kaijutsu_mk::council::wire::ContextPut) -> Result<[u8; 32], PrepareMiss> {
    let bytes = serde_json::to_vec(body)
        .map_err(|e| PrepareMiss(format!("council context body does not serialize: {e}")))?;
    Ok(Sha256::digest(&bytes).into())
}

fn resolve_label(kernel: &crate::Kernel, label: &str) -> Result<ContextId, PrepareMiss> {
    let row = kernel
        .kernel_db()
        .lock()
        .find_context_by_label(label)
        .map_err(|e| PrepareMiss(format!("council context \"{label}\" lookup failed: {e}")))?;
    row.map(|r| r.context_id)
        .ok_or_else(|| PrepareMiss(format!("council context \"{label}\" names no live context")))
}

async fn read_spec(kernel: &crate::Kernel, name: &str) -> Result<(Spec, SpecId), PrepareMiss> {
    use crate::vfs::VfsOps;
    let path = kaijutsu_types::paths::config_path(&format!("council/{name}.json"));
    let bytes = kernel
        .vfs()
        .read_all(std::path::Path::new(&path))
        .await
        .map_err(|e| PrepareMiss(format!("council spec file {path} cannot be read: {e}")))?;
    let spec: Spec = serde_json::from_slice(&bytes)
        .map_err(|e| PrepareMiss(format!("council spec file {path} is not a spec: {e}")))?;
    let id = canon::spec_id(&spec)
        .map_err(|e| PrepareMiss(format!("council spec file {path} has no spec id: {e}")))?;
    Ok((spec, id))
}

#[cfg(test)]
pub(super) mod mock {
    //! A mock council server on 127.0.0.1 for the council's kernel tests.

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use kaijutsu_mk::council::canon;
    use kaijutsu_mk::council::wire::Spec;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Clone, Debug)]
    pub(crate) struct Reply {
        pub(crate) status: u16,
        pub(crate) body: String,
        /// How long the server holds the reply. Requests are served one at
        /// a time, so a held reply holds every later request too.
        pub(crate) delay: std::time::Duration,
    }

    pub(crate) fn reply(status: u16, body: impl Into<String>) -> Reply {
        Reply { status, body: body.into(), delay: std::time::Duration::ZERO }
    }

    #[derive(Debug)]
    pub(crate) struct Captured {
        pub(crate) method: String,
        pub(crate) path: String,
        pub(crate) headers: Vec<(String, String)>,
        pub(crate) body: String,
    }

    impl Captured {
        pub(crate) fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
        }
    }

    /// A mock council server on 127.0.0.1 that answers the contract's happy
    /// path unless a test queues a reply for a method.
    pub(crate) struct Mock {
        pub(crate) base: String,
        seen: Arc<Mutex<Vec<Captured>>>,
        forced: Arc<Mutex<VecDeque<(String, Reply)>>>,
        decide: Decide,
    }

    type Decide = Arc<Mutex<Option<Box<dyn Fn(&str) -> Reply + Send>>>>;

    impl Mock {
        pub(crate) fn force(&self, method: &str, r: Reply) {
            self.forced.lock().unwrap().push_back((method.to_string(), r));
        }

        pub(crate) fn calls(&self, method: &str) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.method == method)
                .map(|c| c.path.clone())
                .collect()
        }

        pub(crate) fn puts(&self) -> Vec<(Option<String>, serde_json::Value)> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.method == "PUT")
                .map(|c| (c.header("if-match").map(String::from), serde_json::from_str(&c.body).unwrap()))
                .collect()
        }

        pub(crate) fn count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        /// Answer each `POST /council/v1/decisions` with `f` of its body.
        pub(crate) fn on_decide(&self, f: impl Fn(&str) -> Reply + Send + 'static) {
            *self.decide.lock().unwrap() = Some(Box::new(f));
        }

        /// The bodies of every request to `path`, in order.
        pub(crate) fn bodies(&self, path: &str) -> Vec<serde_json::Value> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.path == path)
                .map(|c| serde_json::from_str(&c.body).unwrap())
                .collect()
        }
    }

    pub(crate) fn snap(n: u64) -> String {
        format!("snap:{n:064x}")
    }

    pub(crate) fn identity_json() -> String {
        json!({"model": "m", "weight_hash": "w", "tokenizer_hash": "t", "template": "tpl",
               "engine": "e",
               "limits": {"context_tokens": 1, "state_bytes": 1, "contexts_per_decision": 4,
                          "choice_options": 8, "default_timeout_ms": 1000},
               "capabilities": ["leave_one_out"]})
        .to_string()
    }

    fn default_reply(c: &Captured, puts: &mut u64) -> Reply {
        match (c.method.as_str(), c.path.as_str()) {
            ("GET", "/council/v1/identity") => reply(200, identity_json()),
            ("POST", "/council/v1/specs") => {
                let spec: Spec = serde_json::from_str(&c.body).unwrap();
                let id = canon::spec_id(&spec).unwrap();
                reply(200, json!({"spec_id": id, "spec": spec, "template": "t"}).to_string())
            }
            ("PUT", p) if p.starts_with("/council/v1/contexts/") => {
                *puts += 1;
                reply(
                    200,
                    json!({"id": p.rsplit('/').next().unwrap(), "head": snap(*puts), "tokens": 1,
                           "kept": 0, "fed": 1, "dry_run": false, "snapshots": []})
                    .to_string(),
                )
            }
            _ => reply(500, "unexpected"),
        }
    }

    pub(crate) async fn serve() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let forced: Arc<Mutex<VecDeque<(String, Reply)>>> = Arc::default();
        let decide: Decide = Arc::default();
        let (log, queue, decider) = (seen.clone(), forced.clone(), decide.clone());
        tokio::spawn(async move {
            let mut puts = 0u64;
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head_end, len) = loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "client closed before sending a request");
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map(|v| v.trim().parse::<usize>().unwrap())
                            .unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let n = sock.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.lines();
                let mut first = lines.next().unwrap().split(' ');
                let method = first.next().unwrap().to_string();
                let path = first.next().unwrap().to_string();
                let headers = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(n, v)| (n.trim().to_lowercase(), v.trim().to_string()))
                    .collect();
                let captured = Captured {
                    method,
                    path,
                    headers,
                    body: String::from_utf8_lossy(&buf[head_end..]).to_string(),
                };
                let forced_reply = {
                    let mut q = queue.lock().unwrap();
                    q.iter().position(|(m, _)| *m == captured.method).and_then(|i| q.remove(i))
                };
                let decided = (captured.path == "/council/v1/decisions")
                    .then(|| decider.lock().unwrap().as_ref().map(|f| f(&captured.body)))
                    .flatten();
                let r = match (forced_reply, decided) {
                    (Some((_, r)), _) => r,
                    (None, Some(r)) => r,
                    (None, None) => default_reply(&captured, &mut puts),
                };
                log.lock().unwrap().push(captured);
                tokio::time::sleep(r.delay).await;
                let out = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    r.status,
                    r.body.len(),
                    r.body
                );
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        Mock { base, seen, forced, decide }
    }

}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::mock::{Mock, identity_json, reply, serve, snap};
    use super::super::projection::fixtures::{append_dialogue, live_context};
    use super::*;

    /// A deadline no test reaches.
    fn far() -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(30)
    }
    use crate::Kernel;
    use crate::kj::gate_policy::{CouncilCase, CouncilPoolMethod, CouncilPoolWeights, CouncilSpec};
    use crate::vfs::LocalBackend;

    /// The shipped shell spec: no `text` question, so it needs no
    /// `describe` capability.
    const SPEC: &str = crate::config_seed::DEFAULT_COUNCIL_SHELL_GATE;
    /// The contract's example spec, which holds the `text` question `effect`.
    const SPEC_WITH_TEXT: &str = include_str!("../../../kaijutsu-mk/tests/fixtures/spec.json");

    struct Rig {
        kernel: Kernel,
        mock: Mock,
        council: CouncilConfig,
        spec: CouncilSpec,
        reviewer: ContextId,
        config: tempfile::TempDir,
    }

    async fn rig() -> Rig {
        let kernel = Kernel::new_ephemeral("council-sync").await;
        let config = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config.path().join("council")).unwrap();
        std::fs::write(config.path().join("council/shell-gate.json"), SPEC).unwrap();
        kernel
            .vfs()
            .mount(kaijutsu_types::paths::CONFIG_ROOT, LocalBackend::new(config.path()))
            .await;
        let reviewer = live_context(&kernel, "reviewer");
        append_dialogue(&kernel, reviewer, &["never rm -rf the repo", "understood"]);
        let mock = serve().await;
        let spec = CouncilSpec { name: "shell-gate".into(), case: CouncilCase::Shell, contexts: Vec::new(), require_agree: None };
        let council = CouncilConfig {
            server: mock.base.clone(),
            contexts: vec!["reviewer".into()],
            pool_method: CouncilPoolMethod::Linear,
            pool_weights: CouncilPoolWeights::Uniform,
            deadline_ms: 5000,
            require_agree: false,
            reviewer_contexts: false,
            house_rules: false,
            house_rules_tokens: crate::kj::gate_policy::DEFAULT_HOUSE_RULES_TOKENS,
            mode: crate::kj::gate_policy::CouncilMode::Gatekeeper,
            bump_limit: Some(crate::kj::gate_policy::DEFAULT_BUMP_LIMIT),
            escalate: None,
            specs: vec![spec.clone()],
            thresholds: vec![],
        };
        Rig { kernel, mock, council, spec, reviewer, config }
    }

    impl Rig {
        async fn prepare(&self) -> Result<Prepared, PrepareMiss> {
            self.kernel.council_sync().prepare_labels(&self.kernel, &self.council, &self.spec.name, &self.council.contexts, None, far(), &SlotWaits::default()).await
        }

        fn grow(&self, text: &str) {
            let ids = self.kernel.blocks().block_snapshots(self.reviewer).unwrap();
            super::super::projection::fixtures::append(
                &self.kernel,
                self.reviewer,
                ids.last().map(|b| &b.id),
                kaijutsu_types::Role::User,
                kaijutsu_types::BlockKind::Text,
                kaijutsu_types::Status::Done,
                text,
            );
        }
    }

    /// A prepare dropped while a `PUT` waits forgets the context: the server
    /// may hold the new body, so the next `PUT` carries no `If-Match`.
    ///
    /// Falsified by a prepare that keeps the old head when it is dropped:
    /// the next `PUT` names it, and the server answers 412.
    #[tokio::test]
    async fn a_prepare_dropped_during_a_put_sends_the_context_fresh_next_time() {
        let r = rig().await;
        r.prepare().await.unwrap();
        r.grow("and never force push");
        let mut slow = reply(
            200,
            json!({"id": r.reviewer.to_string(), "head": snap(7), "tokens": 1, "kept": 0, "fed": 1,
                   "dry_run": false, "snapshots": []})
            .to_string(),
        );
        slow.delay = std::time::Duration::from_millis(400);
        r.mock.force("PUT", slow);
        let dropped = tokio::time::timeout(std::time::Duration::from_millis(100), r.prepare()).await;
        assert!(dropped.is_err(), "the prepare is dropped while its PUT waits");
        r.prepare().await.expect("prepared");
        let puts = r.mock.puts();
        assert_eq!(puts.len(), 3, "first, the dropped one, the fresh one");
        assert!(puts[1].0.is_some(), "the dropped PUT named the head it knew");
        assert_eq!(puts[2].0, None, "the next PUT carries no If-Match");
    }

    #[tokio::test]
    async fn invalidate_spec_makes_the_next_prepare_post_the_spec_again() {
        let r = rig().await;
        let p = r.prepare().await.unwrap();
        r.prepare().await.unwrap();
        assert_eq!(r.mock.calls("POST").len(), 1);
        r.kernel.council_sync().invalidate_spec(&p.spec_id);
        r.prepare().await.unwrap();
        assert_eq!(r.mock.calls("POST").len(), 2, "posted again");
        assert_eq!(r.mock.puts().len(), 1, "the context is still held");
    }

    #[tokio::test]
    async fn the_first_prepare_gets_the_identity_posts_the_spec_and_puts_each_context() {
        let r = rig().await;
        let p = r.prepare().await.expect("prepared");
        assert_eq!(r.mock.calls("GET"), ["/council/v1/identity"]);
        assert_eq!(r.mock.calls("POST"), ["/council/v1/specs"]);
        let puts = r.mock.puts();
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0].0, None, "the first PUT has no If-Match");
        assert_eq!(puts[0].1["turns"].as_array().unwrap().len(), 2);
        assert!(puts[0].1.get("warm").is_none() && puts[0].1.get("dry_run").is_none());
        assert_eq!(p.contexts.len(), 1);
        assert_eq!(p.contexts[0].label, "reviewer");
        assert_eq!(p.contexts[0].context_id, r.reviewer);
        assert_eq!(p.contexts[0].head.as_str(), snap(1));
        assert_eq!(p.spec_id, canon::spec_id(&p.spec).unwrap());
        assert_eq!(p.identity.engine, "e");
    }

    #[tokio::test]
    async fn a_second_prepare_with_no_change_sends_nothing() {
        let r = rig().await;
        let first = r.prepare().await.unwrap();
        let sent = r.mock.count();
        let second = r.prepare().await.unwrap();
        assert_eq!(r.mock.count(), sent, "no request for an unchanged world");
        assert_eq!(second.contexts[0].head, first.contexts[0].head);
    }

    #[tokio::test]
    async fn a_changed_context_puts_again_with_the_last_head_in_if_match() {
        let r = rig().await;
        let first = r.prepare().await.unwrap();
        let ids = r.kernel.blocks().block_snapshots(r.reviewer).unwrap();
        super::super::projection::fixtures::append(
            &r.kernel,
            r.reviewer,
            ids.last().map(|b| &b.id),
            kaijutsu_types::Role::User,
            kaijutsu_types::BlockKind::Text,
            kaijutsu_types::Status::Done,
            "and never force push",
        );
        let second = r.prepare().await.unwrap();
        let puts = r.mock.puts();
        assert_eq!(puts.len(), 2);
        assert_eq!(puts[1].0.as_deref(), Some(first.contexts[0].head.as_str()));
        assert_eq!(puts[1].1["turns"].as_array().unwrap().len(), 3);
        assert_eq!(second.contexts[0].head.as_str(), snap(2));
    }

    #[tokio::test]
    async fn a_404_on_put_sends_the_context_again_without_if_match() {
        let r = rig().await;
        r.prepare().await.unwrap();
        let ids = r.kernel.blocks().block_snapshots(r.reviewer).unwrap();
        super::super::projection::fixtures::append(
            &r.kernel,
            r.reviewer,
            ids.last().map(|b| &b.id),
            kaijutsu_types::Role::User,
            kaijutsu_types::BlockKind::Text,
            kaijutsu_types::Status::Done,
            "more guidance",
        );
        r.mock.force(
            "PUT",
            reply(404, json!({"error": {"type": "not_found", "message": "no such context"}}).to_string()),
        );
        let p = r.prepare().await.expect("recovers within the call");
        let puts = r.mock.puts();
        assert_eq!(puts.len(), 3, "first, the refused one, the retry");
        assert!(puts[1].0.is_some());
        assert_eq!(puts[2].0, None);
        assert_eq!(p.contexts[0].head.as_str(), snap(2));
    }

    #[tokio::test]
    async fn a_412_is_a_miss_and_the_next_prepare_puts_fresh() {
        let r = rig().await;
        r.prepare().await.unwrap();
        let ids = r.kernel.blocks().block_snapshots(r.reviewer).unwrap();
        super::super::projection::fixtures::append(
            &r.kernel,
            r.reviewer,
            ids.last().map(|b| &b.id),
            kaijutsu_types::Role::User,
            kaijutsu_types::BlockKind::Text,
            kaijutsu_types::Status::Done,
            "more guidance",
        );
        r.mock.force(
            "PUT",
            reply(
                412,
                json!({"error": {"type": "head_mismatch", "message": "moved", "head": snap(9)}})
                    .to_string(),
            ),
        );
        let miss = r.prepare().await.err().expect("a miss");
        assert!(miss.0.contains("reviewer") && miss.0.contains(&snap(9)), "{}", miss.0);
        let p = r.prepare().await.expect("recovers on the next call");
        let puts = r.mock.puts();
        assert_eq!(puts.last().unwrap().0, None, "fresh PUT carries no If-Match");
        assert_eq!(p.contexts[0].label, "reviewer");
    }

    #[tokio::test]
    async fn invalidate_makes_the_next_prepare_send_the_context_again() {
        let r = rig().await;
        r.prepare().await.unwrap();
        r.kernel.council_sync().invalidate(r.reviewer);
        r.prepare().await.unwrap();
        let puts = r.mock.puts();
        assert_eq!(puts.len(), 2);
        assert_eq!(puts[1].0, None);
    }

    #[tokio::test]
    async fn a_label_naming_no_live_context_is_a_miss_naming_it() {
        let mut r = rig().await;
        r.council.contexts.push("missing-rules".into());
        let miss = r.prepare().await.err().expect("a miss");
        assert!(miss.0.contains("missing-rules"), "{}", miss.0);
    }

    #[tokio::test]
    async fn a_missing_spec_file_is_a_miss_naming_the_path() {
        let r = rig().await;
        std::fs::remove_file(r.config.path().join("council/shell-gate.json")).unwrap();
        let miss = r.prepare().await.err().expect("a miss");
        assert!(miss.0.contains("/config/kernel/council/shell-gate.json"), "{}", miss.0);
        assert_eq!(r.mock.puts().len(), 0);
    }

    #[tokio::test]
    async fn an_unparseable_spec_file_is_a_miss_naming_the_path() {
        let r = rig().await;
        std::fs::write(r.config.path().join("council/shell-gate.json"), "{not json").unwrap();
        let miss = r.prepare().await.err().expect("a miss");
        assert!(miss.0.contains("/config/kernel/council/shell-gate.json"), "{}", miss.0);
    }

    #[tokio::test]
    async fn a_server_that_disagrees_on_the_spec_id_is_a_miss() {
        let r = rig().await;
        let other = format!("sha256:{}", "0".repeat(64));
        let spec: serde_json::Value = serde_json::from_str(SPEC).unwrap();
        r.mock.force(
            "POST",
            reply(200, json!({"spec_id": other, "spec": spec, "template": "t"}).to_string()),
        );
        let miss = r.prepare().await.err().expect("a miss");
        assert!(miss.0.contains("spec id disagrees"), "{}", miss.0);
    }

    #[tokio::test]
    async fn the_identity_is_cached_and_fetched_again_after_a_failure() {
        let r = rig().await;
        r.prepare().await.unwrap();
        r.prepare().await.unwrap();
        assert_eq!(r.mock.calls("GET").len(), 1, "cached across prepares");

        let ids = r.kernel.blocks().block_snapshots(r.reviewer).unwrap();
        super::super::projection::fixtures::append(
            &r.kernel,
            r.reviewer,
            ids.last().map(|b| &b.id),
            kaijutsu_types::Role::User,
            kaijutsu_types::BlockKind::Text,
            kaijutsu_types::Status::Done,
            "more guidance",
        );
        r.mock.force("PUT", reply(500, "boom"));
        let miss = r.prepare().await.err().expect("a miss");
        assert!(miss.0.contains(&r.mock.base), "{}", miss.0);
        r.prepare().await.expect("recovered");
        assert_eq!(r.mock.calls("GET").len(), 2, "refetched after the failure");
        assert_eq!(r.mock.calls("POST").len(), 2, "specs are posted again with it");
    }

    #[tokio::test]
    async fn an_unreachable_server_is_a_miss_naming_it() {
        let mut r = rig().await;
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = dead.local_addr().unwrap();
        drop(dead);
        r.council.server = format!("http://{addr}");
        let miss = r.prepare().await.err().expect("a miss");
        assert!(miss.0.contains(&addr.to_string()), "{}", miss.0);
    }

    fn identity_with(capabilities: serde_json::Value) -> String {
        let mut id: serde_json::Value = serde_json::from_str(&identity_json()).unwrap();
        id["capabilities"] = capabilities;
        id.to_string()
    }

    #[tokio::test]
    async fn a_text_question_without_describe_is_a_miss_naming_both() {
        let r = rig().await;
        std::fs::write(r.config.path().join("council/shell-gate.json"), SPEC_WITH_TEXT).unwrap();
        let miss = r.prepare().await.err().expect("a miss");
        assert!(miss.0.contains("`effect`") && miss.0.contains("`describe`"), "{}", miss.0);
        assert!(r.mock.calls("POST").is_empty(), "the spec is not posted");
        assert!(r.mock.puts().is_empty(), "no context is sent");
    }

    #[tokio::test]
    async fn a_text_question_with_describe_prepares() {
        let r = rig().await;
        std::fs::write(r.config.path().join("council/shell-gate.json"), SPEC_WITH_TEXT).unwrap();
        r.mock.force("GET", reply(200, identity_with(json!(["leave_one_out", "describe"]))));
        r.prepare().await.expect("prepared");
    }

    #[tokio::test]
    async fn prepare_labels_reads_exactly_the_labels_given_in_order() {
        let r = rig().await;
        let amy = live_context(&r.kernel, "council-amy");
        append_dialogue(&r.kernel, amy, &["keep main green"]);
        let labels = vec!["reviewer".to_string(), "council-amy".to_string()];
        let p = r.kernel.council_sync().prepare_labels(&r.kernel, &r.council, "shell-gate", &labels, None, far(), &SlotWaits::default()).await.unwrap();
        assert_eq!(p.contexts.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["reviewer", "council-amy"]);
        assert_eq!(p.contexts[1].context_id, amy);

        let one = vec!["council-amy".to_string()];
        let p = r.kernel.council_sync().prepare_labels(&r.kernel, &r.council, "shell-gate", &one, None, far(), &SlotWaits::default()).await.unwrap();
        assert_eq!(p.contexts.len(), 1, "a single reviewer, without [council] contexts");
        assert_eq!(r.mock.puts().len(), 2, "each context is sent once");
    }

    /// A seat with a brief and narration, labeled `label`, working in `cwd`.
    fn seat_in(r: &Rig, label: &str, cwd: Option<&str>) -> ContextId {
        let seat = live_context(&r.kernel, label);
        append_dialogue(&r.kernel, seat, &["recover the records", "The WAL is XORed."]);
        if let Some(cwd) = cwd {
            r.kernel
                .kernel_db()
                .lock()
                .upsert_context_shell(&crate::kernel_db::ContextShellRow { context_id: seat, cwd: Some(cwd.into()), updated_at: 0 })
                .unwrap();
        }
        seat
    }

    /// Mounts `/work` and writes the repo's `AGENTS.md`.
    async fn write_rules(r: &Rig, text: &str) {
        use crate::vfs::VfsOps;
        if !r.kernel.vfs().exists(std::path::Path::new("/work")).await {
            r.kernel.vfs().mount("/work", crate::vfs::MemoryBackend::new()).await;
        }
        r.kernel.vfs().write_all(std::path::Path::new("/work/repo/AGENTS.md"), text.as_bytes()).await.unwrap();
        r.kernel.vfs().write_all(std::path::Path::new("/work/repo/sub/main.rs"), b"fn main() {}").await.unwrap();
    }

    fn append_to(r: &Rig, ctx: ContextId, role: kaijutsu_types::Role, kind: kaijutsu_types::BlockKind, text: &str) {
        let ids = r.kernel.blocks().block_snapshots(ctx).unwrap();
        super::super::projection::fixtures::append(
            &r.kernel,
            ctx,
            ids.last().map(|b| &b.id),
            role,
            kind,
            kaijutsu_types::Status::Done,
            text,
        );
    }

    async fn prepare_seat(r: &Rig, seat: ContextId) -> Result<Prepared, PrepareMiss> {
        r.kernel.council_sync().prepare_labels(&r.kernel, &r.council, "shell-gate", &r.council.contexts, Some(seat), far(), &SlotWaits::default()).await
    }

    /// The house-rules context is the first `AGENTS.md` found walking up
    /// from the seat's working directory, read through the kernel VFS, and
    /// read after the labels. Nothing the seat said or did reaches it: a
    /// brief, narration, a tool call, and later narration leave the body
    /// and the server's copy as they were.
    ///
    /// Falsified by a sync that reads only the working directory itself (the
    /// rules one level up never arrive), or one that projects the seat's
    /// blocks (the narration changes the body and sends a `PUT`).
    #[tokio::test]
    async fn the_house_rules_come_from_the_nearest_agents_md_and_ignore_the_seat() {
        let mut r = rig().await;
        r.council.house_rules = true;
        write_rules(&r, "Back up data before changing it.").await;
        let seat = seat_in(&r, "lane-a", Some("/work/repo/sub"));
        let p = prepare_seat(&r, seat).await.expect("prepared");
        assert_eq!(
            p.contexts.iter().map(|c| (c.label.as_str(), c.house_rules)).collect::<Vec<_>>(),
            [("reviewer", false), ("house-rules", true)]
        );
        let puts = r.mock.puts();
        assert_eq!(puts.len(), 2);
        let body = &puts[1].1;
        assert!(body["system"].as_str().unwrap().contains("house rules of the workspace"), "{body}");
        assert_eq!(body["turns"].as_array().unwrap().len(), 1, "{body}");
        assert_eq!(
            body["turns"][0]["content"].as_str().unwrap(),
            "The house rules in /work/repo/AGENTS.md:\n\nBack up data before changing it."
        );
        assert!(!body.to_string().contains("WAL"), "the seat's narration stays out: {body}");

        append_to(&r, seat, kaijutsu_types::Role::Model, kaijutsu_types::BlockKind::ToolCall, "xxd main.db-wal");
        append_to(&r, seat, kaijutsu_types::Role::Model, kaijutsu_types::BlockKind::Text, "No backup exists yet.");
        append_to(&r, seat, kaijutsu_types::Role::User, kaijutsu_types::BlockKind::Text, "a later prompt");
        let again = prepare_seat(&r, seat).await.unwrap();
        assert_eq!(r.mock.puts().len(), 2, "narration and prompts send nothing");
        assert_eq!(again.house_rules().unwrap().head, p.house_rules().unwrap().head);
    }

    /// A house-rules context is ephemeral: it is sent with `persist: false`
    /// when the server lists the `persist` capability, so the server need not
    /// park it; a labeled context never carries `persist`, and a server
    /// without the capability never sees the field.
    ///
    /// Falsified by a sync that leaves `persist` off: the house-rules body
    /// has no `persist`.
    #[tokio::test]
    async fn house_rules_are_sent_with_persist_false_when_the_server_can_drop_them() {
        let mut r = rig().await;
        r.council.house_rules = true;
        r.mock.force("GET", reply(200, identity_with(json!(["leave_one_out", "persist"]))));
        write_rules(&r, "Back up data before changing it.").await;
        let seat = seat_in(&r, "lane-a", Some("/work/repo"));
        prepare_seat(&r, seat).await.expect("prepared");
        let puts = r.mock.puts();
        assert_eq!(puts.len(), 2);
        assert!(puts[0].1.get("persist").is_none(), "a labeled context stays persistent: {}", puts[0].1);
        assert_eq!(puts[1].1["persist"], json!(false), "{}", puts[1].1);

        let mut plain = rig().await;
        plain.council.house_rules = true;
        write_rules(&plain, "Back up data before changing it.").await;
        let seat = seat_in(&plain, "lane-a", Some("/work/repo"));
        prepare_seat(&plain, seat).await.expect("prepared");
        assert!(plain.mock.puts()[1].1.get("persist").is_none(), "no capability, no field");
    }

    /// The held id comes from the body, so every seat under one `AGENTS.md`
    /// shares one held context, and an edited file is a new id.
    ///
    /// Falsified by an id derived from the seat's context id: the two seats
    /// hold two contexts and the second prepare sends another `PUT`.
    #[tokio::test]
    async fn seats_under_one_agents_md_share_one_held_context_and_an_edit_is_a_new_id() {
        let mut r = rig().await;
        r.council.house_rules = true;
        write_rules(&r, "Back up data before changing it.").await;
        let one = seat_in(&r, "lane-a", Some("/work/repo/sub"));
        let two = seat_in(&r, "lane-b", Some("/work/repo"));
        let p1 = prepare_seat(&r, one).await.unwrap();
        let p2 = prepare_seat(&r, two).await.unwrap();
        let (h1, h2) = (p1.house_rules().unwrap(), p2.house_rules().unwrap());
        assert_eq!(h1.context_id, h2.context_id, "one body, one id");
        assert_ne!(h1.context_id, one);
        assert_ne!(h1.context_id, two);
        assert_eq!(r.mock.puts().len(), 2, "the reviewer, and the rules once");

        write_rules(&r, "Back up data. Never push.").await;
        let p3 = prepare_seat(&r, one).await.unwrap();
        let h3 = p3.house_rules().unwrap();
        assert_ne!(h3.context_id, h1.context_id, "an edited file is a new id");
        let puts = r.mock.puts();
        assert_eq!(puts.len(), 3);
        assert_eq!(puts[2].0, None, "a new id has no head to match");
    }

    /// With no `AGENTS.md` above the working directory, or no working
    /// directory under a mounted tree that holds one, there is no
    /// house-rules context.
    #[tokio::test]
    async fn no_agents_md_means_no_house_rules_context() {
        let mut r = rig().await;
        r.council.house_rules = true;
        r.kernel.vfs().mount("/bare", crate::vfs::MemoryBackend::new()).await;
        let seat = seat_in(&r, "lane-a", Some("/bare"));
        let p = prepare_seat(&r, seat).await.unwrap();
        assert!(p.house_rules().is_none());
        assert_eq!(p.contexts.len(), 1);
        assert_eq!(r.mock.puts().len(), 1, "only the labeled context");
    }

    /// The labels and the house-rules context together must fit the
    /// server's `contexts_per_decision`; a miss names the limit and the
    /// context.
    #[tokio::test]
    async fn the_house_rules_context_counts_against_contexts_per_decision() {
        let mut r = rig().await;
        r.council.house_rules = true;
        for label in ["rules-a", "rules-b", "rules-c"] {
            live_context(&r.kernel, label);
            r.council.contexts.push(label.into());
        }
        let bare = seat_in(&r, "lane-b", Some("/nowhere"));
        prepare_seat(&r, bare).await.expect("four labels and no house rules fit");
        write_rules(&r, "Back up data.").await;
        let seat = seat_in(&r, "lane-a", Some("/work/repo"));
        let miss = prepare_seat(&r, seat).await.err().expect("a miss");
        assert!(miss.0.contains("reads 5 contexts"), "{}", miss.0);
        assert!(miss.0.contains("house-rules") && miss.0.contains("contexts_per_decision"), "{}", miss.0);
    }

    #[tokio::test]
    async fn more_labels_than_the_server_reads_or_a_repeated_label_is_a_miss() {
        let r = rig().await;
        let labels: Vec<String> = (0..5).map(|i| format!("ctx-{i}")).collect();
        let miss = r.kernel.council_sync().prepare_labels(&r.kernel, &r.council, "shell-gate", &labels, None, far(), &SlotWaits::default()).await.err().unwrap();
        assert!(miss.0.contains("at most 4"), "{}", miss.0);
        let twice = vec!["reviewer".to_string(), "reviewer".to_string()];
        let miss = r.kernel.council_sync().prepare_labels(&r.kernel, &r.council, "shell-gate", &twice, None, far(), &SlotWaits::default()).await.err().unwrap();
        assert!(miss.0.contains("\"reviewer\" twice"), "{}", miss.0);
        assert!(r.mock.puts().is_empty());
    }
}
