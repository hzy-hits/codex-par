# Phase 2: Dynamic Scheduling + Shared Context Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add runtime task dispatch (`dispatch_task`, `dispatch_wave`, `seal_dispatch`) and a shared facts store (`write_fact`, `read_facts`) to the `codex-par serve` MCP path, so Claude can spawn follow-up tasks and inject shared context into worker prompts.

**Architecture:** Keep the static wave executor (`execution.rs`) for the CLI `run` path. The `serve` path gets a new `RunCoordinator` (in `src/coordinator.rs`) that owns a long-lived `JoinSet` and an `mpsc` dispatch channel. `ActiveRun` gains a `dispatch_tx` sender. Facts live in `src/facts.rs` and are auto-prepended at dispatch time.

**Tech Stack:** Rust, tokio (`JoinSet`, `mpsc`, `CancellationToken`), serde_json, chrono, rmcp (MCP framework already in use)

---

## Task 1: `src/facts.rs` — Shared Facts Store

**Files:**
- Create: `src/facts.rs`
- Test: inline `#[cfg(test)]` in `src/facts.rs`

### Step 1: Write the failing tests

Add this test module at the bottom of the new file:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_and_read_fact() {
        let dir = TempDir::new().unwrap();
        let store = FactsStore::new(dir.path());
        store.write_fact("repo", "my-value".to_string(), None).unwrap();
        let facts = store.read_all().unwrap();
        assert_eq!(facts["repo"].value, "my-value");
    }

    #[test]
    fn overwrite_same_key() {
        let dir = TempDir::new().unwrap();
        let store = FactsStore::new(dir.path());
        store.write_fact("k", "v1".to_string(), None).unwrap();
        store.write_fact("k", "v2".to_string(), None).unwrap();
        let facts = store.read_all().unwrap();
        assert_eq!(facts["k"].value, "v2");
        assert_eq!(facts.len(), 1);
    }

    #[test]
    fn truncates_value_over_1kib() {
        let dir = TempDir::new().unwrap();
        let store = FactsStore::new(dir.path());
        let big = "x".repeat(2000);
        store.write_fact("k", big, None).unwrap();
        let facts = store.read_all().unwrap();
        assert!(facts["k"].value.len() <= 1024 + 12); // "[truncated]" is 11 chars
        assert!(facts["k"].value.ends_with("[truncated]"));
    }

    #[test]
    fn empty_store_returns_none_preamble() {
        let dir = TempDir::new().unwrap();
        let store = FactsStore::new(dir.path());
        assert!(store.build_preamble().unwrap().is_none());
    }

    #[test]
    fn preamble_contains_key_value_and_path() {
        let dir = TempDir::new().unwrap();
        let store = FactsStore::new(dir.path());
        store.write_fact("model", "o4-mini".to_string(), None).unwrap();
        let preamble = store.build_preamble().unwrap().unwrap();
        assert!(preamble.contains("model: o4-mini"));
        assert!(preamble.contains("facts.json"));
        assert!(preamble.starts_with("=== Shared Context ==="));
    }

    #[test]
    fn preamble_truncates_at_4kib_total() {
        let dir = TempDir::new().unwrap();
        let store = FactsStore::new(dir.path());
        // Write enough facts to exceed 4 KiB total
        for i in 0..20 {
            store.write_fact(
                &format!("key{:02}", i),
                "x".repeat(300),
                None,
            ).unwrap();
        }
        let preamble = store.build_preamble().unwrap().unwrap();
        assert!(preamble.contains("[additional facts truncated"));
        // Total preamble including header/footer should be bounded
        assert!(preamble.len() < 5000);
    }
}
```

### Step 2: Run to verify it fails

```bash
cargo test --offline -p codex-par facts 2>&1 | head -20
```
Expected: compile error — `FactsStore` not found.

### Step 3: Implement `src/facts.rs`

```rust
use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::{Path, PathBuf}};

const MAX_VALUE_BYTES: usize = 1024;
const MAX_PREAMBLE_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactEntry {
    pub value: String,
    pub artifact_path: Option<PathBuf>,
    pub updated_at: DateTime<Local>,
}

pub struct FactsStore {
    run_dir: PathBuf,
}

impl FactsStore {
    pub fn new(run_dir: &Path) -> Self {
        Self { run_dir: run_dir.to_path_buf() }
    }

    fn facts_path(&self) -> PathBuf {
        self.run_dir.join("shared").join("facts.json")
    }

    pub fn write_fact(
        &self,
        key: &str,
        value: String,
        artifact_path: Option<PathBuf>,
    ) -> Result<()> {
        let value = if value.len() > MAX_VALUE_BYTES {
            format!("{}[truncated]", &value[..MAX_VALUE_BYTES])
        } else {
            value
        };
        std::fs::create_dir_all(self.run_dir.join("shared"))?;
        let mut facts = self.read_all().unwrap_or_default();
        facts.insert(
            key.to_string(),
            FactEntry { value, artifact_path, updated_at: Local::now() },
        );
        let tmp = self.facts_path().with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&facts)?)?;
        std::fs::rename(&tmp, self.facts_path())?;
        Ok(())
    }

    pub fn read_all(&self) -> Result<BTreeMap<String, FactEntry>> {
        let path = self.facts_path();
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        Ok(serde_json::from_slice(&std::fs::read(&path)?)?)
    }

    /// Build a prompt preamble from stored facts. Returns None if no facts exist.
    /// Sorted by key. Capped at MAX_PREAMBLE_BYTES total (excluding header/footer).
    pub fn build_preamble(&self) -> Result<Option<String>> {
        let facts = self.read_all()?;
        if facts.is_empty() {
            return Ok(None);
        }
        let facts_path = self.facts_path();
        let mut lines: Vec<String> = Vec::new();
        let mut total = 0usize;

        for (key, entry) in &facts {
            let line = if let Some(ref ap) = entry.artifact_path {
                format!("{}: {} [artifact: {}]", key, entry.value, ap.display())
            } else {
                format!("{}: {}", key, entry.value)
            };
            if total + line.len() + 1 > MAX_PREAMBLE_BYTES {
                lines.push("[additional facts truncated — see facts.json]".to_string());
                break;
            }
            total += line.len() + 1;
            lines.push(line);
        }

        let mut out = vec!["=== Shared Context ===".to_string()];
        out.extend(lines);
        out.push(format!("[Full facts at: {}]", facts_path.display()));
        out.push("======================".to_string());
        Ok(Some(out.join("\n")))
    }
}
```

### Step 4: Run tests

```bash
cargo test --offline -p codex-par facts 2>&1
```
Expected: all `facts::tests::*` pass.

### Step 5: Commit

```bash
git add src/facts.rs
git commit -m "feat: add FactsStore for shared context injection"
```

---

## Task 2: `src/coordinator.rs` — Run Coordinator

**Files:**
- Create: `src/coordinator.rs`
- Test: inline `#[cfg(test)]` in `src/coordinator.rs`

### Step 1: Write failing tests

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::TaskRunner;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    fn make_task(name: &str, deps: Vec<&str>) -> crate::config::TaskDef {
        crate::config::TaskDef {
            name: name.to_string(),
            cwd: std::path::PathBuf::from("/tmp"),
            prompt: format!("echo {}", name),
            sandbox: crate::config::Sandbox::default(),
            agent_id: None,
            thread_id: None,
            output_schema: None,
            ephemeral: false,
            add_dirs: vec![],
            model: None,
            depends_on: deps.into_iter().map(String::from).collect(),
            ask_for_approval: "never".to_string(),
            config_overrides: vec![],
            profile: None,
        }
    }

    #[test]
    fn validate_name_rejects_duplicate() {
        let existing: std::collections::HashSet<String> =
            ["task_a".to_string()].into_iter().collect();
        assert!(validate_dispatch_name("task_a", &existing).is_err());
        assert!(validate_dispatch_name("task_b", &existing).is_ok());
    }

    #[test]
    fn validate_name_rejects_forward_dep() {
        let existing: std::collections::HashSet<String> =
            ["task_a".to_string()].into_iter().collect();
        // "task_b" is not in accepted set
        let result = validate_dispatch_deps(&["task_b".to_string()], &existing);
        assert!(result.is_err());
    }

    #[test]
    fn validate_name_allows_known_dep() {
        let existing: std::collections::HashSet<String> =
            ["task_a".to_string()].into_iter().collect();
        let result = validate_dispatch_deps(&["task_a".to_string()], &existing);
        assert!(result.is_ok());
    }

    #[test]
    fn barrier_snapshot_includes_running_and_pending() {
        let running: std::collections::HashSet<String> =
            ["r1".to_string()].into_iter().collect();
        let pending_names: Vec<String> = vec!["p1".to_string()];
        let barrier = build_barrier(&running, &pending_names);
        assert!(barrier.contains("r1"));
        assert!(barrier.contains("p1"));
    }
}
```

### Step 2: Run to verify it fails

```bash
cargo test --offline coordinator 2>&1 | head -20
```
Expected: compile error.

### Step 3: Implement `src/coordinator.rs`

```rust
use crate::{
    config::TaskDef,
    facts::FactsStore,
    meta::{self, TaskMeta, TaskStatus},
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
    /// All names that must be in `completed` before this task can start.
    barrier: HashSet<String>,
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
        let (tx, rx) = mpsc::channel(256);
        (
            Self {
                run_dir: run_dir.to_path_buf(),
                runner,
                cancel,
                dispatch_rx: rx,
            },
            tx,
        )
    }

    pub async fn run(mut self, initial_tasks: Vec<TaskDef>) -> bool {
        let mut all_ok = true;
        let mut pending: VecDeque<PendingTask> = VecDeque::new();
        let mut accepted: HashSet<String> = HashSet::new();
        let mut running: HashSet<String> = HashSet::new();
        let mut completed: HashSet<String> = HashSet::new();
        let mut failed = false;
        let mut sealed = false;
        let mut join_set: JoinSet<TaskRunReport> = JoinSet::new();
        let log_dir = self.run_dir.join("logs");
        let facts_store = FactsStore::new(&self.run_dir);

        // Accept initial static tasks — no barrier; they all start immediately.
        for task in initial_tasks {
            accepted.insert(task.name.clone());
            pending.push_back(PendingTask { def: task, barrier: HashSet::new() });
        }

        loop {
            // Spawn any newly unblocked tasks.
            if !failed && !self.cancel.is_cancelled() {
                let mut still_pending = VecDeque::new();
                for mut pt in pending.drain(..) {
                    if pt.barrier.iter().all(|b| completed.contains(b)) {
                        // Inject shared context preamble at dispatch time.
                        if let Ok(Some(preamble)) = facts_store.build_preamble() {
                            pt.def.prompt =
                                format!("{}\n\n{}", preamble, pt.def.prompt);
                        }
                        running.insert(pt.def.name.clone());
                        let r = self.runner.clone();
                        let c = self.cancel.clone();
                        join_set
                            .spawn(async move { r.run_task(pt.def, c, None).await });
                    } else {
                        still_pending.push_back(pt);
                    }
                }
                pending = still_pending;
            }

            let done = join_set.is_empty()
                && pending.is_empty()
                && (sealed || self.dispatch_rx.is_closed());
            if done {
                break;
            }

            tokio::select! {
                biased;

                result = join_set.join_next(), if !join_set.is_empty() => {
                    if let Some(Ok(report)) = result {
                        running.remove(&report.name);
                        completed.insert(report.name.clone());
                        match report.status {
                            TaskStatus::Failed => {
                                failed = true;
                                all_ok = false;
                                self.cancel.cancel();
                            }
                            TaskStatus::Cancelled => { all_ok = false; }
                            _ => {}
                        }
                    }
                }

                msg = self.dispatch_rx.recv(), if !sealed => {
                    match msg {
                        Some(DispatchRequest::Task(task)) => {
                            let barrier =
                                build_barrier(&running, &pending.iter().map(|p| p.def.name.clone()).collect::<Vec<_>>());
                            accepted.insert(task.name.clone());
                            pending.push_back(PendingTask { def: task, barrier });
                        }
                        Some(DispatchRequest::Wave(tasks)) => {
                            let barrier =
                                build_barrier(&running, &pending.iter().map(|p| p.def.name.clone()).collect::<Vec<_>>());
                            for task in tasks {
                                accepted.insert(task.name.clone());
                                pending.push_back(PendingTask {
                                    def: task,
                                    barrier: barrier.clone(),
                                });
                            }
                        }
                        Some(DispatchRequest::Seal) | None => {
                            sealed = true;
                        }
                    }
                }

                _ = self.cancel.cancelled() => {
                    // Mark all queued-but-unstarted tasks as Cancelled.
                    for pt in pending.drain(..) {
                        let meta_path =
                            log_dir.join(format!("{}.meta.json", pt.def.name));
                        let mut m = TaskMeta::load(&meta_path).unwrap_or_else(|_| {
                            TaskMeta::new(
                                &pt.def.name,
                                &pt.def.cwd.to_string_lossy(),
                                &pt.def.prompt,
                            )
                        });
                        m.status = TaskStatus::Cancelled;
                        m.error =
                            Some("run cancelled before task started".into());
                        m.end_time = Some(chrono::Local::now());
                        let _ = m.save(&log_dir);
                    }
                    all_ok = false;
                    sealed = true;
                }
            }
        }

        all_ok
    }

    pub fn accepted_names(&self) -> HashSet<String> {
        HashSet::new() // placeholder — actual tracking in run()
    }
}

// ── Public validation helpers (used by serve.rs) ────────────────────────────

pub fn validate_dispatch_name(
    name: &str,
    accepted: &HashSet<String>,
) -> Result<()> {
    crate::commands::serve::validate_task_name(name)?;
    anyhow::ensure!(
        !accepted.contains(name),
        "task name '{}' is already in this run",
        name
    );
    Ok(())
}

pub fn validate_dispatch_deps(
    depends_on: &[String],
    accepted: &HashSet<String>,
) -> Result<()> {
    for dep in depends_on {
        anyhow::ensure!(
            accepted.contains(dep),
            "depends_on references unknown task '{}'; only already-accepted tasks are allowed",
            dep
        );
    }
    Ok(())
}

pub fn build_barrier(
    running: &HashSet<String>,
    pending_names: &[String],
) -> HashSet<String> {
    running
        .iter()
        .cloned()
        .chain(pending_names.iter().cloned())
        .collect()
}
```

### Step 4: Run tests

```bash
cargo test --offline coordinator 2>&1
```
Expected: all `coordinator::tests::*` pass.

### Step 5: Commit

```bash
git add src/coordinator.rs
git commit -m "feat: add RunCoordinator for dynamic task dispatch"
```

---

## Task 3: Update `src/meta.rs` — Add `dispatched_at`

**Files:**
- Modify: `src/meta.rs:41-67` (TaskMeta struct)
- Modify: `src/meta.rs:100-124` (TaskMeta::new)

### Step 1: Write failing test

Add to `src/meta.rs` `#[cfg(test)]` (create one if absent):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn taskmeta_dispatched_at_round_trips() {
        let dir = TempDir::new().unwrap();
        let mut m = TaskMeta::new("t", "/tmp", "p");
        m.dispatched_at = Some(Local::now());
        m.save(dir.path()).unwrap();
        let path = dir.path().join("t.meta.json");
        let loaded = TaskMeta::load(&path).unwrap();
        assert!(loaded.dispatched_at.is_some());
    }

    #[test]
    fn taskmeta_dispatched_at_defaults_none() {
        let dir = TempDir::new().unwrap();
        let m = TaskMeta::new("t", "/tmp", "p");
        m.save(dir.path()).unwrap();
        let path = dir.path().join("t.meta.json");
        let loaded = TaskMeta::load(&path).unwrap();
        assert!(loaded.dispatched_at.is_none());
    }
}
```

### Step 2: Run to verify it fails

```bash
cargo test --offline meta::tests 2>&1 | head -20
```
Expected: compile error — `dispatched_at` field not found.

### Step 3: Add field to TaskMeta struct and `new()`

In `src/meta.rs`, add after the `session_id` field (line ~52):
```rust
    #[serde(default)]
    pub dispatched_at: Option<DateTime<Local>>,
```

In `TaskMeta::new()` add:
```rust
            dispatched_at: None,
```

### Step 4: Run tests

```bash
cargo test --offline meta 2>&1
```
Expected: all pass.

### Step 5: Commit

```bash
git add src/meta.rs
git commit -m "feat: add dispatched_at to TaskMeta for dynamic tasks"
```

---

## Task 4: Wire into `src/main.rs` and `src/commands/serve.rs`

**Files:**
- Modify: `src/main.rs` — add `mod facts; mod coordinator;`
- Modify: `src/commands/serve.rs` — `ActiveRun`, `start_run`, 5 new tools, param structs

### Step 1: Write failing tests for new MCP tools

Add to the `#[cfg(test)]` block in `src/commands/serve.rs`:

```rust
    #[test]
    fn facts_store_write_and_read() {
        let dir = TempDir::new().unwrap();
        let store = crate::facts::FactsStore::new(dir.path());
        store.write_fact("env", "prod".to_string(), None).unwrap();
        let facts = store.read_all().unwrap();
        assert_eq!(facts["env"].value, "prod");
    }

    #[test]
    fn validate_dispatch_name_rejects_duplicate() {
        use crate::coordinator::validate_dispatch_name;
        use std::collections::HashSet;
        let mut accepted = HashSet::new();
        accepted.insert("existing".to_string());
        assert!(validate_dispatch_name("existing", &accepted).is_err());
        assert!(validate_dispatch_name("new_task", &accepted).is_ok());
    }
```

### Step 2: Add `mod` declarations to `src/main.rs`

Find the existing mod declarations and add:
```rust
mod coordinator;
mod facts;
```

### Step 3: Add new param/result structs to `src/commands/serve.rs`

Add after the existing `ListMailboxParams` struct (~line 101):

```rust
#[derive(Debug, Deserialize, JsonSchema)]
struct DispatchTaskParams {
    /// Base directory passed to start_run
    run_dir: String,
    /// Task name ([A-Za-z0-9._-])
    name: String,
    /// Working directory for the Codex process
    cwd: String,
    /// Task prompt
    prompt: String,
    /// read-only | read-write | network-read-only (default: read-only)
    #[serde(default)]
    sandbox: Option<String>,
    /// Override Codex model
    #[serde(default)]
    model: Option<String>,
    /// Names of already-accepted tasks this task waits for (no forward refs)
    #[serde(default)]
    depends_on: Vec<String>,
    /// Agent identity label (default: task name)
    #[serde(default)]
    agent_id: Option<String>,
    /// Thread identity label (default: task name)
    #[serde(default)]
    thread_id: Option<String>,
    /// -a flag: never | on-request | untrusted (default: never)
    #[serde(default)]
    ask_for_approval: Option<String>,
    /// -c key=value overrides (repeated)
    #[serde(default)]
    config_overrides: Vec<String>,
    /// -p profile name
    #[serde(default)]
    profile: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DispatchWaveParams {
    /// Base directory passed to start_run
    run_dir: String,
    /// Batch of tasks to dispatch atomically
    tasks: Vec<DispatchTaskParams>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteFactParams {
    /// Base directory passed to start_run
    run_dir: String,
    /// Fact key
    key: String,
    /// Inline fact value (≤ 1 KiB; truncated if exceeded)
    value: String,
    /// Optional path to a large artifact file (value becomes a summary)
    #[serde(default)]
    artifact_path: Option<String>,
}

#[derive(Debug, Serialize)]
struct DispatchResult {
    /// "dispatched"
    state: String,
    task_count: usize,
}

#[derive(Debug, Serialize)]
struct SealResult {
    /// "sealed" | "not_found" | "already_sealed"
    state: String,
}

#[derive(Debug, Serialize)]
struct WriteFactResult {
    /// "written"
    state: String,
}
```

### Step 4: Add `dispatch_tx` to `ActiveRun`

Change `ActiveRun` struct (~line 163):
```rust
struct ActiveRun {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
    dispatch_tx: tokio::sync::mpsc::Sender<crate::coordinator::DispatchRequest>,
    accepted: std::sync::Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
}
```

### Step 5: Update `start_run` to use `RunCoordinator`

Replace the `tokio::spawn` block at lines ~298-310 in `start_run` with:

```rust
        let all_tasks: Vec<_> = waves.into_iter().flatten().collect();
        let accepted_names: std::collections::HashSet<String> =
            all_tasks.iter().map(|t| t.name.clone()).collect();
        let accepted = std::sync::Arc::new(tokio::sync::Mutex::new(accepted_names));

        let runner = TaskRunner::new(&run_dir)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let cancel = CancellationToken::new();
        let (coord, dispatch_tx) =
            crate::coordinator::RunCoordinator::start(&run_dir, runner, cancel.clone());
        let accepted_bg = Arc::clone(&accepted);
        let run_dir_bg = run_dir.clone();
        let state = Arc::clone(&self.state);

        let handle = tokio::spawn(async move {
            let all_ok = coord.run(all_tasks).await;
            if let Ok(mut rm) = meta::RunMeta::load(&run_dir_bg) {
                rm.status = if all_ok {
                    meta::RunStatus::Done
                } else {
                    rm.error = collect_first_task_error(&run_dir_bg.join("logs"));
                    meta::RunStatus::Failed
                };
                let _ = rm.save(&run_dir_bg);
            }
            state.runs.lock().await.remove(&run_dir_bg);
        });

        runs.insert(
            run_dir.clone(),
            ActiveRun { cancel, handle, dispatch_tx, accepted },
        );
```

Also update `StartRunResult` to remove `wave_count` from the response or set it to 0 for coordinator-based runs (initial static tasks are still counted in `task_count`).

### Step 6: Add the five new MCP tools

Add inside the `#[tool_router] impl CodexParServer` block:

```rust
    #[tool(description = "Add a single task to an active unsealed run. depends_on may only reference tasks already in this run.")]
    async fn dispatch_task(&self, p: Parameters<DispatchTaskParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let run_dir = PathBuf::from(&p.run_dir).canonicalize()
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        validate_task_name(&p.name)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let runs = self.state.runs.lock().await;
        let active = runs.get(&run_dir)
            .ok_or_else(|| McpError::invalid_params("run not found or already sealed".into(), None))?;

        let mut accepted = active.accepted.lock().await;
        crate::coordinator::validate_dispatch_name(&p.name, &accepted)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        crate::coordinator::validate_dispatch_deps(&p.depends_on, &accepted)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let task = params_to_task_def(p)?;
        accepted.insert(task.name.clone());
        drop(accepted);

        active.dispatch_tx.send(crate::coordinator::DispatchRequest::Task(task)).await
            .map_err(|_| McpError::invalid_params("run dispatch channel closed (already sealed)".into(), None))?;

        ok_json(&DispatchResult { state: "dispatched".into(), task_count: 1 })
    }

    #[tool(description = "Add a batch of tasks to an active unsealed run. Intra-batch forward depends_on references are allowed.")]
    async fn dispatch_wave(&self, p: Parameters<DispatchWaveParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let run_dir = PathBuf::from(&p.run_dir).canonicalize()
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let runs = self.state.runs.lock().await;
        let active = runs.get(&run_dir)
            .ok_or_else(|| McpError::invalid_params("run not found or already sealed".into(), None))?;

        let mut accepted = active.accepted.lock().await;
        // Validate all names first (atomically)
        let batch_names: std::collections::HashSet<String> =
            p.tasks.iter().map(|t| t.name.clone()).collect();
        for task_p in &p.tasks {
            validate_task_name(&task_p.name)
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
            // Duplicate check against existing (not within batch — that's caught by HashSet size)
            if accepted.contains(&task_p.name) {
                return Err(McpError::invalid_params(
                    format!("task name '{}' already in run", task_p.name), None));
            }
            // deps must be in existing accepted OR within batch
            for dep in &task_p.depends_on {
                if !accepted.contains(dep) && !batch_names.contains(dep) {
                    return Err(McpError::invalid_params(
                        format!("depends_on '{}' not found in run or batch", dep), None));
                }
            }
        }
        let count = p.tasks.len();
        for tp in &p.tasks { accepted.insert(tp.name.clone()); }
        drop(accepted);

        let tasks: Vec<_> = p.tasks.into_iter()
            .map(params_to_task_def)
            .collect::<Result<_, _>>()
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        active.dispatch_tx.send(crate::coordinator::DispatchRequest::Wave(tasks)).await
            .map_err(|_| McpError::invalid_params("run dispatch channel closed".into(), None))?;

        ok_json(&DispatchResult { state: "dispatched".into(), task_count: count })
    }

    #[tool(description = "Seal a run's dispatch channel. The run finalizes when all in-flight tasks finish. Required to end a dynamically-dispatched run.")]
    async fn seal_dispatch(&self, p: Parameters<RunDirParam>) -> Result<CallToolResult, McpError> {
        let run_dir = PathBuf::from(&p.0.run_dir).canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&p.0.run_dir));
        let runs = self.state.runs.lock().await;
        let state_str = if let Some(active) = runs.get(&run_dir) {
            let _ = active.dispatch_tx.send(
                crate::coordinator::DispatchRequest::Seal).await;
            "sealed"
        } else {
            "not_found"
        };
        ok_json(&SealResult { state: state_str.into() })
    }

    #[tool(description = "Write or overwrite a shared fact for a run. Values > 1 KiB are truncated. Use artifact_path for large data.")]
    async fn write_fact(&self, p: Parameters<WriteFactParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        anyhow::ensure!(!p.key.trim().is_empty(), "key cannot be empty");
        let run_dir = PathBuf::from(&p.run_dir);
        let store = crate::facts::FactsStore::new(&run_dir);
        let artifact_path = p.artifact_path.map(PathBuf::from);
        store.write_fact(&p.key, p.value, artifact_path)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        ok_json(&WriteFactResult { state: "written".into() })
    }

    #[tool(description = "Read all shared facts for a run from shared/facts.json.")]
    async fn read_facts(&self, p: Parameters<RunDirParam>) -> Result<CallToolResult, McpError> {
        let run_dir = PathBuf::from(&p.0.run_dir);
        let store = crate::facts::FactsStore::new(&run_dir);
        let facts = store.read_all()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        ok_json(&facts)
    }
```

### Step 7: Add `params_to_task_def` helper

Add to the helpers section at the bottom of `serve.rs`:

```rust
fn params_to_task_def(p: DispatchTaskParams) -> anyhow::Result<crate::config::TaskDef> {
    use crate::config::{Sandbox, TaskDef};
    let sandbox = match p.sandbox.as_deref().unwrap_or("read-only") {
        "read-write" => Sandbox::ReadWrite,
        "network-read-only" => Sandbox::NetworkReadOnly,
        _ => Sandbox::ReadOnly,
    };
    Ok(TaskDef {
        name: p.name,
        cwd: std::path::PathBuf::from(p.cwd),
        prompt: p.prompt,
        sandbox,
        agent_id: p.agent_id,
        thread_id: p.thread_id,
        output_schema: None,
        ephemeral: false,
        add_dirs: vec![],
        model: p.model,
        depends_on: p.depends_on,
        ask_for_approval: p.ask_for_approval.unwrap_or_else(|| "never".to_string()),
        config_overrides: p.config_overrides,
        profile: p.profile,
    })
}
```

### Step 8: Update `cancel_run` to also seal the channel

In the existing `cancel_run` tool handler, after `active.cancel.cancel()`, add:
```rust
let _ = active.dispatch_tx.try_send(crate::coordinator::DispatchRequest::Seal);
```

### Step 9: Build and run tests

```bash
cargo check --offline 2>&1
cargo test --offline 2>&1
```
Fix any compile errors. Expected: all existing tests pass plus new ones.

### Step 10: Commit

```bash
git add src/main.rs src/commands/serve.rs
git commit -m "feat: wire RunCoordinator + 5 new MCP tools into serve (dispatch_task, dispatch_wave, seal_dispatch, write_fact, read_facts)"
```

---

## Task 5: Update MCP tool list test

**Files:**
- Modify: `tests/mcp_behavior_test.rs`

### Step 1: Check current tool list test

```bash
grep -n "tool" tests/mcp_behavior_test.rs | head -30
```

### Step 2: Add new tools to the expected list

Find the existing expected tools slice and add:
- `"dispatch_task"`
- `"dispatch_wave"`
- `"seal_dispatch"`
- `"write_fact"`
- `"read_facts"`

### Step 3: Run

```bash
cargo test --offline mcp_behavior 2>&1
```
Expected: pass.

### Step 4: Commit

```bash
git add tests/mcp_behavior_test.rs
git commit -m "test: add Phase 2 MCP tools to tool-list test"
```

---

## Task 6: Build Release Binary

```bash
cargo build --release 2>&1
```
Expected: no errors, only pre-existing warnings.

```bash
git add -A && git commit -m "chore: rebuild release binary for Phase 2"
```

Restart Claude Code to reload MCP server with the new binary.

---

## Verification Checklist

After all tasks:
- [ ] `cargo check --offline` clean
- [ ] `cargo test --offline` all pass
- [ ] `target/release/codex-par` rebuilt
- [ ] MCP tool list includes `dispatch_task`, `dispatch_wave`, `seal_dispatch`, `write_fact`, `read_facts`
- [ ] Can call `write_fact` then `read_facts` on a run_dir and see values
- [ ] `start_run` → `dispatch_task` → `seal_dispatch` → `get_status` shows dynamic task completing
- [ ] `cancel_run` still works and cancels both running and queued dynamic tasks
