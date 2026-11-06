use anyhow::Error;
use clap::{ArgAction, Parser, Subcommand};
use log::LevelFilter;

use crate::{
    api::Client,
    download::{DownloadType, download},
};

/// A simple CLI tool to back up and archive every asset (games, places, models, images, passes, etc) on Roblox into a Rojo-compatible Git repository
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Open Cloud API key (create at https://create.roblox.com/dashboard/credentials)
    #[arg(long, env = "ROBLOX_API_KEY", hide_env_values = true)]
    api_key: String,

    /// Increase log level (default info, -v debug, -vv trace)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Download all versions of an asset & associated content
    #[command(subcommand)]
    Download(DownloadCommand),
}

#[derive(Subcommand, Debug)]
enum DownloadCommand {
    /// Downloads all versions of place
    Place {
        /// The id of the place to download (roblox.com/games/id)
        #[arg(name = "id")]
        place_id: u64,

        // The directory to output downloads (will be created)
        #[arg(name = "output")]
        output_dir: String,

        /// Also download assets references (images, meshes, mesh parts, audios, videos, animations)
        #[arg(short, long)]
        references: bool,
    },
}

pub(crate) async fn run() -> Result<(), Error> {
    let args = Args::parse();
    env_logger::builder()
        .filter_module(
            env!("CARGO_CRATE_NAME"),
            match args.verbose {
                ..=0 => LevelFilter::Info,
                1 => LevelFilter::Debug,
                2.. => LevelFilter::Trace,
            },
        )
        .parse_default_env()
        .init();

    let client = Client::new(args.api_key);

    match args.command {
        Command::Download(command) => match command {
            DownloadCommand::Place {
                place_id,
                output_dir,
                references,
            } => {
                download(
                    client,
                    output_dir,
                    DownloadType::Place,
                    place_id,
                    references,
                )
                .await
            }
        },
    }
}
