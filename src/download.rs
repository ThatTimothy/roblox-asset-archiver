use std::{path::Path, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use indicatif::{ProgressBar, ProgressFinish, ProgressStyle};
use log::{debug, warn};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
};

use crate::{
    api::{AssetDeliveryResponse, Client},
    progress::{Progress, message},
};

#[derive(Clone, Debug)]
pub enum DownloadType {
    Place,
}

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

    let mut response = client.raw_get(delivery_response.location).await?;
    debug!("Start streaming content {}", path.as_ref().display());
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
    let mut file = File::create(&path).await?;
    while let Some(item) = response.chunk().await? {
        bar.inc(item.len() as u64);
        file.write_all(&item).await?;
    }
    debug!("Streamed content to {}", path.as_ref().display());
    Ok(())
}

pub async fn download(
    progress: Progress,
    client: Client,
    output_dir: String,
    download_type: DownloadType,
    place_id: u64,
    references: bool,
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
        "API key `{}` owned by {} validated, scopes: {:#?}",
        introspect.name, introspect.authorized_user_id, introspect.scopes
    );

    match download_type {
        DownloadType::Place => {
            eprintln!(
                "Preparing download for place {place_id} to `{output_dir}` with references={references}"
            );

            let spinner =
                progress
                    .add(ProgressBar::new_spinner().with_style(
                        ProgressStyle::with_template("{spinner:.blue} {msg}").unwrap(),
                    ));
            spinner.enable_steady_tick(Duration::from_millis(100));

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
                "Found place {place_id} in universe {universe_id}: {}",
                place.display_name
            ));

            message!(
                spinner,
                "Fetching place version metadata for place {place_id}"
            );
            let place_versions = client
                .place_versions(place_id)
                .await
                .context("Failed to fetch place versions metadata")?;
            let versions = place_versions.len();
            let version_padding = versions.to_string().len();
            progress.print(format!(
                "Got version metadata for {place_id}: {} versions",
                place_versions.len()
            ));
            spinner.finish_and_clear();

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

            eprintln!("Beginning download for place {place_id} to `{output_dir}`");
            let dir_path = Path::new(&output_dir);
            fs::create_dir_all(dir_path)
                .await
                .context(format!("Failed to create {output_dir}"))?;

            let bar = progress.add(
                ProgressBar::new(versions as u64)
                    .with_style(
                        ProgressStyle::with_template(
                            "[{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} ({eta}) • {msg}",
                        )
                        .unwrap(),
                    )
                    .with_finish(ProgressFinish::AndClear),
            );
            let file_bar = progress.add(ProgressBar::new_spinner());
            for place_version in place_versions.iter().rev() {
                let version: u64 = place_version.version.parse().context("Malformed version")?;
                bar.inc(1);
                message!(
                    bar,
                    "Downloading version {} at {:.2} ver/sec",
                    version,
                    bar.per_sec()
                );
                let delivery_response = client
                    .asset_version_delivery(place_id, version)
                    .await
                    .context(format!(
                        "Failed to get asset delivery url for version {version}"
                    ))?;
                debug!("Got delivery URL for version {version}, starting download",);
                let file_path = Path::join(
                    dir_path,
                    format!("v{version:0width$}.rbxl", width = version_padding),
                );
                download_to_file(&file_bar, &client, delivery_response, &file_path).await?;
                progress.print(format!(
                    "Downloaded version {} to `{}`",
                    version,
                    file_path.display()
                ));
            }
            file_bar.finish_and_clear();

            eprintln!("Downloaded {versions} versions to `{output_dir}`")
        }
    };

    Ok(())
}
