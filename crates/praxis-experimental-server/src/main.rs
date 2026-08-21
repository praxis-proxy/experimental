//! Thin server binary that composes the stock praxis-ai server with the
//! experimental filters discovered from this workspace.
//!
//! The build script (`build.rs`) generates `register_external_filters`, included
//! below. `main` follows praxis-ai's own bin flow: resolve and load config,
//! initialise tracing, build the full filter registry (built-ins + praxis-ai
//! filters + this workspace's discovered filters), then hand it to
//! [`praxis_ai::run_server_with_registry`].

use praxis_core::{
    config::Config,
    subrequest::{SubRequestClient, SubRequestConnector},
};

// Provides: fn register_external_filters(registry: &mut praxis_filter::FilterRegistry)
include!(concat!(env!("OUT_DIR"), "/external_filters.rs"));

/// Builds a [`SubRequestClient`] from runtime config, mirroring praxis-ai's
/// `create_subrequest_client` so callout behaviour matches the stock server.
fn create_subrequest_client(config: &Config) -> SubRequestClient {
    let pool_size = config
        .runtime
        .subrequest_pool_size
        .unwrap_or(praxis_core::config::DEFAULT_SUBREQUEST_POOL_SIZE);
    let connector = SubRequestConnector::new(pool_size, config.runtime.subrequest_max_connections);
    let response_ceiling = config.body_limits.max_response_bytes.unwrap_or(usize::MAX);
    SubRequestClient::with_max_response_bytes(connector, response_ceiling)
}

/// Loads config, initialises tracing, composes the registry, and runs the server.
///
/// # Errors
///
/// Returns an error if config loading or tracing initialisation fails. On
/// success it does not return: [`praxis_ai::run_server_with_registry`] blocks
/// for the lifetime of the process.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // This thin POC bin does not parse CLI args; it uses the default config
    // location. Mirror praxis-ai's own bin: pass the same explicit value (here
    // `None`) to both helpers rather than feeding the resolved path back in.
    let config_path = praxis_ai::resolve_config_path(None);
    let config = praxis_ai::load_config(None)?;

    // Hold the tracing guard for the lifetime of the process.
    let _tracing_guard = praxis_ai::init_tracing(&config)?;

    let subrequest_client = create_subrequest_client(&config);
    let mut registry = praxis_ai::build_full_registry(&subrequest_client);
    register_external_filters(&mut registry);

    praxis_ai::run_server_with_registry(config, registry, config_path);
}
