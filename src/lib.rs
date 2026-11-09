//! Gears: a small agent harness for Motor OS.

#[cfg(not(target_os = "motor"))]
compile_error!("Gears supports Motor OS only");

pub mod cancellation;
pub mod cli;
pub mod config;
pub mod directory;
pub mod mock;
pub mod net;
pub mod platform;
pub mod process;
pub mod prompt;
#[cfg(test)]
mod property;
pub mod provider;
pub mod provider_choice;
pub mod reset;
pub mod run_lock;
pub mod runtime;
pub mod session;
pub mod setup;
pub mod shell_state;
pub mod state;
pub mod tools;
pub mod trace;
pub mod ui;
