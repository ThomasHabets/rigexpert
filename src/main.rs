mod bands;
mod tui;
use clap::{Args, Parser, Subcommand};
use rigexpert::{
    Analyzer, ConnectionOptions, DEFAULT_ADDRESS, Error, Result, SweepSettings, SweepStatus, files,
    transport,
};
use std::{path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Control a RigExpert AA-650 ZOOM over Linux BLE. No subcommand opens the TUI."
)]
struct Cli {
    #[arg(long,global=true,default_value=DEFAULT_ADDRESS)]
    device: String,
    #[arg(long, global = true)]
    adapter: Option<String>,
    #[arg(long, global = true, help = "Use a deterministic simulated analyzer")]
    demo: bool,
    #[arg(long,global=true,default_value="15",value_parser=clap::value_parser!(u64).range(1..=120))]
    timeout: u64,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand, Debug)]
enum Command {
    /// Open the interactive terminal UI; optionally load a saved file.
    Tui {
        #[arg(long)]
        load: Option<PathBuf>,
    },
    /// Discover nearby `RigExpert` devices.
    Scan {
        #[arg(long,default_value="5",value_parser=clap::value_parser!(u64).range(1..=60))]
        seconds: u64,
    },
    /// Read device identity and measurement capabilities as JSON.
    Info,
    /// Read impedance at a single frequency (two samples, zero span).
    Measure {
        #[arg(value_parser=parse_frequency)]
        frequency: String,
        #[arg(long,default_value="50",value_parser=parse_positive)]
        z0: f64,
        #[command(flatten)]
        output: Output,
    },
    /// Acquire a sweep, optionally repeating until Ctrl-C.
    Sweep {
        #[command(flatten)]
        settings: SettingsArgs,
        #[arg(long)]
        repeat: bool,
        #[command(flatten)]
        output: Output,
    },
    /// Browse or download measurements saved on the analyzer.
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
}
#[derive(Subcommand, Debug)]
enum MemoryCommand {
    List,
    Download {
        slot: u8,
        #[command(flatten)]
        output: Output,
    },
}
#[derive(Args, Debug)]
struct Output {
    #[arg(
        short,
        long,
        help = "Save as .csv, .s1p, or .json; otherwise print CSV"
    )]
    output: Option<PathBuf>,
    #[arg(long, help = "Allow replacement of the output file")]
    overwrite: bool,
}
#[derive(Args, Debug)]
struct SettingsArgs {
    #[arg(long,default_value="144MHz",value_parser=parse_frequency)]
    start: String,
    #[arg(long,default_value="148MHz",value_parser=parse_frequency)]
    stop: String,
    #[arg(long,default_value="201",value_parser=clap::value_parser!(usize))]
    samples: usize,
    #[arg(long,default_value="50",value_parser=parse_positive)]
    z0: f64,
}
fn parse_positive(s: &str) -> std::result::Result<f64, String> {
    let n: f64 = s.parse().map_err(|_| "expected a number".to_string())?;
    if !n.is_finite() || n <= 0.0 {
        Err("must be positive and finite".into())
    } else {
        Ok(n)
    }
}
/// Accept Hz by default and explicit Hz/kHz/MHz/GHz suffixes.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Frequency is checked finite, integral and within 1..=650000000 before conversion"
)]
pub(crate) fn frequency(s: &str) -> std::result::Result<u64, String> {
    let input = s.trim().to_ascii_lowercase();
    let (number, multiplier) = if let Some(n) = input.strip_suffix("ghz") {
        (n, 1e9)
    } else if let Some(n) = input.strip_suffix("mhz") {
        (n, 1e6)
    } else if let Some(n) = input.strip_suffix("khz") {
        (n, 1e3)
    } else if let Some(n) = input.strip_suffix("hz") {
        (n, 1.0)
    } else {
        (input.as_str(), 1.0)
    };
    let f = number
        .trim()
        .parse::<f64>()
        .map_err(|_| "expected a frequency such as 145.5MHz".to_string())?
        * multiplier;
    if !f.is_finite() || f < 1.0 || f > 650_000_000.0 || (f - f.round()).abs() > 1e-4 {
        return Err("frequency must be a whole number of Hz in 1..=650000000".into());
    }
    Ok(f.round() as u64)
}
fn parse_frequency(s: &str) -> std::result::Result<String, String> {
    frequency(s)?;
    Ok(s.into())
}
fn print_sweep(s: &rigexpert::Sweep) {
    println!("frequency_hz,r_ohm,x_ohm");
    for v in &s.data {
        println!("{},{},{}", v.frequency_hz, v.r, v.x);
    }
}
fn output_sweep(s: &rigexpert::Sweep, output: &Output) -> Result<()> {
    if let Some(path) = &output.output {
        files::export(path, s, output.overwrite)?;
        eprintln!("Saved {} samples to {}", s.data.len(), path.display());
    } else {
        print_sweep(s);
    }
    if let SweepStatus::Partial(reason) = &s.status {
        return Err(Error::Protocol(format!(
            "incomplete sweep ({} samples): {reason}",
            s.data.len()
        )));
    }
    Ok(())
}
async fn run(cli: Cli) -> Result<()> {
    let options = ConnectionOptions {
        address: cli.device,
        adapter: cli.adapter,
        timeout: Duration::from_secs(cli.timeout),
    };
    match cli.command {
        None => return tui::run(options, cli.demo, None).await,
        Some(Command::Tui { load }) => return tui::run(options, cli.demo, load).await,
        Some(Command::Scan { seconds }) => {
            if cli.demo {
                println!("demo  AA-650 DEMO");
            } else {
                for d in transport::scan(options.adapter.as_deref(), Duration::from_secs(seconds))
                    .await?
                {
                    println!("{}  {}  connected={}", d.address, d.name, d.connected);
                }
            }
            return Ok(());
        }
        _ => {}
    }
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    let mut analyzer = if cli.demo {
        Analyzer::demo().await?
    } else {
        Analyzer::connect(&options).await?
    };
    let outcome = async {
        match cli.command.unwrap() {
            Command::Info => println!("{}", serde_json::to_string_pretty(analyzer.info())?),
            Command::Measure {
                frequency: f,
                z0,
                output,
            } => {
                let f = frequency(&f).map_err(Error::Invalid)?;
                let settings = SweepSettings {
                    start_hz: f,
                    stop_hz: f,
                    samples: 2,
                    z0,
                };
                output_sweep(&analyzer.sweep(settings, &cancel, |_| {}).await?, &output)?;
            }
            Command::Sweep {
                settings,
                repeat,
                output,
            } => {
                if repeat && output.output.is_some() && !output.overwrite {
                    return Err(Error::Invalid(
                        "--repeat with --output requires --overwrite".into(),
                    ));
                }
                let settings = SweepSettings {
                    start_hz: frequency(&settings.start).map_err(Error::Invalid)?,
                    stop_hz: frequency(&settings.stop).map_err(Error::Invalid)?,
                    samples: settings.samples,
                    z0: settings.z0,
                };
                loop {
                    let sweep = analyzer.sweep(settings, &cancel, |_| {}).await?;
                    output_sweep(&sweep, &output)?;
                    if !repeat || cancel.is_cancelled() {
                        break;
                    }
                }
            }
            Command::Memory { command } => {
                let records = analyzer.records(50.0, &cancel).await?;
                match command {
                    MemoryCommand::List => println!("{}", serde_json::to_string_pretty(&records)?),
                    MemoryCommand::Download { slot, output } => {
                        let record = records.iter().find(|r| r.slot == slot).ok_or_else(|| {
                            Error::Invalid(format!("memory slot {slot} not found"))
                        })?;
                        output_sweep(&analyzer.download(record, &cancel, |_| {}).await?, &output)?;
                    }
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }
    .await;
    let cleanup = analyzer.disconnect().await;
    signal.abort();
    outcome.and(cleanup)
}
#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_units_and_validation() {
        assert_eq!(frequency("145.5MHz").unwrap(), 145_500_000);
        assert_eq!(frequency("100 kHz").unwrap(), 100_000);
        for s in ["NaN", "inf", "-1", "650.1MHz", "0.1Hz"] {
            assert!(frequency(s).is_err());
        }
        assert!(Cli::try_parse_from(["rigexpert", "sweep", "--z0", "0"]).is_err());
        assert!(Cli::try_parse_from(["rigexpert", "--demo", "memory", "download", "2"]).is_ok());
    }
}
