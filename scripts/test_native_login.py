"""Pure startup-plan tests: no environment reads, subprocesses, profiles or keys."""
import importlib.util
from pathlib import Path
import unittest
import os
import subprocess
import tempfile

spec = importlib.util.spec_from_file_location(
    "embedded_launcher", Path(__file__).with_name("run-native-login.py"))
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)

class StartupPlanTests(unittest.TestCase):
    def test_rust_wrapper_clears_inherited_private_environment(self):
        with tempfile.TemporaryDirectory() as home:
            result = subprocess.run(
                ["bash", str(Path(__file__).with_name("native-rust.sh")),
                 "python3", "-B", "-c",
                 "import os; print(any(k in os.environ for k in "
                 "['NATIVE_WRAPPER_SENTINEL','CHAT_GATEWAY_API_KEY','CHAT_GATEWAY_VAULT_KEY']))"],
                env={"PATH": os.environ["PATH"], "HOME": home,
                     "NATIVE_WRAPPER_SENTINEL": "test-only"},
                capture_output=True, text=True, check=True)
            self.assertEqual(result.stdout.strip(), "False")

    def test_secret_mapping_values_are_never_read(self):
        class PresenceOnly(dict):
            def get(self, key, default=None):
                if key in ("CHAT_GATEWAY_API_KEY", "CHAT_GATEWAY_VAULT_KEY"):
                    raise AssertionError("Secret value read")
                return super().get(key, default)
        result = launcher.plan(PresenceOnly(CHAT_GATEWAY_API_KEY=None, CHAT_GATEWAY_VAULT_KEY=None))
        self.assertEqual(result[0], [])
        self.assertTrue(result[2])

    def test_missing_and_invalid_names_fail_closed(self):
        self.assertIn("CHAT_GATEWAY_API_KEY", launcher.plan({})[0])
        env = {"CHAT_GATEWAY_API_KEY": None, "CHAT_GATEWAY_VAULT_KEY": None}
        for name, value in [("CHAT_GATEWAY_AUTH_SECRET_HASH", "bad"),
                            ("CHAT_GATEWAY_REMOTE_ORIGIN", ""),
                            ("CHAT_GATEWAY_ENABLED_PROVIDERS", "chatgpt,unknown"),
                            ("CHAT_GATEWAY_ENABLED_PROVIDERS", "claude,claude")]:
            self.assertTrue(launcher.plan({**env, name: value})[0])

    def test_provider_opt_in_and_hash_override(self):
        env = {"CHAT_GATEWAY_VAULT_KEY": None,
               "CHAT_GATEWAY_AUTH_SECRET_HASH": "a" * 64,
               "CHAT_GATEWAY_ENABLED_PROVIDERS": "chatgpt, claude"}
        errors, _, from_env, providers = launcher.plan(env)
        self.assertEqual(errors, [])
        self.assertFalse(from_env)
        self.assertEqual(providers, ["chatgpt", "claude"])

if __name__ == "__main__":
    unittest.main()
