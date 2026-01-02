//! Task command runner.

use crate::{Executor, ProcessError};
use zaz_config::TaskCommand;

/// Runs task commands sequentially.
pub struct TaskRunner {
    executor: Executor,
}

impl TaskRunner {
    /// Create a new task runner.
    pub fn new(executor: Executor) -> Self {
        Self { executor }
    }

    /// Run a single task command.
    pub async fn run_one(&self, task: &TaskCommand) -> Result<(), ProcessError> {
        tracing::info!(name = %task.name, "running task command");
        self.executor.run(&task.command).await?;
        tracing::info!(name = %task.name, "task command completed");
        Ok(())
    }

    /// Run all task commands in sequence.
    /// Stops on first error (fail-fast).
    pub async fn run_all(&self, tasks: &[TaskCommand]) -> Result<(), ProcessError> {
        for task in tasks {
            self.run_one(task).await?;
        }
        Ok(())
    }
}
