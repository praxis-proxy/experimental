//! Experimental Praxis AI filters.
//!
//! This crate is discovered at build time by praxis-ai's
//! `[package.metadata.praxis-filters]` auto-discovery (see praxis-ai
//! `server/build.rs` and `praxis-ai-build-support`). The generated registration
//! code calls [`register_filters`], which is emitted by the
//! [`praxis_filter::export_filters!`] macro invoked below.

mod placeholder;
mod switchyard_route;

praxis_filter::export_filters! {
    http "experimental_placeholder" => placeholder::PlaceholderFilter::from_config,
    http "switchyard_route" => switchyard_route::SwitchyardRouteFilter::from_config,
}
