use std::path::PathBuf;

use anyhow::{Context, Error, bail};
use clap::{ArgAction, Args, Parser, Subcommand};
use log::LevelFilter;

use crate::{api::Client, download::download, progress::Progress, state::State};

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
pub struct DownloadOptions {
    /// The minimum version to fetch (negative values mean from the most recent; -1 is the latest, -2 is the previous version, 10 is version 10, default 1)
    #[arg(long, allow_hyphen_values = true)]
    pub min: Option<i64>,

    /// The maximum version to fetch (negative values mean from the most recent; -1 is the latest, -2 is the previous version, 10 is version 10, default latest version)
    #[arg(long, allow_hyphen_values = true)]
    pub max: Option<i64>,

    /// The number of concurrent downloads to perform
    #[arg(short, long, default_value_t = 8)]
    pub concurrent: usize,
}

#[derive(Subcommand, Debug)]
enum DownloadCommand {
    /// Downloads all versions of an asset && associated context
    ///
    /// Supports: Place, Model, Plugin, Audio, Decal, Image, Video, Mesh, MeshPart, Animation
    Asset {
        /// The id of the asset to download
        id: u64,

        /// The directory to output downloads [default: output_{id}]
        output: Option<PathBuf>,

        /// Also download asset's references (images, meshes, mesh parts, audios, videos, animations within it)
        #[arg(short, long)]
        references: bool,

        #[command(flatten)]
        options: DownloadOptions,
    },

    /// Resumes a download of any type
    Resume {
        /// The directory passed on previous download (if not passed, scans in current directory)
        output: Option<PathBuf>,

        #[command(flatten)]
        options: DownloadOptions,
    },
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
            // Determine output location
            let output = match &command {
                DownloadCommand::Resume { output, .. } => {
                    // Output path is optional for resume
                    match output {
                        Some(output) => output.clone(),
                        None => {
                            // If not passed, scan cwd
                            let mut possible = Vec::new();
                            let scan = PathBuf::from(".");
                            for entry in scan.read_dir().context("Failed to scan directory")? {
                                if let Ok(entry) = entry
                                    && let Ok(metadata) = entry.metadata()
                                    && metadata.is_dir()
                                    && let path = entry.path()
                                    && path.join(State::STATE_FILENAME).exists()
                                {
                                    possible.push(path);
                                }
                            }

                            match possible.len() {
                                0 => bail!(
                                    "Scanned current directory and found resumption targets - try passing [output] directly"
                                ),
                                1 => possible.remove(0),
                                _ => bail!(
                                    "Scanned current directory and found multiple resumption targets: {} - pass [output] to select one",
                                    possible
                                        .iter()
                                        .map(|p| p.display().to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ),
                            }
                        }
                    }
                }
                DownloadCommand::Asset { output, id, .. } => match output {
                    Some(output) => output.clone(),
                    None => PathBuf::from(format!("output_{id}")),
                },
            };
            let existing_state = State::load(&output).context("Failed to load state")?;

            match command {
                DownloadCommand::Resume { options, .. } => match existing_state {
                    None => {
                        bail!(
                            "Cannot resume - state file `{}` not found",
                            State::STATE_FILENAME
                        );
                    }
                    Some(state) => download(progress, client, output, state, options).await,
                },
                DownloadCommand::Asset {
                    id,
                    references,
                    options,
                    ..
                } => {
                    if let Some(existing_state) = existing_state {
                        if existing_state.id == id && existing_state.references == references {
                            return download(progress, client, output, existing_state, options)
                                .await;
                        }
                        bail!(
                            "Existing state file found - did you mean to do `{} download resume`?",
                            env!("CARGO_BIN_NAME")
                        )
                    }

                    if output.exists() && !output.is_empty() {
                        bail!(
                            "Cannot download to non-empty directory `{}`",
                            output.display()
                        );
                    }

                    download(
                        progress,
                        client,
                        output,
                        State::new(id, references),
                        options,
                    )
                    .await
                }
            }
        }
    }
}
