use std::{
    collections::{BTreeMap, HashMap},
    fmt::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use indicatif::{ProgressBar, ProgressFinish, ProgressStyle};
use log::{debug, trace, warn};
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncWriteExt, BufWriter},
    task::JoinHandle,
};

use crate::{
    api::{AssetType, Client},
    cli::DownloadOptions,
    progress::{Progress, message},
    state::{Downloaded, State, StateAsset, Version},
};

const TEMP_FILE_EXTENSION: &str = "temp";

/// Util to run `tasks` on `concurrent` workers that call `worker` with `progress, client, bar, task`.
/// Handles creating & updating `overall` progress.
/// `bar` is provided and shared for a worker to prevent flicking/reordering of bars.
async fn run_concurrent_with_progress<T, W, WFut, R>(
    progress: Arc<Progress>,
    client: Arc<Client>,
    concurrent: usize,
    overall_style: ProgressStyle,
    overall_extra: usize,
    tasks: Vec<T>,
    worker: W,
) -> Result<Vec<R>>
where
    T: Clone + Send + Sync + 'static,
    W: Fn(Arc<Progress>, Arc<Client>, Arc<ProgressBar>, T) -> WFut + Send + Sync + 'static,
    R: std::fmt::Debug + Send + Sync + 'static,
    WFut: Future<Output = Result<R>> + Send + Sync + 'static,
{
    let concurrent = concurrent.min(tasks.len());
    let mut handles: Vec<JoinHandle<Result<()>>> = Vec::with_capacity(concurrent);
    let next = Arc::new(AtomicUsize::new(0));
    let worker = Arc::new(worker);
    let tasks = Arc::new(tasks);
    let results: Arc<Vec<OnceLock<R>>> =
        Arc::new((0..tasks.len()).map(|_| OnceLock::new()).collect());

    let overall = progress.add(
        ProgressBar::new((tasks.len() + overall_extra) as u64)
            .with_style(overall_style)
            .with_finish(ProgressFinish::AndLeave),
    );
    overall.set_position(overall_extra as u64);
    overall.enable_steady_tick(Duration::from_millis(100));

    for _ in 0..concurrent {
        let progress = progress.clone();
        let client = client.clone();
        let next = next.clone();
        let worker = worker.clone();
        let tasks = tasks.clone();
        let results = results.clone();
        let overall = overall.clone();
        let handle = tokio::spawn(async move {
            let bar = Arc::new(
                progress
                    .add(ProgressBar::new_spinner().with_style(
                        ProgressStyle::with_template("{spinner:.blue} {msg}").unwrap(),
                    )),
            );
            bar.enable_steady_tick(Duration::from_millis(100));
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= tasks.len() {
                    // No more work to do
                    break;
                }

                let task = tasks[i].clone();
                let result = worker(progress.clone(), client.clone(), bar.clone(), task).await?;
                results[i].set(result).unwrap();
                overall.inc(1);
            }
            bar.finish_and_clear();

            Ok(())
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.await??
    }

    let results = Arc::try_unwrap(results)
        .unwrap()
        .into_iter()
        .map(|result| result.into_inner().unwrap())
        .collect();
    Ok(results)
}

/// Gets a single asset
async fn get_asset(client: &Client, spinner: &ProgressBar, id: u64) -> Result<StateAsset> {
    // Get asset information
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

    if let AssetType::Other(asset_type) = &asset.asset_type {
        warn!("Downloading unknown asset type {asset_type}");
    }

    let state_asset = StateAsset {
        asset,
        place,
        universe,
        versions: None,
        downloaded: None,
    };

    Ok(state_asset)
}

/// Gets all the assets that are not yet fetched
async fn get_all_assets(
    progress: Arc<Progress>,
    client: Arc<Client>,
    state: &mut State,
    options: &DownloadOptions,
) -> Result<()> {
    let to_fetch = state
        .ids
        .iter()
        .filter(|id| !state.assets.contains_key(id))
        .copied()
        .collect::<Vec<_>>();

    // If nothing to fetch, no work to be done
    if to_fetch.len() == 0 {
        return Ok(());
    }

    // Fetch missing assets concurrently
    let overall_style = ProgressStyle::with_template(
        "[{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} ({eta}) • Fetching assets at {per_sec}",
    )
    .unwrap()
    .with_key(
        "per_sec",
        |state: &indicatif::ProgressState, w: &mut dyn Write| {
            write!(w, "{:.2}/s", state.per_sec()).unwrap()
        },
    );
    let assets = run_concurrent_with_progress(
        progress,
        client,
        options.concurrent,
        overall_style,
        state.ids.len() - to_fetch.len(),
        to_fetch.clone(),
        move |progress, client, spinner, id| async move {
            let state_asset = get_asset(&client, &spinner, id).await?;
            progress.print(format!("Found {state_asset:#}"));
            Ok(state_asset)
        },
    )
    .await?;

    // Save assets
    for (id, asset) in to_fetch.iter().zip(assets) {
        state.assets.insert(*id, asset);
    }
    state.save().context("Failed to save state")?;

    Ok(())
}

/// Gets the versions for a specific asset
async fn get_asset_versions(
    client: &Client,
    spinner: &ProgressBar,
    id: u64,
    asset_type: &AssetType,
    asset_display: &str,
) -> Result<BTreeMap<u64, Version>> {
    // Get versions
    let versions = match asset_type {
        AssetType::Place => {
            // Place versions have a special endpoint that includes publish notes
            message!(spinner, "Fetching place versions info for {asset_display}");
            let place_versions = client
                .place_versions(id)
                .await
                .context("Failed to fetch place versions info")?;

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

            place_versions
                .into_iter()
                .rev()
                .map(|place_version| (place_version.version(), Version::Place(place_version)))
                .collect()
        }
        _ => {
            // Every other type uses standard asset versions
            message!(spinner, "Fetching asset versions info for {asset_display}");
            let asset_versions = client
                .asset_versions(id)
                .await
                .context("Failed to fetch asset versions info")?;

            asset_versions
                .into_iter()
                .rev()
                .map(|asset_version| (asset_version.version(), Version::Asset(asset_version)))
                .collect()
        }
    };

    Ok(versions)
}

async fn get_all_versions(
    progress: Arc<Progress>,
    client: Arc<Client>,
    state: &mut State,
    options: &DownloadOptions,
) -> Result<()> {
    let to_fetch = state
        .assets
        .iter()
        .filter_map(|(id, asset)| asset.versions.is_none().then_some(*id))
        .collect::<Vec<_>>();

    // If nothing to fetch, no work to be done
    if to_fetch.len() == 0 {
        return Ok(());
    }

    // Fetch missing asset versions concurrently
    let overall_style = ProgressStyle::with_template(
        "[{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} ({eta}) • Fetching versions info at {per_sec}",
    )
    .unwrap()
    .with_key(
        "per_sec",
        |state: &indicatif::ProgressState, w: &mut dyn Write| {
            write!(w, "{:.2}/s", state.per_sec()).unwrap()
        },
    );
    let tasks = to_fetch
        .iter()
        .map(|id| {
            let state_asset = state.assets.get(id).unwrap();
            (
                *id,
                state_asset.asset.asset_type.clone(),
                format!("{state_asset}"),
            )
        })
        .collect();
    let versions = run_concurrent_with_progress(
        progress,
        client,
        options.concurrent,
        overall_style,
        state.assets.len() - to_fetch.len(),
        tasks,
        move |progress, client, spinner, (id, asset_type, asset_display)| async move {
            let versions =
                get_asset_versions(&client, &spinner, id, &asset_type, &asset_display).await?;
            progress.print(format!(
                "Got asset version metadata for {asset_display}: {} versions",
                versions.len()
            ));
            Ok(versions)
        },
    )
    .await?;

    // Save assets
    for (id, versions) in to_fetch.iter().zip(versions) {
        state.assets.get_mut(id).unwrap().versions = Some(versions);
    }
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
    client: &Client,
    bar: &ProgressBar,
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

/// Removes leftover temp files & returns existing file prefixes mapped to their file name
async fn remove_temp_and_get_existing(dir: &Path) -> Result<HashMap<String, String>> {
    let mut existing = HashMap::new();

    for entry in dir.read_dir()? {
        if let Ok(entry) = entry {
            let path = entry.path();

            if let Some(extension) = entry.path().extension()
                && extension == TEMP_FILE_EXTENSION
            {
                // Remove any temp files
                fs::remove_file(entry.path()).await.with_context(|| {
                    format!("Failed to remove temp file {}", entry.path().display())
                })?;
            } else if let Some(file_name) = path.file_name()
                && let Some(file_name) = file_name.to_str()
                && let Some(file_prefix) = path.file_prefix()
                && let Some(file_prefix) = file_prefix.to_str()
            {
                // If not a temp file, store the file prefix
                existing.insert(file_prefix.to_owned(), file_name.to_owned());
            }
        }
    }

    Ok(existing)
}

async fn download_all_assets(
    progress: Arc<Progress>,
    client: Arc<Client>,
    state: &mut State,
    options: &DownloadOptions,
) -> Result<()> {
    // Only download files within versions bounds and do not already exist
    let mut to_download = Vec::new();
    let mut num_selected = 0;
    for (id, state_asset) in &mut state.assets {
        let state_versions = state_asset.versions.as_ref().unwrap();

        let downloaded_path =
            PathBuf::from(format!("{}", state_asset.asset.asset_type)).join(format!("{id}"));
        let output_path = state.output.join(&downloaded_path);
        fs::create_dir_all(&output_path).await.with_context(|| {
            format!("Failed to create directories for {}", output_path.display())
        })?;
        if state_asset.downloaded.is_none() {
            state_asset.downloaded = Some(Downloaded {
                path: downloaded_path.to_string_lossy().to_string(),
                filenames: BTreeMap::new(),
            });
        }
        let downloaded = state_asset.downloaded.as_mut().unwrap();
        let existing = remove_temp_and_get_existing(&output_path).await?;

        let num_versions = state_versions.len();
        let version_padding = num_versions.to_string().len();

        let mut min = 1usize;
        let mut max = num_versions;
        if let Some(first) = options.first {
            max = first.get().min(num_versions);
        } else if let Some(last) = options.last {
            min = num_versions - (last.get().min(num_versions)) + 1;
        }

        for version in min..=max {
            let file_prefix = format!("v{version:0version_padding$}");

            // If it doesn't exist, we need to download it
            if !existing.contains_key(&file_prefix) {
                let file_prefix_path = output_path.join(file_prefix);
                to_download.push((
                    *id,
                    version as u64,
                    state_asset.asset.asset_type.clone(),
                    file_prefix_path,
                ));
            } else {
                // Otherwise, mark downloaded since we already have the file
                downloaded
                    .filenames
                    .insert(version as u64, existing.get(&file_prefix).unwrap().clone());
            }
        }

        num_selected += max - min + 1;
    }
    trace!("To download: {:?}", to_download);

    // Check if already done
    let num_to_download = to_download.len();
    if num_to_download == 0 {
        progress.print("All versions already downloaded");
        return Ok(());
    }

    // Otherwise, concurrently download all versions
    progress.print(format!(
        "Downloading {num_to_download} versions of {} assets to `{}`{}",
        state.assets.len(),
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

    let overall_style = ProgressStyle::with_template(
        "[{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} ({eta}) • Downloading {per_sec}",
    )
    .unwrap()
    .with_key(
        "per_sec",
        |state: &indicatif::ProgressState, w: &mut dyn Write| {
            write!(w, "{:.2} ver/s", state.per_sec()).unwrap()
        },
    );
    let downloaded_locations = run_concurrent_with_progress(
        progress.clone(),
        client,
        options.concurrent,
        overall_style,
        num_selected - num_to_download,
        to_download,
        move |progress, client, spinner, (id, version, asset_type, file_prefix_path)| async move {
            let file_path = download_asset_version(
                &client,
                &spinner,
                id,
                version,
                &asset_type,
                &file_prefix_path,
            )
            .await?;

            progress.print(format!(
                "Downloaded {id} version {} to `{}`",
                version,
                file_path.display()
            ));

            Ok((id, version, file_path))
        },
    )
    .await?;

    progress.print(format!(
        "Downloaded {num_selected} versions of {} assets to `{}`",
        state.assets.len(),
        state.output.display()
    ));

    // Save downloaded locations
    for (id, version, file_path) in downloaded_locations {
        let filenames = &mut state
            .assets
            .get_mut(&id)
            .unwrap()
            .downloaded
            .as_mut()
            .unwrap()
            .filenames;
        filenames.insert(
            version,
            file_path.file_name().unwrap().to_string_lossy().to_string(),
        );
    }
    state.save().context("Failed to save state")?;

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

    let State { references, .. } = state;

    if references {
        todo!()
    }

    // Output resuming or preparing based on existing asset information
    if !state.assets.is_empty() {
        // eprintln!(
        //     "Resuming download for {state_asset:#} to `{}` with references={references}",
        //     state.output.display()
        // );
    } else {
        // eprintln!(
        //     "Preparing download for {id} to `{}` with references={references}",
        //     state.output.display()
        // );
    }

    let progress = Arc::new(progress);
    let client = Arc::new(client);

    get_all_assets(progress.clone(), client.clone(), &mut state, &options).await?;
    get_all_versions(progress.clone(), client.clone(), &mut state, &options).await?;
    download_all_assets(progress, client, &mut state, &options).await?;

    Ok(())
}
