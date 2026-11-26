//! Versioned, scoped, recoverable restore plans. Unrelated edits are never deleted.
use super::*;
use anyhow::Context;
use codingbuddy_core::EventKind;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileState {
    pub hash: Option<String>,
    pub mode: Option<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointManifest {
    pub version: u32,
    pub created_at: String,
    pub files: BTreeMap<PathBuf, FileState>,
    #[serde(default)]
    pub skipped: Vec<PathBuf>,
    pub session_id: Option<Uuid>,
    pub cursor: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreEntry {
    pub path: PathBuf,
    pub expected: FileState,
    pub target: FileState,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestorePlan {
    pub version: u32,
    pub id: Uuid,
    pub checkpoint_id: Uuid,
    pub workspace: PathBuf,
    pub session_id: Option<Uuid>,
    pub cursor: Option<u64>,
    pub entries: Vec<RestoreEntry>,
    pub preserved: Vec<PathBuf>,
    pub conflicts: Vec<String>,
}

impl MemoryManager {
    pub(super) fn blob_fs(&self) -> Result<WorkspaceFs> {
        Ok(WorkspaceFs::open(&self.workspace)?
            .subdir(Path::new(".codingbuddy/checkpoint-blobs"), true)?)
    }
    pub(super) fn capture_state(&self, files: &WorkspaceFs, path: &Path) -> Result<FileState> {
        let mut input = match files.open_file(path) {
            Ok(file) => file.into_std(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FileState {
                    hash: None,
                    mode: None,
                });
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = input.metadata()?;
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(metadata.permissions().mode() & 0o777)
        };
        #[cfg(not(unix))]
        let mode = None;
        let blobs = self.blob_fs()?;
        let cache_key = (files.root().to_owned(), path.to_owned());
        let stamp = stat_cache::Stamp::of(&metadata);
        if let Some(hash) = stat_cache::lookup(&cache_key, stamp)
            && blobs.resolve(Path::new(&hash), false).is_ok()
        {
            return Ok(FileState {
                hash: Some(hash),
                mode,
            });
        }
        let temporary = PathBuf::from(Uuid::now_v7().to_string());
        let mut output = blobs.open_lock_file(&temporary)?.into_std();
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let size = input.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            digest.update(&buffer[..size]);
            output.write_all(&buffer[..size])?;
        }
        let hash = format!("{:x}", digest.finalize());
        if blobs.resolve(Path::new(&hash), false).is_ok() {
            // Blobs are content-addressed: an existing one already holds
            // these bytes, durably. Skip the fsync and the rename.
            drop(output);
            blobs.remove_file(&temporary)?;
        } else {
            flush_to_device(&output)?;
            drop(output);
            blobs.rename_file(&temporary, Path::new(&hash))?;
        }
        stat_cache::remember(cache_key, stamp, &hash);
        Ok(FileState {
            hash: Some(hash),
            mode,
        })
    }

    /// The content hash of `path` without storing a blob, or `None` when it
    /// does not exist. Uses the same stat cache as [`Self::capture_state`].
    fn current_hash(&self, files: &WorkspaceFs, path: &Path) -> Result<Option<String>> {
        let mut input = match files.open_file(path) {
            Ok(file) => file.into_std(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let cache_key = (files.root().to_owned(), path.to_owned());
        let stamp = stat_cache::Stamp::of(&input.metadata()?);
        if let Some(hash) = stat_cache::lookup(&cache_key, stamp) {
            return Ok(Some(hash));
        }
        let mut digest = Sha256::new();
        std::io::copy(&mut input, &mut digest)?;
        let hash = format!("{:x}", digest.finalize());
        stat_cache::remember(cache_key, stamp, &hash);
        Ok(Some(hash))
    }

    pub fn create_checkpoint_linked(
        &self,
        reason: &str,
        paths: &[PathBuf],
        session: Option<Uuid>,
    ) -> Result<CheckpointRecord> {
        let files = WorkspaceFs::open(&self.workspace)?;
        let checkpoint_id = Uuid::now_v7();
        let root = runtime_dir(files.root())
            .join("checkpoints")
            .join(checkpoint_id.to_string());
        files.subdir(&files.relative(&root.join("fs"))?, true)?;
        let mut skipped = Vec::new();
        let selected: Vec<PathBuf> = if paths.is_empty() {
            let (selected, symlinks) = checkpoint_files(&files)?;
            skipped.extend(symlinks);
            selected
        } else {
            paths
                .iter()
                .map(|path| files.relative(path))
                .collect::<std::io::Result<_>>()?
        };
        for path in &selected {
            anyhow::ensure!(!has_ignored_component(path), "Protected checkpoint path");
        }
        // Capturing is dominated by per-blob fsyncs on first sight of a
        // file; a few workers overlap them.
        let workers = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .clamp(1, 8);
        let chunk = selected.len().div_ceil(workers).max(1);
        let captured: BTreeMap<PathBuf, FileState> = std::thread::scope(|scope| {
            let handles: Vec<_> = selected
                .chunks(chunk)
                .map(|paths| {
                    let files = &files;
                    scope.spawn(move || {
                        paths
                            .iter()
                            .map(|path| Ok((path.clone(), self.capture_state(files, path)?)))
                            .collect::<Result<Vec<_>>>()
                    })
                })
                .collect();
            let mut captured = BTreeMap::new();
            for handle in handles {
                let part = handle
                    .join()
                    .map_err(|_| anyhow!("checkpoint worker panicked"))??;
                captured.extend(part);
            }
            Ok::<_, anyhow::Error>(captured)
        })?;
        let cursor = session
            .map(|id| self.store.checkpoint_cursor(id))
            .transpose()?;
        let manifest = CheckpointManifest {
            version: 2,
            created_at: Utc::now().to_rfc3339(),
            files: captured,
            skipped,
            session_id: session,
            cursor,
        };
        files.replace_from(
            &root.join("manifest.json"),
            &mut serde_json::to_vec_pretty(&manifest)?.as_slice(),
            None,
        )?;
        let record = CheckpointRecord {
            checkpoint_id,
            reason: reason.into(),
            snapshot_path: root.join("fs").to_string_lossy().into_owned(),
            files_count: manifest.files.len() as u64,
            created_at: manifest.created_at,
        };
        self.store.insert_checkpoint(&record)?;
        Ok(record)
    }
    /// Workspace files whose content or presence differs from checkpoint
    /// `checkpoint_id`: modified, deleted, and newly created files. Used to
    /// attribute a shell command's changes to the agent so `/rewind` can
    /// undo them (restore only reverts changes it can attribute).
    pub fn changed_since_checkpoint(&self, checkpoint_id: Uuid) -> Result<Vec<PathBuf>> {
        let files = WorkspaceFs::open(&self.workspace)?;
        let private = files.subdir(Path::new(".codingbuddy"), false)?;
        let manifest: CheckpointManifest = serde_json::from_slice(
            &private.read(
                Path::new("checkpoints")
                    .join(checkpoint_id.to_string())
                    .join("manifest.json")
                    .as_path(),
            )?,
        )?;
        anyhow::ensure!(manifest.version == 2, "Unsupported checkpoint manifest");

        let mut current = BTreeMap::new();
        for path in checkpoint_files(&files)?.0 {
            if let Some(hash) = self.current_hash(&files, &path)? {
                current.insert(path, hash);
            }
        }

        let mut changed: Vec<PathBuf> = manifest
            .files
            .iter()
            .filter(|(path, before)| current.get(*path) != before.hash.as_ref())
            .map(|(path, _)| path.clone())
            .collect();
        changed.extend(
            current
                .keys()
                .filter(|path| !manifest.files.contains_key(*path))
                .cloned(),
        );
        changed.sort();
        changed.dedup();
        Ok(changed)
    }

    pub fn preview_restore(
        &self,
        checkpoint_id: Uuid,
        selected: &[PathBuf],
    ) -> Result<RestorePlan> {
        let files = WorkspaceFs::open(&self.workspace)?;
        let record = self
            .store
            .load_checkpoint(checkpoint_id)?
            .ok_or_else(|| anyhow!("Checkpoint unavailable"))?;
        let root = runtime_dir(files.root())
            .join("checkpoints")
            .join(checkpoint_id.to_string());
        let manifest_path = root.join("manifest.json");
        let mut plan = RestorePlan {
            version: 2,
            id: Uuid::now_v7(),
            checkpoint_id,
            workspace: files.root().to_owned(),
            session_id: None,
            cursor: None,
            entries: vec![],
            preserved: vec![],
            conflicts: vec![],
        };
        if !manifest_path.is_file() {
            plan.conflicts.push("Legacy checkpoint lacks verifiable restore provenance; inspect or export it manually".into());
            self.persist_restore_plan(&plan)?;
            return Ok(plan);
        }
        // Read through a capability directory; substituted snapshot roots cannot escape.
        let private = files.subdir(Path::new(".codingbuddy"), false)?;
        let manifest: CheckpointManifest = serde_json::from_slice(
            &private.read(
                Path::new("checkpoints")
                    .join(checkpoint_id.to_string())
                    .join("manifest.json")
                    .as_path(),
            )?,
        )?;
        anyhow::ensure!(manifest.version == 2, "Unsupported checkpoint manifest");
        plan.preserved.extend(manifest.skipped.clone());
        plan.session_id = manifest.session_id;
        plan.cursor = manifest.cursor;
        let known = self
            .store
            .scoped_postimages_since(&self.workspace, &record.created_at)?;
        let mut candidates = manifest.files.clone();
        for path in known.keys() {
            candidates.entry(path.clone()).or_insert(FileState {
                hash: None,
                mode: None,
            });
        }
        let selected = selected
            .iter()
            .map(|path| files.relative(path))
            .collect::<std::io::Result<Vec<_>>>()?;
        for (path, target) in candidates {
            if !selected.is_empty() && !selected.contains(&path) {
                continue;
            }
            anyhow::ensure!(!has_ignored_component(&path), "Protected restore path");
            let current = match self.capture_state(&files, &path) {
                Ok(state) => state,
                Err(_) => {
                    plan.conflicts
                        .push(format!("{}: unsafe or inaccessible path", path.display()));
                    continue;
                }
            };
            if current == target {
                continue;
            }
            match known.get(&path) {
                None => plan.preserved.push(path),
                Some((hash, mode)) if *hash != current.hash || *mode != current.mode => {
                    plan.conflicts.push(format!(
                        "{}: changed since agent settlement",
                        path.display()
                    ))
                }
                Some(_) => plan.entries.push(RestoreEntry {
                    path,
                    expected: current,
                    target,
                }),
            }
        }
        self.persist_restore_plan(&plan)?;
        Ok(plan)
    }
    fn persist_restore_plan(&self, plan: &RestorePlan) -> Result<()> {
        self.store.begin_operation(
            plan.id,
            plan.session_id,
            None,
            "restore",
            &serde_json::to_value(plan)?,
        )?;
        self.store.transition_operation(
            plan.id,
            "running",
            "prepared",
            &serde_json::to_value(plan)?,
        )
    }
    pub fn preview_step_restore(&self, id: Uuid) -> Result<RestorePlan> {
        let private = WorkspaceFs::open(&runtime_dir(&self.workspace))?;
        let snapshot: StepSnapshot = serde_json::from_slice(
            &private.read(&Path::new("snapshots").join(format!("{id}.json")))?,
        )?;
        anyhow::ensure!(
            snapshot.before.len() == snapshot.after.len(),
            "Incomplete step snapshot"
        );
        let files = WorkspaceFs::open(&self.workspace)?;
        let mut plan = RestorePlan {
            version: 2,
            id: Uuid::now_v7(),
            checkpoint_id: id,
            workspace: files.root().to_owned(),
            session_id: None,
            cursor: None,
            entries: vec![],
            preserved: vec![],
            conflicts: vec![],
        };
        for (before, after) in snapshot.before.iter().zip(&snapshot.after) {
            let path = files.relative(Path::new(&before.path))?;
            anyhow::ensure!(
                before.path == after.path && !has_ignored_component(&path),
                "Invalid step snapshot scope"
            );
            let current = self.capture_state(&files, &path)?;
            if current.hash.as_deref() != after.exists.then_some(after.content_hash.as_str())
                || current.hash.is_some() != after.exists
                || after.mode.is_some() && after.mode != current.mode
            {
                plan.conflicts
                    .push(format!("{}: changed since step", path.display()));
                continue;
            }
            let target = if before.exists && before.blob.is_some() {
                let state = FileState {
                    hash: before.blob.clone(),
                    mode: before.mode,
                };
                self.verify_blob(&state)?;
                state
            } else if before.exists {
                let content = before
                    .full_content
                    .as_ref()
                    .context("Legacy snapshot content unavailable")?;
                let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
                anyhow::ensure!(
                    before.content_hash == hash,
                    "Step snapshot content checksum mismatch"
                );
                let blobs = self.blob_fs()?;
                blobs.replace_from(Path::new(&hash), &mut content.as_bytes(), None)?;
                FileState {
                    hash: Some(hash),
                    mode: current.mode,
                }
            } else {
                FileState {
                    hash: None,
                    mode: None,
                }
            };
            plan.entries.push(RestoreEntry {
                path,
                expected: current,
                target,
            });
        }
        self.persist_restore_plan(&plan)?;
        Ok(plan)
    }
    pub(super) fn verify_blob(&self, state: &FileState) -> Result<()> {
        if let Some(hash) = &state.hash {
            anyhow::ensure!(
                hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "Invalid restore blob identity"
            );
            let root = self.blob_fs()?;
            let mut input = root.open_file(Path::new(hash))?;
            let mut digest = Sha256::new();
            let mut buffer = [0u8; 65536];
            loop {
                let size = input.read(&mut buffer)?;
                if size == 0 {
                    break;
                }
                digest.update(&buffer[..size]);
            }
            anyhow::ensure!(
                format!("{:x}", digest.finalize()) == *hash,
                "Restore blob checksum mismatch"
            );
        }
        Ok(())
    }
    fn write_state(&self, files: &WorkspaceFs, path: &Path, state: &FileState) -> Result<()> {
        self.verify_blob(state)?;
        if let Some(hash) = &state.hash {
            let blobs = self.blob_fs()?;
            files.replace_from(path, &mut blobs.open_file(Path::new(hash))?, state.mode)?;
        } else {
            match files.remove_file(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                other => other?,
            }
        }
        Ok(())
    }
    pub fn complete_restore(
        &self,
        id: Uuid,
        mode: &str,
        approved: bool,
    ) -> Result<(RestorePlan, Option<Uuid>)> {
        anyhow::ensure!(approved, "Restore requires explicit local approval");
        let operation = self.store.operation(id)?;
        anyhow::ensure!(
            operation.kind == "restore" && !operation.cancel_requested,
            "Restore unavailable or cancelled"
        );
        let plan: RestorePlan = serde_json::from_value(operation.payload.clone())?;
        if mode != "files" {
            anyhow::ensure!(
                plan.session_id.is_some() && plan.cursor.is_some(),
                "Checkpoint lacks a recorded conversation cursor"
            );
        }
        self.store.bind_restore_mode(id, mode)?;
        let state = self.reconcile_restore(id)?;
        anyhow::ensure!(
            matches!(state.as_str(), "prepared" | "applied"),
            "Restore needs local recovery"
        );
        if mode != "conversation" {
            if state == "prepared" {
                self.apply_restore(id, true)?;
            } else {
                anyhow::ensure!(state == "applied", "Restore needs local recovery");
                let files = WorkspaceFs::open(&self.workspace)?;
                for entry in &plan.entries {
                    anyhow::ensure!(
                        self.capture_state(&files, &entry.path)? == entry.target,
                        "Applied restore has later edits; inspect before finishing the conversation fork"
                    );
                }
            }
        }
        let fork = if mode != "files" {
            Some(
                self.store
                    .fork_for_restore(
                        id,
                        plan.session_id
                            .ok_or_else(|| anyhow!("restore plan has no session to fork"))?,
                        plan.cursor
                            .ok_or_else(|| anyhow!("restore plan has no conversation cursor"))?,
                    )?
                    .session_id,
            )
        } else {
            None
        };
        if mode == "conversation" && state == "prepared" {
            self.store.settle_operation_event(
                id,
                "prepared",
                "applied",
                &operation.payload,
                plan.session_id
                    .map(|session| {
                        codingbuddy_store::NewEvent::new(
                            session,
                            EventKind::CheckpointRewound {
                                checkpoint_id: plan.checkpoint_id,
                                reason: "conversation fork".into(),
                            },
                        )
                    })
                    .as_ref(),
            )?;
        }
        Ok((plan, fork))
    }
    pub fn apply_restore(&self, id: Uuid, approved: bool) -> Result<RestorePlan> {
        let operation = self.store.operation(id)?;
        anyhow::ensure!(operation.kind == "restore", "Not a restore operation");
        anyhow::ensure!(!operation.cancel_requested, "Restore cancelled");
        let plan: RestorePlan = serde_json::from_value(operation.payload)?;
        anyhow::ensure!(
            plan.version == 2 && plan.id == id,
            "Invalid restore identity"
        );
        anyhow::ensure!(approved, "Restore requires explicit local approval");
        anyhow::ensure!(
            plan.conflicts.is_empty(),
            "Restore has conflicts; select an uncontested subset"
        );
        let files = WorkspaceFs::open(&self.workspace)?;
        anyhow::ensure!(files.root() == plan.workspace, "Restore workspace changed");
        let _workspace_lease = self.store.session_lease(Uuid::nil())?;
        let _lease = plan
            .session_id
            .map(|session| self.store.session_lease(session))
            .transpose()?;
        let cfg = codingbuddy_core::AppConfig::load(files.root())?;
        let policy = codingbuddy_policy::PolicyEngine::from_app_config(&cfg.policy);
        for entry in &plan.entries {
            #[cfg(not(unix))]
            anyhow::ensure!(
                entry.target.mode.is_none(),
                "Unix file modes cannot be restored on this platform"
            );
            anyhow::ensure!(
                files.relative(&entry.path)? == entry.path && !has_ignored_component(&entry.path),
                "Invalid restore scope"
            );
            policy.check_patch_target(&files, &entry.path)?;
            let call = codingbuddy_core::ToolCall {
                name: "fs.write".into(),
                args: serde_json::json!({"path":files.root().join(&entry.path)}),
                requires_approval: false,
            };
            anyhow::ensure!(policy.denial(&call).is_none(), "Restore denied by policy");
            anyhow::ensure!(
                self.capture_state(&files, &entry.path)? == entry.expected,
                "Restore plan is stale"
            );
            self.verify_blob(&entry.target)?;
        }
        self.store.transition_operation(
            id,
            "prepared",
            "applying",
            &serde_json::to_value(&plan)?,
        )?;
        let mut applied: Vec<&RestoreEntry> = vec![];
        for entry in &plan.entries {
            let result = (|| {
                anyhow::ensure!(
                    !self.store.operation(id)?.cancel_requested,
                    "Restore cancelled"
                );
                anyhow::ensure!(
                    self.capture_state(&files, &entry.path)? == entry.expected,
                    "Restore path changed before write"
                );
                self.write_state(&files, &entry.path, &entry.target)?;
                applied.push(entry);
                anyhow::ensure!(
                    self.capture_state(&files, &entry.path)? == entry.target,
                    "Restore verification failed"
                );
                Ok::<_, anyhow::Error>(())
            })();
            if let Err(error) = result {
                let mut recovered = true;
                for previous in applied.into_iter().rev() {
                    if self
                        .capture_state(&files, &previous.path)
                        .is_ok_and(|state| state == previous.target)
                    {
                        recovered &= self
                            .write_state(&files, &previous.path, &previous.expected)
                            .is_ok();
                    } else {
                        recovered = false;
                    }
                }
                self.store.transition_operation(
                    id,
                    "applying",
                    if recovered {
                        "rolled_back"
                    } else {
                        "recovery_required"
                    },
                    &serde_json::to_value(&plan)?,
                )?;
                return Err(error);
            }
        }
        self.store.settle_operation_event(
            id,
            "applying",
            "applied",
            &serde_json::to_value(&plan)?,
            plan.session_id
                .map(|session| {
                    codingbuddy_store::NewEvent::new(
                        session,
                        EventKind::CheckpointRewound {
                            checkpoint_id: plan.checkpoint_id,
                            reason: format!("restore plan {id}"),
                        },
                    )
                })
                .as_ref(),
        )?;
        Ok(plan)
    }
    pub fn reconcile_restore(&self, id: Uuid) -> Result<String> {
        let operation = self.store.operation(id)?;
        let plan: RestorePlan = serde_json::from_value(operation.payload.clone())?;
        let files = WorkspaceFs::open(&self.workspace)?;
        anyhow::ensure!(plan.workspace == files.root(), "Restore workspace changed");
        if operation.state == "applying" {
            let complete = plan.entries.iter().all(|entry| {
                self.capture_state(&files, &entry.path)
                    .is_ok_and(|state| state == entry.target)
            });
            let state = if complete {
                "applied"
            } else {
                "recovery_required"
            };
            self.store.settle_operation_event(
                id,
                "applying",
                state,
                &operation.payload,
                if complete {
                    plan.session_id.map(|session| {
                        codingbuddy_store::NewEvent::new(
                            session,
                            EventKind::CheckpointRewound {
                                checkpoint_id: plan.checkpoint_id,
                                reason: format!("reconciled restore {id}"),
                            },
                        )
                    })
                } else {
                    None
                }
                .as_ref(),
            )?;
            return Ok(state.into());
        }
        Ok(operation.state)
    }
}

#[cfg(test)]
mod delivery_three {
    use super::*;
    use codingbuddy_core::EventKind;
    #[test]
    fn conversation_restore_reuses_fork_receipt_and_preserves_private_same_route_history()
    -> Result<()> {
        use codingbuddy_core::{NativeParts, Session, SessionBudgets, SessionState, TransportPart};
        let temp = tempfile::tempdir()?;
        let manager = MemoryManager::new(temp.path())?;
        let session = Uuid::now_v7();
        manager.store.save_session(&Session {
            session_id: session,
            workspace_root: temp.path().to_string_lossy().into_owned(),
            baseline_commit: None,
            status: SessionState::Idle,
            budgets: SessionBudgets {
                per_turn_seconds: 30,
                max_think_tokens: 100,
            },
            active_plan_id: None,
        })?;
        manager.store.pin_route(session, "same-route")?;
        manager.store.model_step(session,None,None,&[],&[TransportPart::Native(NativeParts {protocol:"anthropic-messages".into(),blocks:vec![serde_json::json!({"type":"thinking","signature":"private-fork-signature","thinking":"private"})]}),TransportPart::Text("checkpoint answer".into())])?;
        let checkpoint = manager.create_checkpoint_linked("fork", &[], Some(session))?;
        manager
            .store
            .append_event(&codingbuddy_store::NewEvent::new(
                session,
                EventKind::ChatTurn {
                    message: codingbuddy_core::ChatMessage::User {
                        content: "after checkpoint".into(),
                    },
                },
            ))?;
        let plan = manager.preview_restore(checkpoint.checkpoint_id, &[])?;
        let (_, fork) = manager.complete_restore(plan.id, "conversation", true)?;
        let (_, again) = manager.complete_restore(plan.id, "conversation", true)?;
        assert_eq!(fork, again);
        let fork = fork.unwrap();
        assert_eq!(
            manager.store.pinned_route(fork)?.as_deref(),
            Some("same-route")
        );
        let portable =
            serde_json::to_string(&manager.store.rebuild_from_events(fork)?.chat_messages)?;
        assert!(
            !portable.contains("private-fork-signature") && !portable.contains("after checkpoint")
        );
        let native = manager.store.provider_history(fork, "same-route")?;
        assert!(native.chat_messages.iter().any(|message|matches!(message,codingbuddy_core::ChatMessage::Assistant {native_parts:Some(parts),..} if parts.blocks.iter().any(|block|block["signature"]=="private-fork-signature"))));
        assert!(manager.store.provider_history(fork, "other-route").is_err());
        assert_eq!(manager.store.list_sessions()?.len(), 2);
        Ok(())
    }
    fn own(manager: &MemoryManager, path: &str) -> Result<()> {
        let hash = fs::read(manager.workspace.join(path))
            .ok()
            .map(|bytes| format!("{:x}", Sha256::digest(bytes)));
        manager
            .store
            .record_postimage(Path::new(path), hash.as_deref())
    }
    #[test]
    fn large_blob_rename_deletion_and_modes_restore_without_touching_user_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let ws = temp.path();
        let bytes = vec![0xfe; 1_100_000];
        fs::write(ws.join("original"), &bytes)?;
        fs::write(ws.join("deleted"), "restore deletion")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(ws.join("original"), fs::Permissions::from_mode(0o755))?;
            std::os::unix::fs::symlink("original", ws.join("preserved-link"))?;
        }
        let manager = MemoryManager::new(ws)?;
        let checkpoint = manager.create_checkpoint("large fixture")?;
        fs::rename(ws.join("original"), ws.join("renamed"))?;
        fs::remove_file(ws.join("deleted"))?;
        fs::write(ws.join("new-user"), "preserve")?;
        for path in ["original", "renamed", "deleted"] {
            own(&manager, path)?;
        }
        let plan = manager.preview_restore(checkpoint.checkpoint_id, &[])?;
        assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
        assert_eq!(plan.entries.len(), 3);
        #[cfg(unix)]
        assert!(plan.preserved.contains(&PathBuf::from("preserved-link")));
        manager.apply_restore(plan.id, true)?;
        assert_eq!(fs::read(ws.join("original"))?, bytes);
        assert_eq!(fs::read_to_string(ws.join("deleted"))?, "restore deletion");
        assert!(!ws.join("renamed").exists());
        assert_eq!(fs::read_to_string(ws.join("new-user"))?, "preserve");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(ws.join("original"))?.permissions().mode() & 0o777,
                0o755
            );
            assert!(fs::symlink_metadata(ws.join("preserved-link"))?.is_symlink());
        }
        Ok(())
    }
    #[test]
    fn binary_restore_preserves_unrelated_files_and_rejects_stale_plans() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let ws = temp.path();
        fs::write(ws.join("binary"), [0, 255, 1])?;
        fs::write(ws.join("user"), "before")?;
        let manager = MemoryManager::new(ws)?;
        let checkpoint = manager.create_checkpoint("before")?;
        fs::write(ws.join("binary"), [255, 0, 2])?;
        own(&manager, "binary")?;
        fs::write(ws.join("user"), "unrelated user edit")?;
        fs::write(ws.join("new-user"), "keep")?;
        let plan = manager.preview_restore(checkpoint.checkpoint_id, &[])?;
        assert_eq!(plan.entries.len(), 1);
        assert!(plan.preserved.contains(&PathBuf::from("user")));
        assert!(manager.apply_restore(plan.id, false).is_err());
        fs::write(ws.join("binary"), [3, 4, 5])?;
        assert!(manager.apply_restore(plan.id, true).is_err());
        assert_eq!(fs::read(ws.join("binary"))?, [3, 4, 5]);
        own(&manager, "binary")?;
        let plan = manager.preview_restore(checkpoint.checkpoint_id, &[])?;
        manager.apply_restore(plan.id, true)?;
        assert_eq!(fs::read(ws.join("binary"))?, [0, 255, 1]);
        assert_eq!(fs::read_to_string(ws.join("user"))?, "unrelated user edit");
        assert_eq!(fs::read_to_string(ws.join("new-user"))?, "keep");
        Ok(())
    }
    #[test]
    fn interrupted_restore_reconciles_only_verified_postimages() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let ws = temp.path();
        fs::write(ws.join("file"), "before")?;
        let manager = MemoryManager::new(ws)?;
        let checkpoint = manager.create_checkpoint("before")?;
        fs::write(ws.join("file"), "after")?;
        own(&manager, "file")?;
        let plan = manager.preview_restore(checkpoint.checkpoint_id, &[])?;
        manager.store.transition_operation(
            plan.id,
            "prepared",
            "applying",
            &serde_json::to_value(&plan)?,
        )?;
        let files = WorkspaceFs::open(ws)?;
        manager.write_state(&files, Path::new("file"), &plan.entries[0].target)?;
        assert_eq!(manager.reconcile_restore(plan.id)?, "applied");
        assert_eq!(manager.reconcile_restore(plan.id)?, "applied");
        Ok(())
    }
    #[test]
    fn binary_step_restore_uses_blob_journal() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let manager = MemoryManager::new(temp.path())?;
        fs::write(temp.path().join("file"), [0, 255, 1])?;
        let before = manager.capture_before(&["file"]);
        assert!(before[0].exists);
        assert!(before[0].blob.is_some());
        fs::write(temp.path().join("file"), [1, 0, 255])?;
        let step = manager.record_step_snapshot("fs.write", "call", before, &["file"])?;
        manager.revert_to_snapshot(step.id)?;
        assert_eq!(fs::read(temp.path().join("file"))?, [0, 255, 1]);
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn runtime_symlink_cannot_redirect_checkpoint_blobs() -> Result<()> {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let manager = MemoryManager::new(temp.path())?;
        fs::write(temp.path().join("file"), "secret")?;
        symlink(
            outside.path(),
            temp.path().join(".codingbuddy/checkpoint-blobs"),
        )?;
        assert!(manager.create_checkpoint("reject").is_err());
        assert_eq!(fs::read_dir(outside.path())?.count(), 0);
        Ok(())
    }
}

/// Files a whole-workspace checkpoint covers, and the symlinks it skips.
///
/// Honours `.gitignore` (and `.git/info/exclude`, even outside a git
/// repository) so dependency and build folders such as `node_modules` are
/// not copied before every shell command. Hidden files are included; `.git`,
/// `.codingbuddy` and `target` never are.
fn checkpoint_files(files: &WorkspaceFs) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let root = files.root().to_owned();
    let filter_root = root.clone();
    let mut selected = Vec::new();
    let mut symlinks = Vec::new();
    let walker = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .parents(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .ignore(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            entry
                .path()
                .strip_prefix(&filter_root)
                .is_ok_and(|path| !has_ignored_component(path))
        })
        .build();
    for entry in walker {
        let entry = entry?;
        let Some(kind) = entry.file_type() else {
            continue;
        };
        let relative = entry.path().strip_prefix(&root)?.to_owned();
        if kind.is_symlink() {
            symlinks.push(relative);
        } else if kind.is_file() {
            selected.push(relative);
        }
    }
    selected.sort();
    Ok((selected, symlinks))
}

/// A process-wide cache of content hashes keyed by file metadata, so a
/// checkpoint only reads files that changed since the last one (the same
/// trick as git's index).
mod stat_cache {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock, PoisonError};
    use std::time::{Duration, SystemTime};

    /// A file is only cached once its timestamps are this old: a write in
    /// the same timestamp tick as the hash would otherwise go unnoticed.
    const SETTLE: Duration = Duration::from_secs(2);
    const MAX_ENTRIES: usize = 200_000;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) struct Stamp {
        len: u64,
        modified: Option<SystemTime>,
        changed: Option<SystemTime>,
        inode: u64,
    }

    impl Stamp {
        pub(super) fn of(metadata: &std::fs::Metadata) -> Self {
            #[cfg(unix)]
            let (changed, inode) = {
                use std::os::unix::fs::MetadataExt;
                let ctime = u64::try_from(metadata.ctime()).ok().map(|secs| {
                    SystemTime::UNIX_EPOCH
                        + Duration::new(secs, u32::try_from(metadata.ctime_nsec()).unwrap_or(0))
                });
                (ctime, metadata.ino())
            };
            #[cfg(not(unix))]
            let (changed, inode) = (None, 0);
            Self {
                len: metadata.len(),
                modified: metadata.modified().ok(),
                changed,
                inode,
            }
        }

        fn settled(&self) -> bool {
            let Some(cutoff) = SystemTime::now().checked_sub(SETTLE) else {
                return false;
            };
            self.modified.is_some_and(|at| at < cutoff) && self.changed.is_none_or(|at| at < cutoff)
        }
    }

    type Key = (PathBuf, PathBuf);

    fn cache() -> &'static Mutex<HashMap<Key, (Stamp, String)>> {
        static CACHE: OnceLock<Mutex<HashMap<Key, (Stamp, String)>>> = OnceLock::new();
        CACHE.get_or_init(Default::default)
    }

    pub(super) fn lookup(key: &Key, stamp: Stamp) -> Option<String> {
        let cache = cache().lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .get(key)
            .filter(|(cached, _)| *cached == stamp)
            .map(|(_, hash)| hash.clone())
    }

    pub(super) fn remember(key: Key, stamp: Stamp, hash: &str) {
        if !stamp.settled() {
            return;
        }
        let mut cache = cache().lock().unwrap_or_else(PoisonError::into_inner);
        if cache.len() >= MAX_ENTRIES {
            cache.clear();
        }
        cache.insert(key, (stamp, hash.to_string()));
    }
}

/// Push a blob's bytes to the storage device.
///
/// On macOS `File::sync_all` issues `F_FULLFSYNC`, which also empties the
/// drive's cache and serialises every caller, costing milliseconds per
/// file. Blobs use a plain `fsync` instead, and the checkpoint manifest,
/// written last with `sync_all`, flushes the drive cache once for the whole
/// batch (git's `core.fsyncMethod=batch`).
fn flush_to_device(file: &std::fs::File) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: `file` owns a valid open descriptor for this call.
        if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        file.sync_all()
    }
}
