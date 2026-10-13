//! Подмодули слоя БД.
pub mod db_service;
pub mod collector;

pub use db_service::{DatabaseService, DbConfig};
pub use collector::{Collector, OnStatus, Record};
