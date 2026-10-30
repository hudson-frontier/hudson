use super::tools::ExecutionError;
use hudson_harness::{ModelRequest, ModelResponse};

/// Runtime-owned IO boundary; a harness never holds this executor.
pub trait ModelExecutor: Send {
    /// Runtime pins this at submission and checks it before resuming effects.
    fn budget_binding(&self) -> Option<crate::budgets::ModelBudgetBinding> {
        None
    }
    /// Consume the latest call's optional usage; never reuse a previous report.
    fn take_usage(&mut self) -> Option<crate::models::TokenUsage> {
        None
    }
    fn call(&mut self, request: &ModelRequest) -> Result<ModelResponse, ExecutionError>;
    /// Called on the host's blocking driver. Adapters with interruptible IO poll
    /// this control between bounded waits; existing executors keep their behavior.
    fn call_controlled(
        &mut self,
        request: &ModelRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ModelResponse, ExecutionError> {
        self.take_usage();
        if cancelled() {
            return Err(ExecutionError::Failed(
                "model call cancelled before dispatch".into(),
            ));
        }
        self.call(request)
    }
}

impl<T: ModelExecutor + ?Sized> ModelExecutor for Box<T> {
    fn budget_binding(&self) -> Option<crate::budgets::ModelBudgetBinding> {
        (**self).budget_binding()
    }
    fn take_usage(&mut self) -> Option<crate::models::TokenUsage> {
        (**self).take_usage()
    }
    fn call(&mut self, request: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        (**self).call(request)
    }
    fn call_controlled(
        &mut self,
        request: &ModelRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ModelResponse, ExecutionError> {
        (**self).call_controlled(request, cancelled)
    }
}
