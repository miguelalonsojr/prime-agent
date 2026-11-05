//! Session-owned CLM hooks. The journal callback, not loop emission, acknowledges rows.
#[cfg(test)]
mod tests;
use crate::clm::{
    controller::{ClmController, ClmRequestInputs, ClmToolOutcome},
    journal::{ClmJournalFn, ClmJournalOp, ClmJournalReply},
    ClmError,
};
use pa_types::clm::{ClmAcceptedView, ClmMode, ClmModeSource, ClmRevisionRef, ClmStatus};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

pub type ClmHelperReceiptFn = Arc<
    dyn Fn(
            crate::clm::journal::ClmJournalView,
            serde_json::Value,
        ) -> futures::future::BoxFuture<'static, anyhow::Result<String>>
        + Send
        + Sync,
>;
pub type ObservedResponseFn = Box<
    dyn FnOnce(
            pa_types::ai::AssistantMessage,
        ) -> futures::future::BoxFuture<'static, anyhow::Result<()>>
        + Send,
>;

struct ClmActionFence {
    tool_call_id: String,
    identity: crate::kernel::shared::ToolExecutionIdentity,
}

#[derive(Clone)]
pub struct ClmChildAdmission {
    runtime: Arc<ClmRuntime>,
    fence: Option<Arc<ClmActionFence>>,
    inherited_mode: bool,
}

impl std::fmt::Debug for ClmChildAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClmChildAdmission")
            .field("inherited_mode", &self.inherited_mode)
            .finish_non_exhaustive()
    }
}

impl ClmChildAdmission {
    #[must_use]
    pub fn inherited_mode(&self) -> bool {
        self.inherited_mode
    }

    /// # Errors
    /// Rejects invalidated origins, unresolved writers, or unavailable journal state.
    pub async fn recheck(&self) -> anyhow::Result<()> {
        let _action = self.runtime.action_gate.lock().await;
        self.runtime.check_child_origin(self.fence.as_ref()).await
    }
}

pub struct ClmRuntime {
    pub controller: Arc<ClmController>,
    journal: ClmJournalFn,
    enabled: AtomicBool,
    agent: std::sync::OnceLock<std::sync::Weak<pa_agent::agent::Agent>>,
    active_tool: Mutex<Option<Arc<ClmActionFence>>>,
    action_gate: tokio::sync::Mutex<()>,
    inherited_source: std::sync::OnceLock<pa_types::clm::ClmStatusSource>,
    helper_receipt: std::sync::OnceLock<ClmHelperReceiptFn>,
    failure: Mutex<Option<String>>,
    request: Mutex<Option<(pa_types::clm::ClmUsagePin, serde_json::Value)>>,
    cleanup_pending: AtomicBool,
    pub(super) standalone_protected: Arc<Mutex<crate::clm::ProtectedActions>>,
}

impl ClmRuntime {
    /// # Errors
    /// Rejects damaged journal state, retained writer markers, or unavailable artifacts.
    pub async fn new(
        journal: ClmJournalFn,
        file: Option<std::path::PathBuf>,
        artifacts: std::path::PathBuf,
        standalone_protected: Arc<Mutex<crate::clm::ProtectedActions>>,
    ) -> anyhow::Result<Arc<Self>> {
        let controller = Arc::new(ClmController::new(journal.clone(), file, artifacts).await?);
        let status = controller.status(false).await?;
        Ok(Arc::new(Self {
            controller,
            journal,
            enabled: AtomicBool::new(status.mode == ClmMode::On),
            agent: std::sync::OnceLock::new(),
            active_tool: Mutex::new(None),
            action_gate: tokio::sync::Mutex::new(()),
            inherited_source: std::sync::OnceLock::new(),
            helper_receipt: std::sync::OnceLock::new(),
            failure: Mutex::new(None),
            request: Mutex::new(None),
            cleanup_pending: AtomicBool::new(false),
            standalone_protected,
        }))
    }

    /// # Errors
    /// Rejects unowned active writers, stale origins, or failed session state.
    pub async fn admit_child(
        self: &Arc<Self>,
        verified_tool_call_id: Option<&str>,
        identity: Option<&crate::kernel::shared::ToolExecutionIdentity>,
    ) -> anyhow::Result<ClmChildAdmission> {
        let fence = self
            .active_tool
            .lock()
            .map_err(|_| ClmError::Writer("runtime lock poisoned".into()))?
            .as_ref()
            .filter(|fence| {
                verified_tool_call_id == Some(fence.tool_call_id.as_str())
                    && identity.is_some_and(|identity| identity.same_instance(&fence.identity))
            })
            .cloned();
        let _action = self.action_gate.lock().await;
        self.check_child_origin(fence.as_ref()).await?;
        Ok(ClmChildAdmission {
            runtime: self.clone(),
            fence,
            inherited_mode: self.enabled(),
        })
    }

    async fn check_child_origin(
        &self,
        captured: Option<&Arc<ClmActionFence>>,
    ) -> anyhow::Result<()> {
        self.check_failure()?;
        let origin = self
            .active_tool
            .lock()
            .map_err(|_| ClmError::Writer("runtime lock poisoned".into()))?
            .as_ref()
            .filter(|current| captured.is_some_and(|captured| Arc::ptr_eq(current, captured)))
            .map(|fence| fence.tool_call_id.clone());
        self.controller
            .assert_child_admission_available(origin.as_deref())
            .await?;
        self.check_failure()
    }

    pub(super) fn mark_inherited(&self) {
        let _ = self
            .inherited_source
            .set(pa_types::clm::ClmStatusSource::InheritedChild);
    }

    fn with_initial_source(&self, mut status: ClmStatus) -> ClmStatus {
        if status.mode_source == pa_types::clm::ClmStatusSource::Unknown {
            if let Some(source) = self.inherited_source.get() {
                status.mode_source = *source;
            }
        }
        status
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// # Errors
    /// Rejects CLM-on replacement operations or unresolved session failures.
    pub fn assert_operation_allowed(&self, operation: &str) -> anyhow::Result<()> {
        self.check_failure()?;
        Ok(self.controller.require_off(operation)?)
    }

    /// # Errors
    /// Reports pending provider cleanup or a retained runtime failure.
    pub fn check_failure(&self) -> anyhow::Result<()> {
        if self.cleanup_pending.load(Ordering::SeqCst) {
            return Err(ClmError::Writer("failed provider cleanup pending".into()).into());
        }
        if let Some(error) = self
            .failure
            .lock()
            .map_err(|_| ClmError::Writer("runtime lock poisoned".into()))?
            .as_ref()
        {
            return Err(ClmError::Writer(error.clone()).into());
        }
        Ok(())
    }

    pub fn fail(&self, error: &dyn std::fmt::Display) {
        if let Ok(mut failure) = self.failure.lock() {
            *failure = Some(error.to_string());
        }
    }

    /// # Errors
    /// Rejects unsettled writers, invalid accepted state, or persistence failures.
    pub async fn set_mode(
        &self,
        enabled: bool,
        source: ClmModeSource,
    ) -> anyhow::Result<ClmStatus> {
        let _action = self.action_gate.lock().await;
        self.check_failure()?;
        let status = self.controller.set_mode(enabled, source).await?;
        self.enabled
            .store(status.mode == ClmMode::On, Ordering::SeqCst);
        Ok(self.with_initial_source(status))
    }

    /// # Errors
    /// Reports invalid or unavailable acknowledged journal state.
    pub async fn status(&self, configured: bool) -> anyhow::Result<ClmStatus> {
        Ok(self.with_initial_source(self.controller.status(configured).await?))
    }
    /// # Errors
    /// Rejects unresolved writers or invalid accepted context.
    pub async fn view(&self) -> anyhow::Result<ClmAcceptedView> {
        Ok(self.controller.accepted_view().await?)
    }
    /// # Errors
    /// Rejects unresolved writers, invalid revision references, or persistence failures.
    pub async fn restore(&self, target: ClmRevisionRef) -> anyhow::Result<ClmStatus> {
        let _action = self.action_gate.lock().await;
        self.check_failure()?;
        Ok(self.controller.restore(target).await?)
    }

    async fn capture_request_pin(
        &self,
    ) -> anyhow::Result<(
        crate::clm::journal::ClmJournalView,
        pa_types::clm::ClmUsagePin,
    )> {
        self.check_failure()?;
        let ClmJournalReply::View(view) = (self.journal)(ClmJournalOp::ReadAfterEvents).await?
        else {
            anyhow::bail!("CLM read returned a commit")
        };
        let state = crate::clm::validate_clm_branch(&view.branch)?;
        if state.mode == ClmMode::On {
            self.controller.project_request().await?;
        }
        let pin = pa_types::clm::ClmUsagePin {
            mode: state.mode,
            branch_id: view.leaf_id.clone(),
            epoch_id: state.epoch_id,
            revision: state.revision,
            entry_id: view
                .branch
                .iter()
                .rev()
                .find(|entry| {
                    matches!(
                        entry,
                        pa_types::session::FileEntry::ClmOn { .. }
                            | pa_types::session::FileEntry::ClmRevision { .. }
                            | pa_types::session::FileEntry::ClmOffBoundary { .. }
                    )
                })
                .and_then(|entry| entry.id().map(str::to_owned)),
        };
        Ok((view, pin))
    }

    async fn begin_request(&self) -> anyhow::Result<()> {
        let (_, pin) = self.capture_request_pin().await?;
        *self
            .request
            .lock()
            .map_err(|_| ClmError::Writer("request lock poisoned".into()))? =
            Some((pin, serde_json::Value::Null));
        Ok(())
    }

    pub(super) fn set_helper_receipt(&self, sink: ClmHelperReceiptFn) {
        let _ = self.helper_receipt.set(sink);
    }

    /// # Errors
    /// Rejects invalid context, an oversized request, or a missing receipt owner.
    pub async fn on_observed_response(
        self: &Arc<Self>,
        model: &pa_types::ai::Model,
        context: &pa_types::ai::Context,
    ) -> anyhow::Result<ObservedResponseFn> {
        let (view, pin) = self.capture_request_pin().await?;
        if pin.mode == ClmMode::On {
            let messages = context
                .messages
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?;
            let estimate = crate::clm::estimate_clm_request_tokens(
                &messages,
                context.system_prompt.as_deref().unwrap_or_default(),
                context.tools.as_deref().unwrap_or_default(),
                model,
            )?;
            let exceeded = estimate
                .max_input_tokens
                .is_some_and(|max| estimate.estimated_input_tokens > max);
            self.controller.record_request_estimate(estimate);
            anyhow::ensure!(
                !exceeded,
                "CLM helper request exceeds the model input budget"
            );
        }
        let sink = self
            .helper_receipt
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("CLM helper receipt owner unavailable"))?;
        let runtime = self.clone();
        Ok(Box::new(move |assistant| {
            Box::pin(async move {
                let message: pa_agent::types::AssistantMessage =
                    serde_json::from_value(serde_json::to_value(&assistant)?)?;
                if message.stop_reason == pa_agent::types::StopReason::Aborted
                    || super::provider_retry::is_agent_lifecycle_failure(&message)
                    || super::provider_retry::is_faux_provider_queue_exhausted(&message)
                {
                    return Ok(());
                }
                let result = async {
                    let receipt = crate::clm::clm_usage_receipt(
                        &pin,
                        &serde_json::to_value(assistant.usage)?,
                        None,
                    );
                    let id = sink(view.clone(), receipt).await?;
                    let ClmJournalReply::View(ack) =
                        (runtime.journal)(ClmJournalOp::ReadAfterEvents).await?
                    else {
                        anyhow::bail!("CLM helper acknowledgment returned a commit")
                    };
                    anyhow::ensure!(
                        ack.session_id == view.session_id && ack.generation == view.generation,
                        "CLM helper session changed before acknowledgment"
                    );
                    anyhow::ensure!(
                        ack.branch
                            .iter()
                            .any(|entry| entry.id() == Some(id.as_str())),
                        "CLM helper receipt not acknowledged"
                    );
                    Ok(())
                }
                .await;
                if let Err(error) = &result {
                    runtime.fail(error);
                }
                result
            })
        }))
    }

    pub(super) fn observe_provider_usage(&self, usage: &pa_types::ai::Usage) {
        if let Ok(mut request) = self.request.lock() {
            if let Some((_, raw)) = request.as_mut() {
                *raw = serde_json::to_value(usage).unwrap_or(serde_json::Value::Null);
            }
        }
    }

    pub fn before_assistant_append(&self, message: &serde_json::Value) {
        if self.enabled() && message["stopReason"] == "error" {
            self.cleanup_pending.store(true, Ordering::SeqCst);
        }
    }

    /// # Errors
    /// Reports invalid canonical assistant data or a poisoned request lock.
    pub fn take_usage_receipt(
        &self,
        block: &pa_types::clm::ClmBlock,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let request = self
            .request
            .lock()
            .map_err(|_| ClmError::Writer("request lock poisoned".into()))?
            .take();
        let message: pa_agent::types::AssistantMessage =
            serde_json::from_value(block.message.clone())?;
        if message.stop_reason == pa_agent::types::StopReason::Aborted
            || super::provider_retry::is_agent_lifecycle_failure(&message)
            || super::provider_retry::is_faux_provider_queue_exhausted(&message)
        {
            return Ok(None);
        }
        Ok(
            request
                .map(|(pin, usage)| crate::clm::clm_usage_receipt(&pin, &usage, Some(&block.id))),
        )
    }

    /// # Errors
    /// Reports failed cleanup of the acknowledged provider error entry.
    pub async fn after_assistant_append(
        &self,
        block: &pa_types::clm::ClmBlock,
    ) -> anyhow::Result<()> {
        if self.enabled() && block.message["stopReason"] == "error" {
            if let Err(error) = self.controller.discard_failed_provider_entry(block).await {
                self.fail(&error);
                return Err(error.into());
            }
            self.cleanup_pending.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    pub(super) async fn persist_standalone_event(
        &self,
        owner: &Arc<tokio::sync::Mutex<crate::session::manager::SessionManager>>,
        event: pa_agent::types::AgentEvent,
    ) -> anyhow::Result<()> {
        if let pa_agent::types::AgentEvent::MessageEnd { message } = &event {
            let Some(message) = super::loop_message_to_session(message) else {
                return Ok(());
            };
            let raw = serde_json::to_value(&message)?;
            let assistant = matches!(message, pa_types::session::AgentMessage::Assistant(_));
            if assistant {
                self.before_assistant_append(&raw);
            }
            let result = async {
                let block = {
                    let mut owner = owner.lock().await;
                    let id = match message {
                        pa_types::session::AgentMessage::Custom(custom) => owner
                            .append_custom_message(
                                &custom.custom_type,
                                custom.content,
                                custom.display,
                                custom.details,
                            )?,
                        other => {
                            let (id, error) = owner.append_message_retained(other);
                            if let Some(error) = error {
                                return Err(error.into());
                            }
                            id
                        }
                    };
                    let entry = owner
                        .retained_entries()
                        .iter()
                        .find(|entry| entry.id() == Some(id.as_str()))
                        .ok_or_else(|| ClmError::Journal("acknowledged message missing".into()))?;
                    let message = match entry {
                        pa_types::session::FileEntry::CustomMessage { payload, .. } => {
                            serde_json::to_value(pa_types::session::AgentMessage::Custom(
                                crate::session::create_custom_message(payload, entry),
                            ))?
                        }
                        pa_types::session::FileEntry::Message { message, .. } => {
                            serde_json::to_value(message)?
                        }
                        _ => {
                            return Err(ClmError::Journal(
                                "acknowledged row is not a message".into(),
                            )
                            .into())
                        }
                    };
                    pa_types::clm::ClmBlock { id, message }
                };
                if matches!(block.message["role"].as_str(), Some("user" | "custom")) {
                    self.standalone_protected
                        .lock()
                        .map_err(|_| ClmError::Protection("delivery lock poisoned".into()))?
                        .insert(uuid::Uuid::new_v4().to_string(), vec![block.clone()]);
                }
                if assistant {
                    if let Some(receipt) = self.take_usage_receipt(&block)? {
                        owner.lock().await.append_custom_entry(
                            receipt["customType"]
                                .as_str()
                                .ok_or_else(|| ClmError::Journal("invalid usage receipt".into()))?,
                            Some(receipt["data"].clone()),
                        )?;
                    }
                    self.after_assistant_append(&block).await?;
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if let Err(error) = &result {
                self.fail(error);
            }
            return result;
        }
        if let pa_agent::types::AgentEvent::AgentEnd { messages } = &event {
            let retrying = messages.last().is_some_and(|message| matches!(message, pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(a)) if a.stop_reason == pa_agent::types::StopReason::Error));
            if !retrying {
                self.standalone_protected
                    .lock()
                    .map_err(|_| ClmError::Protection("delivery lock poisoned".into()))?
                    .clear();
            }
        }
        Ok(super::persist_event(owner, event).await?)
    }

    pub fn transform(self: &Arc<Self>) -> pa_agent::agent_loop::TransformContextFn {
        let runtime = self.clone();
        Arc::new(move |messages, _signal| {
            let runtime = runtime.clone();
            Box::pin(async move {
                runtime.check_failure()?;
                let reply = (runtime.journal)(ClmJournalOp::ReadAfterEvents).await?;
                let ClmJournalReply::View(view) = reply else {
                    anyhow::bail!("CLM read returned a commit")
                };
                let projection = if runtime.enabled() {
                    Some(runtime.controller.project_request().await?)
                } else {
                    crate::clm::project_clm_branch(&view.branch)?
                };
                let Some(projection) = projection else {
                    return Ok(messages);
                };
                projection
                    .blocks
                    .into_iter()
                    .map(|block| {
                        let message: pa_types::session::AgentMessage =
                            serde_json::from_value(block.message)?;
                        super::session_message_to_loop(&message).ok_or_else(|| {
                            ClmError::Journal("accepted message cannot enter the model loop".into())
                                .into()
                        })
                    })
                    .collect()
            })
        })
    }

    async fn request_prompt(&self, mut prompt: String) -> anyhow::Result<String> {
        let path = self.status(false).await?.path;
        let guidance = format!("\n\nCLM working context: {path}\nManage older, completed context proactively when large outputs, dead ends, or superseded notes consume useful space. Finish the current work unit before editing unless near the context limit. Batch stale spans into specific notes rather than repeatedly shortening small early sections; keep recent work you still need. Preserve decisions, exact commands and queries tried, ruled-out options with reasons not to retry them, facts marked VERIFIED or UNVERIFIED, and the NEXT action. Locate spans by existing CTX_TURN IDs or unique anchors instead of retyping the document.\nEdit this file only within one awaited tool action. Use an atomic save. Do not use detached or concurrent writers. Cancellation of bash, Python, custom, or detached writers cannot prove physical settlement and leaves CLM blocked. Preserve every CTX_PROTECTED reference. Invalid edits retain the accepted revision.");
        if !prompt.contains(&guidance) {
            prompt.push_str(&guidance);
        }
        Ok(prompt)
    }

    pub(super) fn bind_agent(&self, agent: &Arc<pa_agent::agent::Agent>) {
        let _ = self.agent.set(Arc::downgrade(agent));
    }

    async fn current_inputs(&self) -> anyhow::Result<ClmRequestInputs> {
        let agent = self
            .agent
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| ClmError::Budget("live request inputs unavailable".into()))?;
        let state = agent.state().await;
        let model = agent
            .model_override()
            .map_or(state.model, |current| current.model);
        Ok(ClmRequestInputs {
            system_prompt: self.request_prompt(state.system_prompt).await?,
            tools: state
                .tools
                .iter()
                .map(|tool| pa_types::ai::Tool {
                    name: tool.name().to_owned(),
                    description: tool.description().to_owned(),
                    parameters: tool.parameters().clone(),
                })
                .collect(),
            model: super::factory_host::agent_model_to_ai_model(&model),
        })
    }

    pub fn before_tool(self: &Arc<Self>) -> pa_agent::agent_loop::BeforeToolCallFn {
        let runtime = self.clone();
        Arc::new(move |context, signal| {
            let runtime = runtime.clone();
            Box::pin(async move {
                let _action = runtime.action_gate.lock().await;
                runtime.check_failure()?;
                if runtime.enabled() {
                    runtime
                        .controller
                        .prepare_tool(&context.tool_call.id)
                        .await?;
                    *runtime
                        .active_tool
                        .lock()
                        .map_err(|_| ClmError::Writer("runtime lock poisoned".into()))? =
                        Some(Arc::new(ClmActionFence {
                            tool_call_id: context.tool_call.id.clone(),
                            identity: crate::kernel::shared::ToolExecutionIdentity::new(),
                        }));
                    if signal.is_aborted() {
                        runtime.cancel_tool(&context.tool_call.id);
                        runtime.check_failure()?;
                    }
                }
                Ok(None)
            })
        })
    }

    pub fn after_tool(self: &Arc<Self>) -> pa_agent::agent_loop::AfterToolCallFn {
        let runtime = self.clone();
        Arc::new(move |context, signal| {
            let runtime = runtime.clone();
            Box::pin(async move {
                let _action = runtime.action_gate.lock().await;
                if runtime.enabled() {
                    if signal.is_aborted() {
                        runtime.cancel_tool(&context.tool_call.id);
                    }
                    runtime.check_failure()?;
                    let settling = async {
                        let inputs = runtime.current_inputs().await?;
                        if signal.is_aborted() {
                            runtime.cancel_tool(&context.tool_call.id);
                        }
                        runtime.check_failure()?;
                        runtime
                            .controller
                            .settle_tool(&context.tool_call.id, ClmToolOutcome::Completed, &inputs)
                            .await?;
                        Ok::<(), anyhow::Error>(())
                    };
                    tokio::pin!(settling);
                    tokio::select! {
                        biased;
                        () = signal.aborted() => {
                            runtime.cancel_tool(&context.tool_call.id);
                            settling.await?;
                        }
                        result = &mut settling => result?,
                    }
                    *runtime
                        .active_tool
                        .lock()
                        .map_err(|_| ClmError::Writer("runtime lock poisoned".into()))? = None;
                }
                Ok(None)
            })
        })
    }

    pub fn cancel_tool(&self, id: &str) {
        self.controller.cancel_tool(id);
        self.fail(&ClmError::Writer(
            "cancelled Python writer has no native repair authority".into(),
        ));
    }

    pub fn observe_python(&self, id: &str, result: &crate::tools::ipython::ExecuteResult) {
        if self.enabled() && result.status == crate::tools::ipython::ExecuteStatus::Aborted {
            self.cancel_tool(id);
        }
    }

    /// # Errors
    /// Rejects an oversized request, invalid budget input, or unresolved runtime failure.
    pub fn record_inputs(
        &self,
        inputs: &ClmRequestInputs,
        messages: &[serde_json::Value],
    ) -> anyhow::Result<Option<String>> {
        self.check_failure()?;
        if self.enabled() {
            let estimate = crate::clm::estimate_clm_request_tokens(
                messages,
                &inputs.system_prompt,
                &inputs.tools,
                &inputs.model,
            )?;
            let exceeded = estimate
                .max_input_tokens
                .is_some_and(|max| estimate.estimated_input_tokens > max);
            self.controller.record_request_estimate(estimate.clone());
            if exceeded {
                return Err(ClmError::Budget(
                    "shorten context or restore an accepted revision before dispatch".into(),
                )
                .into());
            }
            if let Some(maximum) = estimate.max_input_tokens {
                if estimate.estimated_input_tokens as f64
                    >= maximum as f64 * crate::clm::CONTEXT_WARNING_RATIO
                {
                    let prompt = format!("{}\n\nBefore other work, shorten older, completed CLM context via its working file. Aim below 60% of the input budget; preserve protected references and needed facts.",inputs.system_prompt);
                    let reminded = crate::clm::estimate_clm_request_tokens(
                        messages,
                        &prompt,
                        &inputs.tools,
                        &inputs.model,
                    )?;
                    if reminded.estimated_input_tokens <= maximum {
                        self.controller.record_request_estimate(reminded);
                        return Ok(Some(prompt));
                    }
                }
            }
        }
        Ok(None)
    }
}

tokio::task_local! { pub(super) static REQUEST_CLM: Arc<ClmRuntime>; }

pub(super) fn wrap_stream(
    runtime: Arc<ClmRuntime>,
    stream: pa_agent::stream::StreamFn,
) -> pa_agent::stream::StreamFn {
    Arc::new(
        move |model, mut context: pa_agent::stream::LlmContext, options| {
            let runtime = runtime.clone();
            let stream = stream.clone();
            Box::pin(async move {
                runtime.begin_request().await?;
                if runtime.enabled() {
                    context.system_prompt = Some(
                        runtime
                            .request_prompt(context.system_prompt.take().unwrap_or_default())
                            .await?,
                    );
                }
                let result = REQUEST_CLM
                    .scope(runtime.clone(), stream(model, context, options))
                    .await;
                if result.is_err() {
                    runtime
                        .request
                        .lock()
                        .map_err(|_| ClmError::Writer("request lock poisoned".into()))?
                        .take();
                }
                result
            })
        },
    )
}

/// Preserve each tool's policy while off; CLM admits one writer at a time.
struct ClmTool {
    inner: Arc<dyn pa_agent::types::AgentTool>,
    runtime: Arc<ClmRuntime>,
}
impl pa_agent::types::AgentTool for ClmTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters(&self) -> &serde_json::Value {
        self.inner.parameters()
    }
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn prepare_arguments(&self, args: &serde_json::Value) -> Option<serde_json::Value> {
        self.inner.prepare_arguments(args)
    }
    fn accepts_alias(&self, name: &str) -> bool {
        self.inner.accepts_alias(name)
    }
    fn execute(
        self: Arc<Self>,
        id: String,
        params: serde_json::Value,
        signal: pa_agent::abort::AbortSignal,
        update: pa_agent::types::AgentToolUpdateCallback,
    ) -> pa_agent::BoxFut<'static, anyhow::Result<pa_agent::types::AgentToolResult>> {
        let identity = self
            .runtime
            .active_tool
            .lock()
            .map_err(|_| ClmError::Writer("runtime lock poisoned".into()))
            .map(|active| {
                active
                    .as_ref()
                    .filter(|fence| self.runtime.enabled() && fence.tool_call_id == id)
                    .map(|fence| fence.identity.clone())
            });
        Box::pin(async move {
            crate::kernel::shared::with_tool_execution_identity(identity?, async move {
                self.inner.clone().execute(id, params, signal, update).await
            })
            .await
        })
    }
    fn execution_mode(&self) -> Option<pa_agent::types::ToolExecutionMode> {
        if self.runtime.enabled() {
            Some(pa_agent::types::ToolExecutionMode::Sequential)
        } else {
            self.inner.execution_mode()
        }
    }
}
pub(super) fn wrap_tools(
    runtime: &Arc<ClmRuntime>,
    tools: Vec<Arc<dyn pa_agent::types::AgentTool>>,
) -> Vec<Arc<dyn pa_agent::types::AgentTool>> {
    tools
        .into_iter()
        .map(|inner| {
            Arc::new(ClmTool {
                inner,
                runtime: runtime.clone(),
            }) as Arc<dyn pa_agent::types::AgentTool>
        })
        .collect()
}
