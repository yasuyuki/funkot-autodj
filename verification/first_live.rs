#[path = "../funkot-core/src/first_live.rs"]
mod first_live;

use self::first_live::{upgrade_allowed, preview_wait, PreviewWait, FailedPermit, release_failed_permit, waiting_output_frames};

#[kani::proof]
fn upgrade_admission_all_facts() {
    let same: bool = kani::any();
    let preview: bool = kani::any();
    let head_only: bool = kani::any();
    let failed: bool = kani::any();
    let incoming_preview: bool = kani::any();
    let apply = upgrade_allowed(same, preview, head_only, failed, incoming_preview);
    if apply {
        assert!(same && preview && !head_only && !failed && !incoming_preview);
    }
    if same && preview && !head_only && !failed && !incoming_preview {
        assert!(apply);
    }
    // A successful adoption changes preview to false. Repeating that Upgrade
    // cannot replace PCM or restart its fade, even with the same occurrence.
    if apply {
        assert!(!upgrade_allowed(same, false, head_only, failed, incoming_preview));
    }
    kani::cover!(apply);
    kani::cover!(!same && !apply);
    kani::cover!(head_only && !apply);
    kani::cover!(failed && !apply);
    kani::cover!(incoming_preview && !apply);
    kani::cover!(!preview && !apply);
}

#[kani::proof]
fn preview_wait_all_facts() {
    let awaiting: bool = kani::any();
    let stopped: bool = kani::any();
    let finished: bool = kani::any();
    let failed: bool = kani::any();
    let exhausted: bool = kani::any();
    let replay: bool = kani::any();
    let blocking: bool = kani::any();
    let decision = preview_wait(awaiting, stopped, finished, failed, exhausted, replay, blocking);
    if !awaiting || stopped || finished {
        assert_eq!(decision, PreviewWait::Idle);
    } else if failed || (exhausted && !replay) {
        assert_eq!(decision, PreviewWait::EndDeck);
    } else if !blocking || replay {
        assert_eq!(decision, PreviewWait::Yield);
    } else {
        assert_eq!(decision, PreviewWait::Poll);
    }
    if decision == PreviewWait::Poll {
        assert!(awaiting && !stopped && !finished && !failed && !exhausted && !replay && blocking);
    }
    // Realtime and staged replay never enter the sleeping poll path.
    if !blocking || replay {
        assert_ne!(decision, PreviewWait::Poll);
    }
    kani::cover!(decision == PreviewWait::Idle && !awaiting);
    kani::cover!(decision == PreviewWait::Idle && awaiting && stopped);
    kani::cover!(decision == PreviewWait::Idle && awaiting && finished && !stopped);
    kani::cover!(decision == PreviewWait::EndDeck && failed && !exhausted);
    kani::cover!(decision == PreviewWait::EndDeck && exhausted && !replay && !failed);
    kani::cover!(decision == PreviewWait::Yield && !blocking && !replay);
    kani::cover!(decision == PreviewWait::Yield && replay && blocking);
    kani::cover!(decision == PreviewWait::Poll);
}

#[kani::proof]
fn failure_permit_transfer_all_variants() {
    let transferred: bool = kani::any();
    let permit = if transferred { FailedPermit::TransferredToPreview } else { FailedPermit::Untransferred };
    let release = release_failed_permit(permit);
    // Ready already transferred this attempt's permit to its preview. Revoking
    // that preview returns the same permit; its final failure never returns it.
    assert_eq!(release, !transferred);
    kani::cover!(release);
    kani::cover!(!release);
}

#[kani::proof]
fn waiting_output_all_usize() {
    let remaining: usize = kani::any();
    let finished: bool = kani::any();
    let written = waiting_output_frames(remaining, finished);
    assert!(written <= remaining);
    assert_eq!(written == 0, finished || remaining == 0);
    if !finished {
        assert_eq!(written, remaining);
    }
    kani::cover!(finished && remaining > 0 && written == 0);
    kani::cover!(!finished && remaining == 0 && written == 0);
    kani::cover!(!finished && remaining > 0 && written > 0);
    kani::cover!(!finished && remaining == usize::MAX && written == usize::MAX);
}
