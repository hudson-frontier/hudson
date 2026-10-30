use crate::{models::*, Error, Result};

pub fn reserve(run: &mut Run, requests: &[OperationRequest]) -> Result<()> {
    let models = requests
        .iter()
        .filter(|r| matches!(r, OperationRequest::Model { .. }))
        .count() as u32;
    let tools = requests
        .iter()
        .filter(|r| matches!(r, OperationRequest::Tool { .. }))
        .count() as u32;
    let operations = run
        .usage
        .operations
        .checked_add(requests.len() as u32)
        .ok_or_else(|| Error::Invalid("operation limit".into()))?;
    let model_calls = run
        .usage
        .model_calls
        .checked_add(models)
        .ok_or_else(|| Error::Invalid("model call limit".into()))?;
    if operations > run.limits.max_operations
        || model_calls > run.limits.max_model_calls
        || requests.len() > run.limits.max_batch_size
    {
        return Err(Error::Invalid("execution budget exhausted".into()));
    }
    run.usage.operations = operations;
    run.usage.model_calls = model_calls;
    run.usage.tool_calls += tools;
    Ok(())
}

/// A shared, durable admission counter for a family of agent runtimes.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ModelBudget {
    pub limit: u32,
    pub admitted: u32,
}

/// Immutable per-run binding; a different batch may use the same Agent version.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ModelBudgetBinding {
    pub workspace: String,
    pub group: String,
}

pub struct BudgetedModel<M> {
    inner: M,
    store: crate::storage::Store,
    workspace: String,
    group: String,
}
impl<M> BudgetedModel<M> {
    /// Reopening an existing group preserves usage and requires the same limit.
    pub fn new(
        inner: M,
        store: crate::storage::Store,
        workspace: &str,
        group: &str,
        limit: u32,
    ) -> Result<Self> {
        if workspace.trim().is_empty() || group.trim().is_empty() || group.len() > 256 || limit == 0
        {
            return Err(Error::Invalid(
                "budget requires a workspace, group, and positive limit".into(),
            ));
        }
        store.transact(|d| {
            let key = (workspace.to_owned(), group.to_owned());
            match d.model_budgets.get(&key) {
                Some(existing) if existing.limit != limit => {
                    Err(Error::Conflict("shared model budget limit changed".into()))
                }
                Some(_) => Ok(()),
                None => {
                    d.model_budgets
                        .insert(key, ModelBudget { limit, admitted: 0 });
                    Ok(())
                }
            }
        })?;
        Ok(Self {
            inner,
            store,
            workspace: workspace.into(),
            group: group.into(),
        })
    }
    pub fn usage(&self) -> Result<ModelBudget> {
        self.store.read(|d| {
            d.model_budgets
                .get(&(self.workspace.clone(), self.group.clone()))
                .cloned()
                .ok_or(Error::NotFound)
        })
    }
    fn reserve_call(&self) -> std::result::Result<(), crate::adapters::tools::ExecutionError> {
        self.store
            .transact(|d| {
                let budget = d
                    .model_budgets
                    .get_mut(&(self.workspace.clone(), self.group.clone()))
                    .ok_or(Error::NotFound)?;
                if budget.admitted >= budget.limit {
                    return Err(Error::Invalid("shared model call budget exhausted".into()));
                }
                budget.admitted += 1;
                Ok(())
            })
            .map_err(|_| {
                crate::adapters::tools::ExecutionError::Failed(
                    "shared model budget exhausted or unavailable".into(),
                )
            })
    }
}
impl<M: crate::adapters::models::ModelExecutor> crate::adapters::models::ModelExecutor
    for BudgetedModel<M>
{
    fn budget_binding(&self) -> Option<ModelBudgetBinding> {
        Some(ModelBudgetBinding {
            workspace: self.workspace.clone(),
            group: self.group.clone(),
        })
    }
    fn take_usage(&mut self) -> Option<crate::models::TokenUsage> {
        self.inner.take_usage()
    }
    fn call_observed(
        &mut self,
        request: &hudson_harness::ModelRequest,
        observer: &mut dyn crate::adapters::models::ModelObserver,
    ) -> std::result::Result<hudson_harness::ModelResponse, crate::adapters::tools::ExecutionError>
    {
        self.inner.take_usage();
        if observer.cancelled() {
            return Err(crate::adapters::tools::ExecutionError::Failed(
                "model call cancelled before admission".into(),
            ));
        }
        self.reserve_call()?;
        self.inner.call_observed(request, observer)
    }
    fn call(
        &mut self,
        request: &hudson_harness::ModelRequest,
    ) -> std::result::Result<hudson_harness::ModelResponse, crate::adapters::tools::ExecutionError>
    {
        self.call_controlled(request, &|| false)
    }
    fn call_controlled(
        &mut self,
        request: &hudson_harness::ModelRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> std::result::Result<hudson_harness::ModelResponse, crate::adapters::tools::ExecutionError>
    {
        self.inner.take_usage();
        if cancelled() {
            return Err(crate::adapters::tools::ExecutionError::Failed(
                "model call cancelled before admission".into(),
            ));
        }
        self.reserve_call()?;
        // Charge admission, even if the provider fails: an uncertain request may
        // have consumed tokens. Never refund automatically or reset on restart.
        self.inner.call_controlled(request, cancelled)
    }
}
