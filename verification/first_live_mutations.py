FIRST_LIVE_MUTATIONS = (
    ("first_live_occurrence", "first_live_proofs::upgrade_admission_all_facts",
     (("same_occurrence && target_preview", "true && target_preview"),),
     "upgrade_ignores_wrong_occurrence_identity"),
    ("first_live_stage", "first_live_proofs::upgrade_admission_all_facts",
     (("same_occurrence && target_preview", "same_occurrence && true"),),
     "duplicate_full_upgrade_keeps_the_first_full_pcm_and_preview_fade"),
    ("first_live_terminal", "first_live_proofs::upgrade_admission_all_facts",
     (("!target_failed", "true"),),
     "late_upgrade_after_final_preview_failure_does_not_revive_the_terminal_occurrence"),
    ("first_live_failed_permit", "first_live_proofs::failure_permit_transfer_all_variants",
     (("permit == FailedPermit::Untransferred", "true"),),
     "revoked_queued_preview_then_final_failure_releases_only_one_permit"),
    ("first_live_stop", "first_live_proofs::preview_wait_all_facts",
     (("!awaiting || stopped || finished", "!awaiting || finished"),), None),
    ("first_live_realtime", "first_live_proofs::preview_wait_all_facts",
     (("!blocking || replay_pending", "replay_pending"),), None),
)

# Receiver replacement is a channel ownership adapter, outside the pure Kani facts.
FIRST_LIVE_ENGINE_MUTATIONS = (
    ("first_live_receiver", None, (),
     "future_commit_retires_prepared_preparing_and_old_messages_and_permits"),
    ("first_live_producer", None, (),
     "prepare_first_live_reports_preview_permit_transfer_only_after_ready"),
)
ENGINE_MUTATION_EDITS = {
    "first_live_receiver": (("funkot-core/src/engine.rs",
                             "_rx: std::mem::replace(&mut self.loader_rx, rx)",
                             "_rx: rx"),),
    "first_live_producer": (("funkot-core/src/engine.rs",
                             "post_preview_failure = Some(failed_attempt);",
                             "post_preview_failure = None;"),),
}
