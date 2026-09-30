//! gitdashy: a PR dashboard with Claude reviews and shared team memory. One binary: the desktop window,
//! the HTTP server behind it, and the CLI subcommands the hooks call.
//!
//! Module map mirrors the Python it replaced (dashy/core/*.py): each file here names its source.

pub mod autorev;
pub mod bind;
pub mod cli;
pub mod config;
pub mod dbrepo;
pub mod dbschema;
pub mod demo;
pub mod diff;
pub mod founding;
pub mod friction;
pub mod github;
pub mod heartbeat;
pub mod held;
pub mod install;
pub mod knowledge;
pub mod lan;
pub mod learning;
pub mod llm;
pub mod log;
pub mod memory;
pub mod mirror;
pub mod report;
pub mod review;
pub mod shell;
pub mod spells;
pub mod state;
pub mod story;
pub mod team;
/// A stand-in for the GitHub API; tests only. See #165.
#[cfg(test)]
pub mod testapi;
pub mod textdiff;
pub mod types;
pub mod update;
pub mod web;
