//! Gedeelde logica voor de NIBE-collector en de MCP-server:
//! API-model, SQLite-opslag, tijdgewogen aggregatie en bekende punt-id's.

pub mod analysis;
pub mod api;
pub mod db;
pub mod series;
pub mod timeutil;
pub mod wellknown;
