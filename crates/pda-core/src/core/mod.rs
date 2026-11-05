//! Подмодули ядра.
pub mod historian;
pub mod tag_point;
pub mod tag_registry;

pub use historian::Historian;
pub use tag_point::TagPoint;
pub use tag_registry::TagRegistry;
