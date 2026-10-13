//! Промышленные драйверы.
//!
//! Каждый драйвер — набор свободных функций + async-точка входа `run_*`,
//! которую DriverManager запускает отдельной tokio-задачей на подключение.
//! Состояние (статус, метрики, флаг running) живёт в `base::DriverState`,
//! показания публикуются в `TagRegistry`, а в историю пишет журнал.

pub mod base;
pub mod driver_manager;
pub mod modbus_address;
pub mod modbus_client;
pub mod modbus_server;
pub mod registry;
pub mod s7_proto;
pub mod snap7;

pub use driver_manager::DriverManager;
pub use registry::{
    address_info, get_driver_label, is_known, normalize_driver_type, validate_address,
};
