#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlanFact {
    pub(crate) generation: u64,
    pub(crate) start: u64,
    pub(crate) identity_matches: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeadlineFact {
    pub(crate) frame: u64,
    pub(crate) fallback_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TickDecision {
    Wait,
    Drop,
    Apply,
    Replan,
    DeadlineFallback,
    LateFallback,
}

pub(crate) fn reply_is_current(
    generation: u64,
    current_generation: u64,
    identity_matches: bool,
) -> bool {
    generation == current_generation && identity_matches
}

pub(crate) fn tick_decision(
    current_generation: u64,
    active_playhead: Option<u64>,
    plan: Option<PlanFact>,
    deadline: Option<DeadlineFact>,
) -> TickDecision {
    let exact_ready = matches!(
        (active_playhead, plan),
        (Some(playhead), Some(plan))
            if playhead == plan.start
                && reply_is_current(plan.generation, current_generation, plan.identity_matches)
    );

    if let (Some(playhead), Some(deadline)) = (active_playhead, deadline) {
        if deadline.fallback_available && playhead >= deadline.frame && !exact_ready {
            return TickDecision::DeadlineFallback;
        }
    }

    let Some(plan) = plan else {
        return TickDecision::Wait;
    };
    if !reply_is_current(plan.generation, current_generation, plan.identity_matches) {
        return TickDecision::Drop;
    }
    let Some(playhead) = active_playhead else {
        return TickDecision::Drop;
    };
    if playhead > plan.start {
        if deadline.is_some_and(|deadline| playhead < deadline.frame) {
            return TickDecision::Replan;
        }
        return TickDecision::LateFallback;
    }
    if playhead == plan.start {
        return TickDecision::Apply;
    }
    TickDecision::Wait
}
