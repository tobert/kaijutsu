import importlib.util
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('desk', 'contrib/rehearsal_desk.py')
desk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(desk)


class DeskTest(unittest.TestCase):
    def test_opt_in_persists_and_restores(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = desk.Desk(tmp + '/state')
            with patch.object(desk.Desk, 'identity', return_value='123'), patch.object(desk.Desk, 'send') as send:
                with self.assertRaises(ValueError):
                    d.set_control('bass', 'pan', 70)
                d.transaction('bass', 'pan', 64, establish=True)
                with self.assertRaises(ValueError):
                    d.transaction('bass', 'pan', 63, establish=True)
                d = desk.Desk(tmp + '/state')
                d.set_control('bass', 'pan', 70)
                d.restore('bass', 'pan')
                self.assertEqual(send.call_args_list[-1].args, (0, 10, 64))
                with self.assertRaises(ValueError):
                    d.restore('bass', 'pan')

    def test_scope_and_replaced_synth(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = desk.Desk(tmp + '/state')
            with patch.object(d, 'identity', return_value='123'), patch.object(d, 'send'):
                for args in [('other', 'volume', 1), ('bass', 'program', 1), ('bass', 'volume', 128), ('bass', 'volume', True)]:
                    with self.assertRaises(ValueError):
                        d.transaction(*args, establish=True)
                d.transaction('chords', 'expression', 127, establish=True)
            with patch.object(d, 'identity', return_value='456'), patch.object(d, 'send') as send:
                with self.assertRaises(ValueError):
                    d.set_control('chords', 'expression', 90)
                send.assert_not_called()

    def test_reestablish_after_synth_restart(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = desk.Desk(tmp + '/state')
            with patch.object(d, 'identity', return_value='123'), patch.object(d, 'send'):
                d.transaction('bass', 'pan', 64, establish=True)
            with patch.object(d, 'identity', return_value='456'), patch.object(d, 'send') as send:
                d.transaction('bass', 'pan', 65, establish=True)
                send.assert_called_once_with(0, 10, 65)
                with open(d.state) as f:
                    self.assertEqual(desk.json.load(f)['bass:pan']['pid'], '456')

    def test_compact_journal_status_and_tick_spread(self):
        lines = [
            {'MESSAGE': 'playing a#1 hash channels [0] bpm 130 pid 1 tick 42', '__REALTIME_TIMESTAMP': '1000000'},
            {'MESSAGE': 'playing b#1 hash channels [1] bpm 130 pid 2 tick 42', '__REALTIME_TIMESTAMP': '1012000'},
            {'MESSAGE': 'playing c#1 hash channels [9] bpm 130 pid 3 tick 42', '__REALTIME_TIMESTAMP': '1020000'},
        ]
        result = desk.journal_summary('\n'.join(desk.json.dumps(x) for x in lines))
        self.assertEqual(result['latest_shared_tick']['spread_ms'], 20)
        self.assertEqual(result['latest_shared_tick']['tick'], '42')
        self.assertEqual(len(result['launches']), 3)
        del lines[0]['__REALTIME_TIMESTAMP']
        self.assertIsNone(desk.journal_summary(desk.json.dumps(lines[0]))['latest_shared_tick'])

    def test_grouped_bridge_events(self):
        lines = [{'MESSAGE': 'playing tick=42 blocks=a#1,b#1,c#1 bpm=130 pid=55', '__REALTIME_TIMESTAMP': '1000000'},
                 {'MESSAGE': 'miss tick=43 reason=late', '__REALTIME_TIMESTAMP': '2000000'}]
        result = desk.journal_summary('\n'.join(desk.json.dumps(x) for x in lines))
        self.assertEqual(result['grouped_launches'][0]['tick'], '42')
        self.assertEqual(result['misses'][0]['tick'], '43')
        self.assertIsNone(result['latest_shared_tick'])

    def test_newest_clocked_group_supplies_its_measured_launch_spread(self):
        rows = [
            {'MESSAGE': 'playing tick=84 blocks=a,b,c channels=[0, 1, 9] bpm=130.0 launch_spread_ms=0.7', '__REALTIME_TIMESTAMP': '2000'},
            {'MESSAGE': 'playing tick=20 blocks=a,b channels=[0, 1] bpm=130.0 launch_spread_ms=1.0', '__REALTIME_TIMESTAMP': '1000'},
        ]
        result = desk.journal_summary('\n'.join(map(desk.json.dumps, rows)))
        self.assertEqual(result['latest_shared_tick'], {'tick': '84', 'spread_ms': 0.7})
        preview = desk.json.dumps(result)[:180]
        self.assertIn('"spread_ms": 0.7', preview)
        self.assertIn('"miss_count_in_window": 0', preview)

    def test_previous_bridge_misses_are_not_current(self):
        rows = [
            {'MESSAGE': 'miss tick=20 reason=late', '_SYSTEMD_INVOCATION_ID': 'old', '__REALTIME_TIMESTAMP': '1000'},
            {'MESSAGE': 'bridge ready delay_seconds=4', '_SYSTEMD_INVOCATION_ID': 'live', '__REALTIME_TIMESTAMP': '2000'},
        ]
        result = desk.journal_summary('\n'.join(map(desk.json.dumps, rows)))
        self.assertEqual(result['misses'], [])
        self.assertEqual(result['bridge_invocation'], 'live')

    def test_failed_send_retains_baseline(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = desk.Desk(tmp + '/state')
            with patch.object(d, 'identity', return_value='123'), patch.object(d, 'send', side_effect=RuntimeError('failed')):
                with self.assertRaises(RuntimeError):
                    d.transaction('drums', 'expression', 127, establish=True)
            with open(d.state) as f:
                self.assertEqual(desk.json.load(f)['drums:expression']['baseline'], 127)

    def test_schema_and_protocol(self):
        result = desk.dispatch(desk.Desk(), 'tools/list', {})
        self.assertEqual({x['name'] for x in result['tools']},
                         {'rehearsal_status', 'chair_set', 'chair_restore'})
        self.assertTrue(desk.dispatch(desk.Desk(), 'tools/call',
            {'name': 'chair_restore', 'arguments': {'chair': 'bad', 'control': 'pan'}})['isError'])


if __name__ == '__main__':
    unittest.main()
