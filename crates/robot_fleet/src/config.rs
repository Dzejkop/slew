//! Simulation tuning constants.

use std::time::Duration;

pub(crate) const MAP_W: i32 = 40;
pub(crate) const MAP_H: i32 = 20;
/// Number of robots.
pub(crate) const ROBOTS: usize = 4;
/// Default channel capacity.
pub(crate) const CHANNEL_CAP: usize = 8;
/// Shared broadcast channel given to every robot.
pub(crate) const SHARED_CHANNEL: i64 = 100;
/// Game ticks per second (before the speed multiplier).
pub(crate) const TICK: Duration = Duration::from_millis(250);
/// Game ticks a move / mine takes.
pub(crate) const MOVE_TICKS: u32 = 2;
pub(crate) const MINE_TICKS: u32 = 3;
/// VM fuel granted to each robot program per frame.
pub(crate) const STEP_FUEL: u64 = 20_000;
/// Cap on retained log lines.
pub(crate) const MAX_LOG: usize = 500;
