/// A single language server entry.
#[derive(Debug, Clone)]
pub struct LspServerEntry {
    pub language_id: &'static str,
    pub server_command: &'static str,
    pub server_args: &'static [&'static str],
}

/// Lookup the LSP server entry for a file extension (without leading dot).
pub fn lookup_extension(ext: &str) -> Option<LspServerEntry> {
    let (lang_id, cmd, args): (&str, &str, &[&str]) = match ext {
        // Rust
        "rs" => ("rust", "rust-analyzer", &[]),
        // TypeScript / JavaScript
        "ts" | "tsx" => ("typescript", "typescript-language-server", &["--stdio"]),
        "js" | "jsx" | "mjs" | "cjs" => ("javascript", "typescript-language-server", &["--stdio"]),
        // Python
        "py" | "pyi" => ("python", "pyright-langserver", &["--stdio"]),
        // Go
        "go" => ("go", "gopls", &[]),
        // C / C++
        "c" | "h" => ("c", "clangd", &["--background-index"]),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => ("cpp", "clangd", &["--background-index"]),
        // Java
        "java" => ("java", "jdtls", &[]),
        // Kotlin
        "kt" | "kts" => ("kotlin", "kotlin-language-server", &[]),
        // C#
        "cs" => ("csharp", "csharp-ls", &[]),
        // Swift
        "swift" => ("swift", "sourcekit-lsp", &[]),
        // Ruby
        "rb" | "rake" | "gemspec" => ("ruby", "ruby-lsp", &[]),
        // PHP
        "php" => ("php", "intelephense", &["--stdio"]),
        // Lua
        "lua" => ("lua", "lua-language-server", &[]),
        // Zig
        "zig" => ("zig", "zls", &[]),
        // Dart
        "dart" => ("dart", "dart", &["language-server"]),
        // Elixir
        "ex" | "exs" => ("elixir", "elixir-ls", &[]),
        // Haskell
        "hs" | "lhs" => ("haskell", "haskell-language-server", &["--lsp"]),
        // OCaml
        "ml" | "mli" => ("ocaml", "ocamllsp", &[]),
        // Scala
        "scala" | "sc" => ("scala", "metals", &[]),
        // Clojure
        "clj" | "cljs" | "cljc" | "edn" => ("clojure", "clojure-lsp", &[]),
        // Shell
        "sh" | "bash" => ("shellscript", "bash-language-server", &["start"]),
        // YAML
        "yaml" | "yml" => ("yaml", "yaml-language-server", &["--stdio"]),
        // TOML
        "toml" => ("toml", "taplo", &["lsp", "stdio"]),
        // JSON
        "json" | "jsonc" => ("json", "vscode-json-languageserver", &["--stdio"]),
        // CSS / SCSS / LESS
        "css" | "scss" | "less" => ("css", "css-languageserver", &["--stdio"]),
        // HTML
        "html" | "htm" => ("html", "html-languageserver", &["--stdio"]),
        // Vue
        "vue" => ("vue", "vue-language-server", &["--stdio"]),
        // Svelte
        "svelte" => ("svelte", "svelte-language-server", &["--stdio"]),
        // Terraform
        "tf" | "tfvars" => ("terraform", "terraform-ls", &["serve"]),
        // Nix
        "nix" => ("nix", "nixd", &[]),
        // Gleam
        "gleam" => ("gleam", "gleam", &["lsp"]),
        // F#
        "fs" | "fsi" | "fsx" => ("fsharp", "fsautocomplete", &[]),
        // LaTeX
        "tex" | "latex" => ("latex", "texlab", &[]),
        // Prisma
        "prisma" => ("prisma", "prisma-language-server", &["--stdio"]),
        _ => return None,
    };

    Some(LspServerEntry {
        language_id: lang_id,
        server_command: cmd,
        server_args: args,
    })
}

/// Get the language ID for a file extension (without leading dot).
pub fn language_id_for_extension(ext: &str) -> Option<&'static str> {
    lookup_extension(ext).map(|e| e.language_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_extensions_resolve() {
        assert_eq!(language_id_for_extension("rs"), Some("rust"));
        assert_eq!(language_id_for_extension("ts"), Some("typescript"));
        assert_eq!(language_id_for_extension("tsx"), Some("typescript"));
        assert_eq!(language_id_for_extension("py"), Some("python"));
        assert_eq!(language_id_for_extension("go"), Some("go"));
        assert_eq!(language_id_for_extension("java"), Some("java"));
        assert_eq!(language_id_for_extension("cpp"), Some("cpp"));
        assert_eq!(language_id_for_extension("cs"), Some("csharp"));
        assert_eq!(language_id_for_extension("rb"), Some("ruby"));
        assert_eq!(language_id_for_extension("swift"), Some("swift"));
    }

    #[test]
    fn unknown_extensions_return_none() {
        assert_eq!(language_id_for_extension("xyz"), None);
        assert_eq!(language_id_for_extension("md"), None);
        assert_eq!(language_id_for_extension("txt"), None);
    }

    #[test]
    fn server_commands_are_populated() {
        let entry = lookup_extension("rs").unwrap();
        assert_eq!(entry.server_command, "rust-analyzer");
        assert!(entry.server_args.is_empty());

        let entry = lookup_extension("ts").unwrap();
        assert_eq!(entry.server_command, "typescript-language-server");
        assert_eq!(entry.server_args, &["--stdio"]);
    }
}
