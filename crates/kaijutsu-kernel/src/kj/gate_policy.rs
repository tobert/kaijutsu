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
//!    <scope> --family`. The exact statement beats the family. A rule of
//!    scope `character` matches the actor it was learned from, wherever
//!    it performs; its allow covers a `kj` command only under the
//!    character condition ([`serves_character`]).
//! 2. **Performer config** — in `/config/kernel/gate.toml`, for the
//!    calling context's performer, narrowest first: `[character.<name>]`
//!    for the performer, `[root]` when a root character performs it, then
//!    `[rc.<dir>]` for the performer's rc directory. A context with no
//!    performer reads none of them.
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

use approval_ledger::rules::RuleMatch;
use approval_ledger::types::{AskVerdict, FamilyRuleRow, Origin, RuleRow, StatementVerdict};
use kaijutsu_types::{ContextId, PrincipalId};
use kaish_kernel::PlannedStatement;
use kaish_types::plan::PlannedCommand;
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
    /// A tier table in `gate.toml`.
    Config(Section),
    /// The verb's declared effect, the `kj ledger` structural exemption, a
    /// `kj … --help` invocation, or an MCP session created under the
    /// caller's own MCP hub.
    Builtin,
    /// `uncovered = "allow"` in `gate.toml` — the sandbox posture, naming
    /// the section that set it.
    UncoveredAllow(Section),
    /// The council's allow for a statement no other layer covered, naming
    /// the spec it read the submission under (`docs/council.md`).
    Council(String),
    /// A drift along the caller's senpai edge — to its senpai or to a context
    /// that answers to it — that no key decided (`docs/senpai-kohai.md`, "Reports and drift").
    SenpaiEdge,
}

impl std::fmt::Display for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserRule => f.write_str("user rule"),
            Self::UserFamily => f.write_str("user family rule"),
            Self::Config(section) => write!(f, "{section}"),
            Self::Builtin => f.write_str("builtin"),
            Self::UncoveredAllow(section) => write!(f, "{section} uncovered tier"),
            Self::Council(spec) => write!(f, "council ({spec})"),
            Self::SenpaiEdge => f.write_str("senpai edge"),
        }
    }
}

/// A `gate.toml` section that holds a tier table.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Section {
    /// `[global]`.
    Global,
    /// `[rc.<dir>]`: every context whose performer runs this rc directory.
    Rc(String),
    /// `[character.<name>]`: every context this character performs.
    Character(String),
    /// `[root]`: every context a root character performs.
    Root,
}

impl Section {
    /// The section as the file spells it.
    pub(crate) fn header(&self) -> String {
        match self {
            Self::Global => "[global]".into(),
            Self::Rc(dir) => format!("[rc.{dir}]"),
            Self::Character(name) => format!("[character.{name}]"),
            Self::Root => "[root]".into(),
        }
    }
}

impl std::fmt::Display for Section {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Global => f.write_str("global config"),
            Self::Rc(dir) => write!(f, "rc config ({dir})"),
            Self::Character(name) => write!(f, "character config ({name})"),
            Self::Root => f.write_str("root config"),
        }
    }
}

/// What the gate's layers key on: the calling context's performer, read
/// once per decision from the context row and the character sheet
/// ([`performer_of`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GatePerformer {
    /// The character's name: `[character.<name>]`.
    pub name: String,
    /// The character's rc directory: `[rc.<dir>]`. `None` reads no
    /// directory layer.
    pub rc_dir: Option<String>,
    /// The sheet's root flag: `[root]`.
    pub root: bool,
}

impl GatePerformer {
    /// The sections this performer reads beyond `[global]`, earliest
    /// first: each later one reads over the ones before it, so the
    /// narrowest, the character, decides.
    pub(crate) fn sections(&self) -> Vec<Section> {
        let mut out = Vec::with_capacity(3);
        if let Some(dir) = &self.rc_dir {
            out.push(Section::Rc(dir.clone()));
        }
        if self.root {
            out.push(Section::Root);
        }
        out.push(Section::Character(self.name.clone()));
        out
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

    /// Turn every `Uncovered` statement into the council's allow, keyed by
    /// `key(index)`. An `Ask` stays firm and a `Deny` stays a deny, so a
    /// submission holding either is decided as before.
    pub(crate) fn apply_council_allow(&mut self, spec: &str, key: impl Fn(usize) -> String) {
        for (i, verdict) in self.per_statement.iter_mut().enumerate() {
            if matches!(verdict, PolicyVerdict::Uncovered) {
                *verdict = PolicyVerdict::Allow(vec![Decision { layer: Layer::Council(spec.to_string()), key: key(i) }]);
            }
        }
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

/// One tier table: one [`Section`], keys normalized to canonical names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TierTable {
    /// `(normalized key, verdict)`; a key may appear under more than one
    /// verdict, and the higher rank wins.
    entries: Vec<(String, TierVerdict)>,
    /// This section's `uncovered` setting, when it states one.
    uncovered: Option<UncoveredTier>,
    /// This section's council block, when it has one. `[global]` has none.
    council: Option<CouncilBlock>,
}

/// One section's council block, `[<section>.council]`: the keys it states.
/// A later section's stated key reads over an earlier one's
/// ([`GateConfig::council_for`]).
#[derive(Clone, Debug, PartialEq, Eq)]
struct CouncilBlock {
    enabled: bool,
    contexts: Option<Vec<String>>,
    judge: Option<String>,
}

impl TierTable {
    /// The entries for a command's candidate keys, given most specific
    /// first: for each key the table names, its strongest verdict (deny over
    /// ask over allow). The caller takes the first, so a more specific key
    /// wins, and reads on past an allow it cannot use, so a less specific
    /// key's deny or ask in the same table still fires.
    fn lookup<'a>(&'a self, candidates: &'a [String]) -> impl Iterator<Item = (String, TierVerdict)> + 'a {
        candidates.iter().filter_map(|key| {
            self.entries
                .iter()
                .filter(|(k, _)| k == key)
                .map(|(_, v)| *v)
                .max_by_key(|v| v.rank())
                .map(|v| (key.clone(), v))
        })
    }
}

/// The parsed `gate.toml`: the global tier, one tier per rc directory, per
/// character and for roots, and the council's settings when the file
/// declares them.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GateConfig {
    global: TierTable,
    rc: BTreeMap<String, TierTable>,
    character: BTreeMap<String, TierTable>,
    root: Option<TierTable>,
    council: Option<CouncilConfig>,
    /// The `[rc.<dir>]` and `[character.<name>]` sections that name no rc
    /// directory or character, found by [`load_config`].
    nobody: Vec<Section>,
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

/// How the council's answer acts (`docs/council.md`, "Bumper mode").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CouncilMode {
    /// Allow runs the submission; anything else opens an ask. The default.
    Gatekeeper,
    /// Proceed runs the submission; anything else refuses it with guidance
    /// and no ask, until the bump limit.
    Bumper,
    /// Bumper with no ask but an escalation: a miss, a control-text hit,
    /// or a program the council could not read bumps too, each saying why,
    /// and there is no bump limit.
    BumpOnly,
}

/// The kind of case a council spec reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CouncilCase {
    Shell,
    /// The program text a submission runs (`docs/council.md`, "Programs
    /// are cases of their own").
    Program,
}

/// A spec the council holds: `name` is the stem of
/// `/config/kernel/council/<name>.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CouncilSpec {
    pub(crate) name: String,
    pub(crate) case: CouncilCase,
    /// Council contexts this spec's decisions read after `[council]
    /// contexts`, such as a code context for program decisions. Empty by
    /// default.
    pub(crate) contexts: Vec<String>,
    /// This spec's own `require_agree`, overriding `[council]
    /// require_agree` when set.
    pub(crate) require_agree: Option<bool>,
}

/// The cut a spec's pooled answer must clear. The council server's identity
/// is recorded with each decision but does not select a threshold.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CouncilThreshold {
    pub(crate) spec: String,
    /// Pooled p(allow) at or above which the council may allow.
    pub(crate) allow_at: f64,
    /// Each read's verdict mass, a log probability, at or above which the
    /// read counts.
    pub(crate) mass_floor: f64,
}

/// The `[council]` section with its specs and thresholds.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CouncilConfig {
    /// The council server decisions go to: the address of the backend of a
    /// context's `gate` slot, or of a judge shadow's `model` slot for a judge
    /// read. gate.toml names none, so a config
    /// it loads holds an empty address until the gate routes it to a slot
    /// ([`crate::council::gate::seat_route`]); a config never reaches a
    /// server unrouted.
    pub(crate) server: String,
    /// The served model the server's decisions run on, by its `model` or an
    /// alias: the slot's model. Empty-address configs hold `None`.
    pub(crate) model: Option<String>,
    pub(crate) contexts: Vec<String>,
    pub(crate) pool_method: CouncilPoolMethod,
    pub(crate) pool_weights: CouncilPoolWeights,
    /// Bounds a whole decision: preparing the server and the decision call.
    pub(crate) deadline_ms: u64,
    pub(crate) require_agree: bool,
    /// `[council] reviewer_contexts`: compose `council-<character>` contexts along the
    /// reviewer chain (`docs/council.md`, "Council contexts are kaijutsu
    /// contexts"). Off by default.
    pub(crate) reviewer_contexts: bool,
    /// `[council] house_rules`: each decision also reads the house rules of
    /// the proposing seat's workspace, from its `AGENTS.md` when one is
    /// found (`docs/council.md`, "House rules"). Off by default.
    pub(crate) house_rules: bool,
    /// `[council] house_rules_tokens`: the budget for the house rules,
    /// estimated at four bytes a token.
    pub(crate) house_rules_tokens: u64,
    /// `[council] mode`: gatekeeper (default) or bumper.
    pub(crate) mode: CouncilMode,
    /// `[council] bump_limit`: in bumper mode, the bumps one submission gets
    /// before the next would-be bump is an ordinary ask.
    pub(crate) bump_limit: Option<u64>,
    /// `[council] escalate`: in a bumper mode, the would-be bump that makes
    /// `bumps` in a row for one seat within `minutes` is an ordinary ask to
    /// the seat's reviewer. Off when unset.
    pub(crate) escalate: Option<CouncilEscalate>,
    pub(crate) specs: Vec<CouncilSpec>,
    pub(crate) thresholds: Vec<CouncilThreshold>,
    /// `judge = "<character>"` in a council block of the seat's performer:
    /// the character that performs each seat's judge shadow
    /// (`docs/council.md`, "Shadow context"). `None` runs the council with
    /// no judge.
    pub(crate) judge: Option<String>,
}

/// `[council] escalate = { bumps = N, minutes = M }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CouncilEscalate {
    pub(crate) bumps: u64,
    pub(crate) minutes: u64,
}

impl CouncilConfig {
    /// Whether a decision on the spec named `spec_name` needs every read to
    /// agree: the spec's own `require_agree`, else `[council]`'s.
    pub(crate) fn require_agree_for(&self, spec_name: &str) -> bool {
        self.specs.iter().find(|s| s.name == spec_name).and_then(|s| s.require_agree).unwrap_or(self.require_agree)
    }

    /// The threshold for `spec`, or `None` when gate.toml declares none.
    pub(crate) fn threshold_for(&self, spec: &str) -> Option<&CouncilThreshold> {
        self.thresholds.iter().find(|t| t.spec == spec)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CouncilToml {
    contexts: Vec<String>,
    pool: CouncilPoolToml,
    deadline_ms: i64,
    #[serde(default = "default_require_agree")]
    require_agree: bool,
    #[serde(default)]
    reviewer_contexts: bool,
    #[serde(default)]
    house_rules: bool,
    #[serde(default = "default_house_rules_tokens")]
    house_rules_tokens: i64,
    /// Keys the seat context used. Parsed only to refuse them with the new
    /// names.
    #[serde(default)]
    seat: Option<toml::Value>,
    #[serde(default)]
    seat_tokens: Option<toml::Value>,
    /// The key `reviewer_contexts` replaced. Parsed only to refuse it with
    /// the new name.
    #[serde(default)]
    voices: Option<toml::Value>,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    bump_limit: Option<i64>,
    #[serde(default)]
    escalate: Option<CouncilEscalateToml>,
    #[serde(default)]
    spec: Vec<CouncilSpecToml>,
    #[serde(default)]
    threshold: Vec<CouncilThresholdToml>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CouncilEscalateToml {
    bumps: i64,
    minutes: i64,
}

fn default_require_agree() -> bool {
    true
}

/// The house-rules context's default budget, in tokens.
pub(crate) const DEFAULT_HOUSE_RULES_TOKENS: u64 = 2000;

/// The bumps one submission gets by default before it asks.
pub(crate) const DEFAULT_BUMP_LIMIT: u64 = 3;


fn default_mode() -> String {
    "gatekeeper".into()
}

fn default_house_rules_tokens() -> i64 {
    DEFAULT_HOUSE_RULES_TOKENS as i64
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
    #[serde(default)]
    contexts: Vec<String>,
    #[serde(default)]
    require_agree: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CouncilThresholdToml {
    spec: String,
    allow_at: f64,
    mass_floor: f64,
    // Read only to refuse with a message that says to delete them.
    #[serde(default)]
    weight_hash: Option<toml::Value>,
    #[serde(default)]
    engine: Option<toml::Value>,
    #[serde(default)]
    tokenizer_hash: Option<toml::Value>,
    #[serde(default)]
    template: Option<toml::Value>,
}

/// `[rc.<dir>.council]`, `[character.<name>.council]` or `[root.council]`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CouncilSwitchToml {
    enabled: bool,
    #[serde(default)]
    contexts: Option<Vec<String>>,
    /// The character that performs a seat's judge shadow. None leaves the
    /// judge an earlier section named, or none.
    #[serde(default)]
    judge: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct GateToml {
    #[serde(default)]
    global: TierToml,
    #[serde(default)]
    rc: BTreeMap<String, TierToml>,
    #[serde(default)]
    character: BTreeMap<String, TierToml>,
    #[serde(default)]
    root: Option<TierToml>,
    /// The sections the kernel no longer reads. Parsed only to refuse them
    /// with their replacements.
    #[serde(default)]
    context_type: Option<toml::Value>,
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
    /// Accepted under every section but `[global]`, which is refused.
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
    /// The character sheet could not be listed, so the sections that name
    /// no character cannot be found. The file may be fine.
    Sheet(String),
}

impl std::fmt::Display for GateConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (what, detail) = match self {
            Self::Read(e) => ("could not be read", e),
            Self::Parse(e) => ("is not a valid gate policy file", e),
            Self::Sheet(e) => {
                return write!(
                    f,
                    "the character sheet could not be read: {e} — every gated shell submission is \
                     refused until the kernel database reads again; /config/kernel/{GATE_CONFIG_FILE} \
                     was not at fault"
                );
            }
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
    /// named. A `[context_type.<type>]` section fails naming its
    /// replacements.
    pub(crate) fn parse(text: &str) -> Result<Self, GateConfigError> {
        let raw: GateToml =
            toml::from_str(text).map_err(|e| GateConfigError::Parse(e.to_string()))?;
        if let Some(value) = &raw.context_type {
            let name = value.as_table().and_then(|t| t.keys().next());
            return Err(GateConfigError::Parse(match name {
                Some(name) => format!(
                    "[context_type.{name}]: the kernel no longer reads context_type sections; write \
                     [rc.{name}] for every character whose rc directory is {name}, or \
                     [character.<name>] for one character"
                ),
                None => "[context_type]: the kernel no longer reads context_type sections; write \
                         [rc.<dir>] for every character whose rc directory is <dir>, or \
                         [character.<name>] for one character"
                    .to_string(),
            }));
        }
        if raw.global.council.is_some() {
            return Err(GateConfigError::Parse(
                "[global.council]: the council has no global switch; enable it per rc directory \
                 with [rc.<dir>.council] or per character with [character.<name>.council]"
                    .into(),
            ));
        }
        let council = raw.council.as_ref().map(Self::council_from).transpose()?;
        let table = |tier: &TierToml, section: Section| Self::table_from(tier, &section, council.as_ref());
        let mut config = GateConfig {
            global: table(&raw.global, Section::Global)?,
            rc: BTreeMap::new(),
            character: BTreeMap::new(),
            root: raw.root.as_ref().map(|tier| table(tier, Section::Root)).transpose()?,
            council: None,
            nobody: Vec::new(),
        };
        for (dir, tier) in &raw.rc {
            config.rc.insert(dir.clone(), table(tier, Section::Rc(dir.clone()))?);
        }
        for (name, tier) in &raw.character {
            config.character.insert(name.clone(), table(tier, Section::Character(name.clone()))?);
        }
        config.council = council;
        Ok(config)
    }

    /// One section's council block, checked against the file's `[council]`.
    fn council_block(
        switch: &CouncilSwitchToml,
        section: &Section,
        council: Option<&CouncilConfig>,
    ) -> Result<CouncilBlock, GateConfigError> {
        let header = section.header();
        let section = format!("{}.council]", header.trim_end_matches(']'));
        let Some(council) = council else {
            if switch.enabled || switch.contexts.is_some() || switch.judge.is_some() {
                return Err(GateConfigError::Parse(format!(
                    "{section}: the file has no [council] section to enable or override"
                )));
            }
            return Ok(CouncilBlock { enabled: false, contexts: None, judge: None });
        };
        if let Some(contexts) = &switch.contexts {
            if !switch.enabled {
                return Err(GateConfigError::Parse(format!(
                    "{section} contexts: the council is enabled = false, so the list would do nothing; remove it or enable the council"
                )));
            }
            if contexts.is_empty() {
                return Err(GateConfigError::Parse(format!("{section} contexts: at least one label is required")));
            }
            for (i, label) in contexts.iter().enumerate() {
                let why = if label.trim().is_empty() {
                    Some("labels must be non-empty".to_string())
                } else if contexts[..i].contains(label) {
                    Some(format!("`{label}` is listed twice"))
                } else if let Some(spec) = council.specs.iter().find(|s| s.contexts.contains(label)) {
                    Some(format!("`{label}` is already in spec {} contexts", spec.name))
                } else {
                    None
                };
                if let Some(why) = why {
                    return Err(GateConfigError::Parse(format!("{section} contexts: {why}")));
                }
            }
        }
        if let Some(judge) = &switch.judge {
            if !switch.enabled {
                return Err(GateConfigError::Parse(format!(
                    "{section} judge: the council is enabled = false, so the judge would do nothing; remove it or enable the council"
                )));
            }
            if judge.trim().is_empty() {
                return Err(GateConfigError::Parse(format!("{section} judge: name a character")));
            }
        }
        Ok(CouncilBlock { enabled: switch.enabled, contexts: switch.contexts.clone(), judge: switch.judge.clone() })
    }

    fn council_from(raw: &CouncilToml) -> Result<CouncilConfig, GateConfigError> {
        let err = |m: String| GateConfigError::Parse(m);
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
        if raw.seat.is_some() {
            return Err(err("[council] seat: the seat context is gone; the key is now `house_rules`".into()));
        }
        if raw.seat_tokens.is_some() {
            return Err(err("[council] seat_tokens: the seat context is gone; the key is now `house_rules_tokens`".into()));
        }
        if raw.voices.is_some() {
            return Err(err("[council] voices: the key is now `reviewer_contexts`".into()));
        }
        if raw.house_rules_tokens <= 0 {
            return Err(err(format!(
                "[council] house_rules_tokens: {} must be an integer greater than 0",
                raw.house_rules_tokens
            )));
        }
        let mode = match raw.mode.as_str() {
            "gatekeeper" => CouncilMode::Gatekeeper,
            "bumper" => CouncilMode::Bumper,
            "bump-only" => CouncilMode::BumpOnly,
            other => {
                return Err(err(format!(
                    "[council] mode: `{other}` is not one of gatekeeper, bumper, bump-only"
                )))
            }
        };
        let bump_limit = match (mode, raw.bump_limit) {
            (CouncilMode::BumpOnly, Some(_)) => {
                return Err(err(
                    "[council] bump_limit: bump-only mode has no bump limit; remove the key".into(),
                ))
            }
            (CouncilMode::BumpOnly, None) => None,
            (_, Some(limit)) if limit <= 0 => {
                return Err(err(format!("[council] bump_limit: {limit} must be an integer greater than 0")))
            }
            (_, Some(limit)) => Some(limit as u64),
            (_, None) => Some(DEFAULT_BUMP_LIMIT),
        };
        let escalate = match (mode, &raw.escalate) {
            (_, None) => None,
            (CouncilMode::Gatekeeper, Some(_)) => {
                return Err(err(
                    "[council] escalate: gatekeeper mode asks already; escalate needs mode = \"bumper\" or \"bump-only\"".into(),
                ))
            }
            (_, Some(e)) if !(1..=1000).contains(&e.bumps) || !(1..=10080).contains(&e.minutes) => {
                return Err(err(format!(
                    "[council] escalate: bumps ({}) must be 1 to 1000 and minutes ({}) 1 to 10080",
                    e.bumps, e.minutes
                )))
            }
            (_, Some(e)) => Some(CouncilEscalate { bumps: e.bumps as u64, minutes: e.minutes as u64 }),
        };
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
                "program" => CouncilCase::Program,
                other => {
                    return Err(err(format!(
                        "[[council.spec]] {} case: `{other}` is not one of shell, program",
                        s.name
                    )))
                }
            };
            if specs.iter().any(|p| p.case == case) {
                return Err(err(format!(
                    "[[council.spec]] {}: case `{}` already has a spec; declare one spec per case",
                    s.name, s.case
                )));
            }
            for (i, label) in s.contexts.iter().enumerate() {
                if raw.contexts.contains(label) {
                    return Err(err(format!(
                        "[[council.spec]] {} contexts: `{label}` is already in [council] contexts, \
                         which every decision reads",
                        s.name
                    )));
                }
                if s.contexts[..i].contains(label) {
                    return Err(err(format!("[[council.spec]] {} contexts: `{label}` is listed twice", s.name)));
                }
            }
            specs.push(CouncilSpec { name: s.name.clone(), case, contexts: s.contexts.clone(), require_agree: s.require_agree });
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
                if value.is_some() {
                    return Err(err(format!(
                        "{at} {field}: a threshold no longer names the council server's identity; \
                         delete weight_hash, engine, tokenizer_hash and template"
                    )));
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
            if thresholds.iter().any(|p| p.spec == t.spec) {
                return Err(err(format!("{at}: a second threshold for the spec; one threshold per spec")));
            }
            thresholds.push(CouncilThreshold {
                spec: t.spec.clone(),
                allow_at: t.allow_at,
                mass_floor: t.mass_floor,
            });
        }
        Ok(CouncilConfig {
            server: String::new(),
            model: None,
            contexts: raw.contexts.clone(),
            pool_method,
            pool_weights,
            deadline_ms: raw.deadline_ms as u64,
            require_agree: raw.require_agree,
            reviewer_contexts: raw.reviewer_contexts,
            house_rules: raw.house_rules,
            house_rules_tokens: raw.house_rules_tokens as u64,
            mode,
            bump_limit,
            escalate,
            specs,
            thresholds,
            judge: None,
        })
    }

    /// The council's settings, when the file declares `[council]`.
    #[cfg(test)]
    pub(crate) fn council(&self) -> Option<&CouncilConfig> {
        self.council.as_ref()
    }

    /// Whether the file has `section`.
    pub(crate) fn has_section(&self, section: &Section) -> bool {
        self.table(section).is_some()
    }

    /// The table of one section, when the file has it.
    fn table(&self, section: &Section) -> Option<&TierTable> {
        match section {
            Section::Global => Some(&self.global),
            Section::Rc(dir) => self.rc.get(dir),
            Section::Character(name) => self.character.get(name),
            Section::Root => self.root.as_ref(),
        }
    }

    /// The tables `performer` reads beyond `[global]`, latest first: the
    /// order a lookup walks them, so a later section reads over an earlier
    /// one. A caller with no performer reads none.
    fn performer_tables(&self, performer: Option<&GatePerformer>) -> Vec<(Section, &TierTable)> {
        let Some(performer) = performer else { return Vec::new() };
        performer
            .sections()
            .into_iter()
            .rev()
            .filter_map(|section| self.table(&section).map(|table| (section, table)))
            .collect()
    }

    /// The union of the councils any performer's layers can compose: for
    /// each section whose block enables one, `[council]` with that block's
    /// keys over it. It is a deliberate superset of any one performer's
    /// composed council ([`Self::council_for`] reads blocks over one
    /// another and lets the last `enabled` decide, so a council it composes
    /// is built from one of these blocks plus earlier ones). Use it for
    /// judge discovery and seat reachability, which need every label or
    /// judge any seat can use; never as one performer's answer.
    pub(crate) fn councils(&self) -> Vec<CouncilConfig> {
        let Some(base) = &self.council else { return Vec::new() };
        let tables = self.rc.values().chain(self.character.values()).chain(self.root.as_ref());
        tables
            .filter_map(|table| table.council.as_ref())
            .filter(|block| block.enabled)
            .map(|block| block.over(base.clone()))
            .collect()
    }

    /// The enabled council for a context `performer` plays: `[council]`,
    /// then each of its sections' council blocks in order (`[rc.<dir>]`,
    /// `[root]`, `[character.<name>]`), where a block's stated key reads
    /// over the same key before it. The last block decides `enabled`. A
    /// caller with no performer, or whose sections have no council block,
    /// has none.
    pub(crate) fn council_for(&self, performer: Option<&GatePerformer>) -> Option<CouncilConfig> {
        let mut council = self.council.clone()?;
        let mut enabled = false;
        for section in performer?.sections() {
            if let Some(block) = self.table(&section).and_then(|t| t.council.as_ref()) {
                enabled = block.enabled;
                council = block.over(council);
            }
        }
        enabled.then_some(council)
    }

    fn table_from(tier: &TierToml, section: &Section, council: Option<&CouncilConfig>) -> Result<TierTable, GateConfigError> {
        let header = section.header();
        let mut entries = Vec::new();
        for (verdict, list) in [
            (TierVerdict::Allow, &tier.allow),
            (TierVerdict::Ask, &tier.ask),
            (TierVerdict::Deny, &tier.deny),
        ] {
            for raw in list {
                let key = normalize_config_key(raw).map_err(|why| {
                    GateConfigError::Parse(format!("{header} {}: `{raw}` — {why}", verdict.word()))
                })?;
                entries.push((key, verdict));
            }
        }
        let uncovered = match &tier.uncovered {
            Some(raw) => Some(UncoveredTier::parse(raw).map_err(|why| {
                GateConfigError::Parse(format!("{header} uncovered: {why}"))
            })?),
            None => None,
        };
        let council = match &tier.council {
            Some(switch) => Some(Self::council_block(switch, section, council)?),
            None => None,
        };
        Ok(TierTable { entries, uncovered, council })
    }

    /// The uncovered tier in force for a context `performer` plays, and the
    /// section that set it: the latest of its sections that states one,
    /// else `[global]`, else `ask`.
    pub(crate) fn uncovered_for(&self, performer: Option<&GatePerformer>) -> (UncoveredTier, Section) {
        for (section, table) in self.performer_tables(performer) {
            if let Some(tier) = table.uncovered {
                return (tier, section);
            }
        }
        (self.global.uncovered.unwrap_or_default(), Section::Global)
    }

    /// Find the `[rc.<dir>]` and `[character.<name>]` sections that apply
    /// to nobody: a directory not in `rc_dirs`, a character not in
    /// `characters`. An empty `rc_dirs` names every directory, the rule
    /// `KjDispatcher::check_rc_dir` states: there is nothing to validate
    /// against, and a kernel with no rc tree is a legitimate shape.
    fn find_nobody(&mut self, rc_dirs: &[String], characters: &[String]) {
        let dirs = self
            .rc
            .keys()
            .filter(|dir| !rc_dirs.is_empty() && !rc_dirs.contains(dir))
            .map(|dir| Section::Rc(dir.clone()));
        let names = self
            .character
            .keys()
            .filter(|name| !characters.contains(name))
            .map(|name| Section::Character(name.clone()));
        self.nobody = dirs.chain(names).collect();
    }

    /// The sections that name no rc directory or character, so apply to
    /// nobody; `kj ledger rules` lists them.
    pub(crate) fn applies_to_nobody(&self) -> &[Section] {
        &self.nobody
    }

    /// Whether the sandbox posture is on for a context `performer` plays —
    /// what `kj ledger rules` states plainly.
    pub(crate) fn allows_uncovered(&self, performer: Option<&GatePerformer>) -> bool {
        self.uncovered_for(performer).0 == UncoveredTier::Allow
    }

    /// The config entries in force for a context `performer` plays, for
    /// `kj ledger rules`: `(layer, key, verdict)`, in the order a lookup
    /// reads them, the latest section first and `[global]` last.
    pub(crate) fn entries_for(&self, performer: Option<&GatePerformer>) -> Vec<(Layer, String, &'static str)> {
        let mut out = Vec::new();
        // The widest entry first: it decides every key the lists below do
        // not name.
        if let (UncoveredTier::Allow, section) = self.uncovered_for(performer) {
            out.push((Layer::UncoveredAllow(section), "uncovered".to_string(), "allow"));
        }
        let tables = self.performer_tables(performer).into_iter().chain([(Section::Global, &self.global)]);
        for (section, table) in tables {
            out.extend(table.entries.iter().map(|(k, v)| (Layer::Config(section.clone()), k.clone(), v.word())));
        }
        out
    }

    /// Every key with a verdict, for inspection: `(section, key, verdict)`,
    /// the section as the file spells it.
    #[cfg(test)]
    pub(crate) fn entries(&self) -> Vec<(String, String, &'static str)> {
        let sections = [Section::Global, Section::Root]
            .into_iter()
            .chain(self.rc.keys().map(|d| Section::Rc(d.clone())))
            .chain(self.character.keys().map(|n| Section::Character(n.clone())));
        let mut out = Vec::new();
        for section in sections {
            if let Some(table) = self.table(&section) {
                out.extend(table.entries.iter().map(|(k, v)| (section.header(), k.clone(), v.word())));
            }
        }
        out
    }
}

impl CouncilBlock {
    /// `council` with this block's stated keys over it.
    fn over(&self, mut council: CouncilConfig) -> CouncilConfig {
        if let Some(contexts) = &self.contexts {
            council.contexts = contexts.clone();
        }
        if let Some(judge) = &self.judge {
            council.judge = Some(judge.clone());
        }
        council
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

/// Read and parse `/config/kernel/gate.toml`. An absent file (or no
/// `/config/kernel` mount) is the empty config; any other failure is an
/// error the caller must refuse on.
///
/// A `[rc.<dir>]` naming no directory in the rc tree, or a
/// `[character.<name>]` naming no character on the sheet, live or retired,
/// applies to nobody. It is listed by `kj ledger rules` and logged as a
/// warning when the set of such sections differs from the previous load's
/// ([`warn_nobody`]), never a failed load: a failed load refuses every
/// gated statement, including the one that would fix it. Finding them
/// needs the rc tree and the sheet, which is why it lives here and not in
/// [`GateConfig::parse`], a pure shape check over the text. An rc tree that
/// cannot be listed skips the check for `[rc.*]` with a warning; a sheet
/// that cannot be read refuses, as [`GateConfigError::Sheet`].
pub(crate) async fn load_config(vfs: &MountTable, db: &parking_lot::Mutex<KernelDb>) -> GateConfigLoad {
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
    let mut config = GateConfig::parse(&text)?;
    if config.rc.is_empty() && config.character.is_empty() {
        warn_nobody(db as *const _ as usize, &[]);
        return Ok(config);
    }
    // An empty list names every directory, the rule `find_nobody` states, so
    // a tree that cannot be listed skips the directory check.
    let rc_dirs = match super::rc::known_rc_dirs(vfs).await {
        Ok(dirs) => dirs,
        Err(e) => {
            tracing::warn!(
                target: "kaijutsu::gate",
                "the rc tree {} could not be listed ({e}); the [rc.<dir>] sections of /config/kernel/{GATE_CONFIG_FILE} \
                 are not checked against it",
                kaijutsu_types::paths::RC_ROOT
            );
            Vec::new()
        }
    };
    let characters: Vec<String> = db
        .lock()
        .list_characters(true)
        .map_err(|e| GateConfigError::Sheet(e.to_string()))?
        .into_iter()
        .map(|sheet| sheet.name)
        .collect();
    config.find_nobody(&rc_dirs, &characters);
    warn_nobody(db as *const _ as usize, config.applies_to_nobody());
    Ok(config)
}

/// Log the sections that apply to nobody, once per change: when the set
/// differs from the last one `kernel` (the address of its database, which
/// lives as long as the kernel) saw, and always on its first load. A
/// submission loads the file two or three times, so a steady state logs
/// once rather than at every load; fixing the file and breaking it again is
/// a change and logs again.
fn warn_nobody(kernel: usize, nobody: &[Section]) {
    static LAST: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<usize, Vec<Section>>>> =
        std::sync::LazyLock::new(Default::default);
    let mut last = LAST.lock();
    let mut now = nobody.to_vec();
    now.sort();
    if last.get(&kernel) == Some(&now) {
        return;
    }
    for section in &now {
        let why = match section {
            Section::Rc(dir) => format!("no rc directory at {}/{dir}", kaijutsu_types::paths::RC_ROOT),
            _ => "no character on the sheet has that name".to_string(),
        };
        tracing::warn!(
            target: "kaijutsu::gate",
            section = %section.header(),
            "/config/kernel/{GATE_CONFIG_FILE}: {} applies to nobody: {why}",
            section.header()
        );
    }
    last.insert(kernel, now);
}

// ── Layers for one evaluation ───────────────────────────────────────────

/// The config layers in force for one caller: the parsed file and the
/// calling context's performer, when it has one.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Layers<'a> {
    pub config: &'a GateConfig,
    pub performer: Option<&'a GatePerformer>,
}

impl Layers<'_> {
    /// The decision the uncovered tier contributes, or `None` when the tier
    /// is `ask` — the default, where an uncovered statement meets the
    /// pre_call hooks and the gate. `key` names what was allowed.
    fn uncovered_allow(&self, key: impl FnOnce() -> String) -> Option<Decision> {
        match self.config.uncovered_for(self.performer) {
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

/// The council a context's gate decisions read: the one its performer's
/// layers compose ([`GateConfig::council_for`]), except that the console,
/// the context a sheet's `console_ctx` names, and a person's root console,
/// the context a sheet's `root_ctx` names, are never under a council,
/// whatever their performer's layers say: the root is where the person goes
/// when the council is failing (`docs/council.md`, "Council v2 seats"). The
/// gate's consult, the ask's council flag, seat planning and the judge
/// shadow all read it. `Err` is a fault reading the console pointers.
pub(crate) fn council_for_context(
    db: &KernelDb,
    config: &GateConfig,
    context: ContextId,
    performer: Option<&GatePerformer>,
) -> Result<Option<CouncilConfig>, String> {
    let unread = |what: &str, e: crate::kernel_db::KernelDbError| {
        format!("the gate could not read whether context {} is {what}: {e} (fail-closed)", context.short())
    };
    if db.is_console(context).map_err(|e| unread("a console", e))? || db.is_root_console(context).map_err(|e| unread("a root console", e))? {
        return Ok(None);
    }
    Ok(config.council_for(performer))
}

/// The performer of a context, as the gate's layers key on it: its name,
/// rc directory and root flag off the character sheet. `Ok(None)` for a
/// caller with no context or a known context with no performer, which
/// reads `[global]` alone. A context id with no row, a performer with no
/// sheet, or a read that fails, is an error the caller refuses on.
pub(crate) fn performer_of(db: &KernelDb, context_id: Option<ContextId>) -> Result<Option<GatePerformer>, String> {
    let Some(id) = context_id else { return Ok(None) };
    let row = db
        .get_context(id)
        .map_err(|e| format!("gate policy could not read context {}: {e} (fail-closed)", id.short()))?
        .ok_or_else(|| {
            format!("gate policy: context {} has no row, so its gate layers cannot be read (fail-closed)", id.short())
        })?;
    let Some(performer) = row.performer else { return Ok(None) };
    match db.get_character(performer) {
        Ok(Some(sheet)) => Ok(Some(GatePerformer {
            name: sheet.name,
            rc_dir: sheet.rc_dir.filter(|dir| !dir.is_empty()),
            root: sheet.root,
        })),
        Ok(None) => Err(format!(
            "gate policy: context {} is performed by {}, which has no character sheet, so its gate \
             layers cannot be read (fail-closed)",
            id.short(),
            performer.short()
        )),
        Err(e) => Err(format!("gate policy could not read the performer's character sheet: {e} (fail-closed)")),
    }
}

// ── Evaluation ──────────────────────────────────────────────────────────

/// Layers 2–4 over a planned program — what broker PreCall consults before
/// any hook runs. No ledger read.
pub(crate) fn evaluate_planned(
    statements: &[PlannedStatement],
    layers: Layers<'_>,
) -> PolicyEvaluation {
    PolicyEvaluation {
        per_statement: statements.iter().map(|s| statement_verdict(s, layers, &|_| None)).collect(),
    }
}

/// Who presents a submission, for matching learned rules of each scope:
/// a `session` rule binds the context and principal, a `character` rule
/// the actor.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RuleCaller {
    pub context_id: Option<ContextId>,
    pub principal_id: PrincipalId,
    pub actor_id: PrincipalId,
}

impl RuleCaller {
    pub(crate) fn of(caller: &super::KjCaller) -> Self {
        Self { context_id: caller.context_id, principal_id: caller.principal_id, actor_id: caller.actor_id }
    }

    /// The ledger's match keys, with the character condition this call
    /// meets. A caller with no context matches no session rule.
    fn rule_match<'a>(&'a self, context: &'a [u8], character_allows: bool) -> RuleMatch<'a> {
        RuleMatch {
            context_id: Some(context),
            principal_id: Some(self.principal_id.as_bytes().as_slice()),
            actor_id: Some(self.actor_id.as_bytes().as_slice()),
            character_allows,
        }
    }
}

/// Every layer, for one gate ask: exact-statement rules over the ask's
/// statement digests, then family rules over the planned program's
/// command keys, then layers 2–4 over the same program. Fails only when
/// the ledger cannot be read.
pub(crate) fn evaluate(
    db: &KernelDb,
    spec: &GateSpec,
    caller: RuleCaller,
    layers: Layers<'_>,
) -> approval_ledger::Result<PolicyEvaluation> {
    let conn = db.conn_for_ledger();
    let context = caller.context_id.map(|c| c.as_bytes().to_vec()).unwrap_or_default();
    let groups = planned_groups(spec);
    let mut exact = Vec::with_capacity(spec.statements.len());
    for (i, statement) in spec.statements.iter().enumerate() {
        // An exact character rule meets the same condition as a family
        // one: every command of the statement it covers must serve.
        let serves = groups.as_ref().is_some_and(|groups| {
            groups[i].iter().flat_map(|s| s.plan.commands.iter()).all(|cmd| serves_character(db, cmd, &caller))
        });
        exact.push(approval_ledger::rules::redeem_statement(
            conn,
            &statement_digest(spec.origin, &statement.rendered),
            &spec.authorized_label,
            &caller.rule_match(&context, serves),
        )?);
    }

    let family = family_layer_per_gated_statement(db, spec, &caller, &context)?;
    let edge = |cmd: &PlannedCommand| {
        senpai_edge_drift(db, cmd, &caller).or_else(|| mcp_session_create(db, cmd, &caller))
    };
    let lower = lower_layers_per_gated_statement(spec, layers, &edge);
    // The acts that always ask, per gated statement: decided over every
    // layer but a deny.
    let firm: Vec<Option<Decision>> = (0..spec.statements.len())
        .map(|i| {
            let group = groups.as_ref()?.get(i)?;
            group.iter().flat_map(|s| s.plan.commands.iter()).find_map(|cmd| always_asks(db, cmd, &caller))
        })
        .collect();
    let per_statement = exact
        .iter()
        .zip(family)
        .zip(lower)
        .zip(firm)
        .map(|(((rule, family), lower), firm)| match (firm, rule) {
            (Some(_), StatementVerdict::Deny(row)) => PolicyVerdict::Deny(Decision {
                layer: Layer::UserRule,
                key: rule_key(row),
            }),
            (Some(ask), _) => match (family, lower) {
                (PolicyVerdict::Deny(deny), _) | (_, PolicyVerdict::Deny(deny)) => PolicyVerdict::Deny(deny),
                _ => PolicyVerdict::Ask(ask),
            },
            (None, StatementVerdict::Allow(row)) => PolicyVerdict::Allow(vec![Decision {
                layer: Layer::UserRule,
                key: rule_key(row),
            }]),
            (None, StatementVerdict::Deny(row)) => PolicyVerdict::Deny(Decision {
                layer: Layer::UserRule,
                key: rule_key(row),
            }),
            (None, StatementVerdict::Uncovered) => match family {
                PolicyVerdict::Uncovered => lower,
                decided => decided,
            },
        })
        .collect();
    Ok(PolicyEvaluation { per_statement })
}

/// `character ` for a character rule, so an auto-decision's reason says
/// the rule is bound to its performer.
fn scope_word(scope: approval_ledger::types::RuleScope) -> &'static str {
    match scope {
        approval_ledger::types::RuleScope::Character => "character ",
        _ => "",
    }
}

fn rule_key(row: &RuleRow) -> String {
    format!("the exact statement ({}rule {})", scope_word(row.scope), row.rule_id)
}

fn family_rule_key(row: &FamilyRuleRow) -> String {
    format!("{} ({}rule {})", row.family_key, scope_word(row.scope), row.rule_id)
}

/// The planned statements each of `spec.statements` stands for, aligned
/// by index, or `None` when the spec carries no plan that aligns.
fn planned_groups(spec: &GateSpec) -> Option<Vec<&[PlannedStatement]>> {
    let n = spec.statements.len();
    match spec.origin {
        Origin::ShellGate if spec.planned.len() == n && n > 0 => {
            Some(spec.planned.iter().map(std::slice::from_ref).collect())
        }
        Origin::Hook if n == 1 && !spec.planned.is_empty() => Some(vec![spec.planned.as_slice()]),
        Origin::ShellGate | Origin::Hook | Origin::HookResult | Origin::KjVerb | Origin::Drift => None,
    }
}

/// The family-rule verdict for each of `spec.statements`, aligned by
/// index and shaped per origin like [`lower_layers_per_gated_statement`].
/// A family deny on any command denies the statement regardless of
/// structure; a family allow needs every command allowed and structurally
/// plain; anything else is `Uncovered` for the lower layers to decide.
fn family_layer_per_gated_statement(
    db: &KernelDb,
    spec: &GateSpec,
    caller: &RuleCaller,
    context: &[u8],
) -> approval_ledger::Result<Vec<PolicyVerdict>> {
    let n = spec.statements.len();
    let Some(groups) = planned_groups(spec) else {
        return Ok(vec![PolicyVerdict::Uncovered; n]);
    };
    let mut out = Vec::with_capacity(n);
    for group in groups {
        let planned: Vec<&PlannedCommand> = group.iter().flat_map(|s| s.plan.commands.iter()).collect();
        if planned.iter().all(|cmd| command_keys(cmd).is_none()) {
            out.push(PolicyVerdict::Uncovered);
            continue;
        }
        let mut decisions: Vec<Decision> = Vec::new();
        let mut all_allowed = !planned.is_empty();
        let mut deny: Option<Decision> = None;
        for cmd in planned {
            let Some(keys) = command_keys(cmd) else {
                all_allowed = false;
                continue;
            };
            // One lookup per command: the character condition is a fact
            // about this command's own target.
            let wanted: Vec<&str> = keys.candidates.iter().map(String::as_str).collect();
            let scope = caller.rule_match(context, serves_character(db, cmd, caller));
            let rows = approval_ledger::rules::family_coverage_scoped(db.conn_for_ledger(), &wanted, &scope)?;
            let hit: Option<&FamilyRuleRow> = rows.iter().find_map(Option::as_ref);
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

// ── The character condition ─────────────────────────────────────────────

/// The context a `kj` call acts on, as far as a `character` allow rule is
/// concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CharacterTarget {
    /// The call acts on this context reference; `None` is the caller's
    /// current context, which the verb resolves the same way. Covered when
    /// the actor holds that context's senpai.
    Context(Option<String>),
    /// `kj context create --senpai <ctx>`: the new context answers to this
    /// one. Covered when the actor holds it.
    NewUnder(String),
    /// `kj context create` under the caller's current context: the new
    /// context's senpai is the caller's own.
    NewChild,
    /// A verb the condition does not read, or a form of one that acts on
    /// more than one context (`--children`).
    Unread,
}

/// The verbs a `character` allow rule can cover, and the argument each
/// names its target with. The ids are the clap field names in `drive.rs`,
/// `interrupt.rs` and `context.rs`.
fn character_target(args: &[String]) -> CharacterTarget {
    use CharacterTarget::*;
    // After `--` a token such as `--json` is a word, which `strip_flag`
    // would drop: the target read here could then differ from the verb's.
    if args.iter().any(|a| a == "--") {
        return Unread;
    }
    let mut args = args.to_vec();
    super::parse::strip_flag(&mut args, &["--confirm", "--json"]);
    let Ok(matches) = super::effect::cached_kj_args_command().try_get_matches_from(&args) else {
        return Unread;
    };
    // A renamed field reads as an error here, never as "no target given".
    let value = |m: &clap::ArgMatches, id: &str| m.try_get_one::<String>(id).map(|v| v.cloned());
    let flag = |m: &clap::ArgMatches, id: &str| m.try_get_one::<bool>(id).map(|v| v.copied().unwrap_or(false));
    let target = match matches.subcommand() {
        Some(("drive", m)) => value(m, "target").map(Context),
        Some(("interrupt", m)) => value(m, "target").map(Context),
        Some(("context", m)) => match m.subcommand() {
            Some(("archive", m)) => match flag(m, "children") {
                Ok(false) => value(m, "context").map(Context),
                Ok(true) => Ok(Unread),
                Err(e) => Err(e),
            },
            Some(("create", m)) => value(m, "senpai").map(|p| p.map_or(NewChild, NewUnder)),
            _ => Ok(Unread),
        },
        _ => Ok(Unread),
    };
    target.unwrap_or(Unread)
}

/// A host command spelled like the `kj` builtin (`./kj`, `/usr/bin/kj`) or
/// the `kjc` client: either can reach a kernel verb whose target this
/// condition does not read.
fn spells_kj(cmd: &PlannedCommand) -> bool {
    let base = cmd.name.rsplit('/').next().unwrap_or(&cmd.name);
    cmd.name != "kj" && matches!(base, "kj" | "kjc")
}

/// A command other than the `kj` builtin whose literal arguments name `kj`
/// or `kjc` as a word (`env kj`, `xargs kj`, `bash -c 'kj drive x'`): a
/// wrapper that can reach a kernel verb whose target the condition does
/// not read.
fn wraps_kj(cmd: &PlannedCommand) -> bool {
    use kaish_types::plan::PlannedValue;
    cmd.name != "kj"
        && cmd.args.iter().any(|arg| match arg {
            PlannedValue::Literal { value, .. } => value
                .split(|c: char| c.is_whitespace() || ";&|()`$\"'<>".contains(c))
                .any(|word| matches!(word.rsplit('/').next().unwrap_or(word), "kj" | "kjc")),
            _ => false,
        })
}

/// Whether a `character` allow rule may cover `cmd` for `caller`'s actor.
///
/// A host command spelled `kj` or `kjc` fails. Any other command that is
/// not the `kj` builtin passes: the gate reads no context target in it. A `kj` call the builtin layer allows passes, since it only reads.
/// `kj drive`, `kj interrupt` and `kj context archive` pass only when the
/// actor holds the target's senpai, resolved as the verb resolves it;
/// `kj context create --senpai <ctx>` passes only when the actor holds that
/// context, and a bare `kj context create` passes, since its new context
/// answers to the caller's own (`docs/senpai-kohai.md`, "Decided after the
/// docs pass"). Every other `kj` call fails, as does one with an argument
/// that is not a literal word (an expansion could name another target, or a
/// flag) or whose target does not resolve.
pub(crate) fn serves_character(db: &KernelDb, cmd: &PlannedCommand, caller: &RuleCaller) -> bool {
    if spells_kj(cmd) {
        return false;
    }
    if cmd.name != "kj" || builtin_key(cmd).is_some() {
        return true;
    }
    let Some(args) = literal_args(cmd) else {
        return false;
    };
    match character_target(&args) {
        CharacterTarget::NewChild => true,
        CharacterTarget::Unread => false,
        CharacterTarget::NewUnder(senpai) => {
            super::refs::resolve_context_arg_from(Some(&senpai), caller.context_id, db)
                .ok()
                .and_then(|id| db.context_holder(id).ok().flatten())
                .is_some_and(|holder| holder == caller.actor_id)
        }
        CharacterTarget::Context(target) => {
            super::refs::resolve_context_arg_from(target.as_deref(), caller.context_id, db)
                .ok()
                .and_then(|id| db.senpai_of(id).ok().flatten())
                .and_then(|senpai| db.context_holder(senpai).ok().flatten())
                .is_some_and(|holder| holder == caller.actor_id)
        }
    }
}

/// Whether `cmd` drifts along an edge that needs no ask
/// (`docs/senpai-kohai.md`, "Reports and drift"):
///
/// - `kj drift push` from a context to its senpai, or from a context to one
///   that answers to it, with one exception: a push up from a fork (a context of
///   the same character as its senpai) whose `created_by` is not that
///   character keeps the gate. `kj drift push .senpai` is that push: the
///   target resolves through the same context references the verb reads.
///
/// The edge is the context character's `senpai_ctx`. Every argument must be a
/// literal word, and the command plain (no redirect, background flag or
/// heredoc), since the decision reads the target out of the arguments. `None`
/// for anything else; the verb itself still refuses a target that is not
/// live.
fn senpai_edge_drift(db: &KernelDb, cmd: &PlannedCommand, caller: &RuleCaller) -> Option<Decision> {
    if cmd.name != "kj" || !cmd.redirects.is_empty() || cmd.background || !cmd.heredocs.is_empty() {
        return None;
    }
    let here = caller.context_id?;
    let mut args = literal_args(cmd)?;
    // After `--` a token such as `--json` is a word, which `strip_flag`
    // would drop: the target read here could then differ from the verb's.
    if args.iter().any(|a| a == "--") {
        return None;
    }
    super::parse::strip_flag(&mut args, &["--confirm", "--json"]);
    let matches = super::effect::cached_kj_args_command().try_get_matches_from(&args).ok()?;
    let here_row = db.context_character(here).ok()??;
    let label = |id: ContextId| {
        db.get_context(id)
            .ok()
            .flatten()
            .and_then(|row| row.label)
            .unwrap_or_else(|| id.short())
    };
    let edge = |layer_key: String| Some(Decision { layer: Layer::SenpaiEdge, key: layer_key });
    match matches.subcommand()? {
        ("drift", m) => match m.subcommand()? {
            ("push", m) => {
                let dst = m.try_get_one::<String>("dst").ok()??;
                let target = super::refs::resolve_context_arg_from(Some(dst.as_str()), Some(here), db).ok()?;
                if target == here {
                    return None;
                }
                let target_row = db.get_context(target).ok()??;
                if here_row.senpai_ctx == Some(target) {
                    // A fork made by someone other than its character keeps the gate.
                    let fork = target_row.performer == Some(here_row.principal_id);
                    let own = db.get_context(here).ok()??.created_by == here_row.principal_id;
                    if fork && !own {
                        return None;
                    }
                    edge(format!("kj drift push to {}, its senpai", label(target)))
                } else if target_row.senpai_ctx == Some(here) {
                    edge(format!("kj drift push to {}, which answers to it", label(target)))
                } else {
                    None
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Every argument's word, when each is known before the command runs.
fn literal_args(cmd: &PlannedCommand) -> Option<Vec<String>> {
    use kaish_types::plan::PlannedValue;
    cmd.args
        .iter()
        .map(|arg| match arg {
            PlannedValue::Literal { value, .. } => Some(value.clone()),
            _ => None,
        })
        .collect()
}

/// Why a planned program cannot teach a `character` allow rule: a `kj`
/// command the condition does not read, a host command spelled `kj` or
/// `kjc`, or a wrapper whose arguments name either, so the rule could
/// never cover it safely. `None` when every command can
/// be covered.
pub(crate) fn character_rule_refusal(statements: &[PlannedStatement]) -> Option<String> {
    let unread = statements.iter().flat_map(|s| s.plan.commands.iter()).find(|cmd| {
        spells_kj(cmd)
            || wraps_kj(cmd)
            || cmd.name == "kj"
            && builtin_key(cmd).is_none()
            && literal_args(cmd).is_none_or(|args| character_target(&args) == CharacterTarget::Unread)
    })?;
    Some(format!(
        "`{}` is not a kj call a character rule can cover; a character allow covers kj drive, \
         kj interrupt and kj context archive (without --children) on contexts whose senpai the \
         character holds, and kj context create under a context it holds, with literal arguments, \
         and commands other than kj",
        command_clause(unread)
    ))
}

/// Layers 2–4 for each of `spec.statements`, aligned by index.
///
/// `Origin::ShellGate` builds one gated statement per planned statement,
/// so the two align by position; a spec whose `planned` is empty or of
/// another length has no plan to classify and is `Uncovered` throughout.
/// `Origin::Hook` gates one statement standing for the whole call: denied
/// or asked if any planned statement is, allowed only when every one is.
/// `Origin::KjVerb` carries no plan.
fn lower_layers_per_gated_statement(spec: &GateSpec, layers: Layers<'_>, edge: &EdgeCheck<'_>) -> Vec<PolicyVerdict> {
    let n = spec.statements.len();
    match spec.origin {
        Origin::ShellGate if spec.planned.len() == n && n > 0 => {
            spec.planned.iter().map(|s| statement_verdict(s, layers, edge)).collect()
        }
        Origin::Hook if n == 1 && !spec.planned.is_empty() => {
            let program = spec.planned.iter().map(|s| statement_verdict(s, layers, edge)).collect();
            vec![fold_verdicts(program)]
        }
        Origin::ShellGate | Origin::Hook | Origin::HookResult | Origin::KjVerb | Origin::Drift => {
            vec![PolicyVerdict::Uncovered; n]
        }
    }
}

/// The database checks [`statement_verdict`] consults for a command no key
/// decided: a drift along the senpai edge, or a context created under the
/// caller's own MCP hub. `Some` names the allow. Only [`evaluate`] reads the
/// database; [`evaluate_planned`] passes a check that finds neither.
type EdgeCheck<'a> = dyn Fn(&PlannedCommand) -> Option<Decision> + 'a;

/// One statement's verdict: the fold of its commands' verdicts. A
/// statement with no commands is not an allow. A command no key decides
/// is allowed when it drifts along the caller's senpai edge or creates a
/// context under the caller's own MCP hub, before the uncovered tier.
fn statement_verdict(statement: &PlannedStatement, layers: Layers<'_>, edge: &EdgeCheck<'_>) -> PolicyVerdict {
    let verdicts: Vec<PolicyVerdict> = statement
        .plan
        .commands
        .iter()
        .map(|cmd| match keyed_command_verdict(cmd, layers) {
            PolicyVerdict::Uncovered => match edge(cmd) {
                Some(decision) => PolicyVerdict::Allow(vec![decision]),
                None => layers.uncovered_verdict(|| command_key(cmd)),
            },
            decided => decided,
        })
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

/// One command's verdict through layers 2–4: the performer's tables, then
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
        // The performer's sections, latest first, then `[global]`: the
        // first deny, ask or usable allow for any of the command's keys
        // decides, most specific key first within a table.
        let tables = layers
            .config
            .performer_tables(layers.performer)
            .into_iter()
            .chain([(Section::Global, &layers.config.global)]);
        for (section, table) in tables {
            for (key, verdict) in table.lookup(&keys.candidates) {
                let decision = Decision { layer: Layer::Config(section.clone()), key };
                match verdict {
                    TierVerdict::Deny => return PolicyVerdict::Deny(decision),
                    TierVerdict::Ask => return PolicyVerdict::Ask(decision),
                    TierVerdict::Allow if keys.structural_ok => return PolicyVerdict::Allow(vec![decision]),
                    // An allow that cannot cover this command decides
                    // nothing, for this key or this table: the walk goes on,
                    // so a deny or ask in an earlier layer still fires and
                    // the builtin layer can still decide (`--help` on an
                    // allowed verb stays help).
                    TierVerdict::Allow => {}
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

/// Whether `cmd` is one of the acts that always ask (`docs/character.md`,
/// "Gate"), and the ask it raises: `kj context archive` whose target is a
/// console (a context a sheet's `console_ctx` names), or with `--children`
/// whose children include one; `kj context demote` whose next step archives
/// a console; `kj character set <name> --no-root`; and `kj character retire
/// <name>` when `<name>` is a person or performs a live console.
/// [`evaluate`] decides the ask over every layer but a deny, so an allow key,
/// a learned rule, the uncovered tier and the council do not skip it. The
/// target is resolved as the verb resolves it. Each verb with an argument
/// that is not a literal word, or with `--`, asks, and so does a read that
/// fails: the gate cannot tell that it is not one of the acts.
///
/// `None` for anything else: a target that does not resolve, which the verb
/// refuses, and a caller the verb refuses whatever the answer, so no ask
/// offers a reviewer a command that cannot run. That caller is one who is
/// not a live person, for `--no-root` and a person's retirement; one
/// [`KernelDb::console_archive_refusal`] refuses, for a console's archive or
/// demote; and one who is not the console's live person, for the retirement
/// of a character that performs any live person's console
/// (`retire_refusal` in `kj/character.rs`). `kj context archive --children`
/// archives the children in order and stops at the first it may not archive,
/// so its ask names the consoles before the first console the caller may
/// not archive, and there is none when no console comes before it.
fn always_asks(db: &KernelDb, cmd: &PlannedCommand, caller: &RuleCaller) -> Option<Decision> {
    use kaish_types::plan::PlannedValue;
    if cmd.name != "kj" {
        return None;
    }
    let ask = |key: String| Some(Decision { layer: Layer::Builtin, key });
    let literal: Vec<String> = cmd
        .args
        .iter()
        .map_while(|arg| match arg {
            PlannedValue::Literal { value, .. } => Some(value.clone()),
            _ => None,
        })
        .collect();
    let readable = literal.len() == cmd.args.len() && !literal.iter().any(|a| a == "--");
    let mut head = literal;
    super::parse::strip_flag(&mut head, &["--confirm", "--json"]);
    let root = super::kj_command();
    let verb = match head.first() {
        Some(word) => root.find_subcommand(word)?,
        None if readable => return None,
        None => return ask("kj with a verb the gate cannot read, which may archive a console".to_string()),
    };
    let sub = match head.get(1) {
        Some(word) => verb.find_subcommand(word)?,
        None if readable => return None,
        None => return ask(format!("kj {} with a subcommand the gate cannot read", verb.get_name())),
    };
    let act = match (verb.get_name(), sub.get_name()) {
        ("context", "archive") => "kj context archive",
        ("context", "demote") => "kj context demote",
        ("character", "set") => "kj character set",
        ("character", "retire") => "kj character retire",
        _ => return None,
    };
    if !readable {
        return ask(format!("{act} with an argument the gate cannot read, which may name an act that always asks"));
    }
    let matches = super::effect::cached_kj_args_command().try_get_matches_from(&head).ok()?;
    let unread = |what: &str| ask(format!("{act}, whose {what} the gate could not read"));
    // The verb refuses `--no-root` and a person's retirement to a caller who
    // is not a live person, and a console's archive to a caller
    // `console_archive_refusal` refuses. A read that fails still asks.
    let not_a_person = || matches!(db.is_live_person(caller.actor_id), Ok(false));
    let refused_console = |ctx: ContextId| matches!(db.console_archive_refusal(ctx, caller.actor_id), Ok(Some(_)));
    let (_, m) = matches.subcommand()?;
    let (_, m) = m.subcommand()?;
    match act {
        "kj character set" => {
            let name = m.try_get_one::<String>("name").ok().flatten().cloned().unwrap_or_default();
            return match m.try_get_one::<bool>("no_root") {
                Ok(Some(true)) if not_a_person() => None,
                Ok(Some(true)) => ask(format!("kj character set {name} --no-root, which always asks")),
                Ok(_) => None,
                Err(_) => unread("--no-root flag"),
            };
        }
        "kj character retire" => {
            let Ok(Some(name)) = m.try_get_one::<String>("name") else { return unread("name") };
            let sheet = match db.get_character_by_name(name) {
                Ok(Some(sheet)) if sheet.retired_at.is_none() => sheet,
                Ok(_) => return None,
                Err(_) => return unread("character sheet"),
            };
            if sheet.root {
                return if not_a_person() { None } else { ask(format!("kj character retire {name}, a person, which always asks")) };
            }
            let Ok(live) = db.contexts_played_by(sheet.principal_id) else { return unread("contexts") };
            let mut consoles = Vec::new();
            for ctx in live {
                let owner = match db.console_owner(ctx.context_id) {
                    Ok(Some(owner)) => owner,
                    Ok(None) => continue,
                    Err(_) => return unread("console pointers"),
                };
                // Only the console's live person retires its performer, and
                // the verb refuses before anything runs.
                match db.is_live_person(owner) {
                    Ok(true) if owner != caller.actor_id => return None,
                    Ok(_) => consoles.push(db.context_name(ctx.context_id)),
                    Err(_) => return unread("console owners"),
                }
            }
            return match consoles.as_slice() {
                [] => None,
                [one] => ask(format!("kj character retire {name}, who performs the console {one}, which always asks")),
                many => ask(format!(
                    "kj character retire {name}, who performs the consoles {}, which always asks",
                    many.join(", ")
                )),
            };
        }
        "kj context demote" => {
            let Ok(Some(target)) = m.try_get_one::<String>("context") else { return unread("target") };
            let Ok(target) = super::refs::resolve_context_arg_from(Some(target), caller.context_id, db) else {
                return None;
            };
            // The ladder's next step archives a context already demoted
            // (`context_demote` in `kj/context.rs`).
            let archiving = match db.get_context(target) {
                Ok(row) => row.is_some_and(|row| row.demoted_at.is_some() && !row.is_archived()),
                Err(_) => return unread("place on the ladder"),
            };
            if !archiving {
                return None;
            }
            return match db.is_console(target) {
                Ok(true) if refused_console(target) => None,
                Ok(true) => ask(format!(
                    "kj context demote of {}, whose next step archives a console, which always asks",
                    db.context_name(target)
                )),
                Ok(false) => None,
                Err(_) => unread("console pointer"),
            };
        }
        _ => {}
    }
    let (Ok(target), Ok(children)) = (m.try_get_one::<String>("context"), m.try_get_one::<bool>("children")) else {
        return unread("target");
    };
    let Ok(target) = super::refs::resolve_context_arg_from(target.map(String::as_str), caller.context_id, db) else {
        return None;
    };
    if !children.copied().unwrap_or(false) {
        return match db.is_console(target) {
            Ok(true) if refused_console(target) => None,
            Ok(true) => ask(format!("kj context archive of {}, a console, which always asks", db.context_name(target))),
            Ok(false) => None,
            Err(_) => unread("console pointer"),
        };
    }
    let performer = match m.try_get_one::<String>("performer") {
        Ok(None) => None,
        Ok(Some(name)) => match db.get_character_by_name(name) {
            Ok(Some(sheet)) => Some(sheet.principal_id),
            Ok(None) => return None,
            Err(_) => return unread("--performer"),
        },
        Err(_) => return unread("--performer"),
    };
    // The verb archives the children in this order and stops at the first
    // it may not archive (`context_archive_children` in `kj/context.rs`).
    let Ok(children) = db.fork_children(target) else { return unread("children") };
    let mut consoles = Vec::new();
    for child in children.into_iter().filter(|c| performer.is_none_or(|p| c.performer == Some(p))) {
        match db.is_console(child.context_id) {
            Ok(true) if refused_console(child.context_id) => break,
            Ok(true) => consoles.push(db.context_name(child.context_id)),
            Ok(false) => {}
            Err(_) => return unread("children's console pointers"),
        }
    }
    match consoles.as_slice() {
        [] => None,
        [one] => ask(format!(
            "kj context archive --children of {}, whose child {one} is a console, which always asks",
            db.context_name(target)
        )),
        many => ask(format!(
            "kj context archive --children of {}, whose children {} are consoles, which always asks",
            db.context_name(target),
            many.join(", ")
        )),
    }
}

/// Whether `cmd` creates a context under the caller's own MCP hub, which
/// needs no ask (`docs/senpai-kohai.md`, "MCP sessions"): `kj context create
/// <character> --parent <hub>`, where `--parent` resolves, as the verb
/// resolves it, to the `mcp_hub_ctx` of the caller's lineage root, and the
/// senpai the create sets (`--senpai`, else the caller's own context) is
/// that root's console, `root_ctx`: an MCP session answers to the person's
/// root console. A context whose parent is an MCP hub is an MCP session, and
/// creating one runs no turn. Every argument must be a literal word and the command plain (no
/// redirect, background flag or heredoc), since the decision reads the
/// parent out of the arguments. `None` for anything else, including a
/// caller whose lineage has no live root or whose root has no hub.
fn mcp_session_create(db: &KernelDb, cmd: &PlannedCommand, caller: &RuleCaller) -> Option<Decision> {
    if cmd.name != "kj" || !cmd.redirects.is_empty() || cmd.background || !cmd.heredocs.is_empty() {
        return None;
    }
    let here = caller.context_id?;
    let mut args = literal_args(cmd)?;
    // After `--` a token such as `--json` is a word, which `strip_flag`
    // would drop: the parent read here could then differ from the verb's.
    if args.iter().any(|a| a == "--") {
        return None;
    }
    super::parse::strip_flag(&mut args, &["--confirm", "--json"]);
    let matches = super::effect::cached_kj_args_command().try_get_matches_from(&args).ok()?;
    let ("context", context) = matches.subcommand()? else { return None };
    let ("create", create) = context.subcommand()? else { return None };
    let parent = create.try_get_one::<String>("parent").ok()??;
    let parent = super::refs::resolve_context_arg_from(Some(parent.as_str()), Some(here), db).ok()?;
    let root = db.live_lineage_root(here).ok()??;
    let sheet = db.get_character(root).ok()??;
    let hub = sheet.mcp_hub_ctx?;
    if parent != hub {
        return None;
    }
    // The session answers to the person's root console. The senpai the verb
    // will set is `--senpai` resolved as the verb resolves it (an empty
    // word is an error there, so it covers nothing here), or the caller's
    // own context when none is named. Any other senpai takes the ordinary
    // layers.
    let senpai = match create.try_get_one::<String>("senpai").ok()? {
        Some(word) if word.is_empty() => return None,
        Some(word) => super::refs::resolve_context_ref_from(&super::refs::parse_context_ref(word), Some(here), db).ok()?,
        None => here,
    };
    if Some(senpai) != sheet.root_ctx {
        return None;
    }
    let label = db.get_context(hub).ok()??.label.unwrap_or_else(|| hub.short());
    Some(Decision {
        layer: Layer::Builtin,
        key: format!("kj context create under {label}, the MCP hub of its lineage root"),
    })
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

    /// No file and no performer: the builtin layer alone.
    fn unconfigured(source: &str) -> PolicyEvaluation {
        evaluate_planned(&plan(source), Layers { config: &GateConfig::default(), performer: None })
    }

    fn config(text: &str) -> GateConfig {
        GateConfig::parse(text).unwrap_or_else(|e| panic!("test config must parse: {e}"))
    }

    /// The character named `name`, whose rc directory has the same name, as
    /// the shipped characters are made.
    fn dir(name: &str) -> GatePerformer {
        GatePerformer { name: name.to_string(), rc_dir: Some(name.to_string()), root: false }
    }

    /// The first statement's verdict for a context played by [`dir`]`(name)`,
    /// or by no performer.
    fn first_verdict(source: &str, cfg: &GateConfig, played_by: Option<&str>) -> PolicyVerdict {
        let performer = played_by.map(dir);
        first_verdict_for(source, cfg, performer.as_ref())
    }

    fn first_verdict_for(source: &str, cfg: &GateConfig, performer: Option<&GatePerformer>) -> PolicyVerdict {
        let layers = Layers { config: cfg, performer };
        evaluate_planned(&plan(source), layers).per_statement.remove(0)
    }

    /// The verbs a character allow can cover each name their target by a
    /// clap field id. A renamed field must read as `Unread`, never as the
    /// current context, so this pins every id the table reads.
    #[test]
    fn character_target_reads_each_covered_verb_by_its_field() {
        use CharacterTarget::*;
        let target = |line: &str| {
            let args: Vec<String> = line.split_whitespace().map(String::from).collect();
            character_target(&args)
        };
        assert_eq!(target("drive lane --prompt go"), Context(Some("lane".into())));
        assert_eq!(target("drive"), Context(None));
        assert_eq!(target("interrupt lane --immediate"), Context(Some("lane".into())));
        assert_eq!(target("context archive lane"), Context(Some("lane".into())));
        assert_eq!(target("--confirm context archive"), Context(None));
        assert_eq!(target("context archive --children lane"), Unread);
        assert_eq!(target("context create coder --name child"), NewChild);
        assert_eq!(target("context create coder --name child --parent lane"), NewChild, "--parent is provenance only");
        assert_eq!(target("context create coder --name child --senpai lane"), NewUnder("lane".into()));
        assert_eq!(target("context create"), Unread, "the character is required, so a bare create does not parse");
        assert_eq!(target("context create coder"), NewChild, "the character alone parses");
        assert_eq!(target("context set lane --model m"), Unread);
        assert_eq!(target("fork"), Unread);
        assert_eq!(target("drive --no-such-flag"), Unread);
        assert_eq!(target("ctx archive lane"), Context(Some("lane".into())), "the alias reads as its canonical verb");
        assert_eq!(target("drive -- --json"), Unread, "after `--` the gate cannot strip a flag safely");
    }

    /// A program teaches no character allow when a kj target is not a
    /// literal word, or when a host command is spelled `kj` or `kjc`.
    #[test]
    fn a_character_allow_is_refused_where_the_target_cannot_be_read() {
        let refusal = |source: &str| character_rule_refusal(&plan(source));
        assert_eq!(refusal("kj drive lane --prompt go && git log"), None);
        assert_eq!(refusal("kj context create coder --name child"), None);
        assert!(refusal("for x in lane-a; do kj drive $x; done").is_some(), "a loop variable is not a literal target");
        assert!(refusal("kj drive ${LANE}").is_some());
        assert!(refusal("./kj drive amy-lane").is_some());
        assert!(refusal("kjc -c verify kj context archive amy-lane --confirm").is_some());
        assert!(refusal("kj context set lane --model m").is_some());
    }

    /// A wrapper whose arguments name `kj` or `kjc` teaches no character
    /// allow: its family key (`env kj`, `bash`) would later cover the
    /// wrapper with any target, and the condition reads no target in it.
    #[test]
    fn a_character_allow_is_refused_for_a_wrapper_around_kj() {
        let refusal = |source: &str| character_rule_refusal(&plan(source));
        for source in [
            "env kj drive amy-lane",
            "xargs kj drive",
            "timeout 5 kj drive amy-lane",
            "bash -c 'kj drive amy-lane'",
            "sh -c \"kjc -c verify kj drive amy-lane\"",
            "nohup /usr/bin/kj drive amy-lane",
        ] {
            assert!(refusal(source).is_some(), "{source}");
        }
        assert_eq!(refusal("git log --oneline"), None, "an ordinary command still teaches");
        assert_eq!(refusal("grep -r kjx src"), None, "a word that only starts with kj is not kj");
    }

    /// A context env argument (`--env KEY=VALUE`) is plain text and must
    /// not drop a tier-allowed `kj context create` to Uncovered.
    #[test]
    fn env_argument_keeps_the_rc_allow() {
        let cfg = config("[rc.mcp]\nallow = [\"kj context create\"]\n");
        let v = first_verdict(
            "kj context create banto --name banto-probe --env KJ_CHARACTER=banto --env ROTATED_FROM=console",
            &cfg,
            Some("mcp"),
        );
        assert_eq!(
            allow_keys(&v),
            vec![(Layer::Config(Section::Rc("mcp".into())), "kj context create".to_string())],
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

    /// The argv alone allows no create: the MCP session rule reads the
    /// parent against the caller's MCP hub, which needs the database
    /// ([`mcp_session_create`]; `kj/gate.rs` tests it through the gate), so
    /// the planned layers alone leave every create uncovered.
    ///
    /// Falsified by an argv-only arm in `builtin_key` keyed on `--parent`.
    #[test]
    fn the_planned_layers_allow_no_context_create_by_its_argv() {
        for source in [
            "kj context create claude --name sess",
            "kj context create claude --name sess --parent mcp/amy",
            "kj ctx create claude --name sess --cwd /tmp --cwd-optional",
        ] {
            assert_eq!(unconfigured(source).per_statement[0], PolicyVerdict::Uncovered, "{source}");
        }
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
        let cfg = config("[global]\nallow = [\"kj context create\"]\n[rc.banto]\nallow = [\"kj context create\"]\n");
        for played_by in [None, Some("banto")] {
            assert_eq!(builtin_key_of(&first_verdict("kj context create --help", &cfg, played_by)), "kj context --help",
                "{played_by:?}");
        }
        assert_eq!(first_verdict("kj context create coder --name x > /tmp/x", &cfg, None), PolicyVerdict::Uncovered,
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
        let layers = Layers { config: &none, performer: None };
        assert_eq!(evaluate_planned(&[], layers).verdict(), AskVerdict::Escalate);
    }

    /// The council's allow covers only what no layer decided: an uncovered
    /// statement becomes an allow naming the council, an ask stays firm, a
    /// deny stays a deny, and the description names the council's layer.
    #[test]
    fn a_council_allow_converts_only_uncovered_statements() {
        let cfg = config("[global]\nask = [\"git push\"]\ndeny = [\"dd\"]\n");
        let layers = Layers { config: &cfg, performer: None };
        let mut mixed = evaluate_planned(&plan("kj block list; touch x"), layers);
        mixed.apply_council_allow("shell-gate", |i| format!("statement #{i} at p=0.991"));
        assert_eq!(mixed.verdict(), AskVerdict::Allow);
        assert!(matches!(&mixed.per_statement[0], PolicyVerdict::Allow(d) if d[0].layer == Layer::Builtin));
        assert_eq!(
            mixed.describe_planned(&plan("kj block list; touch x"), true),
            "gate policy: builtin allows kj block list; council (shell-gate) allows statement #1 at p=0.991"
        );

        let mut asked = evaluate_planned(&plan("touch x; git push origin main"), layers);
        asked.apply_council_allow("shell-gate", |_| "at p=0.991".into());
        assert_eq!(asked.verdict(), AskVerdict::Escalate, "a static ask stays firm");
        assert!(matches!(asked.per_statement[1], PolicyVerdict::Ask(_)));

        let mut denied = evaluate_planned(&plan("touch x; dd if=/dev/zero"), layers);
        denied.apply_council_allow("shell-gate", |_| "at p=0.991".into());
        assert_eq!(denied.verdict(), AskVerdict::Deny, "a deny stays a deny");
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
            actor_id: None,
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

    /// kaish plans a command name only as a literal word. A substitution, a
    /// variable or a quoted word in command position does not parse, and the
    /// shell gate refuses source that does not parse, so `always_asks` never
    /// meets a command name it cannot read. If this goes red, kaish now plans
    /// a non-literal command name: before taking that kaish, make
    /// `always_asks` ask for one, since it may be `kj` reaching an act that
    /// always asks.
    #[test]
    fn a_command_name_is_always_a_literal_word() {
        for source in [
            "$(echo kj) context archive con",
            "$K context archive con",
            "${K} context archive con",
            "\"kj\" context archive con",
        ] {
            assert!(kaish_kernel::ast::plan::plan_program(source).is_err(), "{source} plans");
            assert!(crate::kj::shell_gate::build_shell_gate_spec(source).is_err(), "{source} reaches the gate");
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
                PolicyVerdict::Deny(Decision { layer: Layer::Config(Section::Global), key: "rm".into() }),
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

[rc.explorer]
deny = ["kj context create"]
"#;

    #[test]
    fn the_shipped_default_parses_and_every_kj_key_names_a_live_leaf() {
        let cfg = config(crate::config_seed::DEFAULT_GATE_CONFIG);
        assert!(
            cfg.entries()
                .iter()
                .any(|(s, k, v)| s == "[global]" && k == "kj handoff note" && *v == "allow"),
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
        let err = GateConfig::parse("[rc.coder]\ndeny = [\"kj block lisst\"]\n").unwrap_err();
        let GateConfigError::Parse(m) = err else { panic!("{err:?}") };
        assert!(m.contains("[rc.coder] deny") && m.contains("lisst"), "{m}");
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
        assert_eq!(allow_keys(&v), vec![(Layer::Config(Section::Global), "kj handoff note".to_string())]);
        let v = first_verdict("rg -n foo src", &cfg, Some("coder"));
        assert_eq!(allow_keys(&v), vec![(Layer::Config(Section::Global), "rg".to_string())]);
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
    fn the_rc_layer_outranks_global_and_a_specific_key_outranks_a_general_one() {
        let cfg = config(
            "[global]\nallow = [\"kj context\"]\ndeny = [\"kj context create\"]\n\
             [rc.explorer]\nallow = [\"kj context create\"]\n",
        );
        // Within global: the specific deny beats the general allow.
        assert!(matches!(first_verdict("kj context create coder --name x", &cfg, None), PolicyVerdict::Deny(_)));
        assert!(matches!(first_verdict("kj context list", &cfg, None), PolicyVerdict::Allow(_)));
        // The rc layer sits above global.
        let v = first_verdict("kj context create coder --name x", &cfg, Some("explorer"));
        assert_eq!(
            allow_keys(&v),
            vec![(Layer::Config(Section::Rc("explorer".into())), "kj context create".to_string())]
        );
        assert!(matches!(
            first_verdict("kj context create coder --name x", &cfg, Some("coder")),
            PolicyVerdict::Deny(_)
        ));
    }

    /// Each later layer reads over the earlier: `[global]`, then
    /// `[rc.<dir>]`, then `[root]` for a root performer, then
    /// `[character.<name>]`: the narrowest section decides. The first table
    /// with a verdict for a command's keys decides; a later section with no
    /// verdict for the key leaves the earlier one's.
    ///
    /// Falsified by walking the performer's sections earliest first, by
    /// putting `[root]` after the character, or by leaving any of the three
    /// out.
    #[test]
    fn each_later_layer_reads_over_the_earlier_ones() {
        let cfg = config(
            "[global]\nask = [\"git push\", \"rg\", \"wc\", \"cat\"]\n\
             [rc.coder]\nallow = [\"git push\", \"rg\", \"wc\"]\n\
             [root]\nallow = [\"git push\", \"cat\"]\n\
             [character.coder-opus]\ndeny = [\"git push\", \"rg\"]\n",
        );
        let coder = dir("coder");
        let opus = GatePerformer { name: "coder-opus".into(), rc_dir: Some("coder".into()), root: false };
        let rooted = GatePerformer { name: "coder-opus".into(), rc_dir: Some("coder".into()), root: true };
        let layer = |v: PolicyVerdict| match v {
            PolicyVerdict::Allow(ds) => (ds[0].layer.clone(), "allow"),
            PolicyVerdict::Ask(d) => (d.layer, "ask"),
            PolicyVerdict::Deny(d) => (d.layer, "deny"),
            PolicyVerdict::Uncovered => panic!("every command here has a key"),
        };
        let rc = Layer::Config(Section::Rc("coder".into()));
        let character = Layer::Config(Section::Character("coder-opus".into()));
        let global = Layer::Config(Section::Global);
        // The directory reads over global, for every character of it.
        assert_eq!(layer(first_verdict_for("git push", &cfg, Some(&coder))), (rc.clone(), "allow"));
        // The character reads over its directory.
        assert_eq!(layer(first_verdict_for("git push", &cfg, Some(&opus))), (character.clone(), "deny"));
        assert_eq!(layer(first_verdict_for("rg x", &cfg, Some(&opus))), (character.clone(), "deny"));
        // A key the character does not name keeps the directory's verdict,
        // and one neither names keeps global's.
        assert_eq!(layer(first_verdict_for("wc x", &cfg, Some(&opus))), (rc, "allow"));
        assert_eq!(layer(first_verdict_for("cat x", &cfg, Some(&opus))), (global.clone(), "ask"));
        // `[root]` reads over the directory for a root performer only, and
        // the character reads over `[root]`.
        assert_eq!(layer(first_verdict_for("cat x", &cfg, Some(&rooted))), (Layer::Config(Section::Root), "allow"));
        assert_eq!(layer(first_verdict_for("cat x", &cfg, Some(&opus))), (global.clone(), "ask"));
        assert_eq!(layer(first_verdict_for("git push", &cfg, Some(&rooted))), (character.clone(), "deny"));
        assert_eq!(layer(first_verdict_for("rg x", &cfg, Some(&rooted))), (character, "deny"));
        // A context with no performer reads `[global]` alone.
        assert_eq!(layer(first_verdict_for("git push", &cfg, None)), (global, "ask"));
        // A performer with no rc directory reads no directory layer.
        let undirected = GatePerformer { name: "loose".into(), rc_dir: None, root: false };
        assert_eq!(layer(first_verdict_for("wc x", &cfg, Some(&undirected))), (Layer::Config(Section::Global), "ask"));
    }

    /// An allow a redirect (or any structural condition) vetoes decides
    /// nothing for its table, so the walk goes on to the earlier tables: a
    /// deny in `[global]` still fires under a vetoed `[rc.coder]` allow,
    /// including when the rc section also sets `uncovered = "allow"`.
    ///
    /// Falsified by `break`ing the walk on a vetoed allow.
    #[test]
    fn a_vetoed_allow_leaves_an_earlier_layers_deny_standing() {
        let cfg = config(
            "[global]\ndeny = [\"rg\"]\n[rc.coder]\nallow = [\"rg\"]\nuncovered = \"allow\"\n",
        );
        // Plain, the narrower allow reads over the deny.
        assert!(matches!(first_verdict("rg foo", &cfg, Some("coder")), PolicyVerdict::Allow(_)));
        // With a redirect the allow is vetoed; the deny fires.
        let v = first_verdict("rg foo > ~/.bashrc", &cfg, Some("coder"));
        assert_eq!(
            v,
            PolicyVerdict::Deny(Decision { layer: Layer::Config(Section::Global), key: "rg".into() }),
            "got {v:?}"
        );
    }

    /// A vetoed allow also leaves an ask in an earlier layer standing, and
    /// the builtin layer still decides when nothing else does.
    #[test]
    fn a_vetoed_allow_leaves_an_earlier_ask_and_the_builtin_standing() {
        let cfg = config("[global]\nask = [\"rg\"]\n[rc.coder]\nallow = [\"rg\", \"kj context\"]\n");
        assert!(matches!(first_verdict("rg foo > out", &cfg, Some("coder")), PolicyVerdict::Ask(_)));
        // `--help` on an allowed verb stays help, with no earlier table to
        // consult.
        assert_eq!(builtin_key_of(&first_verdict("kj context create --help", &cfg, Some("coder"))), "kj context --help");
    }

    /// Inside one table the more specific key's allow, once vetoed, leaves
    /// the same table's less specific deny standing: `lookup` reports every
    /// candidate key's verdict, most specific first, so the walk can pass a
    /// vetoed allow to the next key.
    ///
    /// Falsified by a `lookup` that returns only the first key that hits.
    #[test]
    fn a_vetoed_allow_leaves_the_same_tables_less_specific_deny_standing() {
        let cfg = config("[global]\nallow = [\"git push\"]\ndeny = [\"git\"]\n");
        let plain = first_verdict("git push origin", &cfg, None);
        assert_eq!(allow_keys(&plain), vec![(Layer::Config(Section::Global), "git push".to_string())]);
        let v = first_verdict("git push origin > log", &cfg, None);
        assert_eq!(
            v,
            PolicyVerdict::Deny(Decision { layer: Layer::Config(Section::Global), key: "git".into() }),
            "got {v:?}"
        );
    }

    /// A layer's reasons and `kj ledger rules` name the section the way an
    /// operator reads the file.
    #[test]
    fn each_section_names_itself_in_a_reason() {
        assert_eq!(Layer::Config(Section::Global).to_string(), "global config");
        assert_eq!(Layer::Config(Section::Rc("coder".into())).to_string(), "rc config (coder)");
        assert_eq!(Layer::Config(Section::Character("coder-opus".into())).to_string(), "character config (coder-opus)");
        assert_eq!(Layer::Config(Section::Root).to_string(), "root config");
        assert_eq!(Layer::UncoveredAllow(Section::Root).to_string(), "root config uncovered tier");
        assert_eq!(Section::Character("coder-opus".into()).header(), "[character.coder-opus]");
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
        let e = evaluate_planned(&plan("kj block read 01a0"), Layers { config: &cfg, performer: None });
        assert!(matches!(e.per_statement[0], PolicyVerdict::Ask(_)));
        assert_eq!(e.verdict(), AskVerdict::Escalate);
        assert_eq!(e.per_statement[0].tier(), "ask");
    }

    #[test]
    fn a_statement_folds_its_commands_deny_first_then_ask_then_all_allow() {
        let cfg = config(EXAMPLE);
        let layers = Layers { config: &cfg, performer: None };
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
                (Layer::Config(Section::Global), "rg".to_string()),
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
        let layers = Layers { config: &cfg, performer: None };
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

    /// A sandbox posture on one rc directory, beside ordinary tiers.
    const SANDBOX: &str = r#"
[global]
allow = ["rg"]
ask = ["git push"]
deny = ["dd"]

[rc.coder]
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
                Layer::UncoveredAllow(Section::Rc("coder".into())),
                "`OUT=log`, a statement with no command".to_string()
            )]
        );
        assert_eq!(
            first_verdict("OUT=log", &cfg, Some("banto")),
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
                Layer::UncoveredAllow(Section::Rc("coder".into())),
                "python3 build.py".to_string()
            )]
        );
        // A kj write verb is uncovered too, and the tier decides it.
        assert!(matches!(
            first_verdict("kj context remove 01a0-abc", &cfg, Some("coder")),
            PolicyVerdict::Allow(_)
        ));
    }

    /// The setting is per section: another rc directory, and a caller with
    /// no rc directory at all, keep today's behavior.
    #[test]
    fn the_uncovered_tier_reaches_only_the_section_that_sets_it() {
        let cfg = config(SANDBOX);
        assert_eq!(
            first_verdict("python3 build.py > log", &cfg, Some("explorer")),
            PolicyVerdict::Uncovered
        );
        assert_eq!(first_verdict("python3 build.py", &cfg, None), PolicyVerdict::Uncovered);
    }

    /// A banto's routine lane work runs without an ask: it creates a
    /// coder and drives it (`docs/character.md`, "A session, inside
    /// kaijutsu", step 3). Repair of a running lane, such as recasting its
    /// performer, still meets the banto's reviewer.
    #[test]
    fn the_shipped_default_lets_a_banto_create_and_drive_a_lane() {
        let cfg = config(crate::config_seed::DEFAULT_GATE_CONFIG);
        for source in [
            "kj context create coder --name count-kj-rs",
            "kj fork --name lane --prompt \"count the files\"",
            "kj drive count-kj-rs --prompt \"count the files\"",
        ] {
            let v = first_verdict(source, &cfg, Some("banto"));
            let keys = allow_keys(&v);
            assert_eq!(keys.len(), 1, "{source}: {v:?}");
            assert_eq!(keys[0].0, Layer::Config(Section::Rc("banto".into())), "{source}: {v:?}");
            assert_eq!(
                first_verdict(source, &cfg, Some("coder")),
                PolicyVerdict::Uncovered,
                "{source} is the banto's work, not a coder's"
            );
        }
        assert_eq!(
            first_verdict("kj context set count-kj-rs --cwd /tmp", &cfg, Some("banto")),
            PolicyVerdict::Uncovered
        );
    }

    /// The shipped default leaves a model's `shell_write` statement to ask,
    /// and ships the allow that removes that first ask as a commented
    /// example: loosening is the operator's choice, per seat (F16).
    #[test]
    fn the_shipped_default_comments_out_the_shell_write_allow() {
        const EXAMPLE_LINES: [&str; 2] = ["#   [rc.coder]", "#   allow = [\"shell_write\"]"];
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
            vec![(Layer::Config(Section::Rc("coder".into())), "shell_write".to_string())],
            "got {v:?}"
        );
    }

    /// Absent is today's behavior, and the shipped default leaves it absent.
    #[test]
    fn the_shipped_default_leaves_the_uncovered_tier_at_ask() {
        let cfg = config(crate::config_seed::DEFAULT_GATE_CONFIG);
        let person = GatePerformer { name: "amy".into(), rc_dir: Some("person".into()), root: true };
        let performers = [None, Some(dir("coder")), Some(dir("mcp")), Some(dir("banto")), Some(person)];
        for performer in &performers {
            assert_eq!(
                cfg.uncovered_for(performer.as_ref()),
                (UncoveredTier::Ask, Section::Global),
                "{performer:?}"
            );
            assert_eq!(
                first_verdict_for("python3 build.py", &cfg, performer.as_ref()),
                PolicyVerdict::Uncovered,
                "{performer:?}"
            );
        }
        assert_eq!(config(EXAMPLE).uncovered_for(Some(&dir("coder"))), (UncoveredTier::Ask, Section::Global));
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
        let layers = Layers { config: &cfg, performer: Some(&dir("coder")) };
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
            vec![(Layer::UncoveredAllow(Section::Rc("coder".into())), "rg foo".to_string())]
        );
        let v = first_verdict("rg foo src", &cfg, Some("coder"));
        assert_eq!(allow_keys(&v), vec![(Layer::Config(Section::Global), "rg".to_string())]);
    }

    /// The whole submission auto-allows: every statement is an Allow,
    /// including a bare assignment, which has no command to key on.
    #[test]
    fn a_whole_program_auto_allows_under_the_uncovered_tier() {
        let cfg = config(SANDBOX);
        let layers = Layers { config: &cfg, performer: Some(&dir("coder")) };
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
        let layers = Layers { config: &cfg, performer: Some(&dir("coder")) };
        let stmt = &plan("cat /etc/passwd")[0];
        assert_eq!(command_verdict(&stmt.plan.commands[0], layers).tier(), "allow");
        let layers = Layers { config: &cfg, performer: Some(&dir("explorer")) };
        assert_eq!(command_verdict(&stmt.plan.commands[0], layers).tier(), "score");
    }

    /// A directory's setting wins over the global one, in both directions —
    /// a global sandbox can be withheld from one role.
    #[test]
    fn an_rc_setting_outranks_the_global_one() {
        let cfg = config("[global]\nuncovered = \"allow\"\n[rc.banto]\nuncovered = \"ask\"\n");
        let v = first_verdict("cat x", &cfg, Some("coder"));
        assert_eq!(allow_keys(&v), vec![(Layer::UncoveredAllow(Section::Global), "cat x".to_string())]);
        assert_eq!(first_verdict("cat x", &cfg, Some("banto")), PolicyVerdict::Uncovered);
    }

    /// The uncovered posture reads over the same way the key lists do: the
    /// latest of the performer's sections that states one decides, else
    /// `[global]`. `[root]` reads over the directory, and the character
    /// reads over `[root]`: a character withholds its directory's sandbox
    /// even when it is a root.
    ///
    /// Falsified by reading the sections earliest first, or by reading
    /// `[root]` after the character.
    #[test]
    fn the_latest_section_stating_the_uncovered_posture_decides_it() {
        let cfg = config(
            "[global]\nuncovered = \"ask\"\n[rc.coder]\nuncovered = \"allow\"\n\
             [character.coder-opus]\nuncovered = \"ask\"\n[root]\nuncovered = \"allow\"\n",
        );
        let coder = dir("coder");
        let opus = GatePerformer { name: "coder-opus".into(), rc_dir: Some("coder".into()), root: false };
        let rooted = GatePerformer { root: true, ..opus.clone() };
        assert_eq!(cfg.uncovered_for(Some(&coder)), (UncoveredTier::Allow, Section::Rc("coder".into())));
        assert_eq!(cfg.uncovered_for(Some(&opus)), (UncoveredTier::Ask, Section::Character("coder-opus".into())));
        assert_eq!(cfg.uncovered_for(Some(&rooted)), (UncoveredTier::Ask, Section::Character("coder-opus".into())));
        let plain_root = GatePerformer { name: "coder-plain".into(), rc_dir: Some("coder".into()), root: true };
        assert_eq!(cfg.uncovered_for(Some(&plain_root)), (UncoveredTier::Allow, Section::Root));
        assert_eq!(cfg.uncovered_for(None), (UncoveredTier::Ask, Section::Global));
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
        let err = GateConfig::parse("[rc.coder]\nuncovered = \"deny\"\n").unwrap_err();
        let GateConfigError::Parse(m) = err else { panic!("{err:?}") };
        assert!(m.contains("[rc.coder]") && m.contains("deny"), "{m}");
    }

    /// `kj ledger rules` lists the posture, so an operator sees it without
    /// reading the file.
    #[test]
    fn the_uncovered_tier_is_listed_for_the_context_it_applies_to() {
        let cfg = config(SANDBOX);
        let entries = cfg.entries_for(Some(&dir("coder")));
        assert_eq!(
            entries.first().map(|(l, k, v)| (l.clone(), k.clone(), *v)),
            Some((
                Layer::UncoveredAllow(Section::Rc("coder".into())),
                "uncovered".to_string(),
                "allow"
            )),
            "{entries:?}"
        );
        assert!(cfg.allows_uncovered(Some(&dir("coder"))));
        assert!(!cfg.allows_uncovered(Some(&dir("explorer"))));
        assert!(!cfg.entries_for(Some(&dir("explorer"))).iter().any(|(l, _, _)| {
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

    // ── sections and the load ───────────────────────────────────────

    /// A `MountTable` with an rc tree naming `dirs`, for the sections
    /// `load_config` checks.
    async fn rc_tree(dirs: &[&str]) -> (MountTable, tempfile::TempDir, tempfile::TempDir) {
        use crate::vfs::LocalBackend;
        let rc = tempfile::tempdir().unwrap();
        for d in dirs {
            std::fs::create_dir_all(rc.path().join(d)).unwrap();
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

    /// A sheet holding `live` and `retired` characters.
    fn sheet(live: &[&str], retired: &[&str]) -> parking_lot::Mutex<KernelDb> {
        let db = KernelDb::open(":memory:").unwrap();
        for (name, retired_at) in live.iter().map(|n| (n, None)).chain(retired.iter().map(|n| (n, Some(1)))) {
            db.insert_character(&crate::kernel_db::CharacterRow {
                principal_id: PrincipalId::new(), name: name.to_string(), created_at: 0, retired_at,
                handoff_ctx: None, root_ctx: None, root: false, rc_dir: Some(name.to_string()),
                console_ctx: None, mcp_hub_ctx: None,
            })
            .unwrap();
        }
        parking_lot::Mutex::new(db)
    }

    /// Capture what the current thread logs until the guard drops.
    fn capture_logs() -> (std::sync::Arc<std::sync::Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
        #[derive(Clone)]
        struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf(Default::default());
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt().with_writer(move || sink.clone()).with_ansi(false).finish();
        (buf.0, tracing::subscriber::set_default(subscriber))
    }

    /// The `[context_type.*]` sections are gone: one fails the load, naming both
    /// replacements, through the error every unknown section takes.
    ///
    /// Falsified by accepting the section, or by refusing it as an unknown
    /// field with no replacement named.
    #[test]
    fn a_context_type_section_fails_the_load_naming_its_replacements() {
        for text in [
            "[context_type.coder]\nallow = [\"rg\"]\n",
            "[global]\nallow = [\"rg\"]\n[context_type.coder.council]\nenabled = false\n",
        ] {
            let err = GateConfig::parse(text).unwrap_err();
            let GateConfigError::Parse(m) = &err else { panic!("{err:?}") };
            assert!(m.contains("[context_type.coder]") && m.contains("[rc.coder]") && m.contains("[character.<name>]"), "{m}");
            assert!(err.to_string().contains("kj config reset gate.toml"), "{err}");
        }
    }

    /// A bare `[context_type]` header, or an empty `context_type` table, is
    /// the same removed section with no type named: it fails the load
    /// naming both replacements.
    ///
    /// Falsified by refusing only a non-empty table.
    #[test]
    fn a_bare_context_type_header_fails_the_load_naming_its_replacements() {
        for text in ["[context_type]\n", "[global]\nallow = [\"rg\"]\n[context_type]\n", "context_type = {}\n"] {
            let err = GateConfig::parse(text).unwrap_err();
            let GateConfigError::Parse(m) = &err else { panic!("{text}: {err:?}") };
            assert!(m.contains("[context_type]") && m.contains("[rc.") && m.contains("[character.<name>]"), "{text}: {m}");
        }
    }

    /// A section naming no rc directory or character applies to nobody. It
    /// loads, so the fix can run, and the load lists it. A retired
    /// character is still on the sheet, and `lib` is not a directory a
    /// character runs. The warning is logged when the set of such sections
    /// differs from the last load's, so a steady state logs once: every
    /// shell submission loads the file two or three times.
    ///
    /// Falsified by failing the load on such a section, by not finding it,
    /// by warning at every load, or by never warning again once the set
    /// changes.
    #[tokio::test]
    async fn a_section_that_applies_to_nobody_loads_is_listed_and_warns_when_the_set_changes() {
        let (vfs, _rc, config) = rc_tree(&["coder", "director", "lib"]).await;
        let db = sheet(&["amy", "coder"], &["old-lane"]);
        write_gate(
            &config,
            "[global]\nuncovered = \"allow\"\n\
             [rc.directer]\nuncovered = \"ask\"\n[rc.coder]\nallow = [\"rg\"]\n[rc.lib]\nallow = [\"rg\"]\n\
             [character.bobb]\nallow = [\"rg\"]\n[character.amy]\nallow = [\"rg\"]\n[character.old-lane]\nallow = [\"rg\"]\n",
        );
        let (logs, guard) = capture_logs();
        let cfg = load_config(&vfs, &db).await.expect("a section that applies to nobody does not fail the load");
        load_config(&vfs, &db).await.expect("and loads again");
        load_config(&vfs, &db).await.expect("and again");
        drop(guard);
        assert_eq!(
            cfg.applies_to_nobody(),
            [Section::Rc("directer".into()), Section::Rc("lib".into()), Section::Character("bobb".into())]
        );
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        for header in ["[rc.directer]", "[rc.lib]", "[character.bobb]"] {
            let lines: Vec<&str> = logs.lines().filter(|l| l.contains(header)).collect();
            assert_eq!(lines.len(), 1, "one warning for {header} across three identical loads: {logs}");
            assert!(lines.iter().all(|l| l.contains("WARN") && l.contains("applies to nobody")), "{logs}");
        }
        assert!(!logs.contains("[rc.coder]") && !logs.contains("[character.amy]") && !logs.contains("old-lane"), "{logs}");
        // The typo'd section applies to nobody, so `director` keeps what
        // `[global]` says: the reason the section is listed.
        assert!(cfg.allows_uncovered(Some(&dir("director"))));

        // A changed set warns again, naming the new set; the same set after
        // that is quiet.
        write_gate(&config, "[rc.directer]\nallow = [\"rg\"]\n[character.bobb]\nallow = [\"rg\"]\n[character.carol]\nallow = [\"rg\"]\n");
        let (logs, guard) = capture_logs();
        load_config(&vfs, &db).await.expect("loads");
        load_config(&vfs, &db).await.expect("loads");
        drop(guard);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        for header in ["[rc.directer]", "[character.bobb]", "[character.carol]"] {
            assert_eq!(logs.lines().filter(|l| l.contains(header)).count(), 1, "{header}: {logs}");
        }
        // Fixing the file and breaking it again is a changed set too.
        write_gate(&config, "[global]\nallow = [\"rg\"]\n");
        load_config(&vfs, &db).await.expect("loads");
        write_gate(&config, "[rc.directer]\nallow = [\"rg\"]\n");
        let (logs, guard) = capture_logs();
        load_config(&vfs, &db).await.expect("loads");
        drop(guard);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert_eq!(logs.lines().filter(|l| l.contains("[rc.directer]")).count(), 1, "{logs}");
    }

    /// An rc tree that cannot be listed (here `/config/rc` is a file, not a
    /// directory) skips the check for `[rc.*]` sections with a warning that
    /// names the rc tree. The load succeeds: refusing it would refuse every
    /// gated statement, blaming a gate.toml that is fine. The character
    /// sections are still checked.
    ///
    /// Falsified by mapping the readdir failure to a load error.
    #[tokio::test]
    async fn an_unreadable_rc_tree_skips_the_rc_check_and_the_load_succeeds() {
        use crate::vfs::LocalBackend;
        let rc = tempfile::tempdir().unwrap();
        let rc_file = rc.path().join("not-a-dir");
        std::fs::write(&rc_file, "x").unwrap();
        let config = tempfile::tempdir().unwrap();
        let vfs = MountTable::new();
        vfs.mount(kaijutsu_types::paths::RC_ROOT, LocalBackend::new(&rc_file)).await;
        vfs.mount(kaijutsu_types::paths::CONFIG_ROOT, LocalBackend::new(config.path())).await;
        write_gate(&config, "[rc.unreadable-tree-dir]\nallow = [\"rg\"]\n[character.unreadable-tree-who]\nallow = [\"rg\"]\n");
        let (logs, guard) = capture_logs();
        let cfg = load_config(&vfs, &sheet(&["amy"], &[])).await.expect("an unreadable rc tree does not fail the load");
        drop(guard);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert_eq!(cfg.applies_to_nobody(), [Section::Character("unreadable-tree-who".into())], "the rc sections are unchecked");
        assert!(
            logs.lines().any(|l| l.contains("WARN") && l.contains(kaijutsu_types::paths::RC_ROOT) && l.contains("could not be listed")),
            "{logs}"
        );
    }

    /// A character sheet that cannot be read still refuses the load, and
    /// the message says the sheet is what failed, not the policy file.
    ///
    /// Falsified by reporting the failure as an invalid gate.toml.
    #[tokio::test]
    async fn an_unreadable_character_sheet_refuses_naming_the_sheet() {
        let (vfs, _rc, config) = rc_tree(&["coder"]).await;
        write_gate(&config, "[character.amy]\nallow = [\"rg\"]\n");
        let db = sheet(&[], &[]);
        db.lock().conn_for_ledger().execute_batch("DROP TABLE characters").unwrap();
        let err = load_config(&vfs, &db).await.unwrap_err();
        let m = err.to_string();
        assert!(m.contains("character sheet could not be read"), "{m}");
        assert!(!m.contains("is not a valid gate policy file") && !m.contains("kj config reset"), "{m}");
    }

    /// An rc tree that lists nothing names every directory: there is
    /// nothing to check against, and a kernel with no rc tree is a
    /// legitimate shape. The rule `KjDispatcher::check_rc_dir` states.
    #[tokio::test]
    async fn an_empty_rc_tree_names_every_directory() {
        let (vfs, _rc, config) = rc_tree(&[]).await;
        write_gate(&config, "[rc.anything-goes]\nallow = [\"rg\"]\n");
        let cfg = load_config(&vfs, &sheet(&[], &[])).await.expect("loads");
        assert!(cfg.applies_to_nobody().is_empty(), "{:?}", cfg.applies_to_nobody());
    }

    /// The shipped `gate.toml` loads against the shipped rc tree, and every
    /// section it ships names a shipped directory.
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
        let loaded = load_config(&vfs, &sheet(&[], &[])).await;
        let cfg = loaded.unwrap_or_else(|e| panic!("{e}"));
        assert!(cfg.applies_to_nobody().is_empty(), "{:?}", cfg.applies_to_nobody());
        assert!(cfg.has_section(&Section::Root), "the shipped root tier");
    }

    #[tokio::test]
    async fn an_absent_file_is_the_empty_config_and_a_bad_one_is_an_error() {
        use crate::vfs::LocalBackend;
        let db = sheet(&[], &[]);
        let vfs = MountTable::new();
        assert_eq!(load_config(&vfs, &db).await, Ok(GateConfig::default()), "no mount at all");
        let dir = tempfile::tempdir().unwrap();
        vfs.mount(kaijutsu_types::paths::CONFIG_ROOT, LocalBackend::new(dir.path())).await;
        assert_eq!(load_config(&vfs, &db).await, Ok(GateConfig::default()), "mounted, file absent");
        std::fs::write(dir.path().join(GATE_CONFIG_FILE), "[global]\nallow = [\"kj nope\"]\n").unwrap();
        assert!(matches!(load_config(&vfs, &db).await, Err(GateConfigError::Parse(_))));
        std::fs::write(dir.path().join(GATE_CONFIG_FILE), EXAMPLE).unwrap();
        assert_eq!(load_config(&vfs, &db).await, Ok(config(EXAMPLE)));
    }

    // ---- council configuration ----

    const COUNCIL_FULL: &str = r#"
[council]
contexts = ["reviewer", "system-rules"]
pool = { method = "loglinear", weights = "mass" }
deadline_ms = 700

[[council.spec]]
name = "shell-gate"
case = "shell"

[[council.threshold]]
spec = "shell-gate"
allow_at = 0.98
mass_floor = -0.05

[rc.coder.council]
enabled = true

[rc.toolie.council]
enabled = false
"#;

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
        assert_eq!(c.server, "", "gate.toml names no server; a slot does");
        assert_eq!(c.model, None);
        assert_eq!(c.contexts, ["reviewer", "system-rules"]);
        assert_eq!(c.pool_method, CouncilPoolMethod::LogLinear);
        assert_eq!(c.pool_weights, CouncilPoolWeights::Mass);
        assert_eq!(c.deadline_ms, 700);
        assert!(c.require_agree, "require_agree defaults to true");
        assert_eq!(
            c.specs,
            [CouncilSpec { name: "shell-gate".into(), case: CouncilCase::Shell, contexts: Vec::new(), require_agree: None }]
        );
        assert_eq!(c.thresholds.len(), 1);
        assert_eq!(c.thresholds[0].allow_at, 0.98);
        assert_eq!(c.thresholds[0].mass_floor, -0.05);
    }

    /// The council server and model come from a context's `gate` slot, so
    /// gate.toml refuses both keys as unknown.
    ///
    /// Falsified by adding `server` back to `CouncilToml`.
    #[test]
    fn a_council_server_or_model_key_is_refused_as_unknown() {
        for key in ["server = \"http://nagaya:8091\"", "model = \"gemma-4\""] {
            let m = council_err(&council_with("deadline_ms = 700", &format!("deadline_ms = 700\n{key}")));
            let name = key.split(' ').next().unwrap();
            assert!(m.contains("unknown field") && m.contains(name), "{key}: {m}");
        }
    }

    #[test]
    fn reviewer_contexts_are_off_unless_the_council_turns_them_on() {
        assert!(!config(COUNCIL_FULL).council().unwrap().reviewer_contexts, "reviewer_contexts defaults to false");
        let on = config(&council_with("deadline_ms = 700", "deadline_ms = 700\nreviewer_contexts = true"));
        assert!(on.council().unwrap().reviewer_contexts);
        let m = council_err(&council_with("deadline_ms = 700", "deadline_ms = 700\nreviewer_contexts = \"yes\""));
        assert!(m.contains("reviewer_contexts"), "{m}");
    }

    #[test]
    fn the_house_rules_context_is_off_unless_the_council_turns_it_on() {
        let c = config(COUNCIL_FULL);
        assert!(!c.council().unwrap().house_rules, "the house-rules context defaults to off");
        assert_eq!(c.council().unwrap().house_rules_tokens, DEFAULT_HOUSE_RULES_TOKENS);
        let on = config(&council_with("deadline_ms = 700", "deadline_ms = 700\nhouse_rules = true\nhouse_rules_tokens = 500"));
        assert!(on.council().unwrap().house_rules);
        assert_eq!(on.council().unwrap().house_rules_tokens, 500);
        let m = council_err(&council_with("deadline_ms = 700", "deadline_ms = 700\nhouse_rules_tokens = 0"));
        assert!(m.contains("house_rules_tokens"), "{m}");
        let m = council_err(&council_with("deadline_ms = 700", "deadline_ms = 700\nhouse_rules = \"yes\""));
        assert!(m.contains("house_rules"), "{m}");
    }

    /// A gate.toml written for the seat context fails to parse, and the
    /// message names the key that replaces each old one.
    #[test]
    fn the_old_seat_keys_fail_and_name_their_replacements() {
        let m = council_err(&council_with("deadline_ms = 700", "deadline_ms = 700\nseat = true"));
        assert!(m.contains("[council] seat") && m.contains("house_rules"), "{m}");
        let m = council_err(&council_with("deadline_ms = 700", "deadline_ms = 700\nseat_tokens = 500"));
        assert!(m.contains("[council] seat_tokens") && m.contains("house_rules_tokens"), "{m}");
    }

    /// A gate.toml that still says `voices` fails to parse, and the message
    /// names `reviewer_contexts`.
    #[test]
    fn the_old_voices_key_fails_and_names_its_replacement() {
        let m = council_err(&council_with("deadline_ms = 700", "deadline_ms = 700\nvoices = true"));
        assert!(m.contains("[council] voices") && m.contains("reviewer_contexts"), "{m}");
    }

    #[test]
    fn the_council_mode_defaults_to_gatekeeper_and_parses_bumper() {
        let c = config(COUNCIL_FULL);
        assert_eq!(c.council().unwrap().mode, CouncilMode::Gatekeeper);
        assert_eq!(c.council().unwrap().bump_limit, Some(DEFAULT_BUMP_LIMIT));
        let bumper = config(&council_with("deadline_ms = 700", "deadline_ms = 700\nmode = \"bumper\"\nbump_limit = 5"));
        assert_eq!(bumper.council().unwrap().mode, CouncilMode::Bumper);
        assert_eq!(bumper.council().unwrap().bump_limit, Some(5));
        let gatekeeper = config(&council_with("deadline_ms = 700", "deadline_ms = 700\nmode = \"gatekeeper\""));
        assert_eq!(gatekeeper.council().unwrap().mode, CouncilMode::Gatekeeper);
    }

    #[test]
    fn the_council_mode_and_bump_limit_reject_junk() {
        let m = council_err(&council_with("deadline_ms = 700", "deadline_ms = 700\nmode = \"bouncer\""));
        assert!(m.contains("mode") && m.contains("bouncer") && m.contains("gatekeeper, bumper"), "{m}");
        let m = council_err(&council_with("deadline_ms = 700", "deadline_ms = 700\nbump_limit = 0"));
        assert!(m.contains("bump_limit"), "{m}");
        let m = council_err(&council_with("deadline_ms = 700", "deadline_ms = 700\nbump_limit = \"three\""));
        assert!(m.contains("bump_limit"), "{m}");
    }

    /// `bump-only` parses, has no bump limit, and refuses a `bump_limit`,
    /// which it would ignore.
    #[test]
    fn bump_only_mode_parses_and_takes_no_bump_limit() {
        let c = config(&council_with("deadline_ms = 700", "deadline_ms = 700\nmode = \"bump-only\""));
        assert_eq!(c.council().unwrap().mode, CouncilMode::BumpOnly);
        let m = council_err(&council_with("deadline_ms = 700", "deadline_ms = 700\nmode = \"bump-only\"\nbump_limit = 3"));
        assert!(m.contains("bump_limit") && m.contains("bump-only"), "{m}");
        let m = council_err(&council_with("deadline_ms = 700", "deadline_ms = 700\nmode = \"bouncer\""));
        assert!(m.contains("bump-only"), "the error lists every mode: {m}");
    }

    /// `escalate` opens an ask after N bumps in a row within M minutes. It
    /// is off unless set, belongs to the bumper modes, and refuses zeros.
    #[test]
    fn escalate_parses_in_the_bumper_modes_and_rejects_junk() {
        assert_eq!(config(&council_with("deadline_ms = 700", "deadline_ms = 700\nmode = \"bump-only\"")).council().unwrap().escalate, None);
        let c = config(&council_with("deadline_ms = 700", "deadline_ms = 700\nmode = \"bump-only\"\nescalate = { bumps = 4, minutes = 10 }"));
        assert_eq!(c.council().unwrap().escalate, Some(CouncilEscalate { bumps: 4, minutes: 10 }));
        let c = config(&council_with("deadline_ms = 700", "deadline_ms = 700\nmode = \"bumper\"\nescalate = { bumps = 2, minutes = 1 }"));
        assert_eq!(c.council().unwrap().escalate, Some(CouncilEscalate { bumps: 2, minutes: 1 }));
        let m = council_err(&council_with("deadline_ms = 700", "deadline_ms = 700\nescalate = { bumps = 4, minutes = 10 }"));
        assert!(m.contains("escalate") && m.contains("gatekeeper"), "{m}");
        for junk in ["{ bumps = 0, minutes = 10 }", "{ bumps = 3, minutes = 0 }", "{ bumps = 5000000000, minutes = 10 }", "{ bumps = 3 }", "{ bumps = 3, minutes = 5, extra = 1 }"] {
            let m = council_err(&council_with("deadline_ms = 700", &format!("deadline_ms = 700\nmode = \"bump-only\"\nescalate = {junk}")));
            assert!(m.contains("escalate"), "{junk}: {m}");
        }
    }

    #[test]
    fn a_spec_may_set_its_own_require_agree() {
        let cfg = config(&council_with("case = \"shell\"", "case = \"shell\"\nrequire_agree = false"));
        let c = cfg.council().unwrap();
        assert_eq!(c.specs[0].require_agree, Some(false));
        assert!(!c.require_agree_for("shell-gate"));
        let plain = config(COUNCIL_FULL);
        assert_eq!(plain.council().unwrap().specs[0].require_agree, None);
        assert!(plain.council().unwrap().require_agree_for("shell-gate"));
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
        assert!(cfg.council_for(None).is_none());
        assert!(cfg.council_for(Some(&dir("coder"))).is_none());
        let shipped = config(crate::config_seed::DEFAULT_GATE_CONFIG);
        assert!(shipped.council().is_none(), "the shipped default is council-off");
        assert!(shipped.council_for(Some(&dir("coder"))).is_none());
    }

    #[test]
    fn council_is_enabled_per_rc_directory() {
        let cfg = config(COUNCIL_FULL);
        assert!(cfg.council_for(Some(&dir("coder"))).is_some());
        assert!(cfg.council_for(Some(&dir("toolie"))).is_none(), "enabled = false");
        assert!(cfg.council_for(Some(&dir("banto"))).is_none(), "unmentioned directory");
        assert!(cfg.council_for(None).is_none(), "no performer");
    }

    #[test]
    fn an_rc_directory_can_replace_the_council_contexts() {
        let cfg = config(&format!("{COUNCIL_FULL}\n[rc.banto.council]\nenabled = true\ncontexts = [\"council-amy\", \"council-banto\"]\n"));
        let banto = cfg.council_for(Some(&dir("banto"))).unwrap();
        assert_eq!(banto.contexts, ["council-amy", "council-banto"]);
        let coder = cfg.council_for(Some(&dir("coder"))).unwrap();
        assert_eq!(coder.contexts, ["reviewer", "system-rules"]);
        // Everything but the list is inherited: mode, deadline, house rules,
        // reviewer contexts, agreement, specs, and thresholds.
        let mut inherited = coder.clone();
        inherited.contexts = banto.contexts.clone();
        assert_eq!(banto, inherited);
        assert!(cfg.council_for(Some(&dir("toolie"))).is_none());
        assert!(cfg.council_for(None).is_none());
    }

    /// A council block reads over the blocks before it key by key, as the
    /// type's block read over `[council]`: `enabled` is the last block's, and
    /// `contexts` and `judge` are the last stated. So a character keeps its
    /// directory's judge and contexts unless it states its own, the
    /// character reads over `[root]`, and a later `enabled = false` turns
    /// the council off.
    ///
    /// Falsified by letting a block replace the whole council, by reading
    /// the blocks earliest-last, or by letting an earlier `enabled` stand.
    #[test]
    fn a_later_council_block_reads_over_an_earlier_one_key_by_key() {
        let cfg = config(&format!(
            "{COUNCIL_FULL}\n\
             [rc.coder.council]\nenabled = true\njudge = \"judge\"\ncontexts = [\"council-coder\"]\n\
             [character.coder-opus.council]\nenabled = true\ncontexts = [\"council-opus\"]\n\
             [character.coder-quiet.council]\nenabled = false\n\
             [character.coder-jev.council]\nenabled = true\njudge = \"jev\"\n\
             [root.council]\nenabled = true\njudge = \"root-judge\"\n",
        ).replace("[rc.coder.council]\nenabled = true\n\n", ""));
        let played = |name: &str, root: bool| GatePerformer { name: name.into(), rc_dir: Some("coder".into()), root };
        let coder = cfg.council_for(Some(&played("coder", false))).unwrap();
        assert_eq!((coder.contexts.as_slice(), coder.judge.as_deref()), (["council-coder".to_string()].as_slice(), Some("judge")));
        let opus = cfg.council_for(Some(&played("coder-opus", false))).unwrap();
        assert_eq!((opus.contexts.as_slice(), opus.judge.as_deref()), (["council-opus".to_string()].as_slice(), Some("judge")),
            "a character keeps its directory's judge");
        let jev = cfg.council_for(Some(&played("coder-jev", false))).unwrap();
        assert_eq!((jev.contexts.as_slice(), jev.judge.as_deref()), (["council-coder".to_string()].as_slice(), Some("jev")),
            "and its directory's contexts");
        assert!(cfg.council_for(Some(&played("coder-quiet", false))).is_none(), "a later enabled = false turns it off");
        let rooted = cfg.council_for(Some(&played("coder-other", true))).unwrap();
        assert_eq!(rooted.judge.as_deref(), Some("root-judge"), "[root] reads over the directory");
        assert!(cfg.council_for(Some(&played("coder-quiet", true))).is_none(), "the character reads over [root]");
        let rooted_jev = cfg.council_for(Some(&played("coder-jev", true))).unwrap();
        assert_eq!(rooted_jev.judge.as_deref(), Some("jev"), "the character reads over [root]");
        // Every judge any block names is one the councils list.
        let mut judges: Vec<String> = cfg.councils().into_iter().filter_map(|c| c.judge).collect();
        judges.sort();
        assert_eq!(judges, ["jev", "judge", "root-judge"]);
    }

    #[test]
    fn a_section_council_rejects_empty_duplicate_and_spec_contexts() {
        for (contexts, why) in [
            ("[]", "at least one label"),
            ("[\"\"]", "non-empty"),
            ("[\"a\", \"a\"]", "listed twice"),
            ("[\"examples\"]", "already in spec"),
        ] {
            let base = council_with("case = \"shell\"", "case = \"shell\"\ncontexts = [\"examples\"]");
            let m = council_err(&format!("{base}\n[rc.banto.council]\nenabled = true\ncontexts = {contexts}\n"));
            assert!(m.contains("[rc.banto.council]") && m.contains(why), "{contexts}: {m}");
            let m = council_err(&format!("{base}\n[character.banto.council]\nenabled = true\ncontexts = {contexts}\n"));
            assert!(m.contains("[character.banto.council]") && m.contains(why), "{contexts}: {m}");
        }
    }

    /// A list on a disabled council would be validated and then dropped, so
    /// the file says something it does not do. The load refuses it.
    #[test]
    fn a_disabled_section_council_cannot_name_contexts() {
        let m = council_err(&format!("{COUNCIL_FULL}\n[rc.banto.council]\nenabled = false\ncontexts = [\"council-amy\"]\n"));
        assert!(m.contains("[rc.banto.council]") && m.contains("enabled = false"), "{m}");
        let m = council_err(&format!("{COUNCIL_FULL}\n[root.council]\nenabled = false\ncontexts = [\"council-amy\"]\n"));
        assert!(m.contains("[root.council]") && m.contains("enabled = false"), "{m}");
    }

    /// Each layer's council block can name its judge character; a council
    /// whose blocks name none runs with no judge, and no default name
    /// stands in. A judge on a disabled council, or an empty name, fails
    /// the load.
    #[test]
    fn each_layer_s_council_can_name_its_judge_character() {
        let cfg = config(&format!(
            "{COUNCIL_FULL}\n[rc.director.council]\nenabled = true\njudge = \"judge\"\n\
             [character.banto.council]\nenabled = true\njudge = \"banto-judge\"\n"
        ));
        assert_eq!(cfg.council_for(Some(&dir("director"))).unwrap().judge.as_deref(), Some("judge"));
        let banto = GatePerformer { name: "banto".into(), rc_dir: Some("banto".into()), root: false };
        assert_eq!(cfg.council_for(Some(&banto)).unwrap().judge.as_deref(), Some("banto-judge"));
        assert_eq!(cfg.council_for(Some(&dir("coder"))).unwrap().judge, None, "no judge key, no judge");
        for section in ["rc.director", "character.banto", "root"] {
            let m = council_err(&format!("{COUNCIL_FULL}\n[{section}.council]\nenabled = false\njudge = \"judge\"\n"));
            assert!(m.contains(&format!("[{section}.council] judge")) && m.contains("enabled = false"), "{m}");
            let m = council_err(&format!("{COUNCIL_FULL}\n[{section}.council]\nenabled = true\njudge = \" \"\n"));
            assert!(m.contains("judge: name a character"), "{m}");
        }
        let m = council_err("[rc.coder.council]\nenabled = false\njudge = \"judge\"\n");
        assert!(m.contains("no [council] section"), "{m}");
    }

    #[test]
    fn a_section_enabling_a_council_the_file_lacks_fails_the_load() {
        let m = council_err("[rc.coder.council]\nenabled = true\n");
        assert!(m.contains("coder") && m.contains("[council]"), "{m}");
        config("[rc.coder.council]\nenabled = false\n");
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
    fn council_contexts_must_be_nonempty_distinct_labels() {
        let m = council_err(&council_with("[\"reviewer\", \"system-rules\"]", "[]"));
        assert!(m.contains("[council] contexts") && m.contains("at least one"), "{m}");
        let m = council_err(&council_with("[\"reviewer\", \"system-rules\"]", "[\"reviewer\", \"\"]"));
        assert!(m.contains("[council] contexts") && m.contains("non-empty"), "{m}");
        let m = council_err(&council_with("[\"reviewer\", \"system-rules\"]", "[\"reviewer\", \"reviewer\"]"));
        assert!(m.contains("[council] contexts") && m.contains("reviewer") && m.contains("twice"), "{m}");
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

    /// A spec may name contexts of its own, read after `[council] contexts`
    /// for its decisions only. One already in `[council] contexts`, or
    /// listed twice, is a parse error rather than a decision-time miss.
    #[test]
    fn a_spec_names_contexts_of_its_own() {
        let cfg = config(&council_with("case = \"shell\"", "case = \"shell\"\ncontexts = [\"council-code\"]"));
        let council = cfg.council().unwrap();
        assert_eq!(council.specs[0].contexts, ["council-code"]);
        let plain = config(COUNCIL_FULL);
        assert!(plain.council().unwrap().specs[0].contexts.is_empty());
        let m = council_err(&council_with("case = \"shell\"", "case = \"shell\"\ncontexts = [\"reviewer\"]"));
        assert!(m.contains("[[council.spec]] shell-gate contexts") && m.contains("reviewer"), "{m}");
        let m = council_err(&council_with("case = \"shell\"", "case = \"shell\"\ncontexts = [\"a\", \"a\"]"));
        assert!(m.contains("[[council.spec]] shell-gate contexts") && m.contains("twice"), "{m}");
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
        let m = council_err(&council_with("case = \"shell\"", "case = \"file\""));
        assert!(m.contains("case") && m.contains("file") && m.contains("shell, program"), "{m}");
        let second = COUNCIL_FULL.replacen(
            "[[council.threshold]]",
            "[[council.spec]]\nname = \"other-shell\"\ncase = \"shell\"\n\n[[council.threshold]]",
            1,
        );
        let m = council_err(&second);
        assert!(m.contains("other-shell") && m.contains("one spec per case"), "{m}");
        let dup = COUNCIL_FULL.replacen(
            "[[council.threshold]]",
            "[[council.spec]]\nname = \"shell-gate\"\ncase = \"shell\"\n\n[[council.threshold]]",
            1,
        );
        let m = council_err(&dup);
        assert!(m.contains("shell-gate") && m.contains("twice"), "{m}");
    }

    /// A program spec sits beside the shell spec, with thresholds of its own.
    #[test]
    fn a_program_spec_parses_beside_the_shell_spec() {
        let text = COUNCIL_FULL.replacen(
            "[[council.threshold]]",
            "[[council.spec]]\nname = \"program-gate\"\ncase = \"program\"\n\n[[council.threshold]]\nspec = \"program-gate\"\n\
             allow_at = 0.98\nmass_floor = -1.5\n\n[[council.threshold]]",
            1,
        );
        let cfg = config(&text);
        let c = cfg.council().unwrap();
        assert_eq!(
            c.specs,
            vec![
                CouncilSpec { name: "shell-gate".into(), case: CouncilCase::Shell, contexts: Vec::new(), require_agree: None },
                CouncilSpec { name: "program-gate".into(), case: CouncilCase::Program, contexts: Vec::new(), require_agree: None },
            ]
        );
        assert_eq!(c.threshold_for("program-gate").unwrap().mass_floor, -1.5);
    }

    /// A bump-only gate parses: bump-only mode over the bump spec, the
    /// program spec reading council-code with its own `require_agree`, a
    /// threshold for each, and the coder's deny tier still refusing.
    #[test]
    fn a_bump_only_gate_bumps_and_reads_council_code_for_programs() {
        let cfg = config(
            r#"
[global]
uncovered = "ask"

[council]
mode = "bump-only"
contexts = ["council-system"]
pool = { method = "loglinear", weights = "mass" }
deadline_ms = 20000
require_agree = true
house_rules = true

[[council.spec]]
name = "shell-bump"
case = "shell"
contexts = ["council-shell"]

[[council.threshold]]
spec = "shell-bump"
allow_at = 0.8
mass_floor = -2.5

[[council.spec]]
name = "program-gate"
case = "program"
contexts = ["council-code"]
require_agree = false

[[council.threshold]]
spec = "program-gate"
allow_at = 0.98
mass_floor = -2.5

[rc.coder]
deny = ["kj ledger", "kj rc", "kj binding"]

[rc.coder.council]
enabled = true
"#,
        );
        let c = cfg.council().expect("the gate declares a council");
        assert_eq!(c.mode, CouncilMode::BumpOnly);
        assert_eq!(c.bump_limit, None);
        assert_eq!(c.specs[0].name, "shell-bump");
        assert_eq!(c.specs[0].contexts, ["council-shell"]);
        assert_eq!(c.specs[1].contexts, ["council-code"]);
        assert!(c.require_agree_for("shell-bump") && !c.require_agree_for("program-gate"));
        assert!(c.threshold_for("shell-bump").is_some() && c.threshold_for("program-gate").is_some());
        assert!(cfg.council_for(Some(&dir("coder"))).is_some());
        assert!(c.house_rules);
        for verb in ["kj ledger show 01a1", "kj ledger list --history", "kj rc list", "kj binding list"] {
            assert!(matches!(first_verdict(verb, &cfg, Some("coder")), PolicyVerdict::Deny(_)), "{verb}");
        }
        assert!(!matches!(first_verdict("kj wait --operation 01a1", &cfg, Some("coder")), PolicyVerdict::Deny(_)));
    }

    #[test]
    fn a_threshold_must_name_a_declared_spec() {
        let m = council_err(&council_with("spec = \"shell-gate\"", "spec = \"nope\""));
        assert!(m.contains("[[council.threshold]] spec") && m.contains("nope"), "{m}");
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

    /// A threshold names its spec alone: the council server's identity is
    /// recorded with each decision but no longer selects a threshold. Amy,
    /// 2026-10-05: "let's drop this pinning business against mk, we're not
    /// that precise for this."
    #[test]
    fn thresholds_are_found_by_spec_alone() {
        let cfg = config(COUNCIL_FULL);
        let c = cfg.council().unwrap();
        assert_eq!(c.threshold_for("shell-gate").unwrap().allow_at, 0.98);
        assert!(c.threshold_for("other-spec").is_none());
    }

    #[test]
    fn two_thresholds_for_one_spec_are_an_error() {
        let second = COUNCIL_FULL.replacen(
            "[rc.coder.council]",
            "[[council.threshold]]\nspec = \"shell-gate\"\nallow_at = 0.9\nmass_floor = -0.1\n\n[rc.coder.council]",
            1,
        );
        let m = council_err(&second);
        assert!(m.contains("shell-gate") && m.contains("one threshold per spec"), "{m}");
    }

    /// The old identity lines fail the parse with a message that says to
    /// delete them, rather than an unknown-field error.
    #[test]
    fn the_old_identity_fields_fail_naming_what_to_delete() {
        for field in ["weight_hash", "engine", "tokenizer_hash", "template"] {
            let m = council_err(&council_with("allow_at = 0.98", &format!("{field} = \"x\"\nallow_at = 0.98")));
            assert!(m.contains(field) && m.contains("delete"), "{field}: {m}");
        }
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
