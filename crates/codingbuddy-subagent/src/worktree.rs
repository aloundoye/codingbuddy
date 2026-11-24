//! Immutable input snapshots. The parent index and working files are never modified.
use anyhow::{Context, Result, ensure};
use codingbuddy_core::{execution::OwnedChild, process_env, workspace_fs::WorkspaceFs};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
struct Ownership {
    version: u32,
    id: Uuid,
    workspace: PathBuf,
    path: PathBuf,
    base_tree: String,
    label: String,
}

pub struct WorktreeIsolation {
    ownership: Ownership,
    manifest: PathBuf,
    created: bool,
}
impl WorktreeIsolation {
    pub fn create(workspace: &Path, name: &str) -> Result<Self> {
        Self::create_with_inputs(workspace, name, &[])
    }
    pub fn create_with_inputs(workspace: &Path, name: &str, inputs: &[PathBuf]) -> Result<Self> {
        Self::create_with_trust(
            workspace,
            name,
            inputs,
            codingbuddy_core::workspace_is_trusted(workspace),
        )
    }
    pub fn create_with_trust(
        workspace: &Path,
        name: &str,
        inputs: &[PathBuf],
        trusted: bool,
    ) -> Result<Self> {
        ensure!(trusted, "Worktree execution requires workspace trust");
        let workspace = workspace.canonicalize()?;
        let id = Uuid::now_v7();
        let root = workspace.join(".codingbuddy/worktrees");
        WorkspaceFs::open(&workspace)?.subdir(Path::new(".codingbuddy/worktrees"), true)?;
        // A UUID owns both artifacts. Never delete or reuse a caller-selected directory.
        let path = root.join(id.to_string());
        let manifest = root.join(format!("{id}.json"));
        let index = root.join(format!("{id}.index"));
        let result = (|| {
            let (base_tree, files) = snapshot(&workspace, &index, inputs, false)?;
            git(
                &workspace,
                None,
                &["worktree", "add", "--detach", "--no-checkout"],
                &[path.as_path()],
                None,
            )?;
            let ownership = Ownership {
                version: 1,
                id,
                workspace: workspace.clone(),
                path: path.clone(),
                base_tree,
                label: name.into(),
            };
            // Persist ownership before copying. Failures remain inspectable; no Drop cleanup.
            std::fs::write(&manifest, serde_json::to_vec_pretty(&ownership)?)?;
            let source = WorkspaceFs::open(&workspace)?;
            let target = WorkspaceFs::open(&path)?;
            for relative in files {
                let mut input = source.open_file(&relative)?.into_std();
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    Some(input.metadata()?.permissions().mode() & 0o777)
                };
                #[cfg(not(unix))]
                let mode = None;
                target.replace_from(&relative, &mut input, mode)?;
            }
            git(&path, None, &["read-tree", &ownership.base_tree], &[], None)?;
            let (copied, _) = snapshot(&path, &index, &[], true)?;
            ensure!(
                copied == ownership.base_tree,
                "Input changed during snapshot; worktree retained for inspection"
            );
            Ok(Self {
                ownership,
                manifest,
                created: true,
            })
        })();
        let _ = std::fs::remove_file(index);
        result
    }
    pub fn path(&self) -> &Path {
        &self.ownership.path
    }
    /// Includes new files, deletions, binary changes, and executable modes.
    pub fn collect_diff(&self) -> Result<String> {
        self.collect(false)
    }
    pub fn collect_for_parent(&self) -> Result<String> {
        self.collect(true)
    }
    fn collect(&self, check_parent: bool) -> Result<String> {
        self.verify_owner()?;
        let index = self.manifest.with_extension("output.index");
        let result = (|| {
            let (tree, _) = snapshot(self.path(), &index, &[], true)?;
            if check_parent {
                let changed = git(
                    self.path(),
                    None,
                    &[
                        "diff",
                        "--name-only",
                        "--no-renames",
                        "-z",
                        &self.ownership.base_tree,
                        &tree,
                    ],
                    &[],
                    None,
                )?;
                let parent = WorkspaceFs::open(&self.ownership.workspace)?;
                for raw in changed.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                    let path = path_from_bytes(raw)?;
                    let expected = git(
                        self.path(),
                        None,
                        &["ls-tree", "-z", &self.ownership.base_tree, "--"],
                        &[path.as_path()],
                        None,
                    )?;
                    let meta = expected.split(|b| *b == b'\t').next().unwrap_or_default();
                    let hash = std::str::from_utf8(meta)?.split_whitespace().nth(2);
                    let actual = match parent.read(&path) {
                        Ok(data) => Some(
                            String::from_utf8(git(
                                &self.ownership.workspace,
                                None,
                                &["hash-object", "--no-filters", "--stdin"],
                                &[],
                                Some(&data),
                            )?)?
                            .trim()
                            .to_owned(),
                        ),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(error.into()),
                    };
                    ensure!(
                        actual.as_deref() == hash,
                        "Parent changed at {}; worktree retained for review",
                        path.display()
                    );
                }
            }
            let bytes = git(
                self.path(),
                None,
                &[
                    "diff",
                    "--binary",
                    "--full-index",
                    "--no-ext-diff",
                    "--no-textconv",
                    &self.ownership.base_tree,
                    &tree,
                ],
                &[],
                None,
            )?;
            Ok(String::from_utf8(bytes)?)
        })();
        let _ = std::fs::remove_file(index);
        result
    }
    fn verify_owner(&self) -> Result<()> {
        let record: Ownership = serde_json::from_slice(&std::fs::read(&self.manifest)?)?;
        ensure!(
            record.id == self.ownership.id
                && record.workspace == self.ownership.workspace
                && record.path == self.ownership.path,
            "Worktree ownership changed"
        );
        ensure!(
            self.path().canonicalize()? == self.ownership.path,
            "Worktree path changed"
        );
        Ok(())
    }
    /// Explicit cleanup only. Uncollected changes must first be reviewed or exported.
    pub fn cleanup(&mut self) -> Result<()> {
        if !self.created {
            return Ok(());
        }
        self.verify_owner()?;
        ensure!(
            self.collect_diff()?.is_empty(),
            "Worktree has uncollected changes; preserve or export them before cleanup"
        );
        git(
            &self.ownership.workspace,
            None,
            &["worktree", "remove", "--force"],
            &[self.path()],
            None,
        )?;
        std::fs::remove_file(&self.manifest)?;
        self.created = false;
        Ok(())
    }
}
fn git(
    workspace: &Path,
    index: Option<&Path>,
    args: &[&str],
    paths: &[&Path],
    input: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut command = process_env::git_inspection(workspace);
    command
        .args(args)
        .args(paths)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let mut child = OwnedChild::spawn(&mut command)?;
    let stdin = child.stdin.take();
    // Feed while collecting output, avoiding a full stdin/stdout pipe deadlock.
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || -> Result<()> {
            if let Some(mut stdin) = stdin {
                if let Some(input) = input {
                    stdin.write_all(input)?;
                }
            }
            Ok(())
        });
        let mut out = Vec::new();
        let mut err = Vec::new();
        let stdout = child.stdout.take().context("Missing git stdout")?;
        let stderr = child.stderr.take().context("Missing git stderr")?;
        let reader = scope.spawn(move || {
            use std::io::Read;
            let stderr = stderr;
            stderr.take(1_000_000).read_to_end(&mut err).map(|_| err)
        });
        {
            use std::io::Read;
            let mut stdout = stdout;
            stdout.read_to_end(&mut out)?;
        }
        let status = child.wait()?;
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("Git writer failed"))??;
        let err = reader
            .join()
            .map_err(|_| anyhow::anyhow!("Git reader failed"))??;
        ensure!(
            status.success(),
            "Git snapshot failed: {}",
            String::from_utf8_lossy(&err)
        );
        Ok(out)
    })
}
fn snapshot(
    workspace: &Path,
    index: &Path,
    inputs: &[PathBuf],
    include_new: bool,
) -> Result<(String, Vec<PathBuf>)> {
    let files = WorkspaceFs::open(workspace)?;
    let mut names = BTreeSet::new();
    let tracked = git(workspace, None, &["ls-files", "-z", "--cached"], &[], None)?;
    for path in tracked.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        names.insert(path_from_bytes(path)?);
    }
    if include_new {
        let untracked = git(
            workspace,
            None,
            &["ls-files", "-z", "--others", "--exclude-standard"],
            &[],
            None,
        )?;
        for path in untracked.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            names.insert(path_from_bytes(path)?);
        }
    }
    for input in inputs {
        let relative = files.relative(input)?;
        let mut command = process_env::git_inspection(workspace);
        command
            .args(["check-ignore", "--quiet", "--no-index", "--"])
            .arg(&relative)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut check = OwnedChild::spawn(&mut command)?;
        ensure!(
            check.wait()?.code() == Some(1),
            "Ignored or unverifiable inputs cannot be delegated"
        );
        names.insert(relative);
    }
    git(workspace, Some(index), &["read-tree", "--empty"], &[], None)?;
    let mut records = Vec::new();
    let mut selected = Vec::new();
    for path in names {
        // Never snapshot agent runtime files or Git administration.
        if path
            .components()
            .any(|c| c.as_os_str() == ".git" || c.as_os_str() == ".codingbuddy")
        {
            continue;
        }
        let data = match files.read(&path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let hash = String::from_utf8(git(
            workspace,
            None,
            &["hash-object", "-w", "--no-filters", "--stdin"],
            &[],
            Some(&data),
        )?)?;
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            files
                .open_file(&path)?
                .into_std()
                .metadata()?
                .permissions()
                .mode()
                & 0o111
                != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        records.extend_from_slice(if executable { b"100755 " } else { b"100644 " });
        records.extend_from_slice(hash.trim().as_bytes());
        records.push(b'\t');
        records.extend_from_slice(path_bytes(&path)?);
        records.push(0);
        selected.push(path);
    }
    git(
        workspace,
        Some(index),
        &["update-index", "-z", "--index-info"],
        &[],
        Some(&records),
    )?;
    let tree = String::from_utf8(git(workspace, Some(index), &["write-tree"], &[], None)?)?
        .trim()
        .to_owned();
    Ok((tree, selected))
}
#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}
#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(std::str::from_utf8(bytes)?))
}
#[cfg(unix)]
fn path_bytes(path: &Path) -> Result<&[u8]> {
    use std::os::unix::ffi::OsStrExt;
    Ok(path.as_os_str().as_bytes())
}
#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Result<&[u8]> {
    Ok(path
        .to_str()
        .context("Unsupported Git path encoding")?
        .as_bytes())
}

#[cfg(test)]
mod delivery_three {
    use super::*;
    fn repository() -> Result<tempfile::TempDir> {
        let dir = tempfile::tempdir()?;
        git(dir.path(), None, &["init", "-q"], &[], None)?;
        std::fs::write(dir.path().join("file"), "committed")?;
        std::fs::write(dir.path().join(".gitignore"), ".codingbuddy/\nignored\n")?;
        git(
            dir.path(),
            None,
            &["add", "--", "file", ".gitignore"],
            &[],
            None,
        )?;
        git(
            dir.path(),
            None,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-q",
                "-m",
                "base",
            ],
            &[],
            None,
        )?;
        Ok(dir)
    }
    #[test]
    fn captures_current_input_preserves_index_and_retains_outputs() -> Result<()> {
        let dir = repository()?;
        let ws = dir.path();
        std::fs::write(ws.join("file"), "staged")?;
        git(ws, None, &["add", "--", "file"], &[], None)?;
        std::fs::write(ws.join("file"), "unstaged")?;
        std::fs::write(ws.join("chosen"), [0, 255, 1])?;
        std::fs::write(ws.join("excluded"), "not delegated")?;
        std::fs::write(ws.join("ignored"), "not delegated")?;
        let index = std::fs::read(ws.join(".git/index"))?;
        let wt = WorktreeIsolation::create_with_trust(
            ws,
            "same-name",
            &[PathBuf::from("chosen")],
            true,
        )?;
        let other = WorktreeIsolation::create_with_trust(ws, "same-name", &[], true)?;
        ensure!(wt.path() != other.path(), "Caller labels must not collide");
        ensure!(
            std::fs::read_to_string(wt.path().join("file"))? == "unstaged",
            "Snapshot used HEAD instead of current files"
        );
        ensure!(
            std::fs::read(wt.path().join("chosen"))? == [0, 255, 1],
            "Binary input changed"
        );
        ensure!(
            !wt.path().join("excluded").exists() && !wt.path().join("ignored").exists(),
            "Unselected input leaked"
        );
        ensure!(
            std::fs::read(ws.join(".git/index"))? == index,
            "Parent index changed"
        );
        std::fs::write(wt.path().join("new-binary"), [255, 0, 1])?;
        std::fs::write(wt.path().join("file"), "worker change")?;
        let patch = wt.collect_for_parent()?;
        ensure!(
            patch.contains("GIT binary patch") && patch.contains("worker change"),
            "Incomplete output patch"
        );
        std::fs::write(ws.join("file"), "later parent edit")?;
        ensure!(
            wt.collect_for_parent().is_err(),
            "Stale parent preimage accepted"
        );
        let path = wt.path().to_owned();
        drop(wt);
        ensure!(
            path.join("new-binary").exists(),
            "Drop destroyed worker artifacts"
        );
        ensure!(
            std::fs::read(ws.join(".git/index"))? == index,
            "Collection changed parent index"
        );
        Ok(())
    }
    #[test]
    fn ignored_inputs_and_untrusted_creation_fail_closed() -> Result<()> {
        let dir = repository()?;
        std::fs::write(dir.path().join("ignored"), "no")?;
        ensure!(
            WorktreeIsolation::create_with_trust(dir.path(), "x", &[], false).is_err(),
            "Untrusted worktree started"
        );
        ensure!(
            WorktreeIsolation::create_with_trust(
                dir.path(),
                "x",
                &[PathBuf::from("ignored")],
                true
            )
            .is_err(),
            "Ignored input accepted"
        );
        Ok(())
    }
}
