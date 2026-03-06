# Codex Code Review Record

## Review 1 — Initial Implementation (2026-03-06)

**Reviewer:** Codex (GPT-5.4, xhigh reasoning)
**Scope:** All source files in src/ + Cargo.toml

### High-Risk Findings

| # | Issue | Location | Impact | Fix |
|---|-------|----------|--------|-----|
| 1 | stderr piped but no consumer — pipe buffer fills, child process hangs | runner.rs:44 | Task silently freezes | Added separate stderr drain task writing to `{name}.stderr.log` |
| 2 | meta.json direct overwrite, not atomic — dashboard reads partial JSON | meta.rs:61 | Dashboard crashes or silently drops tasks | Changed to `.tmp` + `rename` atomic write |
| 3 | Only success path writes final status — any error leaves status=running forever | runner.rs:30-86 | Task appears stuck when it actually failed | Wrapped `run_task_inner` with outer error handler that always updates meta |
| 4 | Ctrl+C only kills codex parent — spawned child processes leak | main.rs:65, runner.rs:46 | Zombie codex processes consume resources | Added `setpgid(0,0)` per child + `kill(-pgid, SIGTERM)` on cancel |
| 5 | `truncate()` slices by byte offset — panics on multi-byte UTF-8 (Chinese) | dashboard.rs:91 | Dashboard crashes on Chinese last_action | Changed to `chars().take(n).collect()` |

### Medium-Risk Findings

| # | Issue | Location | Fix |
|---|-------|----------|-----|
| 6 | `tail` reads to EOF then exits, not follow mode | main.rs:95-105 | Rewrote as follow mode: EOF -> check meta.status -> sleep 200ms -> retry |
| 7 | `create_dir_all().ok()` swallows errors | runner.rs:21-22 | Changed `TaskRunner::new` to return `Result`, propagate errors |

### Architecture Suggestions (Implemented)

1. **New `event.rs` module** — unified JSONL event parsing, avoids 3 places hand-writing JSON pointers
2. **`TaskRunner: Clone`** instead of `Arc<TaskRunner>` — each spawn gets lightweight copy
3. **`JoinSet` + `CancellationToken`** for scheduling — not manual `Vec<JoinHandle>`
4. **`TaskStatus::Cancelled`** — added to distinguish user cancellation from failure
5. **`exit_code` and `error` fields on TaskMeta** — better post-mortem debugging
6. **`CODEX_BIN` env var** — portability across machines and test environments
7. **`dashboard::watch_until_cancelled`** — accepts CancellationToken, exits cleanly with tasks

### Architecture Suggestions (Not Yet Implemented)

- Retry logic: optional, only for infrastructure failures (spawn/IO/transient non-zero exit), not "model answered poorly"
- Integration tests: fake codex binary, Ctrl+C cancel test, concurrent meta read/write test

---

## Review 2 — v2.0 depends_on + Wave Scheduling (2026-03-06)

**Reviewer:** Codex (GPT-5.4, xhigh reasoning)
**Scope:** Implementation plan review + 3 rounds of code review
**Tokens used:** ~156K across 3 review rounds

### Round 1 — Plan Review (pre-implementation)

| # | Severity | Issue | Fix Applied |
|---|----------|-------|-------------|
| 1 | High | `wave` field on TaskMeta not backward-compatible — old meta.json fails to parse | Added `#[serde(default)]` on `wave` field |
| 2 | High | `interrupted` flag overloaded for Ctrl+C and task failure — blocks further Ctrl+C after first failure | Split into `wave_failed` and `sigint_received` flags |
| 3 | High | Future-wave tasks invisible to dashboard — totals wrong, watch exits early | Pre-create Pending meta files for all tasks before execution |
| 4 | Medium | `into_waves()` HashMap breaks YAML ordering within waves | Changed to index-keyed HashMap, sort ready indices by original position |
| 5 | Medium | `into_waves()` silently drops duplicate names via HashMap | Moved duplicate validation into `into_waves()` itself |
| 6 | Medium | Test suite missing edge cases: empty list, self-dep, duplicates | Added all missing test cases |
| 7 | Low | CLI help text still says "Launch all tasks in parallel" | Updated to "Launch tasks in dependency waves" |

### Round 2 — Code Review (post-implementation)

| # | Severity | Issue | Fix Applied |
|---|----------|-------|-------------|
| 1 | High | JoinError (panic) path doesn't update meta on disk — stale Running state | Track wave task names, post-wave fixup writes Failed meta for unreported tasks |
| 2 | Medium | stdout-read error overwritten by exit code, reported as Done | Check `meta.error` before overwriting status; propagate error in TaskRunReport |
| 3 | Medium | Ctrl+C races with EOF in select — completed task misclassified as Cancelled | `biased` select prefers stdout over cancel |
| 4 | Medium | Dashboard alphabetical sort breaks wave-internal YAML order | Removed alphabetical sort |

### Round 3 — Final Review (post-fixes)

| # | Severity | Issue | Fix Applied |
|---|----------|-------|-------------|
| 1 | High | JoinError synthetic report still doesn't write meta to disk | Added post-wave loop: scan for unreported tasks, load and update their meta files |
| 2 | Medium | Biased select can starve Ctrl+C on chatty stdout | Added inline `cancel.is_cancelled()` check after each stdout line |
| 3 | Medium | Dashboard relies on `read_dir()` order which is unspecified | Sort metas by `(wave, name)` for deterministic display |

### Final Verification

- 11 unit tests pass (into_waves: no deps, linear chain, diamond, circular, self-dep, unknown dep, duplicates, empty, backward compat, wave-0 order, later-wave order)
- Release build clean (only pre-existing warnings)
- Backward compatible: existing YAML without `depends_on` works identically (single wave)
