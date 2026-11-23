#!/usr/bin/env python3
"""Validate complete, current provider evidence before packaging a release."""
import argparse
import json
import re
from pathlib import Path


def validate(report, manifest, target, revision):
    os_name = "windows" if "windows" in target else "macos" if "apple-darwin" in target else "linux"
    architecture = "aarch64" if target.startswith("aarch64-") else "x86_64"
    if (report.get("schema_version") != 1 or report.get("suite") != manifest["suite"]
            or report.get("passed") is not True or report.get("revision") != revision
            or report.get("platform") != os_name or report.get("architecture") != architecture
            or not re.fullmatch(r"[0-9a-f]{64}", report.get("implementation_digest", ""))):
        raise ValueError("missing, stale, failed, or incompatible provider evidence")
    expected = {(route["id"], route["model"], streaming)
                for route in manifest["routes"] for streaming in (False, True)}
    actual = set()
    for row in report.get("routes", []):
        if row.get("passed") is not True or type(row.get("streaming")) is not bool:
            raise ValueError("provider route did not pass")
        key = (row.get("id"), row.get("model"), row["streaming"])
        if key in actual:
            raise ValueError("duplicate provider evidence")
        actual.add(key)
    if actual != expected:
        raise ValueError("incomplete provider route or model coverage")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("report", type=Path)
    parser.add_argument("--target", required=True)
    parser.add_argument("--revision", required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    manifest = json.loads((root / "crates/codingbuddy-llm/fixtures/provider-contracts.json").read_text())
    try:
        validate(json.loads(args.report.read_text()), manifest, args.target, args.revision)
    except (ValueError, OSError, KeyError, TypeError) as error:
        parser.exit(1, f"Provider certification gate: {error}\n")
    print("Provider certification evidence passed")


if __name__ == "__main__":
    main()
