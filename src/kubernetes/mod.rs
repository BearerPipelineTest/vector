//! This mod contains an implementation of kube::runtime::reflector
//! with the ability to retain cached Objects past their associated
//! DELETE event.
//!
//! https://docs.rs/kube/latest/kube/runtime/reflector/index.html
//!

#![cfg(feature = "kubernetes")]

pub mod pod_manager_logic;
pub mod reflector;

pub use reflector::reflector;
