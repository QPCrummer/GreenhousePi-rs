#![no_std]
#![no_main]

//! # GEM-rs: Greenhouse Environmental Monitor
//! ## A greenhouse monitoring system solution in Rust
//!
//! Features:
//! - Temperature monitoring and safety range
//! - Humidity monitoring and safety range
//! - Pressure monitoring
//! - Uptime tracker
//! - Watering system scheduler
//! - Smoke/fire detection support
//!
//! Links:
//! [GitHub](https://github.com/QPCrummer/GEM-rs)

pub mod preferences;
pub mod rendering;
pub mod sensors;
pub mod timer;
