import os
from pathlib import Path
import subprocess
import sys
import unittest


RUNNER = Path(__file__).with_name("run_isolated_tests.py")


class IsolatedTestRunnerTests(unittest.TestCase):
    def test_child_loses_live_session_context_and_keeps_other_environment(self):
        environment = dict(os.environ, HERDR_SOCKET_PATH="/unrelated/live.sock",
                           HERDR_CLIENT_SOCKET_PATH="/unrelated/client.sock",
                           HERDR_SESSION="human-session", HERDR_RUN_NAME="caller",
                           TEST_RUNNER_KEEP="preserved")
        check = ("import os; assert not any(k.startswith('HERDR_') for k in os.environ); "
                 "assert os.environ['TEST_RUNNER_KEEP'] == 'preserved'")
        result = subprocess.run([sys.executable, str(RUNNER), sys.executable, "-c", check],
                                env=environment, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        # The same real child rejects the unsafe direct entry. This proves the
        # failure-first boundary without a production injection flag.
        direct = subprocess.run([sys.executable, "-c", check], env=environment,
                                capture_output=True, text=True)
        self.assertNotEqual(direct.returncode, 0)

    def test_child_failure_is_the_runner_failure(self):
        result = subprocess.run([sys.executable, str(RUNNER), sys.executable,
                                 "-c", "raise SystemExit(37)"])
        self.assertEqual(result.returncode, 37)


if __name__ == "__main__":
    unittest.main()
