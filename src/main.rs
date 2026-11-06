use clap::Parser;
use color_eyre::eyre::Result;
use redis_rover::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();
    if let Err(e) = redis_rover::run(args).await {
        eprintln!("{}: {e:#}", env!("CARGO_PKG_NAME"));
        Err(e)
    } else {
        Ok(())
    }
}
