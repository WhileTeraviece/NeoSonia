//! NeoSonia core：配置、模型访问、agent 循环、长期记忆、工具协议。
//!
//! 四个 crate 的公共契约都从这里导出；具体工具实现见 `neosonia-tools`，
//! 邮件见 `neosonia-mail`，界面见 `neosonia-app`。

pub mod agent;
pub mod config;
pub mod conversation;
pub mod llm;
pub mod memory;
pub mod model;
pub mod persona;
pub mod provider_status;
pub mod registry;
pub mod review;
pub mod shared;
pub mod skill;
pub mod subagent;
pub mod util;

pub use agent::{AgentEngine, ApproveDecision, EngineConfig, EventSink};
pub use config::{
    AgentMode, ApiProfile, MailOAuthSettings, McpServerConfig, McpSettings, McpTransport,
    OAuthProvider, Paths, ReviewSettings, Settings,
};
pub use conversation::ConversationStore;
pub use memory::MemoryStore;
pub use model::{
    AgentEvent, AgentHost, ChatMessage, DeliverFile, FunctionCall, MemoryApi, MemoryItem,
    MemoryStats, Persona, PlanStep, Role, SubagentRequest, ToolCall, ToolCtx, ToolOutcome,
    ToolSpec,
};
pub use persona::PersonaStore;
pub use provider_status::{
    ProviderKind, ProviderStatus, current_status as current_provider_status, requires_attention,
};
pub use registry::{Tool, ToolRegistry};
pub use review::{ReviewRule, ReviewStore, RiskClassifier, RiskJudge, RiskVerdict};
pub use shared::{
    Attachment, AttachmentKind, ConversationHit, ConversationSummary, SkillDetail, SkillSummary,
    TrustedDevice,
};
pub use skill::SkillStore;
