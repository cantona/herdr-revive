//! Measure the same idle-shell API in the baseline and candidate libraries.
use anyhow::{Context, Result, ensure};
use herdr_revive::{
    model::{ForegroundProcess, ProcessInfo},
    platform,
};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let pid: u32 = args
        .get(1)
        .context("usage: benchmark_idle PID CHECKS")?
        .parse()?;
    let checks: u32 = args.get(2).context("missing check count")?.parse()?;
    ensure!((1..=10000).contains(&checks), "invalid check count");
    let shell = platform::process(pid)?;
    let info = ProcessInfo {
        pane_id: "benchmark".into(),
        shell_pid: Some(pid),
        foreground_process_group_id: Some(shell.group),
        foreground_processes: vec![ForegroundProcess { pid }],
    };
    let start = std::time::Instant::now();
    for _ in 0..checks {
        ensure!(
            platform::idle_shell(&info)?.is_some(),
            "fixture is no longer idle"
        );
    }
    println!(
        "{}",
        serde_json::json!({"checks":checks,"elapsed_ms":start.elapsed().as_secs_f64()*1000.0})
    );
    Ok(())
}
