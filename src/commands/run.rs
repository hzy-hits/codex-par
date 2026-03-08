use crate::{config, dashboard, execution, meta, runner};
use anyhow::{Context, Result};
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Execute all tasks from a YAML config in dependency waves.
/// Returns `true` if all tasks succeeded, `false` if any failed or were cancelled.
pub async fn run_command(config_path: &str, base: &Path, show_dashboard: bool) -> Result<bool> {
    let tasks_config = config::TasksConfig::load(config_path)?;
    let waves = tasks_config.into_waves()?;
    let total_tasks: usize = waves.iter().map(|w| w.len()).sum();

    let task_runner = runner::TaskRunner::new(base)?;
    let shutdown = CancellationToken::new();
    let log_dir = task_runner.log_dir().to_path_buf();

    // Pre-create Pending meta files for all tasks so the dashboard shows the full pipeline.
    for (wave_idx, wave) in waves.iter().enumerate() {
        for task in wave {
            let mut m = meta::TaskMeta::new(&task.name, &task.cwd.to_string_lossy(), &task.prompt);
            m.wave = Some(wave_idx as u32);
            m.status = meta::TaskStatus::Pending;
            let _ = m.save(&log_dir);
        }
    }

    println!(
        "Launching {} tasks in {} wave(s)...\n",
        total_tasks,
        waves.len()
    );
    for (wave_idx, wave) in waves.iter().enumerate() {
        let wave_size = wave.len();
        println!(
            "-- Wave {} ({} task{}, parallel) --",
            wave_idx,
            wave_size,
            if wave_size == 1 { "" } else { "s" }
        );
    }
    if !waves.is_empty() {
        println!();
    }

    // Optionally spawn dashboard.
    let dashboard_handle = if show_dashboard {
        let ld = log_dir.clone();
        let cancel = shutdown.clone();
        Some(tokio::spawn(async move {
            dashboard::watch_until_cancelled(&ld, 1, cancel).await
        }))
    } else {
        None
    };

    let mut run_handle = tokio::spawn({
        let waves = waves.clone();
        let task_runner = task_runner.clone();
        let log_dir = log_dir.clone();
        let cancel = shutdown.clone();
        async move { execution::run_waves_quiet(waves, task_runner, &log_dir, cancel).await }
    });

    let all_ok = tokio::select! {
        result = &mut run_handle => {
            result.context("wave executor panicked")?
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\nReceived Ctrl+C, cancelling all tasks...");
            shutdown.cancel();
            run_handle
                .await
                .context("wave executor panicked after cancellation")?
        }
    };

    // Stop dashboard.
    if let Some(handle) = dashboard_handle {
        shutdown.cancel();
        handle.await.ok();
    }

    let all_reports = collect_reports(&waves, &log_dir);

    // Print summary.
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
    println!("\n  Results: {}/outputs/*.md", base.display());

    Ok(all_ok)
}

fn collect_reports(waves: &[Vec<config::TaskDef>], log_dir: &Path) -> Vec<runner::TaskRunReport> {
    waves
        .iter()
        .flat_map(|wave| wave.iter())
        .map(|task| {
            let meta_path = log_dir.join(format!("{}.meta.json", task.name));
            match meta::TaskMeta::load(&meta_path) {
                Ok(task_meta) => runner::TaskRunReport {
                    name: task_meta.name,
                    status: task_meta.status,
                    error: task_meta.error,
                },
                Err(err) => runner::TaskRunReport {
                    name: task.name.clone(),
                    status: meta::TaskStatus::Failed,
                    error: Some(format!("failed to read task meta: {}", err)),
                },
            }
        })
        .collect()
}
