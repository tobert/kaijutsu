//! The council: a System 1 model check on shell submissions the gate's
//! static rules leave uncovered. See `docs/council.md`.
//!
//! The wire types, spec ids, and pooling math are `gogi_proto`'s, the
//! council contract's crate.
//!
//! `sync` keeps the council server holding the council contexts and specs;
//! `projection` turns a kaijutsu context into a council v2 context;
//! `seat` builds a council v2 queue seat from that context; `gate` makes one
//! decision for a submission and records it; `shell_case` builds the
//! statements its shell case shows.

pub(crate) mod gate;
#[cfg(test)]
mod gate_e2e;
pub(crate) mod observe;
pub(crate) mod programs;
mod projection;
pub(crate) mod report_stop;
pub(crate) mod reviewer_contexts;
mod seat;
pub(crate) mod shadow;
mod shell_case;
pub(crate) mod sync;

/// The rules the kernel checks a spec against when it reads one: mkviolet's
/// limits, with `text` questions allowed up to 4096 tokens. A server without
/// the `describe` capability refuses a `text` question itself.
pub(crate) const SPEC_RULES: gogi_proto::spec::SpecRules =
    gogi_proto::spec::SpecRules { questions_per_spec: 16, choice_options: 26, describe: true, text_tokens: 4096 };

/// Parses spec JSON text by [`SPEC_RULES`].
pub(crate) fn parse_spec(text: &str) -> Result<gogi_proto::spec::Spec, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("the spec is not JSON: {e}"))?;
    gogi_proto::spec::parse_spec(&value, &SPEC_RULES).map_err(|e| match e.param {
        Some(param) => format!("the spec breaks the contract at {param}: {}", e.message),
        None => format!("the spec breaks the contract: {}", e.message),
    })
}
