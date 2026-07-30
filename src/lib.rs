//! poechk — a Path of Exile price-check overlay for the COSMIC desktop.
//!
//! The crate is one binary that plays three roles (`daemon`, `overlay`, `check`),
//! selected by subcommand. See `DESIGN.md` for the architecture and roadmap.

pub mod check;
pub mod checklog;
pub mod config;
pub mod data;
pub mod inject;
pub mod item;
pub mod overlay;
pub mod price;
