use anyhow::Result;
use serde::Serialize;
use std::fmt;
use std::fs;
use std::path::Path;

use codingbuddy_core::AppConfig;

use crate::util::command_exists;

/// Detected project metadata from scanning the working directory.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct DetectedProject {
    pub languages: Vec<String>,
    pub build_cmd: Option<String>,
    pub test_cmd: Option<String>,
    pub lint_cmd: Option<String>,
    pub fmt_cmd: Option<String>,
    pub package_manager: Option<String>,
}

impl DetectedProject {
    /// Format a human-readable summary of the detection results.
    pub(crate) fn summary(&self, settings_written: bool) -> String {
        let mut lines = Vec::new();
        if !self.languages.is_empty() {
            lines.push(format!("detected: {}", self.languages.join(", ")));
        }
        if let Some(ref cmd) = self.build_cmd {
            lines.push(format!("build: {cmd}"));
        }
        if let Some(ref cmd) = self.test_cmd {
            lines.push(format!("test: {cmd}"));
        }
        if let Some(ref cmd) = self.lint_cmd {
            lines.push(format!("lint: {cmd}"));
        }
        if let Some(ref cmd) = self.fmt_cmd {
            lines.push(format!("fmt: {cmd}"));
        }
        if settings_written {
            lines.push("wrote project settings to .codingbuddy/settings.json".to_string());
        }
        lines.join("\n")
    }
}

impl fmt::Display for DetectedProject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary(false))
    }
}

/// Scan the working directory and detect project type, build/test/lint commands.
pub(crate) fn detect_project(cwd: &Path) -> DetectedProject {
    let mut project = DetectedProject::default();

    // Rust
    if cwd.join("Cargo.toml").exists() {
        project.languages.push("rust".to_string());
        project.build_cmd = Some("cargo build".to_string());
        project.test_cmd = Some("cargo test".to_string());
        project.lint_cmd = Some("cargo clippy -- -D warnings".to_string());
        project.fmt_cmd = Some("cargo fmt --all -- --check".to_string());
        project.package_manager = Some("cargo".to_string());
    }

    // Node.js / TypeScript
    let pkg_json = cwd.join("package.json");
    if pkg_json.exists() {
        if cwd.join("tsconfig.json").exists() || cwd.join("tsconfig.base.json").exists() {
            project.languages.push("typescript".to_string());
        } else {
            project.languages.push("javascript".to_string());
        }

        if cwd.join("bun.lockb").exists() || cwd.join("bun.lock").exists() {
            project.package_manager = Some("bun".to_string());
        } else if cwd.join("pnpm-lock.yaml").exists() {
            project.package_manager = Some("pnpm".to_string());
        } else if cwd.join("yarn.lock").exists() {
            project.package_manager = Some("yarn".to_string());
        } else {
            project.package_manager = Some("npm".to_string());
        }

        if let Ok(contents) = fs::read_to_string(&pkg_json)
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&contents)
            && let Some(scripts) = parsed.get("scripts").and_then(|s| s.as_object())
        {
            let pm = project.package_manager.as_deref().unwrap_or("npm");
            let run_prefix = if pm == "npm" { "npm run" } else { pm };
            if scripts.contains_key("build") && project.build_cmd.is_none() {
                project.build_cmd = Some(format!("{run_prefix} build"));
            }
            if scripts.contains_key("test") && project.test_cmd.is_none() {
                project.test_cmd = Some(format!("{run_prefix} test"));
            }
            if scripts.contains_key("lint") && project.lint_cmd.is_none() {
                project.lint_cmd = Some(format!("{run_prefix} lint"));
            }
            if scripts.contains_key("format") && project.fmt_cmd.is_none() {
                project.fmt_cmd = Some(format!("{run_prefix} format"));
            } else if scripts.contains_key("fmt") && project.fmt_cmd.is_none() {
                project.fmt_cmd = Some(format!("{run_prefix} fmt"));
            }
        }
    }

    // Go
    if cwd.join("go.mod").exists() {
        project.languages.push("go".to_string());
        if project.build_cmd.is_none() {
            project.build_cmd = Some("go build ./...".to_string());
        }
        if project.test_cmd.is_none() {
            project.test_cmd = Some("go test ./...".to_string());
        }
        if project.lint_cmd.is_none() {
            project.lint_cmd = if command_exists("golangci-lint") {
                Some("golangci-lint run".to_string())
            } else {
                Some("go vet ./...".to_string())
            };
        }
        if project.package_manager.is_none() {
            project.package_manager = Some("go".to_string());
        }
    }

    // Python
    let pyproject = cwd.join("pyproject.toml");
    let has_pyproject = pyproject.exists();
    if has_pyproject || cwd.join("setup.py").exists() || cwd.join("requirements.txt").exists() {
        project.languages.push("python".to_string());
        if has_pyproject && let Ok(contents) = fs::read_to_string(&pyproject) {
            if contents.contains("[tool.poetry]") {
                project.package_manager = project.package_manager.or(Some("poetry".to_string()));
            } else if contents.contains("[project]") {
                if cwd.join("uv.lock").exists() {
                    project.package_manager = project.package_manager.or(Some("uv".to_string()));
                } else {
                    project.package_manager = project.package_manager.or(Some("pip".to_string()));
                }
            }
        }
        if project.test_cmd.is_none() {
            project.test_cmd = if cwd.join("pytest.ini").exists() || has_pyproject {
                Some("pytest".to_string())
            } else {
                Some("python -m pytest".to_string())
            };
        }
        if project.lint_cmd.is_none() {
            project.lint_cmd = Some("ruff check .".to_string());
        }
        if project.fmt_cmd.is_none() {
            project.fmt_cmd = Some("ruff format --check .".to_string());
        }
    }

    // Java / Kotlin
    if cwd.join("pom.xml").exists() {
        project.languages.push("java".to_string());
        project.build_cmd = project.build_cmd.or(Some("mvn compile".to_string()));
        project.test_cmd = project.test_cmd.or(Some("mvn test".to_string()));
        project.package_manager = project.package_manager.or(Some("maven".to_string()));
    } else {
        let kts = cwd.join("build.gradle.kts").exists();
        if kts || cwd.join("build.gradle").exists() {
            project
                .languages
                .push(if kts { "kotlin" } else { "java" }.to_string());
            let wrapper = if cwd.join("gradlew").exists() {
                "./gradlew"
            } else {
                "gradle"
            };
            project.build_cmd = project.build_cmd.or(Some(format!("{wrapper} build")));
            project.test_cmd = project.test_cmd.or(Some(format!("{wrapper} test")));
            project.package_manager = project.package_manager.or(Some("gradle".to_string()));
        }
    }

    project
}

/// Write detected project settings to `.codingbuddy/settings.json`.
/// Returns `true` if settings were written (only writes if file doesn't exist yet).
pub(crate) fn write_project_settings(cwd: &Path, project: &DetectedProject) -> Result<bool> {
    let settings_path = AppConfig::project_settings_path(cwd);

    if let Ok(contents) = fs::read_to_string(&settings_path) {
        let trimmed = contents.trim();
        if !trimmed.is_empty() && trimmed != "{}" {
            return Ok(false);
        }
    }

    if project.languages.is_empty() {
        return Ok(false);
    }

    let mut wrapper = serde_json::Map::new();
    wrapper.insert("project".to_string(), serde_json::to_value(project)?);

    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&serde_json::Value::Object(wrapper))?,
    )?;

    Ok(true)
}
