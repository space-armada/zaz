//! Prep command runner.

use crate::{Executor, ProcessError};
use zaz_config::PrepCommand;

/// Runs prep commands sequentially.
pub struct PrepRunner {
    executor: Executor,
}

impl PrepRunner {
    /// Create a new prep runner.
    pub fn new(executor: Executor) -> Self {
        Self { executor }
    }

    /// Run a single prep command.
    pub async fn run_one(&self, prep: &PrepCommand) -> Result<(), ProcessError> {
        tracing::info!(name = %prep.name, "running prep command");
        self.executor.run(&prep.command).await?;
        tracing::info!(name = %prep.name, "prep command completed");
        Ok(())
    }

    /// Run all prep commands in sequence.
    /// Stops on first error (fail-fast).
    pub async fn run_all(&self, preps: &[PrepCommand]) -> Result<(), ProcessError> {
        for prep in preps {
            self.run_one(prep).await?;
        }
        Ok(())
    }
}
