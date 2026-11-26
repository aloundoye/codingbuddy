# CodingBuddy

CodingBuddy is an open-source AI coding agent for the terminal, with an optional browser UI and VS Code extension. It can explore a repository, plan changes, edit files, run approved checks, and resume work from the same local session.

Use a hosted provider, a local Ollama model, or an OpenAI-compatible gateway. CodingBuddy supports OpenAI Chat Completions and Responses, Anthropic Messages, Gemini GenerateContent, AWS Bedrock Converse, Vertex AI Gemini, Azure OpenAI, and compatible routes such as DeepSeek and OpenRouter. Coding tasks need a model that supports tool calls.

[Install](#installation) · [Quickstart](#quickstart) · [Provider setup](docs/PROVIDER_SUPPORT.md) · [User guide](docs/USER_GUIDE.md) · [0.5.0 release notes](docs/releases/0.5.0.md)

## Installation

The installers select a prebuilt binary for macOS, GNU/Linux, or Windows on x64 or ARM64.

**macOS or Linux**

```bash
curl -fsSL https://raw.githubusercontent.com/aloundoye/codingbuddy/main/scripts/install.sh | bash
```

**Windows PowerShell**

```powershell
irm https://raw.githubusercontent.com/aloundoye/codingbuddy/main/scripts/install.ps1 | iex
```

You can also download the archive and checksums from [GitHub Releases](https://github.com/aloundoye/codingbuddy/releases), or build from source with Rust 1.94.1 or later. On Linux, source builds require pkg-config and the D-Bus development headers.

```bash
cargo build --locked --release --bin codingbuddy
./target/release/codingbuddy --version
```

A source build puts the binary in `target/release/` (`codingbuddy.exe` on Windows). Move it onto your PATH to use it from any repository. Installers may require a new terminal to pick up the updated PATH. Run `codingbuddy doctor` to check your setup.

## Quickstart

From your repository, run `codingbuddy` and choose **Connect a provider**. Use an environment credential or save one in the system credential store. Ollama can run without an API key. Use `/models` to select a model, `/connect` to switch providers, and `/` to browse commands.

```bash
codingbuddy ask --tools=false "Summarize this repository"
codingbuddy plan "Add pagination to the search results"
codingbuddy review --staged
codingbuddy --continue
```

For scripts, select a configured profile with `codingbuddy --model provider/model -p "Summarize this repo"`. Use `--json` when a caller needs structured output.

### Providers and verification

| Route | Setup |
| --- | --- |
| OpenAI, Anthropic, Gemini, DeepSeek | Select a provider profile and connect its credential. Set `chat_protocol = "openai-responses"` to use OpenAI Responses. |
| Ollama | Run a local server with an installed tool-capable model for coding tasks. |
| AWS Bedrock, Vertex AI, Azure OpenAI | Configure the deployment and local cloud identity in user settings. |
| OpenRouter, Groq, Mistral, xAI, Together, custom gateways | Use a preset or configure a compatible endpoint, model, and credential. |

```bash
codingbuddy provider status --json
codingbuddy provider check --provider deepseek --json
codingbuddy provider verify --provider deepseek --json
```

`status` reads local evidence. `check` sends a bounded text request to confirm connectivity. `verify` runs an explicit live tool round trip using the selected route and may incur charges. The [support matrix](docs/PROVIDER_SUPPORT.md) distinguishes fixture coverage from live results and documents cloud configuration and gateway extensions.

## Sessions and recovery

CodingBuddy journals the order of model output, tool calls, and results to SQLite. CLI, web, and VS Code use the same session runner. Resume a session on its original provider route. To move to another route, use `session portable-fork` to preview the shareable history and create a linked session.

Background agents can survive a normal CLI exit. Inspect their durable status with `background list`, read their logs with `background attach JOB_ID`, or request cancellation with `background stop JOB_ID`. A stopped or interrupted tool whose effect cannot be verified is reported for local review. Worktree output is staged for review before applying it to the parent workspace.

To restore a checkpoint, preview the changes, then approve the selected plan.

```bash
codingbuddy rewind --preview
codingbuddy rewind --plan PLAN_ID --mode files --yes
```

A restore preserves unrelated edits and flags conflicts with newer changes. Use `--mode conversation` to create a conversation fork, or `--mode both` to combine it with a file restore. See [execution and recovery](docs/DELIVERY_ORDER_3.md) for job ownership, restore reconciliation, and compatibility details.

## Workspace trust and approvals

User settings live in `~/.codingbuddy/settings.json`. Keep provider credentials in your environment, or let `/connect` save them to `~/.codingbuddy/credentials.json` (readable only by you, mode 0600). Project and local project settings can configure runtime behavior after you trust the repository. Use its canonical absolute path.

```bash
CODINGBUDDY_TRUST_WORKSPACE="$(pwd -P)" codingbuddy chat
```

Explicit denies take precedence over saved approvals and hooks. Untrusted workspaces require fresh approval for commands that can execute project code. Unsandboxed shell calls also require fresh approval. Headless runs decline commands when fresh approval is required. Linux uses Bubblewrap and macOS uses Seatbelt for supported isolation requests. Windows declines commands that require isolation.

Hooks, MCP servers, shell commands, and language tooling start with a minimal environment. Put named grants in user or managed settings if an integration needs an environment variable. For example, grant one variable to one project MCP server.

```json
{
  "process_env": {
    "grants": [
      {
        "consumer": "mcp:issue-tracker",
        "source": "project",
        "workspace": "/absolute/path/to/project",
        "variables": ["ISSUE_TRACKER_TOKEN"]
      }
    ]
  }
}
```

MCP interpolation uses the same grants. Managed restrictions take precedence. See the [user guide](docs/USER_GUIDE.md) for settings, skills, hooks, and custom agents.

## Web and VS Code

Run `codingbuddy serve --transport http --web` and open the printed URL with its token fragment. The server listens on loopback and scopes access to the launch workspace. To reauthorize a saved browser session after restarting, launch with `--expose-session SESSION_ID` and open the new URL. The [web source](web/) also supports a Vite development proxy.

The [VS Code extension](extensions/vscode/) connects over stdio. It supports live prompt events, cancellation, and local checkpoint review.

## Upgrading to 0.5.0

Stop older CodingBuddy processes before reopening a workspace. Existing SQLite sessions remain available, and CodingBuddy imports a legacy JSONL-only journal once while retaining the original archive. Review the [upgrade notes](docs/releases/0.5.0.md#before-upgrading) if you use hooks, background commands, custom sandbox wrappers, or a cloud gateway.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
```

The [release workflow](.github/workflows/release.yml) gates publication on the locked dependency audit, trust and isolation checks, provider fixtures, and unattended conformance for the six native targets. The [Order 4 audit](docs/DELIVERY_ORDER_4.md) records local verification and the limits of its evidence.

## License

[MIT](LICENSE)
