#[cfg(test)]
use super::tests::io::{fault, pause, Stage};
use super::{
    context::{protected_ids, snapshot_blocks, Context},
    Action, ClmController, ClmError,
};
use crate::clm::{render_context_document, MAX_EDITABLE_BYTES};
use pa_types::clm::ClmSnapshot;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, io::ErrorKind, path::PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) fn hash(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}
pub(super) fn check_size(text: &str) -> Result<(), ClmError> {
    if text.len() > MAX_EDITABLE_BYTES {
        return Err(ClmError::Document(
            "working document exceeds the 32 MiB cap".into(),
        ));
    }
    Ok(())
}
impl ClmController {
    fn marker_path(&self) -> Result<Option<PathBuf>, ClmError> {
        let Some(file) = &self.session_file else {
            return Ok(None);
        };
        match std::fs::canonicalize(file) {
            Ok(path) => {
                let mut name = path.into_os_string();
                name.push(".clm-writer");
                Ok(Some(name.into()))
            }
            Err(error)
                if error.kind() == ErrorKind::NotFound
                    && self.lock()?.accepted.epoch_id.is_none() =>
            {
                Ok(None)
            }
            Err(error) => Err(ClmError::Writer(format!(
                "session identity is unavailable: {error}"
            ))),
        }
    }
    pub(super) fn assert_no_marker(&self) -> Result<(), ClmError> {
        if let Some(path) = self.marker_path()? {
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == ErrorKind::NotFound => (),
                Ok(_) => {
                    return Err(ClmError::Writer(
                        "writer settlement is uncertain; retain the marker".into(),
                    ))
                }
                Err(error) => return Err(ClmError::Writer(error.to_string())),
            }
        }
        Ok(())
    }
    pub(super) fn arm_marker(&self) -> Result<(), ClmError> {
        let result = (|| {
            let path = self.marker_path()?.ok_or_else(|| {
                ClmError::Writer("writer safety requires a persistent session".into())
            })?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            crate::platform::perms::set_private_mode(&mut options);
            let file = options
                .open(&path)
                .map_err(|e| ClmError::Persistence(e.to_string()))?;
            #[cfg(test)]
            fault(&self.path, Stage::MarkerFileSync)
                .map_err(|e| ClmError::Persistence(e.to_string()))?;
            file.sync_all()
                .map_err(|e| ClmError::Persistence(e.to_string()))?;
            #[cfg(test)]
            fault(&self.path, Stage::MarkerDirectorySync)
                .map_err(|e| ClmError::Persistence(e.to_string()))?;
            std::fs::File::open(path.parent().expect("canonical file parent"))
                .and_then(|d| d.sync_all())
                .map_err(|e| ClmError::Persistence(e.to_string()))?;
            self.lock()?.marker = Some(path);
            Ok(())
        })();
        if let Err(error) = &result {
            self.poison(format!(
                "writer marker creation or fsync is uncertain: {error}"
            ));
        }
        result
    }
    pub(super) fn clear_marker(&self) -> Result<(), ClmError> {
        let path = self
            .lock()?
            .marker
            .clone()
            .ok_or_else(|| ClmError::Writer("no owned writer marker".into()))?;
        let result = std::fs::remove_file(&path).and_then(|()| {
            #[cfg(test)]
            fault(&self.path, Stage::MarkerRemovalSync)?;
            std::fs::File::open(path.parent().expect("marker parent"))?.sync_all()
        });
        if let Err(error) = result {
            // Re-arm after a visible unlink whose directory sync did not complete.
            let _ = self.arm_marker();
            self.poison(format!("writer marker removal is uncertain: {error}"));
            return Err(ClmError::Persistence(format!(
                "writer marker removal is uncertain: {error}"
            )));
        }
        self.lock()?.marker = None;
        Ok(())
    }
    pub(super) async fn read_mirror(&self) -> Result<String, ClmError> {
        let file = tokio::fs::File::open(&self.path)
            .await
            .map_err(|e| ClmError::Document(e.to_string()))?;
        #[cfg(test)]
        pause(&self.path, Stage::Read).await;
        let mut bytes = Vec::new();
        file.take((MAX_EDITABLE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| ClmError::Document(e.to_string()))?;
        if bytes.len() > MAX_EDITABLE_BYTES {
            return Err(ClmError::Document(
                "working document exceeds the 32 MiB cap".into(),
            ));
        }
        String::from_utf8(bytes).map_err(|e| ClmError::Document(e.to_string()))
    }
    pub(super) async fn check_drift(&self, context: &Context) -> Result<(), ClmError> {
        if self.lock()?.published_hash.is_none() {
            let ids = snapshot_blocks(&context.snapshot)?
                .into_iter()
                .map(|b| b.id)
                .collect();
            return self.publish(&context.snapshot, &ids).await;
        }
        let result = self.read_mirror().await.and_then(|text| {
            if Some(hash(&text)) != self.lock()?.published_hash {
                return Err(ClmError::Writer("pre-action file drift".into()));
            }
            Ok(())
        });
        if let Err(error) = &result {
            self.poison(format!("working file drift or unreadable mirror: {error}"));
        }
        result
    }
    pub(super) async fn publish(
        &self,
        snapshot: &ClmSnapshot,
        ids: &HashSet<String>,
    ) -> Result<(), ClmError> {
        #[cfg(test)]
        pause(&self.path, Stage::Publish).await;
        let text = render_context_document(snapshot, ids)?;
        check_size(&text)?;
        let parent = self.path.parent().expect("working mirror parent");
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ClmError::Persistence(e.to_string()))?;
        let temporary = parent.join(format!("context.txt.{}.tmp", uuid::Uuid::new_v4()));
        let result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .await?;
            file.write_all(text.as_bytes()).await?;
            #[cfg(test)]
            fault(&self.path, Stage::MirrorFileSync)?;
            file.sync_all().await?;
            drop(file);
            #[cfg(test)]
            fault(&self.path, Stage::MirrorRename)?;
            tokio::fs::rename(&temporary, &self.path).await?;
            #[cfg(test)]
            fault(&self.path, Stage::MirrorDirectorySync)?;
            tokio::fs::File::open(parent).await?.sync_all().await?;
            Ok::<(), std::io::Error>(())
        }
        .await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(ClmError::Persistence(format!(
                "mirror publication failed: {error}"
            )));
        }
        self.lock()?.published_hash = Some(hash(&text));
        Ok(())
    }
    pub(super) async fn publish_accepted(
        &self,
        snapshot: &ClmSnapshot,
        ids: &HashSet<String>,
    ) -> Result<(), ClmError> {
        if self.publish(snapshot, ids).await.is_err() {
            if let Err(error) = self.publish(snapshot, ids).await {
                let revision = self.lock()?.accepted.revision;
                self.poison(format!(
                    "revision {revision:?} was accepted, but its mirror requires repair: {error}"
                ));
                return Err(error);
            }
        }
        Ok(())
    }
    pub(super) async fn repair_rejected(&self, action: &Action) -> Result<(), ClmError> {
        self.check_action(action)?;
        let current = self.read().await?;
        self.check_fence(&action.expected, &current, true)?;
        self.check_action(action)?;
        let rejected = self
            .path
            .with_file_name(format!("context.txt.rejected-{}", uuid::Uuid::new_v4()));
        tokio::fs::rename(&self.path, rejected)
            .await
            .map_err(|e| ClmError::Persistence(e.to_string()))?;
        self.check_action(action)?;
        let context = super::context::accepted_context(&current, true)?;
        self.publish(
            &context.snapshot,
            &protected_ids(&context.snapshot, &action.expected.protected)?,
        )
        .await?;
        self.check_action(action)?;
        let after = self.read().await?;
        self.check_fence(&action.expected, &after, true)?;
        self.check_action(action)?;
        self.clear_marker()?;
        self.lock()?.action = None;
        Ok(())
    }
}
