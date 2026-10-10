// First cut: queue/launch/crash/budget. See README for abstraction boundaries.
module lifecycle {
  type Task = { state: str, attempts: int, notBefore: int, live: bool }
  pure val fresh: Task = { state: "queued", attempts: 0, notBefore: -1, live: false }
  pure def terminal(t: Task): bool = Set("completed", "cancelled", "failed").contains(t.state)
  pure def ready(t: Task, now: int): bool =
    Set("queued", "crashed", "throttled").contains(t.state) and not(t.live) and t.notBefore <= now

  // Retry can preserve attempts on a nonterminal task. Consequently attempts
  // need not be <= maxAttempts: repeated manual retries of throttled sessions
  // can exceed it. The bound is checked when a crash occurs, as in Rust.
  pure def allowed(t: Task, op: str, now: int, until: int, maximum: int): bool = {
    if (op == "Start") ready(t, now)
    else if (op == "Launch") t.state == "starting" and not(t.live)
    else if (op == "Crash") (t.live or t.state == "starting") and t.attempts < maximum
    else if (op == "Fail") (t.live or t.state == "starting") and t.attempts >= maximum
    else if (op == "Finish") (t.state == "completed" and t.live) or Set("running", "idle", "paused", "throttled", "crashed", "failed", "needs_attention").contains(t.state)
    else if (op == "Complete") Set("running", "idle", "paused", "throttled", "crashed", "failed", "needs_attention").contains(t.state)
    else if (op == "Release") t.live and terminal(t)
    else if (op == "Throttle") ready(t, now) and until >= now
    else if (op == "RateLimit") t.live and not(terminal(t)) and until >= now
    else if (op == "Wake") t.state == "throttled" and t.live and t.notBefore <= now
    else if (op == "Pause") not(terminal(t)) and t.state != "paused" and t.state != "starting"
    else if (op == "Resume") Set("paused", "needs_attention").contains(t.state)
    else if (op == "SourceCancel") not(terminal(t)) and t.state != "starting"
    else if (op == "Cancel") not(terminal(t)) and t.state != "starting"
    else if (op == "Retry") terminal(t) or Set("crashed", "throttled", "paused", "needs_attention").contains(t.state)
    else if (op == "Idle") t.live and t.state == "running"
    else if (op == "Block") t.live and Set("running", "idle", "throttled", "needs_attention").contains(t.state)
    else if (op == "Progress") t.live and Set("idle", "needs_attention").contains(t.state)
    else false
  }

  pure def advance(t: Task, op: str, until: int): Task = {
    if (op == "Start") { ...t, state: "starting", attempts: t.attempts + 1, notBefore: -1 }
    else if (op == "Launch") { ...t, state: "running", live: true }
    else if (op == "Crash") { ...t, state: "crashed", live: false, notBefore: until }
    else if (op == "Fail") { ...t, state: "failed", live: false, notBefore: -1 }
    else if (op == "Finish") { ...t, state: "completed", live: false, notBefore: -1 }
    else if (op == "Complete") { ...t, state: "completed", notBefore: -1 }
    else if (op == "Release") { ...t, live: false }
    else if (op == "Throttle") { ...t, state: "throttled", notBefore: until }
    else if (op == "RateLimit") { ...t, state: "throttled", notBefore: until }
    else if (op == "Wake") { ...t, state: "running", notBefore: -1 }
    else if (op == "Pause") { ...t, state: "paused", notBefore: -1 }
    else if (op == "Resume") { ...t, state: if (t.live) "running" else "queued", notBefore: -1 }
    else if (op == "SourceCancel") { ...t, state: "cancelled", notBefore: -1 }
    else if (op == "Cancel") { ...t, state: "cancelled", live: false, notBefore: -1 }
    else if (op == "Retry") { ...t, state: "queued", live: false, notBefore: -1,
        attempts: if (terminal(t)) 0 else t.attempts }
    else if (op == "Idle") { ...t, state: "idle" }
    else if (op == "Block") { ...t, state: "needs_attention", notBefore: -1 }
    else if (op == "Progress") { ...t, state: "running", notBefore: -1 }
    else t
  }
}

module powerqueue {
  import lifecycle.*
  pure val ids = Set(0, 1, 2)
  pure val providers = Set(0, 1)
  pure val maxConcurrent = 2
  pure val maxAttempts = 3
  pure val budget = 1
  pure val backoff = List(1, 2, 4)
  pure def provider(id: int): int = id % 2
  pure def delay(attempt: int): int = backoff.nth(if (attempt <= 1) 0 else if (attempt >= backoff.length()) backoff.length() - 1 else attempt - 1)

  var tasks: int -> Task
  var now: int
  // Pass-local predicted cost, reset at each new pass, NOT cumulative usage.
  var spent: int -> int
  var limitedUntil: int -> int
  var considered: Set[int]
  var inPass: bool
  var schedulingPaused: bool

  val live = ids.filter(i => tasks.get(i).live).size()
  val starting = ids.filter(i => tasks.get(i).state == "starting")
  val stable = starting.size() == 0
  val vars = (tasks, now, spent, limitedUntil, considered, inPass, schedulingPaused)

  action init = all {
    tasks' = ids.mapBy(_ => fresh), now' = 0,
    spent' = providers.mapBy(_ => 0), limitedUntil' = providers.mapBy(_ => -1),
    considered' = Set(), inPass' = false, schedulingPaused' = false,
  }

  action keepClock = all { now' = now, limitedUntil' = limitedUntil }
  action keepPass = all { spent' = spent, considered' = considered, inPass' = inPass, schedulingPaused' = schedulingPaused }

  action beginPass = all {
    stable, not(inPass), not(schedulingPaused),
    inPass' = true, considered' = Set(), spent' = providers.mapBy(_ => 0),
    tasks' = tasks, schedulingPaused' = schedulingPaused, keepClock,
  }
  action endPass = all {
    stable, inPass,
    ids.forall(i => not(ready(tasks.get(i), now)) or considered.contains(i) or live >= maxConcurrent),
    inPass' = false, considered' = considered, spent' = spent,
    tasks' = tasks, schedulingPaused' = schedulingPaused, keepClock,
  }
  action start(id: int): bool = all {
    stable, inPass, not(considered.contains(id)), live < maxConcurrent,
    limitedUntil.get(provider(id)) <= now, spent.get(provider(id)) + 1 <= budget,
    allowed(tasks.get(id), "Start", now, -1, maxAttempts),
    tasks' = tasks.set(id, advance(tasks.get(id), "Start", -1)),
    considered' = considered.union(Set(id)), spent' = spent, inPass' = inPass,
    schedulingPaused' = schedulingPaused, keepClock,
  }
  action launch(id: int): bool = all {
    allowed(tasks.get(id), "Launch", now, -1, maxAttempts),
    tasks' = tasks.set(id, advance(tasks.get(id), "Launch", -1)),
    spent' = spent.setBy(provider(id), n => n + 1),
    considered' = considered, inPass' = inPass, schedulingPaused' = schedulingPaused, keepClock,
  }
  action throttle(id: int): bool = all {
    stable, inPass, not(considered.contains(id)), live < maxConcurrent,
    limitedUntil.get(provider(id)) > now or spent.get(provider(id)) + 1 > budget,
    val until = if (limitedUntil.get(provider(id)) > now) limitedUntil.get(provider(id)) else now + 1
    all {
      allowed(tasks.get(id), "Throttle", now, until, maxAttempts),
      tasks' = tasks.set(id, advance(tasks.get(id), "Throttle", until)),
    },
    considered' = considered.union(Set(id)), spent' = spent, inPass' = inPass,
    schedulingPaused' = schedulingPaused, keepClock,
  }
  action crash(id: int): bool = all {
    // A failed start is resolved within the pass; running crashes between passes.
    (tasks.get(id).state == "starting") or (stable and not(inPass)),
    val op = if (tasks.get(id).attempts >= maxAttempts) "Fail" else "Crash"
    all {
      allowed(tasks.get(id), op, now, now + delay(tasks.get(id).attempts), maxAttempts),
      tasks' = tasks.set(id, advance(tasks.get(id), op, now + delay(tasks.get(id).attempts))),
    },
    keepPass, keepClock,
  }
  action command(id: int, op: str): bool = all {
    stable, not(inPass), allowed(tasks.get(id), op, now, -1, maxAttempts),
    tasks' = tasks.set(id, advance(tasks.get(id), op, -1)), keepPass, keepClock,
  }
  action rateLimit(id: int): bool = all {
    stable, not(inPass), allowed(tasks.get(id), "RateLimit", now, now + 2, maxAttempts),
    tasks' = tasks.set(id, advance(tasks.get(id), "RateLimit", now + 2)),
    limitedUntil' = limitedUntil.set(provider(id), now + 2), now' = now, keepPass,
  }
  action tick = all {
    stable, not(inPass), now' = now + 1,
    limitedUntil' = providers.mapBy(p => if (limitedUntil.get(p) <= now + 1) -1 else limitedUntil.get(p)),
    tasks' = tasks, keepPass,
  }
  action pauseScheduling = all {
    stable, not(inPass), schedulingPaused' = not(schedulingPaused),
    tasks' = tasks, spent' = spent, considered' = considered, inPass' = inPass, keepClock,
  }
  action step = {
    nondet id = ids.oneOf()
    nondet op = Set("Finish", "Complete", "Release", "Wake", "Pause", "Resume", "Cancel", "Retry", "Idle", "Block", "Progress").oneOf()
    any { beginPass, endPass, start(id), launch(id), throttle(id), crash(id), command(id, op), rateLimit(id), tick, pauseScheduling }
  }

  val safety = all {
    live <= maxConcurrent, starting.size() <= 1,
    live + starting.size() <= maxConcurrent,
    providers.forall(p => spent.get(p) >= 0 and spent.get(p) <= budget),
    ids.forall(i => {
      val t = tasks.get(i)
      all {
        t.attempts >= 0,
        t.notBefore == -1 or Set("crashed", "throttled").contains(t.state),
        // CLI completion has a live session until the daemon's next sweep.
        t.state != "cancelled" and t.state != "failed" or not(t.live),
        t.state != "starting" or not(t.live),
      }
    }),
  }

  run completesTest = init.then(beginPass).then(start(0)).then(launch(0))
    .then(start(1)).then(launch(1)).then(endPass)
    .then(command(0, "Complete")).expect(tasks.get(0).live)
    .then(command(0, "Release")).expect(safety and not(tasks.get(0).live))
  run failedStartFreesSlotTest = init.then(beginPass).then(start(0)).then(crash(0))
    .then(start(1)).then(launch(1)).then(start(2)).then(launch(2))
    .expect(safety and live == 2 and spent.get(0) == 1)
  run crashLimitTest = init.then(beginPass).then(start(0)).then(crash(0))
    .then(start(1)).then(launch(1)).then(start(2)).then(launch(2)).then(endPass)
    .then(tick).then(command(0, "Retry")).then(command(1, "Cancel"))
    .then(beginPass).then(start(0)).then(crash(0)).then(endPass)
    .then(tick).then(tick).then(beginPass).then(start(0)).then(crash(0))
    .expect(safety and tasks.get(0).state == "failed")
}

// Progress under a persistent free slot and budget, without competing tasks or
// user commands. These are necessary assumptions: production priority scheduling
// alone does not guarantee starvation freedom for each individual queued task.
module progress {
  import lifecycle.*
  var task: Task
  var now: int
  val vars = (task, now)
  action init = {
    nondet throttled = Set(false, true).oneOf()
    all { now' = 0, task' = if (throttled) { ...fresh, state: "throttled", notBefore: 2 } else fresh }
  }
  action tick = all { now < 3, now' = now + 1, task' = task }
  action launch = all {
    ready(task, now), now' = now,
    task' = advance(advance(task, "Start", -1), "Launch", -1),
  }
  action idle = all { task.live, task' = task, now' = now }
  action step = any { tick, launch, idle }
  temporal eventuallyStarts = all { tick.weakFair(vars), launch.weakFair(vars) }.implies(eventually(task.live))
  run retryTimeTest = (all { now' = 0, task' = { ...fresh, state: "throttled", notBefore: 2 } })
    .then(tick).expect(not(ready(task, now))).then(tick).then(launch).expect(task.live)
}
