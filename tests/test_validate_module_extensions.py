"""Harness-only unit tests; never launch native or genuine Bazel.

Synthetic records test evidence/diagnostic guards, NOT execution parity. Python
AST checks only quote/indentation mistakes in the shared lexical fixture subset;
they do not assert that Python implements Starlark or that Bazel accepted it.
"""

import ast
import base64
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import validate_module_extensions as extensions


def record(stdout='', stderr='', returncode=0, **changes):
    return dict(id=0, name='synthetic-unit-record-NOT-CLI-evidence', stdout=stdout, stderr=stderr,
                returncode=returncode, timed_out=False, **changes)


MESSAGES = {
    'missing': "mandatory attribute 'required' missing",
    'unknown': "unknown repository attribute 'unknown_attr'",
    'rollback': 'Error in fail: ' + extensions.FAIL_MARKER,
    'import': 'extension does not generate repository "missing_repo", yet it is imported as "unused"',
    'extension-label': "invalid label for module extension: no repo visible as '@shared+' here",
    'cycle': 'repository preparation cycle: a_repo -> b_repo -> a_repo',
    'return': 'module extension implementation return values other than None are unsupported',
    'isolated': 'isolated extension usage is not supported',
    'immutable': 'Immutable',
    'name': 'invalid repository name "a+b"',
    'canonical': 'unknown canonical repository @@+unknown+made; extension repository not materialized',
    'ticks': 'Maximum number of ticks exceeded',
    'extension_metadata': "module_ctx has no attribute 'extension_metadata'",
    'facts': "module_ctx has no attribute 'facts'",
    'os-environ': ('bazel-rs: module extension //:ext.bzl: Traceback (most recent call last):\n'
                   '  File <builtin>, in <module>\n'
                   'error: repository_os.environ is unsupported in the native host metadata profile\n'
                   ' --> //:ext.bzl:4:11\n'
                   '  |\n4 |     value=ctx.os.environ\n  |           ^^^^^^^^^^^^^^\n  |\n'),
    'read': "module_ctx has no attribute 'read'",
}


class FixtureTests(unittest.TestCase):
    def test_fixed_unique_plan_eight_differentials_eight_boundaries(self):
        self.assertEqual(len(extensions.CASES), 16)
        self.assertEqual(len(set(n for n, _ in extensions.PLAN)), 16)
        self.assertEqual([c['kind'] for c in extensions.CASES], ['differential'] * 8 + ['native_boundary'] * 8)
        for item in extensions.CASES:
            self.assertTrue(item['probes'])
            self.assertEqual(len(item['probes']), len(set(item['probes'])))
            for probe, engine in extensions.phases_for(item):
                steps = extensions.steps_for(item, probe, engine)
                self.assertTrue(steps)
                self.assertFalse(any(s[0] in ('test', 'run') for s in steps))
        self.assertEqual(sum(len(c['probes']) for c in extensions.CASES), 38)
        self.assertEqual(sum(len(extensions.phases_for(c)) for c in extensions.CASES), 51)
        self.assertEqual(sum(len(extensions.steps_for(c, probe, engine)) for c in extensions.CASES
                             for probe, engine in extensions.phases_for(c)), 107)

    def test_unknown_fixture_names_or_probe_never_fall_back_to_baseline(self):
        for name, probe in [('made-up', 'result'), (extensions.CASES[0]['name'], 'made-up')]:
            with self.assertRaisesRegex(ValueError, 'unknown planned'):
                extensions.fixture(name, probe)

    def test_all_sources_have_valid_python_compatible_lexical_structure(self):
        for item in extensions.CASES:
            for probe in item['probes']:
                value = extensions.fixture(item['name'], probe)
                for path, raw in value['files'].items():
                    if path.endswith(('.bzl', '.bazel')):
                        with self.subTest(case=item['name'], probe=probe, path=path):
                            ast.parse(raw.decode(), filename=path)
                            self.assertTrue(raw.isascii())
                            self.assertNotIn(b'\\x', raw)
                            self.assertNotIn(b'\\u', raw)
                            self.assertNotIn(b'\\U', raw)

    def test_control_quoting_uses_octal_and_binary_is_not_embedded_source(self):
        value = '\x00\x01"\\\r\n\x7f'
        quoted = extensions.quote(value)
        self.assertEqual(ast.literal_eval(quoted), value)
        self.assertIn('\\000', quoted)
        self.assertNotIn('\\x', quoted)
        with self.assertRaises(ValueError):
            extensions.quote('\xff')
        fixture = extensions.fixture('native.refresh-and-repair', 'cold')
        self.assertEqual(fixture['files']['input'], b'A B\r\nraw:\xff\x00\r\n')
        self.assertIn(b'ctx.template', fixture['files']['rules.bzl'])
        self.assertNotIn(b'\x00', fixture['files']['ext.bzl'])

    def test_computed_name_reaches_real_private_export_through_helper(self):
        value = extensions.fixture('computed.transitive-private-rule')
        self.assertIn(b'_private = repository_rule', value['files']['leaf.bzl'])
        self.assertIn(b'maker=_private', value['files']['leaf.bzl'])
        self.assertIn(b'load(":leaf.bzl",_maker="maker")', value['files']['helper.bzl'])
        self.assertIn(b'return _maker(name=name)', value['files']['helper.bzl'])
        self.assertIn(b'declare("ma"+"de")', value['files']['ext.bzl'])
        self.assertIn(b'result != None', value['files']['ext.bzl'])
        self.assertIn(b'doc=None, environ=[], os_dependent=False, arch_dependent=False, facts_version=0', value['files']['ext.bzl'])
        self.assertEqual(value['canonical'], '+files+made')
        self.assertIn(b'genrule(', value['files']['BUILD.bazel'])
        self.assertIn(b'cat $(SRCS) > $@', value['files']['BUILD.bazel'])

    def test_typed_tags_none_private_custom_name_and_call_time_snapshot(self):
        value = extensions.fixture('tags.typed-none-and-call-snapshot', 'valid')
        module, source = value['files']['MODULE.bazel'], value['files']['ext.bzl']
        self.assertIn(b'_private="secret",text=None,unknown=None', module)
        for text in (b'attr.string(mandatory=True)', b'attr.int(default=7)', b'attr.bool(default=True)',
                     b'attr.label()', b'attr.string_list(', b'ctx.is_dev_dependency(t)', b't.optional != None',
                     b't._private != "secret"', b't.name != "made"', b'ctx.modules[0].tags.empty != []'):
            self.assertIn(text, source)
        self.assertLess(source.index(b'maker(**args)'), source.index(b'words.append("after")'))
        self.assertLess(source.index(b'maker(**args)'), source.index(b'args["text"]="after"'))
        self.assertEqual(value['expected']['data'], (b'before|DEFAULT', False))
        for unsupported in (b'attr.int_list', b'attr.string_dict', b'attr.string_list_dict'):
            self.assertNotIn(unsupported, source + value['files']['rules.bzl'])
        negative = extensions.fixture('tags.typed-none-and-call-snapshot', 'mandatory-none')
        self.assertIn(b'required=None', negative['files']['MODULE.bazel'])
        self.assertEqual(negative['diagnostic'], 'missing')

    def test_callback_failure_precedes_any_repo_fetch(self):
        value = extensions.fixture('callback.whole-group-rollback')
        self.assertIn(b'maker(name="made")\n    maker(name="unused")', value['files']['ext.bzl'])
        self.assertIn(extensions.FAIL_MARKER.encode(), value['files']['ext.bzl'])
        self.assertIn(extensions.LAZY_MARKER.encode(), value['files']['rules.bzl'])
        self.assertEqual(value['diagnostic'], 'rollback')

    def test_group_validates_unused_attrs_but_unused_callback_is_lazy(self):
        item = next(c for c in extensions.CASES if c['name'] == 'group.validate-all-fetch-demanded')
        self.assertEqual(item['probes'], ['unused-valid', 'missing', 'unknown'])
        for probe in item['probes']:
            value = extensions.fixture(item['name'], probe)
            self.assertIn(b'maker(name="made",required="actual")', value['files']['ext.bzl'])
            self.assertIn(b'ctx.original_name == "unused"', value['files']['rules.bzl'])
            self.assertIn(extensions.LAZY_MARKER.encode(), value['files']['rules.bzl'])
            self.assertEqual(value['diagnostic'], None if probe == 'unused-valid' else probe)
        missing = extensions.fixture('imports.unused-missing')
        self.assertIn(b'use_repo(e,unused="missing_repo")', missing['files']['MODULE.bazel'])
        self.assertEqual(missing['target'], '@chosen//:data')
        self.assertEqual(missing['diagnostic'], 'import')

    def test_bfs_aliases_zero_tag_users_declared_versions_and_dev_flags(self):
        value = extensions.fixture('participants.bfs-alias-dev-version', 'valid')
        files = value['files']
        self.assertIn(b'other=use_extension("@@shared//:ext.bzl","files")', files['MODULE.bazel'])
        self.assertIn(b'e=use_extension("@shared//:ext.bzl","files")', files['MODULE.bazel'])
        self.assertNotIn(b'@@shared+//:ext.bzl', files['MODULE.bazel'])
        self.assertIn(b'version="1.0+stamp"', files['MODULE.bazel'])
        self.assertIn(b'version="2.0+stamp"', files['z/MODULE.bazel'])
        self.assertIn(b'e=use_extension', files['a/MODULE.bazel'])
        self.assertNotIn(b'e.note', files['a/MODULE.bazel'])
        self.assertNotIn(b'use_extension', files['shared/MODULE.bazel'])
        self.assertIn(b'dev.note(value="ignored")', files['z/MODULE.bazel'])
        self.assertIn(b'ctx.root_module_has_non_dev_dependency', files['shared/ext.bzl'])
        self.assertEqual(value['flags'], [])  # ignore-dev would also suppress root overrides
        self.assertEqual(value['expected']['data'][0],
            b'main:1.0:True:first:False,second:False,third:False,dev:True|z:2.0:False:z:False|a:5:False:|deep:3.0:False:deep:False;root=True')
        self.assertEqual(value['canonical'], 'shared++files+made')

    def test_bfs_canonical_looking_extension_label_is_a_real_negative_differential(self):
        name = 'participants.bfs-alias-dev-version'
        item = next(c for c in extensions.CASES if c['name'] == name)
        self.assertEqual(item['kind'], 'differential')
        self.assertEqual(item['probes'], ['valid', 'invalid-canonical'])
        self.assertEqual(extensions.phases_for(item), [('valid', 'reference'), ('valid', 'native'),
            ('invalid-canonical', 'reference'), ('invalid-canonical', 'native')])
        valid = extensions.fixture(name, 'valid')
        invalid = extensions.fixture(name, 'invalid-canonical')
        self.assertIsNone(valid['diagnostic'])
        self.assertEqual(invalid['diagnostic'], 'extension-label')
        self.assertEqual(invalid['files']['MODULE.bazel'], valid['files']['MODULE.bazel'].replace(
            b'@@shared//:ext.bzl', b'@@shared+//:ext.bzl'))
        for path in valid['files']:
            if path not in ('MODULE.bazel', 'BUILD.bazel'):
                self.assertEqual(valid['files'][path], invalid['files'][path])
        for engine in ('reference', 'native'):
            self.assertEqual(extensions.steps_for(item, 'invalid-canonical', engine), [('query', '@chosen//:data', [])])
        # Extension-label normalization must not alter actual canonical outputs.
        self.assertEqual(valid['canonical'], 'shared++files+made')
        extensions.canonical_result(record('@@shared++files+made//:data\n'), valid['canonical'])

    def test_labels_are_real_module_helper_public_and_forward_sibling_owners(self):
        value = extensions.fixture('labels.owners-and-forward-sibling')
        source = value['files']['pkg/ext.bzl']
        self.assertIn(b'raw="@sibling//:data",explicit=Label("@sibling//:data")', source)
        self.assertLess(source.index(b'name="made"'), source.index(b'name="sibling"'))
        self.assertNotIn(b'use_repo(e,"sibling")', value['files']['MODULE.bazel'])
        self.assertIn(b'fallback":attr.label(default=":input")', value['files']['helper/tags.bzl'])
        self.assertIn(b'relative=":local"', source)
        self.assertEqual({k: raw for k, (raw, _) in value['expected'].items()},
            {'data': b'GENERATED\n', 'explicit': b'HOST\n', 'supplied': b'ROOT\n',
             'fallback': b'HELPER\n', 'relative': b'PUBLIC\n'})

    def test_private_extension_and_same_object_public_alias_keep_requested_identity(self):
        private = extensions.fixture('identity.private-and-public-alias', 'private')
        public = extensions.fixture('identity.private-and-public-alias', 'public')
        self.assertEqual(private['files']['ext.bzl'], public['files']['ext.bzl'])
        self.assertIn(b'_private=module_extension', private['files']['ext.bzl'])
        self.assertIn(b'files=_private', private['files']['ext.bzl'])
        self.assertEqual(private['files']['MODULE.bazel'], public['files']['MODULE.bazel'])
        self.assertEqual([private['canonical'], public['canonical']], ['+_private+made', '+files+made'])
        self.assertNotEqual(private['target'], public['target'])

    def test_both_preparation_cycles_are_real_load_edges(self):
        for probe in ('ordinary', 'direct'):
            value = extensions.fixture('native.preparation-cycles', probe)
            self.assertIn(b'load("@b_repo//:helper.bzl","value")', value['files']['a.bzl'])
            self.assertIn(b'load("@a_repo//:helper.bzl","value")', value['files']['b.bzl'])
            self.assertEqual(value['diagnostic'], 'cycle')
            self.assertIn(b'use_repo_rule' if probe == 'direct' else b'b=use_extension', value['files']['MODULE.bazel'])

    def test_native_contexts_and_non_none_returns_are_not_parity(self):
        item = next(c for c in extensions.CASES if c['name'] == 'native.context-and-return')
        self.assertEqual(item['kind'], 'native_boundary')
        self.assertEqual(item['probes'], ['metadata', 'facts', 'os-environ', 'read', 'integer', 'dict'])
        for probe in item['probes']:
            value = extensions.fixture(item['name'], probe)
            self.assertIn(b'maker(name="made")', value['files']['ext.bzl'])
            self.assertEqual(extensions.steps_for(item, probe, 'native'), [('query', '@chosen//:data', [])])
        self.assertIn(b'return 42', extensions.fixture(item['name'], 'integer')['files']['ext.bzl'])
        self.assertIn(b'return {}', extensions.fixture(item['name'], 'dict')['files']['ext.bzl'])

    def test_os_environ_reaches_host_property_after_recording_without_repo_execution(self):
        value = extensions.fixture('native.context-and-return', 'os-environ')
        source = value['files']['ext.bzl']
        self.assertEqual(value['diagnostic'], 'os-environ')
        self.assertIn(b'maker(name="made")\n    value=ctx.os.environ\n', source)
        self.assertLess(source.index(b'value=ctx.os.environ'), source.index(extensions.FAIL_MARKER.encode()))
        self.assertIn(extensions.LAZY_MARKER.encode(), value['files']['rules.bzl'])
        self.assertNotIn(b'ctx.file', value['files']['rules.bzl'])
        self.assertNotIn(b'ctx.os.name', source)
        self.assertNotIn(b'print(', value['files']['BUILD.bazel'])
        item = next(c for c in extensions.CASES if c['name'] == 'native.context-and-return')
        self.assertNotIn(('os-environ', 'reference'), extensions.phases_for(item))
        with self.assertRaisesRegex(ValueError, 'unknown planned'):
            extensions.fixture(item['name'], 'os')

    def test_isolation_demand_boundary_does_not_force_unused_definition(self):
        for probe in ('demanded', 'unused'):
            value = extensions.fixture('native.isolated-on-demand', probe)
            self.assertIn(b'isolate=True', value['files']['MODULE.bazel'])
            self.assertEqual(value['main_only'], probe == 'unused')
            self.assertEqual(value['diagnostic'], 'isolated' if probe == 'demanded' else None)

    def test_immutability_has_three_independent_nested_projection_probes(self):
        for probe, expression in [('modules', 'ctx.modules'), ('tags', 'ctx.modules[0].tags.make'),
                                  ('words', 'ctx.modules[0].tags.make[0].words')]:
            value = extensions.fixture('native.immutable-projections', probe)
            self.assertIn((expression + '.append(None)').encode(), value['files']['ext.bzl'])
            self.assertEqual(value['diagnostic'], 'immutable')

    def test_names_unknown_prefix_and_unused_definition_are_independent(self):
        invalid = extensions.fixture('native.names-and-laziness', 'invalid-name')
        self.assertIn(b'maker(name="a+b")', invalid['files']['ext.bzl'])
        for probe in ('unknown-canonical', 'main-only'):
            value = extensions.fixture('native.names-and-laziness', probe)
            self.assertIn(extensions.LOAD_MARKER.encode(), value['files']['ext.bzl'])
            self.assertEqual(value['target'], '@@+unknown+made//:data' if probe == 'unknown-canonical' else '//:data')

    def test_budget_probes_are_full_initialization_and_post_record_callback(self):
        init = extensions.fixture('native.tick-budgets', 'initialization')['files']['ext.bzl']
        callback = extensions.fixture('native.tick-budgets', 'callback')['files']['ext.bzl']
        self.assertLess(init.index(b'INITIALIZATION='), init.index(b'def _impl'))
        self.assertLess(callback.index(b'maker(name="made")'), callback.index(b'values=['))
        self.assertIn(b'range(1000000)', init)
        self.assertIn(b'range(1000000)', callback)


class DiagnosticTests(unittest.TestCase):
    def test_captured_missing_import_diagnostics_include_exact_native_declaration_failure(self):
        # Captured in module-extensions-validation-first-failure.json. Retain
        # literal diagnostic text without depending on that optional artifact.
        messages = [
            'ERROR: Target parsing failed due to unexpected exception: module extension @@//:ext.bzl%files '
            'does not generate repository "missing_repo", yet it is imported as "unused" in the usage at '
            '/tmp/opencode/module-extensions-n7qcswvs/workspace/MODULE.bazel:2:16\n',
            'bazel-rs: module extension //:ext.bzl%files did not declare imported repository "missing_repo" '
            '(alias @unused in MODULE owner "")\n',
        ]
        for text in messages:
            extensions.require_rejection(record(stderr=text, returncode=1), 'import')
            for prefix in ('download failed', 'panicked at', 'timeout', 'syntax error', extensions.LAZY_MARKER):
                with self.subTest(text=text, prefix=prefix), self.assertRaises(ValueError):
                    extensions.require_rejection(record(stderr=prefix + '\n' + text, returncode=1), 'import')
        for text in ('did not declare imported repository "other_repo"',
                     'did not declare repository "missing_repo"',
                     'did not declare imported repository "missing_repo_extra"',
                     'error resolving import "missing_repo"'):
            with self.subTest(text=text), self.assertRaises(ValueError):
                extensions.require_rejection(record(stderr=text, returncode=1), 'import')

    def test_extension_label_classifier_requires_the_exact_unmapped_shared_plus_alias(self):
        # Reference text is captured; native text is its precise label-mapping
        # diagnostic contract, not a claim that the corrected CLI has run yet.
        messages = [
            'ERROR: Error computing the main repository mapping: invalid label for module extension found at '
            '/tmp/opencode/module-extensions-n7qcswvs/workspace/MODULE.bazel:10:20: no repo visible as \'@shared+\' here\n',
            'unknown apparent repository @shared+ from canonical repository ""; '
            'extension repository not materialized or dependency not declared',
        ]
        for text in messages:
            extensions.require_rejection(record(stderr=text, returncode=1), 'extension-label')
            for prefix in ('download failed', 'connection refused', 'panicked at', 'timeout', 'parse error'):
                with self.subTest(text=text, prefix=prefix), self.assertRaises(ValueError):
                    extensions.require_rejection(record(stderr=prefix + '\n' + text, returncode=1), 'extension-label')
        for text in ("no repo visible as '@shared' here", "no repo visible as '@shared++' here",
                     'unknown apparent repository @shared+other from canonical repository ""',
                     'unknown canonical repository @@shared+', 'invalid label for module extension',
                     'unknown apparent repository @other from canonical repository ""'):
            with self.subTest(text=text), self.assertRaises(ValueError):
                extensions.require_rejection(record(stderr=text, returncode=1), 'extension-label')

    def test_synthetic_specific_diagnostics_accept_only_the_intended_class(self):
        self.assertEqual(set(MESSAGES), set(extensions.DIAGNOSTICS))
        for diagnostic, text in MESSAGES.items():
            with self.subTest(diagnostic=diagnostic):
                extensions.require_rejection(record(stderr=text, returncode=1), diagnostic)
                for other, wrong in MESSAGES.items():
                    if other != diagnostic:
                        with self.assertRaises(ValueError):
                            extensions.require_rejection(record(stderr=wrong, returncode=1), diagnostic)

    def test_negative_timeout_crash_network_syntax_cannot_pass_with_expected_marker(self):
        for diagnostic, text in MESSAGES.items():
            good = record(stderr=text, returncode=1)
            changes = [{'returncode': c} for c in (0, None, True, -9, 101, 124, 125, 126, 127, 139)]
            changes += [{'timed_out': True}, {'spawn_error': 'missing binary'}, {'interrupted': True},
                        {'output_truncated': True}, {'stdout': '//:data\n'}]
            changes += [{'stderr': prefix + '\n' + text} for prefix in (
                'network unavailable', 'download failed', 'could not download', 'connection refused',
                'failed to download', 'Error downloading',
                'SSLHandshakeException', 'UnknownHost', 'panicked at', 'FATAL:', 'internal error',
                'timeout', 'parse error', 'unexpected token', 'syntax error', 'falling back',
                'permission denied', extensions.LAZY_MARKER, extensions.LOAD_MARKER)]
            for change in changes:
                with self.subTest(diagnostic=diagnostic, change=change), self.assertRaises(ValueError):
                    extensions.require_rejection(dict(good, **change), diagnostic)

    def test_os_environ_rejects_obsolete_missing_os_and_other_field_type_errors(self):
        text = MESSAGES['os-environ']
        for wrong in ("module_ctx has no attribute 'os'", "module_ctx has no attribute 'name'",
                      "repository_os has no attribute 'environ'", "repository_os has no attribute 'name'",
                      text.replace('repository_os.environ', 'module_ctx.environ'),
                      text.replace('repository_os.environ', 'repository_os.name'),
                      text.replace('repository_os.environ', 'repository_os.arch'),
                      text.replace('error: repository_os', 'note: repository_os'),
                      text.replace('is unsupported', 'is Unsupported'),
                      text.replace('metadata profile', 'metadata profile extra'),
                      text+'error: Other\n'):
            with self.subTest(wrong=wrong), self.assertRaises(ValueError):
                extensions.require_rejection(record(stderr=wrong, returncode=1), 'os-environ')

    def test_os_environ_requires_extension_getter_site_not_another_callback(self):
        text = MESSAGES['os-environ']
        for wrong in (text.replace('//:ext.bzl', '//:rules.bzl'),
                      text.replace(' --> //:ext.bzl:4:11', ' --> //:ext.bzl:3:11'),
                      text.replace(' --> //:ext.bzl:4:11', ' --> //:ext.bzl:4:5'),
                      text.replace('4 |     value=ctx.os.environ', '4 |     value=ctx.os.name'),
                      text.replace('4 |     value=ctx.os.environ\n', ''),
                      text.replace('module extension //:ext.bzl', 'repository made (//:rules.bzl%maker)'),
                      text+'HOST_REPO_CALLBACK\n', text+'HOST_AFTER_GETTER\n',
                      text+'1 actions executed, 0 cache hits\n',
                      text+'0 actions executed, 1 cache hits\n',
                      text+extensions.FAIL_MARKER+'\n'):
            with self.subTest(wrong=wrong), self.assertRaises(ValueError):
                extensions.require_rejection(record(stderr=wrong, returncode=1), 'os-environ')
        for output in ('@@+files+made//:data\n', ' ', '\n'):
            with self.subTest(output=output), self.assertRaises(ValueError):
                extensions.require_rejection(record(stdout=output, stderr=text, returncode=1), 'os-environ')

    def test_frozen_list_diagnostic_and_no_generic_evaluation_limit(self):
        extensions.require_rejection(record(stderr='trying to mutate a frozen list value', returncode=1), 'immutable')
        for text in ('evaluation limit exceeded', 'memory exhausted', 'ticks: 1000000', 'timeout'):
            with self.assertRaises(ValueError):
                extensions.require_rejection(record(stderr=text, returncode=1), 'ticks')

    def test_canonical_identity_is_not_rewritten_from_apparent_alias(self):
        extensions.canonical_result(record('@@+files+made//:data\n'), '+files+made')
        for text in ('@chosen//:data\n', '@@+_private+made//:data\n', '',
                     '@@+files+made//:data\n@@+files+made//:data\n'):
            with self.assertRaises(ValueError):
                extensions.canonical_result(record(text), '+files+made')


class EvidenceTests(unittest.TestCase):
    def test_os_environ_failure_cannot_leave_a_published_repository(self):
        # Mocked record only; no native/reference invocation or ambient lookup.
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            scratch = Path(directory)
            workspace = scratch / 'workspace'
            root = scratch / 'case'
            value = extensions.materialize(root, workspace, 'native.context-and-return', 'os-environ')
            cache = root / 'native-cache'
            (extensions.direct.trees_directory(cache) / ('a'*64)).mkdir(parents=True)
            runner = mock.Mock()
            runner.run.return_value = record(stderr=MESSAGES['os-environ'], returncode=1)
            item = next(c for c in extensions.CASES if c['name'] == 'native.context-and-return')
            phase = dict(probe='os-environ', engine='native', status='FAIL', runs=[], cache=str(cache),
                         registry=str(value['registry']), inputs_before=extensions.snapshot(value['paths']))
            extensions.run_phase(extensions.argument_parser().parse_args([]), runner, scratch, workspace,
                                 None, item, phase, value)
            self.assertEqual(phase['status'], 'FAIL')
            self.assertIn('rejection published', phase['error'])
            self.assertNotIn('artifacts', phase)
            self.assertNotIn('copy', phase)

    def test_real_materialization_has_private_registry_and_no_fabricated_builtins(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            root = Path(directory)
            value = extensions.materialize(root / 'case', root / 'workspace', 'participants.bfs-alias-dev-version', 'valid')
            inputs = extensions.snapshot(value['paths'])
            self.assertTrue(all(extensions.valid_evidence(v) for v in inputs.values()))
            self.assertEqual((value['registry'] / 'bazel_registry.json').read_bytes(), b'{}\n')
            self.assertEqual(list(value['registry'].iterdir()), [value['registry'] / 'bazel_registry.json'])
            self.assertNotIn(root / 'workspace/data', value['paths'])
            self.assertFalse((root / 'workspace/bazel_tools').exists())

    def test_refresh_five_explicit_phases_mutate_only_the_planned_input(self):
        probes = ['cold', 'tag-change', 'callback-change', 'input-change', 'repair']
        expected_changed = ['MODULE.bazel', 'ext.bzl', 'input', None]
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            root = Path(directory)
            previous = None
            for index, probe in enumerate(probes):
                value = extensions.materialize(root / 'case', root / 'workspace', 'native.refresh-and-repair', probe, update=index > 0)
                current = extensions.snapshot(value['paths'])
                if previous is not None:
                    changed = [p for p in previous if current[p] != previous[p]]
                    relative = expected_changed[index - 1]
                    self.assertEqual(changed, [] if relative is None else [str(root / 'workspace' / relative)])
                self.assertIn(b'\xff\x00\r\n', value['expected']['data'][0])
                previous = current

    def test_parallel_plan_requires_both_independent_real_cli_builds(self):
        item = next(c for c in extensions.CASES if c['name'] == 'native.parallel-publication')
        self.assertEqual(extensions.phases_for(item), [('parallel-1', 'native'), ('parallel-2', 'native')])
        for probe in item['probes']:
            self.assertEqual(extensions.steps_for(item, probe, 'native'), [
                ('build', '//:data', []), ('query', '@chosen//:data', []), ('build', '//:copy', [])])

    def test_recording_runner_preserves_non_utf8_streams_and_executable_hash(self):
        report = {'runs': [], 'executables': {'native': {'path': '/not-executed', 'sha256': 'a' * 64}}}
        runner = extensions.RecordingRunner(report, 1)

        def seam(*args, **kwargs):
            raw = b'\xff\x00\r\n'
            entry = record(stdout='unused', stdout_base64=base64.b64encode(raw).decode(), argv=['/not-executed'])
            report['runs'].append(entry)
            return entry

        with mock.patch.object(extensions.direct.Runner, 'run', side_effect=seam):
            entry = runner.run('unit-seam', ['/not-executed'], Path('/not-used'))
        self.assertEqual(entry['stdout_base64'], base64.b64encode(b'\xff\x00\r\n').decode())
        self.assertEqual(entry['stdout_full_sha256'], extensions.digest(b'\xff\x00\r\n'))
        self.assertEqual(entry['executable_sha256'], 'a' * 64)
        self.assertNotIn('TOKEN', entry['environment'])

    def test_negative_check_replay_rejects_missing_or_duplicate_phases_and_crash(self):
        # This exercises ONLY report validation using deliberately synthetic
        # records. No synthetic report is written as an execution artifact.
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            scratch = Path(directory)
            report = {'runs': [], 'checks': [], 'scratch_directory': str(scratch)}
            args = extensions.argument_parser().parse_args([])
            runner = mock.Mock(report=report)
            reference = extensions.DirectReference(args.reference, scratch / 'workspace', scratch, runner, 'module-extensions')

            def synthetic(name, argv, cwd, timeout=None):
                entry = record(stderr=MESSAGES['rollback'], returncode=1)
                entry.update(id=len(report['runs']), name=name, argv=list(argv), cwd=str(cwd), timeout_seconds=args.timeout)
                report['runs'].append(entry)
                return entry

            runner.run.side_effect = synthetic
            item = next(c for c in extensions.CASES if c['name'] == 'callback.whole-group-rollback')
            check = extensions.run_case(args, runner, scratch, scratch / 'workspace', reference, item)
            self.assertEqual(check['status'], 'PASS')  # classifier seam, NOT Bazel parity
            extensions.validate_check(check, report, args, scratch / 'workspace', reference)
            for mutation in ('missing', 'duplicate', 'reorder', 'kind', 'snapshot', 'commands', 'publication'):
                altered = copy.deepcopy(check)
                if mutation == 'missing':
                    altered['phases'].pop()
                elif mutation == 'duplicate':
                    altered['phases'].append(altered['phases'][0])
                elif mutation == 'reorder':
                    altered['phases'].reverse()
                elif mutation == 'kind':
                    altered['kind'] = 'native_boundary'
                elif mutation == 'snapshot':
                    altered['phases'][0]['inputs_after'] = {}
                elif mutation == 'commands':
                    altered['phases'][0]['runs'] = []
                else:
                    altered['phases'][1]['publication_entries'] = ['partial']
                with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                    extensions.validate_check(altered, report, args, scratch / 'workspace', reference)
            for change in ({'returncode': 101}, {'timed_out': True}, {'stderr': 'download failed\n' + MESSAGES['rollback']},
                           {'argv': ['/wrong-binary']}, {'timeout_seconds': 0}):
                altered = copy.deepcopy(report)
                altered['runs'][0].update(change)
                with self.subTest(change=change), self.assertRaises(ValueError):
                    extensions.validate_check(check, altered, args, scratch / 'workspace', reference)


class LifecycleTests(unittest.TestCase):
    def test_cli_api_uses_actual_reference_and_native_helpers(self):
        args = extensions.argument_parser().parse_args([])
        self.assertEqual(args.output.name, 'module-extensions-validation.json')
        self.assertEqual(args.scratch_parent, Path('/tmp/opencode'))
        self.assertEqual(extensions.REFERENCE_SHA256, 'd302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8')
        runner = mock.Mock()
        reference = extensions.DirectReference(Path('/official'), Path('/workspace'), Path('/scratch'), runner, 'module-extensions')
        reference.registry = Path('/private/registry space')
        reference.run('listing', 'cquery', '--output=files', '//:data')
        argv = runner.run.call_args.args[1]
        self.assertEqual(argv, extensions.command_argv(args, reference, 'reference', Path('/cache'),
            reference.registry, 'cquery', '//:data', ['--output=files']))
        for flag in ('--ignore_all_rc_files', '--repository_cache=/scratch/repository-cache',
                     '--repo_contents_cache=/scratch/repository-contents', '--remote_cache=', '--remote_executor=', '--bes_backend='):
            self.assertIn(flag, argv)
        self.assertEqual([a for a in argv if a.startswith('--registry=')],
            ['--registry=file:///private/registry%20space', '--registry=https://bcr.bazel.build'])
        self.assertFalse(any('override' in a for a in argv))

    def test_counts_and_pass_strings_never_substitute_for_genuine_evidence(self):
        synthetic = dict(checks=[dict(name=c['name'], kind=c['kind'], status='PASS') for c in extensions.CASES],
            planned_checks=copy.deepcopy(extensions.CASES), completed_checks=16, completed_differential_cases=8,
            completed_phases=51, errors=[], cleanup=[], scratch_cleaned=True, sources_unchanged=True,
            reference_shutdown_attempted=True, reference_shutdown_succeeded=True)
        self.assertFalse(extensions.report_passes(synthetic))
        for mutation in ('missing', 'duplicate', 'reorder', 'unexpected', 'kind', 'reference-unavailable'):
            altered = copy.deepcopy(synthetic)
            if mutation == 'missing':
                altered['checks'].pop()
            elif mutation == 'duplicate':
                altered['checks'][-1] = altered['checks'][0]
            elif mutation == 'reorder':
                altered['checks'].reverse()
            elif mutation == 'unexpected':
                altered['checks'].append({'name': 'extra', 'kind': 'differential', 'status': 'PASS'})
            elif mutation == 'kind':
                altered['checks'][0]['kind'] = 'native_boundary'
            else:
                altered['reference_release'] = 'unavailable'
            self.assertFalse(extensions.report_passes(altered))
        for value in (None, [], {}, {'checks': None}, {'checks': [{'status': 'PASS'}]}):
            self.assertFalse(extensions.report_passes(value))

    def test_reference_network_failure_never_becomes_differential_or_boundary_pass(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            scratch = Path(directory)
            report = {'checks': [], 'runs': []}
            runner = mock.Mock(report=report)
            runner.run.return_value = record(stderr=MESSAGES['rollback'], returncode=1)
            reference = mock.Mock()
            reference.run.return_value = record(stderr='download failed\n' + MESSAGES['rollback'], returncode=1)
            item = next(c for c in extensions.CASES if c['name'] == 'callback.whole-group-rollback')
            check = extensions.run_case(extensions.argument_parser().parse_args([]), runner, scratch, scratch / 'workspace', reference, item)
            self.assertEqual(check['status'], 'REFERENCE_FAILURE')
            self.assertFalse(check['parity_claim'])
            self.assertEqual(check['phases'][0]['status'], 'FAIL')
            self.assertEqual(check['kind'], 'differential')

    def test_run_all_records_failure_and_continues_every_planned_case(self):
        report = {'checks': [], 'runs': []}
        runner = mock.Mock(report=report)
        seen = []

        def broken(args, runner, scratch, workspace, reference, item):
            seen.append(item['name'])
            raise RuntimeError('synthetic setup failure')

        with mock.patch.object(extensions, 'run_case', side_effect=broken):
            extensions.run_all(None, runner, None, None, None)
        self.assertEqual(seen, [c['name'] for c in extensions.CASES])
        self.assertEqual(len(report['checks']), 16)
        self.assertTrue(all(c['status'] == 'FAIL' and not c['parity_claim'] for c in report['checks']))

    def test_shutdown_failure_still_removes_owned_scratch_and_records_receipt(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            scratch = Path(directory) / 'owned'
            (scratch / 'workspace').mkdir(parents=True)
            report = dict(runs=[record(stderr='shutdown rejected', returncode=1)], cleanup=[], errors=[],
                          reference_shutdown_succeeded=False, scratch_cleaned=False)
            report['runs'][0]['name'] = 'module-extensions.shutdown'
            reference = mock.Mock()
            extensions.finish(reference, scratch, report)
            reference.shutdown.assert_called_once()
            self.assertFalse(report['reference_shutdown_succeeded'])
            self.assertTrue(report['scratch_cleaned'])
            self.assertEqual(report['cleanup_receipt'], {'path': str(scratch), 'removed': True})
            self.assertFalse(scratch.exists())
            self.assertTrue(report['errors'])

    def test_interrupted_main_preserves_report_and_shuts_down_in_finally(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as directory:
            output = Path(directory) / 'synthetic-interruption-receipt.json'

            def seam(runner, name, argv, cwd, timeout=None):
                self.assertIn(name, ('reference.version', 'module-extensions.shutdown'))
                entry = record('bazel 9.3.0\n' if name == 'reference.version' else '')
                entry.update(id=len(runner.report['runs']), name=name, argv=list(argv), cwd=str(cwd),
                             timeout_seconds=runner.timeout if timeout is None else timeout)
                runner.report['runs'].append(entry)
                return entry

            with mock.patch.object(extensions, 'validate_binaries', return_value={}), \
                    mock.patch.object(extensions.direct.Runner, 'run', new=seam), \
                    mock.patch.object(extensions, 'run_all', side_effect=KeyboardInterrupt), mock.patch('builtins.print'):
                code = extensions.main(['--output', str(output), '--scratch-parent', directory])
            report = json.loads(output.read_text())
            self.assertEqual(code, 130)
            self.assertEqual(report['status'], 'FAIL')
            self.assertEqual(report['completed_checks'], 0)
            self.assertEqual(report['completed_phases'], 0)
            self.assertIn('interrupted', report['errors'])
            self.assertTrue(report['reference_shutdown_succeeded'])
            self.assertTrue(report['scratch_cleaned'])
            self.assertFalse(Path(report['scratch_directory']).exists())
            self.assertFalse(extensions.report_passes(report))


if __name__ == '__main__':
    unittest.main()
