use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

use crate::graph_flow::{
  context::Context,
  error::{GraphError, Result},
  storage::Session,
  task::{NextAction, Task, TaskResult},
};

/// Type alias for edge condition functions
pub type EdgeCondition = Arc<dyn Fn(&Context) -> bool + Send + Sync>;

/// Edge between tasks in the graph
#[derive(Clone)]
pub struct Edge {
  pub from: String,
  pub to: String,
  pub condition: Option<EdgeCondition>,
}

/// Default per-task execution timeout (5 minutes).
const DEFAULT_TASK_TIMEOUT: Duration = Duration::from_secs(300);

/// A graph of tasks that can be executed.
///
/// A `Graph` is **immutable once built**: all construction-time mutation
/// (`add_task` / `add_edge` / `set_start_task` / timeout) lives on
/// [`GraphBuilder`], and [`GraphBuilder::build`] freezes the accumulated
/// state into plain `HashMap` / `Vec` / `Option` fields. Runtime hot paths
/// such as [`find_next_task`](Self::find_next_task) therefore read these
/// fields directly with **no locking**, even when the graph is shared as
/// `Arc<Graph>`.
pub struct Graph {
  pub id: String,
  tasks: HashMap<String, Arc<dyn Task>>,
  edges: Vec<Edge>,
  start_task_id: Option<String>,
  task_timeout: Duration,
}

impl Graph {
  /// Create an empty, immutable graph with no tasks or edges.
  ///
  /// To populate a graph use [`GraphBuilder`]; this constructor is only
  /// useful for trivial / placeholder graphs (e.g. storage round-trip tests).
  pub fn new(id: impl Into<String>) -> Self {
    Self {
      id: id.into(),
      tasks: HashMap::new(),
      edges: Vec::new(),
      start_task_id: None,
      task_timeout: DEFAULT_TASK_TIMEOUT,
    }
  }

  /// Execute the graph with session management
  /// This method manages the session state and returns a simple status
  pub async fn execute_session(&self, session: &mut Session) -> Result<ExecutionResult> {
    tracing::info!(
        graph_id = %self.id,
        session_id = %session.id,
        current_task = %session.current_task_id,
        "Starting graph execution"
    );

    // Execute ONLY the current task (not the full recursive chain)
    let result = self.execute_single_task(&session.current_task_id, session.context.clone()).await?;

    // Handle next action at the session level
    match &result.next_action {
      NextAction::Continue => {
        // Update session status message if provided
        session.status_message = result.status_message.clone();

        // Find the next task but don't execute it
        if let Some(next_task_id) = self.find_next_task(&result.task_id, &session.context) {
          session.current_task_id = next_task_id.clone();
          Ok(ExecutionResult {
            response: result.response,
            status: ExecutionStatus::Paused {
              next_task_id,
              reason: "Task completed, continuing to next task".to_string(),
            },
          })
        } else {
          // No next task found, stay at current task
          session.current_task_id = result.task_id.clone();
          Ok(ExecutionResult {
            response: result.response,
            status: ExecutionStatus::Paused {
              next_task_id: result.task_id.clone(),
              reason: "No outgoing edge found from current task".to_string(),
            },
          })
        }
      }
      NextAction::ContinueAndExecute => {
        // Update session status message if provided
        session.status_message = result.status_message.clone();

        // Find the next task and execute it immediately (recursive behavior)
        if let Some(next_task_id) = self.find_next_task(&result.task_id, &session.context) {
          // Instead of using the old execute method that clones context,
          // continue executing in session mode to preserve context updates
          session.current_task_id = next_task_id;

          // Recursively call execute_session to maintain proper context sharing
          return Box::pin(self.execute_session(session)).await;
        } else {
          // No next task found, stay at current task
          session.current_task_id = result.task_id.clone();
          Ok(ExecutionResult {
            response: result.response,
            status: ExecutionStatus::Paused {
              next_task_id: result.task_id.clone(),
              reason: "No outgoing edge found from current task".to_string(),
            },
          })
        }
      }
      NextAction::WaitForInput => {
        // Update session status message if provided
        session.status_message = result.status_message.clone();
        // Stay at the current task
        session.current_task_id = result.task_id.clone();
        Ok(ExecutionResult { response: result.response, status: ExecutionStatus::WaitingForInput })
      }
      NextAction::End => {
        // Update session status message if provided
        session.status_message = result.status_message.clone();
        session.current_task_id = result.task_id.clone();
        Ok(ExecutionResult { response: result.response, status: ExecutionStatus::Completed })
      }
      NextAction::GoTo(target_id) => {
        // Update session status message if provided
        session.status_message = result.status_message.clone();
        if self.tasks.contains_key(target_id) {
          session.current_task_id = target_id.clone();
          Ok(ExecutionResult {
            response: result.response,
            status: ExecutionStatus::Paused {
              next_task_id: target_id.clone(),
              reason: "Task requested jump to specific task".to_string(),
            },
          })
        } else {
          Err(GraphError::TaskNotFound(target_id.clone()))
        }
      }
      NextAction::GoBack => {
        // Update session status message if provided
        session.status_message = result.status_message.clone();
        // For now, stay at current task - could implement back navigation logic later
        session.current_task_id = result.task_id.clone();
        Ok(ExecutionResult { response: result.response, status: ExecutionStatus::WaitingForInput })
      }
    }
  }

  /// Execute a single task without following Continue actions
  async fn execute_single_task(&self, task_id: &str, context: Context) -> Result<TaskResult> {
    tracing::debug!(
        task_id = %task_id,
        "Executing single task"
    );

    let task = self.tasks.get(task_id).ok_or_else(|| GraphError::TaskNotFound(task_id.to_string()))?;

    // Execute task with timeout
    let task_future = task.run(context);
    let mut result = match timeout(self.task_timeout, task_future).await {
      Ok(Ok(result)) => result,
      Ok(Err(e)) => return Err(GraphError::TaskExecutionFailed(format!("Task '{}' failed: {}", task_id, e))),
      Err(_) => {
        return Err(GraphError::TaskExecutionFailed(format!(
          "Task '{}' timed out after {:?}",
          task_id, self.task_timeout
        )));
      }
    };

    // Set the task_id in the result to track which task generated it
    result.task_id = task_id.to_string();

    Ok(result)
  }

  /// Execute the graph starting from a specific task
  pub async fn execute(&self, task_id: &str, context: Context) -> Result<TaskResult> {
    let task = self.tasks.get(task_id).ok_or_else(|| GraphError::TaskNotFound(task_id.to_string()))?;

    let mut result = task.run(context.clone()).await?;

    // Set the task_id in the result to track which task generated it
    result.task_id = task_id.to_string();

    // Handle next action
    match &result.next_action {
      NextAction::Continue => {
        // If this task has a response, stop here and don't continue to next task
        // This allows the response to be returned to the user
        if result.response.is_some() {
          Ok(result)
        } else {
          // Find the next task based on edges
          if let Some(next_task_id) = self.find_next_task(task_id, &context) {
            Box::pin(self.execute(&next_task_id, context)).await
          } else {
            Ok(result)
          }
        }
      }
      NextAction::GoTo(target_id) => {
        if self.tasks.contains_key(target_id) {
          Box::pin(self.execute(target_id, context)).await
        } else {
          Err(GraphError::TaskNotFound(target_id.clone()))
        }
      }
      _ => Ok(result),
    }
  }

  /// Find the next task based on edges and conditions.
  ///
  /// Hot path — reads the immutable `edges` slice directly with no locking.
  pub fn find_next_task(&self, current_task_id: &str, context: &Context) -> Option<String> {
    let mut fallback: Option<String> = None;
    for edge in self.edges.iter().filter(|e| e.from == current_task_id) {
      match &edge.condition {
        Some(pred) if pred(context) => return Some(edge.to.clone()),
        None if fallback.is_none() => fallback = Some(edge.to.clone()),
        _ => {}
      }
    }
    fallback
  }

  /// Get the start task ID
  pub fn start_task_id(&self) -> Option<String> {
    self.start_task_id.clone()
  }

  /// Get a task by ID
  pub fn get_task(&self, task_id: &str) -> Option<Arc<dyn Task>> {
    self.tasks.get(task_id).cloned()
  }
}

/// Builder for creating graphs.
///
/// Accumulates tasks / edges / config in plain (non-`Sync`-locked) fields and
/// freezes them into an immutable [`Graph`] on [`build`](Self::build).
pub struct GraphBuilder {
  id: String,
  tasks: HashMap<String, Arc<dyn Task>>,
  edges: Vec<Edge>,
  start_task_id: Option<String>,
  task_timeout: Duration,
}

impl GraphBuilder {
  pub fn new(id: impl Into<String>) -> Self {
    Self {
      id: id.into(),
      tasks: HashMap::new(),
      edges: Vec::new(),
      start_task_id: None,
      task_timeout: DEFAULT_TASK_TIMEOUT,
    }
  }

  pub fn add_task(mut self, task: Arc<dyn Task>) -> Self {
    let task_id = task.id().to_string();
    // The first task added becomes the default start task.
    if self.tasks.is_empty() {
      self.start_task_id = Some(task_id.clone());
    }
    self.tasks.insert(task_id, task);
    self
  }

  pub fn add_edge(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
    self.edges.push(Edge { from: from.into(), to: to.into(), condition: None });
    self
  }

  /// Add a conditional edge with an explicit `else` branch.
  /// `yes` is taken when `condition(ctx)` returns `true`; otherwise `no` is chosen.
  pub fn add_conditional_edge<F>(
    mut self,
    from: impl Into<String>,
    condition: F,
    yes: impl Into<String>,
    no: impl Into<String>,
  ) -> Self
  where
    F: Fn(&Context) -> bool + Send + Sync + 'static,
  {
    let from = from.into();
    let predicate: EdgeCondition = Arc::new(condition);
    // "yes" branch
    self.edges.push(Edge { from: from.clone(), to: yes.into(), condition: Some(predicate) });
    // "else" branch (unconditional fallback)
    self.edges.push(Edge { from, to: no.into(), condition: None });
    self
  }

  /// Set the starting task. Ignored if `task_id` was never added.
  pub fn set_start_task(mut self, task_id: impl Into<String>) -> Self {
    let task_id = task_id.into();
    if self.tasks.contains_key(&task_id) {
      self.start_task_id = Some(task_id);
    }
    self
  }

  /// Set the per-task execution timeout at construction time.
  pub fn with_task_timeout(mut self, timeout: Duration) -> Self {
    self.task_timeout = timeout;
    self
  }

  /// Freeze the accumulated state into an immutable [`Graph`].
  pub fn build(self) -> Graph {
    // Validate the graph before returning
    if self.tasks.is_empty() {
      tracing::warn!("Building graph with no tasks");
    }

    // Check for orphaned tasks (tasks with no incoming or outgoing edges)
    if self.tasks.len() > 1 {
      let mut connected_tasks = std::collections::HashSet::new();
      for edge in &self.edges {
        connected_tasks.insert(edge.from.as_str());
        connected_tasks.insert(edge.to.as_str());
      }
      for task_id in self.tasks.keys() {
        if !connected_tasks.contains(task_id.as_str()) {
          tracing::warn!(
              task_id = %task_id,
              "Task has no edges - it may be unreachable"
          );
        }
      }
    }

    Graph {
      id: self.id,
      tasks: self.tasks,
      edges: self.edges,
      start_task_id: self.start_task_id,
      task_timeout: self.task_timeout,
    }
  }
}

/// Status of graph execution
#[derive(Debug, Clone)]
pub struct ExecutionResult {
  pub response: Option<String>,
  pub status: ExecutionStatus,
}

#[derive(Debug, Clone)]
pub enum ExecutionStatus {
  /// Paused, will continue automatically to the specified next task
  Paused { next_task_id: String, reason: String },
  /// Waiting for user input to continue
  WaitingForInput,
  /// Workflow completed successfully
  Completed,
  /// Error occurred during execution
  Error(String),
}
