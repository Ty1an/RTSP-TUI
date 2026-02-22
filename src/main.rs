mod cache;
mod cli;
mod discovery;
mod onvif;
mod theme;
mod tui;
mod viewer;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Command, StreamsCommand};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        None | Some(Command::Tui) => {
            tui::run_tui().await?;
        }
        Some(Command::View(args)) => {
            viewer::run_view(&args).await?;
        }
        Some(Command::Discover(args)) => {
            let discovered = discovery::discover_streams(&args).await?;

            if discovered.is_empty() {
                println!("No RTSP streams discovered.");
            } else {
                println!("Discovered {} stream(s):", discovered.len());
                for stream in &discovered {
                    println!("- [{}] {}", stream.status, stream.url);
                }
            }

            if !args.no_save {
                let merged = match cache::load_cache() {
                    Ok(mut existing) => {
                        existing.streams.extend(discovered);
                        existing.sort_and_dedup()
                    }
                    Err(_) => cache::DiscoveryCache {
                        streams: discovered,
                    }
                    .sort_and_dedup(),
                };

                cache::save_cache(&merged).context("failed to save discovered streams cache")?;
                let cache_path = cache::cache_path()?;
                println!("Saved discovery cache: {}", cache_path.display());
            }
        }
        Some(Command::Streams(args)) => match args.command {
            StreamsCommand::List(list_args) => {
                let cache = cache::load_cache()?;
                if list_args.json {
                    println!("{}", serde_json::to_string_pretty(&cache)?);
                    return Ok(());
                }

                if cache.streams.is_empty() {
                    println!("No cached streams. Run `rtsp-tui discover` first.");
                    return Ok(());
                }

                println!(
                    "{:<6}  {:<14}  {:<12}  URL",
                    "INDEX", "STATUS", "DISCOVERED"
                );
                for (idx, stream) in cache.streams.iter().enumerate() {
                    println!(
                        "{:<6}  {:<14}  {:<12}  {}",
                        idx + 1,
                        stream.status,
                        stream.discovered_at_unix,
                        stream.url
                    );
                }
            }
        },
    }

    Ok(())
}
