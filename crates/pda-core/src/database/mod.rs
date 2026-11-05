//! Подмодули слоя БД.
pub mod collector;
pub mod db_service;

pub use collector::{Collector, OnStatus, Record};
pub use db_service::{DatabaseService, DbConfig};
