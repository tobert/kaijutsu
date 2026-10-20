//! Keeps the council server holding the council contexts and specs, and
//! hands the gate what a decision needs.
//!
//! [`CouncilSync::prepare_labels`] runs before each decision. It does only
//! the requests the server is not already believed to have answered: the
//! identity once per server, a spec once per spec id, and a context only when
//! its projected body changed.
//!
//! Every call takes a slot at the server's endpoint first
//! (`llm::endpoint::mk_call`), waiting no later than the caller's deadline,
//! and a 429 is sent again after its cooldown.
//!
//! The server speaks council v2 (gogi's `docs/council-api.md`) and keeps
//! no context records: a
//! seat is its context value, the projected context as a queue seat for the
//! decision's spec (`super::seat`). A seat is built with
//! `POST /council/v2/contexts` when its value changes, and each decision
//! sends every seat whole. When the server lists `pins`, the heads of every
//! current seat this kernel built on it are pinned under one owner named
//! by the kernel's id ([`pin_owner`]), for [`PIN_TTL`]; the whole set is
//! sent again when it changes and every [`PIN_RENEW`], off the decision's
//! path. A seat's [`Slot`] keeps only its current body, and a pinner
//! forgets seats nothing current reaches before it sends the set.
//! [`build_seats`] builds the seats of the live contexts when the kernel
//! starts, and [`build_context_seats`] the seats that depend on one proposing
//! seat.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;

use gogi_proto::identity::{Capability, ModelStatus, ServedModel, ServerIdentity};
use gogi_proto::pins::PinsPut;
use gogi_proto::spec::{QuestionKind, Spec};
use gogi_proto::{Context, SnapshotId, SpecId};
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
    /// The served model the decision runs on, which was `ready`.
    pub(crate) identity: ServedModel,
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

    /// The prepared context a read names by its index in the decision's
    /// `contexts`.
    pub(crate) fn read_context(&self, index: u64) -> Option<&PreparedContext> {
        self.contexts.get(usize::try_from(index).ok()?)
    }
}

/// One council context as the server reads it.
pub(crate) struct PreparedContext {
    pub(crate) label: String,
    pub(crate) context_id: ContextId,
    /// The queue seat, which a decision sends whole.
    pub(crate) seat: Box<Context>,
    /// The head the server reported for the body this kernel sent.
    pub(crate) head: SnapshotId,
    /// The house-rules context (`docs/council.md`, "House rules"), not a
    /// labeled council context. Its id comes from its body.
    pub(crate) house_rules: bool,
}

/// Bounds each call that primes a shadow. Priming runs off the gate's path,
/// so it waits longer than a decision's deadline.
pub(crate) const SHADOW_PRIME_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a pin set lasts without a renewal.
pub(crate) const PIN_TTL: Duration = Duration::from_secs(3600);

/// How often the pin set is sent again while nothing changes it.
pub(crate) const PIN_RENEW: Duration = Duration::from_secs(20 * 60);

/// Bounds a pin `PUT`. The server writes each newly pinned snapshot to disk
/// before it answers.
const PIN_TIMEOUT: Duration = Duration::from_secs(120);

/// The owner this kernel's pins go under: `kernel-` and the kernel id.
pub(crate) fn pin_owner(kernel: kaijutsu_types::KernelId) -> String {
    format!("kernel-{}", kernel.to_hex())
}

/// The label the house-rules context goes by in reports and miss causes.
pub(crate) const HOUSE_RULES_LABEL: &str = "house-rules";

/// The namespace of house-rules context ids, which are UUIDv5 over the
/// sha256 of the projected body.
const HOUSE_RULES_NAMESPACE: uuid::Uuid = uuid::uuid!("5c1f0b0e-9a47-4d3b-8e62-7f1a2b3c4d5e");

/// The id the council server holds a house-rules body under: UUIDv5 of the
/// body's sha256 in [`HOUSE_RULES_NAMESPACE`]. The same body is the same id
/// for every seat, and a changed body is a new id.
fn house_rules_id(body: &Context) -> Result<ContextId, PrepareMiss> {
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
    /// The server as configured, for messages.
    server: &'a str,
    /// The server's origin (`llm::endpoint::origin`), which keys what this
    /// kernel believes it holds, so two spellings of one server share it.
    key: String,
    endpoint: Arc<Endpoint>,
    deadline: tokio::time::Instant,
    deadline_ms: u64,
    waits: &'a SlotWaits,
    /// The owner of this kernel's pins ([`pin_owner`]).
    owner: String,
    /// The served model's `model`, which every build names. Empty until
    /// the identity names it.
    model: String,
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

/// The endpoint `server` addresses, and its origin.
fn endpoint_of(kernel: &crate::Kernel, server: &str) -> Result<(Arc<Endpoint>, String), PrepareMiss> {
    let missed = |e: String| PrepareMiss(format!("council server {server}: {e}"));
    let key = crate::llm::endpoint::origin(server).map_err(missed)?;
    Ok((kernel.endpoints().for_url(server).map_err(missed)?, key))
}

/// A served model's status as identity spells it.
pub(crate) fn status_word(status: ModelStatus) -> &'static str {
    match status {
        ModelStatus::Ready => "ready",
        ModelStatus::Loading => "loading",
        ModelStatus::Draining => "draining",
        ModelStatus::Unloaded => "unloaded",
        ModelStatus::Unknown => "in a status this kernel does not know",
    }
}

/// Why the council cannot be asked right now, in plain words; the gate
/// records it as a miss cause.
#[derive(Debug)]
pub(crate) struct PrepareMiss(pub(crate) String);

#[derive(Clone)]
struct Held {
    /// The context the body projects: a labeled context's id, a
    /// house-rules id, or a shadow's id.
    id: ContextId,
    /// sha256 of the seat last built.
    body: [u8; 32],
    head: SnapshotId,
}

/// What a held body stands for. The body a slot produces now replaces the
/// one it produced before, so the kernel keeps, and pins, only current
/// bodies.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Slot {
    /// A labeled council context, by its label.
    Label(String),
    /// The house rules read from one `AGENTS.md` under one token budget.
    HouseRules { path: String, tokens: u64 },
    /// A seat's judge shadow, by the shadow's context id.
    Shadow(ContextId),
}

/// A seat the server holds: the server's origin, the served model, the
/// slot, and the spec the seat serves.
type HeldKey = (String, String, Slot, String);

/// Keyed by each server's origin.
#[derive(Default)]
struct State {
    identities: HashMap<String, ServerIdentity>,
    specs: HashSet<(String, SpecId)>,
    contexts: HashMap<HeldKey, Held>,
    /// Each server's pin set, which its pinner task sends.
    pins: HashMap<String, tokio::sync::watch::Sender<BTreeSet<String>>>,
}

impl State {
    /// The heads of every seat held on the server `key`.
    fn seat_heads(&self, key: &str) -> BTreeSet<String> {
        self.contexts
            .iter()
            .filter(|((server, _, _, _), _)| server == key)
            .map(|(_, held)| held.head.to_string())
            .collect()
    }
}

/// What this kernel believes each council server holds.
pub(crate) struct CouncilSync {
    /// Serializes `prepare`, so two decisions never build the same seat at
    /// once.
    serial: tokio::sync::Mutex<()>,
    /// Serializes [`CouncilSync::prime_shadow`] apart from `prepare`: a
    /// decision never reads a shadow while shadows only record, so a slow
    /// priming build never holds up the gate.
    shadow_serial: tokio::sync::Mutex<()>,
    state: Arc<parking_lot::Mutex<State>>,
    /// How often a pinner sends an unchanged set again, in milliseconds.
    pin_renew_ms: std::sync::atomic::AtomicU64,
    /// The kernel this sync serves, once it is shared
    /// ([`CouncilSync::serve`]). Pinners read it to forget unreachable seats
    /// before each `PUT`.
    kernel: Arc<std::sync::OnceLock<Weak<crate::Kernel>>>,
}

impl Default for CouncilSync {
    fn default() -> Self {
        CouncilSync {
            serial: Default::default(),
            shadow_serial: Default::default(),
            state: Default::default(),
            pin_renew_ms: std::sync::atomic::AtomicU64::new(PIN_RENEW.as_millis() as u64),
            kernel: Default::default(),
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
    /// `added` names the contexts the caller adds to the decision after
    /// this prepare, such as a judge's shadow. They are not sent here, and
    /// they count toward the server's limit.
    ///
    /// A miss, before any context is sent: a spec holding a `text` question
    /// when the server lacks the `describe` capability, a label listed twice,
    /// and more contexts, the house rules and `added` included, than the
    /// server's `contexts_per_decision`.
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
        added: &[String],
        deadline: tokio::time::Instant,
        waits: &SlotWaits,
    ) -> Result<Prepared, PrepareMiss> {
        let _serial = self.serial.lock().await;
        let server = council.server.as_str();
        let (endpoint, key) = endpoint_of(kernel, server)?;
        let client = MkClient::new(server, Duration::from_millis(council.deadline_ms))
            .map_err(|e| self.failed(&key, server, "client", e))?;
        let owner = pin_owner(kernel.id());
        let mut link = Link {
            client: &client,
            server,
            key,
            endpoint: endpoint.clone(),
            deadline,
            deadline_ms: council.deadline_ms,
            waits,
            owner,
            model: String::new(),
        };

        let server_identity = self.identity(&link).await?;
        let identity = self.served(&link, &server_identity, council.model.as_deref())?;
        link.model = identity.model.clone();
        let (spec_body, spec_id) = read_spec(kernel, spec_name).await?;
        if let Some(text) = spec_body.questions.iter().find(|q| matches!(q.kind, QuestionKind::Text { .. }))
            && !identity.has(Capability::Describe)
        {
            return Err(PrepareMiss(format!(
                "spec {spec_name} holds the text question `{}`, and model {} on council server {server} \
                 lacks the `describe` capability that answers one; remove the question from \
                 /config/kernel/council/{spec_name}.json",
                text.id, identity.model
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
                match (project_house_rules(rules.as_ref(), council.house_rules_tokens), rules) {
                    (Some(body), Some(rules)) => {
                        let slot = Slot::HouseRules { path: rules.path, tokens: council.house_rules_tokens };
                        Some((house_rules_id(&body)?, slot, body))
                    }
                    _ => None,
                }
            }
        };
        let reads = labels.len() + usize::from(house.is_some()) + added.len();
        if reads as u64 > identity.limits.contexts_per_decision {
            let mut named = labels.to_vec();
            if house.is_some() {
                named.push("the house-rules context".to_string());
            }
            named.extend(added.iter().cloned());
            return Err(PrepareMiss(format!(
                "a decision on spec {spec_name} reads {reads} contexts ({}), and model {} on council \
                 server {server} reads at most {} per decision (limits.contexts_per_decision)",
                named.join(", "),
                identity.model,
                identity.limits.contexts_per_decision
            )));
        }
        self.ensure_spec(&link, &spec_body, &spec_id).await?;

        let mut contexts = Vec::with_capacity(reads - added.len());
        for label in labels {
            let context_id = resolve_label(kernel, label)?;
            let blocks = kernel.blocks().block_snapshots(context_id).map_err(|e| {
                PrepareMiss(format!("council context \"{label}\" cannot be read: {e}"))
            })?;
            let body = project(label, &blocks, &*kernel.kernel_db().lock())
                .map_err(|e| PrepareMiss(format!("council context \"{label}\" cannot be projected: {e}")))?;
            let slot = Slot::Label(label.clone());
            let (seat, head) = self.hold(&link, &spec_body, slot, label, context_id, body).await?;
            contexts.push(PreparedContext { label: label.clone(), context_id, seat, head, house_rules: false });
        }
        if let Some((context_id, slot, body)) = house {
            let (seat, head) = self.hold(&link, &spec_body, slot, HOUSE_RULES_LABEL, context_id, body).await?;
            contexts.push(PreparedContext { label: HOUSE_RULES_LABEL.to_string(), context_id, seat, head, house_rules: true });
        }
        self.pin_seats(&link, &server_identity);

        let spec = spec_body;
        drop(link);
        Ok(Prepared { client, endpoint, identity, spec, spec_id, contexts })
    }

    /// Builds the shadow `shadow` of the seat labeled `seat` on `server`'s
    /// served model `model` as a queue seat for the spec named `spec_name`, so the server holds its
    /// dialogue before the next question about it, and returns the seat and
    /// its head. Sends nothing when the server already holds this seat. Each call waits for a slot
    /// no later than `deadline`, adding its wait to `waits`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn prime_shadow(
        &self,
        kernel: &crate::Kernel,
        server: &str,
        model: Option<&str>,
        spec_name: &str,
        shadow: ContextId,
        seat: &str,
        deadline: tokio::time::Instant,
        waits: &SlotWaits,
    ) -> Result<(Box<Context>, SnapshotId), PrepareMiss> {
        let _serial = self.shadow_serial.lock().await;
        let (endpoint, key) = endpoint_of(kernel, server)?;
        let client = MkClient::new(server, SHADOW_PRIME_TIMEOUT).map_err(|e| self.failed(&key, server, "client", e))?;
        let deadline_ms = deadline.saturating_duration_since(tokio::time::Instant::now()).as_millis() as u64;
        let mut link =
            Link { client: &client, server, key, endpoint, deadline, deadline_ms, waits, owner: pin_owner(kernel.id()), model: String::new() };
        let identity = self.identity(&link).await?;
        link.model = self.served(&link, &identity, model)?.model;
        let (spec_body, spec_id) = read_spec(kernel, spec_name).await?;
        self.ensure_spec(&link, &spec_body, &spec_id).await?;
        let blocks = kernel
            .blocks()
            .block_snapshots(shadow)
            .map_err(|e| PrepareMiss(format!("shadow {shadow} cannot be read: {e}")))?;
        let body = project_shadow(&blocks, &*kernel.kernel_db().lock())
            .map_err(|e| PrepareMiss(format!("shadow {shadow} cannot be projected: {e}")))?;
        let label = format!("shadow of {seat}");
        let held = self.hold(&link, &spec_body, Slot::Shadow(shadow), &label, shadow, body).await?;
        self.pin_seats(&link, &identity);
        Ok(held)
    }

    /// Makes the server hold the queue seat for `spec`
    /// ([`super::seat::queue_seat`]) built from `body`, the projection of
    /// `context_id`, with `POST /council/v2/contexts`, and returns the seat
    /// and its head. Sends nothing when the server already holds this seat.
    /// The seat replaces whatever `slot` held before on this server and spec.
    async fn hold(
        &self,
        link: &Link<'_>,
        spec: &Spec,
        slot: Slot,
        label: &str,
        context_id: ContextId,
        body: Context,
    ) -> Result<(Box<Context>, SnapshotId), PrepareMiss> {
        let seat = super::seat::queue_seat(body, spec).map_err(|e| {
            PrepareMiss(format!("council context \"{label}\" has no council v2 seat for spec {}: {e}", spec.name))
        })?;
        let hash = body_hash(&seat)?;
        let key = (link.key.clone(), link.model.clone(), slot, spec.name.clone());
        let held =
            self.state.lock().contexts.get(&key).filter(|h| h.id == context_id && h.body == hash).map(|h| h.head.clone());
        if let Some(head) = held {
            return Ok((Box::new(seat), head));
        }
        let built = link
            .send(|| async { link.client.council().build_context(&link.model, &seat, false).await })
            .await?
            .map_err(|e| self.failed(&link.key, link.server, &format!("build of context \"{label}\""), e))?;
        self.state.lock().contexts.insert(key, Held { id: context_id, body: hash, head: built.head.clone() });
        Ok((Box::new(seat), built.head))
    }

    /// Hands the server's pinner the heads of every seat this kernel
    /// built on it, starting the pinner on the first call. A server without
    /// the `pins` capability pins nothing.
    fn pin_seats(&self, link: &Link<'_>, identity: &ServerIdentity) {
        if !identity.capabilities.contains(&Capability::Pins) {
            return;
        }
        let mut state = self.state.lock();
        let heads = state.seat_heads(&link.key);
        if heads.is_empty() {
            return;
        }
        if let Some(pins) = state.pins.get(&link.key) {
            pins.send_if_modified(|set| {
                let changed = *set != heads;
                *set = heads;
                changed
            });
            return;
        }
        let (tx, rx) = tokio::sync::watch::channel(heads);
        state.pins.insert(link.key.clone(), tx);
        let pinner = Pinner {
            server: link.server.to_string(),
            key: link.key.clone(),
            endpoint: link.endpoint.clone(),
            owner: link.owner.clone(),
            state: Arc::downgrade(&self.state),
            kernel: self.kernel.clone(),
            renew: Duration::from_millis(self.pin_renew_ms.load(std::sync::atomic::Ordering::Relaxed)),
        };
        tokio::spawn(pinner.run(rx));
    }

    /// Lets this sync's pinners reach `kernel`, so each forgets the seats
    /// nothing current reaches ([`Self::forget_unreachable`]) before it
    /// sends its set. A serving kernel calls it once, at startup.
    pub(crate) fn serve(&self, kernel: &Arc<crate::Kernel>) {
        let _ = self.kernel.set(Arc::downgrade(kernel));
    }

    /// Forgets every held body that the gate config and the kernel's
    /// contexts no longer reach ([`reachable`]), so the next pin `PUT`
    /// leaves it out and the server's own expiry decides what happens to
    /// it. A gate config that does not load forgets nothing.
    pub(crate) async fn forget_unreachable(&self, kernel: &crate::Kernel) {
        let config = match crate::kj::gate_policy::load_config(kernel.vfs()).await {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!(error = %e, "council seats are kept: the gate config does not load");
                return;
            }
        };
        let held: Vec<(HeldKey, Held)> = self.state.lock().contexts.iter().map(|(k, h)| (k.clone(), h.clone())).collect();
        let mut gone = Vec::new();
        for (key, held) in held {
            if !reachable(kernel, &config, &key, held.id).await {
                gone.push((key, held.head));
            }
        }
        if gone.is_empty() {
            return;
        }
        let mut state = self.state.lock();
        for (key, head) in &gone {
            // A body built since the check is the slot's current one.
            if state.contexts.get(key).is_some_and(|h| h.head == *head) {
                state.contexts.remove(key);
            }
        }
        tracing::info!(seats = gone.len(), "council seats forgotten: nothing current reaches them");
    }

    /// Pinners started after this renew an unchanged set every `renew`.
    #[cfg(test)]
    pub(crate) fn set_pin_renew(&self, renew: Duration) {
        self.pin_renew_ms.store(renew.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// Forgets that `spec_id` was posted, so the next `prepare_labels` for it
    /// posts it again. Call it after a decision's 404 names the spec.
    pub(crate) fn invalidate_spec(&self, spec_id: &SpecId) {
        self.state.lock().specs.retain(|(_, id)| id != spec_id);
    }

    /// The server's council v2 identity, asked once per server.
    async fn identity(&self, link: &Link<'_>) -> Result<ServerIdentity, PrepareMiss> {
        if let Some(i) = self.state.lock().identities.get(&link.key) {
            return Ok(i.clone());
        }
        let identity = link
            .send(|| async { link.client.council().identity().await })
            .await?
            .map_err(|e| self.failed(&link.key, link.server, "council v2 identity", e))?;
        self.state.lock().identities.insert(link.key.clone(), identity.clone());
        Ok(identity)
    }

    /// The served model `model` names on the server, by its `model` or an
    /// alias; `None` names the only one, and every request sends its
    /// `model`. A model that is not `ready` is a miss, and its identity is
    /// asked again on the next prepare.
    fn served(&self, link: &Link<'_>, identity: &ServerIdentity, model: Option<&str>) -> Result<ServedModel, PrepareMiss> {
        let server = link.server;
        let names: Vec<&str> = identity.models.iter().map(|m| m.model.as_str()).collect();
        let served = match (model, identity.models.as_slice()) {
            (Some(name), _) => identity.route(name).map_err(|e| e.message),
            (None, [only]) => Ok(only),
            (None, all) => Err(format!("model is required: this server serves {} models", all.len())),
        }
        .map_err(|why| PrepareMiss(format!("council server {server}: {why}; it serves {}", names.join(", "))))?;
        if served.status != ModelStatus::Ready {
            self.state.lock().identities.remove(&link.key);
            return Err(PrepareMiss(format!(
                "council server {server}: model {} is {}, not ready",
                served.model,
                status_word(served.status)
            )));
        }
        Ok(served.clone())
    }

    async fn ensure_spec(&self, link: &Link<'_>, spec: &Spec, spec_id: &SpecId) -> Result<(), PrepareMiss> {
        let key = (link.key.clone(), spec_id.clone());
        if self.state.lock().specs.contains(&key) {
            return Ok(());
        }
        link.send(|| async { link.client.council().post_spec(spec).await })
            .await?
            .map_err(|e| self.failed(&link.key, link.server, &format!("POST of spec \"{}\"", spec.name), e))?;
        self.state.lock().specs.insert(key);
        Ok(())
    }

    /// A call to `server`, whose origin is `key`, failed: forget its
    /// identity and posted specs, so the next `prepare_labels` asks again,
    /// and name the failure.
    fn failed(&self, key: &str, server: &str, what: &str, e: MkError) -> PrepareMiss {
        let mut state = self.state.lock();
        state.identities.remove(key);
        state.specs.retain(|(s, _)| s != key);
        PrepareMiss(format!("council server {server}: {what} failed: {e}"))
    }
}

/// Sends one server's pin set: the whole set whenever it changes, and
/// again every `renew` while it does not, each under [`PIN_TTL`]. Runs
/// until the [`CouncilSync`] that started it is dropped.
struct Pinner {
    /// The server as configured.
    server: String,
    /// The server's origin, which keys `state`.
    key: String,
    endpoint: Arc<Endpoint>,
    owner: String,
    state: Weak<parking_lot::Mutex<State>>,
    kernel: Arc<std::sync::OnceLock<Weak<crate::Kernel>>>,
    renew: Duration,
}

impl Pinner {
    /// Each time the set changes, and every `renew` while it does not:
    /// forget the seats nothing current reaches, when the kernel is
    /// shared, then send the heads still held.
    async fn run(self, mut set: tokio::sync::watch::Receiver<BTreeSet<String>>) {
        let client = match MkClient::new(&self.server, PIN_TIMEOUT) {
            Ok(client) => client,
            Err(e) => {
                tracing::warn!(server = %self.server, error = %e, "council seats are not pinned: no client for the server");
                return;
            }
        };
        loop {
            set.borrow_and_update();
            if let Some(kernel) = self.kernel.get().and_then(Weak::upgrade) {
                kernel.council_sync().forget_unreachable(&kernel).await;
            }
            let Some(state) = self.state.upgrade() else { return };
            let heads = {
                let state = state.lock();
                let heads = state.seat_heads(&self.key);
                // Record the set sent without waking this pinner, so a
                // later change is measured against it.
                if let Some(pins) = state.pins.get(&self.key) {
                    pins.send_if_modified(|sent| {
                        *sent = heads.clone();
                        false
                    });
                }
                heads
            };
            drop(state);
            self.put(&client, heads.into_iter().collect()).await;
            tokio::select! {
                changed = set.changed() => if changed.is_err() { return },
                _ = tokio::time::sleep(self.renew) => {}
            }
        }
    }

    /// One `PUT` of the whole set. A snapshot the server does not hold is
    /// forgotten, so the next decision builds its seat again; every
    /// failure is logged and the set is sent again at the next change or
    /// renewal. An empty set drops the owner's pins with `DELETE`.
    async fn put(&self, client: &MkClient, heads: Vec<String>) {
        let deadline = tokio::time::Instant::now() + PIN_TIMEOUT;
        let waits = SlotWaits::default();
        if heads.is_empty() {
            let answer = mk_call(&self.endpoint, deadline, &waits, || async { client.council().delete_pins(&self.owner).await }).await;
            match answer {
                Ok((_slot, Ok(()))) => tracing::info!(server = %self.server, owner = %self.owner, "council pins dropped: no seat is current"),
                Ok((_slot, Err(e))) if e.status() == Some(404) => {}
                Ok((_slot, Err(e))) => tracing::warn!(server = %self.server, owner = %self.owner, error = %e, "council pins are not dropped"),
                Err(no_slot) => tracing::warn!(server = %self.server, owner = %self.owner, endpoint = %no_slot.endpoint, why = %no_slot.why, "council pins are not dropped: no slot at the server"),
            }
            return;
        }
        let body = PinsPut { snapshots: heads, ttl_s: PIN_TTL.as_secs() };
        let answer = mk_call(&self.endpoint, deadline, &waits, || async { client.council().put_pins(&self.owner, &body).await }).await;
        let pinned = match answer {
            Ok((_slot, Ok(pinned))) => pinned,
            Ok((_slot, Err(e))) => {
                tracing::warn!(server = %self.server, owner = %self.owner, seats = body.snapshots.len(), error = %e, "council seats are not pinned; the set is sent again at the next change or renewal");
                return;
            }
            Err(no_slot) => {
                tracing::warn!(server = %self.server, owner = %self.owner, endpoint = %no_slot.endpoint, why = %no_slot.why, "council seats are not pinned: no slot at the server");
                return;
            }
        };
        let gone: Vec<String> = pinned.snapshots.into_iter().filter(|p| !p.held).map(|p| p.id).collect();
        tracing::info!(server = %self.server, owner = %self.owner, seats = body.snapshots.len(), gone = gone.len(), expires_in_s = ?pinned.expires_in_s, "council seats pinned");
        if gone.is_empty() {
            return;
        }
        let Some(state) = self.state.upgrade() else { return };
        state.lock().contexts.retain(|(server, _, _, _), held| !(*server == self.key && gone.iter().any(|g| g == held.head.as_str())));
    }
}

/// Builds the council seats of every live context whose type has a council
/// and whose binding has a `gate` slot, so the first decision finds them held
/// and pinned: for each such context and each of its council's specs,
/// `[council] contexts` and the spec's own, and the seats that depend on the
/// context ([`plan_context_seats`]). The servers are the slots', so a config
/// with no instance bound to a `gate` slot builds nothing. An absent config
/// and each failure are logged and leave the seats to the first decision.
pub(crate) async fn build_seats(kernel: &crate::Kernel) {
    let live: Vec<ContextId> = {
        let db = kernel.kernel_db().lock();
        match db.list_active_contexts() {
            Ok(rows) => rows.into_iter().map(|r| r.context_id).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "council seats are not built at startup: the contexts do not list");
                return;
            }
        }
    };
    let mut done: HashSet<(String, String, Vec<String>)> = HashSet::new();
    for context in live {
        let builds = match plan_context_seats(kernel, context).await {
            Ok((builds, _)) => builds,
            Err(cause) => {
                tracing::warn!(context = %context.short(), %cause, "council seats are not planned at startup");
                continue;
            }
        };
        let builds: Vec<SeatBuild> = builds
            .into_iter()
            .filter(|b| b.seat.is_some() || done.insert((b.council.server.clone(), b.spec.clone(), b.labels.clone())))
            .collect();
        build_context_seats(kernel, context, builds).await;
    }
}

/// One decision whose seats [`build_context_seats`] builds ahead: the
/// council config it goes to, the spec, the labels it reads, the seat whose
/// house rules it reads, and the contexts its caller adds after them.
pub(crate) struct SeatBuild {
    council: CouncilConfig,
    pub(crate) spec: String,
    pub(crate) labels: Vec<String>,
    seat: Option<ContextId>,
    added: Vec<String>,
}

impl SeatBuild {
    /// The council server the seats are built on.
    pub(crate) fn server(&self) -> &str {
        &self.council.server
    }

    /// Whether the decision also reads the house rules.
    pub(crate) fn house_rules(&self) -> bool {
        self.seat.is_some()
    }
}

/// What the gate's decisions for `context` read on the seats that depend
/// on it: under each of its council's shell and program specs, the labels
/// along the reviewer chain of its performer and the house rules of its
/// workspace, routed to the server its binding's `gate` slot names; each
/// observing reviewer context alone under `direction-check`; and, when its
/// binding has an `mk` judge, the judge read's labels and house rules on the
/// judge's server. Empty when its type has no council or its binding has no
/// `gate` slot. Waits on no server.
///
/// The second value says why the reviewer chain is left out: a context
/// with no performer, or a chain that does not resolve yet. The labels are
/// then `[council] contexts` and the spec's own, which the decision reads
/// too, so every seat built is one a decision reads.
pub(crate) async fn plan_context_seats(
    kernel: &crate::Kernel,
    context: ContextId,
) -> Result<(Vec<SeatBuild>, Option<String>), String> {
    use crate::kj::gate_policy::CouncilCase;
    let config = crate::kj::gate_policy::load_config(kernel.vfs())
        .await
        .map_err(|e| format!("the gate config does not load: {e}"))?;
    let db = kernel.kernel_db().lock();
    let row = db
        .get_context(context)
        .map_err(|e| format!("context {} cannot be read: {e}", context.short()))?
        .ok_or_else(|| format!("context {} has no row", context.short()))?;
    let Some(council) = config.council_for(Some(&row.context_type)) else { return Ok((Vec::new(), None)) };
    let (routed, chain) = match super::gate::seat_route(&db, council, context, row.played_by)? {
        super::gate::SeatRoute::NoCouncil => return Ok((Vec::new(), None)),
        super::gate::SeatRoute::Council(routed, chain) => (routed, chain),
    };
    let council = &routed;
    let (chain, without_chain) = match chain {
        Ok(chain) => (chain, None),
        Err(cause) => (super::reviewer_contexts::ReviewerContextChain::default(), Some(cause)),
    };
    let seat = council.house_rules.then_some(context);
    let long = CouncilConfig { deadline_ms: SHADOW_PRIME_TIMEOUT.as_millis() as u64, ..council.clone() };
    let shell = super::gate::shell_spec(council);
    let program = council.specs.iter().find(|s| s.case == CouncilCase::Program);
    let mut builds: Vec<SeatBuild> = shell
        .into_iter()
        .chain(program)
        .map(|spec| SeatBuild {
            council: long.clone(),
            spec: spec.name.clone(),
            labels: chain.decision_labels(council, spec),
            seat,
            added: Vec::new(),
        })
        .collect();
    for label in &chain.observing {
        builds.push(SeatBuild {
            council: long.clone(),
            spec: super::observe::DIRECTION_CHECK.to_string(),
            labels: vec![label.clone()],
            seat: None,
            added: Vec::new(),
        });
    }
    let judge = super::shadow::judge_server(&db, context)
        .map_err(|e| format!("the judge slot of context {} cannot be read: {e}", context.short()))?;
    if let (Some(judge), Some(spec)) = (judge, shell) {
        let seat_name = row.label.clone().unwrap_or_else(|| context.short());
        builds.push(SeatBuild {
            council: CouncilConfig { server: judge.server, model: Some(judge.model), ..long.clone() },
            spec: spec.name.clone(),
            labels: super::shadow::judge_labels(council, spec),
            seat,
            added: vec![super::shadow::shadow_label(&seat_name)],
        });
    }
    Ok((builds, without_chain))
}

/// Builds each planned decision's seats ([`plan_context_seats`]) with the
/// prepare the decision runs, so the decision finds them held, and pins
/// them. Each failure is logged.
pub(crate) async fn build_context_seats(kernel: &crate::Kernel, context: ContextId, builds: Vec<SeatBuild>) {
    let sync = kernel.council_sync();
    for build in builds {
        let deadline = tokio::time::Instant::now() + SHADOW_PRIME_TIMEOUT;
        let waits = SlotWaits::default();
        let server = build.server().to_string();
        let prepared = sync
            .prepare_labels(kernel, &build.council, &build.spec, &build.labels, build.seat, &build.added, deadline, &waits)
            .await;
        match prepared {
            Ok(p) => tracing::info!(
                context = %context.short(), %server, spec = %build.spec, seats = p.contexts.len(),
                house_rules = p.house_rules().is_some(), "council seats built ahead of the first decision"
            ),
            Err(PrepareMiss(cause)) => {
                tracing::warn!(context = %context.short(), %server, spec = %build.spec, %cause, "council seats are not built ahead")
            }
        }
    }
}

/// Whether anything current still reaches the body held under `key` for
/// the context `id`: its spec is one a council in `config` declares, or
/// `direction-check` under `reviewer_contexts`, and
///
/// - a label names a live context, `id` itself, that a council reads:
///   one of its `contexts` or a spec's, or, under `reviewer_contexts`, the
///   council context of a character who is not retired;
/// - house rules are read by a council with that token budget, and the
///   file is still there (an edit replaces the body in place);
/// - a shadow is not archived.
async fn reachable(kernel: &crate::Kernel, config: &crate::kj::gate_policy::GateConfig, key: &HeldKey, id: ContextId) -> bool {
    use crate::vfs::VfsOps;
    let (_, _, slot, spec) = key;
    let councils: Vec<&CouncilConfig> = config.councils().collect();
    let reviewer_contexts = councils.iter().any(|c| c.reviewer_contexts);
    if !councils.iter().any(|c| c.specs.iter().any(|s| s.name == *spec))
        && !(reviewer_contexts && spec == super::observe::DIRECTION_CHECK)
    {
        return false;
    }
    match slot {
        Slot::Label(label) => {
            let db = kernel.kernel_db().lock();
            let live = matches!(db.find_context_by_label(label), Ok(Some(row)) if row.context_id == id);
            let named = councils.iter().any(|c| c.contexts.contains(label) || c.specs.iter().any(|s| s.contexts.contains(label)));
            let on_a_chain = || {
                reviewer_contexts
                    && label
                        .strip_prefix(super::projection::COUNCIL_CONTEXT_PREFIX)
                        .and_then(|name| db.get_character_by_name(name).ok().flatten())
                        .is_some_and(|sheet| sheet.retired_at.is_none())
            };
            live && (named || on_a_chain())
        }
        Slot::HouseRules { path, tokens } => {
            councils.iter().any(|c| c.house_rules && c.house_rules_tokens == *tokens)
                && kernel.vfs().exists(std::path::Path::new(path)).await
        }
        Slot::Shadow(shadow) => {
            matches!(kernel.kernel_db().lock().get_context(*shadow), Ok(Some(row)) if row.archived_at.is_none())
        }
    }
}

fn body_hash(body: &impl serde::Serialize) -> Result<[u8; 32], PrepareMiss> {
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
    let text = String::from_utf8(bytes).map_err(|e| PrepareMiss(format!("council spec file {path} is not UTF-8: {e}")))?;
    let spec = super::parse_spec(&text).map_err(|e| PrepareMiss(format!("council spec file {path} is not a spec: {e}")))?;
    let id = SpecId::parse(spec.id()).map_err(|e| PrepareMiss(format!("council spec file {path} has no spec id: {e}")))?;
    Ok((spec, id))
}

#[cfg(test)]
pub(super) mod mock {
    //! A mock council server on 127.0.0.1 for the council's kernel tests.

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

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
        pub(crate) body: String,
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

        /// The body of every seat build, `POST /council/v2/contexts`.
        pub(crate) fn builds(&self) -> Vec<serde_json::Value> {
            self.bodies("/council/v2/contexts")
        }

        pub(crate) fn count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        /// Answer each `POST /council/v2/decisions` with `f` of its body.
        pub(crate) fn on_decide(&self, f: impl Fn(&str) -> Reply + Send + 'static) {
            *self.decide.lock().unwrap() = Some(Box::new(f));
        }

        /// The bodies of every `method` request whose path starts with
        /// `prefix`, in order.
        pub(crate) fn bodies_of(&self, method: &str, prefix: &str) -> Vec<serde_json::Value> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.method == method && c.path.starts_with(prefix))
                .map(|c| serde_json::from_str(&c.body).unwrap())
                .collect()
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

    /// The mock's identity: one served model, `m`, ready. The server and
    /// its model both list `capabilities`.
    pub(crate) fn identity_listing(capabilities: serde_json::Value) -> String {
        json!({"models": [{"model": "m", "status": "ready", "weight_hash": "w", "tokenizer_hash": "t", "template": "tpl",
                           "engine": "e",
                           "limits": {"context_tokens": 1, "decision_context_tokens": 1, "snapshot_bytes": 1,
                                      "contexts_per_decision": 4, "default_timeout_ms": 1000},
                           "capabilities": capabilities}],
               "limits": {"case_bytes": 1, "choice_options": 8},
               "capabilities": capabilities})
        .to_string()
    }

    /// The happy path for `c`, with `capabilities` in the identity. Seat
    /// `n` is built with head [`snap`]`(n)`, and every pin is held.
    fn default_reply(c: &Captured, builds: &mut u64, capabilities: &serde_json::Value) -> Reply {
        match (c.method.as_str(), c.path.as_str()) {
            ("GET", "/council/v2/identity") => reply(200, identity_listing(capabilities.clone())),
            ("POST", "/council/v2/contexts") => {
                *builds += 1;
                reply(
                    200,
                    json!({"model": "m", "head": snap(*builds), "usage": {"prompt": 1, "kept": 0, "fed": 1, "completion": 0, "reasoning": 0},
                           "from": null, "dry_run": false,
                           "snapshots": [{"id": snap(*builds), "message": 1, "tokens": 1}]})
                    .to_string(),
                )
            }
            ("PUT", p) if p.starts_with("/council/v2/pins/") => {
                let body: serde_json::Value = serde_json::from_str(&c.body).unwrap();
                let held: Vec<serde_json::Value> =
                    body["snapshots"].as_array().unwrap().iter().map(|id| json!({"id": id, "held": true})).collect();
                reply(200, json!({"owner": p.rsplit('/').next().unwrap(), "snapshots": held, "expires_in_s": body["ttl_s"]}).to_string())
            }
            ("POST", "/council/v2/specs") => {
                let spec: serde_json::Value = serde_json::from_str(&c.body).unwrap();
                let id = gogi_proto::spec_id_of(&spec).unwrap();
                reply(200, json!({"spec_id": id, "spec": spec, "template": "t"}).to_string())
            }
            _ => reply(404, json!({"error": {"type": "not_found", "message": "no such route"}}).to_string()),
        }
    }

    /// A council v2 server that does not list `pins`.
    pub(crate) async fn serve() -> Mock {
        serve_with(json!(["leave_one_out"])).await
    }

    /// A council v2 server that lists `pins` and holds every pin it is
    /// sent.
    pub(crate) async fn serve_pinning() -> Mock {
        serve_with(json!(["leave_one_out", "pins"])).await
    }

    async fn serve_with(capabilities: serde_json::Value) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let forced: Arc<Mutex<VecDeque<(String, Reply)>>> = Arc::default();
        let decide: Decide = Arc::default();
        let (log, queue, decider) = (seen.clone(), forced.clone(), decide.clone());
        tokio::spawn(async move {
            let mut builds = 0u64;
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
                let mut first = head.lines().next().unwrap().split(' ');
                let method = first.next().unwrap().to_string();
                let path = first.next().unwrap().to_string();
                let captured = Captured {
                    method,
                    path,
                    body: String::from_utf8_lossy(&buf[head_end..]).to_string(),
                };
                let forced_reply = {
                    let mut q = queue.lock().unwrap();
                    q.iter().position(|(m, _)| *m == captured.method).and_then(|i| q.remove(i))
                };
                let decided = (captured.path == "/council/v2/decisions")
                    .then(|| decider.lock().unwrap().as_ref().map(|f| f(&captured.body)))
                    .flatten();
                let r = match (forced_reply, decided) {
                    (Some((_, r)), _) => r,
                    (None, Some(r)) => r,
                    (None, None) => default_reply(&captured, &mut builds, &capabilities),
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

    use super::mock::{Mock, reply, serve, serve_pinning, snap};
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
    /// gogi's example spec, which holds the `text` question `effect`.
    const SPEC_WITH_TEXT: &str = include_str!("../../../kaijutsu-mk/tests/fixtures/spec.json");

    struct Rig {
        kernel: Arc<Kernel>,
        mock: Mock,
        council: CouncilConfig,
        spec: CouncilSpec,
        reviewer: ContextId,
        config: tempfile::TempDir,
    }

    async fn rig() -> Rig {
        rig_on(serve().await).await
    }

    /// The rig with `mock` as its council server.
    async fn rig_on(mock: Mock) -> Rig {
        let kernel = Arc::new(Kernel::new_ephemeral("council-sync").await);
        let config = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config.path().join("council")).unwrap();
        std::fs::write(config.path().join("council/shell-gate.json"), SPEC).unwrap();
        kernel
            .vfs()
            .mount(kaijutsu_types::paths::CONFIG_ROOT, LocalBackend::new(config.path()))
            .await;
        let reviewer = live_context(&kernel, "reviewer");
        append_dialogue(&kernel, reviewer, &["never rm -rf the repo", "understood"]);
        let spec = CouncilSpec { name: "shell-gate".into(), case: CouncilCase::Shell, contexts: Vec::new(), require_agree: None };
        let council = CouncilConfig {
            server: mock.base.clone(),
            model: None,
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
            self.kernel.council_sync().prepare_labels(&self.kernel, &self.council, &self.spec.name, &self.council.contexts, None, &[], far(), &SlotWaits::default()).await
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

    /// The reviewer's dialogue: a question, a thought, and an answer.
    fn converse(r: &Rig) {
        let ids = r.kernel.blocks().block_snapshots(r.reviewer).unwrap();
        let mut last = ids.last().map(|b| b.id.clone());
        for (role, kind, text) in [
            (kaijutsu_types::Role::User, kaijutsu_types::BlockKind::Text, "git push?"),
            (kaijutsu_types::Role::Model, kaijutsu_types::BlockKind::Thinking, "it publishes main"),
            (kaijutsu_types::Role::Model, kaijutsu_types::BlockKind::Text, "ask first"),
        ] {
            last = Some(super::super::projection::fixtures::append(
                &r.kernel, r.reviewer, last.as_ref(), role, kind, kaijutsu_types::Status::Done, text,
            ));
        }
    }

    /// Answers each decision with `allow` from every context it sends,
    /// each read named by its index.
    fn decide_allow(r: &Rig) {
        r.mock.on_decide(|body| {
            let request: gogi_proto::decision::DecisionRequest = serde_json::from_str(body).unwrap();
            reply(200, super::super::gate::test_support::answer(&request, &[[-0.01, -5.0, -6.0]]).to_string())
        });
    }

    /// A decision is council v2: the spec under `/council/v2/specs`, each
    /// context built as a queue
    /// seat for the spec with `POST /council/v2/contexts` (the projection's
    /// messages, the system message first and each turn's `reasoning` on
    /// its message, then the task and the queue calls), and the decision
    /// under `/council/v2/decisions`, each seat sent whole, with no
    /// `think`. The answer names the seat by its index, which reads back to
    /// the reviewer.
    ///
    /// Falsified by a sync that sends a decision without the seat's
    /// messages, or maps an index read to no context.
    #[tokio::test]
    async fn a_decision_sends_each_seat_whole() {
        let r = rig_on(serve_pinning().await).await;
        converse(&r);
        decide_allow(&r);
        let p = r.prepare().await.expect("prepared");
        let council = &r.council;
        let request = super::super::gate::decision_request(&p, council, serde_json::Value::from("git push origin main"));
        let answer = super::super::gate::ask_within(&r.kernel, &p, &request, council, far(), &SlotWaits::default()).await;
        let sent = r.mock.bodies("/council/v2/decisions");
        assert_eq!(sent.len(), 1);
        let messages = sent[0]["contexts"][0]["messages"].as_array().expect("the seat's messages in the decision");
        let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(
            roles,
            ["system", "user", "assistant", "user", "assistant", "user", "assistant", "tool", "assistant", "tool", "assistant"]
        );
        assert!(messages[0]["content"].as_str().unwrap().contains("\"reviewer\""), "{}", sent[0]);
        assert_eq!(messages[4], json!({"role": "assistant", "content": "ask first", "reasoning": "it publishes main"}));
        assert_eq!(messages.last().unwrap()["tool_calls"][0]["name"], super::super::seat::QUEUE_TOOL);
        assert_eq!(sent[0]["contexts"][0]["queue"], super::super::seat::QUEUE_TOOL);
        assert!(sent[0].get("think").is_none(), "{}", sent[0]);

        assert_eq!(r.mock.calls("GET"), ["/council/v2/identity"]);
        assert_eq!(r.mock.calls("POST"), ["/council/v2/specs", "/council/v2/contexts", "/council/v2/decisions"]);
        let built = r.mock.builds();
        assert_eq!(built, [json!({"model": "m", "context": sent[0]["contexts"][0]})], "the decision sends the seat it built, and nothing else");
        assert_eq!(sent[0]["model"], "m", "the decision names the served model");
        assert_eq!(sent[0]["contexts"].as_array().unwrap().len(), 1);
        assert_eq!(p.contexts[0].context_id, r.reviewer);
        assert_eq!(p.contexts[0].head.as_str(), snap(1), "the head the build reported");

        let response = answer.expect("answered");
        gogi_proto::math::verify(&response, &request.expect()).expect("the numbers recompute");
        assert_eq!(response.reads[0].context, Some(0));
        assert_eq!(p.read_context(0).map(|c| c.context_id), Some(r.reviewer));
    }

    /// One council context read under two specs is two queue seats, each with its spec's answers schema, and an unchanged seat
    /// is not built again.
    ///
    /// Falsified by one seat that each spec overwrites in turn, or a seat
    /// built again for every decision.
    #[tokio::test]
    async fn one_seat_per_spec() {
        let r = rig_on(serve_pinning().await).await;
        std::fs::write(r.config.path().join("council/program-gate.json"), crate::config_seed::DEFAULT_COUNCIL_PROGRAM_GATE)
            .unwrap();
        let waits = SlotWaits::default();
        let prepare = |spec: &'static str| {
            r.kernel.council_sync().prepare_labels(&r.kernel, &r.council, spec, &r.council.contexts, None, &[], far(), &waits)
        };
        let shell = prepare("shell-gate").await.expect("prepared");
        let program = prepare("program-gate").await.expect("prepared");
        let again = prepare("shell-gate").await.expect("prepared");
        assert_ne!(shell.contexts[0].head, program.contexts[0].head);
        assert_eq!(again.contexts[0].head, shell.contexts[0].head);
        let built = r.mock.bodies("/council/v2/contexts");
        assert_eq!(built.len(), 2, "each seat once");
        let answers = |built: &serde_json::Value| -> Vec<String> {
            let props = &built["context"]["tools"][0]["input_schema"]["properties"]["answers"]["properties"];
            props.as_object().unwrap().keys().cloned().collect()
        };
        assert_eq!(answers(&built[0]), ["undo", "verdict"]);
        assert_eq!(answers(&built[1]), ["network", "originals", "verdict"]);
    }

    /// Waits until the mock has seen `n` pin `PUT`s and returns their paths
    /// and bodies.
    async fn pin_puts(r: &Rig, n: usize) -> Vec<(String, serde_json::Value)> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let puts: Vec<(String, serde_json::Value)> = r
                .mock
                .calls("PUT")
                .into_iter()
                .filter(|p| p.starts_with("/council/v2/pins/"))
                .zip(r.mock.bodies_of("PUT", "/council/v2/pins/"))
                .collect();
            if puts.len() >= n || tokio::time::Instant::now() > deadline {
                return puts;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Every seat head this kernel built on a server is pinned under one
    /// owner named by the kernel's id, for an hour: the whole set when it
    /// changes, and the same set again at each renewal.
    ///
    /// Falsified by seats left unpinned, pins under another owner, a set
    /// that keeps a replaced head, or pins that are never renewed.
    #[tokio::test]
    async fn seats_are_pinned_under_the_kernel_and_renewed() {
        let r = rig_on(serve_pinning().await).await;
        r.kernel.council_sync().set_pin_renew(Duration::from_millis(300));
        let owner = format!("kernel-{}", r.kernel.id().to_hex());
        let first = r.prepare().await.expect("prepared");
        let puts = pin_puts(&r, 1).await;
        assert_eq!(puts.len(), 1, "{puts:?}");
        assert_eq!(puts[0].0, format!("/council/v2/pins/{owner}"));
        assert_eq!(puts[0].1, json!({"snapshots": [first.contexts[0].head.as_str()], "ttl_s": 3600}));

        r.grow("one more rule");
        let second = r.prepare().await.expect("prepared");
        assert_ne!(second.contexts[0].head, first.contexts[0].head);
        let puts = pin_puts(&r, 2).await;
        assert_eq!(puts[1].1["snapshots"], json!([second.contexts[0].head.as_str()]), "the set changed: the new head alone");

        let puts = pin_puts(&r, 3).await;
        assert_eq!(puts.len(), 3, "renewed: {puts:?}");
        assert_eq!(puts[2], puts[1], "the same set again");
    }

    /// A pin the server reports `held: false` names a seat it no longer
    /// holds, so the next decision builds that seat again.
    ///
    /// Falsified by a sync that keeps believing the server holds it.
    #[tokio::test]
    async fn a_seat_the_server_does_not_hold_is_built_again() {
        let r = rig_on(serve_pinning().await).await;
        r.mock.force("PUT", reply(200, json!({"owner": "k", "snapshots": [{"id": snap(1), "held": false}], "expires_in_s": 3600}).to_string()));
        r.prepare().await.expect("prepared");
        pin_puts(&r, 1).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while r.kernel.council_sync().state.lock().contexts.values().any(|h| h.head.as_str() == snap(1)) {
            assert!(tokio::time::Instant::now() < deadline, "the seat is still believed held");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        r.prepare().await.expect("prepared");
        assert_eq!(r.mock.bodies("/council/v2/contexts").len(), 2, "built again");
    }

    /// A server without the `pins` capability pins nothing.
    #[tokio::test]
    async fn a_server_without_pins_is_not_pinned() {
        let r = rig().await;
        r.prepare().await.expect("prepared");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(r.mock.calls("PUT").is_empty(), "{:?}", r.mock.calls("PUT"));
    }

    /// At startup the kernel builds and pins the council seats of each live
    /// context whose instance binds a `gate` slot: `[council] contexts` under
    /// each spec, on the slot's server.
    ///
    /// Falsified by seats that wait for the first decision, or by building on
    /// a server no gate slot names.
    #[tokio::test]
    async fn build_seats_builds_and_pins_the_configured_seats() {
        let r = rig_on(serve_pinning().await).await;
        let gate = format!(
            "[council]\ncontexts = [\"reviewer\"]\npool = {{ method = \"linear\", weights = \"uniform\" }}\ndeadline_ms = 5000\n\n[[council.spec]]\nname = \"shell-gate\"\ncase = \"shell\"\n\n\
             [[council.threshold]]\nspec = \"shell-gate\"\nallow_at = 0.98\nmass_floor = -0.05\n\n\
             [context_type.coder.council]\nenabled = true\n"
        );
        coder_on_the_mock_council(&r);
        std::fs::write(r.config.path().join("gate.toml"), gate).unwrap();
        let loaded = crate::kj::gate_policy::load_config(r.kernel.vfs()).await.expect("the gate config loads");
        assert_eq!(loaded.councils().count(), 1);
        build_seats(&r.kernel).await;
        assert_eq!(r.mock.bodies("/council/v2/contexts").len(), 1, "{:?}", r.mock.calls("POST"));
        assert_eq!(pin_puts(&r, 1).await.len(), 1);
    }

    /// A failed identity is a miss. A 404, from a server without council
    /// v2, is a miss too: there is no other API to fall back to.
    #[tokio::test]
    async fn a_failed_identity_is_a_miss() {
        for status in [500, 404] {
            let r = rig().await;
            r.mock.force("GET", reply(status, "boom"));
            let miss = r.prepare().await.err().expect("a miss");
            assert!(miss.0.contains("council v2 identity failed") && miss.0.contains(&status.to_string()), "{}", miss.0);
            assert_eq!(r.mock.calls("GET"), ["/council/v2/identity"]);
        }
    }

    #[tokio::test]
    async fn invalidate_spec_makes_the_next_prepare_post_the_spec_again() {
        let r = rig().await;
        let p = r.prepare().await.unwrap();
        r.prepare().await.unwrap();
        assert_eq!(r.mock.calls("POST"), ["/council/v2/specs", "/council/v2/contexts"]);
        r.kernel.council_sync().invalidate_spec(&p.spec_id);
        r.prepare().await.unwrap();
        assert_eq!(r.mock.calls("POST").last().unwrap(), "/council/v2/specs", "posted again");
        assert_eq!(r.mock.builds().len(), 1, "the seat is still held");
    }

    #[tokio::test]
    async fn the_first_prepare_gets_the_identity_posts_the_spec_and_builds_each_seat() {
        let r = rig().await;
        let p = r.prepare().await.expect("prepared");
        assert_eq!(r.mock.calls("GET"), ["/council/v2/identity"]);
        assert_eq!(r.mock.calls("POST"), ["/council/v2/specs", "/council/v2/contexts"]);
        let builds = r.mock.builds();
        assert_eq!(builds.len(), 1);
        let messages = builds[0]["context"]["messages"].as_array().unwrap();
        assert_eq!(messages[1..3].iter().map(|m| m["content"].as_str().unwrap()).collect::<Vec<_>>(), ["never rm -rf the repo", "understood"]);
        assert!(builds[0].get("dry_run").is_none());
        assert_eq!(p.contexts.len(), 1);
        assert_eq!(p.contexts[0].label, "reviewer");
        assert_eq!(p.contexts[0].context_id, r.reviewer);
        assert_eq!(p.contexts[0].head.as_str(), snap(1));
        assert_eq!(p.spec_id.as_str(), p.spec.id());
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
    async fn a_changed_context_builds_its_seat_again() {
        let r = rig().await;
        let first = r.prepare().await.unwrap();
        r.grow("and never force push");
        let second = r.prepare().await.unwrap();
        let builds = r.mock.builds();
        assert_eq!(builds.len(), 2);
        assert!(builds[1].to_string().contains("and never force push"), "{}", builds[1]);
        assert_ne!(second.contexts[0].head, first.contexts[0].head);
        assert_eq!(second.contexts[0].head.as_str(), snap(2));
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
        assert_eq!(r.mock.builds().len(), 0);
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

        r.grow("more guidance");
        r.mock.force("POST", reply(500, "boom"));
        let miss = r.prepare().await.err().expect("a miss");
        assert!(miss.0.contains(&r.mock.base), "{}", miss.0);
        r.prepare().await.expect("recovered");
        assert_eq!(r.mock.calls("GET").len(), 2, "refetched after the failure");
        let specs = r.mock.calls("POST").iter().filter(|p| *p == "/council/v2/specs").count();
        assert_eq!(specs, 2, "specs are posted again with it");
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
        super::mock::identity_listing(capabilities)
    }

    #[tokio::test]
    async fn a_text_question_without_describe_is_a_miss_naming_both() {
        let r = rig().await;
        std::fs::write(r.config.path().join("council/shell-gate.json"), SPEC_WITH_TEXT).unwrap();
        let miss = r.prepare().await.err().expect("a miss");
        assert!(miss.0.contains("`effect`") && miss.0.contains("`describe`"), "{}", miss.0);
        assert!(r.mock.calls("POST").is_empty(), "neither the spec nor a seat is sent");
    }

    /// With `describe`, a spec holding a `text` question is posted. Its
    /// queue seat cannot answer the `text` question with a letter, so the
    /// prepare misses there, naming the question.
    #[tokio::test]
    async fn a_text_question_with_describe_posts_and_has_no_seat() {
        let r = rig().await;
        std::fs::write(r.config.path().join("council/shell-gate.json"), SPEC_WITH_TEXT).unwrap();
        r.mock.force("GET", reply(200, identity_with(json!(["leave_one_out", "describe"]))));
        let miss = r.prepare().await.err().expect("a miss");
        assert!(miss.0.contains("text question `effect`"), "{}", miss.0);
        assert_eq!(r.mock.calls("POST"), ["/council/v2/specs"]);
    }

    #[tokio::test]
    async fn prepare_labels_reads_exactly_the_labels_given_in_order() {
        let r = rig().await;
        let amy = live_context(&r.kernel, "council-amy");
        append_dialogue(&r.kernel, amy, &["keep main green"]);
        let labels = vec!["reviewer".to_string(), "council-amy".to_string()];
        let p = r.kernel.council_sync().prepare_labels(&r.kernel, &r.council, "shell-gate", &labels, None, &[], far(), &SlotWaits::default()).await.unwrap();
        assert_eq!(p.contexts.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["reviewer", "council-amy"]);
        assert_eq!(p.contexts[1].context_id, amy);

        let one = vec!["council-amy".to_string()];
        let p = r.kernel.council_sync().prepare_labels(&r.kernel, &r.council, "shell-gate", &one, None, &[], far(), &SlotWaits::default()).await.unwrap();
        assert_eq!(p.contexts.len(), 1, "a single reviewer, without [council] contexts");
        assert_eq!(r.mock.builds().len(), 2, "each seat is built once");
    }

    /// A seat with a brief and narration, labeled `label`, working in `cwd`.
    /// A live `coder` context whose instance binds a `gate` slot on the mock
    /// council server, so the startup build and the context plans find a
    /// council for it.
    fn coder_on_the_mock_council(r: &Rig) -> ContextId {
        use crate::kernel_db::{BackendRow, CastRow, CastSlotRow, CharacterRow};
        use kaijutsu_types::PrincipalId;
        let character = PrincipalId::new();
        let db = r.kernel.kernel_db().lock();
        db.insert_character(&CharacterRow {
            principal_id: character, name: "council-coder".into(), created_at: 0, retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        })
        .unwrap();
        let backend = db
            .upsert_backend(&BackendRow {
                backend_id: kaijutsu_types::BackendId::new(),
                name: "council-mk".into(),
                kind: "mk".into(),
                base_url: Some(r.mock.base.clone()),
                api_key_env: None,
                api_key_file: None,
                key_optional: true,
                request_timeout_secs: None,
                idle_timeout_secs: None,
                max_concurrent: None,
                created_at: 0,
                created_by: PrincipalId::system(),
            })
            .unwrap();
        let cast = CastRow {
            cast_id: kaijutsu_types::CastId::new(),
            label: "coder-council".into(),
            description: None,
            created_at: 0,
            created_by: PrincipalId::system(),
        };
        db.insert_cast(&cast).unwrap();
        db.set_cast_slot(&CastSlotRow {
            cast_id: cast.cast_id,
            role: "gate".into(),
            backend_id: backend.backend_id,
            model: "m".into(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            effort: None,
            thinking_budget: None,
            thinking_style: None,
            loadout: None,
            extra: None,
        })
        .unwrap();
        db.set_character_cast(character, Some(cast.cast_id)).unwrap();
        drop(db);
        let coder = live_context(&r.kernel, "coder-seat");
        let db = r.kernel.kernel_db().lock();
        db.update_context_type(coder, "coder").unwrap();
        db.assign_stand_in_instance(coder, character).unwrap();
        coder
    }

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
        r.kernel.council_sync().prepare_labels(&r.kernel, &r.council, "shell-gate", &r.council.contexts, Some(seat), &[], far(), &SlotWaits::default()).await
    }

    /// The house-rules context is the first `AGENTS.md` found walking up
    /// from the seat's working directory, read through the kernel VFS, and
    /// read after the labels. Nothing the seat said or did reaches it: a
    /// brief, narration, a tool call, and later narration leave the body
    /// and the server's copy as they were.
    ///
    /// Falsified by a sync that reads only the working directory itself (the
    /// rules one level up never arrive), or one that projects the seat's
    /// blocks (the narration changes the seat and builds it again).
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
        let builds = r.mock.builds();
        assert_eq!(builds.len(), 2);
        let body = &builds[1];
        let messages = body["context"]["messages"].as_array().unwrap();
        assert!(messages[0]["content"].as_str().unwrap().contains("you know the house rules"), "{body}");
        assert_eq!(
            messages[1]["content"].as_str().unwrap(),
            "The house rules in /work/repo/AGENTS.md:\n\nBack up data before changing it."
        );
        assert!(!body.to_string().contains("WAL"), "the seat's narration stays out: {body}");

        append_to(&r, seat, kaijutsu_types::Role::Model, kaijutsu_types::BlockKind::ToolCall, "xxd main.db-wal");
        append_to(&r, seat, kaijutsu_types::Role::Model, kaijutsu_types::BlockKind::Text, "No backup exists yet.");
        append_to(&r, seat, kaijutsu_types::Role::User, kaijutsu_types::BlockKind::Text, "a later prompt");
        let again = prepare_seat(&r, seat).await.unwrap();
        assert_eq!(r.mock.builds().len(), 2, "narration and prompts send nothing");
        assert_eq!(again.house_rules().unwrap().head, p.house_rules().unwrap().head);
    }

    /// The held id comes from the body, so every seat under one `AGENTS.md`
    /// shares one held context, and an edited file is a new id.
    ///
    /// Falsified by an id derived from the seat's context id: the two seats
    /// hold two contexts and the second prepare builds another seat.
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
        assert_eq!(r.mock.builds().len(), 2, "the reviewer, and the rules once");

        write_rules(&r, "Back up data. Never push.").await;
        let p3 = prepare_seat(&r, one).await.unwrap();
        let h3 = p3.house_rules().unwrap();
        assert_ne!(h3.context_id, h1.context_id, "an edited file is a new id");
        assert_eq!(r.mock.builds().len(), 3);
    }

    /// An edit to the house rules replaces the house-rules seat in the pin
    /// set and in what the kernel remembers: the next pin `PUT` names the
    /// new head and not the old one.
    ///
    /// Falsified by a sync that keys a house-rules seat by its body: the
    /// old head stays pinned beside the new one, and the kernel remembers
    /// one more seat for each edit.
    #[tokio::test]
    async fn an_edit_to_the_house_rules_replaces_the_pinned_seat() {
        let mut r = rig_on(serve_pinning().await).await;
        r.council.house_rules = true;
        write_rules(&r, "Back up data before changing it.").await;
        let seat = seat_in(&r, "lane-a", Some("/work/repo/sub"));
        let first = prepare_seat(&r, seat).await.expect("prepared");
        let puts = pin_puts(&r, 1).await;
        let reviewer = first.contexts[0].head.as_str();
        let old = first.house_rules().unwrap().head.as_str();
        assert_eq!(puts[0].1["snapshots"], json!([reviewer, old]));

        write_rules(&r, "Back up data. Never push.").await;
        let second = prepare_seat(&r, seat).await.expect("prepared");
        let new = second.house_rules().unwrap().head.as_str();
        assert_ne!(new, old);
        let puts = pin_puts(&r, 2).await;
        assert_eq!(puts.len(), 2, "{puts:?}");
        assert_eq!(puts[1].1["snapshots"], json!([reviewer, new]), "the edit replaces the old head");
        assert_eq!(r.kernel.council_sync().state.lock().contexts.len(), 2, "the reviewer and the current rules");
    }

    /// A seat nothing current reaches leaves the pin set at the next pin
    /// `PUT` once the kernel is served: a label the gate config no longer
    /// names is forgotten, and a seat it names stays.
    ///
    /// Falsified by a pinner that sends every seat ever built: the dropped
    /// label's head stays in the set.
    #[tokio::test]
    async fn a_seat_the_config_no_longer_reads_leaves_the_pin_set() {
        let r = rig_on(serve_pinning().await).await;
        r.kernel.council_sync().serve(&r.kernel);
        let other = live_context(&r.kernel, "other");
        append_dialogue(&r.kernel, other, &["never delete originals"]);
        let gate = |label: &str| {
            format!(
                "[council]\ncontexts = [\"{label}\"]\npool = {{ method = \"linear\", weights = \"uniform\" }}\ndeadline_ms = 5000\n\n\
                 [[council.spec]]\nname = \"shell-gate\"\ncase = \"shell\"\n\n\
                 [context_type.coder.council]\nenabled = true\n"
            )
        };
        std::fs::write(r.config.path().join("gate.toml"), gate("reviewer")).unwrap();
        let first = r.prepare().await.expect("prepared");
        let puts = pin_puts(&r, 1).await;
        assert_eq!(puts[0].1["snapshots"], json!([first.contexts[0].head.as_str()]));

        std::fs::write(r.config.path().join("gate.toml"), gate("other")).unwrap();
        let labels = vec!["other".to_string()];
        let second = r
            .kernel
            .council_sync()
            .prepare_labels(&r.kernel, &r.council, "shell-gate", &labels, None, &[], far(), &SlotWaits::default())
            .await
            .expect("prepared");
        let puts = pin_puts(&r, 2).await;
        assert_eq!(puts.len(), 2, "{puts:?}");
        assert_eq!(puts[1].1["snapshots"], json!([second.contexts[0].head.as_str()]), "the reviewer seat is gone");
        assert_eq!(r.kernel.council_sync().state.lock().contexts.len(), 1);
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
        assert_eq!(r.mock.builds().len(), 1, "only the labeled context");
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

    /// Two spellings of one server share what this kernel believes it
    /// holds, as they share its endpoint: a prepare through
    /// `http://host:port/` after one through `http://host:port` sends
    /// nothing.
    ///
    /// Falsified by keying the identity, spec, or context caches on the raw
    /// `[council] server` string.
    #[tokio::test]
    async fn two_spellings_of_one_server_share_what_it_holds() {
        let mut r = rig().await;
        r.prepare().await.unwrap();
        let sent = r.mock.count();
        r.council.server = format!("{}/", r.mock.base);
        let p = r.prepare().await.unwrap();
        assert_eq!(p.contexts.len(), 1);
        assert_eq!(r.mock.count(), sent, "the server already holds the identity, the spec, and the context");
    }

    #[tokio::test]
    async fn more_labels_than_the_server_reads_or_a_repeated_label_is_a_miss() {
        let r = rig().await;
        let labels: Vec<String> = (0..5).map(|i| format!("ctx-{i}")).collect();
        let miss = r.kernel.council_sync().prepare_labels(&r.kernel, &r.council, "shell-gate", &labels, None, &[], far(), &SlotWaits::default()).await.err().unwrap();
        assert!(miss.0.contains("at most 4"), "{}", miss.0);
        let twice = vec!["reviewer".to_string(), "reviewer".to_string()];
        let miss = r.kernel.council_sync().prepare_labels(&r.kernel, &r.council, "shell-gate", &twice, None, &[], far(), &SlotWaits::default()).await.err().unwrap();
        assert!(miss.0.contains("\"reviewer\" twice"), "{}", miss.0);
        assert!(r.mock.builds().is_empty());
    }

    /// gogi's fake council server, with room for whole seats: its tokens
    /// are bytes, and a seat with worked examples runs past the default
    /// limits on a decision's context and on pinned bytes.
    fn fake_council() -> gogi_fake::Server {
        let config = gogi_fake::Config {
            context_tokens: 1 << 20,
            decision_context_tokens: 1 << 20,
            pin_bytes: 1 << 34,
            ..gogi_fake::Config::default()
        };
        gogi_fake::Server::start(config).expect("the fake council starts")
    }

    /// Waits until `owner`'s pins on `client`'s server are exactly `want`.
    async fn pinned_exactly(client: &MkClient, owner: &str, want: &[&SnapshotId]) -> gogi_proto::pins::PinSet {
        let want: BTreeSet<&str> = want.iter().map(|id| id.as_str()).collect();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            match client.council().get_pins(owner).await {
                Ok(pins) if pins.snapshots.iter().map(String::as_str).collect::<BTreeSet<_>>() == want => return pins,
                _ if tokio::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(50)).await,
                other => panic!("pins for {owner} never became {want:?}: {other:?}"),
            }
        }
    }

    /// An edited `AGENTS.md` replaces its seat in the kernel's pin set on
    /// the council server at `server`. Drops the pins at the end.
    async fn house_rules_edit_replaces_the_pinned_seat(server: String) {
        let mut r = rig_on(serve_pinning().await).await;
        r.council.server = server.clone();
        r.council.model = std::env::var("COUNCIL_MODEL").ok();
        r.council.deadline_ms = 120_000;
        r.council.house_rules = true;
        let owner = pin_owner(r.kernel.id());
        let client = MkClient::new(&server, Duration::from_secs(30)).unwrap();
        write_rules(&r, "Back up data before changing it.").await;
        let seat = seat_in(&r, "lane-a", Some("/work/repo"));
        let first = prepare_seat(&r, seat).await.expect("prepared");
        let reviewer = first.contexts[0].head.clone();
        let old = first.house_rules().unwrap().head.clone();
        println!("pins: {:?}", pinned_exactly(&client, &owner, &[&reviewer, &old]).await);

        write_rules(&r, "Back up data. Never push.").await;
        let second = prepare_seat(&r, seat).await.expect("prepared");
        let new = second.house_rules().unwrap().head.clone();
        assert_ne!(new, old);
        let pins = pinned_exactly(&client, &owner, &[&reviewer, &new]).await;
        println!("after the edit: {pins:?}");
        assert!(!pins.snapshots.iter().any(|id| id == old.as_str()), "{pins:?}");
        client.council().delete_pins(&owner).await.expect("pins dropped");
    }

    /// A seat built on the council server at `server`, pinned under the
    /// kernel's owner, and read whole by a decision whose numbers
    /// recompute. Drops the pins at the end.
    async fn seat_is_pinned_and_read_whole(server: String) {
        let mut r = rig_on(serve_pinning().await).await;
        r.council.server = server.clone();
        r.council.model = std::env::var("COUNCIL_MODEL").ok();
        r.council.deadline_ms = 120_000;
        converse(&r);
        let owner = pin_owner(r.kernel.id());
        let started = std::time::Instant::now();
        let p = r.prepare().await.expect("prepared");
        let head = p.contexts[0].head.clone();
        println!("seat built in {} ms, head {head}", started.elapsed().as_millis());

        let client = MkClient::new(&server, Duration::from_secs(30)).unwrap();
        let pins = pinned_exactly(&client, &owner, &[&head]).await;
        println!("pins: {pins:?}");
        assert!(pins.expires_in_s > 3500 && pins.expires_in_s <= 3600, "{pins:?}");
        let snapshot = client.council().snapshot(&head).await.expect("the head is held");
        println!("snapshot: {snapshot:?}");
        assert!(snapshot.pins.iter().any(|pin| pin.owner == owner), "{snapshot:?}");

        let request = super::super::gate::decision_request(&p, &r.council, serde_json::Value::from("git push origin main"));
        let asked = std::time::Instant::now();
        let response = super::super::gate::ask_within(&r.kernel, &p, &request, &r.council, far(), &SlotWaits::default())
            .await
            .expect("answered");
        gogi_proto::math::verify(&response, &request.expect()).expect("the numbers recompute");
        assert_eq!(response.reads[0].context, Some(0));
        assert_eq!(response.identity.spec_id.as_ref(), Some(&p.spec_id));
        println!(
            "decision in {} ms: answers {}",
            asked.elapsed().as_millis(),
            serde_json::to_string(&response.answers).unwrap()
        );
        println!("read snapshot {:?}", response.reads[0].snapshot);
        client.council().delete_pins(&owner).await.expect("pins dropped");
    }

    /// The kernel and gogi's fake council server agree on the contract: a
    /// seat the kernel builds is held and pinned, and a decision over it
    /// returns numbers gogi's math recomputes.
    ///
    /// Falsified by a seat, pin set, or decision request the fake refuses,
    /// or a read the kernel cannot map back to its seat.
    #[tokio::test]
    async fn against_the_fake_a_seat_is_pinned_and_read_whole() {
        let fake = fake_council();
        seat_is_pinned_and_read_whole(fake.url()).await;
    }

    /// Against gogi's fake council server, an edited `AGENTS.md` replaces
    /// its seat in the pin set.
    #[tokio::test]
    async fn against_the_fake_a_house_rules_edit_replaces_the_pinned_seat() {
        let fake = fake_council();
        house_rules_edit_replaces_the_pinned_seat(fake.url()).await;
    }

    /// On a server that serves several models, the council names one. With
    /// no `[council] model` it is a miss naming the served models. A model
    /// that is loading is a miss naming its status, and the next prepare
    /// asks the identity again. Named by its alias, the model takes every
    /// build and the decision, and each answer names it by its `model`.
    ///
    /// Falsified by a build or decision that leaves `model` out, or an
    /// identity cached while its model was loading.
    #[tokio::test]
    async fn against_the_fake_the_council_names_its_served_model() {
        let b = gogi_fake::FakeModel { aliases: vec!["bee".into()], ..gogi_fake::FakeModel::ready("b", 1) };
        let config = gogi_fake::Config {
            models: vec![gogi_fake::FakeModel::ready("a", 0), b],
            context_tokens: 1 << 20,
            decision_context_tokens: 1 << 20,
            pin_bytes: 1 << 34,
            ..gogi_fake::Config::default()
        };
        let fake = gogi_fake::Server::start(config).expect("the fake council starts");
        let mut r = rig_on(serve_pinning().await).await;
        r.council.server = fake.url();
        r.council.deadline_ms = 120_000;
        converse(&r);
        let miss = r.prepare().await.err().expect("no model is named");
        assert!(miss.0.contains("model is required") && miss.0.contains("it serves a, b"), "{}", miss.0);

        r.council.model = Some("bee".into());
        fake.set_status("b", gogi_fake::ModelStatus::Loading).unwrap();
        let miss = r.prepare().await.err().expect("b is loading");
        assert!(miss.0.contains("model b is loading, not ready"), "{}", miss.0);
        fake.set_status("b", gogi_fake::ModelStatus::Ready).unwrap();
        let p = r.prepare().await.expect("prepared once b is ready");
        assert_eq!(p.identity.model, "b");
        let client = MkClient::new(&fake.url(), Duration::from_secs(30)).unwrap();
        assert_eq!(client.council().snapshot(&p.contexts[0].head).await.unwrap().model, "b");

        let request = super::super::gate::decision_request(&p, &r.council, serde_json::Value::from("git push origin main"));
        assert_eq!(request.model, "b");
        let response = super::super::gate::ask_within(&r.kernel, &p, &request, &r.council, far(), &SlotWaits::default())
            .await
            .expect("answered");
        gogi_proto::math::verify(&response, &request.expect()).expect("the numbers recompute");
        assert_eq!(response.identity.model, "b");
    }

    /// Live, by hand: [`house_rules_edit_replaces_the_pinned_seat`] against
    /// a real server.
    /// `COUNCIL_URL=http://100.65.47.5:8091 cargo test -p kaijutsu-kernel --lib live_council_v2 -- --ignored --nocapture`
    /// Set `COUNCIL_MODEL` on a server that serves several models.
    #[tokio::test]
    #[ignore = "needs a council v2 server at COUNCIL_URL"]
    async fn live_council_v2_house_rules_edit_replaces_the_pinned_seat() {
        house_rules_edit_replaces_the_pinned_seat(std::env::var("COUNCIL_URL").expect("COUNCIL_URL names the server")).await;
    }

    /// Live, by hand: [`seat_is_pinned_and_read_whole`] against a real
    /// server.
    /// `COUNCIL_URL=http://100.65.47.5:8091 cargo test -p kaijutsu-kernel --lib live_council_v2 -- --ignored --nocapture`
    /// Set `COUNCIL_MODEL` on a server that serves several models.
    #[tokio::test]
    #[ignore = "needs a council v2 server at COUNCIL_URL"]
    async fn live_council_v2_seat_is_pinned_and_read_whole() {
        seat_is_pinned_and_read_whole(std::env::var("COUNCIL_URL").expect("COUNCIL_URL names the server")).await;
    }
}
