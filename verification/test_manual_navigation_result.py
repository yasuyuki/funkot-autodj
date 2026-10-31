import importlib.util
from pathlib import Path
import sys
import tempfile
import time
import unittest

SPEC = importlib.util.spec_from_file_location("navigation_check", Path(__file__).with_name("check_manual_navigation.py"))
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


class ManualNavigationResultTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.tree = Path(self.temp.name)
        (self.tree / "verification/manual_navigation").mkdir(parents=True)
        self.out = self.tree / "evidence"
        self.out.mkdir()
        self.java = self.tree / "java-probe"
        self.marker = self.tree / "executed"

    def run_result(self, output, status=0, violation=None, started=None):
        self.java.write_text("#!" + sys.executable + "\nimport pathlib,sys\n"
                             + "pathlib.Path(" + repr(str(self.marker)) + ").touch()\n"
                             + "print(" + repr(output) + ")\nsys.exit(" + str(status) + ")\n")
        self.java.chmod(0o755)
        return CHECK.run_tlc(self.tree, self.out, str(self.java), self.tree / "jar",
                             "probe", "fixture config", time.monotonic() if started is None else started,
                             violation)

    def test_zero_exit_without_complete_exploration_is_not_pass(self):
        for output in ("", "Model checking completed. No error has been found.",
                       "0 states generated, 0 distinct states found, 0 states left on queue.",
                       "2 states generated, 1 distinct states found, 1 states left on queue.\nModel checking completed. No error has been found."):
            with self.subTest(output=output), self.assertRaises(RuntimeError):
                self.run_result(output)

    def test_tool_error_is_not_a_negative_counterexample(self):
        violation = "Invariant GenerationSafe is violated"
        output = "Error: " + violation + "\nState 1: fixture\n2 states generated, 1 distinct states found, 1 states left on queue."
        for status in (0, 1, 124, 255):
            with self.subTest(status=status), self.assertRaises(RuntimeError):
                self.run_result(output, status, violation)

    def test_missing_trace_is_not_a_negative_counterexample(self):
        violation = "Invariant GenerationSafe is violated"
        with self.assertRaises(RuntimeError):
            self.run_result(violation + "\n2 states generated, 1 distinct states found, 1 states left on queue.", 12, violation)

    def test_expired_total_deadline_does_not_start_another_tool(self):
        with self.assertRaises(RuntimeError):
            self.run_result("", started=time.monotonic() - 15 * 60 - 1)
        self.assertFalse(self.marker.exists())
