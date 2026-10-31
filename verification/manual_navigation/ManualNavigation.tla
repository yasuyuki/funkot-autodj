----------------------- MODULE ManualNavigation -----------------------
EXTENDS Naturals, Sequences, FiniteSets
CONSTANTS CheckGeneration, CheckIdentity, CheckTiming, CheckDeadline, CancelHistory, Stable
VARIABLES e, w, m, sent, used
vars == <<e, w, m, sent, used>>
Ops == 1..2
Actions == {"Next", "Prev", "Restart"}
Deck(occ, pcm, preview) == [occ |-> occ, pcm |-> pcm, preview |-> preview]
NoDeck == Deck(3,0,FALSE)
NoOverlap == [live |-> FALSE, outgoing |-> NoDeck]
NoSnapshot == [active |-> NoDeck, target |-> NoDeck, action |-> "None", epoch |-> 0]
NoPlan == [gen |-> 0, epoch |-> 0, action |-> "None", active |-> NoDeck,
           target |-> NoDeck, start |-> 0, admit |-> FALSE]
Target(action) == CASE action = "Next" -> e.next
                      [] action = "Prev" -> e.history
                      [] OTHER -> e.active
HasTarget(action) == CASE action = "Next" -> e.hasNext
                         [] action = "Prev" -> e.hasHistory
                         [] OTHER -> TRUE
KeyMatches(p) == HasTarget(p.action) /\ p.active = e.active /\ p.target = Target(p.action)
Current(p) == (CheckGeneration => p.gen = e.op) /\ (CheckIdentity => KeyMatches(p))
Exact == IF e.pending = NoPlan THEN FALSE
         ELSE Current(e.pending) /\ e.pending.start = e.now
MakePlan(start, admit) == [gen |-> e.op, epoch |-> e.epoch, action |-> e.action,
                          active |-> e.active, target |-> Target(e.action),
                          start |-> start, admit |-> admit]
Live(replies, op) == IF CheckGeneration THEN {msg \in replies : msg.plan.gen = op} ELSE replies
Rekey(op) == [w EXCEPT !.replies = Live(@,op),
                      !.stale = @ + Cardinality(w.replies \ Live(w.replies,op)),
                      !.attempts = IF op = 0 \/ op # e.op THEN 0 ELSE @]
Cancelled == [e EXCEPT !.op = 0, !.action = "None", !.deadline = 0,
                       !.demand = FALSE, !.pending = NoPlan]
Settle(nextEngine, nextMonitor) ==
    /\ e' = nextEngine /\ w' = Rekey(nextEngine.op)
    /\ m' = [nextMonitor EXCEPT !.accepted = IF nextEngine.op = 0 THEN NoSnapshot ELSE @]
    /\ UNCHANGED <<sent, used>>
Init == /\ e \in {[op |-> 0, action |-> "None", deadline |-> 0, now |-> 0,
                   active |-> Deck(2,0,ap), next |-> Deck(1,0,np), history |-> Deck(0,0,FALSE),
                   hasNext |-> TRUE, hasHistory |-> TRUE, epoch |-> 0,
                   overlap |-> overlap, fence |-> FALSE, stopped |-> FALSE,
                   demand |-> FALSE, pending |-> NoPlan, deferred |-> <<>>, readyEpochs |-> {}] :
                   ap \in BOOLEAN, np \in BOOLEAN,
                   overlap \in {NoOverlap, [live |-> TRUE, outgoing |-> Deck(1,0,FALSE)],
                                          [live |-> TRUE, outgoing |-> Deck(2,0,FALSE)]}}
        /\ w = [replies |-> {}, stale |-> 0, connected |-> TRUE, attempts |-> 0]
        /\ m = [accepted |-> NoSnapshot, adopted |-> [g \in Ops |-> 0], starts |-> 0,
                 generationSafe |-> TRUE, identitySafe |-> TRUE, epochSafe |-> TRUE,
                 frameSafe |-> TRUE, targetSafe |-> TRUE, fenceSafe |-> TRUE,
                 consumptionSafe |-> TRUE, nextConsumed |-> 0, historyConsumed |-> 0]
        /\ sent = 0 /\ used = 0
SendNav == /\ sent < 2 /\ ~e.stopped
           /\ \E action \in Actions: e' = [e EXCEPT !.deferred = Append(@,action)]
           /\ sent' = sent+1 /\ UNCHANGED <<w,m,used>>
DrainNav == /\ ~e.fence /\ ~e.stopped /\ Len(e.deferred) > 0
            /\ (~Stable \/ e.op = 0)
            /\ LET action == e.deferred[Len(e.deferred)] IN
                 IF ~HasTarget(action) \/ (action = "Next" /\ e.action = "Next")
                 THEN /\ e' = [e EXCEPT !.deferred = <<>>]
                      /\ UNCHANGED <<w,m,sent,used>>
                 ELSE /\ used < 2 /\ used' = used+1 /\ UNCHANGED sent
                      /\ \E deadline \in {e.now,2}:
                           e' = [e EXCEPT !.op = used+1, !.action = action,
                                 !.deadline = deadline, !.demand = TRUE,
                                 !.pending = NoPlan, !.deferred = <<>>]
                      /\ w' = [Rekey(used+1) EXCEPT !.attempts = 0]
                      /\ m' = [m EXCEPT !.accepted = [active |-> e.active, target |-> Target(action),
                                                      action |-> action, epoch |-> e.epoch]]
Dispatch == /\ e.op \in Ops /\ e.demand /\ ~e.fence /\ ~e.stopped
            /\ w.connected /\ w.attempts < 2 /\ HasTarget(e.action)
            /\ \E start \in e.now..2, admit \in (IF Stable THEN {TRUE} ELSE BOOLEAN), copies \in 0..2:
                 LET plan == MakePlan(start,admit) IN
                 w' = [w EXCEPT !.attempts = @+1,
                        !.replies = @ \cup {[plan |-> plan, copy |-> i, attempt |-> w.attempts+1] : i \in 1..copies}]
            /\ e' = [e EXCEPT !.demand = FALSE] /\ UNCHANGED <<m,sent,used>>
WorkerFailure == /\ w.connected
                 /\ w' = [w EXCEPT !.connected = FALSE]
                 /\ e' = [e EXCEPT !.demand = FALSE] /\ UNCHANGED <<m,sent,used>>
PollReply == /\ w.replies # {}
             /\ \E message \in w.replies:
                  /\ e' = IF Current(message.plan) THEN [e EXCEPT !.pending = message.plan] ELSE e
                  /\ w' = [w EXCEPT !.replies = @ \ {message}]
             /\ UNCHANGED <<m,sent,used>>
PollStale == /\ w.stale > 0 /\ w' = [w EXCEPT !.stale = @-1]
             /\ UNCHANGED <<e,m,sent,used>>
Apply(p, reply) ==
    LET nextEngine == [Cancelled EXCEPT
          !.active = IF p.admit THEN p.target ELSE @,
          !.now = IF p.admit THEN 0 ELSE @,
          !.next = IF p.admit /\ p.action = "Next" THEN NoDeck ELSE @,
          !.history = IF p.admit /\ p.action = "Prev" THEN NoDeck ELSE @,
          !.hasNext = IF p.admit /\ p.action = "Next" THEN FALSE ELSE @,
          !.hasHistory = IF p.admit /\ p.action = "Prev" THEN FALSE ELSE @,
          !.overlap = IF p.admit THEN [live |-> TRUE, outgoing |-> e.active] ELSE @]
        nextMonitor == IF ~p.admit THEN m ELSE
          [m EXCEPT !.adopted[p.gen] = @+1, !.starts = @+1,
           !.nextConsumed = IF p.action = "Next" THEN @+1 ELSE @,
           !.historyConsumed = IF p.action = "Prev" THEN @+1 ELSE @,
           !.generationSafe = @ /\ (p.gen = e.op),
           !.identitySafe = @ /\ KeyMatches(p) /\ (e.active = m.accepted.active),
           !.epochSafe = @ /\ (p.epoch = e.epoch) /\ (m.accepted.epoch = e.epoch),
           !.frameSafe = @ /\ (~reply \/ p.start = e.now),
           !.targetSafe = @ /\ (p.target = m.accepted.target),
           !.fenceSafe = @ /\ ~e.fence,
           !.consumptionSafe = @ /\ (p.action = m.accepted.action)]
    IN Settle(nextEngine,nextMonitor)
DeadlineFallback == /\ CheckDeadline /\ e.op \in Ops /\ ~e.fence /\ ~e.stopped
                    /\ e.now >= e.deadline /\ ~Exact
                    /\ \E admit \in (IF Stable THEN {TRUE} ELSE BOOLEAN):
                         Apply(MakePlan(e.now,admit /\ HasTarget(e.action)),FALSE)
TickReply == /\ e.pending # NoPlan /\ ~e.fence /\ ~e.stopped
             /\ ~(CheckDeadline /\ e.op \in Ops /\ e.now >= e.deadline /\ ~Exact)
             /\ LET p == e.pending IN
                 IF ~Current(p) THEN Settle([e EXCEPT !.pending = NoPlan],m)
                 ELSE IF ~CheckTiming \/ e.now = p.start THEN Apply(p,TRUE)
                 ELSE IF e.now > p.start THEN
                      IF e.now < e.deadline THEN Settle([e EXCEPT !.pending = NoPlan, !.demand = TRUE],m)
                      ELSE \E admit \in (IF Stable THEN {TRUE} ELSE BOOLEAN):
                            Apply(MakePlan(e.now,admit /\ HasTarget(e.action)),FALSE)
                 ELSE FALSE
Tick == DeadlineFallback \/ TickReply
Advance == /\ ~e.stopped /\ e.now < 2
           /\ e' = [e EXCEPT !.now = @+1] /\ UNCHANGED <<w,m,sent,used>>
Cancel == /\ ~Stable /\ e.op \in Ops /\ Settle(Cancelled,m)
UpgradeActive == /\ ~Stable /\ e.active.preview /\ ~e.fence /\ ~e.stopped
                 /\ Settle([Cancelled EXCEPT !.active.pcm = 1, !.active.preview = FALSE],m)
UpgradeNext == /\ ~Stable /\ e.next.preview /\ e.hasNext /\ ~e.fence /\ ~e.stopped
               /\ Settle([Cancelled EXCEPT !.next.pcm = 1, !.next.preview = FALSE],m)
HistoryRollover == /\ e.overlap.live /\ (~Stable \/ e.op = 0 \/ e.action # "Prev")
                   /\ IF e.overlap.outgoing.occ = e.active.occ
                      THEN Settle([e EXCEPT !.overlap = NoOverlap],m)
                      ELSE LET base == IF CancelHistory /\ e.action = "Prev" THEN Cancelled ELSE e IN
                           Settle([base EXCEPT !.history = e.overlap.outgoing, !.hasHistory = TRUE,
                                              !.overlap = NoOverlap],m)
FutureBegin == /\ ~Stable /\ ~e.fence /\ ~e.stopped /\ ~e.overlap.live
               /\ e.op = 0 /\ Len(e.deferred) = 0 /\ ~e.active.preview
               /\ e' = [e EXCEPT !.fence = TRUE] /\ UNCHANGED <<w,m,sent,used>>
FutureAbort == /\ e.fence /\ e' = [e EXCEPT !.fence = FALSE] /\ UNCHANGED <<w,m,sent,used>>
FutureCommit == /\ e.fence /\ e.epoch = 0
                /\ Settle([Cancelled EXCEPT !.epoch = 1, !.fence = FALSE, !.deferred = <<>>,
                                           !.next = NoDeck, !.hasNext = FALSE],m)
ReadyNext == /\ ~e.hasNext /\ e.epoch \notin e.readyEpochs /\ ~e.fence /\ ~e.stopped
             /\ e' = [e EXCEPT !.next = Deck(1,0,FALSE), !.hasNext = TRUE,
                               !.readyEpochs = @ \cup {e.epoch}]
             /\ UNCHANGED <<w,m,sent,used>>
Stop == /\ ~Stable /\ ~e.stopped
        /\ Settle([Cancelled EXCEPT !.stopped = TRUE, !.deferred = <<>>],m)
Next == SendNav \/ DrainNav \/ Dispatch \/ WorkerFailure \/ PollReply \/ PollStale
        \/ Tick \/ Advance \/ Cancel \/ UpgradeActive \/ UpgradeNext \/ HistoryRollover
        \/ FutureBegin \/ FutureAbort \/ FutureCommit \/ ReadyNext \/ Stop \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars /\ WF_vars(Advance) /\ WF_vars(Tick)
TypeOK == /\ e.op \in 0..2 /\ e.epoch \in 0..1 /\ e.now \in 0..2
          /\ sent \in 0..2 /\ used \in 0..2 /\ w.stale \in 0..8 /\ w.attempts \in 0..2
GenerationOrder == /\ used <= sent /\ e.op \in {0,used}
                   /\ \A msg \in w.replies: msg.plan.gen \in 1..used
GenerationSafe == m.generationSafe
IdentitySafe == m.identitySafe
EpochSafe == m.epochSafe
ExactFrameSafe == m.frameSafe
AcceptedTargetSafe == m.targetSafe
FenceSafe == m.fenceSafe
AdoptOnce == \A g \in Ops: m.adopted[g] <= 1
EventsFollowAdoption == m.starts = m.adopted[1] + m.adopted[2]
ConsumptionFollowsAdoption == m.consumptionSafe
EventuallyResolved == \A g \in Ops: (e.op = g) ~> (e.op # g)
StableAdoption == \A g \in Ops: (e.op = g) ~> (m.adopted[g] = 1)
=============================================================================
