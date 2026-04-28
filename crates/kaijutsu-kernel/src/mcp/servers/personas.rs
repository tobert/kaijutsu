//! `BuiltinPersonasServer` — named archetypes that bundle a
//! `ContextToolBinding` (M3-D1).
//!
//! v1 scope: a persona is a name plus a list of MCP instance ids. `apply`
//! installs those instances as the calling context's `ContextToolBinding`.
//! ListTools hook bundles (planner-strict / explorer-permissive style)
//! are a follow-up — the composition machinery is in place via
//! HookPhase::ListTools but threading a hook bundle through the persona
//! definition shape adds enough surface that v1 leaves it for later.
//!
//! Persistence is in-memory only for v1 (DashMap on the server itself).
//! KernelDb-backed persistence rides alongside the binding-persistence
//! path the broker already uses (D-54) once we agree on the schema.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use dashmap::DashMap;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::super::binding::ContextToolBinding;
use super::super::broker::Broker;
use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PersonasListParams {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PersonasApplyParams {
    /// Persona name to apply to the calling context.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PersonasDefineParams {
    pub name: String,
    /// Ordered instance ids that constitute this persona's binding.
    pub instances: Vec<String>,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
struct Persona {
    name: String,
    description: Option<String>,
    instances: Vec<InstanceId>,
}

pub struct BuiltinPersonasServer {
    instance_id: InstanceId,
    broker: Weak<Broker>,
    /// In-memory persona store (v1). Persistence is a follow-up.
    personas: Arc<DashMap<String, Persona>>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl BuiltinPersonasServer {
    pub const INSTANCE: &'static str = "builtin.personas";

    pub fn new(broker: Weak<Broker>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        let server = Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            broker,
            personas: Arc::new(DashMap::new()),
            notif_tx,
        };
        // Seed three archetypes with concrete builtin instance bundles.
        // `personas_apply` auto-injects `builtin.personas` and
        // `builtin.tool_search` regardless of the persona definition, so
        // the seeds list only the topical tools — switch + search is
        // implicit. Sound-engineer-style archetypes that depend on
        // external servers (gpal, pawlsa) are left for users to
        // `personas_define`; they aren't seeded because the relevant
        // instances may not be registered.
        let seeds: &[(&str, &str, &[&str])] = &[
            (
                "planner",
                "High-level planning. Block manipulation + tool discovery.",
                &[
                    super::block::BlockToolsServer::INSTANCE,
                    super::kernel_info::KernelInfoServer::INSTANCE,
                ],
            ),
            (
                "coder",
                "Default writing surface: blocks, files, hooks.",
                &[
                    super::block::BlockToolsServer::INSTANCE,
                    super::file::FileToolsServer::INSTANCE,
                    super::hooks_builtin::BuiltinHooksServer::INSTANCE,
                    super::kernel_info::KernelInfoServer::INSTANCE,
                ],
            ),
            (
                "explorer",
                "Read-mostly: file reads, blocks, MCP resources.",
                &[
                    super::file::FileToolsServer::INSTANCE,
                    super::block::BlockToolsServer::INSTANCE,
                    super::resources_builtin::BuiltinResourcesServer::INSTANCE,
                    super::kernel_info::KernelInfoServer::INSTANCE,
                ],
            ),
        ];
        for (name, desc, instances) in seeds {
            server.personas.insert(
                name.to_string(),
                Persona {
                    name: name.to_string(),
                    description: Some(desc.to_string()),
                    instances: instances.iter().map(|s| InstanceId::new(*s)).collect(),
                },
            );
        }
        server
    }

    fn broker(&self) -> McpResult<Arc<Broker>> {
        self.broker.upgrade().ok_or_else(|| McpError::InstanceDown {
            instance: self.instance_id.clone(),
            reason: "broker dropped".to_string(),
        })
    }
}

#[async_trait]
impl McpServerLike for BuiltinPersonasServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        let list_schema = schemars::schema_for!(PersonasListParams);
        let apply_schema = schemars::schema_for!(PersonasApplyParams);
        let define_schema = schemars::schema_for!(PersonasDefineParams);
        Ok(vec![
            KernelTool {
                instance: self.instance_id.clone(),
                name: "personas_list".to_string(),
                description: Some(
                    "Enumerate registered personas (planner / coder / \
                     explorer ship by default; users can `personas_define` more)."
                        .to_string(),
                ),
                input_schema: serde_json::to_value(list_schema)
                    .map_err(McpError::InvalidParams)?,
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "personas_apply".to_string(),
                description: Some(
                    "Set the calling context's tool binding to a persona's \
                     instance list. The applied binding always includes \
                     `builtin.personas` and `builtin.tool_search` so the \
                     model can switch personas and discover tools — every \
                     other instance is the persona's literal definition. \
                     Errors when the persona's instance list is empty."
                        .to_string(),
                ),
                input_schema: serde_json::to_value(apply_schema)
                    .map_err(McpError::InvalidParams)?,
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "personas_define".to_string(),
                description: Some(
                    "Create or update a persona. Replaces an existing \
                     entry of the same name. v1: instances only; \
                     ListTools hook bundles are a follow-up."
                        .to_string(),
                ),
                input_schema: serde_json::to_value(define_schema)
                    .map_err(McpError::InvalidParams)?,
            },
        ])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        match params.tool.as_str() {
            "personas_list" => {
                let _: PersonasListParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let mut entries: Vec<serde_json::Value> = self
                    .personas
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "name": p.name,
                            "description": p.description,
                            "instances": p.instances.iter().map(|i| i.as_str()).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                entries.sort_by(|a, b| {
                    a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
                });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![],
                    structured: Some(serde_json::json!({ "personas": entries })),
                })
            }
            "personas_apply" => {
                let parsed: PersonasApplyParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let persona = self
                    .personas
                    .get(&parsed.name)
                    .map(|p| p.value().clone())
                    .ok_or_else(|| McpError::Protocol(format!(
                        "no persona named {:?} (use personas_list to see available)",
                        parsed.name
                    )))?;
                // Reject apply when the persona's user-defined instance list
                // is empty — auto-injecting personas + tool_search would
                // produce a binding with two tools and no real surface,
                // which is not what the user asked for. Point them at
                // personas_define.
                if persona.instances.is_empty() {
                    return Err(McpError::Protocol(format!(
                        "persona {:?} has no instances — use personas_define \
                         to populate it before applying",
                        parsed.name
                    )));
                }
                // Auto-inject personas + tool_search so the model can
                // always switch personas and discover what it has, even
                // when the persona definition omits them. Dedup against
                // the persona's own list to keep ordering stable.
                let mut instances: Vec<InstanceId> = persona.instances.clone();
                for required in [
                    InstanceId::new(Self::INSTANCE),
                    InstanceId::new(super::tool_search::BuiltinToolSearchServer::INSTANCE),
                ] {
                    if !instances.contains(&required) {
                        instances.push(required);
                    }
                }
                let broker = self.broker()?;
                let binding = ContextToolBinding::with_instances(instances.clone());
                broker.set_binding(ctx.context_id, binding).await;
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![],
                    structured: Some(serde_json::json!({
                        "applied": parsed.name,
                        "instances": instances.iter().map(|i| i.as_str()).collect::<Vec<_>>(),
                    })),
                })
            }
            "personas_define" => {
                let parsed: PersonasDefineParams = serde_json::from_value(params.arguments)
                    .map_err(McpError::InvalidParams)?;
                let instances: Vec<InstanceId> =
                    parsed.instances.iter().map(|s| InstanceId::new(s.clone())).collect();
                self.personas.insert(
                    parsed.name.clone(),
                    Persona {
                        name: parsed.name.clone(),
                        description: parsed.description,
                        instances: instances.clone(),
                    },
                );
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![],
                    structured: Some(serde_json::json!({
                        "defined": parsed.name,
                        "instances": instances.iter().map(|i| i.as_str()).collect::<Vec<_>>(),
                    })),
                })
            }
            other => Err(McpError::ToolNotFound {
                instance: self.instance_id.clone(),
                tool: other.to_string(),
            }),
        }
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}
