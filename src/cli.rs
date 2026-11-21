use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use anyhow::{Context, Error, Result, bail};
use clap::{ArgAction, Args, Parser, Subcommand};
use log::{LevelFilter, debug};

use crate::{
    api::{Client, IntrospectResponse},
    download::download,
    progress::Progress,
    state::{Source, State},
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
    /// Download asset(s)
    ///
    /// Supports: Place, Model, Plugin, Audio, Decal, Image, Video, Mesh, MeshPart, Animation, all clothing items, all animation avatar items
    #[command(subcommand)]
    Download(DownloadCommand),
}

#[derive(Args, Debug)]
pub struct DownloadOptions {
    /// Only fetch the first n versions (the earliest n versions, passing without value (`--first`) defaults to the earliest 1)
    #[arg(long, alias="earliest", conflicts_with = "last", num_args=0..=1, default_missing_value = "1")]
    pub first: Option<NonZeroUsize>,

    /// Only fetch the last n versions (the latest n versions, passing without value (`--last`) defaults to the latest 1)
    #[arg(long, alias="latest", conflicts_with = "first", num_args=0..=1, default_missing_value = "1")]
    pub last: Option<NonZeroUsize>,

    /// The number of concurrent downloads to perform
    #[arg(short, long, default_value_t = 8)]
    pub concurrent: usize,
}

#[derive(Subcommand, Debug)]
enum DownloadCommand {
    /// Downloads versions of asset(s)
    ///
    /// Supports: Place, Model, Plugin, Audio, Decal, Image, Video, Mesh, MeshPart, Animation, all clothing items, all animation avatar items
    Asset {
        /// The id of the asset to download (can pass multiple)
        #[arg(required = true, value_delimiter = ',')]
        ids: Vec<u64>,

        /// The directory to output downloads
        #[arg(short, long, default_value = "output")]
        output: PathBuf,

        /// Also download asset's references (Images, Meshes, MeshParts, Audios, Videos, Animations within it)
        #[arg(short, long)]
        references: bool,

        #[command(flatten)]
        options: DownloadOptions,
    },

    /// Downloads versions of all assets under the API key's user
    ///
    /// Supports: Place, Model, Plugin, Audio, Decal, Image, Video, Mesh, MeshPart, Animation, all clothing items, all animation avatar items
    User {
        /// The directory to output downloads
        #[arg(short, long, default_value = "output")]
        output: PathBuf,

        /// Also download asset's references (Images, Meshes, MeshParts, Audios, Videos, Animations within it)
        #[arg(short, long)]
        references: bool,

        #[command(flatten)]
        options: DownloadOptions,
    },

    /// Resumes a download of any type
    Resume {
        /// The directory passed on previous download (if not passed, scans in current directory)
        #[arg(short, long)]
        output: Option<PathBuf>,

        #[command(flatten)]
        options: DownloadOptions,
    },
}

async fn validate_key(client: &Client) -> Result<IntrospectResponse> {
    let introspect = client
        .introspect()
        .await
        .context("Failed to validate API key")?;

    if !introspect.enabled {
        bail!("API key \"{}\" is not enabled", introspect.name);
    }
    if introspect.expired {
        bail!("API key \"{}\" is expired", introspect.name);
    }

    debug!(
        "API key \"{}\" owned by {} validated, scopes: {}",
        introspect.name,
        introspect.authorized_user_id,
        introspect
            .scopes
            .iter()
            .map(|scope| scope.to_string())
            .collect::<Vec<_>>()
            .join(", "),
    );

    Ok(introspect)
}

fn scan_for_existing() -> Result<PathBuf> {
    // Scan cwd
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
        1 => Ok(possible.remove(0)),
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
    let introspect = validate_key(&client).await?;

    match args.command {
        Command::Download(command) => {
            let existing_conflicting_state_error = |output: &Path| -> Result<()> {
                bail!(
                    "Existing state file found in `{}` - did you mean to do `{} download resume` OR set `--output`?",
                    output.display(),
                    env!("CARGO_BIN_NAME")
                );
            };
            let check_empty_output = |output: &Path| {
                if output.exists() && !output.is_empty() {
                    bail!(
                        "Cannot download to non-empty directory `{}`",
                        output.display()
                    );
                }

                Ok(())
            };

            match command {
                DownloadCommand::Resume { output, options } => {
                    // Resume scans cwd if output is not provided
                    let output = match output {
                        None => scan_for_existing()?,
                        Some(output) => output,
                    };
                    match State::load(&output)? {
                        None => {
                            // Unlike other commands, resume only works with a state file
                            bail!(
                                "Cannot resume - {} does not have state file `{}`",
                                output.display(),
                                State::STATE_FILENAME
                            );
                        }
                        Some(existing_state) => {
                            download(progress, client, existing_state, options).await
                        }
                    }
                }
                DownloadCommand::Asset {
                    ids,
                    output,
                    references,
                    options,
                } => {
                    let existing_state = State::load(&output)?;
                    if let Some(existing_state) = existing_state {
                        // Resume if existing state matches the command run, otherwise it conflicts
                        if existing_state.source == Source::Ids
                            && existing_state.ids == ids
                            && existing_state.references == references
                        {
                            return download(progress, client, existing_state, options).await;
                        }
                        existing_conflicting_state_error(&output)?;
                    }

                    check_empty_output(&output)?;

                    download(
                        progress,
                        client,
                        State::new(Source::Ids, ids, references, output),
                        options,
                    )
                    .await
                }
                DownloadCommand::User {
                    output,
                    references,
                    options,
                } => {
                    let existing_state = State::load(&output)?;
                    if let Some(existing_state) = existing_state {
                        // Resume if existing state matches the command run, otherwise it conflicts
                        if existing_state.source == Source::User(introspect.authorized_user_id)
                            && existing_state.references == references
                        {
                            return download(progress, client, existing_state, options).await;
                        }
                        existing_conflicting_state_error(&output)?;
                    }

                    check_empty_output(&output)?;

                    download(
                        progress,
                        client,
                        State::new(
                            Source::User(introspect.authorized_user_id),
                            Vec::new(),
                            references,
                            output,
                        ),
                        options,
                    )
                    .await
                }
            }
        }
    }
}
