import os
import stat
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "slt_reference.py")

# A fake psql driven by FAKE_MODE: "ok" answers select 1 and succeeds; "down" exits 2 like a refused
# connection; "reject:CODE" answers select 1 and rejects every other statement with CODE;
# "accept" answers select 1 and accepts everything.
FAKE = """#!/usr/bin/env python3
import os, sys
sql = sys.argv[-1]
mode = os.environ["FAKE_MODE"]
if mode == "down":
    sys.stderr.write("psql: error: connection to server failed\\n")
    sys.exit(2)
if sql.strip() == "select 1":
    print("1")
    sys.exit(0)
if mode.startswith("reject:"):
    if "drop schema" in sql:
        sys.exit(0)
    sys.stderr.write("ERROR:  " + mode.split(":", 1)[1] + "\\n")
    sys.exit(1)
sys.exit(0)
"""


class Slt(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        bindir = os.path.join(self.dir.name, "bin")
        os.mkdir(bindir)
        fake = os.path.join(bindir, "psql")
        with open(fake, "w") as f:
            f.write(FAKE)
        os.chmod(fake, os.stat(fake).st_mode | stat.S_IXUSR)
        self.bindir = bindir

    def script(self, body):
        path = os.path.join(self.dir.name, "t.test")
        with open(path, "w") as f:
            f.write(body)
        return path

    def run_script(self, mode, *paths, path_dirs=None):
        env = dict(os.environ, FAKE_MODE=mode)
        env["PATH"] = (path_dirs if path_dirs is not None else self.bindir + os.pathsep + env["PATH"])
        return subprocess.run([sys.executable, SCRIPT, *paths], capture_output=True, text=True, env=env)

    def test_no_scripts_fails(self):
        r = self.run_script("ok")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("no scripts", r.stdout)

    def test_unreachable_server_fails_preflight(self):
        p = self.script("statement error\nselect nope\n")
        r = self.run_script("down", p)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("not usable", r.stdout)

    def test_missing_psql_fails(self):
        p = self.script("statement ok\nselect 1\n")
        r = self.run_script("ok", p, path_dirs=self.dir.name)
        self.assertNotEqual(r.returncode, 0)

    def test_expected_error_without_code_passes_on_query_error(self):
        p = self.script("statement error\nselect nope\n")
        r = self.run_script("reject:42703", p)
        self.assertEqual(r.returncode, 0, r.stdout)

    def test_expected_code_must_match(self):
        p = self.script("statement error 42P01\nselect * from missing\n")
        self.assertEqual(self.run_script("reject:42P01", p).returncode, 0)
        r = self.run_script("reject:42703", p)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("expected 42P01, got 42703", r.stdout)

    def test_expected_error_that_succeeds_fails(self):
        p = self.script("statement error\nselect 2\n")
        self.assertNotEqual(self.run_script("accept", p).returncode, 0)

    def test_server_lost_mid_script_fails(self):
        # Select 1 passes, then the connection drops: an error record must not count as satisfied.
        flaky = os.path.join(self.bindir, "psql")
        with open(flaky, "w") as f:
            f.write(FAKE.replace('if mode == "down":', 'if mode == "down" or "drop schema" in sql:'))
        p = self.script("statement error\nselect nope\n")
        r = self.run_script("accept", p)
        self.assertNotEqual(r.returncode, 0)
        self.assertNotIn("mismatches: 0", r.stdout)

    def test_query_result_checked(self):
        p = self.script("query I\nselect 2\n----\n1\n")
        self.assertEqual(self.run_script("accept", p).returncode, 1)


if __name__ == "__main__":
    unittest.main()
