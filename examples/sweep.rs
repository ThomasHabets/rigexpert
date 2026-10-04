//! cargo run --example sweep -- --demo
use rigexpert::{Analyzer, ConnectionOptions, Progress, SweepSettings};
use tokio_util::sync::CancellationToken;
#[tokio::main]
async fn main() -> rigexpert::Result<()> {
    let mut analyzer = if std::env::args().any(|a| a == "--demo") {
        Analyzer::demo().await?
    } else {
        Analyzer::connect(&ConnectionOptions::default()).await?
    };
    println!("{} / {}", analyzer.info().name, analyzer.info().firmware);
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });
    let result = analyzer
        .sweep(SweepSettings::default(), &cancel, |event| {
            if let Progress::Sample { index, sample } = event {
                println!(
                    "{index}: {:.6} MHz, {:.3} + j{:.3} ohm",
                    sample.frequency_hz / 1e6,
                    sample.r,
                    sample.x
                );
            }
        })
        .await;
    let cleanup = analyzer.disconnect().await;
    signal_task.abort();
    let sweep = result?;
    cleanup?;
    println!("{:?}", sweep.status);
    Ok(())
}
