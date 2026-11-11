use std::path::PathBuf;

use anyhow::{Context, Error, bail};
use clap::{ArgAction, Args, Parser, Subcommand};
use log::LevelFilter;

use crate::{
    api::Client,
    download::download,
    progress::Progress,
    state::{State, StateType},
};

/// A simple CLI tool to back up and archive every asset (games, places, models, images, passes, etc) on Roblox into a Rojo-compatible Git repository
#[derive(Parser, Debug)]
#[command(version, about)]
struct CliArgs {
    /// Open Cloud API key (create at https://create.roblox.com/dashboard/credentials)
    #[arg(long, env = "ROBLOX_API_KEY", hide_env_values = true)]
    api_key: String,

    /// Increase log level (default info, -v debug, -vv trace)
    #[arg(short, long, action = ArgAction::Count, global = true, display_order = 100)]
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

#[derive(Args, Debug)]
struct DownloadOptions {
    /// The minimum version to fetch (negative values mean from the most recent; -1 is the latest, -2 is the previous version, 10 is version 10, default 1)
    #[arg(long, allow_hyphen_values = true)]
    min: Option<i64>,

    /// The maximum version to fetch (negative values mean from the most recent; -1 is the latest, -2 is the previous version, 10 is version 10, default latest version)
    #[arg(long, allow_hyphen_values = true)]
    max: Option<i64>,

    /// The number of concurrent downloads to perform
    #[arg(short, long, default_value_t = 8)]
    concurrent: usize,
}

#[derive(Subcommand, Debug)]
enum DownloadCommand {
    /// Downloads all versions of place
    Place {
        /// The id of the place to download (roblox.com/games/id)
        id: u64,

        // The directory to output downloads (will be created)
        #[arg(name = "output")]
        output_dir: String,

        /// Also download assets references (images, meshes, mesh parts, audios, videos, animations)
        #[arg(short, long)]
        references: bool,

        #[command(flatten)]
        options: DownloadOptions,
    },

    /// Resumes a download of any type
    Resume {
        // The directory passed on previous download (should exist)
        #[arg(name = "output")]
        output_dir: String,

        #[command(flatten)]
        options: DownloadOptions,
    },
}

impl DownloadCommand {
    pub fn output(&self) -> String {
        match self {
            DownloadCommand::Place { output_dir, .. } => output_dir.to_owned(),
            DownloadCommand::Resume { output_dir, .. } => output_dir.to_owned(),
        }
    }
}

pub(crate) async fn run() -> Result<(), Error> {
    let args = CliArgs::parse();
    let logger = env_logger::builder()
        .filter_module(
            env!("CARGO_CRATE_NAME"),
            match args.verbose {
                ..=0 => LevelFilter::Info,
                1 => LevelFilter::Debug,
                2.. => LevelFilter::Trace,
            },
        )
        .format_timestamp_millis()
        .parse_default_env()
        .build();
    let progress = Progress::new(logger);

    let client = Client::new(args.api_key);

    match args.command {
        Command::Download(command) => {
            let dir_path = PathBuf::from(command.output());
            let existing_state = State::load(&dir_path).context("Failed to load state")?;

            match command {
                DownloadCommand::Place {
                    id,
                    output_dir,
                    references,
                    options:
                        DownloadOptions {
                            min,
                            max,
                            concurrent,
                        },
                } => {
                    if existing_state.is_some() {
                        bail!(
                            "Existing state file found - did you mean to do `{} download resume`?",
                            env!("CARGO_BIN_NAME")
                        )
                    }
                    if dir_path.exists() && !dir_path.is_empty() {
                        bail!("Cannot download to non-empty directory `{output_dir}`");
                    }

                    download(
                        progress,
                        client,
                        output_dir,
                        State::new(StateType::Place, id, references),
                        min,
                        max,
                        concurrent,
                    )
                    .await
                }
                DownloadCommand::Resume {
                    output_dir,
                    options:
                        DownloadOptions {
                            min,
                            max,
                            concurrent,
                        },
                } => match existing_state {
                    None => {
                        bail!(
                            "Cannot resume - state file `{}` not found",
                            State::STATE_FILENAME
                        );
                    }
                    Some(state) => {
                        download(progress, client, output_dir, state, min, max, concurrent).await
                    }
                },
            }
        }
    }
}
