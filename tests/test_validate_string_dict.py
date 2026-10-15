"""Portable harness units; synthetic rows are never CLI/Starlark parity evidence."""

import ast
import base64
import copy
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'scripts'))
import validate_string_dict as suite


def record(text='',code=1,**changes):
    row = dict(stdout='',stderr=text,returncode=code,timed_out=False)
    row.update(changes)
    return row


MESSAGES = {
    'key':"expected value of type 'string' for dict key element, but got 1 (int)",
    'value':"expected value of type 'string' for dict value element, but got [] (list)",
    'dict':"expected value of type 'dict(string, string)' for attribute 'd', but got [] (list)",
    'mandatory':"missing value for mandatory attribute 'd'",
    'empty':'attribute d: attribute must be non empty',
    'frozen':'trying to mutate a frozen dict value',
    'default-none':"parameter 'default' got value of type 'NoneType', want 'dict'",
    'default-callable':"parameter 'default' got value of type 'function', want 'dict'",
    'doc-int':"parameter 'doc' got value of type 'int', want 'string or NoneType'",
    'allow-int':"parameter 'allow_empty' got value of type 'int', want 'bool'",
    'mandatory-int':"parameter 'mandatory' got value of type 'int', want 'bool'",
    'two-positional':'string_dict() accepts no more than 1 positional argument but got 2',
    'values':"string_dict() got unexpected keyword argument 'values'",
    'duplicate':"multiple values for parameter 'allow_empty'",
    'configurable':'attr.string_dict configurable is unsupported',
    'ascii':'only ASCII strings are supported',
}

# Immutable observed native wording; no ignored receipt is a test dependency.
NATIVE_DOC_TRACE = ('bazel-rs: cannot evaluate //:defs.bzl: Traceback (most recent call last):\n'
    '  * //:defs.bzl:4, in <module>\n'
    '      A=attr.string_dict(doc=1)\n'
    'error: attr.string_dict doc must be a string or None\n'
    ' --> //:defs.bzl:4:3\n'
    '  |\n'
    '4 | A=attr.string_dict(doc=1)\n'
    '  |   ^^^^^^^^^^^^^^^^^^^^^^^\n'
    '  |\n\n')

# Transcribed from the native preparation rows, including caret widths and the
# final blank line. The offline preparation replay checks these against the raw
# receipts; portable units never read ignored artifacts. Only the workspace path
# is substituted for BUILD diagnostics.
NATIVE_BINDER_OBSERVATIONS = {
    'allow-int': ('A=attr.string_dict(allow_empty=1)',
        "Type of parameter `allow_empty` doesn't match, expected `bool`, actual `int (repr: 1)`",
        '^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^'),
    'mandatory-int': ('A=attr.string_dict(mandatory=0)',
        "Type of parameter `mandatory` doesn't match, expected `bool`, actual `int (repr: 0)`",
        '^^^^^^^^^^^^^^^^^^^^^^^^^^^^^'),
    'values': ('A=attr.string_dict(values=["x"])',
        'Found `values` extra named parameter(s) for call to string_dict',
        '^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^'),
    'duplicate': ('A=attr.string_dict(True,allow_empty=True)',
        'Argument `allow_empty` occurs more than once',
        '^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^'),
}
NATIVE_SHAPE_OBSERVATIONS = {
    'key-int': ('r(name="one",d={1:"x"})','^^^^^^^^^^^^^^^^^^^^^^^'),
    'key-bool': ('r(name="one",d={True:"x"})','^^^^^^^^^^^^^^^^^^^^^^^^^^'),
    'value-list': ('r(name="one",d={"z":["x"]})','^^^^^^^^^^^^^^^^^^^^^^^^^^^'),
    'value-tuple': ('r(name="one",d={"z":("x",)})','^^^^^^^^^^^^^^^^^^^^^^^^^^^^'),
    'value-int': ('r(name="one",d={"z":1})','^^^^^^^^^^^^^^^^^^^^^^^'),
    'value-bool': ('r(name="one",d={"z":False})','^^^^^^^^^^^^^^^^^^^^^^^^^^^'),
    'value-none': ('r(name="one",d={"z":None})','^^^^^^^^^^^^^^^^^^^^^^^^^^'),
    'value-dict': ('r(name="one",d={"z":{}})','^^^^^^^^^^^^^^^^^^^^^^^^'),
}


def native_observed_row(probe):
    if probe in NATIVE_BINDER_OBSERVATIONS:
        source,error,carets = NATIVE_BINDER_OBSERVATIONS[probe]
        text = ('bazel-rs: cannot evaluate //:defs.bzl: Traceback (most recent call last):\n'
            '  * //:defs.bzl:4, in <module>\n      @SOURCE@\nerror: @ERROR@\n'
            ' --> //:defs.bzl:4:3\n  |\n4 | @SOURCE@\n  |   @CARETS@\n  |\n\n')
        text = text.replace('@SOURCE@',source).replace('@ERROR@',error).replace('@CARETS@',carets)
        return record(text,name='constructor.'+probe+'.native.0',cwd='/captured/workspace')
    source,carets = NATIVE_SHAPE_OBSERVATIONS[probe]
    text = ('bazel-rs: Traceback (most recent call last):\n'
        '  * /captured/workspace/BUILD.bazel:3, in <module>\n      @SOURCE@\n'
        'error: attribute "d": expected a dict of strings to strings\n'
        ' --> /captured/workspace/BUILD.bazel:3:1\n  |\n3 | @SOURCE@\n  | @CARETS@\n  |\n\n'
        'Supported rules: filegroup, genrule, and local custom rules. select and non-local repositories remain unsupported.\n')
    text = text.replace('@SOURCE@',source).replace('@CARETS@',carets)
    return record(text,name='validation.custom.'+probe+'.native.0',cwd='/captured/workspace')


class PlanTests(unittest.TestCase):
    def test_complete_unique_plan(self):
        self.assertEqual(len(suite.CASES),8)
        self.assertEqual(len(set(suite.PLAN)),8)
        for item in suite.CASES:
            self.assertEqual(len(item['probes']),len(set(item['probes'])))
            for probe,engine in suite.phases_for(item):
                value = suite.fixture(item['name'],probe)
                steps = suite.steps_for(item,probe,engine)
                self.assertEqual(steps[0],('query',value['query']))
                if not value['diagnostic']:
                    self.assertEqual(steps.count(('build','//:result')),2)

    def test_unknown_probe_has_no_fallback(self):
        for name,probe in [('invented','ordered'),('ordinary','invented')]:
            with self.assertRaisesRegex(ValueError,'unknown planned'):
                suite.fixture(name,probe)

    def test_fixture_syntax_only_not_interpreter_emulation(self):
        for item in suite.CASES:
            for probe in item['probes']:
                for path,raw in suite.fixture(item['name'],probe)['files'].items():
                    with self.subTest(name=item['name'],probe=probe,path=path):
                        ast.parse(raw.decode(),filename=path)

    def test_scalar_order_and_literal_labels_not_json_sorted(self):
        for name,probe in [('ordinary','ordered'),('repositories','direct.module-late'),('repositories','ordinary.per-call')]:
            value = suite.fixture(name,probe)
            self.assertIn(b'd.keys()',value['files']['defs.bzl'])
            self.assertIn(b'd.values()',value['files']['defs.bzl'])
            self.assertNotIn(b'json.encode',value['files']['defs.bzl'])
            self.assertNotIn(b'string_list_dict',value['files']['defs.bzl'])
            self.assertIn(b'@missing//pkg:target',value['expected'])
            self.assertNotIn(b'Label(',value['files']['defs.bzl'])
            self.assertIn(b'cat $(SRCS) > $@',value['files']['BUILD.bazel'])

    def test_constructor_and_call_snapshot_clocks(self):
        for route in ('custom','direct'):
            value = suite.fixture('defaults',route+'.snapshot')
            defs = value['files']['defs.bzl']
            self.assertLess(defs.index(b'A=attr.string_dict'),defs.index(suite.MUTATE.encode()))
            self.assertEqual(value['expected'],suite.BEFORE)
        self.assertEqual(suite.fixture('defaults','custom.call-snapshot')['expected'],suite.BEFORE)
        for route in ('direct','tags'):
            value = suite.fixture('repositories',route+'.module-late')
            self.assertIn(suite.MUTATE.encode(),value['files']['MODULE.bazel'])
            self.assertEqual(value['expected'],suite.AFTER)
        value = suite.fixture('repositories','ordinary.per-call')
        self.assertEqual(value['expected'],suite.BEFORE+suite.AFTER)
        ext = value['files']['ext.bzl']
        self.assertLess(ext.index(b'r(name="one"'),ext.index(b'D["z"]="after"'))
        self.assertLess(ext.index(b'D["z"]="after"'),ext.index(b'r(name="two"'))

    def test_bzl_alias_does_not_invent_build_attr_namespace(self):
        value = suite.fixture('constructor','alias')
        self.assertIn(b'ctor=attr.string_dict',value['files']['defs.bzl'])
        self.assertNotIn(b'attr.string_dict',value['files']['BUILD.bazel'])

    def test_none_is_fallback_not_empty_constructor(self):
        self.assertEqual(suite.fixture('constructor','omitted')['expected'],suite.EMPTY)
        self.assertEqual(suite.fixture('constructor','default-none')['diagnostic'],'default-none')
        for route in ('custom','direct','tags'):
            self.assertEqual(suite.fixture('defaults',route+'.none')['expected'],suite.BEFORE)

    def test_nonempty_is_analysis_only_and_repository_tag_ignored(self):
        item = next(c for c in suite.CASES if c['name']=='validation')
        for probe in ('custom.empty-default','custom.empty-explicit'):
            self.assertEqual(suite.steps_for(item,probe,'native'),[('query','//:one'),('build','//:result')])
        for probe in ('custom.empty-string','direct.empty','tags.empty'):
            self.assertIsNone(suite.fixture('validation',probe)['diagnostic'])

    def test_source_change_and_restore_are_real_consumer_inputs(self):
        for route in ('custom','direct'):
            first = suite.fixture('lifecycle',route+'.first')
            self.assertEqual(first,suite.fixture('lifecycle',route+'.repeat'))
            self.assertEqual(first,suite.fixture('lifecycle',route+'.restore'))
            changed = suite.fixture('lifecycle',route+'.change')
            self.assertNotEqual(first['expected'],changed['expected'])
            self.assertNotEqual(first['files'],changed['files'])

    def test_template_order_is_observable_in_actual_output(self):
        for route in ('direct','ordinary'):
            value = suite.fixture('templates',route+'.ordered')
            self.assertIn(b'ctx.template("data","input",substitutions=d',value['files']['defs.bzl'])
            self.assertIn(b'{"A":"B","B":"DONE"}',value['files']['defs.bzl'])
            self.assertEqual(value['expected'],b'DONE')

    def test_native_boundaries_are_not_false_differential_claims(self):
        boundary = suite.CASES[-1]
        self.assertEqual(suite.phases_for(boundary,True),[])
        self.assertTrue(all(engine=='native' for _,engine in suite.phases_for(boundary)))
        self.assertIn('loaded-build-helper',boundary['probes'])
        args = suite.argument_parser().parse_args([])
        with self.assertRaisesRegex(ValueError,'frozen candidate'):
            suite.executable_evidence(args)

    def test_loaded_build_ctor_is_direct_and_observation_is_only_in_bzl(self):
        for probe,call in [('loaded-build-ctor','ctor()'),('loaded-build-helper','helper()')]:
            value = suite.fixture('native-profile',probe)
            build,defs = value['files']['BUILD.bazel'],value['files']['defs.bzl']
            self.assertIn(('observe('+call+')').encode(),build)
            self.assertNotIn(b'print(',build)
            self.assertNotIn(b'attr.',build)
            self.assertIn(b'def observe(a):',defs)
            self.assertIn(b'type(a)!="Attribute" or repr(a)!="<attr.string_dict>"',defs)
            self.assertIn(b'print("NATIVE_METADATA",type(a),repr(a))',defs)
        self.assertIn(b'print("ALIAS_TYPE",type(ctor()))',suite.defect_fixture()['BUILD.bazel'])


class DiagnosticTests(unittest.TestCase):
    def test_categories_are_explicit(self):
        self.assertEqual(set(MESSAGES),set(suite.DIAGNOSTICS))
        for key,text in MESSAGES.items():
            suite.require_rejection(record(text),key)
        for key,text in [('key',MESSAGES['value']),('value',MESSAGES['key'])]:
            with self.assertRaises(ValueError):
                suite.require_rejection(record(text),key)

    def test_unrelated_failures_never_satisfy_negative(self):
        for key,text in MESSAGES.items():
            for bad in ('panicked','internal error','network unavailable','connection refused','failed to download',
                        'syntax error','permission denied','has no attribute string_dict','Variable attr not found'):
                with self.subTest(key=key,bad=bad),self.assertRaises(ValueError):
                    suite.require_rejection(record(text+'\n'+bad),key)
            for code in (0,True,-9,37,101,124,125,126,127,137):
                with self.assertRaises(ValueError):
                    suite.require_rejection(record(text,code),key)
            for flag in ('timed_out','spawn_error','interrupted','output_truncated'):
                with self.assertRaises(ValueError):
                    suite.require_rejection(record(text,**{flag:True}),key)

    def test_constructor_default_requires_bound_source(self):
        generic = 'error: expected a dict of strings to strings\n'
        for key in ('default-none','default-callable'):
            with self.assertRaises(ValueError):
                suite.require_rejection(record(generic),key,'native')
            source = 'attr.string_dict('+suite.BAD_CTORS[key]+')\n'
            suite.require_rejection(record(source+generic),key,'native')
            with self.assertRaises(ValueError):
                suite.require_rejection(record(source+generic),key,'reference')

    def test_native_doc_int_accepts_only_bound_observed_trace(self):
        self.assertEqual(len(NATIVE_DOC_TRACE.encode()),280)
        row = record(NATIVE_DOC_TRACE,name='constructor.doc-int.native.0')
        suite.require_rejection(row,'doc-int','native')
        with self.assertRaises(ValueError):
            suite.require_rejection(row,'doc-int','reference')
        suite.require_rejection(record(MESSAGES['doc-int']),'doc-int','reference')
        with self.assertRaises(ValueError):
            suite.require_rejection(record(MESSAGES['doc-int'],name=row['name']),'doc-int','native')

    def test_native_doc_int_rejects_279_byte_missing_final_newline(self):
        shortened = NATIVE_DOC_TRACE[:-1]
        self.assertEqual(len(shortened.encode()),279)
        with self.assertRaises(ValueError):
            suite.require_rejection(record(shortened,name='constructor.doc-int.native.0'),'doc-int','native')

    def test_native_doc_int_rejects_wrong_source_phase_wording_and_extra_output(self):
        row = record(NATIVE_DOC_TRACE,name='constructor.doc-int.native.0')
        for changes in (
            {'name':'constructor.allow-int.native.0'},
            {'name':'constructor.doc-int.reference.0'},
            {'name':'constructor.doc-int.native.1'},
            {'stderr':NATIVE_DOC_TRACE.replace('A=attr.string_dict(doc=1)','A=attr.string(doc=1)')},
            {'stderr':NATIVE_DOC_TRACE.replace('A=attr.string_dict(doc=1)','A=attr.string_dict(doc=None)')},
            {'stderr':NATIVE_DOC_TRACE.replace('//:defs.bzl','//:other.bzl')},
            {'stderr':NATIVE_DOC_TRACE.replace(':4',':5')},
            {'stderr':NATIVE_DOC_TRACE.replace('must be a string or None','must be string or None')},
            {'stderr':NATIVE_DOC_TRACE.replace('or None\n','or None.\n')},
            {'stderr':NATIVE_DOC_TRACE+'callback executed\n'},
            {'stderr':'error: attr.string_dict doc must be a string or None\n'},
            {'stdout':'//:one\n'}, {'stdout':'\n'}, {'returncode':0}, {'returncode':True},
            {'returncode':37}, {'returncode':101}, {'returncode':-9},
            {'stderr':NATIVE_DOC_TRACE+'panicked\n'},
            {'stderr':NATIVE_DOC_TRACE+'has no attribute string_dict\n'},
            *({flag:True} for flag in ('timed_out','spawn_error','interrupted','output_truncated')),
        ):
            with self.subTest(changes=changes),self.assertRaises(ValueError):
                suite.require_rejection(dict(row,**changes),'doc-int','native')

    def test_observed_native_binder_and_scalar_categories(self):
        for probe in (*NATIVE_BINDER_OBSERVATIONS,*NATIVE_SHAPE_OBSERVATIONS):
            diagnostic = probe if probe in NATIVE_BINDER_OBSERVATIONS else probe.split('-')[0]
            row = native_observed_row(probe)
            with self.subTest(probe=probe):
                suite.require_rejection(row,diagnostic,'native')
                with self.assertRaises(ValueError):
                    suite.require_rejection(row,diagnostic,'reference')
                wrong = 'mandatory-int' if diagnostic=='allow-int' else 'allow-int' if diagnostic in NATIVE_BINDER_OBSERVATIONS else 'value' if diagnostic=='key' else 'key'
                with self.assertRaises(ValueError):
                    suite.require_rejection(row,wrong,'native')

    def test_scalar_diagnostic_accepts_current_and_legacy_footer_only(self):
        legacy = ('Supported rules: filegroup, genrule, and local custom rules. '
                  'select and non-local repositories remain unsupported.\n')
        current = ('Supported rules: the native rule classes in docs/NATIVE_BUILD_RULES.md and '
                   'Starlark rule() definitions. select() resolves against modelled config_setting '
                   'values (docs/CONFIGURATION.md).\n')
        previous = ('Supported rules: the native rule classes in docs/NATIVE_BUILD_RULES.md and '
                    'Starlark rule() definitions. select() values load, but native analysis does not resolve them.\n')
        self.assertEqual(suite.NATIVE_LOADING_FOOTERS, (current, previous, legacy))
        for probe in NATIVE_SHAPE_OBSERVATIONS:
            row = native_observed_row(probe)
            diagnostic = probe.split('-')[0]
            with self.subTest(probe=probe):
                for accepted in (current, previous):
                    suite.require_rejection(dict(row, stderr=row['stderr'].replace(legacy, accepted)), diagnostic, 'native')
                for footer in ('', current.replace('.md).', '.md)!'), previous.replace('them.', 'them!'),
                               legacy + current, previous + current, current.upper()):
                    with self.assertRaises(ValueError):
                        suite.require_rejection(dict(row, stderr=row['stderr'].replace(legacy, footer)), diagnostic, 'native')

    def test_observed_native_categories_reject_context_and_body_mutations(self):
        for probe in (*NATIVE_BINDER_OBSERVATIONS,*NATIVE_SHAPE_OBSERVATIONS):
            diagnostic = probe if probe in NATIVE_BINDER_OBSERVATIONS else probe.split('-')[0]
            row = native_observed_row(probe)
            text = row['stderr']
            error = next(line for line in text.splitlines() if line.startswith('error:'))
            source = (NATIVE_BINDER_OBSERVATIONS if probe in NATIVE_BINDER_OBSERVATIONS else NATIVE_SHAPE_OBSERVATIONS)[probe][0]
            body_mutations = [text.replace(error,error+' nearby wording'),text.replace(source,source.replace('=', '=None #',1))]
            for old,new in (('`allow_empty`','`mandatory`'),('`mandatory`','`allow_empty`'),
                            ('`bool`','`int`'),('`int (repr: 1)`','`string (repr: "1")`'),
                            ('`int (repr: 0)`','`bool (repr: False)`'),('`values`','`default`'),
                            ('attribute "d"','attribute "other"'),('strings to strings','strings to lists')):
                if old in error:
                    body_mutations.append(text.replace(error,error.replace(old,new)))
            changes = [{'stderr':t} for t in body_mutations]
            changes += [{'name':row['name'].replace('.native.0','.native.1')},
                {'name':row['name'].replace('.native.0','.reference.0')},
                {'stderr':text.replace('defs.bzl','other.bzl').replace('BUILD.bazel','other.bazel')},
                {'stderr':text.replace(':4',':5').replace(':3',':8')},
                {'stderr':error+'\n'}, {'stderr':text+'CALLBACK_EXECUTED\n'},
                {'stderr':text+'has no attribute string_dict\n'}, {'stderr':text+'panicked\n'},
                {'stdout':'//:one\n'}, {'stdout':'\n'}, {'returncode':0}, {'returncode':True},
                {'returncode':37}, {'returncode':101}, {'returncode':-9},
                *({flag:True} for flag in ('timed_out','spawn_error','interrupted','output_truncated'))]
            for change in changes:
                with self.subTest(probe=probe,change=change),self.assertRaises(ValueError):
                    suite.require_rejection(dict(row,**change),diagnostic,'native')

    def test_known_crash_is_separate_and_exact_site_bound(self):
        workspace = Path('/owned/workspace')
        text = ('FATAL: bazel crashed due to an internal error. Printing stack trace:\n'
            '\tat <starlark>.string_dict(<builtin>:0)\n'
            '\tat <starlark>.<toplevel>(/owned/workspace/BUILD.bazel:2)\n'+suite.DEFECT_FRAMES)
        suite.require_defect(record(text,37),workspace)
        for bad in (text.replace('BUILD.bazel:2','BUILD.bazel:3'),text.replace('377','378'),text.replace('NullPointerException','OtherException')):
            with self.assertRaises(ValueError):
                suite.require_defect(record(bad,37),workspace)
        with self.assertRaises(ValueError):
            suite.require_defect(record(text,1),workspace)
        with self.assertRaises(ValueError):
            suite.require_rejection(record(text,37),'value','native')

    def test_metadata_success_is_not_phase_guard_error(self):
        suite.require_metadata(record('NATIVE_METADATA Attribute <attr.string_dict>',0))
        with self.assertRaises(ValueError):
            suite.require_metadata(record('only permitted in bzl initialization'))
        for bad in ('NATIVE_METADATA Attribute <attr.string_list_dict>','NATIVE_METADATA dict <attr.string_dict>',
                    'NATIVE_METADATA attribute attr.StringDict','NATIVE_METADATA Attribute <attr.string_dict> extra'):
            with self.assertRaises(ValueError):
                suite.require_metadata(record(bad,0))


class ReplayTests(unittest.TestCase):
    def prepare(self,root):
        item = next(c for c in suite.CASES if c['name']=='validation')
        probe,engine = 'custom.value-list','reference'
        value = suite.fixture(item['name'],probe)
        workspace = root/'workspace'; registry,paths = suite.common.materialize(root/'case',workspace,value)
        suite.shutil.copytree(suite.REGISTRY,registry,dirs_exist_ok=True)
        paths = [workspace/p for p in value['files']]+sorted(p for p in registry.rglob('*') if p.is_file())
        args = suite.argument_parser().parse_args(['--reference-only'])
        ref = suite.DirectReference(args.reference,workspace,root,None,'string-dict')
        cache = root/'case/native-cache'
        row = record(MESSAGES['value'],id=0,name='validation.custom.value-list.reference.0',cwd=str(workspace),
            timeout_seconds=args.timeout,argv=suite.command_argv(args,ref,engine,cache,registry,'query','//:one'))
        phase = dict(probe=probe,engine=engine,status='PASS',runs=[0],outputs=[],sources=[],cache=str(cache),registry=str(registry),
            inputs_before=suite.snapshot(paths),inputs_after=suite.snapshot(paths))
        return phase,dict(runs=[row],scratch_directory=str(root)),args,workspace,ref,item

    def test_native_observed_categories_keep_input_command_and_publication_barriers(self):
        for probe in (*NATIVE_BINDER_OBSERVATIONS,*NATIVE_SHAPE_OBSERVATIONS):
            with self.subTest(probe=probe),tempfile.TemporaryDirectory(dir=suite.ROOT/'artifacts') as tmp:
                root = Path(tmp)
                family = 'constructor' if probe in NATIVE_BINDER_OBSERVATIONS else 'validation'
                planned_probe = probe if family=='constructor' else 'custom.'+probe
                item = next(c for c in suite.CASES if c['name']==family)
                value = suite.fixture(family,planned_probe)
                workspace = root/'workspace'
                registry,_ = suite.common.materialize(root/'case',workspace,value)
                suite.shutil.copytree(suite.REGISTRY,registry,dirs_exist_ok=True)
                paths = [workspace/p for p in value['files']]+sorted(p for p in registry.rglob('*') if p.is_file())
                args = suite.argument_parser().parse_args([])
                cache = root/'case/native-cache'
                row = native_observed_row(probe)
                row.update(id=0,cwd=str(workspace),stderr=row['stderr'].replace('/captured/workspace',str(workspace)),
                    timeout_seconds=args.timeout,argv=suite.command_argv(args,None,'native',cache,registry,'query','//:one'))
                phase = dict(probe=planned_probe,engine='native',status='PASS',runs=[0],outputs=[],sources=[],
                    publication_entries=[],cache=str(cache),registry=str(registry),
                    inputs_before=suite.snapshot(paths),inputs_after=suite.snapshot(paths))
                report = dict(runs=[row],scratch_directory=str(root))
                suite.validate_phase(phase,report,args,workspace,None,item)
                for mutate in (
                    lambda p,r:p['inputs_before'].clear(),
                    lambda p,r:p['runs'].append(0),
                    lambda p,r:r['runs'][0].update(argv=['wrong','build']),
                    lambda p,r:r['runs'][0].update(name=r['runs'][0]['name'][:-1]+'1'),
                    lambda p,r:p['outputs'].append({'invented':True}),
                    lambda p,r:p['sources'].append({'invented':True}),
                    lambda p,r:p['publication_entries'].append('unexpected-tree')):
                    changed_phase,changed_report = copy.deepcopy((phase,report))
                    mutate(changed_phase,changed_report)
                    with self.assertRaises((ValueError,KeyError)):
                        suite.validate_phase(changed_phase,changed_report,args,workspace,None,item)

    def test_negative_replay_rederives_sources_commands_and_diagnostics(self):
        with tempfile.TemporaryDirectory(dir=suite.ROOT/'artifacts') as tmp:
            values = self.prepare(Path(tmp)); suite.validate_phase(*values)
            for mutate in (
                lambda p,r:p['runs'].clear(),lambda p,r:p['runs'].append(0),lambda p,r:p.update(status='FAIL'),
                lambda p,r:p['inputs_before'].clear(),lambda p,r:r['runs'][0].update(stderr='generic error'),
                lambda p,r:r['runs'][0].update(argv=['fake','query']),lambda p,r:r['runs'][0].update(cwd='/other'),
                lambda p,r:r['runs'][0].update(name='wrong'),lambda p,r:p['outputs'].append({'invented':True})):
                phase,report = copy.deepcopy(values[:2]); mutate(phase,report)
                with self.assertRaises((KeyError,ValueError)):
                    suite.validate_phase(phase,report,*values[2:])

    def test_rehashed_altered_fixture_rejected(self):
        with tempfile.TemporaryDirectory(dir=suite.ROOT/'artifacts') as tmp:
            phase,report,*rest = self.prepare(Path(tmp))
            p = next(p for p in phase['inputs_before'] if p.endswith('defs.bzl')); raw = b'altered\n'
            for key in ('inputs_before','inputs_after'):
                phase[key][p].update(bytes_base64=base64.b64encode(raw).decode(),sha256=suite.digest(raw),size_bytes=len(raw))
            with self.assertRaisesRegex(ValueError,'immutable fixture'):
                suite.validate_phase(phase,report,*rest)

    def test_status_labels_cannot_mint_pass(self):
        for report in ({},{'status':'PASS_ONLY_BOUNDED_PROFILE'},{'status':'REFERENCE_ONLY_PREPARATION_PASS'}):
            self.assertFalse(suite.report_passes(report))


class SafetyTests(unittest.TestCase):
    def test_controlled_environment_is_fixed_and_runner_does_not_copy_ambient(self):
        # Patching the mapping detects all reads, including key enumeration.
        class Poison:
            def __getattribute__(self,name):
                raise AssertionError('ambient environment inspected: '+name)
            def __iter__(self):
                raise AssertionError('ambient environment enumerated')
        with mock.patch.object(suite.controlled.os,'environ',Poison()):
            runner = suite.RecordingRunner({'runs':[]},10,Path('/owned'))
            self.assertEqual(suite.controlled.controlled_environment(runner.scratch,{}),
                {'PATH':'/usr/bin:/bin','HOME':'/owned/home','TMPDIR':'/owned/tmp','LC_ALL':'C.UTF-8'})

    def test_commands_never_add_relevant_or_ambient_env(self):
        args = suite.argument_parser().parse_args([])
        ref = suite.DirectReference(args.reference,Path('/owned/workspace'),Path('/owned'),None,'string-dict')
        for engine in ('native','reference'):
            argv = suite.command_argv(args,ref,engine,Path('/owned/cache'),Path('/owned/registry'),'build','//:result')
            self.assertFalse(any(a.startswith('--repo_env') for a in argv))

    def test_portable_source_has_no_ignored_research_dependency(self):
        tree = ast.parse(Path(suite.__file__).read_text())
        for node in ast.walk(tree):
            if isinstance(node,ast.Constant) and isinstance(node.value,str):
                self.assertNotIn('/home/coder/bazel',node.value)
                self.assertNotIn('artifacts/string-dict-contract.json',node.value)
        self.assertEqual(len(suite.OFFICIAL),2)
        self.assertEqual(suite.DEFECT_PROVENANCE['source_files'],suite.OFFICIAL)


if __name__=='__main__':
    (suite.ROOT/'artifacts').mkdir(exist_ok=True)
    unittest.main()
