import importlib.util
from pathlib import Path
import struct
import unittest

spec = importlib.util.spec_from_file_location("bridge", Path(__file__).with_name("rehearsal_midi_bridge.py"))
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class BridgeTest(unittest.TestCase):
    def test_latest_entry_for_channel_wins(self):
        group = bridge.Group(100, 10.0)
        group.add("old", {0}, b"old")
        group.add("drums", {9}, b"drums")
        group.add("new", {0}, b"new")
        self.assertEqual([x[0] for x in group.entries], ["drums", "new"])

    def test_late_group_is_missed(self):
        self.assertFalse(bridge.can_play(10.0, 10.051, 0.05))
        self.assertTrue(bridge.can_play(10.0, 10.049, 0.05))

    def test_group_start_is_based_on_tick_and_anchor(self):
        self.assertAlmostEqual(bridge.deadline(120, 100, 20.0, 130.0, 3.0), 32.2307692308)

    def test_subset_replaces_only_overlapping_players(self):
        players = [({0}, 'bass'), ({1}, 'pads'), ({9}, 'drums')]
        self.assertEqual(bridge.overlapping(players, {9}), [players[2]])
        self.assertEqual(bridge.overlapping(players, {0, 1}), players[:2])

    def test_stable_tempo_does_not_reanchor(self):
        self.assertEqual(bridge.reanchor((100, 5.0), 130, 130, 101, 5.46), (100, 5.0))

    def test_resume_at_same_tempo_establishes_new_anchor(self):
        self.assertEqual(bridge.reanchor(None, 130, 130, 101, 5.46), (101, 5.46))

    def test_midi_merge_preserves_channels_and_forces_tempo(self):
        def midi(channel):
            body = bytes([0, 0xff, 0x51, 3, 7, 0xa1, 0x20,
                          0, 0x90 | channel, 60, 100, 0x60, 0x80 | channel, 60, 0,
                          0, 0xff, 0x2f, 0])
            return b'MThd' + struct.pack('>IHHH', 6, 0, 1, 96) + b'MTrk' + struct.pack('>I', len(body)) + body
        entries = [('bass', {0}, midi(0)), ('drums', {9}, midi(9))]
        combined = bridge.merge(entries, 130)
        division, channels, tracks = bridge.prepare(combined, 130)
        self.assertEqual((division, channels, len(tracks)), (96, {0, 9}, 3))
        self.assertIn(b'\x90\x3c\x64', combined)
        self.assertIn(b'\x99\x3c\x64', combined)
        self.assertEqual(combined.count(b'\xff\x51\x03' + round(60000000 / 130).to_bytes(3, 'big')), 3)
        for channel, track in [(0, tracks[1]), (9, tracks[2])]:
            resets = b''.join(bytes([0, 0xb0 | channel, cc, 0]) for cc in (64, 123, 120))
            self.assertTrue(track.startswith(resets))
            self.assertLess(track.index(resets), track.index(bytes([0x90 | channel, 60, 100])))
            self.assertNotIn(bytes([0, 0xb0 | ((channel + 1) % 16), 64, 0]), track)


if __name__ == "__main__":
    unittest.main()
