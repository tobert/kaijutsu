//! Typed ledger values off the wire, and the filter and remember request
//! onto it. The counterpart of `kaijutsu-server`'s `ledger_wire`. Every
//! match is total, so a new variant fails to compile here.

use kaijutsu_types::{
    AskAnswerFailure, AskAnswerFailureKind, AskAnswered, AskDecision, AskDetail, AskEnv, AskFilter, AskOrigin,
    AskReassignment, AskStatus, AskSummary, AskVerdict, AskView, ContextId, PrincipalId, PrincipalRef, Remember,
    RememberResult, RememberScope,
};

use crate::kaijutsu_capnp as wire;
use crate::rpc::RpcError;

fn text(t: capnp::Result<capnp::text::Reader<'_>>) -> Result<String, RpcError> {
    Ok(t?.to_string()?)
}

pub(crate) fn ask_status(status: wire::AskStatus) -> AskStatus {
    match status {
        wire::AskStatus::Pending => AskStatus::Pending,
        wire::AskStatus::Claimed => AskStatus::Claimed,
        wire::AskStatus::Allowed => AskStatus::Allowed,
        wire::AskStatus::Denied => AskStatus::Denied,
        wire::AskStatus::Expired => AskStatus::Expired,
        wire::AskStatus::Abandoned => AskStatus::Abandoned,
    }
}

fn wire_ask_status(status: AskStatus) -> wire::AskStatus {
    match status {
        AskStatus::Pending => wire::AskStatus::Pending,
        AskStatus::Claimed => wire::AskStatus::Claimed,
        AskStatus::Allowed => wire::AskStatus::Allowed,
        AskStatus::Denied => wire::AskStatus::Denied,
        AskStatus::Expired => wire::AskStatus::Expired,
        AskStatus::Abandoned => wire::AskStatus::Abandoned,
    }
}

fn origin(origin: wire::AskOrigin) -> AskOrigin {
    match origin {
        wire::AskOrigin::Hook => AskOrigin::Hook,
        wire::AskOrigin::HookResult => AskOrigin::HookResult,
        wire::AskOrigin::ShellGate => AskOrigin::ShellGate,
        wire::AskOrigin::KjVerb => AskOrigin::KjVerb,
    }
}

fn wire_origin(origin: AskOrigin) -> wire::AskOrigin {
    match origin {
        AskOrigin::Hook => wire::AskOrigin::Hook,
        AskOrigin::HookResult => wire::AskOrigin::HookResult,
        AskOrigin::ShellGate => wire::AskOrigin::ShellGate,
        AskOrigin::KjVerb => wire::AskOrigin::KjVerb,
    }
}

fn principal(reader: wire::principal_ref::Reader<'_>) -> Result<PrincipalRef, RpcError> {
    let raw = reader.get_id()?;
    let id = PrincipalId::try_from_slice(raw)
        .ok_or_else(|| RpcError::ServerError(format!("malformed {}-byte principal id", raw.len())))?;
    Ok(PrincipalRef { id, name: text(reader.get_name())? })
}

pub(crate) fn ask_summary(reader: wire::ask_summary::Reader<'_>) -> Result<AskSummary, RpcError> {
    Ok(AskSummary {
        request_id: text(reader.get_request_id())?,
        status: ask_status(reader.get_status()?),
        origin: origin(reader.get_origin()?),
        context_id: ContextId::try_from_slice(reader.get_context_id()?),
        description: text(reader.get_description())?,
        statements: reader.get_statements()?.iter().map(text).collect::<Result<_, _>>()?,
        requester: if reader.has_requester() { Some(principal(reader.get_requester()?)?) } else { None },
        performer: if reader.has_performer() { Some(principal(reader.get_performer()?)?) } else { None },
        reviewer: if reader.has_reviewer() { Some(principal(reader.get_reviewer()?)?) } else { None },
        created_at_ms: reader.get_created_at_ms(),
        decided_at_ms: reader.get_has_decided_at().then(|| reader.get_decided_at_ms()),
    })
}

pub(crate) fn ask_detail(reader: wire::ask_detail::Reader<'_>) -> Result<AskDetail, RpcError> {
    let opt = |has: bool, t: capnp::Result<capnp::text::Reader<'_>>| -> Result<Option<String>, RpcError> {
        if has { Ok(Some(text(t)?)) } else { Ok(None) }
    };
    let decision = if reader.has_decision() {
        let d = reader.get_decision()?;
        Some(AskDecision {
            decided_by: if d.has_decided_by() { Some(principal(d.get_decided_by()?)?) } else { None },
            option: opt(d.has_option(), d.get_option())?,
            remember_scope: opt(d.has_remember_scope(), d.get_remember_scope())?,
            auto_reason: opt(d.has_auto_reason(), d.get_auto_reason())?,
        })
    } else {
        None
    };
    let env = reader.get_env()?.iter()
        .map(|e| Ok(AskEnv { name: text(e.get_name())?, value: opt(e.has_value(), e.get_value())? }))
        .collect::<Result<Vec<_>, RpcError>>()?;
    Ok(AskDetail {
        summary: ask_summary(reader.get_summary()?)?,
        instance: opt(reader.has_instance(), reader.get_instance())?,
        tool: opt(reader.has_tool(), reader.get_tool())?,
        hook_id: opt(reader.has_hook_id(), reader.get_hook_id())?,
        label: opt(reader.has_label(), reader.get_label())?,
        tool_call_block_id: if reader.has_tool_call_block_id() {
            Some(crate::rpc::parse_block_id(&reader.get_tool_call_block_id()?)?)
        } else {
            None
        },
        exec_source: opt(reader.has_exec_source(), reader.get_exec_source())?,
        cwd: opt(reader.has_cwd(), reader.get_cwd())?,
        env,
        decision,
        redeemed_at_ms: reader.get_has_redeemed_at().then(|| reader.get_redeemed_at_ms()),
        publication_abandoned: opt(reader.has_publication_abandoned(), reader.get_publication_abandoned())?,
        reassignments: reader.get_reassignments()?.iter()
            .map(|m| Ok(AskReassignment {
                from: if m.has_from() { Some(principal(m.get_from()?)?) } else { None },
                to: principal(m.get_to()?)?,
                by: principal(m.get_by()?)?,
                at_ms: m.get_at_ms(),
            }))
            .collect::<Result<_, RpcError>>()?,
    })
}

pub(crate) fn answer_outcome(
    reader: wire::ask_answer_outcome::Reader<'_>,
) -> Result<Result<AskAnswered, AskAnswerFailure>, RpcError> {
    use wire::ask_answer_outcome::Which;
    match reader.which()? {
        Which::Answered(a) => {
            let a = a?;
            let remembered = if a.has_remembered() {
                let r = a.get_remembered()?;
                Some(RememberResult { learned: r.get_learned(), note: text(r.get_note())? })
            } else {
                None
            };
            Ok(Ok(AskAnswered { summary: ask_summary(a.get_summary()?)?, remembered }))
        }
        Which::Failed(f) => {
            let f = f?;
            let kind = match f.get_kind()? {
                wire::AskAnswerFailureKind::NotFound => AskAnswerFailureKind::NotFound,
                wire::AskAnswerFailureKind::NotReviewer => AskAnswerFailureKind::NotReviewer,
                wire::AskAnswerFailureKind::AlreadyAnswered => AskAnswerFailureKind::AlreadyAnswered,
                wire::AskAnswerFailureKind::Archived => AskAnswerFailureKind::Archived,
                wire::AskAnswerFailureKind::Refused => AskAnswerFailureKind::Refused,
            };
            Ok(Err(AskAnswerFailure { kind, message: text(f.get_message())? }))
        }
    }
}

pub(crate) fn set_ask_filter(mut builder: wire::ask_filter::Builder<'_>, filter: &AskFilter) {
    builder.set_view(match filter.view {
        AskView::Queue => wire::AskView::Queue,
        AskView::History => wire::AskView::History,
    });
    if let Some(status) = filter.status {
        builder.set_status(wire_ask_status(status));
        builder.set_has_status(true);
    }
    if let Some(o) = filter.origin {
        builder.set_origin(wire_origin(o));
        builder.set_has_origin(true);
    }
    if let Some(since) = filter.since_ms {
        builder.set_since_ms(since);
        builder.set_has_since(true);
    }
    if let Some(limit) = filter.limit {
        builder.set_limit(limit);
        builder.set_has_limit(true);
    }
}

pub(crate) fn wire_verdict(verdict: AskVerdict) -> wire::AskVerdict {
    match verdict {
        AskVerdict::Allow => wire::AskVerdict::Allow,
        AskVerdict::Deny => wire::AskVerdict::Deny,
        AskVerdict::PromptCancelled => wire::AskVerdict::PromptCancelled,
    }
}

pub(crate) fn set_remember(mut builder: wire::remember::Builder<'_>, remember: Remember) {
    builder.set_scope(match remember.scope {
        RememberScope::Session => wire::RememberScope::Session,
        RememberScope::Always => wire::RememberScope::Always,
    });
    builder.set_family(remember.family);
}
