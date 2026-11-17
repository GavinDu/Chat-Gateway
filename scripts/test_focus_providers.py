"""Offline tests for reversible, secret-free provider routing focus."""
import json
import os
from pathlib import Path
import stat
import tempfile
import unittest

from focus_providers import FocusError, operate

ORIGINAL = """auth:
  keys:
    - name: local
      secret_hash: "not-a-real-secret"
server:
  port: 8080
providers:
  - name: qwen
    enabled: true # do not change the inline comment
    models: [qwen]
  - name: metaai
    enabled: true
    selectors:
      input: ['textarea[placeholder="Message"]']
  - name: claude
    enabled: true
  - name: chatgpt
    enabled: true
  - name: gemini
    enabled: true
  - name: grok
    enabled: true
  - name: mistral
    enabled: true
  - name: perplexity
    enabled: true
  - name: manus
    enabled: true
memory:
  max_tokens: 8000
"""
FOCUS = ("metaai", "chatgpt")


class ProviderFocusTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.config = Path(self.temp.name) / "config.yaml"
        self.config.write_text(ORIGINAL, encoding="utf-8")

    @property
    def manifest(self):
        return self.config.with_name("config.yaml.focus-flags.json")

    def test_dry_run_is_read_only_and_never_shows_config_or_secrets(self):
        result = operate(self.config, "dry_run")
        self.assertEqual(result["state"], "dry_run")
        self.assertEqual(result["disabled"], sorted([
            "qwen", "claude", "gemini", "grok", "mistral", "perplexity", "manus"
        ]))
        self.assertEqual(result["chatgpt"], "enabled")
        self.assertEqual(result["metaai"], "enabled")
        self.assertEqual(self.config.read_text(), ORIGINAL)
        self.assertFalse(self.manifest.exists())
        self.assertNotIn("not-a-real-secret", json.dumps(result))

    def test_apply_disables_seven_only_then_restore_is_exact(self):
        result = operate(self.config, "apply")
        self.assertEqual(result["state"], "applied")
        new = self.config.read_text()
        self.assertIn('    selectors:\n      input: [\'textarea[placeholder="Message"]\']', new)
        self.assertIn('secret_hash: "not-a-real-secret"', new)
        self.assertIn("enabled: false # do not change the inline comment", new)
        self.assertIn("- name: chatgpt\n    enabled: true", new)
        self.assertIn("- name: metaai\n    enabled: true", new)
        self.assertEqual(new.count("enabled: false"), 7)
        self.assertEqual(new.count("enabled: true"), 2)
        self.assertTrue(self.manifest.exists())
        self.assertNotIn("not-a-real-secret", self.manifest.read_text())
        self.assertEqual(set(json.loads(self.manifest.read_text())["flags"]), {
            "qwen", "metaai", "claude", "chatgpt", "gemini",
            "grok", "mistral", "perplexity", "manus"
        })
        self.assertEqual(stat.S_IMODE(self.config.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(self.manifest.stat().st_mode), 0o600)
        self.assertEqual(operate(self.config, "apply")["state"], "applied")
        self.assertEqual(operate(self.config, "restore")["state"], "restored")
        self.assertEqual(self.config.read_text(), ORIGINAL)
        self.assertFalse(self.manifest.exists())

    def test_never_auto_enables_target_from_example_defaults(self):
        config = ORIGINAL.replace(
            "- name: chatgpt\n    enabled: true",
            "- name: chatgpt\n    enabled: false"
        ).replace("- name: metaai\n    enabled: true",
                  "- name: metaai\n    enabled: false")
        self.config.write_text(config)
        result = operate(self.config, "apply")
        self.assertEqual(result["chatgpt"], "disabled_unchanged")
        self.assertEqual(result["metaai"], "disabled_unchanged")
        text = self.config.read_text()
        self.assertIn("- name: chatgpt\n    enabled: false", text)
        self.assertIn("- name: metaai\n    enabled: false", text)

    def test_idempotence_does_not_overwrite_original_flags(self):
        operate(self.config, "apply")
        original_snapshot = self.manifest.read_text()
        operate(self.config, "apply")
        self.assertEqual(self.manifest.read_text(), original_snapshot)
        operate(self.config, "restore")
        self.assertEqual(self.config.read_text(), ORIGINAL)

    def test_rejects_bad_layout_without_writing_any_files(self):
        variants = [
            ORIGINAL.replace("- name: metaai", "- name: qwen"),
            ORIGINAL.replace("- name: metaai\n    enabled: true",
                             "- name: metaai\n    enabled: null"),
            ORIGINAL.replace("- name: metaai\n    enabled: true",
                             "- name: metaai\n    enabled: true\n    enabled: true"),
            ORIGINAL.replace("providers:\n", "providers: []\n"),
            ORIGINAL.replace("providers:\n", "providers:\nproviders:\n"),
            ORIGINAL.replace("- name: metaai\n    enabled: true", ""),
            ORIGINAL.replace("- name: mistral", "- name: unknown-provider"),
        ]
        for variant in variants:
            with self.subTest(variant=variants.index(variant)):
                self.config.write_text(variant)
                with self.assertRaises(FocusError):
                    operate(self.config, "apply")
                self.assertEqual(self.config.read_text(), variant)
                self.assertFalse(self.manifest.exists())

    def test_refuses_symlink_and_changed_inventory(self):
        other = Path(self.temp.name) / "source.yaml"
        other.write_text(ORIGINAL)
        link = Path(self.temp.name) / "link.yaml"
        link.symlink_to(other)
        with self.assertRaises(FocusError):
            operate(link, "apply")
        self.assertFalse(Path(str(link) + ".focus-flags.json").exists())
        operate(self.config, "apply")
        changed = self.config.read_text().replace("  - name: manus\n    enabled: false\n", "")
        self.config.write_text(changed)
        with self.assertRaises(FocusError):
            operate(self.config, "restore")
        self.assertEqual(self.config.read_text(), changed)
        self.assertTrue(self.manifest.exists())

    def test_refuses_unsafe_manifest_and_modified_focus_state(self):
        operate(self.config, "apply")
        self.config.write_text(self.config.read_text().replace(
            "- name: qwen\n    enabled: false", "- name: qwen\n    enabled: true"
        ))
        with self.assertRaises(FocusError):
            operate(self.config, "restore")
        self.assertTrue(self.manifest.exists())
        self.config.write_text(self.config.read_text().replace(
            "- name: qwen\n    enabled: true", "- name: qwen\n    enabled: false"
        ))
        self.manifest.unlink()
        target = Path(self.temp.name) / "other"
        target.write_text('{"version":1,"flags":{}}')
        self.manifest.symlink_to(target)
        with self.assertRaises(FocusError):
            operate(self.config, "apply")
        self.assertTrue(self.manifest.is_symlink())


if __name__ == "__main__":
    unittest.main()
