# depends_on + Wave Scheduling Implementation Plan

> **Status:** IMPLEMENTED (v2 with Codex review fixes applied)

**Goal:** Add `depends_on` field to task definitions so codex-par can topologically sort tasks into waves, executing each wave in parallel while enforcing inter-wave ordering.

**Architecture:** Add an optional `depends_on: Vec<String>` to `TaskDef`. A new `into_waves()` method on `TasksConfig` uses Kahn's algorithm (index-keyed for order preservation) to partition tasks into waves. The `Run` command iterates waves sequentially, spawning a `JoinSet` per wave. If any task in a wave fails, subsequent waves are skipped. Dashboard and meta are updated to show wave membership.

**Tech Stack:** Rust, serde, tokio JoinSet, Kahn's algorithm (no new dependencies)

## Codex Review Fixes Applied

1. **Backward compat (meta.rs):** Added `#[serde(default)]` on `wave` field so old meta.json files still parse
2. **Split interrupt flags (main.rs):** `wave_failed` vs `sigint_received` — correct cancellation reasons
3. **Pre-created Pending metas (main.rs):** All tasks get Pending meta before execution starts — dashboard shows full pipeline
4. **Order preservation (config.rs):** `into_waves()` uses index-keyed HashMap, sorts ready indices by original position
5. **Duplicate validation (config.rs):** Moved into `into_waves()` itself
6. **Stdout-read error (runner.rs):** Error no longer overwritten by exit code; propagated in report
7. **Ctrl+C race (runner.rs):** `biased` select prefers stdout EOF over cancel to avoid misclassification
8. **Panic handling (main.rs):** JoinError produces synthetic failed report
9. **Dashboard order (dashboard.rs):** Removed alphabetical sort, preserves meta creation order
10. **CLI help (main.rs):** Updated to "Launch tasks in dependency waves"
11. **Edge case tests (config.rs):** Added empty list, self-dep, duplicate names, later-wave order tests

---

### Task 1: Add `depends_on` field to `TaskDef`

**Files:**
- Modify: `src/config.rs:8-16`

**Step 1: Add the field to TaskDef**

```rust
#[derive(Debug, Deserialize, Clone)]
pub struct TaskDef {
    pub name: String,
    pub cwd: String,
    pub prompt: String,
    #[serde(default = "default_sandbox")]
    pub sandbox: String,
    pub model: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
}
```

**Step 2: Verify it compiles and existing YAML still works**

Run: `cargo build 2>&1`
Expected: compiles with same warnings as before (no new warnings)

**Step 3: Commit**

```bash
git add src/config.rs
git commit -m "feat: add depends_on field to TaskDef"
```

---

### Task 2: Implement `into_waves()` with Kahn's algorithm

**Files:**
- Modify: `src/config.rs` (add method to `TasksConfig`)

**Step 1: Write the `into_waves()` method**

Add this to the `impl TasksConfig` block after the `load` method:

```rust
/// Partition tasks into execution waves using topological sort (Kahn's algorithm).
///
/// Returns `Vec<Vec<TaskDef>>` where wave[0] has no dependencies, wave[1] depends
/// only on wave[0] tasks, etc.
///
/// Errors:
/// - Unknown task name in depends_on
/// - Circular dependency detected
pub fn into_waves(self) -> anyhow::Result<Vec<Vec<TaskDef>>> {
    use std::collections::{HashMap, HashSet};

    let task_names: HashSet<&str> = self.tasks.iter().map(|t| t.name.as_str()).collect();

    // Validate all depends_on references exist
    for task in &self.tasks {
        for dep in &task.depends_on {
            anyhow::ensure!(
                task_names.contains(dep.as_str()),
                "task '{}' depends on unknown task '{}'",
                task.name,
                dep
            );
        }
    }

    let mut remaining: HashMap<String, TaskDef> = self
        .tasks
        .into_iter()
        .map(|t| (t.name.clone(), t))
        .collect();
    let mut placed: HashSet<String> = HashSet::new();
    let mut waves: Vec<Vec<TaskDef>> = Vec::new();

    while !remaining.is_empty() {
        let ready: Vec<String> = remaining
            .iter()
            .filter(|(_, t)| t.depends_on.iter().all(|d| placed.contains(d)))
            .map(|(name, _)| name.clone())
            .collect();

        anyhow::ensure!(
            !ready.is_empty(),
            "circular dependency detected among tasks: {:?}",
            remaining.keys().collect::<Vec<_>>()
        );

        let mut wave = Vec::new();
        for name in &ready {
            placed.insert(name.clone());
            wave.push(remaining.remove(name).unwrap());
        }
        waves.push(wave);
    }

    Ok(waves)
}
```

**Step 2: Verify it compiles**

Run: `cargo build 2>&1`
Expected: compiles successfully

**Step 3: Commit**

```bash
git add src/config.rs
git commit -m "feat: implement into_waves() topological sort"
```

---

### Task 3: Add unit tests for `into_waves()`

**Files:**
- Modify: `src/config.rs` (add `#[cfg(test)]` module at bottom)

**Step 1: Write tests**

Add at the bottom of `src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn task(name: &str, depends_on: Vec<&str>) -> TaskDef {
        TaskDef {
            name: name.to_string(),
            cwd: "/tmp".to_string(),
            prompt: "test".to_string(),
            sandbox: "read-only".to_string(),
            model: None,
            depends_on: depends_on.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn no_dependencies_single_wave() {
        let config = TasksConfig {
            tasks: vec![task("a", vec![]), task("b", vec![]), task("c", vec![])],
        };
        let waves = config.into_waves().unwrap();
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].len(), 3);
    }

    #[test]
    fn linear_chain_three_waves() {
        let config = TasksConfig {
            tasks: vec![
                task("a", vec![]),
                task("b", vec!["a"]),
                task("c", vec!["b"]),
            ],
        };
        let waves = config.into_waves().unwrap();
        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0].len(), 1);
        assert_eq!(waves[0][0].name, "a");
        assert_eq!(waves[1][0].name, "b");
        assert_eq!(waves[2][0].name, "c");
    }

    #[test]
    fn diamond_dependency() {
        // a -> b, a -> c, b+c -> d
        let config = TasksConfig {
            tasks: vec![
                task("a", vec![]),
                task("b", vec!["a"]),
                task("c", vec!["a"]),
                task("d", vec!["b", "c"]),
            ],
        };
        let waves = config.into_waves().unwrap();
        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0].len(), 1); // a
        assert_eq!(waves[1].len(), 2); // b, c
        assert_eq!(waves[2].len(), 1); // d
        assert_eq!(waves[2][0].name, "d");
    }

    #[test]
    fn circular_dependency_error() {
        let config = TasksConfig {
            tasks: vec![task("a", vec!["b"]), task("b", vec!["a"])],
        };
        let err = config.into_waves().unwrap_err();
        assert!(err.to_string().contains("circular dependency"));
    }

    #[test]
    fn unknown_dependency_error() {
        let config = TasksConfig {
            tasks: vec![task("a", vec!["nonexistent"])],
        };
        let err = config.into_waves().unwrap_err();
        assert!(err.to_string().contains("unknown task"));
    }

    #[test]
    fn backward_compat_no_depends_on() {
        // Simulates YAML without depends_on — Vec<String> defaults to empty
        let yaml = r#"
tasks:
  - name: "x"
    cwd: "/tmp"
    prompt: "hello"
"#;
        let config: TasksConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.tasks[0].depends_on.is_empty());
        let waves = config.into_waves().unwrap();
        assert_eq!(waves.len(), 1);
    }
}
```

**Step 2: Run tests**

Run: `cargo test 2>&1`
Expected: all 6 tests pass

**Step 3: Commit**

```bash
git add src/config.rs
git commit -m "test: add unit tests for into_waves() topological sort"
```

---

### Task 4: Add `wave` field to `TaskMeta`

**Files:**
- Modify: `src/meta.rs:6-21` (add field to struct)
- Modify: `src/meta.rs:53-71` (update `new()`)

**Step 1: Add `wave` field to `TaskMeta`**

Add after the `name` field:

```rust
pub wave: Option<u32>,
```

Update `TaskMeta::new()` to include:

```rust
wave: None,
```

**Step 2: Verify it compiles**

Run: `cargo build 2>&1`
Expected: compiles

**Step 3: Commit**

```bash
git add src/meta.rs
git commit -m "feat: add wave field to TaskMeta"
```

---

### Task 5: Wave-based execution in `main.rs`

**Files:**
- Modify: `src/main.rs:57-151` (replace flat JoinSet with wave loop)

**Step 1: Update the Run command handler**

Replace the Run handler (from `let runner =` through the summary section) with wave-based execution:

```rust
Commands::Run { config, dashboard, dir } => {
    let base = PathBuf::from(&dir);
    let tasks_config = config::TasksConfig::load(&config)?;

    // Validate: no duplicate task names
    let mut seen = std::collections::HashSet::new();
    for task in &tasks_config.tasks {
        anyhow::ensure!(
            seen.insert(&task.name),
            "duplicate task name: {}",
            task.name
        );
    }

    // Topological sort into waves
    let waves = tasks_config.into_waves()?;
    let total_tasks: usize = waves.iter().map(|w| w.len()).sum();

    let runner = runner::TaskRunner::new(&base)?;
    let shutdown = CancellationToken::new();
    let log_dir = runner.log_dir().to_path_buf();

    // Optionally spawn dashboard
    let dashboard_handle = if dashboard {
        let ld = log_dir.clone();
        let cancel = shutdown.clone();
        Some(tokio::spawn(async move {
            dashboard::watch_until_cancelled(&ld, 1, cancel).await
        }))
    } else {
        None
    };

    let mut all_reports = Vec::new();
    let mut interrupted = false;

    for (wave_idx, wave) in waves.into_iter().enumerate() {
        if interrupted {
            // Mark remaining tasks as cancelled
            for task in &wave {
                let mut meta = meta::TaskMeta::new(&task.name, &task.cwd, &task.prompt);
                meta.wave = Some(wave_idx as u32);
                meta.status = meta::TaskStatus::Cancelled;
                meta.error = Some("skipped: previous wave had failures".into());
                meta.end_time = Some(chrono::Local::now());
                let _ = meta.save(&log_dir);
                all_reports.push(runner::TaskRunReport {
                    name: task.name.clone(),
                    status: meta::TaskStatus::Cancelled,
                    error: meta.error.clone(),
                });
            }
            continue;
        }

        let wave_size = wave.len();
        println!(
            "-- Wave {} ({} task{}, parallel) --",
            wave_idx,
            wave_size,
            if wave_size == 1 { "" } else { "s" }
        );

        let mut set = JoinSet::new();
        for task in wave {
            let runner = runner.clone();
            let cancel = shutdown.clone();
            let w = wave_idx as u32;
            set.spawn(async move {
                runner.run_task(task, cancel, Some(w)).await
            });
        }

        // Collect results for this wave
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c(), if !interrupted => {
                    interrupted = true;
                    eprintln!("\nReceived Ctrl+C, cancelling all tasks...");
                    shutdown.cancel();
                }
                result = set.join_next() => {
                    match result {
                        Some(Ok(report)) => {
                            if report.status == meta::TaskStatus::Failed {
                                interrupted = true;
                            }
                            all_reports.push(report);
                        }
                        Some(Err(e)) => {
                            eprintln!("Task panicked: {}", e);
                            interrupted = true;
                        }
                        None => break, // All tasks in this wave done
                    }
                }
            }
        }
    }

    // Wait for dashboard to finish
    if let Some(handle) = dashboard_handle {
        shutdown.cancel(); // signal dashboard to stop
        handle.await.ok();
    }

    // Print summary
    println!("\n{}", "=".repeat(50));
    println!("  SUMMARY ({} tasks)", total_tasks);
    println!("{}", "=".repeat(50));
    for report in &all_reports {
        let icon = match report.status {
            meta::TaskStatus::Done => "\x1b[32m✓\x1b[0m",
            meta::TaskStatus::Failed => "\x1b[31m✗\x1b[0m",
            meta::TaskStatus::Cancelled => "\x1b[35m⊘\x1b[0m",
            _ => "?",
        };
        print!("  {} {}", icon, report.name);
        if let Some(ref err) = report.error {
            print!("  -- {}", err);
        }
        println!();
    }
    println!("\n  Results: {}/outputs/*.md", dir);

    let all_ok = all_reports.iter().all(|r| r.status == meta::TaskStatus::Done);
    if !all_ok {
        std::process::exit(1);
    }
}
```

**Step 2: Update `TaskRunner::run_task` signature to accept wave**

In `src/runner.rs`, update `run_task`:

```rust
pub async fn run_task(&self, task: TaskDef, cancel: CancellationToken, wave: Option<u32>) -> TaskRunReport {
    let mut meta = TaskMeta::new(&task.name, &task.cwd, &task.prompt);
    meta.wave = wave;
    meta.status = TaskStatus::Running;
    // ... rest unchanged
```

**Step 3: Verify it compiles**

Run: `cargo build 2>&1`
Expected: compiles

**Step 4: Commit**

```bash
git add src/main.rs src/runner.rs
git commit -m "feat: wave-based execution with failure propagation"
```

---

### Task 6: Update dashboard to show wave info

**Files:**
- Modify: `src/dashboard.rs:27-57` (add wave grouping to render)

**Step 1: Update `render_once` to group tasks by wave**

Replace the task rendering loop in `render_once` (after the header) with:

```rust
// Group by wave
let mut wave_groups: std::collections::BTreeMap<u32, Vec<&TaskMeta>> = std::collections::BTreeMap::new();
let mut no_wave: Vec<&TaskMeta> = Vec::new();
for meta in &metas {
    match meta.wave {
        Some(w) => wave_groups.entry(w).or_default().push(meta),
        None => no_wave.push(meta),
    }
}

// Render tasks without wave info (v1 compat)
for meta in &no_wave {
    render_task_line(&mut out, meta)?;
}

// Render wave groups
for (wave_idx, tasks) in &wave_groups {
    let wave_done = tasks.iter().filter(|m| m.status == TaskStatus::Done).count();
    writeln!(out, "  -- Wave {} ({}/{} done) --", wave_idx, wave_done, tasks.len())?;
    for meta in tasks {
        render_task_line(&mut out, meta)?;
    }
    writeln!(out)?;
}
```

Extract a `render_task_line` helper from the existing rendering code:

```rust
fn render_task_line(out: &mut impl Write, meta: &TaskMeta) -> Result<()> {
    let icon = match meta.status {
        TaskStatus::Running => "\x1b[33m⟳\x1b[0m",
        TaskStatus::Done => "\x1b[32m✓\x1b[0m",
        TaskStatus::Failed => "\x1b[31m✗\x1b[0m",
        TaskStatus::Pending => "\x1b[90m◯\x1b[0m",
        TaskStatus::Cancelled => "\x1b[35m⊘\x1b[0m",
    };

    let duration = if let Some(end) = meta.end_time {
        let dur = end - meta.start_time;
        format!("{}m{}s", dur.num_minutes(), dur.num_seconds() % 60)
    } else if meta.status == TaskStatus::Running {
        let dur = chrono::Local::now() - meta.start_time;
        format!("{}m{}s", dur.num_minutes(), dur.num_seconds() % 60)
    } else {
        "-".to_string()
    };

    let tokens_k = meta.input_tokens as f64 / 1000.0;

    writeln!(out, "    {} {}", icon, meta.name)?;
    writeln!(out, "      Status: {:<10} Duration: {:<10} Events: {}", meta.status, duration, meta.events_count)?;
    writeln!(out, "      Tokens: {:.0}K in / {:.0}K out    Files: {}  Cmds: {}",
        tokens_k, meta.output_tokens as f64 / 1000.0, meta.files_read, meta.commands_run)?;
    writeln!(out, "      Last: {}", truncate_utf8(&meta.last_action, 60))?;
    if let Some(ref err) = meta.error {
        writeln!(out, "      \x1b[31mError: {}\x1b[0m", truncate_utf8(err, 60))?;
    }
    writeln!(out)?;
    Ok(())
}
```

**Step 2: Verify it compiles**

Run: `cargo build 2>&1`
Expected: compiles

**Step 3: Commit**

```bash
git add src/dashboard.rs
git commit -m "feat: dashboard groups tasks by wave"
```

---

### Task 7: Add example YAML with depends_on

**Files:**
- Create: `tasks_pipeline.example.yaml`

**Step 1: Write the example**

```yaml
# Example: multi-wave pipeline with depends_on
# Wave 0: Three parallel analysis tasks
# Wave 1: Cross-review depends on all three

tasks:
  - name: "codex_fee_calculator"
    cwd: "/path/to/project"
    sandbox: "read-only"
    prompt: |
      Analyze the fee calculation logic and report findings.

  - name: "codex_retry_job"
    cwd: "/path/to/project"
    sandbox: "read-only"
    prompt: |
      Analyze the retry job failure recovery paths.

  - name: "codex_bff_judgement"
    cwd: "/path/to/project"
    sandbox: "read-only"
    prompt: |
      Analyze the business judgement logic.

  - name: "cross_review"
    depends_on:
      - "codex_fee_calculator"
      - "codex_retry_job"
      - "codex_bff_judgement"
    cwd: "/path/to/project"
    sandbox: "read-only"
    prompt: |
      Read outputs/ and cross-review all three analysis reports.
      Find contradictions, systemic risks, and prioritize fixes.
```

**Step 2: Verify it parses correctly**

Run: `cargo run -- run tasks_pipeline.example.yaml 2>&1 | head -5`
Expected: prints "-- Wave 0 (3 tasks, parallel) --" (will fail on actual codex exec, but parsing works)

**Step 3: Commit**

```bash
git add tasks_pipeline.example.yaml
git commit -m "docs: add pipeline example YAML with depends_on"
```

---

### Task 8: Final integration build and test

**Step 1: Run all tests**

Run: `cargo test 2>&1`
Expected: all tests pass

**Step 2: Build release**

Run: `cargo build --release 2>&1`
Expected: compiles without errors

**Step 3: Verify backward compatibility with existing example**

Run: `cargo run -- run tasks.example.yaml 2>&1 | head -5`
Expected: prints "-- Wave 0 (3 tasks, parallel) --" (all tasks in wave 0 since no depends_on)

**Step 4: Final commit if any fixups needed**
