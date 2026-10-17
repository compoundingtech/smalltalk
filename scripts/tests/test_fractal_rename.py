"""Pure tests for the display-string migration's exact literal allowlist."""
import pathlib
import runpy
import unittest

RENAME = runpy.run_path(str(pathlib.Path(__file__).resolve().parents[1] / 'fractal-rename'))


class DisplayStringsTest(unittest.TestCase):
    def test_all_rules_are_idempotent(self):
        paths = []
        for _, path, literals in RENAME['DISPLAY_RULES']:
            paths.append(path)
            text = '\n'.join(literals)
            rewritten, count = RENAME['rewrite_display'](text, literals)
            self.assertGreater(count, 0, path)
            self.assertNotRegex(rewritten, r'Smalltalk|Small Talk', path)
            self.assertEqual(RENAME['rewrite_display'](rewritten, literals), (rewritten, 0), path)
        self.assertEqual(len(paths), len(set(paths)))
        rules = {path: literals for _, path, literals in RENAME['DISPLAY_RULES']}
        self.assertEqual(rules['crates/st3-schema/src/lib.rs'], rules['docs/st3/schema.md'])

    def test_durable_identities_and_stored_names_are_not_rules(self):
        preserved = '''smalltalk com.compoundingtech.smalltalk SmalltalkRuntime
agent/st/assistant /Apps/SmallTalk.app Smalltalk Ci
ST_SMALLTALK Smalltalk tag release Smalltalk daily release'''
        for _, path, literals in RENAME['DISPLAY_RULES']:
            self.assertEqual(RENAME['rewrite_display'](preserved, literals), (preserved, 0), path)
        paths = {path for _, path, _ in RENAME['DISPLAY_RULES']}
        self.assertNotIn('apps/ios/agentsView.test.mjs', paths)
        self.assertNotIn('crates/st3/src/config.rs', paths)
        self.assertNotIn('fixtures/clients/transcripts/claude.json', paths)

    def test_display_name_does_not_rename_bundle_name(self):
        text = '''  <key>CFBundleName</key>
  <string>Small Talk</string>
  <key>CFBundleDisplayName</key>
  <string>Small Talk</string>'''
        literals = next(literals for _, path, literals in RENAME['DISPLAY_RULES'] if path == 'apps/macos/listen/Info.plist')
        rewritten, count = RENAME['rewrite_display'](text, literals)
        self.assertEqual(count, 1)
        self.assertIn('<key>CFBundleName</key>\n  <string>Small Talk</string>', rewritten)
        self.assertIn('<key>CFBundleDisplayName</key>\n  <string>Fractal</string>', rewritten)


if __name__ == '__main__':
    unittest.main()
