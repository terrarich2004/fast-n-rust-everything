//! fast-n-rust: unified CLI and MCP server combining the best of
//! `fast-anything` (hotspot 5% optimization) and `rust-anything` (deep unit & whole-app migration).

#![forbid(unsafe_code)]

pub mod clean;
pub mod cli;
pub mod doctor;
pub mod mcp;
pub mod mcp_prompts;
pub mod setup;
pub mod synergy;
