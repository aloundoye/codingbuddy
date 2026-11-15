"""Gate release publication on vulnerabilities in its locked production graphs."""
import datetime
import json
import re
import subprocess
import sys
from pathlib import Path


TARGETS = (
    "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin", "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc",
)


def validate_exceptions(entries, today):
    exceptions = set()
    for entry in entries:
        required = ("advisory", "package", "version", "reachability", "mitigation", "owner", "expires")
        if any(not isinstance(entry.get(key), str) or not entry[key].strip() for key in required):
            raise ValueError("incomplete advisory exception")
        if not re.fullmatch(r"RUSTSEC-\d{4}-\d{4,}", entry["advisory"]):
            raise ValueError("invalid RustSec advisory ID")
        if entry["advisory"] == "RUSTSEC-2026-0187":
            raise ValueError("the reachable PDF parser advisory has no exception")
        expiry = datetime.date.fromisoformat(entry["expires"])
        if expiry <= today or expiry > today + datetime.timedelta(days=90):
            raise ValueError(f"exception {entry['advisory']} must expire within 90 days")
        targets = entry.get("targets")
        if not isinstance(targets, list) or not targets or any(target not in TARGETS for target in targets):
            raise ValueError(f"exception {entry['advisory']} has invalid targets")
        for target in targets:
            entry_key = (entry["advisory"], entry["package"], entry["version"], target)
            if entry_key in exceptions:
                raise ValueError(f"duplicate advisory exception {entry_key}")
            exceptions.add(entry_key)
    return exceptions


def check(audit, graphs, exceptions):
    if set(graphs) != set(TARGETS) or not all(graphs.values()):
        raise ValueError("release dependency graph is missing for one or more targets")
    report = audit["vulnerabilities"]
    vulnerabilities = report["list"]
    if report["count"] != len(vulnerabilities):
        raise ValueError("incomplete RustSec report")
    blocked = []
    for vulnerability in vulnerabilities:
        advisory = vulnerability["advisory"]["id"]
        package = vulnerability["package"]
        for target, packages in graphs.items():
            if (package["name"], package["version"]) in packages:
                key = (advisory, package["name"], package["version"], target)
                if key not in exceptions:
                    blocked.append(key)
    return blocked


def production_graph(target):
    result = subprocess.run(
        ["cargo", "tree", "--locked", "--target", target, "-p", "codingbuddy-cli",
         "--edges", "normal,build", "--prefix", "none", "--format", "{p}"],
        check=True, capture_output=True, text=True,
    )
    packages = set(re.findall(r"^([\w-]+) v([^\s]+)", result.stdout, flags=re.M))
    if not any(package == "codingbuddy-cli" for package, _ in packages):
        raise ValueError(f"missing release graph for {target}")
    return packages


def main(audit_path, exception_path):
    audit = json.loads(Path(audit_path).read_text())
    entries = json.loads(Path(exception_path).read_text())
    today = datetime.datetime.now(datetime.timezone.utc).date()
    exceptions = validate_exceptions(entries, today)
    graphs = {target: production_graph(target) for target in TARGETS}
    blocked = check(audit, graphs, exceptions)
    for advisory, package, version, target in blocked:
        print(f"{advisory} {package} {version} is reachable on {target}", file=sys.stderr)
    print(f"Audited {len(graphs)} release targets. {len(blocked)} unexcepted findings.")
    return int(bool(blocked))


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:]))
