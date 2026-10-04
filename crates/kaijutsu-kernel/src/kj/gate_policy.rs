//! The gate policy evaluator: one function composes every layer of gate
//! rules into a per-statement verdict, and both gate pinch points consult
//! it — broker PreCall ([`evaluate_planned`]) and `run_gate` ([`evaluate`]).
//! Nothing else grows a checker. `docs/gate-policy-tuning.md` is canonical.
//!
//! Layers, top wins:
//!
//! 1. **User rules** — `approval_rules`, digest-keyed, learned from a human
//!    answer. A human decision outranks everything shipped or configured.
//!    Beneath them in the same layer, **family rules** —
//!    `approval_rule_families`, keyed on a command family (`kj handoff
//!    note`, `git push`), learned with `kj ledger allow --remember
//!    <scope> --family`. The exact statement beats the family.
//! 2. **context_type config** — `[context_type.<type>]` in
//!    `/config/kernel/gate.toml`, for the calling context's type.
//! 3. **Global config** — `[global]` in the same file.
//! 4. **Builtin** — the verb's declared effect: a `kj` call that classifies
//!    as [`Effect::Read`](super::effect::Effect::Read), any `kj ledger`
//!    call, or a `kj … --help` invocation, under the six structural
//!    conditions `kj/readonly.rs` states (a redirect, a background flag, a
//!    heredoc or a substituted argument refuses).
//!
//! **Keys** (layers 1's families, 2 and 3) are `kj <verb> [<subcommand>]`
//! in canonical names, or `<command> [<first argument>]` for anything else,
//! where the first argument counts only when it is not a flag. A more
//! specific key wins inside a layer; at equal specificity deny beats ask
//! beats allow. A `kj` key that names no live verb fails the load.
//!
//! **Verdicts.** `allow` skips every checker. `deny` refuses at broker
//! PreCall and inside `run_gate`. `ask` is firm: no lower layer can allow
//! the statement, and each stack's own ask machinery does the asking.
//!
//! **The uncovered tier.** `uncovered = "allow"` in a `gate.toml` section
//! allows every statement no key covers, redirects and all — the sandbox
//! posture, off unless the file says so. It is decided last, so an `ask`
//! key stays firm, a `deny` key still denies, and a user rule still
//! outranks it; what it catches is everything no list names. Its decisions
//! read as `… uncovered tier allows …`, never as an allow-list hit, so a
//! ledger row says which is in force. The `Ask` tier is the default and
//! today's behavior.
//!
//! **Structural refusals veto allows.** An allow from a family rule, a
//! config layer or the builtin layer covers a *key*, never arguments, so a redirect, a
//! background flag, a heredoc or a non-plain argument drops the statement
//! to `Uncovered` and it meets the pre_call hooks and the gate as usual. A deny
//! or an ask fires regardless of structure. A command whose arguments the
//! evaluator cannot read as plain text has no key at all and is
//! `Uncovered`, which fails toward the default: ask — or toward the
//! uncovered tier where a section sets one.
//!
//! Composition per program is the ledger's own [`AskCoverage`] rule: a Deny
//! anywhere denies the whole submission, every statement must be Allow to
//! auto-allow, anything else escalates. A submission is never partially
//! applied.
//!
//! **Origin boundary.** Layers 2–4 classify a *planned command tree*, which
//! a [`GateSpec`] carries for `Origin::ShellGate` and for a shell-shaped
//! `Origin::Hook` ask, but not for `Origin::KjVerb`. A `KjVerb` ask meets
//! the user-rule layer only and is otherwise `Uncovered`. The uncovered
//! tier rides the config layers, so it reaches exactly what they reach: a
//! `KjVerb` ask still asks.
//!
//! **The file fails loudly.** An unreadable or unparseable `gate.toml`
//! refuses every shell submission that consults it, naming the file and the
//! remedy (`kj config reset gate.toml`). An absent file is an empty layer:
//! deleting it is a deliberate act the seed respects.
//!
//! [`AskCoverage`]: approval_ledger::types::AskCoverage

use std::collections::BTreeMap;

use approval_ledger::types::{AskVerdict, FamilyRuleRow, Origin, RuleRow, StatementVerdict};
use kaish_kernel::PlannedStatement;
use kaish_types::plan::PlannedCommand;
use rusqlite::Connection;
use serde::Deserialize;

use super::gate::{statement_digest, GateSpec, GatedStatement};
use super::readonly;
use crate::kernel_db::KernelDb;
use crate::vfs::MountTable;

/// The config file's name under `/config/kernel`.
pub(crate) const GATE_CONFIG_FILE: &str = "gate.toml";

/// Which layer decided a statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Layer {
    /// A digest-keyed rule a human taught the ledger.
    UserRule,
    /// A family-keyed rule a human taught the ledger.
    UserFamily,
    /// `[context_type.<type>]` in `gate.toml`.
    ContextTypeConfig(String),
    /// `[global]` in `gate.toml`.
    GlobalConfig,
    /// The verb's declared effect, the `kj ledger` structural exemption, or
    /// a `kj … --help` invocation.
    Builtin,
    /// `uncovered = "allow"` in `gate.toml` — the sandbox posture. `Some`
    /// names the `[context_type.<type>]` section that set it; `None` is
    /// `[global]`.
    UncoveredAllow(Option<String>),
}

impl std::fmt::Display for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserRule => f.write_str("user rule"),
            Self::UserFamily => f.write_str("user family rule"),
            Self::ContextTypeConfig(t) => write!(f, "context_type config ({t})"),
            Self::GlobalConfig => f.write_str("global config"),
            Self::Builtin => f.write_str("builtin"),
            Self::UncoveredAllow(None) => f.write_str("global config uncovered tier"),
            Self::UncoveredAllow(Some(t)) => write!(f, "context_type config ({t}) uncovered tier"),
        }
    }
}

/// One layer's decision on one key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Decision {
    pub layer: Layer,
    pub key: String,
}

/// One statement's verdict, naming the layer and key that decided it. An
/// `Allow` carries one decision per command in the statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PolicyVerdict {
    Allow(Vec<Decision>),
    Ask(Decision),
    Deny(Decision),
    /// No layer has a verdict; the statement meets the gate as usual.
    Uncovered,
}

impl PolicyVerdict {
    /// The tier word a hook body reads off `KJ_TOOL_PLAN`: `allow`, `ask`,
    /// `deny`, or `score` for a command no layer decided.
    pub(crate) fn tier(&self) -> &'static str {
        match self {
            Self::Allow(_) => "allow",
            Self::Ask(_) => "ask",
            Self::Deny(_) => "deny",
            Self::Uncovered => "score",
        }
    }
}

/// The composed verdicts for one submission, one per gated statement in the
/// order the caller gave them.
#[derive(Clone, Debug, Default)]
pub(crate) struct PolicyEvaluation {
    pub per_statement: Vec<PolicyVerdict>,
}

impl PolicyEvaluation {
    /// The whole-submission verdict, composed the way [`AskCoverage`] does:
    /// deny wins, allow needs every statement, an empty set escalates. An
    /// `Ask` escalates like `Uncovered`; the stacks tell them apart through
    /// the per-command tier.
    ///
    /// [`AskCoverage`]: approval_ledger::types::AskCoverage
    pub(crate) fn verdict(&self) -> AskVerdict {
        if self.per_statement.iter().any(|v| matches!(v, PolicyVerdict::Deny(_))) {
            return AskVerdict::Deny;
        }
        if !self.per_statement.is_empty()
            && self.per_statement.iter().all(|v| matches!(v, PolicyVerdict::Allow(_)))
        {
            return AskVerdict::Allow;
        }
        AskVerdict::Escalate
    }

    /// The `auto_reason` text for an auto-decision on a gate ask, naming the
    /// winning layer and key per statement. On a deny only the denied
    /// statements are named, by the SOURCE index the caller published
    /// (`GatedStatement::source_index`), never a position re-derived from
    /// `statements` — that Vec is post-filter, so a recount blames the
    /// wrong line.
    pub(crate) fn describe(&self, statements: &[GatedStatement], allow: bool) -> String {
        self.describe_with(allow, |i| match statements.get(i) {
            Some(s) => match s.source_index {
                Some(idx) => format!("statement #{idx} (`{}`)", truncate_for_reason(&s.rendered)),
                None => format!("`{}`", truncate_for_reason(&s.rendered)),
            },
            None => format!("statement #{i}"),
        })
    }

    /// [`Self::describe`] for a planned program with no gate ask behind it
    /// (broker PreCall), naming statements by kaish's published index.
    pub(crate) fn describe_planned(&self, statements: &[PlannedStatement], allow: bool) -> String {
        self.describe_with(allow, |i| match statements.get(i) {
            Some(s) => format!(
                "statement #{} (`{}`)",
                s.index,
                truncate_for_reason(&s.plan.rendered)
            ),
            None => format!("statement #{i}"),
        })
    }

    /// The ask description for a planned program's asking statements,
    /// naming each layer, key, and statement: the ask-tier ones, and with
    /// `uncovered_asks` the ones no layer covers. `None` when none asks.
    pub(crate) fn describe_asks_planned(&self, statements: &[PlannedStatement], uncovered_asks: bool) -> Option<String> {
        let name = |i: usize| match statements.get(i) {
            Some(s) => format!("statement #{} (`{}`)", s.index, truncate_for_reason(&s.plan.rendered)),
            None => format!("statement #{i}"),
        };
        let parts: Vec<String> = self
            .per_statement
            .iter()
            .enumerate()
            .filter_map(|(i, v)| match v {
                PolicyVerdict::Ask(d) => Some(format!("{} asks {} — {}", d.layer, d.key, name(i))),
                PolicyVerdict::Uncovered if uncovered_asks => {
                    Some(format!("no layer covers {} and the actor is not a root character", name(i)))
                }
                _ => None,
            })
            .collect();
        (!parts.is_empty()).then(|| format!("gate policy: {}", parts.join("; ")))
    }

    fn describe_with(&self, allow: bool, name: impl Fn(usize) -> String) -> String {
        let parts: Vec<String> = self
            .per_statement
            .iter()
            .enumerate()
            .filter_map(|(i, v)| match v {
                PolicyVerdict::Allow(decisions) if allow => Some(
                    decisions
                        .iter()
                        .map(|d| format!("{} allows {}", d.layer, d.key))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                PolicyVerdict::Deny(d) if !allow => {
                    Some(format!("{} denies {} — {}", d.layer, d.key, name(i)))
                }
                _ => None,
            })
            .collect();
        format!("gate policy: {}", parts.join("; "))
    }
}

/// One line of a statement, short enough for a ledger row and a refusal.
fn truncate_for_reason(s: &str) -> String {
    const LIMIT: usize = 80;
    if s.chars().count() <= LIMIT {
        return s.replace('\n', "⏎");
    }
    let head: String = s.chars().take(LIMIT).collect();
    format!("{}…", head.replace('\n', "⏎"))
}

// ── The config file ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TierVerdict {
    Allow,
    Ask,
    Deny,
}

impl TierVerdict {
    /// Deny beats ask beats allow at equal specificity.
    fn rank(self) -> u8 {
        match self {
            Self::Allow => 1,
            Self::Ask => 2,
            Self::Deny => 3,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

/// What a section does with a statement no key covers. `Ask` is the
/// default: the statement meets the pre_call hooks and the gate. `Allow` is the sandbox posture — see [`GateConfig::uncovered_for`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum UncoveredTier {
    #[default]
    Ask,
    Allow,
}

impl UncoveredTier {
    /// The two words `uncovered` takes. `deny` is not one of them: a
    /// standing refusal is a key in the `deny` list, not a posture.
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "ask" => Ok(Self::Ask),
            "allow" => Ok(Self::Allow),
            other => Err(format!(
                "`{other}` is not an uncovered tier — `ask` (the default: a statement no key \
                 covers meets the pre_call hooks and the gate) or `allow` (a sandboxed or throwaway \
                 kernel, where no ask reaches a human)"
            )),
        }
    }
}

/// One tier table: `[global]` or one `[context_type.<type>]` section, keys
/// normalized to canonical names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TierTable {
    /// `(normalized key, verdict)`; a key may appear under more than one
    /// verdict, and the higher rank wins.
    entries: Vec<(String, TierVerdict)>,
    /// This section's `uncovered` setting, when it states one.
    uncovered: Option<UncoveredTier>,
}

impl TierTable {
    /// The winning entry for a command's candidate keys, given most
    /// specific first.
    fn lookup(&self, candidates: &[String]) -> Option<(String, TierVerdict)> {
        for key in candidates {
            let best = self
                .entries
                .iter()
                .filter(|(k, _)| k == key)
                .map(|(_, v)| *v)
                .max_by_key(|v| v.rank());
            if let Some(v) = best {
                return Some((key.clone(), v));
            }
        }
        None
    }
}

/// The parsed `gate.toml`: the global tier, one tier per context type, and
/// the council's settings when the file declares them.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GateConfig {
    global: TierTable,
    context_types: BTreeMap<String, TierTable>,
    council: Option<CouncilConfig>,
    /// Context types whose `[context_type.<type>.council]` says `enabled = true`.
    council_enabled: std::collections::BTreeSet<String>,
}

/// How the kernel pools the council's per-context reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CouncilPoolMethod {
    Linear,
    LogLinear,
}

/// What weights the pool gives each read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CouncilPoolWeights {
    Uniform,
    Mass,
}

/// The kind of case a council spec reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CouncilCase {
    Shell,
}

/// A spec the council holds: `name` is the stem of
/// `/config/kernel/council/<name>.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CouncilSpec {
    pub(crate) name: String,
    pub(crate) case: CouncilCase,
}

/// What produced a number: a threshold fitted under one identity does not
/// carry to another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CouncilIdentity {
    pub(crate) weight_hash: String,
    pub(crate) engine: String,
    pub(crate) tokenizer_hash: String,
    pub(crate) template: String,
}

/// The cut a spec's pooled answer must clear under one identity.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CouncilThreshold {
    pub(crate) spec: String,
    pub(crate) identity: CouncilIdentity,
    /// Pooled p(allow) at or above which the council may allow.
    pub(crate) allow_at: f64,
    /// Each read's verdict mass, a log probability, at or above which the
    /// read counts.
    pub(crate) mass_floor: f64,
}

/// The `[council]` section with its specs and thresholds.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CouncilConfig {
    pub(crate) server: String,
    pub(crate) contexts: Vec<String>,
    pub(crate) pool_method: CouncilPoolMethod,
    pub(crate) pool_weights: CouncilPoolWeights,
    pub(crate) deadline_ms: u64,
    pub(crate) require_agree: bool,
    pub(crate) specs: Vec<CouncilSpec>,
    pub(crate) thresholds: Vec<CouncilThreshold>,
}

impl CouncilConfig {
    /// The threshold for `spec` under `identity`, or `None` when none was
    /// fitted for that identity.
    pub(crate) fn threshold_for(
        &self,
        spec: &str,
        identity: &CouncilIdentity,
    ) -> Option<&CouncilThreshold> {
        self.thresholds.iter().find(|t| t.spec == spec && t.identity == *identity)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CouncilToml {
    server: String,
    contexts: Vec<String>,
    pool: CouncilPoolToml,
    deadline_ms: i64,
    #[serde(default = "default_require_agree")]
    require_agree: bool,
    #[serde(default)]
    spec: Vec<CouncilSpecToml>,
    #[serde(default)]
    threshold: Vec<CouncilThresholdToml>,
}

fn default_require_agree() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CouncilPoolToml {
    method: String,
    weights: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CouncilSpecToml {
    name: String,
    case: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CouncilThresholdToml {
    spec: String,
    weight_hash: String,
    engine: String,
    tokenizer_hash: String,
    template: String,
    allow_at: f64,
    mass_floor: f64,
}

/// `[context_type.<type>.council]`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CouncilSwitchToml {
    enabled: bool,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct GateToml {
    #[serde(default)]
    global: TierToml,
    #[serde(default)]
    context_type: BTreeMap<String, TierToml>,
    council: Option<CouncilToml>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TierToml {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    ask: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    /// `ask` (the default) or `allow`.
    uncovered: Option<String>,
    /// Accepted under `[context_type.<type>]` only; `[global]` is refused.
    council: Option<CouncilSwitchToml>,
}

/// Why `gate.toml` could not be used. Every variant refuses the
/// submission that met it; the message names the file and the remedy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GateConfigError {
    /// The VFS could not read the file (not an absent file — that is an
    /// empty config).
    Read(String),
    /// The body is not the shape the example in `docs/gate-policy-tuning.md`
    /// shows: bad TOML, an unknown section or verdict word, a key that
    /// names no kj verb.
    Parse(String),
}

impl std::fmt::Display for GateConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (what, detail) = match self {
            Self::Read(e) => ("could not be read", e),
            Self::Parse(e) => ("is not a valid gate policy file", e),
        };
        write!(
            f,
            "/config/kernel/{GATE_CONFIG_FILE} {what}: {detail} — every gated shell submission \
             is refused until it is fixed; `kj config reset {GATE_CONFIG_FILE}` restores the \
             shipped default"
        )
    }
}

pub(crate) type GateConfigLoad = Result<GateConfig, GateConfigError>;

/// The load for a gate the config layers do not apply to — an
/// `Origin::KjVerb` ask, which carries no plan to key on — and for tests
/// that exercise the other layers alone.
pub(crate) fn no_config() -> GateConfigLoad {
    Ok(GateConfig::default())
}

impl GateConfig {
    /// Parse a `gate.toml` body. Unknown sections and verdict words, and a
    /// `kj` key that names no live verb, fail with the section and key
    /// named.
    pub(crate) fn parse(text: &str) -> Result<Self, GateConfigError> {
        let raw: GateToml =
            toml::from_str(text).map_err(|e| GateConfigError::Parse(e.to_string()))?;
        let mut config = GateConfig {
            global: Self::table_from(&raw.global, "[global]")?,
            context_types: BTreeMap::new(),
            council: None,
            council_enabled: Default::default(),
        };
        if raw.global.council.is_some() {
            return Err(GateConfigError::Parse(
                "[global.council]: the council has no global switch; enable it per context type \
                 with [context_type.<type>.council]"
                    .into(),
            ));
        }
        for (context_type, tier) in &raw.context_type {
            let section = format!("[context_type.{context_type}]");
            config
                .context_types
                .insert(context_type.clone(), Self::table_from(tier, &section)?);
            if tier.council.as_ref().is_some_and(|c| c.enabled) {
                config.council_enabled.insert(context_type.clone());
            }
        }
        if let Some(council) = &raw.council {
            config.council = Some(Self::council_from(council)?);
        } else if let Some(name) = config.council_enabled.iter().next() {
            return Err(GateConfigError::Parse(format!(
                "[context_type.{name}.council] enabled = true: the file has no [council] section \
                 to enable"
            )));
        }
        Ok(config)
    }

    fn council_from(raw: &CouncilToml) -> Result<CouncilConfig, GateConfigError> {
        let err = |m: String| GateConfigError::Parse(m);
        let rest = raw
            .server
            .strip_prefix("http://")
            .or_else(|| raw.server.strip_prefix("https://"));
        if !rest.is_some_and(|r| !r.is_empty() && !r.contains(char::is_whitespace)) {
            return Err(err(format!(
                "[council] server: `{}` must be an http:// or https:// URL with a host",
                raw.server
            )));
        }
        if raw.contexts.is_empty() {
            return Err(err("[council] contexts: must list at least one context label".into()));
        }
        for (i, label) in raw.contexts.iter().enumerate() {
            if label.trim().is_empty() {
                return Err(err("[council] contexts: labels must be non-empty".into()));
            }
            if raw.contexts[..i].contains(label) {
                return Err(err(format!("[council] contexts: `{label}` is listed twice")));
            }
        }
        let pool_method = match raw.pool.method.as_str() {
            "linear" => CouncilPoolMethod::Linear,
            "loglinear" => CouncilPoolMethod::LogLinear,
            other => {
                return Err(err(format!(
                    "[council] pool.method: `{other}` is not one of linear, loglinear"
                )))
            }
        };
        let pool_weights = match raw.pool.weights.as_str() {
            "uniform" => CouncilPoolWeights::Uniform,
            "mass" => CouncilPoolWeights::Mass,
            other => {
                return Err(err(format!(
                    "[council] pool.weights: `{other}` is not one of uniform, mass"
                )))
            }
        };
        if raw.deadline_ms <= 0 {
            return Err(err(format!(
                "[council] deadline_ms: {} must be an integer greater than 0",
                raw.deadline_ms
            )));
        }
        if raw.spec.is_empty() {
            return Err(err("[council]: declare at least one [[council.spec]]".into()));
        }
        let mut specs: Vec<CouncilSpec> = Vec::new();
        for s in &raw.spec {
            if s.name.is_empty()
                || !s.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(err(format!(
                    "[[council.spec]] name: `{}` must match [A-Za-z0-9_-]+ (a file stem under \
                     /config/kernel/council/)",
                    s.name
                )));
            }
            if specs.iter().any(|p| p.name == s.name) {
                return Err(err(format!("[[council.spec]] name: `{}` is declared twice", s.name)));
            }
            let case = match s.case.as_str() {
                "shell" => CouncilCase::Shell,
                other => {
                    return Err(err(format!(
                        "[[council.spec]] {} case: `{other}` is not one of shell",
                        s.name
                    )))
                }
            };
            specs.push(CouncilSpec { name: s.name.clone(), case });
        }
        let mut thresholds: Vec<CouncilThreshold> = Vec::new();
        for t in &raw.threshold {
            let at = format!("[[council.threshold]] spec {}", t.spec);
            if !specs.iter().any(|s| s.name == t.spec) {
                return Err(err(format!(
                    "[[council.threshold]] spec: `{}` names no [[council.spec]]",
                    t.spec
                )));
            }
            for (field, value) in [
                ("weight_hash", &t.weight_hash),
                ("engine", &t.engine),
                ("tokenizer_hash", &t.tokenizer_hash),
                ("template", &t.template),
            ] {
                if value.trim().is_empty() {
                    return Err(err(format!("{at} {field}: must be a non-empty string")));
                }
            }
            if !(t.allow_at.is_finite() && t.allow_at > 0.0 && t.allow_at <= 1.0) {
                return Err(err(format!(
                    "{at} allow_at: {} must satisfy 0 < allow_at <= 1",
                    t.allow_at
                )));
            }
            if !(t.mass_floor.is_finite() && t.mass_floor <= 0.0) {
                return Err(err(format!(
                    "{at} mass_floor: {} must be a log probability, at most 0",
                    t.mass_floor
                )));
            }
            let identity = CouncilIdentity {
                weight_hash: t.weight_hash.clone(),
                engine: t.engine.clone(),
                tokenizer_hash: t.tokenizer_hash.clone(),
                template: t.template.clone(),
            };
            if thresholds.iter().any(|p| p.spec == t.spec && p.identity == identity) {
                return Err(err(format!(
                    "{at}: a second threshold repeats the same weight_hash, engine, \
                     tokenizer_hash and template; one threshold per identity"
                )));
            }
            thresholds.push(CouncilThreshold {
                spec: t.spec.clone(),
                identity,
                allow_at: t.allow_at,
                mass_floor: t.mass_floor,
            });
        }
        Ok(CouncilConfig {
            server: raw.server.clone(),
            contexts: raw.contexts.clone(),
            pool_method,
            pool_weights,
            deadline_ms: raw.deadline_ms as u64,
            require_agree: raw.require_agree,
            specs,
            thresholds,
        })
    }

    /// The council's settings, when the file declares `[council]`.
    pub(crate) fn council(&self) -> Option<&CouncilConfig> {
        self.council.as_ref()
    }

    /// Whether the council reads for a caller of `context_type`: the file
    /// declares `[council]` and the type's own section enables it. Off for
    /// every other type, and for a caller with no type.
    pub(crate) fn council_enabled_for(&self, context_type: Option<&str>) -> bool {
        self.council.is_some()
            && context_type.is_some_and(|t| self.council_enabled.contains(t))
    }

    fn table_from(tier: &TierToml, section: &str) -> Result<TierTable, GateConfigError> {
        let mut entries = Vec::new();
        for (verdict, list) in [
            (TierVerdict::Allow, &tier.allow),
            (TierVerdict::Ask, &tier.ask),
            (TierVerdict::Deny, &tier.deny),
        ] {
            for raw in list {
                let key = normalize_config_key(raw).map_err(|why| {
                    GateConfigError::Parse(format!("{section} {}: `{raw}` — {why}", verdict.word()))
                })?;
                entries.push((key, verdict));
            }
        }
        let uncovered = match &tier.uncovered {
            Some(raw) => Some(UncoveredTier::parse(raw).map_err(|why| {
                GateConfigError::Parse(format!("{section} uncovered: {why}"))
            })?),
            None => None,
        };
        Ok(TierTable { entries, uncovered })
    }

    /// The uncovered tier in force for a caller of `context_type`, and the
    /// section that set it: the context type's own setting when it states
    /// one, else `[global]`, else `ask`.
    pub(crate) fn uncovered_for(
        &self,
        context_type: Option<&str>,
    ) -> (UncoveredTier, Option<String>) {
        if let Some(t) = context_type
            && let Some(tier) = self.context_types.get(t).and_then(|table| table.uncovered)
        {
            return (tier, Some(t.to_string()));
        }
        (self.global.uncovered.unwrap_or_default(), None)
    }

    /// Refuse a `[context_type.<name>]` section naming no live context
    /// type, whatever the section holds: an unknown section applies to
    /// nobody, so a typo'd `ask` tier silently leaves its type on whatever
    /// `[global]` said.
    ///
    /// `known` is the rc tree's list (`kj::rc::known_context_types`), and
    /// an empty list accepts every section — there is nothing to validate
    /// against, and a kernel with no rc tree is a legitimate shape. That is
    /// the rule `kj::rc::check_context_type` states for `--type`, kept as
    /// one rule with one meaning.
    pub(crate) fn check_context_types(&self, known: &[String]) -> Result<(), GateConfigError> {
        if known.is_empty() {
            return Ok(());
        }
        for name in self.context_types.keys() {
            if !known.iter().any(|t| t == name) {
                return Err(GateConfigError::Parse(format!(
                    "[context_type.{name}] names no context type: no rc bucket at {}/{name}; \
                     known types: {}",
                    kaijutsu_types::paths::RC_ROOT,
                    known.join(", "),
                )));
            }
        }
        Ok(())
    }

    /// Whether the sandbox posture is on for a caller of `context_type` —
    /// what `kj ledger rules` states plainly.
    pub(crate) fn allows_uncovered(&self, context_type: Option<&str>) -> bool {
        self.uncovered_for(context_type).0 == UncoveredTier::Allow
    }

    /// The config entries in force for a caller of `context_type`, for
    /// `kj ledger rules`: `(layer, key, verdict)`, the context_type section
    /// first.
    pub(crate) fn entries_for(&self, context_type: Option<&str>) -> Vec<(Layer, String, &'static str)> {
        let mut out = Vec::new();
        // The widest entry first: it decides every key the lists below do
        // not name.
        if let (UncoveredTier::Allow, section) = self.uncovered_for(context_type) {
            out.push((Layer::UncoveredAllow(section), "uncovered".to_string(), "allow"));
        }
        if let Some(t) = context_type
            && let Some(table) = self.context_types.get(t)
        {
            out.extend(
                table
                    .entries
                    .iter()
                    .map(|(k, v)| (Layer::ContextTypeConfig(t.to_string()), k.clone(), v.word())),
            );
        }
        out.extend(self.global.entries.iter().map(|(k, v)| (Layer::GlobalConfig, k.clone(), v.word())));
        out
    }

    /// Every key with a verdict, for inspection: `(section, key, verdict)`.
    #[cfg(test)]
    pub(crate) fn entries(&self) -> Vec<(String, String, &'static str)> {
        let mut out: Vec<(String, String, &'static str)> = self
            .global
            .entries
            .iter()
            .map(|(k, v)| ("global".to_string(), k.clone(), v.word()))
            .collect();
        for (t, table) in &self.context_types {
            out.extend(table.entries.iter().map(|(k, v)| (t.clone(), k.clone(), v.word())));
        }
        out
    }
}

/// Canonicalize one config key: `kj <verb> [<subcommand>]` resolves through
/// the verb tables (aliases become canonical names, an unknown verb or
/// subcommand is an error); anything else is `<command> [<first argument>]`
/// kept verbatim. Tokens are whitespace-separated.
fn normalize_config_key(raw: &str) -> Result<String, String> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let Some(first) = tokens.first() else {
        return Err("empty key".to_string());
    };
    if *first == "kj" {
        let root = super::kj_command();
        let Some(verb_name) = tokens.get(1) else {
            return Err("a kj key names a verb: `kj <verb> [<subcommand>]`".to_string());
        };
        let Some(verb) = root.find_subcommand(verb_name) else {
            return Err(format!("`{verb_name}` is not a kj verb"));
        };
        let mut key = format!("kj {}", verb.get_name());
        if let Some(sub_name) = tokens.get(2) {
            let Some(sub) = verb.find_subcommand(sub_name) else {
                return Err(format!(
                    "`{sub_name}` is not a subcommand of `kj {}`",
                    verb.get_name()
                ));
            };
            key.push(' ');
            key.push_str(sub.get_name());
        }
        if tokens.len() > 3 {
            return Err("a kj key is at most `kj <verb> <subcommand>`".to_string());
        }
        return Ok(key);
    }
    match tokens.len() {
        1 => Ok(first.to_string()),
        2 if tokens[1].starts_with('-') => Err(format!(
            "a flag is not a key token, so `{first} {}` would never match; use `{first}`",
            tokens[1]
        )),
        2 => Ok(format!("{first} {}", tokens[1])),
        _ => Err("a key is `<command>` or `<command> <first argument>`".to_string()),
    }
}

/// Read, parse and validate `/config/kernel/gate.toml`. An absent file (or
/// no `/config/kernel` mount) is the empty config; any other failure is an
/// error the caller must refuse on.
///
/// Validation needs the live rc tree, which is why it lives here and not in
/// [`GateConfig::parse`]: `parse` is a pure shape check over the text.
pub(crate) async fn load_config(vfs: &MountTable) -> GateConfigLoad {
    use crate::vfs::{VfsError, VfsOps};
    let path = kaijutsu_types::paths::config_path(GATE_CONFIG_FILE);
    let bytes = match vfs.read_all(std::path::Path::new(&path)).await {
        Ok(b) => b,
        Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => {
            return Ok(GateConfig::default());
        }
        Err(e) => return Err(GateConfigError::Read(e.to_string())),
    };
    let text = String::from_utf8(bytes)
        .map_err(|e| GateConfigError::Parse(format!("not valid UTF-8: {e}")))?;
    let config = GateConfig::parse(&text)?;
    let known = super::rc::known_context_types(vfs)
        .await
        .map_err(GateConfigError::Read)?;
    config.check_context_types(&known)?;
    Ok(config)
}

// ── Layers for one evaluation ───────────────────────────────────────────

/// The config layers in force for one caller: the parsed file and the
/// calling context's type, when it has one.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Layers<'a> {
    pub config: &'a GateConfig,
    pub context_type: Option<&'a str>,
}

impl Layers<'_> {
    /// The decision the uncovered tier contributes, or `None` when the tier
    /// is `ask` — the default, where an uncovered statement meets the
    /// pre_call hooks and the gate. `key` names what was allowed.
    fn uncovered_allow(&self, key: impl FnOnce() -> String) -> Option<Decision> {
        match self.config.uncovered_for(self.context_type) {
            (UncoveredTier::Allow, section) => Some(Decision {
                layer: Layer::UncoveredAllow(section),
                key: key(),
            }),
            (UncoveredTier::Ask, _) => None,
        }
    }

    /// [`Self::uncovered_allow`] as a whole verdict, for a caller holding an
    /// `Uncovered` it may convert.
    fn uncovered_verdict(&self, key: impl FnOnce() -> String) -> PolicyVerdict {
        match self.uncovered_allow(key) {
            Some(decision) => PolicyVerdict::Allow(vec![decision]),
            None => PolicyVerdict::Uncovered,
        }
    }
}

/// The `context_type` of a context, read off its row. `None` for a caller
/// with no context or a context the db does not know.
pub(crate) fn context_type_of(
    db: &KernelDb,
    context_id: Option<kaijutsu_types::ContextId>,
) -> Option<String> {
    let id = context_id?;
    db.get_context(id).ok().flatten().map(|row| row.context_type)
}

// ── Evaluation ──────────────────────────────────────────────────────────

/// Layers 2–4 over a planned program — what broker PreCall consults before
/// any hook runs. No ledger read.
pub(crate) fn evaluate_planned(
    statements: &[PlannedStatement],
    layers: Layers<'_>,
) -> PolicyEvaluation {
    PolicyEvaluation {
        per_statement: statements.iter().map(|s| statement_verdict(s, layers)).collect(),
    }
}

/// Every layer, for one gate ask: exact-statement rules over the ask's
/// statement digests, then family rules over the planned program's
/// command keys, then layers 2–4 over the same program. Fails only when
/// the ledger cannot be read.
pub(crate) fn evaluate(
    conn: &Connection,
    spec: &GateSpec,
    context_id: Option<&[u8]>,
    principal_id: Option<&[u8]>,
    layers: Layers<'_>,
) -> approval_ledger::Result<PolicyEvaluation> {
    let digests: Vec<String> = spec
        .statements
        .iter()
        .map(|s| statement_digest(spec.origin, &s.rendered))
        .collect();
    let digest_refs: Vec<&str> = digests.iter().map(String::as_str).collect();
    let coverage = approval_ledger::rules::redeem(
        conn,
        &digest_refs,
        &spec.authorized_label,
        context_id,
        principal_id,
    )?;

    let family = family_layer_per_gated_statement(conn, spec, context_id, principal_id)?;
    let lower = lower_layers_per_gated_statement(spec, layers);
    let per_statement = coverage
        .per_statement
        .iter()
        .zip(family)
        .zip(lower)
        .map(|((rule, family), lower)| match rule {
            StatementVerdict::Allow(row) => PolicyVerdict::Allow(vec![Decision {
                layer: Layer::UserRule,
                key: rule_key(row),
            }]),
            StatementVerdict::Deny(row) => PolicyVerdict::Deny(Decision {
                layer: Layer::UserRule,
                key: rule_key(row),
            }),
            StatementVerdict::Uncovered => match family {
                PolicyVerdict::Uncovered => lower,
                decided => decided,
            },
        })
        .collect();
    Ok(PolicyEvaluation { per_statement })
}

fn rule_key(row: &RuleRow) -> String {
    format!("the exact statement (rule {})", row.rule_id)
}

fn family_rule_key(row: &FamilyRuleRow) -> String {
    format!("{} (rule {})", row.family_key, row.rule_id)
}

/// The family-rule verdict for each of `spec.statements`, aligned by
/// index and shaped per origin like [`lower_layers_per_gated_statement`].
/// A family deny on any command denies the statement regardless of
/// structure; a family allow needs every command allowed and structurally
/// plain; anything else is `Uncovered` for the lower layers to decide.
fn family_layer_per_gated_statement(
    conn: &Connection,
    spec: &GateSpec,
    context_id: Option<&[u8]>,
    principal_id: Option<&[u8]>,
) -> approval_ledger::Result<Vec<PolicyVerdict>> {
    let n = spec.statements.len();
    // The planned statements each gated statement stands for.
    let groups: Vec<&[PlannedStatement]> = match spec.origin {
        Origin::ShellGate if spec.planned.len() == n && n > 0 => {
            spec.planned.iter().map(std::slice::from_ref).collect()
        }
        Origin::Hook if n == 1 && !spec.planned.is_empty() => vec![spec.planned.as_slice()],
        Origin::ShellGate | Origin::Hook | Origin::HookResult | Origin::KjVerb => return Ok(vec![PolicyVerdict::Uncovered; n]),
    };
    let mut out = Vec::with_capacity(n);
    for group in groups {
        let commands: Vec<Option<CommandKeys>> = group
            .iter()
            .flat_map(|s| s.plan.commands.iter())
            .map(command_keys)
            .collect();
        let mut wanted: Vec<&str> = Vec::new();
        for keys in commands.iter().flatten() {
            for k in &keys.candidates {
                if !wanted.contains(&k.as_str()) {
                    wanted.push(k);
                }
            }
        }
        if wanted.is_empty() {
            out.push(PolicyVerdict::Uncovered);
            continue;
        }
        let rows = approval_ledger::rules::family_coverage(conn, &wanted, context_id, principal_id)?;
        let rule_for = |key: &str| -> Option<&FamilyRuleRow> {
            wanted.iter().position(|w| *w == key).and_then(|i| rows[i].as_ref())
        };
        let mut decisions: Vec<Decision> = Vec::new();
        let mut all_allowed = !commands.is_empty();
        let mut deny: Option<Decision> = None;
        for keys in &commands {
            let Some(keys) = keys else {
                all_allowed = false;
                continue;
            };
            let hit = keys.candidates.iter().find_map(|k| rule_for(k));
            match hit {
                Some(row) if !row.allow => {
                    deny.get_or_insert(Decision { layer: Layer::UserFamily, key: family_rule_key(row) });
                }
                Some(row) if keys.structural_ok => {
                    let d = Decision { layer: Layer::UserFamily, key: family_rule_key(row) };
                    if !decisions.contains(&d) {
                        decisions.push(d);
                    }
                }
                _ => all_allowed = false,
            }
        }
        out.push(match deny {
            Some(d) => PolicyVerdict::Deny(d),
            None if all_allowed => PolicyVerdict::Allow(decisions),
            None => PolicyVerdict::Uncovered,
        });
    }
    Ok(out)
}

/// Why a statement cannot teach a family rule: the condition, named for
/// the human who asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FamilyRefusal(pub String);

impl std::fmt::Display for FamilyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The family keys a planned program teaches — one per command, most
/// specific form — or the refusal: a family allow covers a key and never
/// arguments, so a command with a redirect, a background flag, a heredoc
/// or an argument that is neither literal nor plain, or a `kj` argv that does not classify, would
/// authorize text the human never saw.
pub(crate) fn family_keys_for_program(statements: &[PlannedStatement]) -> Result<Vec<String>, FamilyRefusal> {
    let mut keys: Vec<String> = Vec::new();
    for cmd in statements.iter().flat_map(|s| s.plan.commands.iter()) {
        let refuse = |why: &str| FamilyRefusal(format!("`{}` {why}", command_clause(cmd)));
        if !cmd.redirects.is_empty() {
            return Err(refuse("has a redirect"));
        }
        if cmd.background {
            return Err(refuse("runs in the background"));
        }
        if !cmd.heredocs.is_empty() {
            return Err(refuse("carries a heredoc"));
        }
        let Some(k) = command_keys(cmd) else {
            return Err(refuse(
                "has an argument that is not plain text, or names no kj verb",
            ));
        };
        if !k.structural_ok {
            return Err(refuse("does not parse as a kj command"));
        }
        let key = k.candidates.into_iter().next().expect("candidates are never empty");
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    if keys.is_empty() {
        return Err(FamilyRefusal("the program has no command".to_string()));
    }
    Ok(keys)
}

/// Layers 2–4 for each of `spec.statements`, aligned by index.
///
/// `Origin::ShellGate` builds one gated statement per planned statement,
/// so the two align by position; a spec whose `planned` is empty or of
/// another length has no plan to classify and is `Uncovered` throughout.
/// `Origin::Hook` gates one statement standing for the whole call: denied
/// or asked if any planned statement is, allowed only when every one is.
/// `Origin::KjVerb` carries no plan.
fn lower_layers_per_gated_statement(spec: &GateSpec, layers: Layers<'_>) -> Vec<PolicyVerdict> {
    let n = spec.statements.len();
    match spec.origin {
        Origin::ShellGate if spec.planned.len() == n && n > 0 => {
            spec.planned.iter().map(|s| statement_verdict(s, layers)).collect()
        }
        Origin::Hook if n == 1 && !spec.planned.is_empty() => {
            let program = evaluate_planned(&spec.planned, layers);
            vec![fold_verdicts(program.per_statement)]
        }
        Origin::ShellGate | Origin::Hook | Origin::HookResult | Origin::KjVerb => {
            vec![PolicyVerdict::Uncovered; n]
        }
    }
}

/// One statement's verdict: the fold of its commands' verdicts. A
/// statement with no commands is not an allow.
fn statement_verdict(statement: &PlannedStatement, layers: Layers<'_>) -> PolicyVerdict {
    let verdicts: Vec<PolicyVerdict> = statement
        .plan
        .commands
        .iter()
        .map(|cmd| command_verdict(cmd, layers))
        .collect();
    match fold_verdicts(verdicts) {
        // A statement with no command has nothing to key on, so the tier
        // decides it directly; without this, one assignment in a program
        // would escalate the whole submission. The condition is "no
        // command", not "an assignment": an exit, a return, `break`,
        // `continue`, arithmetic, a `[[ ]]` test, a loop or case over
        // literals, and a function definition whose body runs nothing all
        // plan this way. It rests on kaish collecting every nested command,
        // including one inside an assignment value, a test operand or a
        // redirect target, so a commandless statement invokes nothing a
        // hook would have judged. A statement that has
        // commands is decided per command and keeps that verdict, so this
        // reason never names the wrong thing.
        PolicyVerdict::Uncovered if statement.plan.commands.is_empty() => {
            layers.uncovered_verdict(|| statement_key(statement))
        }
        decided => decided,
    }
}

/// What the uncovered tier allowed when a statement has no command to name.
fn statement_key(statement: &PlannedStatement) -> String {
    format!(
        "`{}`, a statement with no command",
        truncate_for_reason(&statement.plan.rendered)
    )
}

/// Deny anywhere → that deny; else ask anywhere → that ask; else every
/// verdict an allow (at least one) → one allow carrying every decision;
/// else uncovered.
fn fold_verdicts(verdicts: Vec<PolicyVerdict>) -> PolicyVerdict {
    if let Some(deny) = verdicts.iter().find(|v| matches!(v, PolicyVerdict::Deny(_))) {
        return deny.clone();
    }
    if let Some(ask) = verdicts.iter().find(|v| matches!(v, PolicyVerdict::Ask(_))) {
        return ask.clone();
    }
    if verdicts.is_empty() {
        return PolicyVerdict::Uncovered;
    }
    let mut decisions: Vec<Decision> = Vec::new();
    for v in verdicts {
        match v {
            PolicyVerdict::Allow(ds) => {
                for d in ds {
                    if !decisions.contains(&d) {
                        decisions.push(d);
                    }
                }
            }
            _ => return PolicyVerdict::Uncovered,
        }
    }
    PolicyVerdict::Allow(decisions)
}

/// One command's verdict through layers 2–4: the context_type table, then
/// the global table, then the builtin layer, and last the uncovered tier.
/// This is also what stamps the per-command `tier` on `KJ_TOOL_PLAN`.
pub(crate) fn command_verdict(cmd: &PlannedCommand, layers: Layers<'_>) -> PolicyVerdict {
    match keyed_command_verdict(cmd, layers) {
        // The tier is last on purpose: every key a layer names, and every
        // structural refusal of an allow, is decided above it.
        PolicyVerdict::Uncovered => layers.uncovered_verdict(|| command_key(cmd)),
        decided => decided,
    }
}

/// The command's most specific key, or its name when it has none — what
/// the uncovered tier reports it allowed.
fn command_key(cmd: &PlannedCommand) -> String {
    command_keys(cmd)
        .and_then(|keys| keys.candidates.into_iter().next())
        .unwrap_or_else(|| cmd.name.clone())
}

/// [`command_verdict`] without the uncovered tier: the keyed layers alone.
fn keyed_command_verdict(cmd: &PlannedCommand, layers: Layers<'_>) -> PolicyVerdict {
    if let Some(keys) = command_keys(cmd) {
        let context_table = layers
            .context_type
            .and_then(|t| layers.config.context_types.get(t).map(|table| (t, table)));
        let mut tables: Vec<(Layer, &TierTable)> = Vec::with_capacity(2);
        if let Some((t, table)) = context_table {
            tables.push((Layer::ContextTypeConfig(t.to_string()), table));
        }
        tables.push((Layer::GlobalConfig, &layers.config.global));
        for (layer, table) in tables {
            if let Some((key, verdict)) = table.lookup(&keys.candidates) {
                let decision = Decision { layer, key };
                match verdict {
                    TierVerdict::Deny => return PolicyVerdict::Deny(decision),
                    TierVerdict::Ask => return PolicyVerdict::Ask(decision),
                    TierVerdict::Allow if keys.structural_ok => return PolicyVerdict::Allow(vec![decision]),
                    // An allow that cannot cover this command decides
                    // nothing, so the builtin layer still can: `--help` on
                    // an allowed verb stays help.
                    TierVerdict::Allow => break,
                }
            }
        }
    }
    if let Some(key) = builtin_key(cmd) {
        return PolicyVerdict::Allow(vec![Decision {
            layer: Layer::Builtin,
            key,
        }]);
    }
    PolicyVerdict::Uncovered
}

/// A command's config keys, most specific first, and whether an allow may
/// cover it.
struct CommandKeys {
    candidates: Vec<String>,
    /// No redirect, background flag or heredoc; for `kj`, the argv also
    /// classifies.
    structural_ok: bool,
}

/// The config keys one command can match. `None` when an argument is not
/// plain text (nothing to key on) or a `kj` call names no verb.
fn command_keys(cmd: &PlannedCommand) -> Option<CommandKeys> {
    use kaish_types::plan::PlannedValue;
    let mut args = Vec::with_capacity(cmd.args.len());
    for arg in &cmd.args {
        match arg {
            PlannedValue::Literal { value, .. } => args.push(value.clone()),
            PlannedValue::Plain(s) => args.push(s.clone()),
            _ => return None,
        }
    }
    let plain_shape = cmd.redirects.is_empty() && !cmd.background && cmd.heredocs.is_empty();
    if cmd.name == "kj" {
        super::parse::strip_flag(&mut args, &["--confirm", "--json"]);
        let root = super::kj_command();
        let verb = root.find_subcommand(args.first()?)?;
        let verb_key = format!("kj {}", verb.get_name());
        let mut candidates = Vec::with_capacity(2);
        if let Some(sub) = args.get(1).and_then(|s| verb.find_subcommand(s)) {
            candidates.push(format!("{verb_key} {}", sub.get_name()));
        }
        candidates.push(verb_key);
        return Some(CommandKeys {
            candidates,
            structural_ok: plain_shape && super::effect::classify(&args).is_ok(),
        });
    }
    // The two-token key needs a first argument that is not a flag: `git
    // push` is a family, `rg -n` is not.
    let mut candidates = Vec::with_capacity(2);
    if let Some(first) = args.first().filter(|a| !a.starts_with('-')) {
        candidates.push(format!("{} {first}", cmd.name));
    }
    candidates.push(cmd.name.clone());
    Some(CommandKeys {
        candidates,
        structural_ok: plain_shape,
    })
}

/// The builtin key for one command — `kj <verb> [<subcommand>]` in canonical
/// names — when the builtin layer allows it, else `None`.
///
/// The exemption itself is `readonly::is_gate_exempt_kj`; this only names
/// what it allowed. `kj ledger` keys as the whole verb, the structural
/// exemption's own shape. A help invocation keys as `kj [<verb>] --help`.
fn builtin_key(cmd: &PlannedCommand) -> Option<String> {
    let mut args = readonly::resolved_kj_args(cmd)?;
    if is_kj_help(&args) {
        let root = super::kj_command();
        return Some(match args.first().and_then(|v| root.find_subcommand(v)) {
            Some(verb) if args.len() > 1 => format!("kj {} --help", verb.get_name()),
            _ => "kj --help".to_string(),
        });
    }
    if !readonly::is_gate_exempt_kj(cmd) {
        return None;
    }
    super::parse::strip_flag(&mut args, &["--confirm", "--json"]);
    let root = super::kj_command();
    let verb = root.find_subcommand(args.first()?)?;
    let mut key = format!("kj {}", verb.get_name());
    if verb.get_name() == "ledger" {
        return Some(key);
    }
    if let Some(sub) = args.get(1).and_then(|s| verb.find_subcommand(s)) {
        key.push(' ');
        key.push_str(sub.get_name());
    }
    Some(key)
}

/// One command as a human would read it in a refusal: the name and its
/// plain arguments, cut short.
fn command_clause(cmd: &PlannedCommand) -> String {
    use kaish_types::plan::PlannedValue;
    let mut words = vec![cmd.name.clone()];
    for arg in &cmd.args {
        match arg {
            PlannedValue::Plain(s) | PlannedValue::Literal { text: s, .. } => words.push(s.clone()),
            _ => words.push("<redacted>".to_string()),
        }
    }
    truncate_for_reason(&words.join(" "))
}

/// `kj … --help` / `kj … -h`: clap resolves help before dispatch, so
/// nothing runs. Three conditions, and the third is what makes it a rule
/// rather than a convenience: the help flag is the LAST argument, and no
/// argument before it starts with `-`. `kj rc add <path> --content --help`
/// binds `--help` as the content VALUE (the field allows hyphen values)
/// and performs a real write, so it is not help.
fn is_kj_help(args: &[String]) -> bool {
    let Some(last) = args.last() else {
        return false;
    };
    if last != "--help" && last != "-h" {
        return false;
    }
    !args[..args.len() - 1].iter().any(|a| a.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(source: &str) -> Vec<PlannedStatement> {
        kaish_kernel::ast::plan::plan_program(source)
            .unwrap_or_else(|e| panic!("plan_program({source:?}) failed to parse: {e:?}"))
    }

    /// No file and no context type: the builtin layer alone.
    fn unconfigured(source: &str) -> PolicyEvaluation {
        evaluate_planned(&plan(source), Layers { config: &GateConfig::default(), context_type: None })
    }

    fn config(text: &str) -> GateConfig {
        GateConfig::parse(text).unwrap_or_else(|e| panic!("test config must parse: {e}"))
    }

    fn first_verdict(source: &str, cfg: &GateConfig, context_type: Option<&str>) -> PolicyVerdict {
        let layers = Layers { config: cfg, context_type };
        evaluate_planned(&plan(source), layers).per_statement.remove(0)
    }

    /// A context env argument (`--env KEY=VALUE`) is plain text and must
    /// not drop a tier-allowed `kj context create` to Uncovered.
    #[test]
    fn env_argument_keeps_the_context_type_allow() {
        let cfg = config("[context_type.mcp]\nallow = [\"kj context create\"]\n");
        let v = first_verdict(
            "kj context create banto-probe --type director --env KJ_CHARACTER=banto --env ROTATED_FROM=ROOT",
            &cfg,
            Some("mcp"),
        );
        assert_eq!(
            allow_keys(&v),
            vec![(Layer::ContextTypeConfig("mcp".to_string()), "kj context create".to_string())],
            "got {v:?}"
        );
    }

    fn allow_keys(v: &PolicyVerdict) -> Vec<(Layer, String)> {
        match v {
            PolicyVerdict::Allow(ds) => ds.iter().map(|d| (d.layer.clone(), d.key.clone())).collect(),
            other => panic!("expected an allow, got {other:?}"),
        }
    }

    fn builtin_key_of(v: &PolicyVerdict) -> String {
        let keys = allow_keys(v);
        assert_eq!(keys.len(), 1, "{keys:?}");
        assert_eq!(keys[0].0, Layer::Builtin);
        keys[0].1.clone()
    }

    // ── builtin layer ───────────────────────────────────────────────

    #[test]
    fn a_read_verb_keys_as_verb_and_subcommand() {
        let e = unconfigured("kj block list");
        assert_eq!(builtin_key_of(&e.per_statement[0]), "kj block list");
        assert_eq!(e.verdict(), AskVerdict::Allow);
    }

    #[test]
    fn a_ledger_call_keys_as_the_whole_verb_behind_root_flags() {
        let e = unconfigured("kj --json ledger allow 01a0-abc");
        assert_eq!(builtin_key_of(&e.per_statement[0]), "kj ledger");
    }

    #[test]
    fn a_pipeline_of_reads_keys_every_command_once() {
        let e = unconfigured("kj block list | kj block list");
        assert_eq!(builtin_key_of(&e.per_statement[0]), "kj block list");
    }

    #[test]
    fn a_write_verb_is_uncovered() {
        let e = unconfigured("kj context archive 01a0-abc");
        assert_eq!(e.per_statement[0], PolicyVerdict::Uncovered);
        assert_eq!(e.verdict(), AskVerdict::Escalate);
    }

    #[test]
    fn a_redirect_drops_a_read_to_uncovered() {
        let e = unconfigured("kj block list > /tmp/x");
        assert_eq!(e.per_statement[0], PolicyVerdict::Uncovered);
    }

    /// A substitution is planned as its own command beside the kj call, so
    /// the kj half is allowed in isolation and the statement rule is what
    /// refuses: the substituted command has no builtin key.
    #[test]
    fn a_substituted_argument_drops_the_statement_to_uncovered() {
        let e = unconfigured("kj ledger allow $(cat /tmp/id)");
        assert_eq!(e.per_statement[0], PolicyVerdict::Uncovered);
    }

    /// A config allow that cannot cover a command leaves the builtin layer
    /// to decide it: allowing `kj context create` must not make
    /// `kj context create --help` ask when no config names it. A config ask or
    /// deny still decides first.
    ///
    /// Falsified by returning uncovered from a config allow that fails
    /// the structural check, skipping the builtin layer.
    #[test]
    fn a_config_allow_does_not_take_help_away() {
        let cfg = config("[global]\nallow = [\"kj context create\"]\n[context_type.director]\nallow = [\"kj context create\"]\n");
        for context_type in [None, Some("director")] {
            assert_eq!(builtin_key_of(&first_verdict("kj context create --help", &cfg, context_type)), "kj context --help",
                "{context_type:?}");
        }
        assert_eq!(first_verdict("kj context create x > /tmp/x", &cfg, None), PolicyVerdict::Uncovered,
            "a redirect still keeps the allow from covering, and no builtin covers it");
        let asks = config("[global]\nask = [\"kj context create\"]\n");
        assert!(matches!(first_verdict("kj context create --help", &asks, None), PolicyVerdict::Ask(_)),
            "an ask is firm");
    }

    /// The `--help` rule: help
    /// is the last word with no flag before it. `--content --help` binds
    /// help as a VALUE and writes for real; it must stay uncovered.
    #[test]
    fn help_is_builtin_allowed_and_the_content_help_bypass_is_not() {
        assert_eq!(builtin_key_of(&unconfigured("kj rc add --help").per_statement[0]), "kj rc --help");
        assert_eq!(builtin_key_of(&unconfigured("kj -h").per_statement[0]), "kj --help");
        assert_eq!(
            builtin_key_of(&unconfigured("kj rc rm /config/rc/coder/create/S00.kai --help").per_statement[0]),
            "kj rc --help"
        );
        assert_eq!(
            unconfigured("kj rc add /config/rc/x --content --help").per_statement[0],
            PolicyVerdict::Uncovered,
            "--content --help is a write, not help"
        );
        assert_eq!(
            unconfigured("kj rc add --help > /tmp/x").per_statement[0],
            PolicyVerdict::Uncovered,
            "a redirect makes help a write"
        );
        assert_eq!(
            unconfigured("cargo test --help").per_statement[0],
            PolicyVerdict::Uncovered,
            "only kj's own help is known to be inert"
        );
        assert_eq!(
            unconfigured("kj --json rc add --help").per_statement[0],
            PolicyVerdict::Uncovered,
            "a root flag ahead of the verb starts with '-', so this over-asks rather than \
             widening the rule; the hook's jq copy draws the same line"
        );
    }

    /// The whole-program composition: one non-allowed statement escalates
    /// the submission, and an empty program is never vacuously allowed.
    #[test]
    fn a_mixed_program_escalates_and_an_empty_one_is_not_an_allow() {
        let mixed = unconfigured("kj block list; kj block create --role user --kind text");
        assert_eq!(mixed.per_statement.len(), 2);
        assert!(matches!(mixed.per_statement[0], PolicyVerdict::Allow(_)));
        assert_eq!(mixed.per_statement[1], PolicyVerdict::Uncovered);
        assert_eq!(mixed.verdict(), AskVerdict::Escalate);

        let only_answers = unconfigured("kj ledger allow 01a0-abc; kj block list");
        assert_eq!(only_answers.verdict(), AskVerdict::Allow);

        let none = GateConfig::default();
        let layers = Layers { config: &none, context_type: None };
        assert_eq!(evaluate_planned(&[], layers).verdict(), AskVerdict::Escalate);
    }

    /// `verdict()` re-states the ledger's composition rather than
    /// delegating to it (the ledger composes rule rows, this composes
    /// layers). Pinned against `AskCoverage::verdict` over every mix of
    /// verdicts so the two cannot drift apart unnoticed.
    #[test]
    fn program_verdict_agrees_with_the_ledger_s_own_composition() {
        use approval_ledger::types::{AskCoverage, RuleScope};
        let row = || RuleRow {
            rule_id: "r".into(),
            statement_digest: "d".into(),
            authorized_label: "l".into(),
            context_id: None,
            principal_id: None,
            scope: RuleScope::Always,
            allow: true,
            created_at: 0,
            created_by: None,
            learned_from: None,
            revoked_at: None,
        };
        let d = || Decision { layer: Layer::Builtin, key: "k".into() };
        let allow = || PolicyVerdict::Allow(vec![d()]);
        let deny = || PolicyVerdict::Deny(d());
        let cases: Vec<Vec<PolicyVerdict>> = vec![
            vec![],
            vec![allow()],
            vec![PolicyVerdict::Uncovered],
            vec![PolicyVerdict::Ask(d())],
            vec![deny()],
            vec![allow(), allow()],
            vec![allow(), PolicyVerdict::Uncovered],
            vec![allow(), PolicyVerdict::Ask(d())],
            vec![allow(), deny()],
            vec![PolicyVerdict::Uncovered, deny()],
        ];
        for per_statement in cases {
            let ledger = AskCoverage {
                per_statement: per_statement
                    .iter()
                    .map(|v| match v {
                        PolicyVerdict::Allow(_) => StatementVerdict::Allow(row()),
                        PolicyVerdict::Deny(_) => StatementVerdict::Deny(row()),
                        PolicyVerdict::Ask(_) | PolicyVerdict::Uncovered => {
                            StatementVerdict::Uncovered
                        }
                    })
                    .collect(),
            };
            let ours = PolicyEvaluation { per_statement: per_statement.clone() };
            assert_eq!(ours.verdict(), ledger.verdict(), "{per_statement:?}");
        }
    }

    #[test]
    fn a_program_that_does_not_parse_is_not_evaluated_here() {
        // `plan_program` refuses; the caller sees no statements and the
        // hooks decide. Pinned so the evaluator never grows a lenient path.
        assert!(kaish_kernel::ast::plan::plan_program("kj block list ((").is_err());
    }

    #[test]
    fn describe_names_layer_and_key_on_allow_and_the_denied_statement_on_deny() {
        let statements = vec![
            GatedStatement {
                rendered: "kj block list".into(),
                statement_kind: "command".into(),
                vars: vec![],
                source_index: Some(1),
            },
            GatedStatement {
                rendered: "rm -rf foo".into(),
                statement_kind: "command".into(),
                vars: vec![],
                source_index: Some(3),
            },
        ];
        let allow = PolicyEvaluation {
            per_statement: vec![
                PolicyVerdict::Allow(vec![Decision { layer: Layer::Builtin, key: "kj block list".into() }]),
                PolicyVerdict::Allow(vec![Decision {
                    layer: Layer::UserRule,
                    key: "the exact statement (rule r1)".into(),
                }]),
            ],
        };
        assert_eq!(
            allow.describe(&statements, true),
            "gate policy: builtin allows kj block list; user rule allows the exact statement (rule r1)"
        );
        let deny = PolicyEvaluation {
            per_statement: vec![
                PolicyVerdict::Uncovered,
                PolicyVerdict::Deny(Decision { layer: Layer::GlobalConfig, key: "rm".into() }),
            ],
        };
        let text = deny.describe(&statements, false);
        assert_eq!(text, "gate policy: global config denies rm — statement #3 (`rm -rf foo`)");
    }

    // ── the config file ─────────────────────────────────────────────

    const EXAMPLE: &str = r#"
[global]
allow = ["kj handoff note", "rg", "wc"]
ask = ["kj rc add", "git push"]
deny = ["dd"]

[context_type.explorer]
deny = ["kj context create"]
"#;

    #[test]
    fn the_shipped_default_parses_and_every_kj_key_names_a_live_leaf() {
        let cfg = config(crate::config_seed::DEFAULT_GATE_CONFIG);
        assert!(
            cfg.entries()
                .iter()
                .any(|(s, k, v)| s == "global" && k == "kj handoff note" && *v == "allow"),
            "{:?}",
            cfg.entries()
        );
    }

    #[test]
    fn an_unknown_section_or_verdict_word_fails_the_load() {
        let err = GateConfig::parse("[globl]\nallow = [\"rg\"]\n").unwrap_err();
        assert!(matches!(err, GateConfigError::Parse(ref m) if m.contains("globl")), "{err:?}");
        let err = GateConfig::parse("[global]\npermit = [\"rg\"]\n").unwrap_err();
        assert!(matches!(err, GateConfigError::Parse(ref m) if m.contains("permit")), "{err:?}");
    }

    #[test]
    fn a_kj_key_naming_no_live_leaf_fails_the_load_naming_it() {
        let err = GateConfig::parse("[global]\nallow = [\"kj blok list\"]\n").unwrap_err();
        let GateConfigError::Parse(m) = err else { panic!("{err:?}") };
        assert!(m.contains("[global] allow") && m.contains("kj blok list") && m.contains("blok"), "{m}");
        let err = GateConfig::parse("[context_type.coder]\ndeny = [\"kj block lisst\"]\n").unwrap_err();
        let GateConfigError::Parse(m) = err else { panic!("{err:?}") };
        assert!(m.contains("[context_type.coder] deny") && m.contains("lisst"), "{m}");
        let err = GateConfig::parse("[global]\nask = [\"git push origin\"]\n").unwrap_err();
        assert!(matches!(err, GateConfigError::Parse(_)), "three tokens is not a key: {err:?}");
        // A flag as the second token could never match a command (the key
        // derivation skips flags), so the entry would be silently dead.
        let err = GateConfig::parse("[global]\nallow = [\"rg -n\"]\n").unwrap_err();
        let GateConfigError::Parse(m) = err else { panic!("{err:?}") };
        assert!(m.contains("rg -n") && m.contains("flag"), "{m}");
    }

    #[test]
    fn a_kj_key_written_with_an_alias_stores_the_canonical_name() {
        let cfg = config("[global]\nallow = [\"kj stage ex\"]\n");
        assert!(
            cfg.entries().iter().any(|(_, k, _)| k == "kj stage exclude"),
            "{:?}",
            cfg.entries()
        );
    }

    #[test]
    fn the_error_text_names_the_file_and_the_remedy() {
        let text = GateConfigError::Parse("x".into()).to_string();
        assert!(
            text.contains("/config/kernel/gate.toml") && text.contains("kj config reset gate.toml"),
            "{text}"
        );
    }

    /// A `[classifier]` section is an unknown section like any other: the
    /// load fails naming it, so a host file still carrying one is fixed
    /// rather than half-read.
    #[test]
    fn a_classifier_section_fails_the_load_as_an_unknown_section() {
        let err = GateConfig::parse("[classifier]\nurl = \"http://127.0.0.1:8088\"\n").unwrap_err();
        assert!(matches!(err, GateConfigError::Parse(ref m) if m.contains("classifier")), "{err:?}");
        assert!(err.to_string().contains("kj config reset gate.toml"), "{err}");
    }

    // ── config layers ───────────────────────────────────────────────

    #[test]
    fn a_global_allow_covers_a_kj_write_verb_and_a_non_kj_command() {
        let cfg = config(EXAMPLE);
        let v = first_verdict("kj handoff note 'a note'", &cfg, None);
        assert_eq!(allow_keys(&v), vec![(Layer::GlobalConfig, "kj handoff note".to_string())]);
        let v = first_verdict("rg -n foo src", &cfg, Some("coder"));
        assert_eq!(allow_keys(&v), vec![(Layer::GlobalConfig, "rg".to_string())]);
    }

    #[test]
    fn a_two_token_key_matches_the_first_argument_only() {
        let cfg = config(EXAMPLE);
        assert!(matches!(first_verdict("git push origin main", &cfg, None), PolicyVerdict::Ask(_)));
        assert_eq!(first_verdict("git status", &cfg, None), PolicyVerdict::Uncovered);
        assert_eq!(
            first_verdict("git -C x push", &cfg, None),
            PolicyVerdict::Uncovered,
            "a flag is not a family token, and nothing looks past it"
        );
    }

    #[test]
    fn deny_and_ask_fire_regardless_of_structure_and_an_allow_does_not() {
        let cfg = config(EXAMPLE);
        assert!(matches!(
            first_verdict("dd if=/dev/zero of=/dev/sda > /tmp/log", &cfg, None),
            PolicyVerdict::Deny(_)
        ));
        assert!(matches!(first_verdict("dd if=x &", &cfg, None), PolicyVerdict::Deny(_)));
        assert!(matches!(first_verdict("git push > /tmp/log", &cfg, None), PolicyVerdict::Ask(_)));
        assert_eq!(first_verdict("rg foo > ~/.bashrc", &cfg, None), PolicyVerdict::Uncovered);
        assert_eq!(
            first_verdict("kj handoff note 'x' > ~/.bashrc", &cfg, None),
            PolicyVerdict::Uncovered
        );
        assert_eq!(first_verdict("rg foo &", &cfg, None), PolicyVerdict::Uncovered);
    }

    #[test]
    fn a_kj_allow_needs_the_argv_to_classify() {
        let cfg = config("[global]\nallow = [\"kj wait\"]\n");
        assert_eq!(
            first_verdict("kj wait ${CTX} --timeout ${T}", &cfg, None),
            PolicyVerdict::Uncovered,
            "a ${{VAR}} in a typed slot does not parse, so the allow does not cover it"
        );
        assert!(matches!(
            first_verdict("kj wait 01a0 --timeout 5", &cfg, None),
            PolicyVerdict::Allow(_)
        ));
    }

    #[test]
    fn the_context_type_layer_outranks_global_and_a_specific_key_outranks_a_general_one() {
        let cfg = config(
            "[global]\nallow = [\"kj context\"]\ndeny = [\"kj context create\"]\n\
             [context_type.explorer]\nallow = [\"kj context create\"]\n",
        );
        // Within global: the specific deny beats the general allow.
        assert!(matches!(first_verdict("kj context create x", &cfg, None), PolicyVerdict::Deny(_)));
        assert!(matches!(first_verdict("kj context list", &cfg, None), PolicyVerdict::Allow(_)));
        // The context_type layer sits above global.
        let v = first_verdict("kj context create x", &cfg, Some("explorer"));
        assert_eq!(
            allow_keys(&v),
            vec![(Layer::ContextTypeConfig("explorer".into()), "kj context create".to_string())]
        );
        assert!(matches!(
            first_verdict("kj context create x", &cfg, Some("coder")),
            PolicyVerdict::Deny(_)
        ));
    }

    #[test]
    fn at_equal_specificity_deny_beats_ask_beats_allow() {
        let cfg = config("[global]\nallow = [\"rg\"]\nask = [\"rg\"]\n");
        assert!(matches!(first_verdict("rg x", &cfg, None), PolicyVerdict::Ask(_)));
        let cfg = config("[global]\nask = [\"rg\"]\ndeny = [\"rg\"]\n");
        assert!(matches!(first_verdict("rg x", &cfg, None), PolicyVerdict::Deny(_)));
    }

    #[test]
    fn a_config_deny_outranks_the_builtin_allow_and_an_ask_is_firm() {
        let cfg = config("[global]\ndeny = [\"kj block list\"]\nask = [\"kj block read\"]\n");
        assert!(matches!(first_verdict("kj block list", &cfg, None), PolicyVerdict::Deny(_)));
        let e = evaluate_planned(&plan("kj block read 01a0"), Layers { config: &cfg, context_type: None });
        assert!(matches!(e.per_statement[0], PolicyVerdict::Ask(_)));
        assert_eq!(e.verdict(), AskVerdict::Escalate);
        assert_eq!(e.per_statement[0].tier(), "ask");
    }

    #[test]
    fn a_statement_folds_its_commands_deny_first_then_ask_then_all_allow() {
        let cfg = config(EXAMPLE);
        let layers = Layers { config: &cfg, context_type: None };
        assert!(matches!(
            evaluate_planned(&plan("rg x | dd"), layers).per_statement[0],
            PolicyVerdict::Deny(_)
        ));
        assert!(matches!(
            evaluate_planned(&plan("rg x | git push"), layers).per_statement[0],
            PolicyVerdict::Ask(_)
        ));
        let v = evaluate_planned(&plan("kj block list | rg x"), layers).per_statement.remove(0);
        assert_eq!(
            allow_keys(&v),
            vec![
                (Layer::Builtin, "kj block list".to_string()),
                (Layer::GlobalConfig, "rg".to_string()),
            ]
        );
        assert_eq!(
            evaluate_planned(&plan("rg x | cat"), layers).per_statement[0],
            PolicyVerdict::Uncovered
        );
    }

    #[test]
    fn tiers_name_the_hook_facing_words() {
        let cfg = config(EXAMPLE);
        let layers = Layers { config: &cfg, context_type: None };
        let stmt = &plan("rg x")[0];
        assert_eq!(command_verdict(&stmt.plan.commands[0], layers).tier(), "allow");
        let stmt = &plan("git push")[0];
        assert_eq!(command_verdict(&stmt.plan.commands[0], layers).tier(), "ask");
        let stmt = &plan("dd")[0];
        assert_eq!(command_verdict(&stmt.plan.commands[0], layers).tier(), "deny");
        let stmt = &plan("cat x")[0];
        assert_eq!(command_verdict(&stmt.plan.commands[0], layers).tier(), "score");
    }

    // ── the uncovered tier ──────────────────────────────────────────

    /// A sandbox posture on one context type, beside ordinary tiers.
    const SANDBOX: &str = r#"
[global]
allow = ["rg"]
ask = ["git push"]
deny = ["dd"]

[context_type.coder]
uncovered = "allow"
"#;

    /// Which constructs plan a statement with no command — what the
    /// statement-level branch of the tier decides. kaish's `collect_stmt`
    /// walks every nested construct, including a `$(…)` inside an
    /// assignment, a test or an arithmetic operand, so a statement with no
    /// planned command invokes nothing: an assignment to a literal, a
    /// `test`, arithmetic, a control-flow statement over literals, a
    /// function definition whose body runs nothing, and the loop keywords.
    /// It is not assignments alone, which is why the branch is written
    /// against "no command" rather than against the statement kind.
    #[test]
    fn a_statement_with_no_command_is_more_than_an_assignment() {
        let commandless = |src: &str| {
            let statements = plan(src);
            assert_eq!(statements.len(), 1, "{src}");
            statements[0].plan.commands.is_empty()
        };
        for src in [
            "OUT=log",                     // assignment
            "exit 3",                      // exit
            "return",                      // return
            "break",                       // break
            "continue",                    // continue
            "(( i + 1 ))",                 // arith
            "[[ -f /etc/passwd ]]",        // test
            "for x in a b; do Y=$x; done", // for
            "case $x in a) Y=1;; esac",    // case
            "deploy() { X=1 }",            // tooldef
            "function deploy { X=1 }",     // tooldef
        ] {
            assert!(commandless(src), "{src} was expected to plan no command");
        }
        // A nested command is collected wherever it hides, so these are not
        // commandless and the per-command layers decide them. `test -f x`
        // is an ordinary command named `test`, not the `test` statement.
        for src in [
            "OUT=$(ls)",
            "if ls; then X=1; fi",
            "for x in $(ls); do Y=$x; done",
            "test -f /etc/passwd",
        ] {
            assert!(!commandless(src), "{src} was expected to plan a command");
        }
    }

    /// The tier covers a commandless statement, and without it that one
    /// statement escalates the whole submission.
    #[test]
    fn the_tier_covers_a_commandless_statement_and_the_default_still_escalates_it() {
        let cfg = config(SANDBOX);
        let v = first_verdict("OUT=log", &cfg, Some("coder"));
        assert_eq!(
            allow_keys(&v),
            vec![(
                Layer::UncoveredAllow(Some("coder".to_string())),
                "`OUT=log`, a statement with no command".to_string()
            )]
        );
        assert_eq!(
            first_verdict("OUT=log", &cfg, Some("director")),
            PolicyVerdict::Uncovered,
            "with the tier off, a commandless statement escalates as it does today"
        );
    }

    /// The posture: a statement no key covers is allowed, redirect and all,
    /// and the decision names the tier rather than an allow list.
    #[test]
    fn the_uncovered_tier_allows_what_no_key_covers() {
        let cfg = config(SANDBOX);
        let v = first_verdict("python3 build.py > log 2>&1", &cfg, Some("coder"));
        assert_eq!(
            allow_keys(&v),
            vec![(
                Layer::UncoveredAllow(Some("coder".to_string())),
                "python3 build.py".to_string()
            )]
        );
        // A kj write verb is uncovered too, and the tier decides it.
        assert!(matches!(
            first_verdict("kj context remove 01a0-abc", &cfg, Some("coder")),
            PolicyVerdict::Allow(_)
        ));
    }

    /// The setting is per section: another context type, and a caller with
    /// no context type at all, keep today's behavior.
    #[test]
    fn the_uncovered_tier_reaches_only_the_section_that_sets_it() {
        let cfg = config(SANDBOX);
        assert_eq!(
            first_verdict("python3 build.py > log", &cfg, Some("explorer")),
            PolicyVerdict::Uncovered
        );
        assert_eq!(first_verdict("python3 build.py", &cfg, None), PolicyVerdict::Uncovered);
    }

    /// A director's routine lane work runs without an ask: it creates a
    /// coder and drives it (`docs/character.md`, "A session, inside
    /// kaijutsu", step 3). Repair of a running lane, such as recasting its
    /// performer, still meets the director's reviewer.
    #[test]
    fn the_shipped_default_lets_a_director_create_and_drive_a_lane() {
        let cfg = config(crate::config_seed::DEFAULT_GATE_CONFIG);
        for source in [
            "kj context create count-kj-rs --type coder --as coder",
            "kj fork --name lane --prompt \"count the files\"",
            "kj drive count-kj-rs --prompt \"count the files\"",
        ] {
            let v = first_verdict(source, &cfg, Some("director"));
            let keys = allow_keys(&v);
            assert_eq!(keys.len(), 1, "{source}: {v:?}");
            assert_eq!(keys[0].0, Layer::ContextTypeConfig("director".to_string()), "{source}: {v:?}");
            assert_eq!(
                first_verdict(source, &cfg, Some("coder")),
                PolicyVerdict::Uncovered,
                "{source} is the director's work, not a coder's"
            );
        }
        assert_eq!(
            first_verdict("kj context set count-kj-rs --as coder", &cfg, Some("director")),
            PolicyVerdict::Uncovered
        );
    }

    /// The shipped default leaves a model's `shell_write` statement to ask,
    /// and ships the allow that removes that first ask as a commented
    /// example: loosening is the operator's choice, per seat (F16).
    #[test]
    fn the_shipped_default_comments_out_the_shell_write_allow() {
        const EXAMPLE_LINES: [&str; 2] = ["#   [context_type.coder]", "#   allow = [\"shell_write\"]"];
        let shipped = crate::config_seed::DEFAULT_GATE_CONFIG;
        assert!(
            shipped.contains(&EXAMPLE_LINES.join("\n")),
            "the shipped gate.toml must carry the shell_write example verbatim"
        );
        let source = "shell_write 'git commit -m \"a message\"'";
        assert_eq!(first_verdict(source, &config(shipped), Some("coder")), PolicyVerdict::Uncovered);
        let uncommented: String = EXAMPLE_LINES.iter().map(|l| format!("{}\n", &l[4..])).collect();
        let v = first_verdict(source, &config(&uncommented), Some("coder"));
        assert_eq!(
            allow_keys(&v),
            vec![(Layer::ContextTypeConfig("coder".to_string()), "shell_write".to_string())],
            "got {v:?}"
        );
    }

    /// Absent is today's behavior, and the shipped default leaves it absent.
    #[test]
    fn the_shipped_default_leaves_the_uncovered_tier_at_ask() {
        let cfg = config(crate::config_seed::DEFAULT_GATE_CONFIG);
        for context_type in [None, Some("coder"), Some("mcp"), Some("explorer")] {
            assert_eq!(
                cfg.uncovered_for(context_type),
                (UncoveredTier::Ask, None),
                "{context_type:?}"
            );
            assert_eq!(
                first_verdict("python3 build.py", &cfg, context_type),
                PolicyVerdict::Uncovered,
                "{context_type:?}"
            );
        }
        assert_eq!(config(EXAMPLE).uncovered_for(Some("coder")), (UncoveredTier::Ask, None));
    }

    /// Ask stays firm and deny stays deny: both are decided before the
    /// tier is reached.
    #[test]
    fn deny_and_ask_outrank_the_uncovered_tier() {
        let cfg = config(SANDBOX);
        assert!(matches!(
            first_verdict("dd if=/dev/zero of=/dev/sda", &cfg, Some("coder")),
            PolicyVerdict::Deny(_)
        ));
        assert!(matches!(
            first_verdict("git push origin main", &cfg, Some("coder")),
            PolicyVerdict::Ask(_)
        ));
        let layers = Layers { config: &cfg, context_type: Some("coder") };
        assert_eq!(
            evaluate_planned(&plan("python3 x.py; git push origin main"), layers).verdict(),
            AskVerdict::Escalate,
            "one ask-tier statement still escalates the submission"
        );
    }

    /// An allow list entry the structural veto drops falls through to the
    /// tier, and an ordinary allow-list hit still reads as one — an
    /// operator can tell the two apart in a ledger row.
    #[test]
    fn a_structurally_vetoed_allow_falls_through_to_the_tier_and_stays_distinguishable() {
        let cfg = config(SANDBOX);
        let v = first_verdict("rg foo > ~/.bashrc", &cfg, Some("coder"));
        assert_eq!(
            allow_keys(&v),
            vec![(Layer::UncoveredAllow(Some("coder".to_string())), "rg foo".to_string())]
        );
        let v = first_verdict("rg foo src", &cfg, Some("coder"));
        assert_eq!(allow_keys(&v), vec![(Layer::GlobalConfig, "rg".to_string())]);
    }

    /// The whole submission auto-allows: every statement is an Allow,
    /// including a bare assignment, which has no command to key on.
    #[test]
    fn a_whole_program_auto_allows_under_the_uncovered_tier() {
        let cfg = config(SANDBOX);
        let layers = Layers { config: &cfg, context_type: Some("coder") };
        let e = evaluate_planned(
            &plan("OUT=log; cargo test --workspace 2>&1 | tee $OUT; kj block create --role user --kind text"),
            layers,
        );
        assert_eq!(e.verdict(), AskVerdict::Allow, "{:?}", e.per_statement);
        assert_eq!(
            e.describe(&[], true).contains("uncovered tier"),
            true,
            "{}",
            e.describe(&[], true)
        );
    }

    /// The hook stack reads the same tier off `KJ_TOOL_PLAN`: an allow-tier
    /// command is dropped from the scored clause set, so nothing re-scores
    /// what the tier allowed.
    #[test]
    fn the_uncovered_tier_stamps_allow_on_every_command() {
        let cfg = config(SANDBOX);
        let layers = Layers { config: &cfg, context_type: Some("coder") };
        let stmt = &plan("cat /etc/passwd")[0];
        assert_eq!(command_verdict(&stmt.plan.commands[0], layers).tier(), "allow");
        let layers = Layers { config: &cfg, context_type: Some("explorer") };
        assert_eq!(command_verdict(&stmt.plan.commands[0], layers).tier(), "score");
    }

    /// A context type's setting wins over the global one, in both
    /// directions — a global sandbox can be withheld from one role.
    #[test]
    fn a_context_type_setting_outranks_the_global_one() {
        let cfg = config("[global]\nuncovered = \"allow\"\n[context_type.director]\nuncovered = \"ask\"\n");
        let v = first_verdict("cat x", &cfg, Some("coder"));
        assert_eq!(allow_keys(&v), vec![(Layer::UncoveredAllow(None), "cat x".to_string())]);
        assert_eq!(first_verdict("cat x", &cfg, Some("director")), PolicyVerdict::Uncovered);
    }

    #[test]
    fn an_unknown_uncovered_value_fails_the_load_naming_the_words_it_takes() {
        let err = GateConfig::parse("[global]\nuncovered = \"yes\"\n").unwrap_err();
        let GateConfigError::Parse(m) = err else { panic!("{err:?}") };
        assert!(
            m.contains("[global]") && m.contains("yes") && m.contains("ask") && m.contains("allow"),
            "{m}"
        );
        // `deny` is a key list, never a posture: denying everything unlisted
        // is not offered here.
        let err = GateConfig::parse("[context_type.coder]\nuncovered = \"deny\"\n").unwrap_err();
        let GateConfigError::Parse(m) = err else { panic!("{err:?}") };
        assert!(m.contains("[context_type.coder]") && m.contains("deny"), "{m}");
    }

    /// `kj ledger rules` lists the posture, so an operator sees it without
    /// reading the file.
    #[test]
    fn the_uncovered_tier_is_listed_for_the_context_it_applies_to() {
        let cfg = config(SANDBOX);
        let entries = cfg.entries_for(Some("coder"));
        assert_eq!(
            entries.first().map(|(l, k, v)| (l.clone(), k.clone(), *v)),
            Some((
                Layer::UncoveredAllow(Some("coder".to_string())),
                "uncovered".to_string(),
                "allow"
            )),
            "{entries:?}"
        );
        assert!(cfg.allows_uncovered(Some("coder")));
        assert!(!cfg.allows_uncovered(Some("explorer")));
        assert!(!cfg.entries_for(Some("explorer")).iter().any(|(l, _, _)| {
            matches!(l, Layer::UncoveredAllow(_))
        }));
    }

    // ── family keys ─────────────────────────────────────────────────

    #[test]
    fn family_keys_are_the_most_specific_key_per_command() {
        let keys = family_keys_for_program(&plan("kj handoff note 'a note'; git push origin main | wc -l")).unwrap();
        assert_eq!(keys, vec!["kj handoff note", "git push", "wc"], "a flag is not a family token");
    }

    /// Guarantee 3 does not apply to a family: the note text is a free
    /// variable, which a digest rule refuses and a family key never reads.
    #[test]
    fn a_free_variable_in_a_value_slot_still_teaches_a_family() {
        let keys = family_keys_for_program(&plan("kj handoff note ${NOTE}")).unwrap();
        assert_eq!(keys, vec!["kj handoff note"]);
    }

    #[test]
    fn a_family_is_refused_on_structure_naming_the_condition() {
        let err = |src: &str| family_keys_for_program(&plan(src)).unwrap_err().to_string();
        assert!(err("kj handoff note 'x' > ~/.bashrc").contains("has a redirect"));
        assert!(
            err("ls; kj handoff note 'x' > ~/.bashrc").contains("`kj handoff note x` has a redirect"),
            "the refusal names the offending command, not just its name: {}",
            err("ls; kj handoff note 'x' > ~/.bashrc")
        );
        assert!(err("kj handoff note 'x' &").contains("runs in the background"));
        // kaish plans every value `Plain` today; the non-plain arm guards
        // its `#[non_exhaustive]` redaction seam and cannot be reached
        // through `plan_program`.
        assert!(err("kj wait ${CTX} --timeout ${T}").contains("does not parse"));
        // A substitution plans as its own command beside the kj call, so
        // the program teaches two families, and the kj half is the verb.
        assert_eq!(
            family_keys_for_program(&plan("kj ledger allow $(cat /tmp/id)")).unwrap(),
            vec!["kj ledger allow", "cat /tmp/id"]
        );
        assert!(err("kj blok list").contains("names no kj verb"));
    }

    // ── context_type sections ───────────────────────────────────────

    /// A `MountTable` with an rc tree naming `types`, for the section
    /// validation `load_config` does.
    async fn rc_tree(types: &[&str]) -> (MountTable, tempfile::TempDir, tempfile::TempDir) {
        use crate::vfs::LocalBackend;
        let rc = tempfile::tempdir().unwrap();
        for t in types {
            std::fs::create_dir_all(rc.path().join(t)).unwrap();
        }
        let config = tempfile::tempdir().unwrap();
        let vfs = MountTable::new();
        vfs.mount(kaijutsu_types::paths::RC_ROOT, LocalBackend::new(rc.path())).await;
        vfs.mount(kaijutsu_types::paths::CONFIG_ROOT, LocalBackend::new(config.path())).await;
        (vfs, rc, config)
    }

    fn write_gate(config: &tempfile::TempDir, text: &str) {
        std::fs::write(config.path().join(GATE_CONFIG_FILE), text).unwrap();
    }

    /// A typo'd section names no context type, so it applies to nobody and
    /// the type it meant to name keeps whatever `[global]` said. The load
    /// refuses it the way a kj key naming no verb is refused.
    #[tokio::test]
    async fn an_unknown_context_type_section_fails_the_load_naming_the_known_types() {
        let (vfs, _rc, config) = rc_tree(&["coder", "director", "lib"]).await;
        write_gate(
            &config,
            "[global]\nuncovered = \"allow\"\n[context_type.directer]\nuncovered = \"ask\"\n",
        );
        let err = load_config(&vfs).await.unwrap_err();
        let GateConfigError::Parse(m) = err else { panic!("{err:?}") };
        assert!(
            m.contains("context_type.directer") && m.contains("coder, director"),
            "{m}"
        );
        assert!(!m.contains("lib"), "`lib` is not a seat: {m}");
    }

    /// The fail-open shape the check exists for: without it the typo'd
    /// section is inert and `director` inherits the global allow. With the
    /// name spelled right, the section applies and holds the tier at ask.
    #[tokio::test]
    async fn a_typo_can_no_longer_leave_a_context_type_on_the_global_allow() {
        let (vfs, _rc, config) = rc_tree(&["coder", "director"]).await;
        write_gate(
            &config,
            "[global]\nuncovered = \"allow\"\n[context_type.director]\nuncovered = \"ask\"\n",
        );
        let cfg = load_config(&vfs).await.expect("the spelled-right file loads");
        assert!(cfg.allows_uncovered(Some("coder")));
        assert!(!cfg.allows_uncovered(Some("director")), "the section protects its type");

        // The same file with the name misspelled must not load at all: a
        // parsed-clean typo would leave `director` on the global allow.
        write_gate(
            &config,
            "[global]\nuncovered = \"allow\"\n[context_type.directer]\nuncovered = \"ask\"\n",
        );
        let typo = load_config(&vfs).await;
        assert!(typo.is_err(), "a typo'd section must refuse the file");
        let parsed = GateConfig::parse(
            "[global]\nuncovered = \"allow\"\n[context_type.directer]\nuncovered = \"ask\"\n",
        )
        .expect("parse itself stays a pure shape check");
        assert!(
            parsed.allows_uncovered(Some("director")),
            "this is what the load refuses to hand out: the typo'd section applies to nobody"
        );
    }

    /// Any unknown section, not only one setting `uncovered`: a typo'd
    /// allow list is inert in the same way.
    #[tokio::test]
    async fn an_unknown_section_with_only_a_key_list_fails_the_load_too() {
        let (vfs, _rc, config) = rc_tree(&["coder"]).await;
        write_gate(&config, "[context_type.codr]\nallow = [\"rg\"]\n");
        assert!(matches!(load_config(&vfs).await, Err(GateConfigError::Parse(_))));
    }

    /// An rc tree that lists nothing accepts every section — there is
    /// nothing to validate against, and a kernel with no rc tree is a
    /// legitimate shape. The rule `kj::rc::check_context_type` states.
    #[tokio::test]
    async fn an_empty_rc_tree_accepts_any_section() {
        let (vfs, _rc, config) = rc_tree(&[]).await;
        write_gate(&config, "[context_type.anything-goes]\nallow = [\"rg\"]\n");
        assert!(load_config(&vfs).await.is_ok());
    }

    /// The shipped `gate.toml` must load against the shipped rc tree.
    /// `[context_type.explorer]` did not: the type was renamed and the
    /// section had been inert ever since.
    #[tokio::test]
    async fn the_shipped_default_loads_against_the_shipped_rc_tree() {
        use crate::vfs::LocalBackend;
        let rc = tempfile::tempdir().unwrap();
        crate::seed_scripts::ensure_rc_seed_files(rc.path()).expect("seed rc");
        let config = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join(GATE_CONFIG_FILE),
            crate::config_seed::DEFAULT_GATE_CONFIG,
        )
        .unwrap();
        let vfs = MountTable::new();
        vfs.mount(kaijutsu_types::paths::RC_ROOT, LocalBackend::new(rc.path())).await;
        vfs.mount(kaijutsu_types::paths::CONFIG_ROOT, LocalBackend::new(config.path())).await;
        let loaded = load_config(&vfs).await;
        assert!(loaded.is_ok(), "{loaded:?}");
    }

    #[tokio::test]
    async fn an_absent_file_is_the_empty_config_and_a_bad_one_is_an_error() {
        use crate::vfs::LocalBackend;
        let vfs = MountTable::new();
        assert_eq!(load_config(&vfs).await, Ok(GateConfig::default()), "no mount at all");
        let dir = tempfile::tempdir().unwrap();
        vfs.mount(kaijutsu_types::paths::CONFIG_ROOT, LocalBackend::new(dir.path())).await;
        assert_eq!(load_config(&vfs).await, Ok(GateConfig::default()), "mounted, file absent");
        std::fs::write(dir.path().join(GATE_CONFIG_FILE), "[global]\nallow = [\"kj nope\"]\n").unwrap();
        assert!(matches!(load_config(&vfs).await, Err(GateConfigError::Parse(_))));
        std::fs::write(dir.path().join(GATE_CONFIG_FILE), EXAMPLE).unwrap();
        assert_eq!(load_config(&vfs).await, Ok(config(EXAMPLE)));
    }

    // ---- council configuration ----

    const COUNCIL_FULL: &str = r#"
[council]
server = "http://zorak:8090"
contexts = ["voice", "system-rules"]
pool = { method = "loglinear", weights = "mass" }
deadline_ms = 700

[[council.spec]]
name = "shell-gate"
case = "shell"

[[council.threshold]]
spec = "shell-gate"
weight_hash = "wh1"
engine = "mk-1"
tokenizer_hash = "tk1"
template = "mk-letters-1:abc"
allow_at = 0.98
mass_floor = -0.05

[[council.threshold]]
spec = "shell-gate"
weight_hash = "wh2"
engine = "mk-1"
tokenizer_hash = "tk1"
template = "mk-letters-1:abc"
allow_at = 0.9
mass_floor = -0.1

[context_type.coder.council]
enabled = true

[context_type.toolie.council]
enabled = false
"#;

    fn identity(weight_hash: &str) -> CouncilIdentity {
        CouncilIdentity {
            weight_hash: weight_hash.into(),
            engine: "mk-1".into(),
            tokenizer_hash: "tk1".into(),
            template: "mk-letters-1:abc".into(),
        }
    }

    fn council_err(text: &str) -> String {
        let GateConfigError::Parse(m) = GateConfig::parse(text).unwrap_err() else {
            panic!("expected a parse error")
        };
        m
    }

    /// COUNCIL_FULL with one `from` substring replaced by `to`.
    fn council_with(from: &str, to: &str) -> String {
        assert!(COUNCIL_FULL.contains(from), "fixture lacks {from}");
        COUNCIL_FULL.replacen(from, to, 1)
    }

    #[test]
    fn a_full_council_config_round_trips() {
        let cfg = config(COUNCIL_FULL);
        let c = cfg.council().expect("council declared");
        assert_eq!(c.server, "http://zorak:8090");
        assert_eq!(c.contexts, ["voice", "system-rules"]);
        assert_eq!(c.pool_method, CouncilPoolMethod::LogLinear);
        assert_eq!(c.pool_weights, CouncilPoolWeights::Mass);
        assert_eq!(c.deadline_ms, 700);
        assert!(c.require_agree, "require_agree defaults to true");
        assert_eq!(
            c.specs,
            [CouncilSpec { name: "shell-gate".into(), case: CouncilCase::Shell }]
        );
        assert_eq!(c.thresholds.len(), 2);
        assert_eq!(c.thresholds[0].allow_at, 0.98);
        assert_eq!(c.thresholds[0].mass_floor, -0.05);
    }

    #[test]
    fn require_agree_can_be_turned_off() {
        let cfg = config(&council_with("deadline_ms = 700", "deadline_ms = 700\nrequire_agree = false"));
        assert!(!cfg.council().unwrap().require_agree);
    }

    #[test]
    fn council_absent_means_off_everywhere() {
        let cfg = config("[global]\nallow = [\"rg\"]\n");
        assert!(cfg.council().is_none());
        assert!(!cfg.council_enabled_for(None));
        assert!(!cfg.council_enabled_for(Some("coder")));
        let shipped = config(crate::config_seed::DEFAULT_GATE_CONFIG);
        assert!(shipped.council().is_none(), "the shipped default is council-off");
        assert!(!shipped.council_enabled_for(Some("coder")));
    }

    #[test]
    fn council_is_enabled_per_context_type() {
        let cfg = config(COUNCIL_FULL);
        assert!(cfg.council_enabled_for(Some("coder")));
        assert!(!cfg.council_enabled_for(Some("toolie")), "enabled = false");
        assert!(!cfg.council_enabled_for(Some("director")), "unmentioned type");
        assert!(!cfg.council_enabled_for(None), "no context type");
    }

    #[test]
    fn a_type_enabling_a_council_the_file_lacks_fails_the_load() {
        let m = council_err("[context_type.coder.council]\nenabled = true\n");
        assert!(m.contains("coder") && m.contains("[council]"), "{m}");
        config("[context_type.coder.council]\nenabled = false\n");
    }

    #[test]
    fn global_gets_no_council_switch() {
        let m = council_err(&format!("[global.council]\nenabled = true\n{COUNCIL_FULL}"));
        assert!(m.contains("[global.council]"), "{m}");
    }

    #[test]
    fn unknown_council_fields_fail_the_load() {
        for text in [
            council_with("deadline_ms = 700", "deadline_ms = 700\nretries = 2"),
            council_with("case = \"shell\"", "case = \"shell\"\nextra = 1"),
            council_with("allow_at = 0.98", "allow_at = 0.98\nextra = 1"),
            council_with("enabled = true", "enabled = true\nextra = 1"),
            council_with("weights = \"mass\" }", "weights = \"mass\", extra = 1 }"),
        ] {
            let m = council_err(&text);
            assert!(m.contains("extra") || m.contains("retries"), "{m}");
        }
    }

    #[test]
    fn a_bad_council_server_is_rejected_naming_the_field() {
        for bad in ["zorak:8090", "ftp://zorak", "http://", "http://a b"] {
            let m = council_err(&council_with("http://zorak:8090", bad));
            assert!(m.contains("[council] server") && m.contains("http"), "{bad}: {m}");
        }
        config(&council_with("http://zorak:8090", "https://zorak.example:8090"));
    }

    #[test]
    fn council_contexts_must_be_nonempty_distinct_labels() {
        let m = council_err(&council_with("[\"voice\", \"system-rules\"]", "[]"));
        assert!(m.contains("[council] contexts") && m.contains("at least one"), "{m}");
        let m = council_err(&council_with("[\"voice\", \"system-rules\"]", "[\"voice\", \"\"]"));
        assert!(m.contains("[council] contexts") && m.contains("non-empty"), "{m}");
        let m = council_err(&council_with("[\"voice\", \"system-rules\"]", "[\"voice\", \"voice\"]"));
        assert!(m.contains("[council] contexts") && m.contains("voice") && m.contains("twice"), "{m}");
    }

    #[test]
    fn council_pool_words_are_checked() {
        let m = council_err(&council_with("method = \"loglinear\"", "method = \"geometric\""));
        assert!(m.contains("pool.method") && m.contains("linear, loglinear"), "{m}");
        let m = council_err(&council_with("weights = \"mass\"", "weights = \"even\""));
        assert!(m.contains("pool.weights") && m.contains("uniform, mass"), "{m}");
        let cfg = config(&council_with("\"loglinear\", weights = \"mass\"", "\"linear\", weights = \"uniform\""));
        let c = cfg.council().unwrap();
        assert_eq!((c.pool_method, c.pool_weights), (CouncilPoolMethod::Linear, CouncilPoolWeights::Uniform));
    }

    #[test]
    fn council_deadline_must_be_positive() {
        for bad in ["0", "-5"] {
            let m = council_err(&council_with("deadline_ms = 700", &format!("deadline_ms = {bad}")));
            assert!(m.contains("[council] deadline_ms") && m.contains("greater than 0"), "{m}");
        }
    }

    #[test]
    fn council_needs_a_spec_with_a_safe_unique_name_and_a_known_case() {
        let no_spec = COUNCIL_FULL
            .replace("[[council.spec]]\nname = \"shell-gate\"\ncase = \"shell\"\n", "");
        let m = council_err(&no_spec.replace("spec = \"shell-gate\"", "spec = \"x\""));
        assert!(m.contains("at least one [[council.spec]]"), "{m}");
        for bad in ["a/b", "a.json", "", "a b", ".."] {
            let m = council_err(&council_with("name = \"shell-gate\"", &format!("name = \"{bad}\"")));
            assert!(m.contains("[[council.spec]] name") && m.contains("[A-Za-z0-9_-]+"), "{bad}: {m}");
        }
        let m = council_err(&council_with("case = \"shell\"", "case = \"program\""));
        assert!(m.contains("case") && m.contains("program") && m.contains("shell"), "{m}");
        let dup = COUNCIL_FULL.replacen(
            "[[council.threshold]]",
            "[[council.spec]]\nname = \"shell-gate\"\ncase = \"shell\"\n\n[[council.threshold]]",
            1,
        );
        let m = council_err(&dup);
        assert!(m.contains("shell-gate") && m.contains("twice"), "{m}");
    }

    #[test]
    fn a_threshold_must_name_a_declared_spec() {
        let m = council_err(&council_with("spec = \"shell-gate\"", "spec = \"nope\""));
        assert!(m.contains("[[council.threshold]] spec") && m.contains("nope"), "{m}");
    }

    #[test]
    fn threshold_identity_fields_must_be_nonempty() {
        for field in ["weight_hash", "engine", "tokenizer_hash", "template"] {
            let from = format!("{field} = ");
            let line = COUNCIL_FULL
                .lines()
                .find(|l| l.starts_with(&from))
                .unwrap_or_else(|| panic!("fixture lacks {field}"));
            let m = council_err(&council_with(line, &format!("{field} = \"\"")));
            assert!(m.contains(field) && m.contains("non-empty"), "{field}: {m}");
        }
    }

    #[test]
    fn threshold_allow_at_must_be_in_the_unit_interval() {
        for bad in ["0", "-0.1", "1.01"] {
            let m = council_err(&council_with("allow_at = 0.98", &format!("allow_at = {bad}")));
            assert!(m.contains("allow_at") && m.contains("0 < allow_at <= 1"), "{bad}: {m}");
        }
        config(&council_with("allow_at = 0.98", "allow_at = 1"));
    }

    #[test]
    fn threshold_mass_floor_must_be_a_log_probability() {
        let m = council_err(&council_with("mass_floor = -0.05", "mass_floor = 0.1"));
        assert!(m.contains("mass_floor") && m.contains("at most 0"), "{m}");
        config(&council_with("mass_floor = -0.05", "mass_floor = 0"));
    }

    #[test]
    fn two_thresholds_for_one_spec_and_identity_are_an_error() {
        let m = council_err(&council_with("weight_hash = \"wh2\"", "weight_hash = \"wh1\""));
        assert!(m.contains("one threshold per identity") && m.contains("shell-gate"), "{m}");
    }

    #[test]
    fn thresholds_are_found_by_spec_and_identity() {
        let cfg = config(COUNCIL_FULL);
        let c = cfg.council().unwrap();
        assert_eq!(c.threshold_for("shell-gate", &identity("wh1")).unwrap().allow_at, 0.98);
        assert_eq!(c.threshold_for("shell-gate", &identity("wh2")).unwrap().allow_at, 0.9);
        assert!(c.threshold_for("shell-gate", &identity("other")).is_none());
        assert!(c.threshold_for("other-spec", &identity("wh1")).is_none());
        let mut shifted = identity("wh1");
        shifted.template = "mk-letters-2:abc".into();
        assert!(c.threshold_for("shell-gate", &shifted).is_none(), "every identity field counts");
    }

    #[test]
    fn the_shipped_default_has_no_live_council_section_but_documents_one() {
        let body = crate::config_seed::DEFAULT_GATE_CONFIG;
        assert!(body.contains("[council]"), "the example is in the file");
        assert!(
            body.lines().all(|l| !l.starts_with("[council") && !l.starts_with("[[council")),
            "every council line in the shipped default is a comment"
        );
    }
}
