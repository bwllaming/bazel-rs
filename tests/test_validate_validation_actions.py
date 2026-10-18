"""Hermetic N56 receipt guards: structure, pins and classifier contract.

The frozen paired receipt is immutable evidence produced once (plus one
determinism rerun) by artifacts/validation_actions-capture.py; no test here
executes genuine Bazel. Native behavior is covered by tests/validation_actions.rs
and by scripts/validate_validation_actions.py --native (explicit runs only).
"""
import copy
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import validate_validation_actions as suite


class ReceiptTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.receipt = suite.load_receipt()

    def test_schema_reference_and_fixture_pins(self):
        self.assertEqual(suite.structural_failures(self.receipt), [])
        self.assertEqual(self.receipt['reference']['sha256'], suite.GENUINE_SHA256)
        self.assertIn('--batch', self.receipt['reference']['startup'])
        self.assertIn('offline', self.receipt['reference'])

    def test_phase_plan_and_counts(self):
        self.assertEqual(tuple(p['id'] for p in self.receipt['phases']), suite.PHASE_IDS)
        self.assertEqual(suite.counts(self.receipt), suite.expected_counts())
        self.assertEqual(suite.expected_counts(),
                         {'exact': 16, 'native_unsupported': 2})

    def test_exact_phases_agree_on_rc_bin_and_reported_outputs(self):
        for phase in self.receipt['phases']:
            if phase['classification'] != 'exact':
                continue
            genuine, native = phase['genuine'], phase['native']
            self.assertEqual(genuine['returncode'], native['returncode'], phase['id'])
            self.assertEqual(genuine['bin'], native['bin'], phase['id'])
            self.assertEqual(genuine['reported'], native['reported'], phase['id'])

    def test_semantics_recorded_per_phase(self):
        bins = {p['id']: p['genuine']['bin'] for p in self.receipt['phases']}
        # Transitive collection: the dependency's validation outputs are built
        # with the dependent; --run_validations=false removes exactly them.
        self.assertTrue(any(k.endswith('leaf.validated') for k in bins['A01-transitive-collection']))
        self.assertFalse(any(k.endswith('.validated') or k.endswith('.mine')
                             for k in bins['A02-run-validations-false']))
        # `-_validation` cannot disable them (OutputGroupInfo.java:126-133).
        self.assertTrue(any(k.endswith('leaf.validated')
                            for k in bins['A03-minus-validation-quirk']))
        # --output_groups override/add/missing.
        override = set(bins['A04-output-groups-override'])
        self.assertTrue(any(k.endswith('g.extra') for k in override)
                        and not any(k.endswith('g.out') for k in override))
        add = set(bins['A05-output-groups-add'])
        self.assertTrue(any(k.endswith('g.extra') for k in add)
                        and any(k.endswith('g.out') for k in add))
        self.assertEqual(bins['A06-output-groups-missing'], {})
        # Tool dependencies and `_`-prefixed implicit attributes never
        # propagate; label_list_dict attributes do.
        self.assertFalse(any(k.endswith('leaf.validated')
                             for k in bins['A07-tool-dependency-not-collected']))
        self.assertFalse(any(k.endswith('leaf.validated')
                             for k in bins['A08-private-attribute-collected']))
        self.assertTrue(any(k.endswith('leaf.validated')
                            for k in bins['A09-label-list-dict-collected']))
        # Output-file targets delegate the generating rule's group.
        self.assertTrue(any(k.endswith('vout.validated')
                            for k in bins['A10-output-file-target']))
        self.assertTrue(any(k.endswith('vout.validated')
                            for k in bins['A11-output-file-consumer']))
        # Native rules collect too; a failing validation action fails the
        # build; the private _validation_transitive API is rejected.
        self.assertTrue(any(k.endswith('leaf.validated')
                            for k in bins['A12-native-rule-propagation']))
        self.assertEqual(bins['A13-failing-validation-action'], {})
        self.assertEqual(bins['A14-validation-transitive-private'], {})
        # Genuine aspect validation outputs run with the target; genuine
        # skip_validations needs Bazel's allowlist (both fail).
        aspect = bins['A16-aspect-validation']
        self.assertTrue(any(k.endswith('plain.aspect_validated') for k in aspect))
        self.assertEqual(bins['A15-skip-validations'], {})

    def test_native_unsupported_boundaries_are_explicit(self):
        for phase in self.receipt['phases']:
            if phase['classification'] == 'native_unsupported':
                self.assertIn(phase['id'], suite.NATIVE_UNSUPPORTED)
                self.assertTrue(phase['note'], phase['id'])
                self.assertNotEqual(phase['native']['returncode'], 0)

    def test_no_timed_out_phases(self):
        for phase in self.receipt['phases']:
            for engine in ('genuine', 'native'):
                if phase[engine]:
                    self.assertFalse(phase[engine]['timed_out'], phase['id'])

    def test_classifier_rejects_diverging_receipts(self):
        broken = copy.deepcopy(self.receipt)
        broken['phases'][0]['genuine']['bin'] = {'stale': 'x'}
        problems = suite.structural_failures(broken)
        self.assertTrue(any('exact with differing columns' in p for p in problems))

        broken = copy.deepcopy(self.receipt)
        broken['phases'][0]['classification'] = 'native_unsupported'
        problems = suite.structural_failures(broken)
        self.assertTrue(any('unexpected native_unsupported' in p for p in problems))

        broken = copy.deepcopy(self.receipt)
        broken['reference']['sha256'] = '0' * 64
        self.assertIn('reference sha256 pin', suite.structural_failures(broken))

    def test_reported_parser(self):
        self.assertEqual(suite._reported('//:g\t/cache/g.extra\n//:g\t/cache/g.out'),
                         ['g.extra', 'g.out'])
        self.assertEqual(suite._reported(''), [])


if __name__ == '__main__':
    unittest.main()
