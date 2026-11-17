use std::{
    fmt::Display,
    fs::{self, File},
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::api::{Asset, AssetVersion, Place, PlaceVersion, Universe};

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StateAsset {
    pub asset: Asset,
    pub place: Option<Place>,
    pub universe: Option<Universe>,
}

impl Display for StateAsset {
    /// Displays an asset as `{type} {id} "{name}"`, including `in universe {id} "{name}"` for alternate form (#)
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} \"{}\"",
            self.asset.asset_type, self.asset.asset_id, self.asset.display_name,
        )?;

        if f.alternate()
            && let Some(universe) = &self.universe
        {
            write!(
                f,
                " in universe {} \"{}\"",
                universe.universe_id(),
                universe.display_name
            )?;
        }

        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub enum StateVersionMetadata {
    Place(PlaceVersion),
    Asset(AssetVersion),
}

impl StateVersionMetadata {
    pub fn version(&self) -> u64 {
        match self {
            Self::Place(place_version) => place_version.version(),
            Self::Asset(asset_version) => asset_version.version(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StateVersion {
    pub version: u64,
    pub metadata: StateVersionMetadata,
    pub file_prefix: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct State {
    schema_version: String,
    pub id: u64,
    pub references: bool,
    pub asset: Option<StateAsset>,
    pub versions: Option<Vec<StateVersion>>,

    #[serde(skip)]
    pub output: PathBuf,
}

impl State {
    pub const STATE_FILENAME: &str = concat!(env!("CARGO_PKG_NAME"), ".json");
    pub const STATE_SCHEMA_VERSION: &str = concat!(env!("CARGO_PKG_NAME"), ":", "1");

    pub fn new(id: u64, references: bool, output: PathBuf) -> Self {
        Self {
            schema_version: Self::STATE_SCHEMA_VERSION.to_owned(),
            id,
            references,
            asset: None,
            versions: None,
            output,
        }
    }

    pub fn load(output: &Path) -> Result<Option<Self>> {
        let state_path = output.join(Self::STATE_FILENAME);
        if !state_path.exists() {
            return Ok(None);
        }

        let file = File::open(&state_path)
            .with_context(|| format!("Failed to open `{}`", &state_path.display()))?;
        let reader = BufReader::new(file);
        let mut state: State = serde_json::from_reader(reader)
            .with_context(|| format!("Failed to read from `{}`", &state_path.display()))?;

        if state.schema_version != Self::STATE_SCHEMA_VERSION {
            bail!(
                "Expected schema version {}, read schema version `{}`",
                Self::STATE_SCHEMA_VERSION,
                state.schema_version
            )
        }

        state.output = output.into();

        Ok(Some(state))
    }

    pub fn save(&self) -> Result<()> {
        let state_path = self.output.join(Self::STATE_FILENAME);
        let temp_state_path = self.output.join(Self::STATE_FILENAME.to_owned() + ".temp");

        // Cleanup old copy
        if temp_state_path.exists() {
            fs::remove_file(&temp_state_path)
                .with_context(|| format!("Failed to remove `{}`", &temp_state_path.display()))?
        }

        // We write to .temp version first, then move it to actual location to prevent mid-write crash causing corrupted file
        fs::create_dir_all(&self.output)
            .with_context(|| format!("Failed to create {}", self.output.display()))?;
        let file = File::create(&temp_state_path)
            .with_context(|| format!("Failed to open `{}`", &temp_state_path.display()))?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, &self)
            .with_context(|| format!("Failed to save to `{}`", &temp_state_path.display()))?;
        writer
            .flush()
            .with_context(|| format!("Failed to flush to `{}`", &temp_state_path.display()))?;
        fs::rename(&temp_state_path, &state_path).with_context(|| {
            format!(
                "Failed to commit by renaming `{}` to `{}`",
                &temp_state_path.display(),
                &state_path.display()
            )
        })?;
        Ok(())
    }
}
