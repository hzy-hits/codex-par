# codex-par Design Document

## Problem Statement

Claude Code calls Codex via MCP (Model Context Protocol) for code analysis tasks. Two critical issues exist:

1. **Serial Execution**: All Codex MCP calls from Claude are serialized — even when using multiple team agents, they share one MCP server and queue up. A 3-task workload that could finish in 30min takes 90min.

2. **MCP Deadlock**: Claude's MCP implementation is half-duplex synchronous blocking. When Claude sends a request to Codex and blocks waiting for the response, it stops processing inbound messages. If Codex needs Claude to respond to a sub-request or notification, both sides deadlock — alive but waiting for the other to move first.

3. **No Process Visibility**: When Codex runs via MCP, all intermediate events (files read, commands executed, reasoning steps) are invisible. Only the final result is returned. A single task can consume 4.7M+ input tokens over 30+ minutes with zero progress feedback.

## Solution

`codex-par` bypasses the MCP layer entirely. Each task spawns an independent `codex exec --json` process, achieving true process-isolated parallelism with live monitoring.

## Architecture

```
codex-par run tasks.yaml --dashboard

  +-- main ------------------------------------------------+
  |  for wave in waves:                                     |
  |    JoinSet + CancellationToken + tokio::select!         |
  |                                                         |
  |    +-- spawn task1 (wave 0) --------------------------+ |
  |    |  codex exec --json -C /path -s read-only ...     | |
  |    |  +- stdout reader -> JSONL parse -> meta update  | |
  |    |  +- stderr reader -> drain to .stderr.log        | |
  |    |  +- biased select! { stdout, cancel }            | |
  |    +--------------------------------------------------+ |
  |                                                         |
  |    +-- spawn task2 (wave 0) --------------------------+ |
  |    |  (same structure, independent process)            | |
  |    +--------------------------------------------------+ |
  |                                                         |
  |    wave 0 completes -> wave 1 starts                    |
  |                                                         |
  |    +-- spawn task3 (wave 1, depends on wave 0) -------+ |
  |    |  (same structure)                                 | |
  |    +--------------------------------------------------+ |
  |                                                         |
  |  +-- dashboard ----------------------------------------+ |
  |  |  reads logs/*.meta.json every 1s                    | |
  |  |  renders TUI grouped by wave                        | |
  |  +----------------------------------------------------+ |
  |                                                         |
  |  Ctrl+C -> shutdown.cancel() -> SIGTERM to all pgrps    |
  |  Wave failure -> skip subsequent waves                  |
  +---------------------------------------------------------+
```

### Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Parallel model | Independent `codex exec` processes | Bypasses MCP serial bottleneck and deadlock entirely |
| Wave scheduling | Kahn's algorithm topological sort | `depends_on` DAG partitioned into waves; parallel within, serial between |
| Async runtime | tokio JoinSet per wave | Structured concurrency, `join_next()` + `select!` for Ctrl+C |
| Select bias | `biased` select, stdout first | Prevents Ctrl+C from misclassifying completed tasks as cancelled |
| Cancel responsiveness | Inline `is_cancelled()` check | Ensures chatty tasks still respond to Ctrl+C promptly |
| Interrupt flags | Separate `wave_failed` / `sigint_received` | Correct cancellation reasons; Ctrl+C still works after a task failure |
| Pre-created metas | All tasks get Pending meta before execution | Dashboard shows full pipeline from the start |
| Panic recovery | Post-wave meta fixup | JoinError path writes terminal meta to disk, prevents stale Running state |
| Task runner sharing | `TaskRunner: Clone` | Lighter than `Arc`, each spawn gets own copy |
| Inter-process comms | File-based (meta.json) | Dashboard can be a separate command (`status`), not coupled to runner |
| Meta file writes | Atomic `.tmp` + `rename` | Prevents dashboard reading partial JSON |
| Process cleanup | `setpgid(0,0)` per child | Kill entire process group on Ctrl+C, no zombie leaks |
| stderr handling | Separate drain task to file | Prevents pipe buffer deadlock (4KB-64KB buffer fills -> child blocks) |
| Event parsing | Unified `event.rs` | Single source of truth for runner, dashboard, and tail commands |
| Codex binary path | `CODEX_BIN` env var | Defaults to `/opt/homebrew/bin/codex`, portable across machines |
| Meta update frequency | Every 20 events | Balances disk IO cost vs dashboard realtime visibility |
| Order preservation | Index-keyed Kahn's algorithm | Tasks within each wave maintain YAML input order |
| Dashboard sort | `(wave, name)` | Deterministic display across platforms (read_dir order is unspecified) |

## File Structure

```
src/
+-- main.rs        CLI entry, wave scheduling, Ctrl+C, panic recovery, tail follow
+-- config.rs      YAML config parsing, depends_on, into_waves() topological sort
+-- event.rs       Unified CodexEvent enum + JSONL parsing
+-- meta.rs        TaskMeta with wave field, atomic save, TaskStatus enum
+-- runner.rs      TaskRunner::run_task(), biased select, stderr drain, cancel
+-- dashboard.rs   TUI render grouped by wave, watch mode, UTF-8 safe truncate

Runtime generated:
+-- outputs/       {task_name}.md -- final Codex results
+-- logs/          {task_name}.jsonl -- full event stream
                   {task_name}.meta.json -- live status for dashboard
                   {task_name}.stderr.log -- stderr output
```

## Usage

### Task Configuration (YAML)

```yaml
tasks:
  # Wave 0: no dependencies, run in parallel
  - name: "fee_calculator_v2"
    cwd: "/path/to/project"
    sandbox: "read-only"
    prompt: |
      Analyze InsuranceFeeCalculatorV2Facade...

  - name: "retry_job"
    cwd: "/path/to/project"
    sandbox: "read-only"
    model: "gpt-5.4"        # optional model override
    prompt: |
      Analyze InsureAndClaimFailedRetryJob...

  # Wave 1: depends on both Wave 0 tasks
  - name: "cross_review"
    depends_on:
      - "fee_calculator_v2"
      - "retry_job"
    cwd: "/path/to/project"
    sandbox: "read-only"
    prompt: |
      Read outputs/ and cross-review both analysis reports...
```

### Commands

```bash
# Launch tasks in dependency waves with live dashboard
codex-par run tasks.yaml --dashboard

# Just launch (check results later)
codex-par run tasks.yaml

# Monitor running/completed tasks
codex-par status -w 3         # watch mode, refresh every 3s

# Follow a specific task's event stream
codex-par tail fee_calculator_v2

# Clean up
codex-par clean
```

### Dashboard Output

```
========================================================================
  CODEX PARALLEL DASHBOARD  |  19:30:45  |  3/4 done, 1 running, 0 failed
========================================================================

  -- Wave 0 (2/2 done) --
    v fee_calculator_v2
      Status: done       Duration: 12m34s   Events: 303
      Tokens: 1100K in / 15K out    Files: 45  Cmds: 12
      Last: COMPLETED

    v retry_job
      Status: done       Duration: 8m21s    Events: 187
      Tokens: 680K in / 9K out    Files: 28  Cmds: 8
      Last: COMPLETED

  -- Wave 1 (1/2 done) --
    ~ cross_review
      Status: running    Duration: 5m02s    Events: 82
      Tokens: 412K in / 6K out    Files: 12  Cmds: 3
      Last: READ fee_calculator_v2.md

    o final_report
      Status: pending    Duration: -        Events: 0
      Tokens: 0K in / 0K out    Files: 0  Cmds: 0
      Last:

------------------------------------------------------------------------
  Results: outputs/*.md  |  Logs: logs/*.jsonl  |  Ctrl+C to cancel
```

## Wave Scheduling

Tasks with `depends_on` are partitioned into waves using Kahn's algorithm:

1. Wave 0: all tasks with no dependencies (parallel)
2. Wave 1: tasks whose dependencies are all in Wave 0 (parallel)
3. Wave N: tasks whose dependencies are all in Wave < N (parallel)

Behavior:
- Circular dependencies are detected and reported as errors
- Unknown dependency names are rejected at parse time
- Duplicate task names are rejected
- If any task in a wave fails, all subsequent waves are skipped (tasks marked Cancelled)
- Ctrl+C cancels all running tasks and skips remaining waves
- Tasks within each wave preserve YAML input order

## Error Handling

| Scenario | Behavior |
|----------|----------|
| Task fails (non-zero exit) | Wave marked failed, subsequent waves skipped |
| stdout read error | Task marked Failed (not overwritten by exit code) |
| Task panics (JoinError) | Meta updated to Failed on disk, synthetic report created |
| Ctrl+C during wave | All tasks killed via process group SIGTERM, remaining waves cancelled |
| Ctrl+C after task failure | Still works (separate `sigint_received` flag) |
| Chatty task ignoring cancel | Inline `is_cancelled()` check between stdout lines |
