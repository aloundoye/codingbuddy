#!/usr/bin/env python3
"""Run native Order 3 fixtures and record revision-scoped evidence. Never runs live probes."""
import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys

GROUPS = {
    "process": ["-p", "codingbuddy-core", "--lib"],
    "operations": ["-p", "codingbuddy-store", "--lib"],
    "restore": ["-p", "codingbuddy-memory", "--lib"],
    "worktree": ["-p", "codingbuddy-subagent", "--lib"],
    "mcp": ["-p", "codingbuddy-mcp", "--lib"],
    "hooks": ["-p", "codingbuddy-hooks", "--lib"],
    "language": ["-p", "codingbuddy-lsp", "--lib"],
    "verification": ["-p", "codingbuddy-tools", "--lib"],
    "local_rpc": ["-p", "codingbuddy-jsonrpc", "--lib"],
    "agent": ["-p", "codingbuddy-agent", "--test", "tool_use_default"],
    "clients": ["-p", "codingbuddy-cli", "--test", "cli_json"],
}
TARGETS = {
    "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin", "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--target")
    args = parser.parse_args()
    host = subprocess.check_output(["rustc", "-vV"], text=True).split("host: ", 1)[1].splitlines()[0]
    target = args.target or host
    if target not in TARGETS or target != host:
        parser.error("Conformance requires execution on the native shipped target")
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    if args.revision != revision:
        parser.error("Evidence revision must match the checked-out implementation")
    report = {
        "version": 1, "revision": args.revision, "target": target, "native": True,
        "lockfile_sha256": hashlib.sha256(pathlib.Path("Cargo.lock").read_bytes()).hexdigest(),
        "worktree_clean": not subprocess.check_output(["git", "status", "--porcelain"], text=True).strip(),
        "groups": {}, "status": "failed",
    }
    for name, selection in GROUPS.items():
        command = ["cargo", "test", "--locked"]
        if args.target:
            command += ["--target", target]
        command += selection + ["delivery_three"]
        print(f"Order 3: {name}", flush=True)
        try:
            result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=900)
            output, code = result.stdout, result.returncode
        except subprocess.TimeoutExpired as error:
            output = (error.stdout or b"").decode(errors="replace") if isinstance(error.stdout, bytes) else error.stdout or ""
            code = 124
        print(output, flush=True)
        rows = re.findall(r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", output)
        passed = sum(int(row[0]) for row in rows)
        verified = code == 0 and len(rows) == 1 and passed > 0 and all(row[1:] == ("0", "0") for row in rows)
        report["groups"][name] = {"status": "passed" if verified else "failed", "passed": passed, "exit_code": code,
                                  "output_sha256": hashlib.sha256(output.encode()).hexdigest()}
    if all(group["status"] == "passed" for group in report["groups"].values()):
        report["status"] = "passed"
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
