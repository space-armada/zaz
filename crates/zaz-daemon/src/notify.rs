//! Desktop notification support.
//!
//! Sends desktop notifications for task completion events using the system's
//! native notification mechanism (D-Bus on Linux, Notification Center on macOS).

use zaz_config::NotificationConfig;

/// Notification event types.
#[derive(Debug, Clone)]
pub enum NotifyEvent {
    /// A task completed successfully.
    TaskSuccess {
        task: String,
        group: String,
        duration_ms: u64,
    },
    /// A task failed.
    TaskFailed {
        task: String,
        group: String,
        exit_code: Option<i32>,
    },
    /// All tasks in a group completed successfully.
    GroupComplete { group: String },
    /// A group has failed tasks.
    GroupFailed { group: String },
}

impl NotifyEvent {
    /// Create a task success event.
    pub fn task_success(
        task: impl Into<String>,
        group: impl Into<String>,
        duration_ms: u64,
    ) -> Self {
        Self::TaskSuccess {
            task: task.into(),
            group: group.into(),
            duration_ms,
        }
    }

    /// Create a task failed event.
    pub fn task_failed(
        task: impl Into<String>,
        group: impl Into<String>,
        exit_code: Option<i32>,
    ) -> Self {
        Self::TaskFailed {
            task: task.into(),
            group: group.into(),
            exit_code,
        }
    }

    /// Create a group complete event.
    pub fn group_complete(group: impl Into<String>) -> Self {
        Self::GroupComplete {
            group: group.into(),
        }
    }

    /// Create a group failed event.
    pub fn group_failed(group: impl Into<String>) -> Self {
        Self::GroupFailed {
            group: group.into(),
        }
    }
}

/// Send a desktop notification based on configuration and event.
///
/// This function checks the notification configuration to determine if the
/// notification should be shown, then sends it via the system's native
/// notification mechanism.
///
/// Returns silently if notifications are disabled or if sending fails.
pub fn send_notification(config: &NotificationConfig, event: NotifyEvent) {
    if !config.enabled {
        return;
    }

    let (should_notify, title, body) = match event {
        NotifyEvent::TaskFailed {
            task,
            group,
            exit_code,
        } => {
            if !config.on_failure {
                return;
            }
            let body = match exit_code {
                Some(code) => format!("Task {} in {} failed with exit code {}", task, group, code),
                None => format!("Task {} in {} failed", task, group),
            };
            (true, format!("zaz: {} failed", task), body)
        }

        NotifyEvent::TaskSuccess {
            task,
            group,
            duration_ms,
        } => {
            if !config.on_success {
                return;
            }
            let duration_secs = duration_ms as f64 / 1000.0;
            (
                true,
                format!("zaz: {} complete", task),
                format!(
                    "Task {} in {} completed in {:.1}s",
                    task, group, duration_secs
                ),
            )
        }

        NotifyEvent::GroupComplete { group } => {
            if !config.on_group_complete {
                return;
            }
            (
                true,
                "zaz: Ready".to_string(),
                format!("Group {} is ready", group),
            )
        }

        NotifyEvent::GroupFailed { group } => {
            if !config.on_failure {
                return;
            }
            (
                true,
                "zaz: Group Failed".to_string(),
                format!("Group {} has failed tasks", group),
            )
        }
    };

    if !should_notify {
        return;
    }

    // Send the notification
    let result = notify_rust::Notification::new()
        .summary(&title)
        .body(&body)
        .appname("zaz")
        .show();

    if let Err(e) = result {
        tracing::debug!(error = %e, "failed to show desktop notification");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notify_event_constructors() {
        let event = NotifyEvent::task_success("build", "backend", 1500);
        match event {
            NotifyEvent::TaskSuccess {
                task,
                group,
                duration_ms,
            } => {
                assert_eq!(task, "build");
                assert_eq!(group, "backend");
                assert_eq!(duration_ms, 1500);
            }
            _ => panic!("wrong variant"),
        }

        let event = NotifyEvent::task_failed("test", "backend", Some(1));
        match event {
            NotifyEvent::TaskFailed {
                task,
                group,
                exit_code,
            } => {
                assert_eq!(task, "test");
                assert_eq!(group, "backend");
                assert_eq!(exit_code, Some(1));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_notifications_disabled() {
        let config = NotificationConfig::default(); // enabled = false by default
                                                    // This should not panic or error - it just returns early
        send_notification(&config, NotifyEvent::task_success("test", "group", 100));
    }

    #[test]
    fn test_on_failure_disabled() {
        let config = NotificationConfig {
            enabled: true,
            on_failure: false,
            on_success: true,
            on_group_complete: true,
        };
        // This should not send (on_failure is false)
        send_notification(&config, NotifyEvent::task_failed("test", "group", Some(1)));
    }

    #[test]
    fn test_on_success_disabled() {
        let config = NotificationConfig {
            enabled: true,
            on_failure: true,
            on_success: false, // This is the default
            on_group_complete: true,
        };
        // This should not send (on_success is false)
        send_notification(&config, NotifyEvent::task_success("test", "group", 100));
    }
}
