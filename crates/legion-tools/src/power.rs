//! `power`: putting this machine to sleep, restarting it and shutting it down.
//!
//! Everything that touches the machine goes through an [`Actuator`]; see [`actuator`] for why a
//! test build can never act on the machine it runs on.

mod actuator;
mod delay;
#[cfg(any(test, feature = "testing"))]
pub mod fake;

pub use self::actuator::{Actuator, Ran, SystemActuator, actuator};
pub use self::delay::{DELAY_MODE, try_delay_cli};
