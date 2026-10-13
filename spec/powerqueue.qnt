// Queue/launch/crash/budget, dependency holds and PR review lifecycle.
// See README for abstraction boundaries.
module lifecycle {
  type Task = { state: str, attempts: int, reviewBase: int, watching: bool, notBefore: int, live: bool, approved: bool, routed: bool, legalStarts: bool }
  pure val fresh: Task = { state: "queued", attempts: 0, reviewBase: 0, watching: false, notBefore: -1, live: false, approved: true, routed: true, legalStarts: true }
  // A requested review round gets a fresh crash allowance. Manual retries of
  // terminal tasks reset attempts but can retain the review request in Rust.
  pure def usedAttempts(t: Task): int = if (t.attempts >= t.reviewBase) t.attempts - t.reviewBase else 0
  pure def terminal(t: Task): bool = Set("completed", "cancelled", "failed").contains(t.state)
  pure def ready(t: Task, now: int): bool =
    Set("queued", "crashed", "throttled").contains(t.state) and t.approved and t.routed and not(t.live) and t.notBefore <= now

  // Retry can preserve attempts on a nonterminal task. Consequently attempts
  // need not be <= maxAttempts: repeated manual retries of throttled sessions
  // can exceed it. The bound is checked when a crash occurs, as in Rust.
  pure def allowed(t: Task, op: str, now: int, until: int, maximum: int): bool = {
    if (Set("Approve", "Revoke", "RouteAllow", "RouteBlock").contains(op)) true
    else if (op == "Hold") t.state == "starting" and not(t.approved and t.routed) and not(t.live)
    else if (op == "Start") ready(t, now)
    else if (op == "Launch") t.state == "starting" and t.approved and t.routed and not(t.live)
    else if (op == "Crash") (t.live or t.state == "starting") and t.state != "in_review" and usedAttempts(t) < maximum
    else if (op == "Fail") (t.live or t.state == "starting") and t.state != "in_review" and usedAttempts(t) >= maximum
    else if (op == "Finish") (t.state == "completed" and t.live) or Set("running", "idle", "paused", "throttled", "crashed", "failed", "needs_attention").contains(t.state)
    else if (op == "Complete") Set("running", "idle", "paused", "throttled", "crashed", "failed", "needs_attention", "blocked", "in_review").contains(t.state)
    else if (op == "Release") t.live and terminal(t)
    else if (op == "DependencyBlock") Set("queued", "throttled").contains(t.state)
    else if (op == "Unblock") t.state == "blocked"
    else if (op == "HandOff") Set("running", "idle", "paused", "needs_attention", "in_review").contains(t.state)
    else if (op == "ReviewRelease") t.state == "in_review" and t.live
    else if (op == "ReviewRetry" or op == "Merge") t.state == "in_review" and not(t.live)
    else if (op == "Throttle") ready(t, now) and until >= now
    else if (op == "RateLimit") t.live and not(terminal(t)) and t.state != "in_review" and until >= now
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
    if (op == "CreateGitHub" or op == "Revoke") { ...t, approved: false }
    else if (op == "Approve") { ...t, approved: true }
    else if (op == "RouteAllow") { ...t, routed: true }
    else if (op == "RouteBlock") { ...t, routed: false }
    else if (op == "Hold") { ...t, state: "queued" }
    // History witness: catches a start/launch guard accidentally weakened later.
    else if (op == "Start") { ...t, state: "starting", attempts: t.attempts + 1, notBefore: -1, legalStarts: t.legalStarts and t.approved and t.routed }
    else if (op == "Launch") { ...t, state: "running", live: true, legalStarts: t.legalStarts and t.approved and t.routed }
    else if (op == "Crash") { ...t, state: "crashed", live: false, notBefore: until }
    else if (op == "Fail") { ...t, state: "failed", live: false, notBefore: -1 }
    else if (op == "Finish") { ...t, state: "completed", live: false, notBefore: -1 }
    else if (op == "Complete") { ...t, state: "completed", notBefore: -1 }
    else if (op == "Release") { ...t, live: false }
    else if (op == "DependencyBlock") { ...t, state: "blocked", notBefore: -1 }
    else if (op == "Unblock") { ...t, state: "queued" }
    else if (op == "HandOff") { ...t, state: "in_review", notBefore: -1, reviewBase: 0, watching: true }
    else if (op == "ReviewRelease") { ...t, live: false }
    else if (op == "ReviewRetry") { ...t, state: "queued", notBefore: -1, reviewBase: t.attempts, watching: false }
    else if (op == "Merge") { ...t, state: "completed" }
    else if (op == "Throttle") { ...t, state: "throttled", notBefore: until }
    else if (op == "RateLimit") { ...t, state: "throttled", notBefore: until }
    else if (op == "Wake") { ...t, state: "running", notBefore: -1 }
    else if (op == "Pause") { ...t, state: "paused", notBefore: -1 }
    else if (op == "Resume") { ...t, state: if (t.live) "running" else if (t.watching) "in_review" else "queued", notBefore: -1 }
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
  pure val projects = Set(0, 1, 2)
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
  // A checkout is an opaque identity, distinct from a project ID. Moving a
  // configured checkout must never redirect an already-bound task.
  var project: int -> int
  var checkout: int -> int
  var available: Set[int]
  var boundProject: int -> int
  var boundCheckout: int -> int
  // History witness: unlike runtime/artifact fields, this is written only once.
  var firstBinding: int -> { project: int, checkout: int }
  var artifactProject: int -> int
  var artifactCheckout: int -> int
  var lastRepoTask: int
  var lastRepoProject: int
  var lastRepoCheckout: int

  val projectVars = (project, checkout, available, boundProject, boundCheckout, firstBinding,
    artifactProject, artifactCheckout, lastRepoTask, lastRepoProject, lastRepoCheckout)
  def checkoutMatches(id: int): bool = boundCheckout.get(id) == -1 or
    boundCheckout.get(id) == checkout.get(project.get(id))
  def canReassign(id: int): bool = {
    val t = tasks.get(id)
    all {
      boundProject.get(id) == -1, t.attempts == 0, not(t.live),
      Set("queued", "paused", "blocked", "throttled", "needs_attention").contains(t.state),
    }
  }
  action keepProjects = all {
    project' = project, checkout' = checkout, available' = available,
    boundProject' = boundProject, boundCheckout' = boundCheckout, firstBinding' = firstBinding,
    artifactProject' = artifactProject, artifactCheckout' = artifactCheckout,
    lastRepoTask' = lastRepoTask, lastRepoProject' = lastRepoProject, lastRepoCheckout' = lastRepoCheckout,
  }

  val live = ids.filter(i => tasks.get(i).live).size()
  val starting = ids.filter(i => tasks.get(i).state == "starting")
  val stable = starting.size() == 0
  val vars = (tasks, now, spent, limitedUntil, considered, inPass, schedulingPaused, projectVars)

  action init = all {
    tasks' = ids.mapBy(_ => fresh), now' = 0,
    spent' = providers.mapBy(_ => 0), limitedUntil' = providers.mapBy(_ => -1),
    considered' = Set(), inPass' = false, schedulingPaused' = false,
    project' = ids.mapBy(i => i), checkout' = projects.mapBy(p => p), available' = projects,
    boundProject' = ids.mapBy(_ => -1), boundCheckout' = ids.mapBy(_ => -1),
    firstBinding' = ids.mapBy(_ => { project: -1, checkout: -1 }),
    artifactProject' = ids.mapBy(_ => -1), artifactCheckout' = ids.mapBy(_ => -1),
    lastRepoTask' = -1, lastRepoProject' = -1, lastRepoCheckout' = -1,
  }

  action keepClock = all { now' = now, limitedUntil' = limitedUntil }
  action keepPass = all { spent' = spent, considered' = considered, inPass' = inPass, schedulingPaused' = schedulingPaused }

  action beginPass = all {
    keepProjects,
    stable, not(inPass), not(schedulingPaused),
    inPass' = true, considered' = Set(), spent' = providers.mapBy(_ => 0),
    tasks' = tasks, schedulingPaused' = schedulingPaused, keepClock,
  }
  action endPass = all {
    keepProjects,
    stable, inPass,
    ids.forall(i => not(ready(tasks.get(i), now)) or not(available.contains(project.get(i))) or considered.contains(i) or live >= maxConcurrent),
    inPass' = false, considered' = considered, spent' = spent,
    tasks' = tasks, schedulingPaused' = schedulingPaused, keepClock,
  }
  action start(id: int): bool = all {
    available.contains(project.get(id)), checkoutMatches(id),
    project' = project, checkout' = checkout, available' = available,
    boundProject' = boundProject.set(id, project.get(id)),
    boundCheckout' = boundCheckout.set(id, checkout.get(project.get(id))),
    firstBinding' = if (firstBinding.get(id).project == -1)
      firstBinding.set(id, { project: project.get(id), checkout: checkout.get(project.get(id)) }) else firstBinding,
    artifactProject' = artifactProject.set(id, project.get(id)),
    artifactCheckout' = artifactCheckout.set(id, checkout.get(project.get(id))),
    lastRepoTask' = id, lastRepoProject' = project.get(id), lastRepoCheckout' = checkout.get(project.get(id)),
    stable, inPass, not(considered.contains(id)), live < maxConcurrent,
    limitedUntil.get(provider(id)) <= now, spent.get(provider(id)) + 1 <= budget,
    allowed(tasks.get(id), "Start", now, -1, maxAttempts),
    tasks' = tasks.set(id, advance(tasks.get(id), "Start", -1)),
    considered' = considered.union(Set(id)), spent' = spent, inPass' = inPass,
    schedulingPaused' = schedulingPaused, keepClock,
  }
  action launch(id: int): bool = all {
    keepProjects,
    allowed(tasks.get(id), "Launch", now, -1, maxAttempts),
    tasks' = tasks.set(id, advance(tasks.get(id), "Launch", -1)),
    spent' = spent.setBy(provider(id), n => n + 1),
    considered' = considered, inPass' = inPass, schedulingPaused' = schedulingPaused, keepClock,
  }
  action throttle(id: int): bool = all {
    keepProjects, available.contains(project.get(id)),
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
    keepProjects,
    // A failed start is resolved within the pass; running crashes between passes.
    (tasks.get(id).state == "starting") or (stable and not(inPass)),
    val op = if (usedAttempts(tasks.get(id)) >= maxAttempts) "Fail" else "Crash"
    all {
      allowed(tasks.get(id), op, now, now + delay(usedAttempts(tasks.get(id))), maxAttempts),
      tasks' = tasks.set(id, advance(tasks.get(id), op, now + delay(usedAttempts(tasks.get(id))))),
    },
    keepPass, keepClock,
  }
  action command(id: int, op: str): bool = all {
    keepProjects,
    stable, not(inPass), allowed(tasks.get(id), op, now, -1, maxAttempts),
    tasks' = tasks.set(id, advance(tasks.get(id), op, -1)), keepPass, keepClock,
  }
  action rateLimit(id: int): bool = all {
    keepProjects,
    stable, not(inPass), allowed(tasks.get(id), "RateLimit", now, now + 2, maxAttempts),
    tasks' = tasks.set(id, advance(tasks.get(id), "RateLimit", now + 2)),
    limitedUntil' = limitedUntil.set(provider(id), now + 2), now' = now, keepPass,
  }
  action tick = all {
    keepProjects,
    stable, not(inPass), now' = now + 1,
    limitedUntil' = providers.mapBy(p => if (limitedUntil.get(p) <= now + 1) -1 else limitedUntil.get(p)),
    tasks' = tasks, keepPass,
  }
  action pauseScheduling = all {
    keepProjects,
    stable, not(inPass), schedulingPaused' = not(schedulingPaused),
    tasks' = tasks, spent' = spent, considered' = considered, inPass' = inPass, keepClock,
  }
  // Rejected reassignment is observable as a stutter, including while a
  // launch is in progress. Retry resetting attempts does not clear the pin.
  action reassign(id: int, target: int): bool = all {
    project' = if (canReassign(id)) project.set(id, target) else project,
    checkout' = checkout, available' = available,
    boundProject' = boundProject, boundCheckout' = boundCheckout, firstBinding' = firstBinding,
    artifactProject' = artifactProject, artifactCheckout' = artifactCheckout,
    lastRepoTask' = lastRepoTask, lastRepoProject' = lastRepoProject, lastRepoCheckout' = lastRepoCheckout,
    tasks' = tasks, keepPass, keepClock,
  }
  // A missing project is omitted from the pass. A changed bound checkout is
  // considered and rejected before a repository operation, using no capacity.
  action rejectCheckout(id: int): bool = all {
    stable, inPass, live < maxConcurrent, not(considered.contains(id)),
    available.contains(project.get(id)), ready(tasks.get(id), now), not(checkoutMatches(id)),
    limitedUntil.get(provider(id)) <= now, spent.get(provider(id)) + 1 <= budget,
    considered' = considered.union(Set(id)), spent' = spent, inPass' = inPass,
    schedulingPaused' = schedulingPaused, tasks' = tasks, keepClock, keepProjects,
  }
  action configure(p: int, present: bool, revision: int): bool = all {
    stable, not(inPass), project' = project,
    checkout' = checkout.set(p, p + revision * 2),
    available' = if (present) available.union(Set(p)) else available.exclude(Set(p)),
    boundProject' = boundProject, boundCheckout' = boundCheckout, firstBinding' = firstBinding,
    artifactProject' = artifactProject, artifactCheckout' = artifactCheckout,
    lastRepoTask' = lastRepoTask, lastRepoProject' = lastRepoProject, lastRepoCheckout' = lastRepoCheckout,
    tasks' = tasks, keepPass, keepClock,
  }
  // Abstract push/cleanup operation: only the pinned project's currently
  // configured checkout may receive it. Actual git/path validation stays in Rust.
  action repositoryOp(id: int): bool = all {
    stable, not(inPass), boundProject.get(id) != -1,
    available.contains(project.get(id)), checkoutMatches(id),
    lastRepoTask' = id, lastRepoProject' = project.get(id), lastRepoCheckout' = checkout.get(project.get(id)),
    project' = project, checkout' = checkout, available' = available,
    boundProject' = boundProject, boundCheckout' = boundCheckout, firstBinding' = firstBinding,
    artifactProject' = artifactProject, artifactCheckout' = artifactCheckout,
    tasks' = tasks, keepPass, keepClock,
  }
  // Approval can change while queued, during setup, or during active work.
  // Revoke abstracts removal, material edits, untrusted actors and unavailable evidence.
  action admission(id: int, approved: bool): bool = all {
    tasks' = tasks.set(id, advance(tasks.get(id), if (approved) "Approve" else "Revoke", -1)),
    keepProjects, keepPass, keepClock,
  }
  // Routing and source approval are independent prerequisites for new execution.
  action routing(id: int, permitted: bool): bool = all {
    tasks' = tasks.set(id, advance(tasks.get(id), if (permitted) "RouteAllow" else "RouteBlock", -1)),
    keepProjects, keepPass, keepClock,
  }
  action hold(id: int): bool = all {
    allowed(tasks.get(id), "Hold", now, -1, maxAttempts),
    tasks' = tasks.set(id, advance(tasks.get(id), "Hold", -1)),
    keepProjects, keepPass, keepClock,
  }
  action step = {
    nondet id = ids.oneOf()
    nondet p = projects.oneOf()
    nondet present = Set(false, true).oneOf()
    nondet revision = Set(0, 1).oneOf()
    nondet op = Set("Finish", "Complete", "Release", "Wake", "Pause", "Resume", "Cancel", "Retry", "Idle", "Block", "Progress",
      "DependencyBlock", "Unblock", "HandOff", "ReviewRelease", "ReviewRetry", "Merge").oneOf()
    any { beginPass, endPass, start(id), launch(id), throttle(id), crash(id), command(id, op), rateLimit(id), tick, pauseScheduling, reassign(id, p), configure(p, present, revision), rejectCheckout(id), repositoryOp(id), admission(id, present), routing(id, present), hold(id) }
  }

  val projectSafety = all {
    ids.forall(i => all {
      projects.contains(project.get(i)),
      boundProject.get(i) == -1 or boundProject.get(i) == project.get(i),
      boundProject.get(i) == firstBinding.get(i).project,
      boundCheckout.get(i) == firstBinding.get(i).checkout,
      (boundProject.get(i) == -1) == (boundCheckout.get(i) == -1),
      artifactProject.get(i) == boundProject.get(i),
      artifactCheckout.get(i) == boundCheckout.get(i),
      tasks.get(i).attempts == 0 or boundProject.get(i) != -1,
    }),
    lastRepoTask == -1 or all {
      lastRepoProject == boundProject.get(lastRepoTask),
      lastRepoCheckout == boundCheckout.get(lastRepoTask),
    },
  }
  val safety = all {
    projectSafety,
    live <= maxConcurrent, starting.size() <= 1,
    live + starting.size() <= maxConcurrent,
    providers.forall(p => spent.get(p) >= 0 and spent.get(p) <= budget),
    ids.forall(i => {
      val t = tasks.get(i)
      all {
        t.attempts >= 0, t.legalStarts,
        t.notBefore == -1 or Set("crashed", "throttled").contains(t.state),
        // CLI completion has a live session until the daemon's next sweep.
        t.state != "cancelled" and t.state != "failed" or not(t.live),
        t.state != "starting" or not(t.live),
      }
    }),
  }

  run approvalCannotBypassRoutingTest = init.then(routing(0, false)).then(admission(0, true)).then(beginPass)
    .expect(safety and not(allowed(tasks.get(0), "Start", now, -1, maxAttempts)))
    .then(routing(0, true)).then(start(0)).then(launch(0)).expect(safety and tasks.get(0).live)
  run routingCannotBypassApprovalTest = init.then(admission(0, false)).then(routing(0, true)).then(beginPass)
    .expect(safety and not(allowed(tasks.get(0), "Start", now, -1, maxAttempts)))
  run routingConflictDuringSetupTest = init.then(beginPass).then(start(0)).then(routing(0, false))
    .expect(not(allowed(tasks.get(0), "Launch", now, -1, maxAttempts)))
    .then(hold(0)).then(start(1)).then(launch(1))
    .expect(safety and tasks.get(0).state == "queued" and boundProject.get(0) == 0 and spent.get(0) == 0)
  run activeRoutingConflictPreservesSessionTest = init.then(beginPass).then(start(0)).then(launch(0))
    .then(routing(0, false)).expect(safety and tasks.get(0).live and tasks.get(0).state == "running")
  run retryCannotRestoreRoutingTest = init.then(routing(0, false)).then(command(0, "Cancel")).then(command(0, "Retry"))
    .expect(safety and not(ready(tasks.get(0), now)))

  run unapprovedCannotStartTest = init.then(admission(0, false)).then(beginPass)
    .expect(safety and not(allowed(tasks.get(0), "Start", now, -1, maxAttempts)))
    .then(admission(0, true)).then(start(0)).then(launch(0)).expect(safety and tasks.get(0).live)
  run revocationDuringSetupTest = init.then(beginPass).then(start(0)).then(admission(0, false))
    .expect(not(allowed(tasks.get(0), "Launch", now, -1, maxAttempts)))
    .then(hold(0)).then(start(1)).then(launch(1))
    .expect(safety and tasks.get(0).state == "queued" and not(tasks.get(0).live)
      and boundProject.get(0) == 0 and spent.get(0) == 0 and live == 1)
  run activeRevocationPreservesSessionTest = init.then(beginPass).then(start(0)).then(launch(0))
    .then(admission(0, false)).expect(safety and tasks.get(0).live and tasks.get(0).state == "running")
  run retryCannotRestoreApprovalTest = init.then(admission(0, false)).then(command(0, "Pause"))
    .then(command(0, "Resume")).then(command(0, "Cancel")).then(command(0, "Retry"))
    .expect(safety and not(ready(tasks.get(0), now)))

  run reassignBeforeStartTest = init.then(reassign(0, 1)).then(beginPass).then(start(0))
    .then(reassign(0, 0)).then(launch(0))
    .expect(safety and project.get(0) == 1 and boundCheckout.get(0) == 1)
  run retryKeepsBindingTest = init.then(beginPass).then(start(0)).then(launch(0))
    .then(start(1)).then(launch(1)).then(endPass).then(command(0, "Cancel"))
    .then(command(0, "Retry")).then(reassign(0, 1))
    .expect(safety and tasks.get(0).attempts == 0 and project.get(0) == 0 and boundCheckout.get(0) == 0)
  run missingProjectDoesNotBlockOtherTest = init.then(configure(0, false, 0)).then(configure(2, false, 2))
    .then(beginPass).then(start(1)).then(launch(1)).then(endPass)
    .expect(safety and live == 1 and tasks.get(0) == fresh and spent.get(0) == 0)
  run movedCheckoutRejectedTest = init.then(beginPass).then(start(0)).then(crash(0))
    .then(start(1)).then(launch(1)).then(start(2)).then(launch(2)).then(endPass)
    .then(command(1, "Cancel")).then(command(0, "Retry")).then(configure(0, true, 1))
    .then(beginPass).then(rejectCheckout(0)).then(endPass)
    .expect(safety and tasks.get(0).state == "queued" and boundCheckout.get(0) == 0 and spent.get(0) == 0)
    .then(configure(0, true, 0)).then(beginPass).then(start(0)).then(launch(0))
    .expect(safety and boundCheckout.get(0) == 0)
  run projectsShareProviderBudgetTest = init.then(reassign(2, 1))
    .then(beginPass).then(start(0)).then(launch(0)).then(throttle(2))
    .expect(safety and project.get(0) != project.get(2) and live == 1 and spent.get(0) == 1)

  run threeProjectsShareSlotsAndBudgetTest = init
    .then(beginPass).then(start(0)).then(launch(0)).then(throttle(2))
    .then(start(1)).then(launch(1)).then(endPass)
    .expect(safety and live == maxConcurrent and spent.get(0) == budget and spent.get(1) == budget)
    .then(command(0, "Complete")).then(command(0, "Release"))
    .then(tick).then(beginPass).then(start(2)).then(launch(2)).then(endPass)
    .expect(safety and live == maxConcurrent and boundProject.get(2) == 2)
  run threeProjectsApprovalAndFailureIsolationTest = init.then(admission(0, false)).then(configure(1, false, 1))
    .then(beginPass).then(start(2)).then(launch(2)).then(endPass)
    .expect(safety and live == 1 and tasks.get(0).attempts == 0 and tasks.get(1).attempts == 0 and boundProject.get(2) == 2)

  run completesTest = init.then(beginPass).then(start(0)).then(launch(0))
    .then(start(1)).then(launch(1)).then(endPass)
    .then(command(0, "Complete")).expect(tasks.get(0).live)
    .then(command(0, "Release")).expect(safety and not(tasks.get(0).live))
  run dependencyHoldTest = init.then(command(0, "DependencyBlock"))
    .expect(safety and not(allowed(tasks.get(0), "Start", now, -1, maxAttempts)))
    .then(command(0, "Unblock")).then(beginPass).then(start(0)).then(launch(0))
    .expect(safety and tasks.get(0).attempts == 1)
  run reviewRoundTest = init.then(beginPass).then(start(0)).then(launch(0))
    .then(start(1)).then(launch(1)).then(endPass).then(command(0, "HandOff"))
    .expect(safety and tasks.get(0).live and not(allowed(tasks.get(0), "Merge", now, -1, maxAttempts)))
    .then(command(0, "ReviewRelease")).then(command(0, "ReviewRetry"))
    .then(beginPass).then(start(0)).then(launch(0)).then(endPass)
    .expect(safety and tasks.get(0).attempts == 2 and usedAttempts(tasks.get(0)) == 1)
    .then(command(0, "HandOff")).then(command(0, "ReviewRelease")).then(command(0, "Merge"))
    .expect(safety and tasks.get(0).state == "completed" and not(tasks.get(0).live))
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
  action tick = all { now' = if (now < 3) now + 1 else now, task' = task }
  action launch = all {
    ready(task, now), now' = now,
    task' = advance(advance(task, "Start", -1), "Launch", -1),
  }
  action idle = all { task.live, task' = task, now' = now }
  action step = any { tick, launch, idle }
  temporal eventuallyStarts = all { tick.weakFair(vars), launch.weakFair(vars) }.implies(eventually(task.live))
  run retryTimeTest = (all { now' = 0, task' = { ...fresh, state: "throttled", notBefore: 2 } })
    .then(tick).expect(not(ready(task, now))).then(tick).then(launch).expect(task.live)
  run clockCanStutterBeforeLaunchTest = (all { now' = 0, task' = fresh })
    .then(tick).then(tick).then(tick).then(tick).expect(now == 3 and not(task.live))

}

// A separate finite model exposes the CLI/daemon interleaving hidden by the
// scheduler's atomic Start action. These are abstract project IDs, not paths.
module assignment_race {
  var project: int
  var state: str
  var bound: int
  var snapshotProject: int
  var snapshotState: str
  var workingProject: int
  val vars = (project, state, bound, snapshotProject, snapshotState, workingProject)
  action init = all {
    project' = 0, state' = "queued", bound' = -1,
    snapshotProject' = 0, snapshotState' = "queued", workingProject' = -1,
  }
  action readCandidate = all {
    snapshotProject' = project, snapshotState' = state,
    project' = project, state' = state, bound' = bound, workingProject' = workingProject,
  }
  action reassign(target: int): bool = all {
    project' = if (state == "queued" and bound == -1) target else project,
    state' = state, bound' = bound, workingProject' = workingProject,
    snapshotProject' = snapshotProject, snapshotState' = snapshotState,
  }
  val canClaim = snapshotState == "queued" and state == snapshotState and project == snapshotProject
  action claim = all {
    // Production uses one conditional SQLite UPDATE. A rejected stale read
    // must never be followed by setup, launch, push, or cleanup for that read.
    state' = if (canClaim) "starting" else state,
    workingProject' = if (canClaim) snapshotProject else workingProject,
    project' = project, bound' = bound,
    snapshotProject' = snapshotProject, snapshotState' = snapshotState,
  }
  action bind = all {
    state == "starting", bound == -1 or bound == project,
    bound' = project, state' = "running", project' = project, workingProject' = workingProject,
    snapshotProject' = snapshotProject, snapshotState' = snapshotState,
  }
  action finish = all {
    state == "running", state' = "completed",
    project' = project, bound' = bound, workingProject' = workingProject,
    snapshotProject' = snapshotProject, snapshotState' = snapshotState,
  }
  action retry = all {
    state == "completed", state' = "queued",
    project' = project, bound' = bound, workingProject' = workingProject,
    snapshotProject' = snapshotProject, snapshotState' = snapshotState,
  }
  action step = {
    nondet target = Set(0, 1).oneOf()
    any { readCandidate, reassign(target), claim, bind, finish, retry }
  }
  val safety = all {
    bound == -1 or bound == project,
    workingProject == -1 or workingProject == project,
    state != "running" or bound == project,
  }
  run reassignmentWinsTest = init.then(readCandidate).then(reassign(1)).then(claim)
    .expect(safety and state == "queued" and workingProject == -1)
    .then(readCandidate).then(claim).then(bind).expect(safety and bound == 1)
  run claimWinsTest = init.then(readCandidate).then(claim).then(reassign(1))
    .expect(safety and state == "starting" and project == 0)
    .then(bind).then(finish).then(retry).then(reassign(1))
    .expect(safety and state == "queued" and project == 0 and bound == 0)
}
