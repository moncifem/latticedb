import contextlib
import io
import os
import stat
import unittest
from unittest import mock

import helpers
from console import __main__ as cli
from console.demo import DemoSource
from console.source import PsqlSource


def run(argv, env=None):
    err = io.StringIO()
    out = io.StringIO()
    env = {} if env is None else env
    with mock.patch.dict(os.environ, env, clear=False), contextlib.redirect_stderr(err), \
            contextlib.redirect_stdout(out):
        try:
            rc = cli.main(argv)
        except SystemExit as e:  # argparse errors
            rc = e.code
    return rc, out.getvalue(), err.getvalue()


class ArgumentTests(unittest.TestCase):
    def test_a_command_is_required(self):
        self.assertEqual(run([])[0], 2)

    def test_bad_port(self):
        rc, _, err = run(["serve", "--demo", "--port", "70000"])
        self.assertEqual(rc, 2)
        self.assertIn("--port", err)
        self.assertEqual(run(["serve", "--demo", "--port", "-1"])[0], 2)
        self.assertEqual(run(["serve", "--demo", "--port", "http"])[0], 2)

    def test_no_bind_option_exists(self):
        # The console cannot be pointed at another address, not even by mistake.
        self.assertEqual(run(["serve", "--demo", "--bind", "0.0.0.0"])[0], 2)
        self.assertEqual(run(["serve", "--demo", "--host", "0.0.0.0"])[0], 2)

    def test_url_with_a_password_is_refused_and_not_echoed(self):
        rc, out, err = run(["serve", "--url", "postgresql://dev:topsecret@127.0.0.1:5432/x"])
        self.assertEqual(rc, 2)
        self.assertNotIn("topsecret", out + err)
        self.assertIn("password", err)

    def test_environment_url_with_a_password_is_refused(self):
        rc, out, err = run(["serve"], {"LATTICE_BOARD_URL": "postgresql://dev:topsecret@h/x"})
        self.assertEqual(rc, 2)
        self.assertNotIn("topsecret", out + err)

    def test_thresholds_are_validated(self):
        self.assertEqual(run(["serve", "--demo", "--stale-minutes", "0"])[0], 2)
        self.assertEqual(run(["serve", "--demo", "--open-hours", "0"])[0], 2)

    def test_port_in_use_is_reported_without_a_traceback(self):
        import socket
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        s.listen(1)
        try:
            rc, out, err = run(["serve", "--demo", "--port", str(s.getsockname()[1])])
        finally:
            s.close()
        self.assertEqual(rc, 1)
        self.assertIn("cannot listen", err)
        self.assertNotIn("Traceback", err)


class StartupTests(unittest.TestCase):
    """main() with serve_forever stubbed out, to see what it builds and prints."""

    def start(self, argv, env=None):
        captured = {}

        def fake_forever(self_):
            captured["server"] = self_

        with mock.patch("console.server.ConsoleServer.serve_forever", fake_forever):
            rc, out, err = run(argv, env)
        if "server" in captured:
            self.addCleanup(captured["server"].server_close)
        return rc, out, err, captured.get("server")

    def test_demo_prints_a_label_and_the_loopback_url(self):
        rc, out, err, srv = self.start(["serve", "--demo", "--port", "0"])
        self.assertEqual(rc, 0)
        self.assertIn("DEMO DATA", out)
        self.assertIn("http://127.0.0.1:", out)
        self.assertTrue(isinstance(srv.service.source, DemoSource))

    def test_real_source_never_prints_the_url(self):
        rc, out, err, srv = self.start(
            ["serve", "--port", "0", "--url", "postgresql://dev@127.0.0.1:5999/boarddb"],
            {"PGPASSWORD": "pw-secret-77"})
        self.assertEqual(rc, 0)
        self.assertIsInstance(srv.service.source, PsqlSource)
        self.assertNotIn("5999", out + err)
        self.assertNotIn("boarddb", out + err)
        self.assertNotIn("pw-secret-77", out + err)
        self.assertIn("read-only", out)

    def test_thresholds_reach_the_service(self):
        rc, out, err, srv = self.start(["serve", "--demo", "--port", "0", "--stale-minutes", "7", "--open-hours", "3"])
        self.assertEqual((srv.service.stale_s, srv.service.open_s), (420, 10800))

    def test_defaults(self):
        rc, out, err, srv = self.start(["serve", "--demo", "--port", "0"])
        self.assertEqual((srv.service.stale_s, srv.service.open_s), (1800, 86400))
        self.assertEqual(srv.server_address[0], "127.0.0.1")


class WrapperTests(unittest.TestCase):
    def test_wrapper_script_is_executable_and_runs_the_module(self):
        path = os.path.join(helpers.ROOT, "lattice-console")
        self.assertTrue(os.stat(path).st_mode & stat.S_IXUSR)
        with open(path) as f:
            text = f.read()
        self.assertIn("exec python3 -m console", text)
        self.assertIn('"$@"', text)


if __name__ == "__main__":
    unittest.main()
