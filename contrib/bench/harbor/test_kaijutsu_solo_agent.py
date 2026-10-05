#!/usr/bin/env python3
"""Tests for the OTel settings in kaijutsu_solo_agent.py.

Harbor must be importable, so run with the Python Harbor is installed in:

    source /home/atobey/src/bench-work/harbor/env.sh
    harbor_python=$(head -1 "$(command -v harbor)" | sed 's/^#!//')
    "$harbor_python" -m unittest test_kaijutsu_solo_agent -v
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import kaijutsu_solo_agent as agent  # noqa: E402

ENDPOINT = "http://host.containers.internal:4317"
LOGS = Path("/jobs/kj-tb2-20/build-pmars__AbC1234/agent")
COMMIT = "0123456789abcdef"


def attrs(env: dict[str, str]) -> dict[str, str]:
    pairs = (p.split("=", 1) for p in env["OTEL_RESOURCE_ATTRIBUTES"].split(","))
    return dict(pairs)


class TestOtelEnv(unittest.TestCase):
    def test_unset_means_no_export(self):
        self.assertEqual(agent._otel_env(None, LOGS, COMMIT, False), {})
        self.assertEqual(agent._otel_env("  ", LOGS, COMMIT, False), {})

    def test_endpoint_and_resource_names(self):
        env = agent._otel_env(ENDPOINT, LOGS, COMMIT, False)
        self.assertEqual(env["OTEL_EXPORTER_OTLP_ENDPOINT"], ENDPOINT)
        self.assertEqual(
            attrs(env),
            {
                "harbor.job.name": "kj-tb2-20",
                "harbor.trial.name": "build-pmars__AbC1234",
                "harbor.task.name": "build-pmars",
                "kaijutsu.commit": COMMIT,
            },
        )

    def test_dirty_worktree_is_marked(self):
        env = agent._otel_env(ENDPOINT, LOGS, COMMIT, True)
        self.assertEqual(attrs(env)["kaijutsu.commit"], COMMIT + "-dirty")

    def test_no_commit_omits_the_attribute(self):
        env = agent._otel_env(ENDPOINT, LOGS, None, False)
        self.assertNotIn("kaijutsu.commit", attrs(env))

    def test_separators_in_names_are_escaped(self):
        logs = Path("/jobs/a,b=c/t__x/agent")
        env = agent._otel_env(ENDPOINT, logs, None, False)
        self.assertEqual(len(env["OTEL_RESOURCE_ATTRIBUTES"].split(",")), 3)
        self.assertEqual(attrs(env)["harbor.job.name"], "a%2Cb%3Dc")

    def test_loopback_endpoint_is_refused(self):
        for bad in ("http://127.0.0.1:4317", "http://localhost:4317", "http://[::1]:4317"):
            with self.assertRaises(ValueError, msg=bad):
                agent._otel_env(bad, LOGS, COMMIT, False)

    def test_non_url_endpoint_is_refused(self):
        with self.assertRaises(ValueError):
            agent._otel_env("host.containers.internal:4317", LOGS, COMMIT, False)

    def test_unexpected_log_directory_is_refused(self):
        with self.assertRaises(ValueError):
            agent._otel_env(ENDPOINT, Path("/jobs/j/t/step-1"), COMMIT, False)
        with self.assertRaises(ValueError):
            agent._otel_env(ENDPOINT, None, COMMIT, False)


class TestOtelFileExport(unittest.TestCase):
    def test_file_export_sets_the_directory_under_the_agent_logs(self):
        env = agent._otel_env(None, LOGS, COMMIT, False, file_export=True)
        self.assertEqual(env["KAIJUTSU_OTEL_FILE_DIR"], "/logs/agent/otel")
        self.assertNotIn("OTEL_EXPORTER_OTLP_ENDPOINT", env)
        self.assertEqual(attrs(env)["harbor.task.name"], "build-pmars")

    def test_file_export_off_leaves_the_environment_alone(self):
        self.assertEqual(agent._otel_env(None, LOGS, COMMIT, False, file_export=False), {})
        env = agent._otel_env(ENDPOINT, LOGS, COMMIT, False, file_export=False)
        self.assertNotIn("KAIJUTSU_OTEL_FILE_DIR", env)
        self.assertEqual(env["OTEL_EXPORTER_OTLP_ENDPOINT"], ENDPOINT)

    def test_both_exports_can_run_together(self):
        env = agent._otel_env(ENDPOINT, LOGS, COMMIT, False, file_export=True)
        self.assertEqual(env["OTEL_EXPORTER_OTLP_ENDPOINT"], ENDPOINT)
        self.assertEqual(env["KAIJUTSU_OTEL_FILE_DIR"], "/logs/agent/otel")

    def test_file_only_without_a_trial_directory_omits_the_names(self):
        env = agent._otel_env(None, None, COMMIT, False, file_export=True)
        self.assertEqual(env, {"KAIJUTSU_OTEL_FILE_DIR": "/logs/agent/otel"})

    def test_endpoint_still_requires_a_trial_directory_with_file_export(self):
        with self.assertRaises(ValueError):
            agent._otel_env(ENDPOINT, None, COMMIT, False, file_export=True)

    def test_default_is_on_and_the_knob_turns_it_off(self):
        self.assertTrue(agent._otel_file_enabled(None, None))
        for off in (False, "false", "0", "no", "off", " False "):
            self.assertFalse(agent._otel_file_enabled(off, None), off)
        for on in (True, "true", "1", "yes", "on"):
            self.assertTrue(agent._otel_file_enabled(on, None), on)
        self.assertFalse(agent._otel_file_enabled(None, "0"))
        self.assertTrue(agent._otel_file_enabled(True, "0"), "the option beats the variable")

    def test_unrecognized_knob_value_is_refused(self):
        with self.assertRaises(ValueError):
            agent._otel_file_enabled("maybe", None)


if __name__ == "__main__":
    unittest.main()
