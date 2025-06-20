mod api;
mod cache;
mod cli;
mod config;
mod install;
mod ui;

use anyhow::Result;
use clap::Parser;
use cli::Cli;
use config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    // Load configuration
    let config = Config::load()?;
    
    // Parse command line arguments
    let cli = Cli::parse();
    
    // Execute the command
    cli.execute(config).await
}