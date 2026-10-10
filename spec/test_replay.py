"""Exercise the adapter and prove corrupted schedules fail in Quint itself."""
from datetime import datetime, timedelta, timezone
from pathlib import Path
import subprocess
import tempfile
import unittest

import replay


def event(index, kind, data=None, message='', seconds=None, task='task-1'):
    return dict(id=index, timestamp=(datetime(2026, 1, 1, tzinfo=timezone.utc)
                                    + timedelta(seconds=index if seconds is None else seconds)).isoformat(),
                task_id=task, kind=kind, data=data or {}, message=message)


def launched():
    return [event(1, 'task.created'), event(2, 'task.starting', {'attempt': 1}),
            event(3, 'session.launched', {'attempt': 1})]


class ProjectionTest(unittest.TestCase):
    def test_nanos_and_newest_first_input(self):
        rows = launched()
        rows[0]['timestamp'] = '2026-01-01T00:00:01.000000001Z'
        observations, _ = replay.project(list(reversed(rows)))
        self.assertEqual([o['op'] for o in observations], ['Create', 'Start', 'Launch'])

    def test_no_launch_is_not_success(self):
        with self.assertRaisesRegex(ValueError, 'no in-scope launches'):
            replay.project([event(1, 'task.created')])

    def test_unknown_lifecycle_event_is_not_silently_ignored(self):
        with self.assertRaisesRegex(ValueError, 'unmapped kind'):
            replay.project(launched() + [event(4, 'task.teleported')])

    def test_review_ends_prefix_and_never_reenters(self):
        rows = launched() + [event(4, 'task.in_review'), event(5, 'task.starting', {'attempt': 2})]
        observations, report = replay.project(rows)
        self.assertEqual(len(observations), 3)
        self.assertEqual(report['truncated_or_skipped_tasks'], {'prefix ends at task.in_review': 1})

    def test_duplicate_ids_fail(self):
        with self.assertRaisesRegex(ValueError, 'duplicate event IDs'):
            replay.project(launched() + [event(3, 'task.completed')])

    def test_linear_audit_echo_is_not_a_second_creation(self):
        rows = launched()
        rows[0]['data'] = {'identifier': 'EXAMPLE-1'}
        rows.insert(1, event(0, 'daemon.started', task=None, seconds=0))
        rows.insert(2, event(4, 'task.created', seconds=4))
        observations, report = replay.project(rows)
        self.assertEqual([o['op'] for o in observations], ['Create', 'Start', 'Launch'])
        self.assertEqual(report['stutters']['task.created (daemon echo of Linear sync)'], 1)


class QuintReplayTest(unittest.TestCase):
    def check_trace(self, rows, success):
        observations, _ = replay.project(rows)
        destination = replay.SPEC / '.generated'
        destination.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=destination) as directory:
            # The generated import is relative to .generated, so use the same
            # directory for test modules and isolate them by basename.
            path = destination / (Path(directory).name + '.qnt')
            try:
                path.write_text(replay.render(observations))
                result = subprocess.run([str(replay.SPEC / 'node_modules/.bin/quint'), 'test', str(path),
                                         '--max-samples=1'], capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
                if not success:
                    self.assertIn('Assertion failed', result.stdout + result.stderr)
            finally:
                path.unlink(missing_ok=True)

    def test_cli_completion_then_done_marker_is_valid(self):
        self.check_trace(launched() + [event(4, 'task.completed_by_command'), event(5, 'task.completed')], True)

    def test_cli_completion_then_finalize_is_valid(self):
        self.check_trace(launched() + [event(4, 'task.completed_by_command'), event(5, 'session.finalized')], True)

    def test_duplicate_dead_completion_is_rejected(self):
        self.check_trace(launched() + [event(4, 'task.completed'), event(5, 'task.completed')], False)

    def test_launch_without_start_is_rejected(self):
        self.check_trace([launched()[0], launched()[2]], False)

    def test_wrong_attempt_is_rejected(self):
        rows = launched()
        rows[-1]['data']['attempt'] = 2
        self.check_trace(rows, False)

    def test_restart_before_backoff_is_rejected(self):
        rows = launched() + [event(4, 'session.crashed', {'attempt': 1, 'max_attempts': 3},
                                  'dead; retrying in 10s (attempt 1 of 3)'),
                             event(5, 'task.starting', {'attempt': 2})]
        self.check_trace(rows, False)
        rows[-1]['timestamp'] = event(15, '')['timestamp']
        self.check_trace(rows, True)

    def test_giving_up_before_limit_is_rejected(self):
        self.check_trace(launched() + [event(4, 'task.failed', {'attempt': 1, 'max_attempts': 3})], False)

    def test_crash_at_limit_must_fail(self):
        self.check_trace(launched() + [event(4, 'session.crashed', {'attempt': 1, 'max_attempts': 1},
                                               'dead; retrying in 10s (attempt 1 of 1)')], False)

    def test_rate_limit_retains_session(self):
        self.check_trace(launched() + [event(4, 'budget.rate_limited', {'until': event(20, '')['timestamp']}),
                                      event(5, 'task.starting', {'attempt': 2}, seconds=30)], False)


if __name__ == '__main__':
    unittest.main()
