#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailedPermit {
    Untransferred,
    TransferredToPreview,
}

pub(crate) fn release_failed_permit(permit: FailedPermit) -> bool {
    permit == FailedPermit::Untransferred
}

pub(crate) const fn upgrade_allowed(
    same_occurrence: bool,
    target_preview: bool,
    target_head_only: bool,
    target_failed: bool,
    incoming_preview: bool,
) -> bool {
    same_occurrence && target_preview && !target_head_only && !target_failed && !incoming_preview
}


pub(crate) const fn waiting_output_frames(remaining: usize, finished: bool) -> usize {
    if finished { 0 } else { remaining }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviewWait {
    Idle,
    EndDeck,
    Yield,
    Poll,
}

pub(crate) const fn preview_wait(
    awaiting: bool,
    stopped: bool,
    finished: bool,
    failed: bool,
    loader_exhausted: bool,
    replay_pending: bool,
    blocking: bool,
) -> PreviewWait {
    if !awaiting || stopped || finished {
        PreviewWait::Idle
    } else if failed || (loader_exhausted && !replay_pending) {
        PreviewWait::EndDeck
    } else if !blocking || replay_pending {
        PreviewWait::Yield
    } else {
        PreviewWait::Poll
    }
}
