//! Host request handling: execute-side requests answered by the host
//! (harness/goal/etc.) and their settle/exit waits.

use super::{
    anyhow, json, lock, Arc, Duration, HostRequestPayload, Inner, Value,
    MAX_HANDLED_HOST_REQUEST_IDS,
};

impl Inner {
    /// Dispatch one typed request from kernel code to the registered handler
    /// and reply over the protocol. Unhandled requests answer with an error.
    pub(crate) fn start_host_request(
        self: &Arc<Self>,
        request_id: &str,
        cell_id: Option<&str>,
        data: Value,
    ) {
        {
            let mut g = lock(&self.guarded);
            let (seen, order) = &mut g.handled_host_request_ids;
            if seen.contains(request_id) {
                return;
            }
            seen.insert(request_id.to_string());
            order.push_back(request_id.to_string());
            while seen.len() > MAX_HANDLED_HOST_REQUEST_IDS {
                if let Some(oldest) = order.pop_front() {
                    seen.remove(&oldest);
                } else {
                    break;
                }
            }
        }
        // Verify before dispatch. Detached requests must not borrow the next cell's identity.
        let (verified_tool_call_id, verified_tool_execution_identity) = self
            .verified_tool_origin(cell_id)
            .map_or((None, None), |(id, identity)| (Some(id), identity));
        let inner = Arc::clone(self);
        let request_id = request_id.to_string();
        let task = tokio::spawn(async move {
            let result = inner
                .handle_host_request(
                    &data,
                    verified_tool_call_id,
                    verified_tool_execution_identity,
                )
                .await;
            let reply = match result {
                Ok(result) => json!({ "status": "ok", "result": result }),
                Err(error) => {
                    inner.append_diagnostic(&format!(
                        "host request failed for {request_id}: {error:#}"
                    ));
                    json!({ "status": "error", "error": format!("{error:#}") })
                }
            };
            let frame = json!({ "type": "host_reply", "id": request_id, "data": reply });
            if let Err(error) = inner.write_line(&frame).await {
                inner.append_diagnostic(&format!(
                    "failed to send host request reply for {request_id}: {error:#}"
                ));
            }
        });
        let mut g = lock(&self.guarded);
        // Completed task handles are dropped so the inflight set stays bounded.
        g.host_inflight.retain(|handle| !handle.is_finished());
        g.host_inflight.push(task);
    }

    #[cfg(test)]
    pub(crate) fn verified_tool_call_id(&self, cell_id: Option<&str>) -> Option<String> {
        self.verified_tool_origin(cell_id).map(|(id, _)| id)
    }

    fn verified_tool_origin(
        &self,
        cell_id: Option<&str>,
    ) -> Option<(String, Option<crate::kernel::shared::ToolExecutionIdentity>)> {
        let execution = lock(&self.guarded).active_execution.clone()?;
        self.verified_execution_origin(&execution, cell_id)
    }

    pub(super) fn verified_execution_origin(
        &self,
        execution: &Arc<super::ActiveExecution>,
        cell_id: Option<&str>,
    ) -> Option<(String, Option<crate::kernel::shared::ToolExecutionIdentity>)> {
        let buffers = lock(&execution.buffers);
        let guarded = lock(&self.guarded);
        if !guarded
            .active_execution
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, execution))
            || cell_id != Some(execution.request_id.as_str())
            || execution.opts.internal
            || !execution.namespace_code
            || buffers.settled
            || buffers.status != super::ExecuteStatus::Ok
            || execution
                .opts
                .signal
                .as_ref()
                .is_some_and(super::super::cancellation::AbortSignal::is_aborted)
        {
            return None;
        }
        Some((
            execution.opts.tool_call_id.clone()?,
            execution.opts.tool_execution_identity.clone(),
        ))
    }

    async fn handle_host_request(
        &self,
        data: &Value,
        verified_tool_call_id: Option<String>,
        verified_tool_execution_identity: Option<crate::kernel::shared::ToolExecutionIdentity>,
    ) -> anyhow::Result<Value> {
        let Some(obj) = data.as_object() else {
            return Err(anyhow!("host request payload must be an object"));
        };
        let request_type = obj
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow!("host request payload must have a string type"))?;
        let handler = self
            .options
            .host_handlers
            .get(request_type)
            .ok_or_else(|| {
                anyhow!("host request type \"{request_type}\" is not available in this session")
            })?
            .clone();
        handler(HostRequestPayload {
            data: data.clone(),
            verified_tool_call_id,
            verified_tool_execution_identity,
        })
        .await
    }

    /// Wait (bounded) for the in-flight host request tasks to settle.
    pub(crate) async fn wait_for_host_requests_to_settle(
        &self,
        tasks: Vec<tokio::task::JoinHandle<()>>,
        timeout_ms: u64,
    ) {
        let all = async {
            for task in tasks {
                let _ = task.await;
            }
        };
        if tokio::time::timeout(Duration::from_millis(timeout_ms), all)
            .await
            .is_err()
        {
            self.append_diagnostic(&format!(
                "timed out waiting {timeout_ms}ms for host request task(s) during shutdown"
            ));
        }
    }

    pub(crate) async fn wait_for_kernel_exit(&self) {
        let exit_rx = match lock(&self.child).as_ref() {
            Some(child) => child.exit_rx.clone(),
            None => return,
        };
        let mut exit_rx = exit_rx;
        if exit_rx.borrow().is_some() {
            return;
        }
        loop {
            if exit_rx.borrow().is_some() {
                return;
            }
            if exit_rx.changed().await.is_err() {
                return;
            }
        }
    }
}
