import unittest
from unittest.mock import patch

import music_tour


class MusicTourTest(unittest.TestCase):
    def test_tour_checks_control_before_changing_the_window(self):
        with patch.object(music_tour.brp, "rpc", return_value={"agent_control": False}) as rpc:
            with self.assertRaisesRegex(RuntimeError, "Agent control is off"):
                music_tour.run("score", [], 1)
        rpc.assert_called_once_with("kaijutsu/state")

    def test_default_tour_dwells_on_requested_seats_and_restores_layout(self):
        screen = "Heroic"
        context = "original"
        selected = "drums"
        mode = {"enabled": True, "panels_open": True}
        seats = ["drums", "bass", "pads"]
        holds = []

        def rpc(method, params=None):
            nonlocal screen, context, selected
            if method == "kaijutsu/state":
                return dict(agent_control=True, screen=screen, active_context_id=context,
                            heroic=dict(performers=seats, selected=selected))
            if method == "world.get_resources":
                return {"value": mode.copy()}
            if method == "kaijutsu/switch_context":
                context = params["context_id"]
                return {"context_id": context}
            action = params["action"]
            if action == "DetachToConversation":
                screen = "Conversation"
                selected = None
            elif action == "GoToHeroic":
                screen = "Heroic"
            elif action == "ToggleNeo":
                mode["enabled"] = not mode["enabled"]
            elif action == "ToggleConversationPanels":
                mode["panels_open"] = not mode["panels_open"]
            elif action.startswith("JumpSeat:"):
                selected = seats[int(action.split(":")[1])]

        def hold(seconds):
            holds.append((screen, selected, mode.copy(), seconds))

        with patch.object(music_tour.brp, "rpc", rpc), patch.object(music_tour.time, "sleep", hold):
            music_tour.run("score", ["bass", "pads", "drums"], 2)
        self.assertTrue(all(not m["enabled"] and not m["panels_open"] for _, _, m, _ in holds))
        heroic = [(seat, sec) for s, seat, _, sec in holds if s == "Heroic"]
        self.assertEqual([seat for seat, _ in heroic], [None, "bass", "pads", "drums"])
        self.assertGreater(sum(sec for _, sec in heroic), sum(h[-1] for h in holds if h[0] != "Heroic"))
        self.assertEqual((screen, context, selected, mode), ("Heroic", "original", "drums",
                                                           {"enabled": True, "panels_open": True}))
