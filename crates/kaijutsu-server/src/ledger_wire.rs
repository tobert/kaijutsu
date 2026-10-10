//! Typed ledger values on the wire: the builders for `AskSummary`,
//! `AskDetail` and `AskAnswerOutcome`, and the readers for a listing
//! filter and a remember request. Every match is total, so a new variant
//! fails to compile here instead of arriving as a neighbouring one.

use crate::kaijutsu_capnp::{
    ask_answer_outcome, ask_detail, ask_filter, ask_summary, principal_ref, remember, AskAnswerFailureKind,
    AskOrigin, AskStatus, AskVerdict, AskView, RememberScope,
};

pub(crate) fn wire_ask_status(status: kaijutsu_types::AskStatus) -> AskStatus {
    use kaijutsu_types::AskStatus as S;
    match status {
        S::Pending => AskStatus::Pending,
        S::Claimed => AskStatus::Claimed,
        S::Allowed => AskStatus::Allowed,
        S::Denied => AskStatus::Denied,
        S::Expired => AskStatus::Expired,
        S::Abandoned => AskStatus::Abandoned,
    }
}

fn ask_status(status: AskStatus) -> kaijutsu_types::AskStatus {
    use kaijutsu_types::AskStatus as S;
    match status {
        AskStatus::Pending => S::Pending,
        AskStatus::Claimed => S::Claimed,
        AskStatus::Allowed => S::Allowed,
        AskStatus::Denied => S::Denied,
        AskStatus::Expired => S::Expired,
        AskStatus::Abandoned => S::Abandoned,
    }
}

fn wire_origin(origin: kaijutsu_types::AskOrigin) -> AskOrigin {
    use kaijutsu_types::AskOrigin as O;
    match origin {
        O::Hook => AskOrigin::Hook,
        O::HookResult => AskOrigin::HookResult,
        O::ShellGate => AskOrigin::ShellGate,
        O::KjVerb => AskOrigin::KjVerb,
    }
}

fn origin(origin: AskOrigin) -> kaijutsu_types::AskOrigin {
    use kaijutsu_types::AskOrigin as O;
    match origin {
        AskOrigin::Hook => O::Hook,
        AskOrigin::HookResult => O::HookResult,
        AskOrigin::ShellGate => O::ShellGate,
        AskOrigin::KjVerb => O::KjVerb,
    }
}

fn set_principal(mut builder: principal_ref::Builder<'_>, principal: &kaijutsu_types::PrincipalRef) {
    builder.set_id(principal.id.as_bytes());
    builder.set_name(&principal.name);
}

pub(crate) fn set_ask_summary(mut builder: ask_summary::Builder<'_>, ask: &kaijutsu_types::AskSummary) {
    builder.set_request_id(&ask.request_id);
    builder.set_status(wire_ask_status(ask.status));
    builder.set_origin(wire_origin(ask.origin));
    if let Some(context) = ask.context_id {
        builder.set_context_id(context.as_bytes());
    }
    builder.set_description(&ask.description);
    let mut statements = builder.reborrow().init_statements(ask.statements.len() as u32);
    for (i, statement) in ask.statements.iter().enumerate() {
        statements.set(i as u32, statement.as_str());
    }
    if let Some(p) = &ask.requester {
        set_principal(builder.reborrow().init_requester(), p);
    }
    if let Some(p) = &ask.performer {
        set_principal(builder.reborrow().init_performer(), p);
    }
    if let Some(p) = &ask.reviewer {
        set_principal(builder.reborrow().init_reviewer(), p);
    }
    builder.set_created_at_ms(ask.created_at_ms);
    if let Some(at) = ask.decided_at_ms {
        builder.set_decided_at_ms(at);
        builder.set_has_decided_at(true);
    }
}

pub(crate) fn set_ask_detail(mut builder: ask_detail::Builder<'_>, ask: &kaijutsu_types::AskDetail) {
    set_ask_summary(builder.reborrow().init_summary(), &ask.summary);
    if let Some(v) = &ask.instance { builder.set_instance(v); }
    if let Some(v) = &ask.tool { builder.set_tool(v); }
    if let Some(v) = &ask.hook_id { builder.set_hook_id(v); }
    if let Some(v) = &ask.label { builder.set_label(v); }
    if let Some(block) = &ask.tool_call_block_id {
        crate::rpc::set_block_id_builder(&mut builder.reborrow().init_tool_call_block_id(), block);
    }
    if let Some(v) = &ask.exec_source { builder.set_exec_source(v); }
    if let Some(v) = &ask.cwd { builder.set_cwd(v); }
    let mut env = builder.reborrow().init_env(ask.env.len() as u32);
    for (i, var) in ask.env.iter().enumerate() {
        let mut e = env.reborrow().get(i as u32);
        e.set_name(&var.name);
        if let Some(value) = &var.value {
            e.set_value(value);
        }
    }
    if let Some(decision) = &ask.decision {
        let mut d = builder.reborrow().init_decision();
        if let Some(p) = &decision.decided_by {
            set_principal(d.reborrow().init_decided_by(), p);
        }
        if let Some(v) = &decision.option { d.set_option(v); }
        if let Some(v) = &decision.remember_scope { d.set_remember_scope(v); }
        if let Some(v) = &decision.auto_reason { d.set_auto_reason(v); }
    }
    if let Some(at) = ask.redeemed_at_ms {
        builder.set_redeemed_at_ms(at);
        builder.set_has_redeemed_at(true);
    }
    if let Some(v) = &ask.publication_abandoned { builder.set_publication_abandoned(v); }
    let mut moves = builder.init_reassignments(ask.reassignments.len() as u32);
    for (i, moved) in ask.reassignments.iter().enumerate() {
        let mut m = moves.reborrow().get(i as u32);
        if let Some(from) = &moved.from {
            set_principal(m.reborrow().init_from(), from);
        }
        set_principal(m.reborrow().init_to(), &moved.to);
        set_principal(m.reborrow().init_by(), &moved.by);
        m.set_at_ms(moved.at_ms);
    }
}

pub(crate) fn set_answer_outcome(
    builder: ask_answer_outcome::Builder<'_>,
    outcome: &Result<kaijutsu_types::AskAnswered, kaijutsu_types::AskAnswerFailure>,
) {
    use kaijutsu_types::AskAnswerFailureKind as K;
    match outcome {
        Ok(answered) => {
            let mut a = builder.init_answered();
            set_ask_summary(a.reborrow().init_summary(), &answered.summary);
            if let Some(remembered) = &answered.remembered {
                let mut r = a.init_remembered();
                r.set_learned(remembered.learned);
                r.set_note(&remembered.note);
            }
        }
        Err(failure) => {
            let mut f = builder.init_failed();
            f.set_kind(match failure.kind {
                K::NotFound => AskAnswerFailureKind::NotFound,
                K::NotReviewer => AskAnswerFailureKind::NotReviewer,
                K::AlreadyAnswered => AskAnswerFailureKind::AlreadyAnswered,
                K::Archived => AskAnswerFailureKind::Archived,
                K::Refused => AskAnswerFailureKind::Refused,
            });
            f.set_message(&failure.message);
        }
    }
}

pub(crate) fn read_ask_filter(reader: ask_filter::Reader<'_>) -> capnp::Result<kaijutsu_types::AskFilter> {
    Ok(kaijutsu_types::AskFilter {
        view: match reader.get_view()? {
            AskView::Queue => kaijutsu_types::AskView::Queue,
            AskView::History => kaijutsu_types::AskView::History,
        },
        status: if reader.get_has_status() { Some(ask_status(reader.get_status()?)) } else { None },
        origin: if reader.get_has_origin() { Some(origin(reader.get_origin()?)) } else { None },
        since_ms: reader.get_has_since().then(|| reader.get_since_ms()),
        limit: reader.get_has_limit().then(|| reader.get_limit()),
    })
}

pub(crate) fn read_verdict(verdict: AskVerdict) -> kaijutsu_types::AskVerdict {
    match verdict {
        AskVerdict::Allow => kaijutsu_types::AskVerdict::Allow,
        AskVerdict::Deny => kaijutsu_types::AskVerdict::Deny,
        AskVerdict::PromptCancelled => kaijutsu_types::AskVerdict::PromptCancelled,
    }
}

pub(crate) fn read_remember(reader: remember::Reader<'_>) -> capnp::Result<kaijutsu_types::Remember> {
    Ok(kaijutsu_types::Remember {
        scope: match reader.get_scope()? {
            RememberScope::Session => kaijutsu_types::RememberScope::Session,
            RememberScope::Always => kaijutsu_types::RememberScope::Always,
            RememberScope::Character => kaijutsu_types::RememberScope::Character,
        },
        family: reader.get_family(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A variable set to the empty string is not an unset one: `kj ledger
    /// show` prints `NAME=""` for the first and `NAME unset` for the second,
    /// and the wire must keep the two apart.
    #[test]
    fn an_empty_env_value_is_not_unset_on_the_wire() {
        let detail = kaijutsu_types::AskDetail {
            summary: kaijutsu_types::AskSummary {
                request_id: "r".into(), status: kaijutsu_types::AskStatus::Pending,
                origin: kaijutsu_types::AskOrigin::ShellGate, context_id: None, description: String::new(),
                statements: Vec::new(), requester: None, performer: None, reviewer: None,
                created_at_ms: 0, decided_at_ms: None,
            },
            instance: None, tool: None, hook_id: None, label: None, tool_call_block_id: None,
            exec_source: None, cwd: None,
            env: vec![
                kaijutsu_types::AskEnv { name: "EMPTY".into(), value: Some(String::new()) },
                kaijutsu_types::AskEnv { name: "UNSET".into(), value: None },
            ],
            decision: None, redeemed_at_ms: None, publication_abandoned: None, reassignments: Vec::new(),
        };
        let mut message = capnp::message::Builder::new_default();
        set_ask_detail(message.init_root::<ask_detail::Builder<'_>>(), &detail);
        let reader = message.get_root_as_reader::<ask_detail::Reader<'_>>().unwrap();
        let env = reader.get_env().unwrap();
        assert!(env.get(0).has_value(), "an empty value must stay present");
        assert!(!env.get(1).has_value(), "an unset value must stay absent");
    }
}
