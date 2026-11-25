# Security Policy

CodingBuddy runs shell commands, edits files and talks to model providers and
MCP servers on your behalf, so we treat security reports as a priority.

## Supported versions

Only the latest minor release receives security fixes.

| Version | Supported |
|---------|-----------|
| 0.5.x   | Yes       |
| < 0.5   | No        |

## Reporting a vulnerability

Please **do not open a public issue** for a security problem.

Report it privately through GitHub's
[private vulnerability reporting](https://github.com/aloundoye/codingbuddy/security/advisories/new).
Include:

- the CodingBuddy version (`codingbuddy --version`) and your OS
- the provider and permission mode in use
- steps to reproduce, and what an attacker gains

You should get an acknowledgement within 3 business days and a status update
within 10. We will credit you in the release notes unless you ask us not to.

## In scope

- Escaping the workspace (path traversal, symlink escape) in file tools
- Running a command that the permission policy or sandbox should have blocked
  or asked about
- Leaking API keys, OAuth tokens or environment variables to the model, logs,
  MCP servers or child processes beyond `process_env.grants`
- Prompt injection from tool output or MCP results that bypasses an approval
- Authentication flaws in the JSON-RPC / HTTP server or browser RPC token

## Out of scope

- Actions you approved, or that run in `BypassPermissions` mode
- Behaviour of third-party MCP servers, hooks or plugins you installed
- Model output quality that does not cross a security boundary
