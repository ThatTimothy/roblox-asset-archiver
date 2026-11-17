use std::{
    collections::HashSet,
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
    io::{AsyncReadExt, AsyncWriteExt, BufWriter},
};

use crate::{
    api::{AssetType, Client},
    cli::DownloadOptions,
    progress::{Progress, message},
    state::{State, StateAsset, StateVersion, StateVersionMetadata},
};

const TEMP_FILE_EXTENSION: &str = "temp";

async fn get_asset(progress: &Progress, client: &Client, state: &mut State) -> Result<AssetType> {
    // If it is defined, resuming and no work to be done
    if let Some(state_asset) = &state.asset {
        let asset_type = state_asset.asset.asset_type.clone();

        return Ok(asset_type);
    }

    let spinner = progress.add(
        ProgressBar::new_spinner()
            .with_style(ProgressStyle::with_template("{spinner:.blue} {msg}").unwrap()),
    );
    spinner.enable_steady_tick(Duration::from_millis(100));

    // Get asset information
    let id = state.id;
    message!(spinner, "Fetching {id}");
    let asset = client
        .asset(id)
        .await
        .with_context(|| format!("Failed to fetch {id} - make sure the id is correct"))?;

    let universe = match asset.asset_type {
        AssetType::Place => {
            // We fetch universe_id to confirm universe exists for places
            message!(spinner, "Getting universe id for place {id}");
            let universe_id = async {
                let response = client.universe_containing_place(id).await?;
                response
                    .universe_id
                    .ok_or_else(|| anyhow!("universe_id returned was undefined"))
            }
            .await
            .context("Failed to get universe id for place - make sure the id is correct")?;
            debug!("Got universe_id={universe_id}");

            message!(spinner, "Getting universe {id}");
            let universe = client
                .universe(universe_id)
                .await
                .with_context(|| format!("Failed to fetch universe {universe_id}"))?;
            debug!("Got universe {universe_id} \"{}\"", universe.display_name);

            Some(universe)
        }
        _ => {
            // Otherwise, no universe id
            None
        }
    };

    let place = if asset.asset_type == AssetType::Place {
        message!(spinner, "Getting place {id}");
        let place = client
            .place(universe.as_ref().unwrap().universe_id(), id)
            .await
            .with_context(|| format!("Failed to fetch place {id}"))?;
        debug!("Got place {id} \"{}\"", place.display_name);

        Some(place)
    } else {
        None
    };

    progress.print(format!(
        "Found {} {id} \"{}\"{}",
        asset.asset_type,
        match &universe {
            Some(universe) => format!(
                " in universe {} \"{}\"",
                universe.universe_id(),
                universe.display_name
            ),
            None => "".to_owned(),
        },
        asset.display_name,
    ));

    if let AssetType::Other(asset_type) = &asset.asset_type {
        warn!("Downloading unknown asset type {asset_type}");
    }
    let asset_type = asset.asset_type.clone();

    // Update and save state
    state.asset = Some(StateAsset {
        asset,
        place,
        universe,
    });
    state.save().context("Failed to save state")?;

    Ok(asset_type)
}

async fn get_versions(
    progress: &Progress,
    client: &Client,
    state: &mut State,
    asset_type: &AssetType,
) -> Result<()> {
    // If defined, resuming and no work to be done
    if state.versions.is_some() {
        return Ok(());
    }

    let spinner = progress.add(
        ProgressBar::new_spinner()
            .with_style(ProgressStyle::with_template("{spinner:.blue} {msg}").unwrap()),
    );
    spinner.enable_steady_tick(Duration::from_millis(100));
    // Download version metadata
    let id = state.id;
    let versions_metadata: Vec<StateVersionMetadata> = match asset_type {
        AssetType::Place => {
            // Place versions have a special endpoint that includes publish notes
            message!(
                spinner,
                "Fetching place version metadata for {asset_type} {id}"
            );
            let place_versions = client
                .place_versions(id)
                .await
                .context("Failed to fetch place versions metadata")?;
            progress.print(format!(
                "Got place version metadata for {asset_type} {id}: {} versions",
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

            let versions_metadata = place_versions
                .into_iter()
                .rev()
                .map(|place_version| StateVersionMetadata::Place(place_version))
                .collect();
            versions_metadata
        }
        _ => {
            // Every other type uses standard asset versions
            message!(
                spinner,
                "Fetching asset version metadata for {asset_type} {id}"
            );
            let asset_versions = client
                .asset_versions(id)
                .await
                .context("Failed to fetch asset version metadata")?;
            progress.print(format!(
                "Got asset version metadata for {asset_type} {id}: {} versions",
                asset_versions.len()
            ));

            let versions_metadata = asset_versions
                .into_iter()
                .rev()
                .map(|asset_version| StateVersionMetadata::Asset(asset_version))
                .collect();
            versions_metadata
        }
    };

    let version_padding = versions_metadata.len().to_string().len();
    let versions = versions_metadata
        .into_iter()
        .map(|version_metadata| {
            let version = version_metadata.version();
            StateVersion {
                version,
                metadata: version_metadata,
                file_prefix: format!("v{version:0version_padding$}"),
            }
        })
        .collect();
    // Update and save state
    state.versions = Some(versions);
    state.save().context("Failed to save state")?;

    Ok(())
}

/// Determines a file extension a download based on asset_type and magic header
async fn file_extension(asset_type: &AssetType, path: &Path) -> Result<&'static str> {
    let file = File::open(&path).await?;
    let mut header = Vec::with_capacity(1024);
    file.take(1024)
        .read_to_end(&mut header)
        .await
        .with_context(|| format!("Failed to read header from {}", &path.display()))?;
    let extension = match header {
        // Per rbx-dom docs, binary formats rbxl and rbxm have this magic header: https://github.com/rojo-rbx/rbx-dom/blob/master/docs/xml.md#file-structure
        header if header.starts_with(b"<roblox!") => match asset_type {
            AssetType::Place => "rbxl",
            _ => "rbxm",
        },
        // Per rbx-dom docs, xml formats rbxlx and rbxmx have this magic header: https://github.com/rojo-rbx/rbx-dom/blob/master/docs/xml.md#file-structure
        header if header.starts_with(b"<roblox") => match asset_type {
            AssetType::Place => "rbxlx",
            _ => "rbxmx",
        },
        // Per MaximumADHD's reverse engineering, meshes use proprietary format: https://devforum.roblox.com/t/roblox-filemesh-format-specification/326114
        // For sanity purposes I'm not writing a conversion for this, but if you want to this is open for PR but probably needs to be a separate project this supports
        header if header.starts_with(b"version") && asset_type == &AssetType::Mesh => "mesh",
        header => {
            let kind = infer::get(&header)
                .with_context(|| format!("Unknown header {header:?} for {asset_type}"))?;

            kind.extension()
        }
    };

    Ok(extension)
}

/// Downloads to an asset version to a file
///
/// First, contacts asset delivery to find cdn url.
/// Then, downloads to `{prefix_path}.temp` - this ensures that partial downloads are always `.temp`.
/// Next, moves to proper file extension after download is complete.
/// As such, `prefix_path` should not have an extension since it will be overridden.
///
/// Returns the final path
async fn download_asset_version(
    bar: &ProgressBar,
    client: &Client,
    id: u64,
    version: u64,
    asset_type: &AssetType,
    prefix_path: &Path,
) -> Result<PathBuf> {
    debug!("Getting asset delivery url for {id} version {version}");
    let delivery_response = client
        .asset_version_delivery(id, version)
        .await
        .context(format!(
            "Failed to get asset delivery url for version {version}"
        ))?;
    let uncompressed_length = delivery_response.uncompressed_length();

    // Make request, raw_get so content can be streamed to file
    debug!("Got delivery URL for {id} version {version}, making request");
    let mut response = client
        .raw_get("asset_download".to_owned(), delivery_response.location)
        .await?;

    // Use progress bar if length defined, otherwise a spinner
    if let Some(uncompressed_length) = uncompressed_length {
        bar.set_length(uncompressed_length);
        bar.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{bar:30.green}] {bytes}/{total_bytes} at {bytes_per_sec} ({eta}) • {msg}",
            )
            .unwrap(),
        )
    } else {
        bar.set_style(
            ProgressStyle::with_template(
                "{spinner:.blue} {msg}, {bytes} downloaded ({bytes_per_sec}) • {msg}",
            )
            .unwrap(),
        )
    }
    bar.set_position(0);
    bar.set_elapsed(Duration::ZERO);
    bar.set_message(format!("Downloading to `{}`", prefix_path.display()));

    // Stream to temp path use BufWriter
    let temp_path = prefix_path.with_extension("temp");
    debug!("Start streaming content to `{}`", temp_path.display());
    {
        let file = File::create(&temp_path).await?;
        let mut writer = BufWriter::new(file);
        while let Some(item) = response.chunk().await? {
            bar.inc(item.len() as u64);
            writer.write_all(&item).await?;
        }
        writer.flush().await?;
    }
    debug!("Streamed content to {}", &temp_path.display());

    // Commit by renaming to proper extension
    let extension = file_extension(asset_type, &temp_path)
        .await
        .with_context(|| {
            format!(
                "Failed to determine file extension for {}",
                temp_path.display()
            )
        })?;
    let file_path = prefix_path.with_extension(extension);
    fs::rename(&temp_path, &file_path).await.with_context(|| {
        format!(
            "Failed to commit rename from {} to {}",
            &temp_path.display(),
            &file_path.display()
        )
    })?;
    debug!(
        "Committed content from {} to {}",
        &temp_path.display(),
        &file_path.display()
    );
    Ok(file_path)
}

async fn download_asset(
    progress: Progress,
    client: Client,
    state: State,
    asset_type: &AssetType,
    options: &DownloadOptions,
) -> Result<()> {
    assert!(state.versions.is_some());

    // Remove temp files & store existing file prefixes
    let mut existing = HashSet::new();
    for entry in state.output.read_dir()? {
        if let Ok(entry) = entry {
            let path = entry.path();

            // Remove any temp files
            if let Some(extension) = entry.path().extension()
                && extension == TEMP_FILE_EXTENSION
            {
                fs::remove_file(entry.path()).await.with_context(|| {
                    format!("Failed to remove temp file {}", entry.path().display())
                })?;
            } else if let Some(file_prefix) = path.file_prefix()
                && let Some(file_prefix) = file_prefix.to_str()
            {
                // If not a temp file, store the file prefix
                existing.insert(file_prefix.to_owned());
            }
        }
    }

    // Only download files within versions bounds and do not already exist
    let id = state.id;
    let state_versions = state.versions.as_ref().unwrap();
    let num_versions = state_versions.len();
    let mut min = 1usize;
    let mut max = num_versions;
    if let Some(first) = options.first {
        max = first.get().min(num_versions);
    } else if let Some(last) = options.last {
        min = num_versions - (last.get().min(num_versions)) + 1;
    }
    let num_selected = max - min + 1;
    let mut to_download = Vec::new();
    for state_version in &state_versions[min - 1..=max - 1] {
        let file_prefix = &state_version.file_prefix;
        if !existing.contains(file_prefix) {
            let file_prefix_path = state.output.join(file_prefix);
            to_download.push((state_version.version, asset_type.clone(), file_prefix_path));
        }
    }
    trace!("To download: {:?}", to_download);

    let num_to_download = to_download.len();

    if num_to_download == 0 {
        progress.print("All versions already downloaded");
        return Ok(());
    }

    progress.print(format!(
        "Downloading {num_to_download} versions for {asset_type} {id} to `{}`{}",
        state.output.display(),
        if num_selected > num_to_download {
            format!(
                ", {}/{num_selected} already downloaded",
                num_selected - num_to_download
            )
        } else {
            "".to_owned()
        }
    ));

    let bar = progress.add(
                ProgressBar::new(num_selected as u64)
                    .with_style(
                        ProgressStyle::with_template(
                            "[{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} ({eta}) • Downloading {per_sec}",
                        )
                        .unwrap().with_key("per_sec", |state: &indicatif::ProgressState, w: &mut dyn Write| write!(w, "{:.2} ver/s", state.per_sec()).unwrap()),
                    )
                    .with_finish(ProgressFinish::AndClear),
            );
    bar.set_position((num_selected - num_to_download) as u64);

    let concurrent = options.concurrent.min(num_to_download);
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
                let (version, asset_type, file_prefix_path) = &to_download[download_index];
                let file_path = download_asset_version(
                    &file_bar,
                    &client,
                    id,
                    *version,
                    asset_type,
                    file_prefix_path,
                )
                .await?;

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

    progress.print(format!(
        "Downloaded {num_selected} versions to `{}`",
        state.output.display()
    ));

    Ok(())
}

pub async fn download(
    progress: Progress,
    client: Client,
    mut state: State,
    options: DownloadOptions,
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

    let State { id, references, .. } = state;

    if references {
        todo!()
    }

    // Output resuming or preparing based on existing asset information
    if let Some(state_asset) = &state.asset {
        let asset = &state_asset.asset;
        eprintln!(
            "Resuming download for {} {id} \"{}\"{} to `{}` with references={references}",
            asset.asset_type,
            asset.display_name,
            match &state_asset.universe {
                Some(universe) => format!(
                    " in universe {} \"{}\"",
                    universe.universe_id(),
                    universe.display_name
                ),
                None => "".to_owned(),
            },
            state.output.display()
        );
    } else {
        eprintln!(
            "Preparing download for {id} to `{}` with references={references}",
            state.output.display()
        );
    }

    let asset_type = get_asset(&progress, &client, &mut state).await?;
    get_versions(&progress, &client, &mut state, &asset_type).await?;
    download_asset(progress, client, state, &asset_type, &options).await?;

    Ok(())
}
