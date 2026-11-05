use clap::{Parser, Subcommand};

use crate::download::{DownloadType, download};

/// A simple CLI tool to back up and archive every asset (games, places, models, images, passes, etc) on Roblox into a Rojo-compatible Git repository
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Open Cloud API key (create at https://create.roblox.com/dashboard/credentials)
    #[arg(long, env = "ROBLOX_API_KEY", hide_env_values = true)]
    api_key: String,

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

pub(crate) fn run() {
    let args = Args::parse();

    match args.command {
        Command::Download(command) => match command {
            DownloadCommand::Place {
                place_id,
                output_dir,
                references,
            } => download(output_dir, DownloadType::Place, place_id, references),
        },
    }
}
