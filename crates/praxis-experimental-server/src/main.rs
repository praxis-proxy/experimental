//! Thin server binary that composes the stock praxis-ai server with the
//! experimental filters discovered from this workspace.
//!
//! The build script (`build.rs`) generates `register_external_filters`, included
//! below. `main` follows praxis-ai's own bin flow: resolve and load config,
//! initialise tracing, build the full filter registry (built-ins + praxis-ai
//! filters + this workspace's discovered filters), then hand it to
//! [`praxis_ai::run_server_with_registry`].

use clap::Parser;
use praxis_core::{
    config::Config,
    subrequest::{SubRequestClient, SubRequestConnector},
};

// Provides: fn register_external_filters(registry: &mut praxis_filter::FilterRegistry)
include!(concat!(env!("OUT_DIR"), "/external_filters.rs"));

/// Environment variable consulted when `--config` is not given, matching
/// praxis-ai's own bin.
const CONFIG_ENV_VAR: &str = "PRAXIS_CONFIG";

/// Experimental Praxis AI gateway.
///
/// Mirrors the `--config` surface of praxis-ai's own bin. The validate and dump
/// subcommands are deliberately not mirrored: they live in praxis-ai's private
/// `commands` module and are not reachable from a downstream binary.
#[derive(Parser, Debug)]
#[command(name = "praxis-experimental-server", version, about)]
struct Cli {
    /// Path to the YAML configuration file.
    ///
    /// Falls back to `$PRAXIS_CONFIG`, then to `praxis.yaml` in the working
    /// directory. The container image sets that directory to `/etc/praxis`.
    #[arg(short = 'c', long = "config")]
    config: Option<String>,
}

impl Cli {
    /// Resolves the explicit config path from the flag, then the environment.
    ///
    /// `env_value` is passed in rather than read here so the precedence rule is
    /// testable without mutating process-wide state, which this crate cannot do
    /// anyway: the workspace sets `unsafe_code = "forbid"`.
    fn explicit_config_from(&self, env_value: Option<String>) -> Option<String> {
        // Each candidate is checked independently. Filtering after `or` would let
        // a blank `--config ""` discard a perfectly good `$PRAXIS_CONFIG` and then
        // vanish itself, silently starting the gateway on built-in defaults.
        let non_blank = |value: &String| !value.trim().is_empty();
        self.config
            .clone()
            .filter(non_blank)
            .or_else(|| env_value.filter(non_blank))
    }

    /// Resolves the explicit config path, consulting the real environment.
    fn explicit_config(&self) -> Option<String> {
        self.explicit_config_from(std::env::var(CONFIG_ENV_VAR).ok())
    }
}

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
/// Returns an error if config loading or tracing initialisation fails. Loading
/// an explicitly requested path that does not exist is an error; falling back to
/// the built-in defaults when no path was requested is not, so that case is
/// reported through tracing instead.
///
/// On success this does not return: [`praxis_ai::run_server_with_registry`]
/// blocks for the lifetime of the process.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let explicit = cli.explicit_config();

    // Pass the same explicit value to both helpers rather than feeding the
    // resolved path back in, mirroring praxis-ai's own bin.
    let config_path = praxis_ai::resolve_config_path(explicit.as_deref());
    let config = praxis_ai::load_config(explicit.as_deref())?;

    // Hold the tracing guard for the lifetime of the process.
    let _tracing_guard = praxis_ai::init_tracing(&config)?;

    // Without this the server starts on built-in defaults -- loopback-only
    // listeners and no filters -- which looks like a broken deployment rather
    // than a missing file.
    if config_path.is_none() {
        tracing::warn!(
            env_var = CONFIG_ENV_VAR,
            "no configuration file found; starting with built-in defaults"
        );
    }

    let subrequest_client = create_subrequest_client(&config);
    let mut registry = praxis_ai::build_full_registry(&subrequest_client);
    register_external_filters(&mut registry);

    praxis_ai::run_server_with_registry(config, registry, config_path);
}

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test-module suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "unwrap/expect/panic are acceptable in tests"
)]
mod tests {
    use clap::{CommandFactory as _, Parser as _};

    use super::{CONFIG_ENV_VAR, Cli};

    /// Guards against a malformed `#[command]`/`#[arg]` definition, which clap
    /// only detects when the command is built.
    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn config_defaults_to_none_without_arguments() {
        let cli = Cli::try_parse_from(["praxis-experimental-server"]).expect("no arguments should parse");
        assert_eq!(cli.config, None, "config should be unset by default");
    }

    #[test]
    fn short_and_long_config_flags_are_equivalent() {
        let short =
            Cli::try_parse_from(["praxis-experimental-server", "-c", "/etc/praxis/a.yaml"]).expect("-c should parse");
        let long = Cli::try_parse_from(["praxis-experimental-server", "--config", "/etc/praxis/a.yaml"])
            .expect("--config should parse");
        assert_eq!(short.config, long.config, "-c and --config should agree");
        assert_eq!(short.config.as_deref(), Some("/etc/praxis/a.yaml"));
    }

    #[test]
    fn unknown_flags_are_rejected() {
        Cli::try_parse_from(["praxis-experimental-server", "--nope"]).expect_err("unknown flags should be rejected");
    }

    /// The flag wins over the environment, and both are preferred over the
    /// working-directory default that a `None` result selects.
    #[test]
    fn explicit_config_prefers_flag_over_environment() {
        let with_flag = Cli {
            config: Some("/from/flag.yaml".to_owned()),
        };
        assert_eq!(
            with_flag
                .explicit_config_from(Some("/from/env.yaml".to_owned()))
                .as_deref(),
            Some("/from/flag.yaml"),
            "the flag should win over the environment"
        );

        let without_flag = Cli { config: None };
        assert_eq!(
            without_flag
                .explicit_config_from(Some("/from/env.yaml".to_owned()))
                .as_deref(),
            Some("/from/env.yaml"),
            "the environment should be used when the flag is absent"
        );
    }

    /// A blank value from either source is treated as absent, independently, so
    /// an empty `--config ""` cannot discard a usable `$PRAXIS_CONFIG`.
    #[test]
    fn blank_values_fall_through_to_the_next_source() {
        let blank_flag = Cli {
            config: Some("  ".to_owned()),
        };
        assert_eq!(
            blank_flag
                .explicit_config_from(Some("/from/env.yaml".to_owned()))
                .as_deref(),
            Some("/from/env.yaml"),
            "a blank flag should fall through to the environment, not discard it"
        );

        let without_flag = Cli { config: None };
        assert_eq!(
            without_flag.explicit_config_from(Some("   ".to_owned())),
            None,
            "a blank environment value should fall through to the default path"
        );
        assert_eq!(
            without_flag.explicit_config_from(None),
            None,
            "an unset environment variable should fall through to the default path"
        );
        assert_eq!(
            blank_flag.explicit_config_from(None),
            None,
            "a blank flag with no environment value selects the default path"
        );
    }

    /// The environment variable name is part of the deployment contract, so a
    /// rename should break a test rather than a running gateway.
    #[test]
    fn config_env_var_name_is_stable() {
        assert_eq!(CONFIG_ENV_VAR, "PRAXIS_CONFIG");
    }
}
