use crate::app::{Action, Cli};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub fn run(mut cli: Cli) -> Result<Value> {
    let Action::Timer { interval_seconds } = cli.command else {
        anyhow::bail!("invalid timer invocation");
    };
    let config_dir = cli
        .config_dir
        .clone()
        .or_else(|| std::env::var_os("HERDR_PLUGIN_CONFIG_DIR").map(Into::into))
        .context("HERDR_PLUGIN_CONFIG_DIR is required")?;
    let interval =
        interval_seconds.unwrap_or(crate::store::load_config(&config_dir)?.interval_seconds);
    ensure!(
        (1..=86400).contains(&interval),
        "timer interval must be 1..86400 seconds"
    );
    #[cfg(target_os = "linux")]
    {
        use nix::sys::{
            signal::{SigSet, Signal},
            signalfd::{SfdFlags, SignalFd},
        };
        let mut mask = SigSet::empty();
        for signal in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP] {
            mask.add(signal);
        }
        mask.thread_block()?;
        let signals = SignalFd::with_flags(&mask, SfdFlags::SFD_NONBLOCK)?;
        cli.command = Action::Autosave { force: true };
        loop {
            println!("{}", crate::app::run(cli.clone())?);
            let until = std::time::Instant::now() + std::time::Duration::from_secs(interval);
            while std::time::Instant::now() < until {
                if signals.read_signal()?.is_some() {
                    let result = crate::app::run(cli)?;
                    return Ok(json!({"status":"timer_stopped","final_save":result}));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("timer signal handling is only validated on Linux")
    }
}
