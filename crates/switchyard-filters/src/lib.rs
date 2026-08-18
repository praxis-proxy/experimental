//! Experimental Praxis AI filters built around NVIDIA NeMo Switchyard.
//!
//! This crate is discovered at build time by praxis-ai's
//! `[package.metadata.praxis-filters]` auto-discovery (see praxis-ai
//! `server/build.rs` and `praxis-ai-build-support`). The generated
//! registration code calls [`register_filters`], which is emitted by the
//! [`praxis_filter::export_filters!`] macro invoked below.
//!
//! It ships the `switchyard_route` filter (praxis-proxy/experimental#2):
//! Capability-mode Mixture-of-Models routing on top of `switchyard-libsy`
//! v0.2.0. See the `switchyard` module docs for the design.

mod switchyard;

praxis_filter::export_filters! {
    http "switchyard_route" => switchyard::SwitchyardRouteFilter::from_config,
}
