use std::{
    fs::{self, File},
    io::{BufReader, BufWriter, Write},
    path::Path,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::api::{Place, PlaceVersion};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub enum StateType {
    Place,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StateVersion {
    pub version: u64,
    pub metadata: PlaceVersion,
    pub filename: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct State {
    #[serde(rename = "type")]
    pub state_type: StateType,
    schema_version: String,
    pub id: u64,
    pub references: bool,
    pub place: Option<Place>,
    pub versions: Option<Vec<StateVersion>>,
}

impl State {
    pub const STATE_FILENAME: &str = concat!(env!("CARGO_PKG_NAME"), ".json");
    pub const STATE_SCHEMA_VERSION: &str = concat!(env!("CARGO_PKG_NAME"), ":", "1");

    pub fn new(state_type: StateType, id: u64, references: bool) -> Self {
        Self {
            state_type,
            schema_version: Self::STATE_SCHEMA_VERSION.to_owned(),
            id,
            references,
            place: None,
            versions: None,
        }
    }

    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let state_path = dir.join(Self::STATE_FILENAME);
        if !state_path.exists() {
            return Ok(None);
        }

        let file = File::open(&state_path)
            .with_context(|| format!("Failed to open `{}`", &state_path.display()))?;
        let reader = BufReader::new(file);
        let state: State = serde_json::from_reader(reader)
            .with_context(|| format!("Failed to read from `{}`", &state_path.display()))?;

        if state.schema_version != Self::STATE_SCHEMA_VERSION {
            bail!(
                "Expected schema version {}, read schema version `{}`",
                Self::STATE_SCHEMA_VERSION,
                state.schema_version
            )
        }

        Ok(Some(state))
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let state_path = dir.join(Self::STATE_FILENAME);
        let temp_state_path = dir.join(Self::STATE_FILENAME.to_owned() + ".temp");

        // Cleanup old copy
        if temp_state_path.exists() {
            fs::remove_file(&temp_state_path)
                .with_context(|| format!("Failed to remove `{}`", &temp_state_path.display()))?
        }

        // We write to .temp version first, then move it to actual location to prevent mid-write crash causing corrupted file
        fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
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
