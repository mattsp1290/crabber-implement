mod agent;
mod config;
#[cfg(test)]
mod formal_verification;
mod git;
mod workflow;

use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let config = config::RunConfig::load(config::Cli::parse())?;
    workflow::run(config).await
}
