//! CLM controls share the worker replacement fence and durable owner.
use crate::{
    protocol::{response_failure, response_success, DaemonResponse},
    worker::Worker,
};
use serde_json::Value;
impl Worker {
    pub(crate) async fn assert_clm_operation_allowed(&self, operation: &str) -> anyhow::Result<()> {
        let Some(engine) = &self.agent_engine else {
            return Ok(());
        };
        if !self
            .core
            .lock()
            .map_err(|_| anyhow::anyhow!("CLM owner lock poisoned"))?
            .created
        {
            return Ok(());
        }
        let model = engine.resolve_model()?;
        engine.ensure_core_session_async(&model).await?;
        let built = engine
            .session
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("CLM session unavailable"))?;
        if let Some(runtime) = &built.session.clm {
            runtime.assert_operation_allowed(operation)?;
        }
        Ok(())
    }
    pub(crate) async fn handle_clm(&self, kind: &str, payload: &Value) -> DaemonResponse {
        let result = self.clm_command(kind, payload).await;
        match result {
            Ok(data) => response_success(None, kind, Some(data)),
            Err(error) => response_failure(None, kind, &format!("{error:#}"), None),
        }
    }
    async fn clm_command(&self, kind: &str, payload: &Value) -> anyhow::Result<Value> {
        let _replacement = self.replacement_gate.lock().await;
        let _pause = self.hold_queued_work_pause("clm-control");
        let mutation = matches!(kind, "set_clm_mode" | "restore_clm_revision");
        {
            let core = self
                .core
                .lock()
                .map_err(|_| anyhow::anyhow!("CLM owner lock poisoned"))?;
            anyhow::ensure!(core.created, "CLM session not created");
            anyhow::ensure!(
                !mutation || !core.has_ongoing_work(),
                "CLM requires an idle session and empty queues"
            );
        }
        let engine = self
            .agent_engine
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("CLM requires the native agent engine"))?;
        let model = engine.resolve_model()?;
        engine.ensure_core_session_async(&model).await?;
        let built = engine
            .session
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("CLM session unavailable"))?;
        let clm = built
            .session
            .clm
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("CLM requires session artifacts"))?;
        match kind {
            "set_clm_mode" => Ok(serde_json::to_value(
                clm.set_mode(
                    payload["enabled"]
                        .as_bool()
                        .ok_or_else(|| anyhow::anyhow!("enabled must be boolean"))?,
                    pa_types::clm::ClmModeSource::SessionOverride,
                )
                .await?,
            )?),
            "restore_clm_revision" => Ok(serde_json::to_value(
                clm.restore(pa_types::clm::ClmRevisionRef {
                    epoch_id: payload["epochId"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("epochId required"))?
                        .into(),
                    revision: payload["revision"]
                        .as_u64()
                        .ok_or_else(|| anyhow::anyhow!("revision required"))?,
                })
                .await?,
            )?),
            "get_clm_status" => Ok(serde_json::to_value(
                clm.status(
                    pa_core::settings::SettingsManager::create(
                        engine.cwd(),
                        &self.config.agent_dir,
                    )
                    .get_clm_enabled(),
                )
                .await?,
            )?),
            "get_clm_model_snapshot" => Ok(serde_json::to_value(clm.view().await?)?),
            _ => anyhow::bail!("unknown CLM control"),
        }
    }
}
