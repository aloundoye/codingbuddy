import copy
import json
import unittest
from pathlib import Path
from provider_certification_gate import validate


class ProviderGateTests(unittest.TestCase):
    def test_missing_failed_stale_and_wrong_platform_results_block(self):
        manifest = json.loads((Path(__file__).resolve().parents[1] / "crates/codingbuddy-llm/fixtures/provider-contracts.json").read_text())
        complete = {"schema_version": 1, "suite": manifest["suite"], "revision": "fixture-revision",
                    "platform": "linux", "architecture": "aarch64", "passed": True,
                    "implementation_digest": "a" * 64,
                    "routes": [{"id": route["id"], "model": route["model"], "streaming": stream, "passed": True}
                               for route in manifest["routes"] for stream in (False, True)]}
        validate(complete, manifest, "aarch64-unknown-linux-gnu", "fixture-revision")
        mutations = [{"platform": "windows"}, {"architecture": "x86_64"}, {"revision": "stale"},
                     {"passed": False}, {"routes": complete["routes"][:-1]}, {"implementation_digest": ""},
                     {"routes": complete["routes"] + [complete["routes"][0]]}]
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                broken = copy.deepcopy(complete)
                broken.update(mutation)
                with self.assertRaises(ValueError):
                    validate(broken, manifest, "aarch64-unknown-linux-gnu", "fixture-revision")


if __name__ == "__main__":
    unittest.main()
