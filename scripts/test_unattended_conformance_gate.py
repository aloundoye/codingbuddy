import copy
import unittest
from unattended_conformance import GROUPS, TARGETS
from unattended_conformance_gate import validate


class GateTests(unittest.TestCase):
    def setUp(self):
        self.reports = [{"version": 1, "target": target, "revision": "revision", "lockfile_sha256": "lock",
                         "native": True, "worktree_clean": True, "status": "passed",
                         "groups": {name: {"status": "passed", "exit_code": 0, "passed": 1, "output_sha256": "a" * 64} for name in GROUPS}}
                        for target in sorted(TARGETS)]

    def test_complete_native_matrix_passes(self):
        validate(self.reports, TARGETS, "revision", "lock")

    def test_missing_or_duplicate_platform_blocks(self):
        for reports in [self.reports[:-1], self.reports + [self.reports[0]]]:
            with self.assertRaises(ValueError):
                validate(reports, TARGETS, "revision", "lock")

    def test_failed_missing_empty_stale_and_cross_compiled_evidence_blocks(self):
        mutations = [lambda report: report.update(native=False), lambda report: report.update(revision="old"),
                     lambda report: report.update(lockfile_sha256="old"), lambda report: report.update(worktree_clean=False),
                     lambda report: report["groups"].pop("mcp"), lambda report: report["groups"]["mcp"].update(passed=0),
                     lambda report: report["groups"]["mcp"].update(exit_code=1), lambda report: report["groups"]["mcp"].update(status="failed")]
        for mutate in mutations:
            reports = copy.deepcopy(self.reports)
            mutate(reports[0])
            with self.assertRaises(ValueError):
                validate(reports, TARGETS, "revision", "lock")


if __name__ == "__main__":
    unittest.main()
