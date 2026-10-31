----------------------------- MODULE FirstLive -----------------------------
EXTENDS Naturals, Integers, Sequences, FiniteSets, TLC

(* Bounded Engine projection; see verification/README.md for correspondence.
   Initial receiver uses occurrence key 1; its loop may reuse that key. The
   replacement uses key 2, satisfying replacement-source index uniqueness.
   The initial receiver has three preparation attempts with independently
   named PCM; the replacement has one fresh-key attempt. Each FIFO has capacity
   one. Three initial attempts exercise normal current/next/history capacity
   and surplus backpressure. A repeated replacement key would violate its
   lifetime-unique-index contract. Head-only retains the real 12-slot prefetch
   capacity; these inputs do NOT prove full head-queue occupancy.
   Exactly one optional receiver replacement is explored, without generation
   wraparound or reuse. Initial non-head preview exists at most once/Engine.
   Attempt numbers are ghost allocation names, NEVER Failed/Upgrade guards.
   Preview/full PCM names differ even when path and occurrence match.
   Accepted hard jumps represent navigation ownership boundaries; transition
   DSP, concurrent prev deck, restart aliases and scheduled replay are outside
   this model. Engine actions serialize; producer steps may interleave. *)

CONSTANTS None, Stable, Robust, FaultKind, CheckReceiver, CheckOccurrence, CheckUpgradeStage,
          EndFailedPreview, HoldFailedPermit, KeepTerminal
ASSUME FaultKind \in {"None","ReadyDuplicate","FailureDuplicate","EarlyExhausted"}
VARIABLE s
vars == <<s>>
Gens == 0..1
Track(a,k) == [allocation |-> a, stage |-> k,
                index |-> IF a<=3 THEN 1 ELSE 2, path |-> "Same"]
Preview == Track(1,"Preview")
Full == Track(1,"Full")
Kinds == {"Preview","Full","Head"}
OtherPath == [Full EXCEPT !.path="Other"]
Tracks == {Track(a,k): a \in {1,2,3,5}, k \in Kinds} \cup {OtherPath}
SameOccurrence(t,u) == t.index=u.index /\ t.path=u.path
Pcm(t) == <<t.allocation,t.stage>>
NonHeadPreview(t) == IF t = None THEN FALSE ELSE t.stage = "Preview"
Frames(t) == IF t.stage = "Full" THEN 2 ELSE 1
Ticket(g,n) == 4*g+n
Depth == IF s.head THEN 12 ELSE 0
Capacity == 3+Depth
Inc(n) == IF n < Capacity THEN n+1 ELSE n
Failed(t) == t # None /\ s.failedPcm = Pcm(t)
OpenEngine == ~s.stopped /\ ~s.blocked
CanControl == OpenEngine /\ ~Stable

Msg(k,t,a,transferred) == [kind |-> k, track |-> t, attempt |-> a, transferred |-> transferred,
                         previewPcm |-> IF transferred THEN Pcm(Preview) ELSE None]
EmptyMail == [g \in Gens |-> <<>>]
PushHistory(st,t) ==
 LET release == st.history # None \/ ~st.armed
 IN [st EXCEPT !.history = t, !.armed = TRUE,
       !.credits[st.gen] = IF release THEN Inc(@) ELSE @]
Start(st,t) ==
 [st EXCEPT !.active = t, !.position = 0,
     !.credits[st.gen] = IF st.reserved THEN Inc(@) ELSE @,
     !.reserved = FALSE]
PopNext(st) ==
 [st EXCEPT !.next = IF Len(st.heads)=0 THEN None ELSE Head(st.heads),
     !.heads = IF Len(@)=0 THEN <<>> ELSE Tail(@)]

Init ==
 /\ s \in [head: BOOLEAN, realtime: BOOLEAN, firstPreview: BOOLEAN,
            gen: {0}, phase: [{0,1} -> {"Acquire"}],
            number: [{0,1} -> {1}], mail: {EmptyMail},
            credits: [{0,1} -> 0..15], armed: {FALSE}, reserved: {FALSE},
            active: {None}, next: {None}, heads: {<<>>}, history: {None},
            position: {0}, fence: {FALSE}, stopped: {FALSE}, finished: {FALSE},
            blocked: {FALSE}, exhausted: {FALSE}, failedPcm: {None},
            terminalWitness: {FALSE}, upgrades: {0},
            receiverSafe: {TRUE}, upgradeSafe: {TRUE}, permitSafe: {TRUE},
            playheadSafe: {TRUE}, injected: {FALSE}, dropped: {{}},
            echo: {None}, transportInjected: {FALSE}]
 /\ s.credits[0] = 2 + Depth
 /\ s.credits[1] = 0
 /\ s.head => ~s.firstPreview

Acquire ==
 /\ ~s.stopped
 /\ s.phase[s.gen] = "Acquire"
 /\ s.credits[s.gen] > 0
 /\ s' = [s EXCEPT !.phase[s.gen]="Preparing", !.credits[s.gen]=@-1]

SendPrepared ==
 /\ ~s.stopped /\ s.phase[s.gen]="Preparing" /\ s.mail[s.gen]= <<>>
 /\ LET n == s.number[s.gen]
        a == Ticket(s.gen,n)
        preview == s.gen=0 /\ n=1 /\ s.firstPreview
        kind == IF preview THEN "Preview" ELSE IF s.head THEN "Head" ELSE "Full"
        exhaustedAt == IF s.gen=0 THEN 4 ELSE 2
    IN IF n=exhaustedAt
       THEN s'=[s EXCEPT !.mail[s.gen]= <<Msg("Exhausted",None,a,FALSE)>>,
                        !.phase[s.gen]="Done"]
       ELSE \/ s'=[s EXCEPT !.mail[s.gen]= <<Msg("Ready",Track(a,kind),a,FALSE)>>,
                            !.phase[s.gen]=IF preview THEN "Final" ELSE "Acquire",
                            !.number[s.gen]=IF preview THEN @ ELSE @+1]
            \/ s'=[s EXCEPT !.mail[s.gen]= <<Msg("Failed",None,a,FALSE)>>,
                            !.phase[s.gen]="Acquire", !.number[s.gen]=@+1]

SendFinal ==
 /\ ~s.stopped /\ s.gen=0 /\ s.phase[0]="Final" /\ s.mail[0]= <<>>
 /\ \E outcome \in {"Upgrade","Failed"}:
      s'=[s EXCEPT !.mail[0]= <<Msg(outcome,IF outcome="Upgrade" THEN Full ELSE None,1,TRUE)>>,
                   !.phase[0]="Acquire", !.number[0]=2]
Producer == Acquire \/ SendPrepared \/ SendFinal

(* Upgrade messages carry path/index and a PreparedTrack, not generation.
   Receiver selection is the generation isolation. The all-same-path legal
   input retains the independent occurrence check; robust foreign input uses
   the other key. No ghost attempt equality participates in the guard. *)
UpgradeMatches(t,incoming) ==
 IF t=None THEN FALSE
 ELSE /\ (~CheckOccurrence \/ SameOccurrence(t,incoming))
      /\ (~CheckUpgradeStage \/
           (NonHeadPreview(t) /\ ~Failed(t) /\ incoming.stage="Full"))
UpgradeTarget(incoming) ==
 IF UpgradeMatches(s.active,incoming) THEN "Active"
 ELSE IF UpgradeMatches(s.next,incoming) THEN "Next" ELSE "Ignore"
CorrectUpgrade(t,incoming) ==
 /\ NonHeadPreview(t) /\ ~Failed(t) /\ incoming.stage="Full"
 /\ SameOccurrence(t,incoming)
ApplyUpgrade(st,incoming) ==
 LET target == UpgradeTarget(incoming)
     old == IF target="Active" THEN s.active ELSE s.next
     legal == IF target="Ignore" THEN TRUE ELSE CorrectUpgrade(old,incoming)
     count == IF s.upgrades=2 THEN 2 ELSE s.upgrades+1
 IN IF target="Ignore" THEN st
    ELSE IF target="Active"
    THEN [st EXCEPT !.active=incoming,
          !.position=IF @<=Frames(incoming) THEN @ ELSE Frames(incoming),
          !.upgradeSafe=@ /\ legal,
          !.playheadSafe=@ /\ (s.position<=Frames(incoming)), !.upgrades=count]
    ELSE [st EXCEPT !.next=incoming, !.upgradeSafe=@ /\ legal, !.upgrades=count]


Consume(g) ==
 /\ ~s.stopped /\ ~s.fence /\ s.mail[g] # <<>>
 /\ (g=s.gen \/ ~CheckReceiver)
 /\ LET m == Head(s.mail[g])
        base == [s EXCEPT !.mail[g]=Tail(@), !.receiverSafe=@ /\ g=s.gen,
                 !.echo=IF (FaultKind="ReadyDuplicate" /\ m.kind="Ready")
                           \/ (FaultKind="FailureDuplicate" /\ m.kind="Failed" /\ ~m.transferred)
                        THEN m ELSE @]
    IN CASE m.kind="Ready" ->
         IF s.next=None THEN s'=[base EXCEPT !.next=m.track]
         ELSE IF Len(s.heads)<Depth THEN s'=[base EXCEPT !.heads=Append(@,m.track)]
         ELSE s'=[base EXCEPT !.dropped=@ \cup {m.attempt}]
       [] m.kind="Upgrade" -> s'=ApplyUpgrade(base,m.track)
       [] m.kind="Failed" ->
         IF ~m.transferred THEN s'=[base EXCEPT !.credits[s.gen]=Inc(@)]
         ELSE LET result == [base EXCEPT
                    !.failedPcm=m.previewPcm, !.terminalWitness=TRUE,
                    !.credits[s.gen]=IF HoldFailedPermit THEN @ ELSE Inc(@)]
              IN s'=[result EXCEPT !.permitSafe=@ /\
                         result.credits[s.gen]=s.credits[s.gen]]
       [] m.kind="Exhausted" -> s'=[base EXCEPT !.exhausted=TRUE]
DrainCurrent == Consume(s.gen)
Drain == \E g \in Gens: Consume(g)

(* One additional Upgrade is an explicit robustness input, outside the legal
   producer language. Its payload may duplicate, precede Ready, arrive after
   failure/adoption, use another occurrence/path, or itself be preview/head.
   A single injected copy suffices to exercise duplicate adoption alongside
   the genuine Upgrade. It does not acquire a permit. *)
Inject(incoming) ==
 /\ Robust /\ ~s.stopped /\ ~s.injected /\ s.mail[s.gen]= <<>>
 /\ s'=[s EXCEPT !.injected=TRUE,
        !.mail[s.gen]= <<Msg("Upgrade",incoming,1,FALSE)>>]

(* These three diagnostics intentionally violate the real producer contract.
   They do not promise that Engine tolerates a forged extra permit owner,
   duplicate untransferred failure or end-of-source ahead of remaining input.
   The echo is recorded only AFTER a real message was consumed. Normal configs
   keep it None, so it adds no behavioral state to their exploration. *)
InjectTransport ==
 /\ FaultKind # "None" /\ ~s.stopped /\ ~s.transportInjected
 /\ s.mail[s.gen]= <<>>
 /\ IF FaultKind="EarlyExhausted"
    THEN /\ s.phase[s.gen] # "Done"
         /\ s'=[s EXCEPT !.transportInjected=TRUE,
                        !.mail[s.gen]= <<Msg("Exhausted",None,4,FALSE)>>]
    ELSE /\ s.echo # None
         /\ s'=[s EXCEPT !.transportInjected=TRUE, !.mail[s.gen]= <<s.echo>>]

Retire(st) == PushHistory([st EXCEPT !.active=None, !.blocked=FALSE],s.active)
EndPreview(st) ==
 LET retired == Retire(st)
 IN IF st.next # None THEN Start(PopNext(retired),st.next)
    ELSE [retired EXCEPT !.finished=st.exhausted]
JumpNext(st) ==
 LET moved == IF st.active=None THEN st ELSE PushHistory(st,st.active)
 IN Start(PopNext(moved),st.next)

Render ==
 /\ ~s.stopped /\ ~s.finished /\ s.mail[s.gen]= <<>>
 /\ IF s.fence
       THEN /\ s.active # None /\ s.position<Frames(s.active)
            /\ s'=[s EXCEPT !.position=@+1]
    ELSE IF s.blocked /\ ~NonHeadPreview(s.active)
       THEN s'=[s EXCEPT !.blocked=FALSE]
    ELSE IF s.active=None
       THEN IF s.next # None THEN s'=Start(PopNext(s),s.next)
            ELSE /\ s.exhausted /\ s'=[s EXCEPT !.finished=TRUE]
    ELSE IF s.position<Frames(s.active)
       THEN s'=[s EXCEPT !.position=@+1]
    ELSE IF NonHeadPreview(s.active)
       THEN IF (EndFailedPreview /\ Failed(s.active)) \/ s.exhausted
            THEN s'=EndPreview(s)
            ELSE /\ ~s.realtime /\ ~s.blocked /\ s'=[s EXCEPT !.blocked=TRUE]
    ELSE IF s.active.stage="Head"
       THEN IF s.exhausted /\ s.next=None
            THEN s'=[Retire(s) EXCEPT !.finished=TRUE]
            ELSE IF s.next # None /\ s.next.stage="Full"
                 THEN s'=JumpNext(s)
                 ELSE s'=[s EXCEPT !.position=0]
    ELSE IF s.next # None THEN s'=JumpNext(s)
    ELSE s'=[Retire(s) EXCEPT !.finished=s.exhausted]

JumpForward ==
 /\ CanControl /\ ~s.fence /\ ~s.finished /\ s.next # None
 /\ s'=JumpNext(s)
JumpPrevious ==
 /\ CanControl /\ ~s.fence /\ ~s.finished /\ s.history # None
 /\ LET cleared == [s EXCEPT !.history=None]
        moved == IF s.active=None THEN cleared
                 ELSE IF s.next=None THEN [cleared EXCEPT !.next=s.active]
                 ELSE IF s.head THEN [cleared EXCEPT !.next=s.active, !.heads= <<s.next>> \o @]
                 ELSE [cleared EXCEPT !.next=s.active, !.credits[s.gen]=Inc(@)]
    IN s'=Start(moved,s.history)
Revoke ==
 /\ CanControl /\ ~s.fence /\ s.next # None
 /\ s'=[PopNext(s) EXCEPT !.credits[s.gen]=Inc(@)]
Stop ==
 /\ CanControl
 /\ s'=[s EXCEPT !.stopped=TRUE, !.finished=TRUE, !.blocked=FALSE,
                 !.mail=[g \in Gens |-> <<>>]]
Resume ==
 /\ CanControl /\ ~s.fence /\ s.finished
 /\ s'=[s EXCEPT !.finished=FALSE]
BeginFuture ==
 /\ CanControl /\ s.gen=0 /\ ~s.fence /\ ~NonHeadPreview(s.active)
 /\ (IF s.active=None THEN TRUE ELSE
       s.head \/ s.position<Frames(s.active) \/ (s.exhausted /\ s.next=None))
 /\ s'=[s EXCEPT !.fence=TRUE]
AbortFuture ==
 /\ CanControl /\ s.fence
 /\ s'=[s EXCEPT !.fence=FALSE]
CommitFuture ==
 /\ CanControl /\ s.fence /\ s.gen=0
 /\ s'=[s EXCEPT !.gen=1, !.fence=FALSE, !.next=None, !.heads= <<>>,
        !.credits[1]=1+Depth, !.armed=(s.history # None),
        !.reserved=(s.active=None), !.exhausted=FALSE,
        !.failedPcm=IF KeepTerminal THEN @ ELSE None]

(* Real render/stop terminal boundaries and preview wait polling may leave
   this frame/ownership abstraction unchanged. Keep only these justified
   self-loops so TLC's default deadlock check still detects other stuck states. *)
TerminalStutter ==
 /\ (s.stopped \/ (s.finished /\ s.phase[s.gen]="Done"))
 /\ UNCHANGED s
PreviewWaitStutter ==
 /\ ~s.stopped /\ ~s.finished /\ ~s.fence
 /\ NonHeadPreview(s.active) /\ s.position>=Frames(s.active)
 /\ ~(EndFailedPreview /\ Failed(s.active)) /\ ~s.exhausted
 /\ s.mail[s.gen]= <<>>
 /\ UNCHANGED s

Next == TerminalStutter \/ PreviewWaitStutter \/ Producer \/ Drain \/ InjectTransport \/ Render \/ JumpForward \/ JumpPrevious \/ Revoke
        \/ Stop \/ Resume \/ BeginFuture \/ AbortFuture \/ CommitFuture
        \/ (\E incoming \in {Full,Preview,Track(1,"Head"),Track(5,"Full"),OtherPath}: Inject(incoming))
SafetySpec == Init /\ [][Next]_vars
Spec == SafetySpec /\ WF_vars(Producer) /\ WF_vars(DrainCurrent) /\ WF_vars(Render)

TypeOK ==
 /\ s.gen \in Gens /\ s.head \in BOOLEAN /\ s.realtime \in BOOLEAN
 /\ s.firstPreview \in BOOLEAN
 /\ s.phase \in [Gens -> {"Acquire","Preparing","Final","Done"}]
 /\ s.number \in [Gens -> 1..4] /\ s.number[1] \in 1..2 /\ s.credits \in [Gens -> 0..15]
 /\ \A g \in Gens: Len(s.mail[g])<=1
 /\ s.active \in Tracks \cup {None} /\ s.next \in Tracks \cup {None}
 /\ s.history \in Tracks \cup {None}
 /\ \A i \in DOMAIN s.heads: s.heads[i] \in Tracks
 /\ Len(s.heads)<=Depth /\ s.position \in 0..2
 /\ s.upgrades \in 0..2 /\ s.failedPcm \in {None,Pcm(Preview)}
 /\ s.armed \in BOOLEAN /\ s.reserved \in BOOLEAN /\ s.fence \in BOOLEAN
 /\ s.stopped \in BOOLEAN /\ s.finished \in BOOLEAN /\ s.blocked \in BOOLEAN
 /\ s.exhausted \in BOOLEAN /\ s.terminalWitness \in BOOLEAN
 /\ s.injected \in BOOLEAN /\ s.transportInjected \in BOOLEAN
NoRetiredReceiverEffect == s.receiverSafe
UpgradeSafe == s.upgradeSafe
UpgradeAtMostOnce == s.upgrades<=1
PlayheadPreserved == s.playheadSafe
(* An AfterPreview failure never increases the current pool credit, even
   after revoke already returned that preview slot. This is an action-local
   credit property, not a proof of global permit conservation. *)
NoPrematurePermitRelease == s.permitSafe
(* The weak PCM allocation identity stays recorded across retirement, source
   commit and Prev. Native tests own Weak lifetime/address-reuse adapters. *)
TerminalIdentitySafe == s.terminalWitness => s.failedPcm=Pcm(Preview)
RealtimeNeverBlocks == s.realtime => ~s.blocked
StopIsTerminal == s.stopped => s.finished /\ ~s.blocked
HeadOnlyNeverWaits == s.head => ~s.blocked
QueueShape == Len(s.heads)>0 => s.next # None
ReceiverOwnsFirstLive == s.phase[1] # "Final"
PreviewEventuallyResolves ==
 [](s.active=Preview /\ s.position>=Frames(Preview)
       => <>(s.active # Preview \/ s.position<Frames(Preview) \/ s.stopped))
EventuallyFiniteSourceEnds == ~s.head => <>s.finished

(* Transport diagnostics observe real buffer slots/credits/finish, not the
   fault selector. They are checked separately from supported-protocol safety.
   NoPrematureFinish is scoped to Stable: no Stop or replacement can legitimately
   leave finished set while a different producer starts. *)
BufferedPcmUnique ==
 LET slots == <<s.active,s.next,s.history>> \o s.heads
     held == {i \in DOMAIN slots: slots[i] # None}
 IN Cardinality(held)=Cardinality({Pcm(slots[i]): i \in held})
PermitCreditBound == s.credits[s.gen] <= 2+Depth+(IF s.armed THEN 1 ELSE 0)
NoPrematureFinish == s.finished => s.phase[s.gen]="Done"
=============================================================================
