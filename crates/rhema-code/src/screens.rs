//! The three screens' input handling and outcome logic, pure state, no
//! terminal. The one exception is the chat palette's path rows, which
//! list the filesystem. The renderer reads these structs;
//! [`crate::app`] routes their actions into transitions and effects.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rhema_provider::ModelCard;

mod chat;
pub(crate) mod clipboard;
mod decision;
mod dialogs;
mod mcp;
mod model_picker;
mod palette;
mod picker;
mod profiles;
mod providers;
mod settings;
#[cfg(test)]
mod tests;
mod websearch;

pub use chat::{ChatAction, ChatState, Chip};
pub use decision::{
    AddDecisionDialog, DECISION_KINDS, DecisionBuilt, DecisionDialogAction, DecisionDialogStage,
    DecisionKind, DecisionRow, decision_rows,
};
pub use dialogs::{
    API_CHOICES, AddInstanceDialog, AddProfileDialog, AddSandboxDialog, DialogAction, DialogStage,
    DialogStatus, PendingInstance, ProfileDialogAction, SANDBOX_ADD_KINDS, SandboxDialogAction,
    SandboxRow,
};
pub use mcp::{
    AddMcpServerDialog, MCP_TRANSPORTS, McpDialogAction, McpDialogStage, McpServerRow, PairLines,
    mcp_server_rows,
};
pub use model_picker::{
    CatalogModel, CurrentModel, ModelPicker, ModelPickerAction, ModelPickerStatus, PickerRow,
};
pub(crate) use palette::format_tokens;
pub use palette::{
    PaletteProfile, PaletteRow, PaletteSandbox, filtered_commands, palette_rows,
    palette_rows_cached,
};
pub use picker::{PickerScreen, ResumeAction, ResumeRow, ResumeScreen};
pub use profiles::{ProfileAction, ProfileRow, ProfileScreen, profile_rows};
pub use providers::{
    CompanionProvider, LoginProvider, PROVIDERS, ProviderCategory, ProviderEntry, ProviderFollowUp,
    providers_in,
};
pub use settings::{
    InstanceRow, SettingsAction, SettingsScreen, SettingsSection, SkillRow, SkillSource,
    SubagentRow, SubagentSource, UpdateCheck, UpdateInstall, UpdatePage,
};
pub use websearch::{
    AddWebSearchDialog, WEBSEARCH_KINDS, WebSearchDialogAction, WebSearchDialogStage, WebSearchRow,
    websearch_rows,
};
