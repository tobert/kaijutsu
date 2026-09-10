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
//! **Structural refusals veto allows.** An allow from a family rule, a
//! config layer or the builtin layer covers a *key*, never arguments, so a redirect, a
//! background flag, a heredoc or a non-plain argument drops the statement
//! to `Uncovered` and it meets the classifier and the gate as usual. A deny
//! or an ask fires regardless of structure. A command whose arguments the
//! evaluator cannot read as plain text has no key at all and is
//! `Uncovered`, which fails toward the default: ask.
//!
//! Composition per program is the ledger's own [`AskCoverage`] rule: a Deny
//! anywhere denies the whole submission, every statement must be Allow to
//! auto-allow, anything else escalates. A submission is never partially
//! applied.
//!
//! **Origin boundary.** Layers 2–4 classify a *planned command tree*, which
//! a [`GateSpec`] carries for `Origin::ShellGate` and for a shell-shaped
//! `Origin::Hook` ask, but not for `Origin::KjVerb`. A `KjVerb` ask meets
//! the user-rule layer only and is otherwise `Uncovered`.
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
}

impl std::fmt::Display for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserRule => f.write_str("user rule"),
            Self::UserFamily => f.write_str("user family rule"),
            Self::ContextTypeConfig(t) => write!(f, "context_type config ({t})"),
            Self::GlobalConfig => f.write_str("global config"),
            Self::Builtin => f.write_str("builtin"),
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

/// One tier table: `[global]` or one `[context_type.<type>]` section, keys
/// normalized to canonical names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TierTable {
    /// `(normalized key, verdict)`; a key may appear under more than one
    /// verdict, and the higher rank wins.
    entries: Vec<(String, TierVerdict)>,
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

/// The parsed `gate.toml`: the global tier and one tier per context type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GateConfig {
    global: TierTable,
    context_types: BTreeMap<String, TierTable>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct GateToml {
    #[serde(default)]
    global: TierToml,
    #[serde(default)]
    context_type: BTreeMap<String, TierToml>,
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
        };
        for (context_type, tier) in &raw.context_type {
            let section = format!("[context_type.{context_type}]");
            config
                .context_types
                .insert(context_type.clone(), Self::table_from(tier, &section)?);
        }
        Ok(config)
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
        Ok(TierTable { entries })
    }

    /// The config entries in force for a caller of `context_type`, for
    /// `kj ledger rules`: `(layer, key, verdict)`, the context_type section
    /// first.
    pub(crate) fn entries_for(&self, context_type: Option<&str>) -> Vec<(Layer, String, &'static str)> {
        let mut out = Vec::new();
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

/// Read and parse `/config/kernel/gate.toml`. An absent file (or no
/// `/config/kernel` mount) is the empty config; any other failure is an
/// error the caller must refuse on.
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
    GateConfig::parse(&text)
}

// ── Layers for one evaluation ───────────────────────────────────────────

/// The config layers in force for one caller: the parsed file and the
/// calling context's type, when it has one.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Layers<'a> {
    pub config: &'a GateConfig,
    pub context_type: Option<&'a str>,
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
        Origin::ShellGate | Origin::Hook | Origin::KjVerb => return Ok(vec![PolicyVerdict::Uncovered; n]),
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
/// or a non-plain argument, or a `kj` argv that does not classify, would
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
        Origin::ShellGate | Origin::Hook | Origin::KjVerb => {
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
    fold_verdicts(verdicts)
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
/// the global table, then the builtin layer. This is also what stamps the
/// per-command `tier` on `KJ_TOOL_PLAN`.
pub(crate) fn command_verdict(cmd: &PlannedCommand, layers: Layers<'_>) -> PolicyVerdict {
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
                return match verdict {
                    TierVerdict::Deny => PolicyVerdict::Deny(decision),
                    TierVerdict::Ask => PolicyVerdict::Ask(decision),
                    TierVerdict::Allow if keys.structural_ok => {
                        PolicyVerdict::Allow(vec![decision])
                    }
                    TierVerdict::Allow => PolicyVerdict::Uncovered,
                };
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
            PlannedValue::Plain(s) => words.push(s.clone()),
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
        let e = unconfigured("kj context remove 01a0-abc");
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

    /// The `--help` rule, mirroring `contrib/lfm2d-ladder-check.kai`: help
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
}
