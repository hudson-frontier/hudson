use super::tools::ExecutionError;
use hudson_harness::{ModelRequest, ModelResponse};

/// Host-owned correlation for one admitted attempt. Never a credential or a
/// capability grant; adapters return it unchanged when reporting native state.
#[derive(Clone, Copy, Debug)]
pub struct ModelCallContext {
    pub run_id: uuid::Uuid,
    pub operation_id: uuid::Uuid,
    pub attempt_id: uuid::Uuid,
}

/// Provisional output belongs to the exact active attempt. Executors must obey
/// backpressure and stop on errors; successful text is still verified normally.
pub trait ModelObserver {
    fn context(&self) -> Option<ModelCallContext> {
        None
    }
    fn cancelled(&self) -> bool;
    fn text_delta(&mut self, text: &str) -> Result<(), ExecutionError>;
    fn heartbeat(&mut self) -> Result<(), ExecutionError> {
        Ok(())
    }
}

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
    fn call_observed(
        &mut self,
        request: &ModelRequest,
        observer: &mut dyn ModelObserver,
    ) -> Result<ModelResponse, ExecutionError> {
        self.call_controlled(request, &|| observer.cancelled())
    }
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
    fn call_observed(
        &mut self,
        request: &ModelRequest,
        observer: &mut dyn ModelObserver,
    ) -> Result<ModelResponse, ExecutionError> {
        (**self).call_observed(request, observer)
    }
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
