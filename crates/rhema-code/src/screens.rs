//! The three screens' input handling and outcome logic — pure state, no
//! terminal. The one exception is the chat palette's path rows, which
//! list the filesystem. The renderer reads these structs;
//! [`crate::app`] routes their actions into transitions and effects.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rhema_provider::ModelCard;

mod chat;
pub(crate) mod clipboard;
mod dialogs;
mod mcp;
mod palette;
mod picker;
mod profiles;
mod providers;
mod settings;
#[cfg(test)]
mod tests;

pub use chat::{ChatAction, ChatState, Chip};
pub use dialogs::{
    AddInstanceDialog, AddProfileDialog, AddSandboxDialog, DialogAction, DialogStage, DialogStatus,
    PendingInstance, ProfileDialogAction, SANDBOX_ADD_KINDS, SandboxDialogAction, SandboxRow,
};
pub use mcp::{
    AddMcpServerDialog, MCP_TRANSPORTS, McpDialogAction, McpDialogStage, McpServerRow, PairLines,
    mcp_server_rows,
};
pub(crate) use palette::format_tokens;
pub use palette::{
    PaletteProfile, PaletteRow, PaletteSandbox, filtered_commands, palette_rows,
    palette_rows_cached,
};
pub use picker::{
    CatalogModel, PickerAction, PickerScreen, PickerStatus, ResumeAction, ResumeRow, ResumeScreen,
};
pub use profiles::{ProfileAction, ProfileRow, ProfileScreen, profile_rows};
pub use providers::{LoginProvider, PROVIDERS, ProviderCategory, ProviderEntry, providers_in};
pub use settings::{
    InstanceRow, SettingsAction, SettingsScreen, SettingsSection, SkillRow, SkillSource,
    SubagentRow, SubagentSource, UpdateCheck, UpdateInstall, UpdatePage,
};
