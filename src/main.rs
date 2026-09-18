use clap::Parser;
use std::io::{BufWriter, Write};

fn main() {
    if let Err(error) = run() {
        eprintln!("herdr-revive: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let value = herdr_revive::app::run(herdr_revive::app::Cli::parse())?;
    let mut output = BufWriter::new(std::io::stdout().lock());
    if let Some(text) = value
        .get("config")
        .or_else(|| value.get("text"))
        .and_then(serde_json::Value::as_str)
    {
        output.write_all(text.as_bytes())?;
    } else {
        serde_json::to_writer_pretty(&mut output, &value)?;
        writeln!(output)?;
    }
    output.flush()?;
    Ok(())
}
