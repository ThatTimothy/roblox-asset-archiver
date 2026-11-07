use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use log::{debug, trace, warn};
use reqwest::Url;
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
};

use crate::api::Client;

#[derive(Clone, Debug)]
pub enum DownloadType {
    Place,
}

async fn download_to_file<P: AsRef<Path>>(client: &Client, url: String, path: P) -> Result<()> {
    let url = Url::parse(&url)?;
    let mut response = client.raw_get(url).await?;
    debug!("Start streaming content {}", path.as_ref().display());
    let mut file = File::create(&path).await?;
    while let Some(item) = response.chunk().await? {
        file.write_all(&item).await?;
    }
    debug!("Streamed content to {}", path.as_ref().display());
    Ok(())
}

pub async fn download(
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

            debug!("Getting universe id for place {place_id}");
            let universe_id = async {
                let response = client.universe_containing_place(place_id).await?;
                response
                    .universe_id
                    .ok_or_else(|| anyhow!("universe_id returned was undefined"))
            }
            .await
            .context("Failed to get universe id for place - make sure the id is correct")?;
            debug!("Got universe_id={universe_id}");

            debug!("Fetching place {place_id}");
            let place = client
                .place(universe_id, place_id)
                .await
                .context("Failed to fetch place - make sure the id is correct")?;
            eprintln!(
                "Found place {place_id} in universe {universe_id}: {}",
                place.display_name
            );

            debug!("Fetching place version metadata for place {place_id}");
            let place_versions = client
                .place_versions(place_id)
                .await
                .context("Failed to fetch place versions metadata")?;
            let versions = place_versions.len();
            eprintln!(
                "Got version metadata for {place_id}: {} versions",
                place_versions.len()
            );

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

            for place_version in place_versions.iter().rev() {
                let version: u64 = place_version.version.parse().context("Malformed version")?;
                eprintln!("Downloading version {}/{}", version, place_versions.len());
                let delivery_response = client
                    .asset_version_delivery(place_id, version)
                    .await
                    .context(format!(
                        "Failed to get asset delivery url for version {version}"
                    ))?;
                debug!("Got delivery URL for version {version}, starting download",);
                let file_path = Path::join(dir_path, format!("v{version}.rbxl"));
                download_to_file(&client, delivery_response.location, &file_path).await?;
                eprintln!(
                    "Downloaded version {}/{} to `{}`",
                    version,
                    place_versions.len(),
                    file_path.display()
                );
            }

            eprintln!("Downloaded {versions} versions to `{output_dir}`")
        }
    };

    Ok(())
}
