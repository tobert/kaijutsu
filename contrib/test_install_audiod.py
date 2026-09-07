import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("installer", Path(__file__).with_name("install-audiod.py"))
installer = importlib.util.module_from_spec(spec)


class InstallerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec.loader.exec_module(installer)

    def test_unit_quotes_systemd_expansions(self):
        unit = installer.unit_text(["/path with spaces/bin", "--output", 'a"b\\c%h$HOME'])
        self.assertIn('"/path with spaces/bin"', unit)
        self.assertIn('"a\\"b\\\\c%%h$$HOME"', unit)

    def test_unit_rejects_newlines(self):
        with self.assertRaises(ValueError):
            installer.unit_text(["daemon", "bad\nExecStart=other"])

    def test_existing_key_is_not_regenerated(self):
        with tempfile.TemporaryDirectory() as directory:
            key = Path(directory) / "key"
            key.write_text("private")
            key.with_suffix(".pub").write_text("ssh-ed25519 public audio/test\n")
            with patch.object(installer, "run") as run:
                run.return_value.stdout = "ssh-ed25519 public\n"
                installer.ensure_key(key, "audio/test")
                self.assertEqual(key.read_text(), "private")
                self.assertFalse(any("-t" in call.args[0] for call in run.call_args_list))

    def test_mismatched_public_key_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            key = Path(directory) / "key"
            key.write_text("private")
            Path(str(key) + ".pub").write_text("ssh-ed25519 wrong\n")
            with patch.object(installer, "run") as run:
                run.return_value.stdout = "ssh-ed25519 correct\n"
                with self.assertRaises(RuntimeError):
                    installer.ensure_key(key, "audio/test")

    def test_service_restart_during_startup_is_failure(self):
        from types import SimpleNamespace
        with patch.object(installer, "run") as run:
            run.side_effect = [SimpleNamespace(stdout=value) for value in
                               ("first", "audio node running; kernel drives playback", "running", "second")]
            with self.assertRaises(RuntimeError):
                installer.wait_started()

    def test_install_copies_binary_backs_up_unit_and_starts_service(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "build/kaijutsu-audiod"
            binary.parent.mkdir()
            binary.write_text("test binary")
            binary.chmod(0o755)
            unit = root / "config/systemd/user/kaijutsu-audiod.service"
            unit.parent.mkdir(parents=True)
            unit.write_text("old unit")
            public = root / "test.pub"
            public.write_text("ssh-ed25519 test")
            argv = ["install-audiod.py", "--host", "zorak", "--no-midi", "--enrolled", "--binary", str(binary)]
            with patch.object(installer.sys, "argv", argv), \
                 patch.object(installer.Path, "home", return_value=root), \
                 patch.dict(installer.os.environ, {"XDG_CONFIG_HOME": str(root / "config")}), \
                 patch.object(installer, "ensure_key", return_value=public), \
                 patch.object(installer, "wait_started") as ready, \
                 patch.object(installer, "run") as run, \
                 patch("builtins.print"):
                installer.main()
            self.assertEqual((root / ".local/bin/kaijutsu-audiod").read_text(), "test binary")
            self.assertEqual(next(unit.parent.glob("*.bak")).read_text(), "old unit")
            self.assertIn('"--host" "zorak"', unit.read_text())
            run.assert_any_call(["systemctl", "--user", "restart", installer.UNIT])
            ready.assert_called_once()


if __name__ == "__main__":
    unittest.main()
