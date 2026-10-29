"""Exercise the coordinator's timeout boundary using a real sleeping grandchild, not mocked calls."""
import os
import pathlib
import sys
import tempfile
import time
import unittest
import measure

class TimeoutBoundary(unittest.TestCase):
    @unittest.skipUnless(os.name == 'posix', 'manual measurement workflow runs on Linux')
    def test_timeout_stops_compiler_descendants(self):
        with tempfile.TemporaryDirectory(prefix='lattice-planner-group-') as tmp:
            root=pathlib.Path(tmp)
            started=root/'started'
            survived=root/'survived'
            grandchild=(f'import pathlib,time;pathlib.Path({str(started)!r}).write_text("started");'
                        f'time.sleep(2);pathlib.Path({str(survived)!r}).write_text("leaked")')
            parent=(f'import subprocess,sys,time;subprocess.Popen([sys.executable,"-c",{grandchild!r}]);'
                    'time.sleep(30)')
            result=measure.command([sys.executable,'-c',parent],root/'timeout.log',timeout=1)
            self.assertTrue(started.exists(),'regression must actually launch a grandchild')
            time.sleep(1.3)
            self.assertTrue(result['timed_out'])
            self.assertTrue(result['whole_process_group_killed'])
            self.assertNotEqual(result['exit'],0)
            self.assertFalse(survived.exists(),'compiler child continued after parent timeout')


class CriterionAssessment(unittest.TestCase):
    def test_missing_or_cached_evidence_cannot_accept_the_adr(self):
        from criteria import assess
        value=assess({'measurement_mode':'functional_retry','candidates':{
            'custom':{'C5_added_build_under_60s':True,'C5_dependency_review':True}}})
        self.assertFalse(value['automatic_acceptance'])
        for row in value['candidates'].values():
            self.assertEqual(row['C1']['status'],'unresolved')
            self.assertIsNone(row['C1']['historical_effort_days'])
            self.assertEqual(row['C4']['status'],'unresolved')
            self.assertIsNone(row['C5']['added_clean_build_under_60s'])
        self.assertEqual(value['candidates']['datafusion']['C2']['status'],'failed')

    def test_ratios_recomputed_from_raw_samples_not_claimed_pass_flags(self):
        from criteria import assess
        queries=[{'query':name,'lattice_client':{'samples_ns':[400]*9},
                  'postgres_client':{'samples_ns':[100]*9},'C4_within_3x_pg':True}
                 for name in ('point','range_10000','join','group')]
        row=assess({'candidates':{'parser':{'postgres_comparison':{'queries':queries}}}})['candidates']['parser']
        self.assertEqual(row['C4']['status'],'failed')
        self.assertEqual(set(row['C4']['recomputed_median_client_ratios'].values()),{4.0})
        queries[0]['lattice_client']['samples_ns']=[0]*9
        row=assess({'candidates':{'parser':{'postgres_comparison':{'queries':queries}}}})['candidates']['parser']
        self.assertNotIn('point',row['C4']['recomputed_median_client_ratios'])

    def test_passing_pg_comparison_does_not_invent_phase1_baseline(self):
        from criteria import assess
        queries=[{'query':name,'lattice_client':{'samples_ns':[300]*9},
                  'postgres_client':{'samples_ns':[100]*9}}
                 for name in ('point','range_10000','join','group')]
        row=assess({'candidates':{'custom':{'postgres_comparison':{'queries':queries}}}})['candidates']['custom']
        self.assertEqual(row['C4']['same_runner_postgres_3x'],'passed')
        self.assertEqual(row['C4']['status'],'unresolved')

if __name__=='__main__':unittest.main()
