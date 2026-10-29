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

if __name__=='__main__':unittest.main()
