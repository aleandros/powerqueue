// Bounded queue/workflow model. Launch is atomic; budgets and external delivery
// are covered separately. Rust's stateful test drives the production planner.
module triage {
  type Row = { state: str, live: bool, queue: int, analysis: bool, firstQueue: int,
    firstAnalysis: bool, outcome: bool, report: int, closed: bool }
  pure val ids = Set(0, 1, 2, 3, 4, 5)
  pure val fresh: Row = { state: "queued", live: false, queue: -1, analysis: false,
    firstQueue: -1, firstAnalysis: false, outcome: false, report: 0, closed: false }
  var s: { rows: int -> Row, selected: Set[int], enabled: Set[int], caps: int -> int,
    global: int, workflowAnalysis: bool, serial: int, legalStarts: bool }

  action init = s' = { rows: ids.mapBy(_ => fresh), selected: Set(3, 4, 5),
    enabled: Set(0, 1), caps: Map(0 -> 1, 1 -> 1), global: 2,
    workflowAnalysis: true, serial: 0, legalStarts: true }
  val live = ids.filter(i => s.rows.get(i).live).size()
  def queueLive(q: int): int = ids.filter(i => s.rows.get(i).live and s.rows.get(i).queue == q).size()
  def limit(q: int): int = if (s.enabled.size() == 0) { if (q == 0) s.global else 0 }
    else if (s.enabled.contains(q)) s.caps.get(q) else 0
  def canStart(i: int): bool = {
    val r = s.rows.get(i)
    r.queue >= 0 and Set("queued", "crashed").contains(r.state) and not(r.live)
      and live < s.global and queueLive(r.queue) < limit(r.queue)
  }
  def canComplete(i: int): bool = Set("running", "crashed").contains(s.rows.get(i).state)
  def canHandOff(i: int): bool = s.rows.get(i).state == "running" and not(s.rows.get(i).analysis)

  action bind(i: int): bool = {
    val r = s.rows.get(i)
    val q = if (s.selected.contains(i) and s.enabled.contains(1)) 1 else 0
    val analysis = q == 1 and s.workflowAnalysis
    all {
      r.queue == -1,
      s' = { ...s, rows: s.rows.set(i, { ...r, queue: q, firstQueue: q,
        analysis: analysis, firstAnalysis: analysis }) },
    }
  }
  action launch(i: int, succeeds: bool): bool = {
    val r = s.rows.get(i)
    all {
      canStart(i),
      s' = { ...s, rows: s.rows.set(i, { ...r,
        state: if (succeeds) "running" else "crashed", live: succeeds }),
        legalStarts: s.legalStarts and live < s.global and queueLive(r.queue) < limit(r.queue) },
    }
  }
  action complete(i: int): bool = {
    val r = s.rows.get(i)
    all {
      canComplete(i),
      s' = { ...s, serial: s.serial + 1, rows: s.rows.set(i, { ...r, state: "completed",
        outcome: r.analysis, report: s.serial + 1, closed: not(r.analysis) }) },
    }
  }
  action done(i: int): bool = {
    val r = s.rows.get(i)
    all {
      r.live,
      // Missing-report markers are refused; successful markers only release
      // an already recorded analysis and preserve its report identity.
      s' = if (r.analysis and not(r.outcome)) s
        else { ...s, rows: s.rows.set(i, { ...r, state: "completed", live: false,
          closed: not(r.analysis) }) },
    }
  }
  action release(i: int): bool = {
    val r = s.rows.get(i)
    all { r.live, r.state == "completed", s' = { ...s, rows: s.rows.set(i, { ...r, live: false }) } }
  }
  action retry(i: int): bool = {
    val r = s.rows.get(i)
    all {
      Set("completed", "crashed").contains(r.state),
      s' = { ...s, rows: s.rows.set(i, { ...r, state: "queued", live: false, outcome: false, report: 0 }) },
    }
  }
  action crash(i: int): bool = {
    val r = s.rows.get(i)
    all { r.live, r.state == "running", s' = { ...s, rows: s.rows.set(i, { ...r, state: "crashed", live: false }) } }
  }
  action relabel(i: int): bool = s' = { ...s, selected:
    if (s.selected.contains(i)) s.selected.exclude(Set(i)) else s.selected.union(Set(i)) }
  action configure(coding: int, analysis: int, presence: int): bool = s' = { ...s,
    caps: Map(0 -> coding, 1 -> analysis), global: coding + analysis,
    enabled: if (presence == 0) Set(0, 1) else if (presence == 1) Set(0) else Set(),
    workflowAnalysis: false }
  // Persisted rows are the whole abstract execution state. Restart cannot
  // reconstruct bindings from the current selectors or workflow settings.
  action restart = s' = s
  action step = {
    nondet i = ids.oneOf()
    nondet capacity = Set(1, 2).oneOf()
    nondet presence = Set(0, 1, 2).oneOf()
    nondet success = Set(false, true).oneOf()
    any { bind(i), launch(i, success), complete(i), done(i), release(i), retry(i),
      crash(i), relabel(i), configure(capacity, capacity, presence), restart }
  }
  val safety = all {
    s.legalStarts,
    ids.forall(i => {
      val r = s.rows.get(i)
      all {
        r.queue == r.firstQueue, r.analysis == r.firstAnalysis,
        not(r.analysis) or not(r.closed),
        not(r.analysis) or r.state != "completed" or (r.outcome and r.report > 0),
        not(r.analysis) or not(canHandOff(i)),
        not(r.live) or Set("running", "completed").contains(r.state),
      }
    }),
  }

  run codingCannotOccupyTriageTest = init.then(bind(0)).then(bind(1)).then(bind(3))
    .then(launch(0, true)).expect(not(canStart(1)) and canStart(3))
    .then(launch(3, true)).expect(safety and live == 2)
  run reportRequiredAndRetryTest = init.then(bind(3)).then(launch(3, true))
    .then(done(3)).expect(s.rows.get(3).state == "running" and not(canHandOff(3)))
    .then(complete(3)).then(done(3)).then(restart)
    .expect(safety and s.rows.get(3).report == 1 and not(s.rows.get(3).closed))
    .then(retry(3)).expect(s.rows.get(3).analysis and not(s.rows.get(3).outcome))
    .then(launch(3, true)).then(complete(3)).expect(safety and s.rows.get(3).report == 2)
  run removingQueueCannotConvertTriageTest = init.then(bind(3)).then(relabel(3))
    .then(configure(1, 1, 1)).then(restart).expect(not(canStart(3)))
    .then(configure(1, 1, 2)).expect(not(canStart(3)))
    .then(configure(1, 1, 0)).then(launch(3, true)).then(complete(3))
    .expect(safety and s.rows.get(3).analysis and not(s.rows.get(3).closed))
  run failedLaunchKeepsCapacityTest = init.then(bind(0)).then(bind(1)).then(bind(3))
    .then(launch(0, false)).then(launch(1, true)).then(launch(3, true))
    .expect(safety and queueLive(0) == 1 and queueLive(1) == 1)
  run lowerLimitsDrainTest = init.then(bind(0)).then(bind(1)).then(bind(3))
    .then(configure(2, 2, 0)).then(launch(0, true)).then(launch(1, true))
    .then(configure(1, 1, 0)).expect(live == 2 and not(canStart(3)))
    .then(complete(0)).then(release(0)).then(launch(3, true)).expect(safety and live == 2)
}
