use crate::{
    config::TaskDef,
    meta::{TaskMeta, TaskStatus},
    runner::{TaskRunReport, TaskRunner},
};
use anyhow::Result;
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
};
use tokio::{sync::mpsc, task::JoinSet};
use tokio_util::sync::CancellationToken;

pub enum DispatchRequest {
    Task(TaskDef),
    Wave(Vec<TaskDef>),
    Seal,
}

struct PendingTask {
    def: TaskDef,
    barrier: HashSet<String>,
}

impl PendingTask {
    fn is_unblocked(&self, completed: &HashSet<String>) -> bool {
        self.barrier.is_subset(completed)
            && self
                .def
                .depends_on
                .iter()
                .all(|dep| completed.contains(dep))
    }
}

pub struct RunCoordinator {
    run_dir: PathBuf,
    runner: TaskRunner,
    cancel: CancellationToken,
    dispatch_rx: mpsc::Receiver<DispatchRequest>,
}

impl RunCoordinator {
    pub fn start(
        run_dir: &Path,
        runner: TaskRunner,
        cancel: CancellationToken,
    ) -> (Self, mpsc::Sender<DispatchRequest>) {
        let (dispatch_tx, dispatch_rx) = mpsc::channel(256);
        (
            Self {
                run_dir: run_dir.to_path_buf(),
                runner,
                cancel,
                dispatch_rx,
            },
            dispatch_tx,
        )
    }

    pub async fn run(mut self, initial_tasks: Vec<TaskDef>) -> bool {
        let log_dir = self.run_dir.join("logs");
        let mut join_set: JoinSet<TaskRunReport> = JoinSet::new();
        let mut pending = VecDeque::new();
        let mut accepted = HashSet::new();
        let mut running = HashSet::new();
        let mut completed = HashSet::new();
        let mut all_ok = true;
        let mut sealed = false;

        for task in initial_tasks {
            accepted.insert(task.name.clone());
            pending.push_back(PendingTask {
                def: task,
                barrier: HashSet::new(),
            });
        }

        loop {
            self.spawn_ready_tasks(&mut pending, &mut running, &completed, &mut join_set);

            if join_set.is_empty() && pending.is_empty() && (sealed || self.dispatch_rx.is_closed())
            {
                break;
            }

            tokio::select! {
                biased;

                result = join_set.join_next(), if !join_set.is_empty() => {
                    match result {
                        Some(Ok(report)) => {
                            running.remove(&report.name);
                            completed.insert(report.name.clone());

                            match report.status {
                                TaskStatus::Failed => {
                                    all_ok = false;
                                    self.cancel.cancel();
                                }
                                TaskStatus::Cancelled => {
                                    all_ok = false;
                                }
                                _ => {}
                            }
                        }
                        Some(Err(_join_err)) => {
                            all_ok = false;
                            self.cancel.cancel();
                        }
                        None => {}
                    }
                }

                msg = self.dispatch_rx.recv(), if !sealed => {
                    match msg {
                        Some(DispatchRequest::Task(task)) => {
                            let pending_names = pending_names(&pending);
                            let barrier = if task.depends_on.is_empty() {
                                build_barrier(&running, &pending_names)
                            } else {
                                HashSet::new()
                            };
                            accepted.insert(task.name.clone());
                            pending.push_back(PendingTask { def: task, barrier });
                        }
                        Some(DispatchRequest::Wave(tasks)) => {
                            let pending_names = pending_names(&pending);
                            let barrier = build_barrier(&running, &pending_names);
                            for task in tasks {
                                accepted.insert(task.name.clone());
                                pending.push_back(PendingTask {
                                    def: task,
                                    barrier: barrier.clone(),
                                });
                            }
                        }
                        Some(DispatchRequest::Seal) | None => {
                            self.dispatch_rx.close();
                            sealed = true;
                        }
                    }
                }

                _ = self.cancel.cancelled() => {
                    self.dispatch_rx.close();
                    while let Some(task) = pending.pop_front() {
                        write_cancelled_meta(
                            &log_dir,
                            &task.def,
                            "run cancelled before task started",
                        );
                    }
                    all_ok = false;
                    sealed = true;
                }
            }
        }

        all_ok
    }

    fn spawn_ready_tasks(
        &self,
        pending: &mut VecDeque<PendingTask>,
        running: &mut HashSet<String>,
        completed: &HashSet<String>,
        join_set: &mut JoinSet<TaskRunReport>,
    ) {
        if self.cancel.is_cancelled() {
            return;
        }

        let pending_count = pending.len();
        for _ in 0..pending_count {
            let Some(task) = pending.pop_front() else {
                break;
            };

            if task.is_unblocked(completed) {
                let task_name = task.def.name.clone();
                let runner = self.runner.clone();
                let cancel = self.cancel.clone();

                // TODO: inject facts preamble from FactsStore here
                join_set.spawn(async move { runner.run_task(task.def, cancel, None).await });
                running.insert(task_name);
            } else {
                pending.push_back(task);
            }
        }
    }
}

pub fn validate_dispatch_name(name: &str, accepted: &HashSet<String>) -> Result<()> {
    crate::commands::serve::validate_task_name(name)?;
    anyhow::ensure!(
        !accepted.contains(name),
        "task name '{}' is already in this run",
        name
    );
    Ok(())
}

pub fn validate_dispatch_deps(depends_on: &[String], accepted: &HashSet<String>) -> Result<()> {
    for dep in depends_on {
        anyhow::ensure!(
            accepted.contains(dep),
            "depends_on references unknown task '{}'; only already-accepted tasks are allowed",
            dep
        );
    }
    Ok(())
}

pub fn build_barrier(running: &HashSet<String>, pending_names: &[String]) -> HashSet<String> {
    running
        .iter()
        .cloned()
        .chain(pending_names.iter().cloned())
        .collect()
}

fn pending_names(pending: &VecDeque<PendingTask>) -> Vec<String> {
    pending.iter().map(|task| task.def.name.clone()).collect()
}

fn write_cancelled_meta(log_dir: &Path, task: &TaskDef, reason: &str) {
    let meta_path = log_dir.join(format!("{}.meta.json", task.name));
    let mut meta = TaskMeta::load(&meta_path)
        .unwrap_or_else(|_| TaskMeta::new(&task.name, &task.cwd.to_string_lossy(), &task.prompt));
    if meta.agent_id.is_empty() {
        meta.agent_id = task.agent_id.clone().unwrap_or_else(|| task.name.clone());
    }
    if meta.thread_id.is_empty() {
        meta.thread_id = task.thread_id.clone().unwrap_or_else(|| task.name.clone());
    }
    meta.status = TaskStatus::Cancelled;
    meta.error = Some(reason.to_string());
    meta.end_time = Some(chrono::Local::now());
    let _ = meta.save(log_dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_rejects_duplicate() {
        let accepted: std::collections::HashSet<String> =
            ["task_a".to_string()].into_iter().collect();
        assert!(validate_dispatch_name("task_a", &accepted).is_err());
        assert!(validate_dispatch_name("task_b", &accepted).is_ok());
    }

    #[test]
    fn validate_name_rejects_forward_dep() {
        let accepted: std::collections::HashSet<String> =
            ["task_a".to_string()].into_iter().collect();
        let result = validate_dispatch_deps(&["task_b".to_string()], &accepted);
        assert!(result.is_err());
    }

    #[test]
    fn validate_name_allows_known_dep() {
        let accepted: std::collections::HashSet<String> =
            ["task_a".to_string()].into_iter().collect();
        let result = validate_dispatch_deps(&["task_a".to_string()], &accepted);
        assert!(result.is_ok());
    }

    #[test]
    fn barrier_snapshot_includes_running_and_pending() {
        let running: std::collections::HashSet<String> = ["r1".to_string()].into_iter().collect();
        let pending_names = vec!["p1".to_string()];
        let barrier = build_barrier(&running, &pending_names);
        assert!(barrier.contains("r1"));
        assert!(barrier.contains("p1"));
    }
}
