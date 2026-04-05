use super::*;

/// Returns tool definitions for the DeepSeek API function calling interface.
/// Parameter names MUST match what `run_tool()` reads from `call.args`.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "fs_read".to_string(),
                description: "Reads a file from the filesystem and returns its contents with line numbers.\n\n\
## CRITICAL RULES\n\
- You MUST call fs_read BEFORE making any claims about a file's contents. NEVER guess or fabricate what a file contains.\n\
- You MUST call fs_read BEFORE calling fs_edit on any file. The edit tool requires an exact string match, so you need to see the current content first.\n\
- DO NOT use bash_run with cat, head, tail, or sed to read files — use this tool instead. It provides structured output with line numbers and metadata.\n\n\
## Usage\n\
- By default reads the entire file (up to max_bytes, default 1MB).\n\
- For large files, use start_line and end_line to read specific sections. This is especially useful when you already know which lines to inspect.\n\
- Returns line-numbered content in the format `  N→content` for easy reference.\n\
- For binary files (images), returns base64-encoded content with MIME type metadata.\n\
- For PDF files, extracts text content. Use the `pages` parameter (e.g. '1-5') for large PDFs.\n\n\
## When to use\n\
- Before editing any file (ALWAYS)\n\
- To verify file contents before making claims about them\n\
- To understand existing code before suggesting modifications\n\
- To check current state after making edits\n\n\
## When NOT to use\n\
- To search for content across many files — use fs_grep instead\n\
- To find files by name pattern — use fs_glob instead\n\
- To list directory contents — use fs_list instead".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path to the file to read"
                        },
                        "start_line": {
                            "type": "integer",
                            "description": "1-based line number to start reading from. Optional."
                        },
                        "end_line": {
                            "type": "integer",
                            "description": "1-based line number to stop reading at. Optional."
                        },
                        "max_bytes": {
                            "type": "integer",
                            "description": "Maximum bytes to read. Defaults to 1MB."
                        },
                        "pages": {
                            "type": "string",
                            "description": "Page range for PDF files (e.g. '1-5', '3', '10-20'). Only applicable to PDF files."
                        }
                    },
                    "required": ["path"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "fs_write".to_string(),
                description: "Creates or completely overwrites a file with the specified content. Creates parent directories automatically if they don't exist.\n\n\
## CRITICAL RULES\n\
- ALWAYS prefer fs_edit over fs_write for modifying existing files. fs_write replaces the ENTIRE file content, which is dangerous for partial changes.\n\
- If the file already exists, you MUST have read it with fs_read first to understand what will be overwritten.\n\
- NEVER write files that contain secrets, credentials, API keys, or sensitive data.\n\n\
## When to use\n\
- Creating new files that don't exist yet\n\
- Complete rewrites where fs_edit would require too many individual replacements\n\
- Writing generated content (configs, boilerplate, test fixtures)\n\n\
## When NOT to use\n\
- Making targeted changes to existing files — use fs_edit instead\n\
- Making multiple small edits across a file — use fs_edit or multi_edit instead".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path to the file to write"
                        },
                        "content": {
                            "type": "string",
                            "description": "The full content to write to the file"
                        }
                    },
                    "required": ["path", "content"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "fs_edit".to_string(),
                description: "Performs exact string replacement in a file. The search string must match EXACTLY — including whitespace, indentation, and line endings.\n\n\
## CRITICAL RULES\n\
- You MUST call fs_read on the file BEFORE using fs_edit. The search string must be copied exactly from the file's current content. If you guess the content, the edit will fail.\n\
- The search string must be unique enough to match only the intended location. If the search matches multiple places, all occurrences are replaced by default (set 'all': false for first-only).\n\
- Preserve the exact indentation (tabs/spaces) as shown in the fs_read output. The line number prefix (e.g. '  42→') is NOT part of the file content — do not include it in the search string.\n\n\
## When to use\n\
- Making targeted changes to existing files (the preferred editing approach)\n\
- Renaming variables, functions, or identifiers\n\
- Updating specific code blocks, imports, or configuration values\n\
- Any modification where you want to change only part of a file\n\n\
## When NOT to use\n\
- Creating new files — use fs_write instead\n\
- Complete file rewrites — use fs_write instead\n\
- Editing multiple files at once — use multi_edit instead\n\n\
## Tips\n\
- If the search string is not unique, include more surrounding context (extra lines above/below) to make it unique.\n\
- After editing, consider reading the file again to verify the change was applied correctly.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path to the file to edit"
                        },
                        "search": {
                            "type": "string",
                            "description": "The exact text to find in the file"
                        },
                        "replace": {
                            "type": "string",
                            "description": "The text to replace the search string with"
                        },
                        "all": {
                            "type": "boolean",
                            "description": "Replace all occurrences (true, default) or just first (false)"
                        }
                    },
                    "required": ["path", "search", "replace"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "fs_list".to_string(),
                description: "Lists files and directories in a single directory level. Returns names, types (file/dir), and sizes.\n\n\
## When to use\n\
- To see what's in a directory before navigating deeper\n\
- To discover project structure at the top level\n\
- Quick check of a specific directory's immediate contents\n\n\
## When NOT to use\n\
- For recursive file searching — use fs_glob with a pattern like '**/*.rs' instead\n\
- For searching file contents — use fs_grep instead\n\
- To read a specific file — use fs_read instead".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "dir": {
                            "type": "string",
                            "description": "Directory path to list. Defaults to '.' (workspace root)."
                        }
                    },
                    "required": []
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "fs_glob".to_string(),
                description: "Fast file pattern matching that finds files by name/path pattern. Returns matching file paths sorted by modification time.\n\n\
## CRITICAL RULES\n\
- ALWAYS use fs_glob for finding files by name. NEVER use bash_run with find or ls for file discovery.\n\
- Use this tool when you need to locate files before reading or editing them.\n\n\
## Pattern examples\n\
- '**/*.rs' — all Rust files recursively\n\
- 'src/**/*.ts' — TypeScript files under src/\n\
- '*.json' — JSON files in root only\n\
- '**/test_*.py' — Python test files anywhere\n\
- 'Cargo.toml' — exact file name anywhere (if base is '.')\n\n\
## When to use\n\
- Finding files by extension or name pattern\n\
- Discovering project structure (e.g. all config files, all test files)\n\
- Locating a file when you know part of its name but not the full path\n\n\
## When NOT to use\n\
- Searching file CONTENTS — use fs_grep instead (fs_glob only matches file paths)\n\
- Listing a single directory — use fs_list for simpler directory listing\n\
- Reading file contents — use fs_read after finding the path".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern to match (e.g. '**/*.rs', 'src/**/*.ts')"
                        },
                        "base": {
                            "type": "string",
                            "description": "Base directory to search in. Defaults to '.'."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum results to return. Defaults to 200."
                        },
                        "respectGitignore": {
                            "type": "boolean",
                            "description": "Whether to respect .gitignore rules. Defaults to true."
                        }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "fs_grep".to_string(),
                description: "Searches file contents using regex patterns. Returns matching lines with file paths and line numbers. Built on ripgrep for fast, accurate results.\n\n\
## CRITICAL RULES\n\
- ALWAYS use fs_grep for searching file contents. NEVER use bash_run with grep, rg, ag, or ack — this tool is faster and provides structured output.\n\
- Supports full regex syntax (e.g. 'log.*Error', 'function\\s+\\w+', 'impl\\s+Display').\n\n\
## When to use\n\
- Finding where a function, class, variable, or string is defined or used\n\
- Searching for error messages, log patterns, or specific code constructs\n\
- Finding all usages of an API or import across the codebase\n\
- Locating TODO/FIXME/HACK comments\n\n\
## When NOT to use\n\
- Finding files by name pattern — use fs_glob instead (fs_grep searches contents, not names)\n\
- Reading a specific file — use fs_read instead\n\
- Listing directory contents — use fs_list instead\n\n\
## Tips\n\
- Use the 'glob' parameter to narrow the search to specific file types (e.g. '**/*.rs' for Rust files only)\n\
- Use case_sensitive: false for case-insensitive searches\n\
- Results include file path, line number, and matching line content".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for"
                        },
                        "glob": {
                            "type": "string",
                            "description": "Glob pattern to filter which files to search (e.g. '**/*.rs'). Defaults to '**/*'."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum matches to return. Defaults to 200."
                        },
                        "respectGitignore": {
                            "type": "boolean",
                            "description": "Whether to respect .gitignore rules. Defaults to true."
                        },
                        "case_sensitive": {
                            "type": "boolean",
                            "description": "Whether the search is case-sensitive. Defaults to true."
                        }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "bash_run".to_string(),
                description: "Executes a shell command in the workspace directory and returns stdout/stderr.\n\n\
## CRITICAL RULES\n\
- DO NOT use bash_run for operations that have dedicated tools:\n\
  - Reading files: use fs_read (NOT cat, head, tail, less)\n\
  - Searching file contents: use fs_grep (NOT grep, rg, ag, ack)\n\
  - Finding files: use fs_glob (NOT find, ls -R, fd)\n\
  - Editing files: use fs_edit or multi_edit (NOT sed, awk, perl -i)\n\
  - Writing files: use fs_write (NOT echo >, cat <<EOF, tee)\n\
  - Git status: use git_status (NOT git status)\n\
  - Git diff: use git_diff (NOT git diff)\n\
Using dedicated tools provides structured output, better error handling, and clearer audit trail.\n\n\
## USE bash_run for\n\
- Building projects: cargo build, npm run build, make\n\
- Running tests: cargo test, pytest, npm test\n\
- Package management: cargo add, npm install, pip install\n\
- Git operations beyond status/diff: git commit, git push, git log, git branch\n\
- System commands: docker, curl (for APIs), env checks, process management\n\
- Language-specific tools: rustfmt, eslint --fix, black\n\
- Any command that doesn't have a dedicated tool equivalent\n\n\
## Tips\n\
- Always provide a 'description' so the user understands what the command does\n\
- Commands time out after 120 seconds by default. Set a higher timeout for long builds.\n\
- Quote file paths with spaces using double quotes\n\
- Prefer absolute paths to avoid directory confusion".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute"
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Timeout in seconds. Defaults to 120."
                        },
                        "description": {
                            "type": "string",
                            "description": "Short description of what this command does"
                        }
                    },
                    "required": ["command"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "multi_edit".to_string(),
                description: "Apply multiple search/replace edits across one or more files in a single atomic operation. Each file entry contains a path and an array of edits.\n\n\
## CRITICAL RULES\n\
- You MUST have read each file with fs_read before editing it. Search strings must exactly match current file content.\n\
- Same rules as fs_edit apply to each individual edit: exact string match required, preserve indentation.\n\n\
## When to use\n\
- Making related changes across multiple files (e.g. renaming a function and updating all call sites)\n\
- Applying several edits to the same file in one operation\n\
- Refactoring that touches many files simultaneously\n\n\
## When NOT to use\n\
- Single edit to a single file — use fs_edit instead (simpler)\n\
- Creating new files — use fs_write instead\n\
- Complete file rewrites — use fs_write instead".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": { "type": "string", "description": "Relative file path" },
                                    "edits": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "search": { "type": "string", "description": "Text to find" },
                                                "replace": { "type": "string", "description": "Replacement text" }
                                            },
                                            "required": ["search", "replace"]
                                        }
                                    }
                                },
                                "required": ["path", "edits"]
                            },
                            "description": "Array of files with their edit operations"
                        }
                    },
                    "required": ["files"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "git_status".to_string(),
                description: "Shows the working tree status (staged, unstaged, untracked files) in short format.\n\n\
## When to use\n\
- Before committing: check which files are staged, modified, or untracked\n\
- After making changes: verify that only the intended files were modified\n\
- Before creating a PR: ensure no unintended files are included\n\
- To check for merge conflicts or unresolved state\n\n\
## When NOT to use\n\
- To see file contents — use fs_read instead\n\
- To see actual diff of changes — use git_diff instead\n\
- To see commit history — use bash_run with 'git log'\n\n\
## Common mistakes\n\
- Do NOT run bash_run with 'git status' — use this tool, it's faster and returns structured output".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "git_diff".to_string(),
                description: "Shows the unified diff of all unstaged changes in the working directory.\n\n\
## When to use\n\
- Before committing: review exactly what changed to write an accurate commit message\n\
- After editing files: verify your changes are correct and complete\n\
- To understand what modifications exist before deciding next steps\n\
- To compare current working tree against the last commit\n\n\
## When NOT to use\n\
- To see staged changes — use bash_run with 'git diff --cached' instead\n\
- To compare branches — use bash_run with 'git diff branch1..branch2'\n\
- To see which files changed (without content) — use git_status instead\n\
- To see a specific file's full content — use fs_read instead\n\n\
## Common mistakes\n\
- Do NOT run bash_run with 'git diff' — use this tool, it returns structured output\n\
- This only shows UNSTAGED changes. If you just staged files, the diff will be empty".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "web_fetch".to_string(),
                description: "Fetches content from a URL and returns it as text. HTML is automatically stripped to plain text.\n\n\
## When to use\n\
- Fetching documentation, API references, or web pages referenced by the user\n\
- Downloading configuration or data files from URLs\n\
- Checking API endpoints or service responses\n\n\
## Limitations\n\
- Will fail for authenticated/private URLs (Google Docs, Confluence, Jira, private GitHub repos)\n\
- HTTP URLs are automatically upgraded to HTTPS\n\
- Large pages may be truncated at max_bytes (default 500KB)".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The URL to fetch (must start with http:// or https://)"
                        },
                        "max_bytes": {
                            "type": "integer",
                            "description": "Maximum bytes to retrieve. Defaults to 500000 (500KB)."
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Request timeout in seconds. Defaults to 30."
                        }
                    },
                    "required": ["url"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "web_search".to_string(),
                description: "Searches the web and returns results with titles, URLs, and snippets. Use for finding documentation, looking up APIs, researching error messages, or getting up-to-date information beyond the model's training data.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query"
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum results to return. Defaults to 10."
                        }
                    },
                    "required": ["query"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "notebook_read".to_string(),
                description: "Read a Jupyter notebook (.ipynb file), returning all cells with their type (code/markdown), source content, and output summaries. Use this to understand notebook structure before making edits with notebook_edit.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path to the notebook file"
                        }
                    },
                    "required": ["path"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "notebook_edit".to_string(),
                description: "Edit a cell in a Jupyter notebook (.ipynb file). Supports replace (overwrite cell content), insert (add new cell), and delete operations. You MUST read the notebook with notebook_read first to know the cell indices and current content.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path to the notebook file"
                        },
                        "cell_index": {
                            "type": "integer",
                            "description": "0-based cell index to edit"
                        },
                        "new_source": {
                            "type": "string",
                            "description": "New content for the cell"
                        },
                        "cell_type": {
                            "type": "string",
                            "enum": ["code", "markdown"],
                            "description": "Cell type. Optional for replace, required for insert."
                        },
                        "operation": {
                            "type": "string",
                            "enum": ["replace", "insert", "delete"],
                            "description": "Edit operation. Defaults to 'replace'."
                        }
                    },
                    "required": ["path", "cell_index", "new_source"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "git_show".to_string(),
                description: "Show a git object (commit, tag, tree, or blob). Use to inspect commit details, view files at specific revisions (e.g. 'HEAD:src/main.rs'), or compare changes between commits (e.g. 'main..feature'). For current working directory changes, use git_diff instead.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "spec": {
                            "type": "string",
                            "description": "Git revision spec (e.g. 'HEAD', 'abc123', 'HEAD:src/main.rs', 'main..feature')"
                        }
                    },
                    "required": ["spec"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "index_query".to_string(),
                description: "Full-text semantic search across the code index. Returns matching file paths and snippets ranked by relevance. Use for conceptual searches (e.g. 'authentication middleware') when you don't know the exact string. For exact pattern matching, use fs_grep instead.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "q": {
                            "type": "string",
                            "description": "Search query string"
                        },
                        "top_k": {
                            "type": "integer",
                            "description": "Maximum results to return. Defaults to 10."
                        }
                    },
                    "required": ["q"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "patch_stage".to_string(),
                description: "Stage a unified diff as a patch for later application. Returns a patch_id for use with patch_apply. Use this for complex multi-file changes where you want to prepare a patch first and apply it atomically. For simple edits, prefer fs_edit or multi_edit instead.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "unified_diff": {
                            "type": "string",
                            "description": "The unified diff content to stage"
                        },
                        "base": {
                            "type": "string",
                            "description": "Base file content for SHA verification. Optional."
                        }
                    },
                    "required": ["unified_diff"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "patch_apply".to_string(),
                description: "Apply a previously staged patch by its patch_id (from patch_stage). Returns success/failure and any conflicts. Always stage with patch_stage first before applying.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "patch_id": {
                            "type": "string",
                            "description": "UUID of the staged patch to apply"
                        }
                    },
                    "required": ["patch_id"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "patch_direct".to_string(),
                description: "Apply a unified diff directly to the workspace in one step. \
This is the preferred tool for applying code changes when you have a unified diff. \
Unlike patch_stage + patch_apply (which require two calls), this combines both steps. \
\n\nThe diff must be in standard unified diff format:\n\
```\n\
--- a/path/to/file.rs\n\
+++ b/path/to/file.rs\n\
@@ -10,3 +10,4 @@\n\
 unchanged line\n\
-old line\n\
+new line\n\
+added line\n\
 unchanged line\n\
```\n\n\
Returns the list of affected files and whether the patch applied cleanly. \
If there are conflicts, they are reported in the response. \
For simple single-site edits, prefer fs_edit. Use this tool for multi-hunk or multi-file changes \
where a unified diff is more natural.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "unified_diff": {
                            "type": "string",
                            "description": "The unified diff to apply. Must use standard unified diff format with --- a/ and +++ b/ headers."
                        }
                    },
                    "required": ["unified_diff"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "diagnostics_check".to_string(),
                description: "Run language-specific diagnostics (cargo check, tsc, ruff, etc.) on the project or a specific path. Auto-detects the appropriate checker based on project files. Use this after making changes to verify they compile and pass basic checks. Prefer this over running build commands manually with bash_run when you just need to check for errors.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Optional path to check. If omitted, checks the entire project."
                        }
                    },
                    "required": []
                }),
            },
        },
        // ── Batch tool ────────────────────────────────────────────────────
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "batch".to_string(),
                description: "Execute multiple read-only tool calls in a single request. \
Use this when you need to perform several independent read operations (reading multiple files, \
searching in parallel, checking git status and diff together). \
\n\nRules:\n\
- Only read-only tools allowed: fs_read, fs_list, fs_glob, fs_grep, git_status, git_diff, \
git_show, web_fetch, web_search, notebook_read, index_query, diagnostics_check\n\
- Cannot nest batch inside batch\n\
- Cannot batch write tools (fs_edit, fs_write, bash_run) — use individual calls for those\n\
- Maximum 25 tool calls per batch\n\
\nReturns an array of results with per-tool success/error status.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "tool_calls": {
                            "type": "array",
                            "description": "Array of tool calls to execute",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "tool": {
                                        "type": "string",
                                        "description": "Name of the tool to call (e.g. 'fs.read', 'fs.grep')"
                                    },
                                    "parameters": {
                                        "type": "object",
                                        "description": "Parameters to pass to the tool"
                                    }
                                },
                                "required": ["tool", "parameters"]
                            },
                            "minItems": 1,
                            "maxItems": 25
                        }
                    },
                    "required": ["tool_calls"]
                }),
            },
        },
        // ── LSP tools (code intelligence) ───────────────────────────────
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "lsp_hover".to_string(),
                description: "Get type information and documentation for a symbol at a specific position in a file.\n\n\
Use this when you need to understand the type, signature, or documentation of a function, variable, \
class, or other symbol. Works with 50+ languages via Language Server Protocol.\n\n\
Returns the hover information (type signature, docs) from the language server. \
If no LSP server is available for the file type, returns a message saying so.\n\n\
The file must exist and the position must be valid (1-based line and column).".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path to the file" },
                        "line": { "type": "integer", "description": "1-based line number" },
                        "column": { "type": "integer", "description": "1-based column number" }
                    },
                    "required": ["path", "line", "column"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "lsp_definition".to_string(),
                description: "Jump to the definition of a symbol at a specific position.\n\n\
Returns the file path and line number where the symbol is defined. \
Works for functions, types, variables, imports, and other symbols across 50+ languages.\n\n\
If the symbol has multiple definitions (e.g. overloaded functions), returns all locations.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path to the file" },
                        "line": { "type": "integer", "description": "1-based line number" },
                        "column": { "type": "integer", "description": "1-based column number" }
                    },
                    "required": ["path", "line", "column"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "lsp_references".to_string(),
                description: "Find all references to a symbol at a specific position.\n\n\
Returns a list of file paths and line numbers where the symbol is used. \
Includes the declaration itself. Works across files in the workspace.\n\n\
Useful for understanding the impact of a change before refactoring.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path to the file" },
                        "line": { "type": "integer", "description": "1-based line number" },
                        "column": { "type": "integer", "description": "1-based column number" }
                    },
                    "required": ["path", "line", "column"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "lsp_symbols".to_string(),
                description: "List all symbols (functions, classes, types, variables) in a file.\n\n\
Returns a hierarchical outline of the file's structure. Each symbol includes \
its name, kind (function, class, struct, etc.), and line number.\n\n\
Use this to quickly understand the structure of a file without reading the entire content.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path to the file" }
                    },
                    "required": ["path"]
                }),
            },
        },
        // ── Agent-level tools (handled by AgentEngine, not LocalToolHost) ───
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "user_question".to_string(),
                description: "Ask the user a question and wait for their response. Use when requirements are genuinely ambiguous or you need a decision to proceed.\n\n\
## CRITICAL RULES\n\
- Do NOT ask to confirm actions the user already explicitly requested. If they said 'fix the bug', just fix it.\n\
- Do NOT ask permission before using tools. Just use them.\n\
- DO ask when: there are multiple valid approaches and user preference matters, requirements are unclear, you need information that isn't in the codebase.\n\n\
## Tips\n\
- Provide 'options' when the choices are clear-cut, so the user can pick quickly\n\
- Keep questions concise and specific — ask one thing at a time".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The question to ask the user"
                        },
                        "options": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional list of suggested answer choices"
                        }
                    },
                    "required": ["question"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "task_create".to_string(),
                description: "Create a task to track progress on the current work. Returns the task ID.\n\n\
## When to use\n\
- Complex multi-step work: break the work into trackable tasks before starting\n\
- User provides multiple items: capture each as a separate task\n\
- Non-trivial implementations: create tasks so the user can see progress\n\n\
## When NOT to use\n\
- Single trivial tasks (e.g. fixing a typo, answering a question)\n\
- Tasks that can be completed in one step without tracking\n\n\
## Tips\n\
- Use imperative form for subject ('Fix auth bug', not 'Fixing auth bug')\n\
- Include enough detail in description for another agent to complete the task\n\
- After creating tasks, use task_update to set status as you work".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "subject": {
                            "type": "string",
                            "description": "Brief title for the task"
                        },
                        "description": {
                            "type": "string",
                            "description": "Detailed description of what needs to be done"
                        },
                        "priority": {
                            "type": "integer",
                            "description": "Priority level (0=low, 1=normal, 2=high). Defaults to 1."
                        }
                    },
                    "required": ["subject"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "task_update".to_string(),
                description: "Update a task's status or details.\n\n\
## When to use\n\
- Mark a task as in_progress BEFORE starting work on it\n\
- Mark a task as completed AFTER fully finishing the work\n\
- Mark a task as failed if you encounter unresolvable blockers\n\
- Update the outcome field with a summary when completing or failing\n\n\
## When NOT to use\n\
- Do NOT mark a task completed if tests are failing or implementation is partial\n\
- Do NOT update tasks that don't exist — use task_list to check first\n\n\
## Status workflow\n\
pending → in_progress → completed (or failed)\n\
Always set in_progress before starting, completed only when fully done".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "UUID of the task to update"
                        },
                        "status": {
                            "type": "string",
                            "enum": ["pending", "in_progress", "completed", "failed"],
                            "description": "New status for the task"
                        },
                        "outcome": {
                            "type": "string",
                            "description": "Optional outcome description (typically set when completing or failing)"
                        }
                    },
                    "required": ["task_id", "status"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "todo_read".to_string(),
                description: "Read the session-native working checklist for this conversation.\n\n\
## When to use\n\
- At the start of complex work to see the current checklist\n\
- Before updating todos so you keep existing IDs and ordering stable\n\
- After subagent work to decide which items to mark complete\n\n\
## Returns\n\
- Current todo items with id/content/status\n\
- Summary counts (active/completed/in_progress)\n\
- Current executing item (if any)\n\n\
## Notes\n\
- This checklist is session-local and separate from task queue delegation.\n\
- Use task_* tools for durable delegated/background units; use todo_* for the live checklist.".to_string(),
                strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "todo_write".to_string(),
                description: "Replace the session-native working checklist with an updated ordered list.\n\n\
## CRITICAL RULES\n\
- Send the FULL desired checklist each time; omitted items are removed.\n\
- Keep exactly one in_progress item while actively executing.\n\
- Mark items completed as soon as work is verified.\n\
- Preserve existing ids when possible (read first with todo_read).\n\n\
## Status values\n\
- pending\n\
- in_progress\n\
- completed\n\
\n\
## Notes\n\
- This checklist is separate from task_* delegated work records.\n\
- For complex tasks, update this after each meaningful step.".to_string(),
                strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "items": {
                            "type": "array",
                            "description": "Complete ordered todo checklist for this session.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": {
                                        "type": "string",
                                        "description": "Existing todo UUID (optional; preserve if known)."
                                    },
                                    "content": {
                                        "type": "string",
                                        "description": "Todo item description."
                                    },
                                    "status": {
                                        "type": "string",
                                        "enum": ["pending", "in_progress", "completed"],
                                        "description": "Todo status."
                                    }
                                },
                                "required": ["content"]
                            }
                        }
                    },
                    "required": ["items"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "spawn_task".to_string(),
                description: "Launch a specialized sub-agent to handle complex, multi-step tasks autonomously. Each agent type has specific capabilities.\n\n\
## Agent types\n\
- 'explore': Fast codebase search/read — use for finding files, searching code, answering questions about the codebase\n\
- 'plan': Design implementation approaches — returns step-by-step plans with architectural considerations\n\
- 'bash': Command execution — for build, test, deploy tasks that need many sequential commands\n\
- 'general-purpose': Full capabilities — for complex multi-step tasks requiring all tools\n\n\
## Tips\n\
- Launch multiple agents concurrently when tasks are independent (use separate calls)\n\
- Provide clear, detailed prompts so the agent can work autonomously".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "description": {
                            "type": "string",
                            "description": "A short (3-5 word) description of the task"
                        },
                        "prompt": {
                            "type": "string",
                            "description": "The task for the agent to perform"
                        },
                        "subagent_type": {
                            "type": "string",
                            "enum": ["explore", "plan", "bash", "general-purpose"],
                            "description": "The type of specialized agent: 'explore' for codebase search/read, 'plan' for designing approaches, 'bash' for command execution, 'general-purpose' for complex multi-step tasks"
                        },
                        "model": {
                            "type": "string",
                            "description": "Optional model override for this delegated task"
                        },
                        "max_turns": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Optional maximum turns for the delegated task"
                        },
                        "run_in_background": {
                            "type": "boolean",
                            "description": "When true, run the delegated task as a detached background job and return immediately with tracking IDs"
                        }
                    },
                    "required": ["description", "prompt", "subagent_type"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "task_output".to_string(),
                description: "Read the latest persisted output for a delegated task or subagent run. Use this after spawn_task when you need the current status, the child session ID, or the final summary.".to_string(),
                strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "Task UUID returned by task_create or spawn_task"
                        },
                        "run_id": {
                            "type": "string",
                            "description": "Subagent run UUID returned by spawn_task"
                        }
                    },
                    "required": []
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "send_message".to_string(),
                description: "Send a follow-up message to a running or completed subagent. Use this to continue a previously spawned agent's work, ask for clarification, or provide additional instructions. The agent resumes with its full context preserved.".to_string(),
                strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "to": {
                            "type": "string",
                            "description": "The agent's run_id (UUID) or name to send the message to"
                        },
                        "message": {
                            "type": "string",
                            "description": "The message/instruction to send to the agent"
                        }
                    },
                    "required": ["to", "message"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "task_stop".to_string(),
                description: "Stop a running delegated background task. Use this when a spawned task is hung, no longer needed, or should be cancelled before completion.".to_string(),
                strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "Task UUID returned by task_create or spawn_task"
                        },
                        "run_id": {
                            "type": "string",
                            "description": "Subagent run UUID returned by spawn_task"
                        }
                    },
                    "required": []
                }),
            },
        },
        // ── Plan mode tools (handled by AgentEngine) ──────────────────────
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "enter_plan_mode".to_string(),
                description: "Enter plan mode to design an implementation approach before writing code. In plan mode, you can only use read-only tools (Read, Glob, Grep, search, git status/diff). Use this proactively when the task requires planning: new features, multiple valid approaches, multi-file changes, or unclear requirements. You will explore the codebase, design a plan, and present it for user approval before executing.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "exit_plan_mode".to_string(),
                description: "Exit plan mode after writing your plan. This signals that you are done planning and ready for the user to review and approve. The user will see the plan and decide whether to let you proceed with execution. Only use this after you have completed your plan.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "allowedPrompts": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "tool": { "type": "string", "description": "The tool this permission applies to (e.g. 'bash_run')" },
                                    "prompt": { "type": "string", "description": "Description of the action (e.g. 'run tests', 'install dependencies')" }
                                },
                                "required": ["tool", "prompt"]
                            },
                            "description": "Prompt-based permissions needed to implement the plan"
                        }
                    },
                    "required": []
                }),
            },
        },
        // ── Background task management tools (handled by AgentEngine) ─────
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "task_get".to_string(),
                description: "Retrieve full details of a task by its ID.\n\n\
## When to use\n\
- Before starting work on a task: read the full description and requirements\n\
- To check task dependencies (blockedBy) before claiming it\n\
- To understand the full context of what a task requires\n\n\
## Returns\n\
- subject, description, status (pending/in_progress/completed/failed)\n\
- blocks: tasks waiting on this one\n\
- blockedBy: tasks that must complete first\n\n\
## Tips\n\
- Use task_list for a summary of all tasks; use task_get for one task's full details\n\
- Always verify blockedBy is empty before starting work on a task".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "The ID of the task to retrieve"
                        }
                    },
                    "required": ["task_id"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "task_list".to_string(),
                description: "List all tasks in the task list with summary info.\n\n\
## When to use\n\
- To see what tasks are available to work on (status=pending, not blocked)\n\
- To check overall progress on the project\n\
- After completing a task, to find the next one to work on\n\
- To verify task dependencies and identify blocked work\n\n\
## Returns\n\
For each task: id, subject, status, owner, blockedBy list\n\n\
## Tips\n\
- Prefer working on tasks in ID order (lowest first) — earlier tasks set up context\n\
- Use task_get with a specific ID to see full description and requirements\n\
- Tasks with non-empty blockedBy cannot be started until dependencies resolve".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
        },
        // ── Worktree isolation tools ─────────────────────────────────────
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "enter_worktree".to_string(),
                description: "Create an isolated git worktree and switch the session's working directory into it. \
This lets you experiment on a separate branch without affecting the main checkout. \
All subsequent tool calls operate inside the worktree until you call exit_worktree.\n\n\
## When to use\n\
- Before making risky or experimental changes that you may want to discard\n\
- When you need to work on a separate branch while preserving the main checkout\n\
- For safe prototyping: try an approach, then merge or discard\n\n\
## Important\n\
- A new branch is created automatically from the current HEAD\n\
- You MUST call exit_worktree when done to merge changes or clean up\n\
- Only one worktree can be active at a time per session".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "branch_name": {
                            "type": "string",
                            "description": "Name for the worktree branch (e.g. 'experiment/try-new-api'). Auto-generated if omitted."
                        }
                    },
                    "required": []
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "exit_worktree".to_string(),
                description: "Exit the current git worktree and return to the main checkout. \
Changes made in the worktree can be merged or discarded.\n\n\
## When to use\n\
- After finishing experimental work in a worktree\n\
- To merge successful changes back to the main branch\n\
- To discard failed experiments\n\n\
## Actions\n\
- merge: merge the worktree branch into the original branch, then clean up\n\
- discard: delete the worktree and branch without merging".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["merge", "discard"],
                            "description": "What to do with the worktree changes: 'merge' to keep, 'discard' to throw away"
                        }
                    },
                    "required": ["action"]
                }),
            },
        },
        // ── Skill tool (LLM invokes slash commands) ──────────────────────
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "skill".to_string(),
                description: "Execute a skill (slash command) within the current conversation. Use this when the user asks you to perform tasks that match available skills, or when they reference a slash command like '/commit' or '/review-pr'.".to_string(),
            strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "The skill name to invoke (e.g. 'commit', 'review-pr', 'pdf')"
                        },
                        "args": {
                            "type": "string",
                            "description": "Optional arguments for the skill"
                        }
                    },
                    "required": ["skill"]
                }),
            },
        },
        // ── extended_thinking: R1 consultation for complex subproblems ──
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "extended_thinking".to_string(),
                description: "Escalate to deep reasoning model for problems requiring extensive \
                              chain-of-thought beyond the main model's thinking budget. Use when \
                              you encounter: repeated failures on the same approach, architectural \
                              decisions with multiple valid options, complex error analysis needing \
                              root cause identification, or task decomposition for multi-step \
                              changes. Returns strategic advice — you keep control and execute \
                              the recommended approach."
                    .to_string(),
                strict: None,
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The specific question to reason about"
                        },
                        "context": {
                            "type": "string",
                            "description": "Relevant context: error messages, file contents, constraints, what you have tried so far"
                        },
                        "type": {
                            "type": "string",
                            "enum": ["error_analysis", "architecture_advice", "plan_review", "task_decomposition"],
                            "description": "Type of reasoning needed"
                        }
                    },
                    "required": ["question", "context", "type"]
                }),
            },
        },
    ]
}

/// Tools allowed in plan mode.
///
/// Plan mode blocks file edits and shell execution, but still allows planning
/// metadata tools such as task creation and plan completion.
pub static PLAN_MODE_TOOLS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    tool_definitions()
        .into_iter()
        .filter_map(|tool| codingbuddy_core::ToolName::from_api_name(&tool.function.name))
        .filter(|tool| tool.is_allowed_in_phase(codingbuddy_core::TaskPhase::Plan))
        .map(|tool| tool.as_api_name())
        .collect()
});

/// Filter tool definitions by allowed/disallowed lists.
///
/// - If `allowed` is `Some`, only tools whose `function.name` is in the list are kept.
/// - If `disallowed` is `Some`, tools whose `function.name` is in the list are removed.
/// - `allowed` and `disallowed` should not both be `Some` (caller must validate).
pub fn filter_tool_definitions(
    tools: Vec<ToolDefinition>,
    allowed: Option<&[String]>,
    disallowed: Option<&[String]>,
) -> Vec<ToolDefinition> {
    let tools = if let Some(allow_list) = allowed {
        tools
            .into_iter()
            .filter(|t| allow_list.iter().any(|a| a == &t.function.name))
            .collect()
    } else {
        tools
    };
    if let Some(deny_list) = disallowed {
        tools
            .into_iter()
            .filter(|t| !deny_list.iter().any(|d| d == &t.function.name))
            .collect()
    } else {
        tools
    }
}

/// Map tool definition function names (underscored) to internal tool names (dotted).
///
/// Delegates to [`codingbuddy_core::ToolName`] for known tools. Unknown names
/// (plugins, MCP tools) pass through unchanged.
pub fn map_tool_name(function_name: &str) -> &str {
    match codingbuddy_core::ToolName::from_api_name(function_name) {
        Some(t) => t.as_internal(),
        None => function_name,
    }
}

/// Return a user-friendly hint for a tool error, or `None` if no specific hint applies.
pub fn tool_error_hint(tool_name: &str, error_msg: &str) -> Option<String> {
    let lower = error_msg.to_ascii_lowercase();
    match tool_name {
        "fs.edit" | "multi_edit" => {
            if lower.contains("search pattern not found") {
                Some("Hint: the old_string was not found in the file. Try reading the file first with fs.read to verify the exact content.".to_string())
            } else if lower.contains("line range out of bounds") {
                Some("Hint: the line range exceeds the file length. Read the file first to check how many lines it has.".to_string())
            } else {
                None
            }
        }
        "fs.read" => {
            if lower.contains("no such file") || lower.contains("not found") {
                Some(
                    "Hint: file does not exist. Use fs.glob to search for the correct path."
                        .to_string(),
                )
            } else if lower.contains("permission denied") {
                Some(
                    "Hint: permission denied. The file may be outside the allowed workspace."
                        .to_string(),
                )
            } else {
                None
            }
        }
        "fs.write" => {
            if lower.contains("permission denied") {
                Some(
                    "Hint: permission denied. Check that the directory exists and is writable."
                        .to_string(),
                )
            } else {
                None
            }
        }
        "bash.run" => {
            if lower.contains("timed out") || lower.contains("timeout") {
                Some(
                    "Hint: command timed out. Try a shorter operation or increase the timeout."
                        .to_string(),
                )
            } else if lower.contains("not found") || lower.contains("command not found") {
                Some(
                    "Hint: command not found. Check that the program is installed and in PATH."
                        .to_string(),
                )
            } else if lower.contains("forbidden shell metacharacters") {
                Some(
                    "Hint: bash.run blocks shell metacharacters (;, &&, ||, backticks, $()). \
                     A single pipeline (|) is allowed only when each command segment is allowlisted. \
                     Do NOT retry with similar commands. Instead use the built-in tools: \
                     fs.grep for searching file contents, fs.glob for finding files by pattern, \
                     fs.read for reading files. These tools do not have shell restrictions."
                        .to_string(),
                )
            } else if lower.contains("not allowlisted") {
                Some(
                    "Hint: this command is not in the allowed command list. \
                     Do NOT retry other shell commands — most are restricted. Instead use built-in tools: \
                     fs.glob to list/find files, fs.grep to search content, fs.read to read files, \
                     git_status/git_diff/git_show for git operations. \
                     Only allowlisted commands (e.g. cargo, git, rg) can be run via bash.run."
                        .to_string(),
                )
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Validate tool call arguments against the tool's JSON schema.
///
/// Returns `Ok(())` if the arguments are valid, or `Err(message)` with a
/// structured description of which fields are wrong, so the model can
/// self-correct instead of getting cryptic runtime errors.
///
/// This complements `validation::validate_tool_args` which does deeper
/// semantic checks (required fields, range checks). This function validates
/// against the formal JSON schema from tool definitions.
pub fn validate_tool_args_schema(
    tool_name: &str,
    args: &serde_json::Value,
    tools: &[ToolDefinition],
) -> Result<(), String> {
    // Find the tool definition by API name
    let tool_def = tools.iter().find(|t| t.function.name == tool_name);
    let Some(tool_def) = tool_def else {
        return Ok(()); // Unknown tool — skip validation (MCP, plugin)
    };

    let schema = &tool_def.function.parameters;
    if schema.is_null() || schema.as_object().is_some_and(|o| o.is_empty()) {
        return Ok(()); // No schema defined
    }

    // Compile and validate
    let validator = match jsonschema::validator_for(schema) {
        Ok(v) => v,
        Err(_) => return Ok(()), // Invalid schema — skip validation
    };

    let errors: Vec<String> = validator
        .iter_errors(args)
        .map(|e| {
            let path = e.instance_path.to_string();
            if path.is_empty() {
                e.to_string()
            } else {
                format!("{}: {}", path, e)
            }
        })
        .collect();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Invalid arguments for tool '{}': {}",
            tool_name,
            errors.join("; ")
        ))
    }
}

/// Tools that are handled by AgentEngine directly, not by LocalToolHost.
pub static AGENT_LEVEL_TOOLS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    tool_definitions()
        .into_iter()
        .filter_map(|tool| codingbuddy_core::ToolName::from_api_name(&tool.function.name))
        .filter(|tool| tool.is_agent_level())
        .map(|tool| tool.as_api_name())
        .collect()
});
