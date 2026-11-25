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
        let blobs = self.blob_fs()?;
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
        output.sync_all()?;
        drop(output);
        let hash = format!("{:x}", digest.finalize());
        blobs.rename_file(&temporary, Path::new(&hash))?;
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(input.metadata()?.permissions().mode() & 0o777)
        };
        #[cfg(not(unix))]
        let mode = None;
        Ok(FileState {
            hash: Some(hash),
            mode,
        })
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
            let mut selected = Vec::new();
            for entry in WalkDir::new(files.root())
                .into_iter()
                .filter_entry(|entry| {
                    entry
                        .path()
                        .strip_prefix(files.root())
                        .is_ok_and(|path| !has_ignored_component(path))
                })
            {
                let entry = entry?;
                if entry.file_type().is_symlink() {
                    skipped.push(entry.path().strip_prefix(files.root())?.to_owned());
                    continue;
                }
                if entry.file_type().is_file() {
                    selected.push(entry.path().strip_prefix(files.root())?.to_owned());
                }
            }
            selected
        } else {
            paths
                .iter()
                .map(|path| files.relative(path))
                .collect::<std::io::Result<_>>()?
        };
        let mut captured = BTreeMap::new();
        for path in selected {
            anyhow::ensure!(!has_ignored_component(&path), "Protected checkpoint path");
            captured.insert(path.clone(), self.capture_state(&files, &path)?);
        }
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
