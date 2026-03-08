# Phase 2 Design: Dynamic Scheduling + Shared Context

**Date:** 2026-03-08
**Status:** Approved
**Scope:** `codex-par serve` path only — CLI (`run`) is unchanged.

---

## Problem

Phase 1 gave Claude identity, messages, and artifacts, but scheduling is still static: Claude
must provide the full task DAG upfront via `start_run`. Two capabilities are missing:

1. **Dynamic scheduling** — Claude cannot spawn new tasks after a run starts, so it cannot
   react to what workers discover or produce.
2. **Shared context** — each worker starts cold. Claude has no way to inject shared facts
   (e.g. "the target repo is at /foo, use model o4-mini") into worker prompts at spawn time.

---

## Goals

- Claude can call `dispatch_task` or `dispatch_wave` on an active run to add tasks at any point.
- Claude can call `write_fact` to store structured facts that are auto-prepended to subsequently
  spawned worker prompts.
- Workers can read `shared/facts.json` explicitly for the full current fact set.
- The CLI path (`run` subcommand) is **not changed**.
- Fail-fast semantics are preserved: first task failure cancels the run and stops dispatch.

## Non-Goals

- Dynamic `depends_on` pointing to future (not-yet-dispatched) tasks — that is Approach B and
  is out of scope for Phase 2.
- Crash recovery / run resumption across process restarts.
- Any changes to the dashboard TUI.

---

## Architecture

### Current serve path

```
start_run → run_waves_quiet (static DAG) → run finalizes
```

`ActiveRun` today carries: `cancel: CancellationToken`, `handle: JoinHandle<bool>`.

### Phase 2 serve path

```
start_run → RunCoordinator (long-lived, owns JoinSet) → run finalizes on seal_dispatch
```

`ActiveRun` gains: `dispatch_tx: mpsc::Sender<DispatchRequest>`.

The coordinator loop:

```
loop {
    select! {
        msg = dispatch_rx.recv()    => accept task(s), validate names, spawn via TaskRunner
        result = join_set.join_next() => record completion, unblock waiting tasks
        _ = cancel.cancelled()      => drain queue, mark pending as Cancelled, break
    }
}
```

A task is unblocked when all its `depends_on` names are in the `completed` set.
Newly dispatched tasks with no `depends_on` wait for a **barrier snapshot**: the set of
task names that were running or queued at dispatch time. This is the safe default for
output-driven follow-ups.

### Run lifecycle

```
start_run  →  [dispatch_task / dispatch_wave]*  →  seal_dispatch  →  run finalizes
```

`seal_dispatch` closes `dispatch_tx`. The coordinator drains remaining queued tasks, then
exits when `join_set` is empty. Without `seal_dispatch`, the run stays open indefinitely.

---

## New MCP Tools

| Tool | Description |
|------|-------------|
| `dispatch_task` | Add a single task to an active (unsealed) run |
| `dispatch_wave` | Add a batch of tasks to an active (unsealed) run |
| `seal_dispatch` | Close the dispatch channel; run finalizes when in-flight tasks finish |
| `write_fact` | Write or overwrite a fact in the run's shared facts store |
| `read_facts` | Read all facts for a run |

### `dispatch_task` params
```json
{
  "run_dir": "string",
  "name": "string",
  "cwd": "string",
  "prompt": "string",
  "sandbox": "read-only | read-write | network-read-only",
  "model": "string | null",
  "depends_on": ["already-existing-task-name", ...],
  "agent_id": "string | null",
  "thread_id": "string | null",
  "ask_for_approval": "never | on-request | untrusted",
  "config_overrides": ["key=value", ...],
  "profile": "string | null"
}
```

`depends_on` may only reference tasks already accepted into this run. Forward references
(to tasks not yet dispatched) are rejected with an error.

### `dispatch_wave` params
Same as `dispatch_task` but `tasks` is an array of task definitions. The entire batch is
validated atomically before any task is accepted; within the batch, forward `depends_on`
references to other tasks in the same batch are allowed.

### `write_fact` params
```json
{
  "run_dir": "string",
  "key": "string",
  "value": "string",           // inline string value (≤ 1 KiB enforced)
  "artifact_path": "string | null"  // optional: path to a large artifact file
}
```

Facts are stored at `run_dir/shared/facts.json` as a `BTreeMap<String, FactEntry>`,
atomically rewritten (`.tmp` + rename). If `artifact_path` is set, `value` is treated
as a summary/description.

### `read_facts` params
```json
{ "run_dir": "string" }
```

Returns the full `facts.json` contents as a JSON object.

---

## Facts Store

### Storage

```
run_dir/
  shared/
    facts.json     ← BTreeMap<String, FactEntry>, atomically rewritten
```

```rust
pub struct FactEntry {
    pub value: String,               // inline value (≤ 1 KiB)
    pub artifact_path: Option<PathBuf>, // path to large artifact
    pub updated_at: DateTime<Local>,
}
```

### Prompt injection

At dispatch time (in the coordinator, before calling `TaskRunner::run_task`), if
`facts.json` exists and is non-empty, prepend to the task prompt:

```
=== Shared Context ===
{key}: {value}
{key2}: {value2}
...
[Full facts at: /run_dir/shared/facts.json]
======================

{original prompt}
```

Constraints:
- Max 1 KiB per fact value (truncated with `[truncated]` marker if exceeded at write time)
- Max 4 KiB total preamble (facts sorted by key, truncated if total exceeds limit)
- Always include the absolute path to `facts.json` so workers can re-read explicitly

Injection happens **in the coordinator**, not in `TaskRunner::run_task`, so `prompt_preview`
in `TaskMeta` reflects the actual injected prompt.

---

## Data Structures (new/changed)

### `src/facts.rs` (new)
```rust
pub struct FactEntry { pub value: String, pub artifact_path: Option<PathBuf>, pub updated_at: ... }
pub struct FactsStore { run_dir: PathBuf }
impl FactsStore {
    pub fn write_fact(&self, key: &str, entry: FactEntry) -> Result<()>; // atomic
    pub fn read_all(&self) -> Result<BTreeMap<String, FactEntry>>;
    pub fn build_preamble(&self) -> Result<Option<String>>;
}
```

### `src/coordinator.rs` (new)
```rust
pub struct RunCoordinator { ... }
pub enum DispatchRequest { Task(TaskDef), Wave(Vec<TaskDef>), Seal }
impl RunCoordinator {
    pub fn start(run_dir, runner, cancel) -> (Self, mpsc::Sender<DispatchRequest>);
    pub async fn run(self) -> bool; // returns all_ok
}
```

### `src/commands/serve.rs` (changed)
- `ActiveRun` gains `dispatch_tx: mpsc::Sender<DispatchRequest>`
- `start_run` uses `RunCoordinator::start` instead of `run_waves_quiet`
- Add `dispatch_task`, `dispatch_wave`, `seal_dispatch`, `write_fact`, `read_facts` tools

### `src/meta.rs` (changed)
- `TaskMeta` gains `dispatched_at: Option<DateTime<Local>>` to distinguish static vs dynamic tasks
- `wave` field becomes `Option<u32>` — dynamic tasks with no static wave get `None`

---

## Wave Metadata

Dynamic tasks get `wave: None` in their meta. The `get_status` tool already sorts by
`(wave, name)`; `None` waves sort last. The dashboard TUI (CLI path) is unaffected.

---

## Validation Rules for Dispatch

1. Run must exist and be unsealed (`dispatch_tx` still open).
2. Task name must match `[A-Za-z0-9._-]`, not be empty, not be `.` or `..`.
3. Task name must be unique across: already-accepted + already-running + already-completed
   task names for this run, AND must not collide with any `.meta.json` file already on disk.
4. `depends_on` names must all be already accepted into this run (no forward refs, except
   within a single `dispatch_wave` batch where intra-batch forward refs are allowed).
5. The `cwd` must be a non-empty path.

---

## Fail-Fast Semantics

- First task failure triggers `cancel.cancel()` — same as today.
- Coordinator stops spawning new tasks once cancelled.
- Tasks in the coordinator queue that haven't started are marked `Cancelled` with
  `error: "run cancelled before task started"`.
- `seal_dispatch` after a failed run is still valid (no-op / immediate finalization).

---

## Audit Log

Each accepted dispatch request is appended to `run_dir/logs/dispatch.jsonl`:
```json
{"ts":"...","type":"task","name":"fix_bug_42","depends_on":[],"queued_at":"..."}
```
This is for debugging only — the coordinator's source of truth is in-memory state.

---

## Risks & Mitigations

| Risk | Mitigation |
|------|-----------|
| Run completion race (last task done before Claude dispatches follow-ups) | `seal_dispatch` is required; run stays open until Claude explicitly seals it |
| Task name collision | Validate at dispatch time against in-memory + on-disk state |
| Wave field drift in dashboard | `wave: Option<u32>`, dynamic tasks sort last, documented |
| Approach C scope creep (forward deps → Approach B) | Hard error: forward `depends_on` refs rejected at dispatch time |
| `resume_agent` anti-pattern (untracked background work) | Dynamic dispatch goes through coordinator channel only |
| Fact value too large | Enforced ≤ 1 KiB per fact at `write_fact` time; artifact_path for large data |
| Concurrent fact writes | `FactsStore::write_fact` uses atomic `.tmp` + rename; serialized per run via coordinator |
