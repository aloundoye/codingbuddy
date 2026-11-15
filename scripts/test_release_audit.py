import datetime
import unittest

from release_audit import TARGETS, check, validate_exceptions


class ReleaseAuditTests(unittest.TestCase):
    def setUp(self):
        self.today = datetime.date(2026, 11, 15)
        self.advisory = "RUSTSEC-2026-1234"
        self.target = TARGETS[0]
        self.graphs = {target: {("codingbuddy-cli", "1.0")} for target in TARGETS}
        self.graphs[self.target].add(("fixture", "1.2.3"))
        self.audit = {"vulnerabilities": {"count": 1, "list": [{
            "advisory": {"id": self.advisory},
            "package": {"name": "fixture", "version": "1.2.3"},
        }]}}
        self.entry = {
            "advisory": self.advisory, "package": "fixture", "version": "1.2.3",
            "targets": [self.target], "reachability": "release dependency graph",
            "mitigation": "fixture capability disabled", "owner": "release owner",
            "expires": "2026-12-01",
        }

    def test_vulnerability_is_blocked_until_matching_exception_exists(self):
        self.assertEqual(check(self.audit, self.graphs, set()), [
            (self.advisory, "fixture", "1.2.3", self.target),
        ])
        exceptions = validate_exceptions([self.entry], self.today)
        self.assertEqual(check(self.audit, self.graphs, exceptions), [])
        self.graphs[TARGETS[1]].add(("fixture", "1.2.3"))
        self.assertEqual(len(check(self.audit, self.graphs, exceptions)), 1)

    def test_missing_graph_and_malformed_or_expired_exceptions_fail_closed(self):
        del self.graphs[self.target]
        with self.assertRaises(ValueError):
            check(self.audit, self.graphs, set())
        for change in [
            {"expires": "2026-11-15"}, {"owner": ""}, {"targets": []},
            {"advisory": "RUSTSEC-2026-0187"},
        ]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                validate_exceptions([{**self.entry, **change}], self.today)


if __name__ == "__main__":
    unittest.main()
