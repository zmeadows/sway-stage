//! Offline tests of the integration harness's pure logic, from constructed Sway trees and
//! events in the shape `swaymsg -r` reports.
//!
//! This is a separate crate root over the harness modules that leaves out `main.rs`, so
//! no test can reach the harness entry point. Tests never read `SWAYSOCK`, connect to Sway,
//! or start a process. Outside their own temporary directories, they read only this test
//! process's `/proc` entry.

#![allow(
    dead_code,
    reason = "launcher, session, IPC, process, and runner code runs only in the harness executable"
)]

mod alternating_profile;
mod capture;
mod fixtures;
mod floating_refusal;
mod headless;
mod held_launch;
mod interference;
mod ipc;
mod launch_context;
mod launcher;
mod layout;
mod lifecycle;
mod load_failure;
mod observation;
mod output_expectation;
mod output_migration;
mod ownership;
mod preservation;
mod process;
mod profile;
mod scenarios;
mod secondary_output;
mod session_roundtrip;
