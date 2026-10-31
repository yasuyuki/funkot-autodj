#[path = "../funkot-core/src/manual_navigation.rs"]
mod manual_navigation;

use self::manual_navigation::{
    reply_is_current, tick_decision, DeadlineFact, PlanFact, TickDecision,
};

#[kani::proof]
fn reply_is_current_all_u64_bool() {
    let generation: u64 = kani::any();
    let current_generation: u64 = kani::any();
    let identity_matches: bool = kani::any();
    let current = reply_is_current(generation, current_generation, identity_matches);

    assert_eq!(current, generation == current_generation && identity_matches);

    kani::cover!(current);
    kani::cover!(generation != current_generation && !current);
    kani::cover!(generation == current_generation && !identity_matches && !current);
}

#[kani::proof]
fn tick_decision_all_facts() {
    let current_generation: u64 = kani::any();
    let active_playhead = if kani::any::<bool>() {
        Some(kani::any::<u64>())
    } else {
        None
    };
    let plan = if kani::any::<bool>() {
        Some(PlanFact {
            generation: kani::any(),
            start: kani::any(),
            identity_matches: kani::any(),
        })
    } else {
        None
    };
    let deadline = if kani::any::<bool>() {
        Some(DeadlineFact {
            frame: kani::any(),
            fallback_available: kani::any(),
        })
    } else {
        None
    };
    let decision = tick_decision(current_generation, active_playhead, plan, deadline);

    let exact_ready = matches!(
        (active_playhead, plan),
        (Some(playhead), Some(plan))
            if playhead == plan.start
                && reply_is_current(plan.generation, current_generation, plan.identity_matches)
    );
    if exact_ready {
        assert_eq!(decision, TickDecision::Apply);
    }
    if let (Some(playhead), Some(plan), Some(deadline)) = (active_playhead, plan, deadline) {
        if reply_is_current(plan.generation, current_generation, plan.identity_matches)
            && playhead > plan.start && playhead < deadline.frame
        {
            assert_eq!(decision, TickDecision::Replan);
        }
    }
    if let (Some(playhead), Some(deadline)) = (active_playhead, deadline) {
        if deadline.fallback_available && playhead >= deadline.frame && !exact_ready {
            assert_eq!(decision, TickDecision::DeadlineFallback);
        }
    }
    if decision == TickDecision::Apply {
        let (Some(playhead), Some(plan)) = (active_playhead, plan) else {
            panic!("apply requires an active plan");
        };
        assert!(reply_is_current(
            plan.generation,
            current_generation,
            plan.identity_matches,
        ));
        assert_eq!(playhead, plan.start);
    }
    if decision == TickDecision::Replan {
        let (Some(playhead), Some(plan), Some(deadline)) = (active_playhead, plan, deadline) else {
            panic!("replan requires active plan and deadline");
        };
        assert!(reply_is_current(
            plan.generation,
            current_generation,
            plan.identity_matches,
        ));
        assert!(playhead > plan.start && playhead < deadline.frame);
    }
    if decision == TickDecision::LateFallback {
        let (Some(playhead), Some(plan)) = (active_playhead, plan) else {
            panic!("late fallback requires active plan");
        };
        assert!(reply_is_current(
            plan.generation,
            current_generation,
            plan.identity_matches,
        ));
        assert!(playhead > plan.start);
    }
    if decision == TickDecision::Drop {
        let Some(plan) = plan else {
            panic!("drop requires a plan");
        };
        assert!(
            !reply_is_current(
                plan.generation,
                current_generation,
                plan.identity_matches,
            ) || active_playhead.is_none()
        );
    }

    kani::cover!(decision == TickDecision::Wait);
    kani::cover!(decision == TickDecision::Drop);
    kani::cover!(decision == TickDecision::Apply);
    kani::cover!(decision == TickDecision::Replan);
    kani::cover!(decision == TickDecision::DeadlineFallback);
    kani::cover!(decision == TickDecision::LateFallback);
    kani::cover!(matches!(
        (active_playhead, plan, deadline),
        (Some(playhead), Some(plan), Some(deadline))
            if deadline.fallback_available
                && playhead == plan.start
                && playhead == deadline.frame
                && reply_is_current(plan.generation, current_generation, plan.identity_matches)
                && decision == TickDecision::Apply
    ));
    kani::cover!(matches!(
        (active_playhead, plan, deadline),
        (Some(playhead), Some(plan), Some(deadline))
            if deadline.fallback_available
                && playhead >= deadline.frame
                && !reply_is_current(plan.generation, current_generation, plan.identity_matches)
                && decision == TickDecision::DeadlineFallback
    ));
    kani::cover!(matches!(
        (active_playhead, plan),
        (None, Some(plan))
            if reply_is_current(plan.generation, current_generation, plan.identity_matches)
                && decision == TickDecision::Drop
    ));
    kani::cover!(matches!(
        (active_playhead, plan, deadline),
        (Some(_), Some(plan), None)
            if !reply_is_current(plan.generation, current_generation, plan.identity_matches)
                && decision == TickDecision::Drop
    ));
}
