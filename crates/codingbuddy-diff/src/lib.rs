use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use codingbuddy_core::workspace_fs::WorkspaceFs;
use codingbuddy_core::{EventKind, Session, runtime_dir};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchSet {
    pub patch_id: Uuid,
    pub base_sha256: String,
    pub unified_diff: String,
    pub created_at: String,
    pub applied: bool,
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub target_files: Vec<String>,
    #[serde(default)]
    pub apply_attempts: u32,
    #[serde(default)]
    pub last_base_sha256: Option<String>,
    #[serde(default)]
    pub last_base_sha_match: Option<bool>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    workspace_preimage: Option<Vec<Option<FileImage>>>,
    #[serde(default)]
    apply_images: Option<ApplyImages>,
    #[serde(default)]
    pub recovery_status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct FileImage {
    digest: String,
    mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApplyImages {
    before: Vec<Option<FileImage>>,
    after: Vec<Option<FileImage>>,
}

#[derive(Debug, Clone)]
pub struct PatchStore {
    workspace: PathBuf,
    root: PathBuf,
}

struct PatchLease(fs::File);

impl Drop for PatchLease {
    fn drop(&mut self) {
        // A concurrent fork can retain a reference to the file until it execs.
        let _ = self.0.unlock();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitApplyStrategy {
    /// Verify a clean application against the staged workspace state.
    Auto,
    /// Legacy three-way selection. Patches requiring a merge must be resolved and staged again.
    ThreeWay,
}

impl PatchStore {
    pub fn new(workspace: &Path) -> Result<Self> {
        let root = runtime_dir(workspace).join("patches");
        fs::create_dir_all(&root)?;
        Ok(Self {
            workspace: workspace.to_path_buf(),
            root,
        })
    }

    pub fn stage(&self, diff: &str, base_blob: &[u8]) -> Result<PatchSet> {
        self.stage_checked(diff, base_blob, |_| Ok(()))
    }

    pub fn stage_checked(
        &self,
        diff: &str,
        base_blob: &[u8],
        check: impl Fn(&Path) -> Result<()>,
    ) -> Result<PatchSet> {
        let target_files = checked_patch_paths(&self.workspace, diff)?;
        for path in &target_files {
            check(Path::new(path))?;
        }
        let base_sha256 = if base_blob.is_empty() {
            hash_workspace_state(&self.workspace, &target_files)?
        } else {
            sha256_hex(base_blob)
        };

        let patch = PatchSet {
            patch_id: Uuid::now_v7(),
            base_sha256,
            unified_diff: diff.to_string(),
            created_at: Utc::now().to_rfc3339(),
            applied: false,
            conflicts: Vec::new(),
            workspace_preimage: Some(workspace_images(&self.workspace, &target_files)?),
            target_files,
            apply_attempts: 0,
            last_base_sha256: None,
            last_base_sha_match: None,
            last_error: None,
            apply_images: None,
            recovery_status: None,
        };
        self.write_patch(&patch)?;
        Ok(patch)
    }

    pub fn list(&self) -> Result<Vec<PatchSet>> {
        let mut patches: Vec<PatchSet> = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                let content = fs::read_to_string(path)?;
                patches.push(serde_json::from_str(&content)?);
            }
        }
        patches.sort_by_key(|p| p.created_at.clone());
        Ok(patches)
    }

    pub fn apply(&self, workspace: &Path, patch_id: Uuid) -> Result<(bool, Vec<String>)> {
        self.apply_with_strategy(workspace, patch_id, GitApplyStrategy::ThreeWay)
    }

    pub fn apply_with_strategy(
        &self,
        workspace: &Path,
        patch_id: Uuid,
        strategy: GitApplyStrategy,
    ) -> Result<(bool, Vec<String>)> {
        self.apply_with_check(workspace, patch_id, strategy, |_| Ok(()))
    }

    pub fn apply_checked(
        &self,
        workspace: &Path,
        patch_id: Uuid,
        check: impl Fn(&Path) -> Result<()>,
    ) -> Result<(bool, Vec<String>)> {
        self.apply_with_check(workspace, patch_id, GitApplyStrategy::ThreeWay, check)
    }

    pub fn apply_for_session(
        &self,
        store: &codingbuddy_store::Store,
        session: Uuid,
        patch_id: Uuid,
        check: impl Fn(&Path) -> Result<()>,
    ) -> Result<(bool, Vec<String>, Option<String>)> {
        let _lease = self.patch_lease()?;
        let was_applied = self.read_patch(patch_id)?.applied;
        let (applied, conflicts) =
            self.apply_locked(&self.workspace, patch_id, GitApplyStrategy::ThreeWay, check)?;
        let recorded =
            store.record_patch_application(session, patch_id, applied, conflicts.clone())?;
        let recovery_status = if was_applied && applied && recorded {
            Some("reconciled".into())
        } else {
            self.read_patch(patch_id)?.recovery_status
        };
        Ok((applied, conflicts, recovery_status))
    }

    pub fn verify_postimage(&self, patch_id: Uuid) -> Result<bool> {
        let patch = self.read_patch(patch_id)?;
        let Some(images) = &patch.apply_images else {
            return Ok(false);
        };
        let paths = checked_patch_paths(&self.workspace, &patch.unified_diff)?;
        Ok(paths == patch.target_files
            && workspace_images(&self.workspace, &paths)? == images.after)
    }

    pub fn apply_with_check(
        &self,
        workspace: &Path,
        patch_id: Uuid,
        strategy: GitApplyStrategy,
        check: impl Fn(&Path) -> Result<()>,
    ) -> Result<(bool, Vec<String>)> {
        let _lease = self.patch_lease()?;
        self.apply_locked(workspace, patch_id, strategy, check)
    }

    fn patch_lease(&self) -> Result<PatchLease> {
        let lease = WorkspaceFs::open(&self.root)?
            .open_lock_file(Path::new("apply.lock"))?
            .into_std();
        lease
            .try_lock()
            .map_err(|error| anyhow!("patch application is busy: {error}"))?;
        Ok(PatchLease(lease))
    }

    fn apply_locked(
        &self,
        workspace: &Path,
        patch_id: Uuid,
        strategy: GitApplyStrategy,
        check: impl Fn(&Path) -> Result<()>,
    ) -> Result<(bool, Vec<String>)> {
        anyhow::ensure!(
            workspace.canonicalize()? == self.workspace.canonicalize()?,
            "patch workspace does not match its store"
        );
        let mut patch = self.read_patch(patch_id)?;
        patch.target_files = checked_patch_paths(workspace, &patch.unified_diff)?;
        for path in &patch.target_files {
            check(Path::new(path))?;
        }
        let current = workspace_images(workspace, &patch.target_files)?;
        if let Some(images) = &patch.apply_images {
            if current == images.after {
                if !patch.applied {
                    patch.applied = true;
                    patch.conflicts.clear();
                    patch.last_error = None;
                    patch.recovery_status = Some("reconciled".into());
                    self.write_patch(&patch)?;
                }
                return Ok((true, Vec::new()));
            }
            if patch.applied || current != images.before {
                return self.apply_failure(
                    &mut patch,
                    "ambiguous",
                    "The workspace differs from the recorded patch images",
                );
            }
            return self.apply_failure(&mut patch, "interrupted", "The patch was interrupted before any recorded file changed. Stage it again to retry.");
        }
        if patch.applied {
            return self.apply_failure(
                &mut patch,
                "unverified",
                "This previously applied patch has no verification record",
            );
        }
        patch.apply_attempts = patch.apply_attempts.saturating_add(1);
        let current_base = hash_workspace_state(workspace, &patch.target_files)?;
        patch.last_base_sha_match = Some(patch.base_sha256 == current_base);
        patch.last_base_sha256 = Some(current_base.clone());
        let stale = patch.workspace_preimage.as_ref().map_or_else(
            || patch.base_sha256 != current_base,
            |expected| expected != &current,
        );
        if stale {
            return self.apply_failure(
                &mut patch,
                "stale_preimage",
                "The workspace changed since this patch was staged",
            );
        }

        let scratch = tempfile::tempdir()?;
        let files = WorkspaceFs::open(workspace)?;
        for (path, image) in patch.target_files.iter().zip(&current) {
            if image.is_some() {
                let dest = scratch.path().join(path);
                fs::create_dir_all(dest.parent().context("patch path has no parent")?)?;
                let mut input = files.open_file(Path::new(path))?.into_std();
                let mut output = fs::File::create(dest)?;
                std::io::copy(&mut input, &mut output)?;
                output.set_permissions(input.metadata()?.permissions())?;
            }
        }
        let preview = apply_plain(scratch.path(), &patch.unified_diff)?;
        if !preview.status.success() {
            let diagnostic = String::from_utf8_lossy(&preview.stderr);
            let summary = if strategy == GitApplyStrategy::ThreeWay {
                format!(
                    "Patch needs a manually resolved preimage to verify its postimage.\n{diagnostic}"
                )
            } else {
                diagnostic.into_owned()
            };
            return self.apply_failure(&mut patch, "conflict", &summary);
        }
        let after = workspace_images(scratch.path(), &patch.target_files)?;
        patch.apply_images = Some(ApplyImages {
            before: current.clone(),
            after: after.clone(),
        });
        patch.recovery_status = Some("pending".into());
        self.write_patch(&patch)?;
        for path in &patch.target_files {
            check(Path::new(path))?;
        }
        if workspace_images(workspace, &patch.target_files)? != current {
            return self.apply_failure(
                &mut patch,
                "ambiguous",
                "The workspace changed before applying the patch",
            );
        }
        let output = apply_plain(workspace, &patch.unified_diff)?;
        let settled = workspace_images(workspace, &patch.target_files)?;
        if settled != after {
            return self.apply_failure(
                &mut patch,
                "ambiguous",
                "Patch application did not produce the expected workspace state",
            );
        }
        patch.applied = true;
        patch.conflicts.clear();
        patch.last_error = None;
        patch.recovery_status = Some(
            if output.status.success() {
                "applied"
            } else {
                "reconciled"
            }
            .into(),
        );
        self.write_patch(&patch)?;
        Ok((true, Vec::new()))
    }

    fn apply_failure(
        &self,
        patch: &mut PatchSet,
        status: &str,
        message: &str,
    ) -> Result<(bool, Vec<String>)> {
        patch.recovery_status = Some(status.into());
        patch.last_error = Some(message.into());
        patch.conflicts = message.lines().map(str::to_owned).collect();
        self.write_patch(patch)?;
        Ok((false, patch.conflicts.clone()))
    }

    pub fn event_for_stage(
        &self,
        session: &Session,
        patch: &PatchSet,
        seq_no: u64,
    ) -> codingbuddy_core::EventEnvelope {
        codingbuddy_core::EventEnvelope {
            seq_no,
            at: Utc::now(),
            session_id: session.session_id,
            kind: EventKind::PatchStaged {
                patch_id: patch.patch_id,
                base_sha256: patch.base_sha256.clone(),
            },
        }
    }

    fn write_patch(&self, patch: &PatchSet) -> Result<()> {
        let json_path = self.root.join(format!("{}.json", patch.patch_id));
        let diff_path = self.root.join(format!("{}.diff", patch.patch_id));
        let mut metadata = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer_pretty(&mut metadata, patch)?;
        metadata.as_file().sync_all()?;
        metadata.persist(json_path)?;
        if !diff_path.exists() {
            fs::write(diff_path, &patch.unified_diff)?;
        }
        Ok(())
    }

    pub fn read_patch(&self, patch_id: Uuid) -> Result<PatchSet> {
        let path = self.root.join(format!("{}.json", patch_id));
        if !path.exists() {
            return Err(anyhow!("unknown patch_id {}", patch_id));
        }
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }
}

fn apply_plain(workspace: &Path, diff: &str) -> Result<std::process::Output> {
    let mut command = codingbuddy_core::process_env::git_inspection(workspace);
    command.args(["apply", "--whitespace=nowarn"]);
    git_apply(command, workspace, diff)
}

fn workspace_images(workspace: &Path, paths: &[String]) -> Result<Vec<Option<FileImage>>> {
    let workspace = WorkspaceFs::open(workspace)?;
    paths
        .iter()
        .map(|path| {
            let path = Path::new(path);
            anyhow::ensure!(
                workspace.resolve(path, true)? == path,
                "patch targets through symbolic links are unsupported"
            );
            let mut file = match workspace.open_file(path) {
                Ok(file) => file.into_std(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let metadata = file.metadata()?;
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o7777
            };
            #[cfg(not(unix))]
            let mode = u32::from(metadata.permissions().readonly());
            let mut digest = Sha256::new();
            std::io::copy(&mut file, &mut digest)?;
            Ok(Some(FileImage {
                digest: format!("{:x}", digest.finalize()),
                mode,
            }))
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn git_apply(mut command: Command, workspace: &Path, diff: &str) -> Result<std::process::Output> {
    let mut child = command
        .arg("-")
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start git apply")?;
    std::thread::scope(|scope| {
        let mut stdin = child.stdin.take().expect("piped git stdin");
        let writer = scope.spawn(move || stdin.write_all(diff.as_bytes()));
        let output = child.wait_with_output()?;
        let written = writer.join().expect("git stdin writer panicked");
        if output.status.success() {
            written?;
        }
        Ok(output)
    })
}

fn hash_workspace_state(workspace: &Path, files: &[String]) -> Result<String> {
    let workspace = WorkspaceFs::open(workspace)?;
    let mut hasher = Sha256::new();
    for path in files {
        hasher.update(path.as_bytes());
        hasher.update([0_u8]);
        let resolved = workspace.resolve(Path::new(path), true)?;
        match workspace.read(&resolved) {
            Ok(bytes) => {
                hasher.update([1_u8]);
                hasher.update(bytes);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => hasher.update([0_u8]),
            Err(err) => return Err(err.into()),
        }
        hasher.update([255_u8]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn checked_patch_paths(workspace: &Path, diff: &str) -> Result<Vec<String>> {
    let mut command = codingbuddy_core::process_env::git_inspection(workspace);
    command.args(["apply", "--numstat", "-z", "--allow-empty"]);
    let output = git_apply(command, workspace, diff)?;
    anyhow::ensure!(
        output.status.success(),
        "invalid patch: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut files = BTreeSet::new();
    for entry in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let path = entry
            .splitn(3, |byte| *byte == b'\t')
            .nth(2)
            .ok_or_else(|| anyhow!("invalid git patch statistics"))?;
        files.insert(std::str::from_utf8(path)?.to_owned());
    }
    // Git's numstat reports only the destination of renames and copies.
    for line in diff.lines() {
        if let Some(path) = line
            .strip_prefix("rename from ")
            .or_else(|| line.strip_prefix("copy from "))
        {
            files.insert(decode_git_path(path)?);
        }
    }

    let workspace = WorkspaceFs::open(workspace)?;
    for path in &files {
        let path = Path::new(path);
        anyhow::ensure!(
            !path.is_absolute() && !workspace.relative(path)?.as_os_str().is_empty(),
            "invalid patch path: {}",
            path.display()
        );
        workspace.resolve(path, true)?;
    }
    Ok(files.into_iter().collect())
}

fn decode_git_path(path: &str) -> Result<String> {
    let Some(quoted) = path.strip_prefix('"') else {
        return Ok(path.to_owned());
    };
    let mut bytes = quoted
        .strip_suffix('"')
        .ok_or_else(|| anyhow!("unterminated git path"))?
        .bytes();
    let mut decoded = Vec::new();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte != b'\\' {
            byte
        } else {
            match bytes.next() {
                Some(b'\\') => b'\\',
                Some(b'"') => b'"',
                Some(b'a') => 7,
                Some(b'b') => 8,
                Some(b'f') => 12,
                Some(b'n') => b'\n',
                Some(b'r') => b'\r',
                Some(b't') => b'\t',
                Some(b'v') => 11,
                Some(first @ b'0'..=b'3') => {
                    let second = bytes.next().filter(|b| (b'0'..=b'7').contains(b));
                    let third = bytes.next().filter(|b| (b'0'..=b'7').contains(b));
                    let (Some(second), Some(third)) = (second, third) else {
                        anyhow::bail!("invalid octal escape in git path");
                    };
                    (first - b'0') * 64 + (second - b'0') * 8 + third - b'0'
                }
                _ => anyhow::bail!("invalid escape in git path"),
            }
        });
    }
    Ok(String::from_utf8(decoded)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn patch_lease_releases_lock_while_another_descriptor_is_open() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let patches = PatchStore::new(directory.path())?;
        let lease = patches.patch_lease()?;
        let inherited = lease.0.try_clone()?;
        assert!(patches.patch_lease().is_err());
        drop(lease);
        let _next = patches.patch_lease()?;
        drop(inherited);
        Ok(())
    }

    #[test]
    fn delivery_one_retries_patch_settlement_without_reapplying_the_patch() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let patches = PatchStore::new(root)?;
        let journal = codingbuddy_store::Store::new(root)?;
        let session = Uuid::now_v7();
        fs::write(root.join("original"), "one\n")?;
        fs::write(root.join("removed"), "two\n")?;
        let diff = "diff --git a/original b/renamed\nsimilarity index 100%\nrename from original\nrename to renamed\n\
            diff --git a/removed b/removed\ndeleted file mode 100644\n--- a/removed\n+++ /dev/null\n@@ -1 +0,0 @@\n-two\n";
        let patch = patches.stage(diff, &[])?;
        journal.db()?.execute_batch(
            "CREATE TRIGGER suppress_applied BEFORE INSERT ON events
            WHEN NEW.kind='PatchApplied@v1' BEGIN SELECT RAISE(ABORT,'fixture interruption'); END;",
        )?;
        assert!(
            patches
                .apply_for_session(&journal, session, patch.patch_id, |_| Ok(()))
                .is_err()
        );
        let renamed = fs::read(root.join("renamed"))?;
        assert!(matches!(renamed.as_slice(), b"one\n" | b"one\r\n"));
        assert!(!root.join("original").exists() && !root.join("removed").exists());
        assert!(journal.session_events(session)?.is_empty());
        journal
            .db()?
            .execute_batch("DROP TRIGGER suppress_applied;")?;
        let (applied, _, status) =
            patches.apply_for_session(&journal, session, patch.patch_id, |_| Ok(()))?;
        assert!(applied);
        assert_eq!(status.as_deref(), Some("reconciled"));
        assert!(
            patches
                .apply_for_session(&journal, session, patch.patch_id, |_| Ok(()))?
                .0
        );
        assert_eq!(fs::read(root.join("renamed"))?, renamed);
        assert_eq!(journal.session_events(session)?.len(), 1);
        Ok(())
    }

    #[test]
    fn delivery_one_reconciles_verified_postimage_and_pauses_on_mixed_state() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let root = workspace.path();
        let store = PatchStore::new(root)?;
        fs::write(root.join("first"), "before\n")?;
        fs::write(root.join("second"), "before\n")?;
        let diff = "diff --git a/first b/first\n--- a/first\n+++ b/first\n@@ -1 +1 @@\n-before\n+after\n\
                    diff --git a/second b/second\n--- a/second\n+++ b/second\n@@ -1 +1 @@\n-before\n+after\n";
        let staged = store.stage(diff, &[])?;
        assert!(store.apply(root, staged.patch_id)?.0);
        let first = fs::read(root.join("first"))?;
        assert!(matches!(first.as_slice(), b"after\n" | b"after\r\n"));
        let mut interrupted = store.read_patch(staged.patch_id)?;
        interrupted.applied = false;
        interrupted.recovery_status = Some("pending".into());
        store.write_patch(&interrupted)?;
        assert!(store.apply(root, staged.patch_id)?.0);
        assert_eq!(
            store
                .read_patch(staged.patch_id)?
                .recovery_status
                .as_deref(),
            Some("reconciled")
        );
        store.write_patch(&interrupted)?;
        fs::write(root.join("second"), "before\n")?;
        assert!(!store.apply(root, staged.patch_id)?.0);
        assert_eq!(
            store
                .read_patch(staged.patch_id)?
                .recovery_status
                .as_deref(),
            Some("ambiguous")
        );
        assert_eq!(fs::read(root.join("first"))?, first);
        Ok(())
    }

    #[test]
    fn stage_tracks_target_files_and_base_hash() {
        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-diff-test-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(workspace.join("demo.txt"), "before\n").expect("seed");

        let store = PatchStore::new(&workspace).expect("store");
        let diff = "diff --git a/demo.txt b/demo.txt\n--- a/demo.txt\n+++ b/demo.txt\n@@ -1 +1 @@\n-before\n+after\n";
        let patch = store.stage(diff, &[]).expect("stage");

        assert_eq!(patch.target_files, vec!["demo.txt".to_string()]);
        assert!(!patch.base_sha256.is_empty());
    }

    #[test]
    fn apply_records_base_mismatch_metadata() {
        if !git_available() {
            return;
        }

        let workspace =
            std::env::temp_dir().join(format!("codingbuddy-diff-test-{}", Uuid::now_v7()));
        fs::create_dir_all(&workspace).expect("workspace");
        git(&workspace, &["init"]).expect("git init");
        git(
            &workspace,
            &["config", "user.email", "deepseek@example.test"],
        )
        .expect("git config email");
        git(&workspace, &["config", "user.name", "CodingBuddy"]).expect("git config name");
        fs::write(workspace.join("demo.txt"), "before\n").expect("seed");
        git(&workspace, &["add", "."]).expect("git add");
        git(&workspace, &["commit", "-m", "init"]).expect("git commit");

        fs::write(workspace.join("demo.txt"), "after\n").expect("edit");
        let diff = git(&workspace, &["diff"]).expect("git diff");
        git(&workspace, &["checkout", "--", "demo.txt"]).expect("git checkout");

        let store = PatchStore::new(&workspace).expect("store");
        let patch = store.stage(&diff, &[]).expect("stage");
        fs::write(workspace.join("demo.txt"), "drifted\n").expect("drift");
        let _ = store.apply(&workspace, patch.patch_id).expect("apply");

        let refreshed = store
            .list()
            .expect("list")
            .into_iter()
            .find(|p| p.patch_id == patch.patch_id)
            .expect("patched record");
        assert_eq!(refreshed.last_base_sha_match, Some(false));
        assert!(refreshed.last_base_sha256.is_some());
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git(workspace: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(workspace)
            .output()
            .context("git command failed")?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    #[test]
    fn checked_patch_paths_include_rename_source_and_destination() {
        let workspace = std::env::temp_dir();
        let diff = "diff --git a/old.txt b/new.txt\nsimilarity index 100%\nrename from old.txt\nrename to new.txt\n";
        assert_eq!(
            checked_patch_paths(&workspace, diff).unwrap(),
            ["new.txt", "old.txt"]
        );
    }

    // ── sha256_hex tests ──

    #[test]
    fn sha256_hex_deterministic() {
        let h1 = sha256_hex(b"hello");
        let h2 = sha256_hex(b"hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 = 64 hex chars
    }

    #[test]
    fn sha256_hex_different_inputs() {
        assert_ne!(sha256_hex(b"a"), sha256_hex(b"b"));
    }

    // ── hash_workspace_state tests ──

    #[test]
    fn hash_workspace_state_changes_when_file_changes() {
        let ws = std::env::temp_dir().join(format!("cb-diff-hash-{}", Uuid::now_v7()));
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("f.txt"), "v1").unwrap();

        let h1 = hash_workspace_state(&ws, &["f.txt".to_string()]).unwrap();
        fs::write(ws.join("f.txt"), "v2").unwrap();
        let h2 = hash_workspace_state(&ws, &["f.txt".to_string()]).unwrap();

        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_workspace_state_handles_missing_files() {
        let ws = std::env::temp_dir().join(format!("cb-diff-missing-{}", Uuid::now_v7()));
        fs::create_dir_all(&ws).unwrap();

        // Should not panic on missing file — uses [0] sentinel
        let h = hash_workspace_state(&ws, &["nonexistent.rs".to_string()]).unwrap();
        assert!(!h.is_empty());
    }

    // ── PatchStore CRUD tests ──

    #[test]
    fn patch_store_list_returns_empty_initially() {
        let ws = std::env::temp_dir().join(format!("cb-diff-list-{}", Uuid::now_v7()));
        fs::create_dir_all(&ws).unwrap();
        let store = PatchStore::new(&ws).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn patch_store_stage_and_list_roundtrip() {
        let ws = std::env::temp_dir().join(format!("cb-diff-rt-{}", Uuid::now_v7()));
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("x.txt"), "orig").unwrap();

        let store = PatchStore::new(&ws).unwrap();
        let diff = "--- a/x.txt\n+++ b/x.txt\n@@ -1 +1 @@\n-orig\n+new\n";
        let patch = store.stage(diff, &[]).unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].patch_id, patch.patch_id);
        assert_eq!(listed[0].target_files, vec!["x.txt"]);
        assert!(!listed[0].applied);
    }

    #[test]
    fn patch_store_read_unknown_id_errors() {
        let ws = std::env::temp_dir().join(format!("cb-diff-unknown-{}", Uuid::now_v7()));
        fs::create_dir_all(&ws).unwrap();
        let store = PatchStore::new(&ws).unwrap();
        assert!(store.read_patch(Uuid::nil()).is_err());
    }

    #[test]
    fn patch_store_stage_with_base_blob() {
        let ws = std::env::temp_dir().join(format!("cb-diff-blob-{}", Uuid::now_v7()));
        fs::create_dir_all(&ws).unwrap();

        let store = PatchStore::new(&ws).unwrap();
        let diff = "--- a/x.txt\n+++ b/x.txt\n@@ -1 +1 @@\n-a\n+b\n";
        let patch = store.stage(diff, b"explicit base content").unwrap();

        // base_sha256 should be the hash of the explicit blob, not workspace files
        assert_eq!(patch.base_sha256, sha256_hex(b"explicit base content"));
    }
}
