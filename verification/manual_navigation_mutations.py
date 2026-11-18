NAVIGATION_MUTATIONS = (
    ("nav_generation", "manual_navigation_proofs::reply_is_current_all_u64_bool",
     (("generation == current_generation && identity_matches", "identity_matches"),),
     "consecutive_restarts_drop_a_delayed_reply_before_the_current_reply"),
    ("nav_identity", "manual_navigation_proofs::reply_is_current_all_u64_bool",
     (("generation == current_generation && identity_matches", "generation == current_generation"),),
     "manual_reply_identity_guard_rejects_replaced_pcm_without_cancellation"),
    ("nav_deadline", "manual_navigation_proofs::tick_decision_all_facts",
     (("deadline.fallback_available && playhead >= deadline.frame && !exact_ready", "false"),),
     "deadline_beats_ready_reply_once_and_records_simple_plan"),
    ("nav_start_frame", "manual_navigation_proofs::tick_decision_all_facts",
     (("if playhead > plan.start {", "if false {"),
      ("if playhead == plan.start {", "if playhead >= plan.start {")),
     "late_manual_reply_replans_without_an_intermediate_transition"),
)

ENGINE_MUTATION_EDITS = {
    "nav_identity": (("funkot-core/src/engine.rs",
                      "key.active_path == active.track.path",
                      "true || key.active_path == active.track.path"),),
}
