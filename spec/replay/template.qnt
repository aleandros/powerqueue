module replay {
  import lifecycle.* from "../powerqueue"
  pure val observations = List(
// OBSERVATIONS
  )
  var tasks: int -> Task
  var seen: Set[int]
  var index: int
  var event: int
  action init = all {
    tasks' = observations.foldl(Set(), (s, o) => s.union(Set(o.id))).mapBy(_ => fresh),
    seen' = Set(), index' = 0, event' = 0,
  }
  action replayStep = {
    val o = observations.nth(index)
    val t = tasks.get(o.id)
    val creation = o.op == "Create" or o.op == "CreateGitHub"
    all {
      // assert, not a disabled action: a bad observation MUST fail the test.
      assert(if (creation) not(seen.contains(o.id)) else seen.contains(o.id)),
      assert(creation or allowed(t, o.op, o.time, o.until, o.maximum)),
      assert(o.attempt == -1 or o.attempt == (if (o.op == "Start") t.attempts + 1 else t.attempts)),
      tasks' = if (o.op == "Create") tasks else tasks.set(o.id, advance(t, o.op, o.until)),
      seen' = seen.union(Set(o.id)), index' = index + 1, event' = o.event,
    }
  }
  run replayTest = init.then(observations.length().reps(_ => replayStep))
    .expect(index == observations.length())
}
