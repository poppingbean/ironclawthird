//! Built-in tools that come with the agent.

mod binance;
mod echo;
pub mod extension_tools;
mod file;
mod http;
mod job;
mod json;
mod memory;
mod message;
pub mod path_utils;
pub mod routine;
pub(crate) mod shell;
pub mod skill_tools;
mod telegram;
mod time;
mod web_fetch;

pub use binance::{
    BinanceFuturesAccountTool, BinanceFuturesOpenOrdersTool, BinanceFuturesOrderTool,
    BinanceFuturesSetTpSlTool, BinanceSnapshotTool, PriceAnalysisTool,
};
pub use echo::EchoTool;
pub use extension_tools::{
    ToolActivateTool, ToolAuthTool, ToolInstallTool, ToolListTool, ToolRemoveTool, ToolSearchTool,
};
pub use file::{ApplyPatchTool, ListDirTool, ReadFileTool, WriteFileTool};
pub use http::HttpTool;
pub use job::{
    CancelJobTool, CreateJobTool, JobEventsTool, JobPromptTool, JobStatusTool, ListJobsTool,
    PromptQueue, SchedulerSlot,
};
pub use json::JsonTool;
pub use memory::{MemoryReadTool, MemorySearchTool, MemoryTreeTool, MemoryWriteTool};
pub use message::MessageTool;
pub use routine::{
    RoutineCreateTool, RoutineDeleteTool, RoutineHistoryTool, RoutineListTool, RoutineUpdateTool,
};
pub use shell::ShellTool;
pub use skill_tools::{SkillInstallTool, SkillListTool, SkillRemoveTool, SkillSearchTool};
pub use telegram::TelegramNotifyTool;
pub use time::TimeTool;
pub use web_fetch::WebFetchTool;

mod html_converter;

pub use html_converter::convert_html_to_markdown;
