//! Offline tests of the integration harness's pure logic, from constructed Sway trees and
//! events in the shape `swaymsg -r` reports.
//!
//! This is a separate crate root over the harness modules that leaves out `main.rs`, so
//! no test can reach the harness entry point. Tests never read `SWAYSOCK`, connect to Sway,
//! or start a process. Outside their own temporary directories, they read only this test
//! process's `/proc` entry.

#![allow(
    dead_code,
    reason = "IPC, process, and preparation code is exercised only by the live harness"
)]

mod capture;
mod fixtures;
mod ipc;
mod layout;
mod observation;
mod ownership;
mod process;
mod scenarios;
