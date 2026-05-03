#![cfg_attr(not(test), no_std)]

pub mod config;
pub mod k8s;
#[cfg(not(test))]
pub mod kubelet;
#[cfg(not(test))]
pub mod net;
pub mod wallclock;
