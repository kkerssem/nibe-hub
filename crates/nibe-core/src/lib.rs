//! Shared logic for the NIBE collector and the MCP server:
//! API model, SQLite storage, time-weighted aggregation and well-known point ids.

pub mod analysis;
pub mod api;
pub mod config;
pub mod db;
pub mod logging;
pub mod series;
pub mod timeutil;
pub mod wellknown;
