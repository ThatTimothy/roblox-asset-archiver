use std::{
    fmt::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Error, Result, anyhow, bail};
use indicatif::{ProgressBar, ProgressFinish, ProgressStyle};
use log::{debug, trace, warn};
use tokio::{
    fs::{self, File},
    io::{AsyncWriteExt, BufWriter},
};

use crate::{
    api::{AssetDeliveryResponse, Client},
    progress::{Progress, message},
    state::{State, StateType, StateVersion},
};

const TEMP_FILE_EXTENSION: &str = "temp";
/// Downloads to a file by streaming response into it
async fn download_to_file<P: AsRef<Path>>(
    bar: &ProgressBar,
    client: &Client,
    delivery_response: AssetDeliveryResponse,
    path: P,
) -> Result<()> {
    // Asset metadata contains uncompressed length as attribute 1, this is undocumented so do not rely but fine for now
    let uncompressed_length = match delivery_response.asset_metadatas {
        Some(asset_metadatas) => asset_metadatas
            .iter()
            .find(|asset_metadata| asset_metadata.metadata_type == 1)
            .map(|asset_metadata| {
                asset_metadata
                    .value
                    .parse()
                    .expect("Invalid asset attribute")
            }),
        None => None,
    };

    let mut response = client
        .raw_get("asset_download".to_owned(), delivery_response.location)
        .await?;
    if uncompressed_length.is_some() {
        bar.set_length(uncompressed_length.unwrap());
        bar.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{bar:30.green}] {bytes}/{total_bytes} at {bytes_per_sec} ({eta}) • {msg}",
            )
            .unwrap(),
        )
    } else {
        bar.set_style(
            ProgressStyle::with_template(
                "{spinner:.blue} {msg}, {bytes} downloaded ({bytes_per_sec})",
            )
            .unwrap(),
        )
    }
    bar.set_position(0);
    bar.set_elapsed(Duration::ZERO);
    bar.set_message(format!("Downloading to `{}`", path.as_ref().display()));

    let temp_path = path.as_ref().with_extension({
        let mut extension = path.as_ref().extension().unwrap_or_default().to_os_string();
        extension.push(".");
        extension.push(TEMP_FILE_EXTENSION);
        extension
    });
    debug!("Start streaming content to `{}`", temp_path.display());
    let file = File::create(&temp_path).await?;
    let mut writer = BufWriter::new(file);
    while let Some(item) = response.chunk().await? {
        bar.inc(item.len() as u64);
        writer.write_all(&item).await?;
    }
    debug!("Streamed content to {}", &temp_path.display());
    fs::rename(&temp_path, &path).await.with_context(|| {
        format!(
            "Failed to commit rename from {} to {}",
            &temp_path.display(),
            &path.as_ref().display()
        )
    })?;
    debug!(
        "Committed content from {} to {}",
        &temp_path.display(),
        &path.as_ref().display()
    );
    Ok(())
}

pub async fn download_place(
    progress: Progress,
    client: Client,
    output_dir: String,
    mut state: State,
    concurrent: usize,
) -> Result<()> {
    let State {
        id: place_id,
        references,
        ..
    } = state;

    let dir_path = PathBuf::from(output_dir.clone());
    let spinner = progress.add(
        ProgressBar::new_spinner()
            .with_style(ProgressStyle::with_template("{spinner:.blue} {msg}").unwrap()),
    );
    spinner.enable_steady_tick(Duration::from_millis(100));

    match &state.place {
        None => {
            // If place not defined, haven't started a download before
            eprintln!(
                "Preparing download for place {place_id} to `{output_dir}` with references={references}"
            );
            // We fetch universe_id then place so we can validate the place exists
            message!(spinner, "Getting universe id for place {place_id}");
            let universe_id = async {
                let response = client.universe_containing_place(place_id).await?;
                response
                    .universe_id
                    .ok_or_else(|| anyhow!("universe_id returned was undefined"))
            }
            .await
            .context("Failed to get universe id for place - make sure the id is correct")?;
            debug!("Got universe_id={universe_id}");

            message!(spinner, "Fetching place {place_id}");
            let place = client
                .place(universe_id, place_id)
                .await
                .context("Failed to fetch place - make sure the id is correct")?;
            progress.print(format!(
                "Found place {place_id} in universe {universe_id}: `{}`",
                place.display_name
            ));

            // Update and save state
            state.place = Some(place);
            state.save(&dir_path).context("Failed to save state")?;
        }
        Some(place) => {
            // If it is defined, we're resuming a download
            let universe_id = place.universe_id();
            eprintln!(
                "Resuming download for place {place_id} in {universe_id}: `{}` to `{output_dir}` with references={references}",
                place.display_name
            );
        }
    };

    let have_version_metadata = state.versions.is_some();
    if !have_version_metadata {
        // Next step: download version metadata
        message!(
            spinner,
            "Fetching place version metadata for place {place_id}"
        );
        let place_versions = client
            .place_versions(place_id)
            .await
            .context("Failed to fetch place versions metadata")?;
        let version_padding = place_versions.len().to_string().len();
        progress.print(format!(
            "Got version metadata for {place_id}: {} versions",
            place_versions.len()
        ));

        // Version does not always have creator - this seems to happen with version 1 specifically but generalized here
        // If the version does not have a creator, the safe default is the first creator in all versions
        // Since versions are most recent to oldest, we start from back until we find a version that has a creator
        let first_creator = place_versions
            .iter()
            .rev()
            .find(|place_version| place_version.created_by.is_some())
            .map(|place_version| place_version.created_by.unwrap());
        if first_creator.is_none() {
            warn!("No version exists with a defined creator - creator will not be logged");
        }

        for place_version in place_versions.iter().rev() {
            if place_version.created_by.is_none() {
                warn!(
                    "Version {} has no defined creator, defaulting to first_creator={}",
                    place_version.version,
                    first_creator
                        .map(|first_creator| first_creator.to_string())
                        .unwrap_or("None, so will remain undefined".to_owned())
                );
            }
        }

        // Update and save state
        state.versions = Some(
            place_versions
                .into_iter()
                .rev()
                .map(|place_version| {
                    let version = place_version.version.parse().expect("Malformed version");
                    StateVersion {
                        version,
                        metadata: place_version,
                        filename: format!("v{version:0version_padding$}.rbxl"),
                    }
                })
                .collect(),
        );
        state.save(&dir_path).context("Failed to save state")?;
    }
    let state_versions = state.versions.as_ref().unwrap();
    spinner.finish_and_clear();

    // Remove temp files
    for entry in dir_path.read_dir()? {
        if let Ok(entry) = entry
            && let Some(extension) = entry.path().extension()
            && extension == TEMP_FILE_EXTENSION
        {
            fs::remove_file(entry.path()).await.with_context(|| {
                format!("Failed to remove temp file {}", entry.path().display())
            })?;
        }
    }

    // Only download files which do not already exists
    let mut to_download = Vec::new();
    for state_version in state_versions {
        let file_path = Path::join(&dir_path, state_version.filename.clone());
        if !file_path.exists() {
            to_download.push((
                state_version.metadata.version.parse::<u64>().unwrap(),
                file_path,
            ));
        }
    }
    trace!("To download: {:?}", to_download);

    let num_versions = state_versions.len();
    let num_to_download = to_download.len();

    if num_to_download == 0 {
        eprintln!("All versions already downloaded");
        return Ok(());
    }

    // If we already have version metadata, we already logged we are resuming so this message is redundant
    // Otherwise marks next phase
    if !have_version_metadata {
        eprintln!("Beginning download for place {place_id} to `{output_dir}`");
    }

    let bar = progress.add(
                ProgressBar::new(num_versions as u64)
                    .with_style(
                        ProgressStyle::with_template(
                            "[{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} ({eta}) • Downloading {per_sec}",
                        )
                        .unwrap().with_key("per_sec", |state: &indicatif::ProgressState, w: &mut dyn Write| write!(w, "{:.2} ver/s", state.per_sec()).unwrap()),
                    )
                    .with_finish(ProgressFinish::AndClear),
            );
    bar.set_position((num_versions - num_to_download) as u64);

    let concurrent = concurrent.min(num_to_download);
    let mut handles = Vec::new();
    let progress = Arc::new(progress);
    let next_download_index = Arc::new(AtomicUsize::new(0));
    let to_download = Arc::new(to_download);
    let bar = Arc::new(bar);
    let client = Arc::new(client);
    for _ in 0..concurrent {
        let progress = Arc::clone(&progress);
        let next_download_index = Arc::clone(&next_download_index);
        let to_download = Arc::clone(&to_download);
        let bar = Arc::clone(&bar);
        let client = Arc::clone(&client);

        let handle = tokio::spawn(async move {
            let file_bar =
                progress.add(ProgressBar::new_spinner().with_finish(ProgressFinish::AndClear));
            loop {
                // Ordering does not matter, but we need to have one thread do change at a time
                let download_index = next_download_index.fetch_add(1, Ordering::Relaxed);
                if download_index >= num_to_download {
                    // No more work to do
                    break;
                }
                let (version, file_path) = &to_download[download_index];

                debug!("Downloading version {version}");
                let delivery_response = client
                    .asset_version_delivery(place_id, *version)
                    .await
                    .context(format!(
                        "Failed to get asset delivery url for version {version}"
                    ))?;
                debug!("Got delivery URL for version {version}, starting download",);
                download_to_file(&file_bar, &client, delivery_response, &file_path).await?;
                bar.inc(1);
                progress.print(format!(
                    "Downloaded version {} to `{}`",
                    version,
                    file_path.display()
                ));
            }

            Ok::<(), Error>(())
        });

        handles.push(handle);
    }

    for handle in handles {
        handle.await??
    }

    eprintln!("Downloaded {num_versions} versions to `{output_dir}`");

    Ok(())
}

pub async fn download(
    progress: Progress,
    client: Client,
    output_dir: String,
    state: State,
    concurrent: usize,
) -> Result<()> {
    let introspect = client
        .introspect()
        .await
        .context("Failed to validate API key")?;

    if !introspect.enabled {
        bail!("API key {} is not enabled", introspect.name);
    }
    if introspect.expired {
        bail!("API key {} is expired", introspect.name);
    }

    debug!(
        "API key `{}` owned by {} validated, scopes: {}",
        introspect.name,
        introspect.authorized_user_id,
        introspect
            .scopes
            .iter()
            .map(|scope| scope.to_string())
            .collect::<Vec<_>>()
            .join(", "),
    );

    match state.state_type {
        StateType::Place => download_place(progress, client, output_dir, state, concurrent),
    }
    .await
}
