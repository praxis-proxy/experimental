//! Judge-failure disposition: reuse, default Strong, reject, or unrouted.

use super::config::{FailureMode, Tier};

/// Metadata / log token for a live judge success.
pub(crate) const DECISION_ROUTED: &str = "routed";

/// Metadata / log token when `on_failure: closed` rejects the request.
pub(crate) const DECISION_REJECTED: &str = "rejected";

/// Metadata / log token when fail-open cannot apply a tier.
pub(crate) const DECISION_UNROUTED: &str = "unrouted";

/// Metadata / log token when the session floor is already at max (judge skipped).
pub(crate) const DECISION_FLOOR_SKIP: &str = "floor_skip";

/// How to finish a request after the judge (or decode) failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureAction {
    /// HTTP 503; ignore any remembered tier.
    Reject,
    /// Continue without rewriting `model` or selecting a Switchyard cluster.
    Unrouted,
    /// Rewrite to a tier and continue (`open` only).
    Apply(FailureApply),
}

/// Tier applied on the `open` failure path, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FailureApply {
    /// Weak or Strong to rewrite onto the request.
    pub(crate) tier: Tier,
    /// Distinguishes reuse of a real success from default Strong.
    pub(crate) kind: FailureApplyKind,
}

/// Why an `open` failure still selected a tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureApplyKind {
    /// Last successful judge verdict for this session key.
    Reuse,
    /// No remembered success: serve Strong and do not write the map.
    DefaultStrong,
}

impl FailureApplyKind {
    /// Value stored in `switchyard_route.decision`.
    pub(crate) fn metadata(self) -> &'static str {
        match self {
            Self::Reuse => "reuse",
            Self::DefaultStrong => "default_strong",
        }
    }
}

/// Chooses reject / unrouted / apply from `on_failure` and map lookup.
pub(crate) fn failure_action(mode: FailureMode, may_apply: bool, remembered: Option<Tier>) -> FailureAction {
    if may_apply {
        live_chat_failure(mode, remembered)
    } else {
        closed_or_unrouted(mode)
    }
}

/// `on_failure` when the body is a chat request we could rewrite.
fn live_chat_failure(mode: FailureMode, remembered: Option<Tier>) -> FailureAction {
    match mode {
        FailureMode::Closed => FailureAction::Reject,
        FailureMode::Open => FailureAction::Apply(open_apply(remembered)),
    }
}

/// Reuse a stored success, or default Strong without recording it.
fn open_apply(remembered: Option<Tier>) -> FailureApply {
    match remembered {
        Some(tier) => FailureApply {
            tier,
            kind: FailureApplyKind::Reuse,
        },
        None => FailureApply {
            tier: Tier::Strong,
            kind: FailureApplyKind::DefaultStrong,
        },
    }
}

/// `on_failure` when we must not rewrite (bad JSON, wrong path, …).
fn closed_or_unrouted(mode: FailureMode) -> FailureAction {
    match mode {
        FailureMode::Closed => FailureAction::Reject,
        FailureMode::Open => FailureAction::Unrouted,
    }
}

#[cfg(test)]
#[expect(clippy::allow_attributes, reason = "blanket test-module suppressions")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    reason = "unwrap/expect/panic/length are acceptable in tests"
)]
mod tests {
    use super::{FailureAction, FailureApply, FailureApplyKind, failure_action};
    use crate::switchyard_route::config::{FailureMode, Tier};

    #[test]
    fn closed_always_rejects_even_with_remembered_tier() {
        let action = failure_action(FailureMode::Closed, true, Some(Tier::Weak));
        assert_eq!(action, FailureAction::Reject, "closed must ignore the session map");
    }

    #[test]
    fn closed_rejects_when_rewrite_is_not_allowed() {
        assert_eq!(
            failure_action(FailureMode::Closed, false, None),
            FailureAction::Reject,
            "closed still 503s on unroutable bodies"
        );
    }

    #[test]
    fn open_reuses_remembered_weak_or_strong() {
        let weak = failure_action(FailureMode::Open, true, Some(Tier::Weak));
        assert_eq!(
            weak,
            FailureAction::Apply(FailureApply {
                tier: Tier::Weak,
                kind: FailureApplyKind::Reuse,
            }),
            "open + map hit must reuse Weak"
        );
        let strong = failure_action(FailureMode::Open, true, Some(Tier::Strong));
        assert_eq!(
            strong,
            FailureAction::Apply(FailureApply {
                tier: Tier::Strong,
                kind: FailureApplyKind::Reuse,
            }),
            "open + map hit must reuse Strong"
        );
    }

    #[test]
    fn open_defaults_strong_when_map_misses() {
        let action = failure_action(FailureMode::Open, true, None);
        assert_eq!(
            action,
            FailureAction::Apply(FailureApply {
                tier: Tier::Strong,
                kind: FailureApplyKind::DefaultStrong,
            }),
            "empty store under open must default Strong"
        );
    }

    #[test]
    fn open_stays_unrouted_when_rewrite_is_not_allowed() {
        assert_eq!(
            failure_action(FailureMode::Open, false, Some(Tier::Strong)),
            FailureAction::Unrouted,
            "wrong path / bad JSON must not apply a sticky tier"
        );
    }

    #[test]
    fn apply_kind_metadata_tokens_are_stable() {
        assert_eq!(FailureApplyKind::Reuse.metadata(), "reuse");
        assert_eq!(FailureApplyKind::DefaultStrong.metadata(), "default_strong");
    }
}
