#!/usr/bin/env python3
"""Project event JSON to a deterministic Quint test using the shared lifecycle.

No runtime dependencies beyond Python 3.9. Raw event text is never copied to the
spec/ITF: only event IDs, anonymized task indices, times and numeric fields.
"""
import argparse
from collections import Counter, defaultdict
from datetime import datetime
import json
from pathlib import Path
import re
import subprocess
import sys

SPEC = Path(__file__).resolve().parent
OPS = {
    'task.starting': 'Start', 'session.launched': 'Launch',
    'session.crashed': 'Crash', 'task.failed': 'Fail',
    'task.completed': 'Finish', 'task.completed_by_command': 'Complete',
    'session.finalized': 'Release', 'task.throttled': 'Throttle',
    'session.waiting_for_reset': 'RateLimit',
    'task.paused': 'Pause', 'task.resumed': 'Resume',
    'task.cancelled': 'Cancel', 'task.retried': 'Retry',
    'task.blocked': 'Block', 'task.blocked_by_command': 'Block',
    'task.attention_resolved': 'Progress',
}
# At the first out-of-scope transition, stop checking that task. Never resume
# after a skipped transition: its next launch would be checked against stale state.
OUT_OF_SCOPE = {
    'task.in_review', 'task.unblocked', 'task.skipped', 'task.unskipped',
    'task.needs_attention', 'task.error', 'launch.superseded', 'session.exited',
    'task.blocked', 'task.blocked_by_command', 'session.permission_prompt',
}
STUTTERS = {
    'task.updated', 'task.models_changed', 'task.model_set', 'task.model_override',
    'task.retry_ignored', 'session.started', 'session.turn_ended',
    'session.turn_failed', 'session.idle_prompt', 'session.permission_prompt',
    'session.nudged', 'session.stale', 'session.timeout', 'session.discovered',
    'review.lookup_failed',
    'session.marker_polled', 'session.rate_limit_detected',
}
STUTTER_PREFIXES = ('hook.', 'worktree.', 'cleanup.', 'linear.', 'github.', 'repo.',
                    'daemon.', 'rules.', 'tune.', 'config.', 'usage.')


def instant(value):
    """RFC3339 to datetime (fractional precision beyond microseconds is discarded)."""
    result = datetime.fromisoformat(re.sub(r'(\.\d{6})\d+', r'\1', value).replace('Z', '+00:00'))
    if result.tzinfo is None:
        raise ValueError('event timestamp needs a timezone')
    return result


def project(events):
    """Return observations and a coverage report; reject malformed/unknown events."""
    if not isinstance(events, list) or not events:
        raise ValueError('expected a nonempty JSON array from logs --events --json')
    events = sorted(events, key=lambda e: e['id'])
    if len({e['id'] for e in events}) != len(events):
        raise ValueError('duplicate event IDs')
    origin = min(instant(e['timestamp']) for e in events)
    grouped = defaultdict(list)
    for event in events:
        if event.get('task_id'):
            grouped[event['task_id']].append(event)
    skipped = Counter()
    eligible = {}
    cutoff = {}
    for task, rows in grouped.items():
        if rows[0]['kind'] != 'task.created':
            skipped['missing initial creation (partial history)'] += 1
            continue
        eligible[task] = len(eligible)
        for row in rows:
            kind = row['kind']
            if kind in OUT_OF_SCOPE or (kind.startswith(('review.', 'relay.', 'inbox.')) and kind not in STUTTERS):
                cutoff[task] = row['id']
                skipped['prefix ends at ' + kind] += 1
                break
    observations = []
    creations = {}
    cancellations = set()
    ignored = Counter()
    last_time = 0
    for event in events:
        task = event.get('task_id')
        if task not in eligible or event['id'] >= cutoff.get(task, float('inf')):
            continue
        kind = event['kind']
        data = event['data']
        # Store IDs define ordering, not wall clocks. Refuse clock regressions
        # instead of silently making a premature launch legal.
        time = int((instant(event['timestamp']) - origin).total_seconds())
        if time < last_time:
            raise ValueError(f"event {event['id']}: clock moved backwards")
        last_time = time
        until = -1
        attempt = -1
        maximum = 0
        if kind == 'task.created':
            if task in creations and not data and 'identifier' in creations[task]:
                creations[task] = {}
                ignored['task.created (daemon echo of Linear sync)'] += 1
                continue
            creations[task] = data
            op = 'Create'
        elif kind == 'budget.rate_limited':
            if data.get('source') == 'pane':
                ignored[kind + ' (provider only)'] += 1
                continue
            op = 'RateLimit'
            until = int((instant(data['until']) - origin).total_seconds())
        elif kind == 'task.cancelled' and not data and event['message'] == 'issue closed in Linear' and task in cancellations:
            cancellations.remove(task)
            ignored['task.cancelled (daemon echo of Linear sync)'] += 1
            continue
        elif kind in OPS:
            op = OPS[kind]
            if kind == 'task.cancelled' and 'linear_state_type' in data:
                op = 'SourceCancel'
                cancellations.add(task)
            if kind == 'task.resumed' and event['message'] == 'rate-limit cooldown passed; session still alive':
                op = 'Wake'
            if op in ('Start', 'Launch', 'Crash', 'Fail'):
                attempt = data['attempt']
                if not isinstance(attempt, int) or attempt < 1:
                    raise ValueError(f"event {event['id']}: invalid attempt")
            if op in ('Crash', 'Fail'):
                maximum = data['max_attempts']
                if not isinstance(maximum, int) or maximum < 1:
                    raise ValueError(f"event {event['id']}: invalid max_attempts")
            if op == 'Crash':
                # v0.14 logs the delay only in the message, not a retry timestamp.
                match = re.search(r'; retrying in (\d+)s \(attempt ', event['message'])
                if not match:
                    raise ValueError(f"event {event['id']}: missing crash backoff")
                until = time + int(match.group(1))
            elif op in ('Throttle', 'RateLimit'):
                until = int((instant(data['retry_at' if op == 'Throttle' else 'until']) - origin).total_seconds())
        elif kind in STUTTERS or kind.startswith(STUTTER_PREFIXES):
            ignored[kind] += 1
            continue
        else:
            raise ValueError(f"event {event['id']}: unmapped kind {kind!r}; classify explicitly")
        observations.append(dict(id=eligible[task], op=op, time=time, until=until,
                                 attempt=attempt, maximum=maximum, event=event['id']))
    actions = Counter(o['op'] for o in observations)
    if not actions['Launch']:
        raise ValueError('no in-scope launches; refusing a vacuous replay')
    report = dict(events=len(events), tasks=len(eligible), truncated_or_skipped_tasks=dict(skipped),
                  observations=len(observations), actions=dict(actions), stutters=dict(ignored),
                  first_timestamp=events[0]['timestamp'], last_timestamp=events[-1]['timestamp'])
    return observations, report


def render(observations):
    """Render a schedule whose EVERY step asserts the shared transition guard."""
    rows = ',\n'.join('    { ' + ', '.join(f'{k}: {json.dumps(v)}' for k, v in o.items()) + ' }' for o in observations)
    return '''module replay {
  import lifecycle.* from "../powerqueue"
  pure val observations = List(
''' + rows + '''
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
    all {
      // assert, not a disabled action: a bad observation MUST fail the test.
      assert(if (o.op == "Create") not(seen.contains(o.id)) else seen.contains(o.id)),
      assert(o.op == "Create" or allowed(t, o.op, o.time, o.until, o.maximum)),
      assert(o.attempt == -1 or o.attempt == (if (o.op == "Start") t.attempts + 1 else t.attempts)),
      tasks' = if (o.op == "Create") tasks else tasks.set(o.id, advance(t, o.op, o.until)),
      seen' = seen.union(Set(o.id)), index' = index + 1, event' = o.event,
    }
  }
  run replayTest = init.then(observations.length().reps(_ => replayStep))
    .expect(index == observations.length())
}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('events', type=Path)
    parser.add_argument('--name', default='replay', help='local output basename')
    parser.add_argument('--run', action='store_true', help='run Quint; emit validated ITF')
    args = parser.parse_args()
    if not re.fullmatch(r'[a-zA-Z0-9_-]+', args.name):
        parser.error('--name must contain only letters, numbers, underscore or hyphen')
    try:
        observations, report = project(json.loads(args.events.read_text()))
        destination = SPEC / '.generated'
        destination.mkdir(exist_ok=True)
        path = destination / (args.name + '.qnt')
        path.write_text(render(observations))
        (destination / (args.name + '.coverage.json')).write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2), flush=True)
        if args.run:
            trace = destination / (args.name + '.itf.json')
            trace.unlink(missing_ok=True)
            result = subprocess.run([str(SPEC / 'node_modules/.bin/quint'), 'test', str(path),
                                     '--max-samples=1', '--out-itf=' + str(trace)])
            if result.returncode:
                if trace.exists():
                    states = json.loads(trace.read_text()).get('states', [])
                    valid = [s for s in states if 'index' in s]
                    if valid:
                        position = int(valid[-1]['index']['#bigint'])
                        if position < len(observations):
                            print('Rejected observation: ' + json.dumps(observations[position]), file=sys.stderr)
                raise ValueError(f'Quint replay failed (exit {result.returncode})')

    except (ValueError, KeyError, TypeError, OSError, subprocess.CalledProcessError) as error:
        print(f'replay failed: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
