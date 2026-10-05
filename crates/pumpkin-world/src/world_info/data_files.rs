use std::{
    fs::{self, File, OpenOptions},
    io::BufWriter,
    path::{Path, PathBuf},
};

use pumpkin_data::game_rules::{GameRule, GameRuleRegistry, GameRuleValue};
use pumpkin_nbt::{compound::NbtCompound, nbt_compress::read_gzip_compound_tag, tag::NbtTag};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::world_info::{CURRENT_WORLD_DATA_VERSION, WorldGenSettings, WorldInfoError};

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct DataFileRoot<T> {
    #[serde(rename = "data")]
    pub data: T,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct WeatherData {
    #[serde(rename = "rain_time", default)]
    pub rain_time: i32,
    #[serde(rename = "raining", default)]
    pub raining: bool,
    #[serde(rename = "thundering", default)]
    pub thundering: bool,
    #[serde(rename = "thunder_time", default)]
    pub thunder_time: i32,
    #[serde(rename = "clear_weather_time", default)]
    pub clear_weather_time: i32,
    #[serde(rename = "DataVersion", default)]
    pub data_version: i32,
}

impl Default for WeatherData {
    fn default() -> Self {
        Self {
            rain_time: 0,
            raining: false,
            thundering: false,
            thunder_time: 0,
            clear_weather_time: -1,
            data_version: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct WorldGenSettingsData {
    #[serde(flatten)]
    pub settings: WorldGenSettings,
    #[serde(rename = "DataVersion", default)]
    pub data_version: i32,
    #[serde(rename = "bonus_chest", default)]
    pub bonus_chest: bool,
    #[serde(rename = "generate_structures", default = "default_true")]
    pub generate_structures: bool,
}

const fn default_true() -> bool {
    true
}

impl WorldGenSettingsData {
    #[must_use]
    pub const fn new(settings: WorldGenSettings, data_version: i32) -> Self {
        Self {
            settings,
            data_version,
            bonus_chest: false,
            generate_structures: true,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct DimensionClock {
    pub total_ticks: i64,
    pub partial_tick: f32,
    pub rate: f32,
    pub paused: bool,
}

impl Default for DimensionClock {
    fn default() -> Self {
        Self {
            total_ticks: 0,
            partial_tick: 0.0,
            rate: 1.0,
            paused: false,
        }
    }
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct WorldClocksData {
    pub clocks: std::collections::HashMap<String, DimensionClock>,
    pub data_version: i32,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct WanderingTraderData {
    #[serde(rename = "spawn_delay", default = "default_wandering_trader_delay")]
    pub spawn_delay: i32,
    #[serde(rename = "spawn_chance", default = "default_wandering_trader_chance")]
    pub spawn_chance: i32,
    #[serde(rename = "DataVersion", default)]
    pub data_version: i32,
}

const fn default_wandering_trader_delay() -> i32 {
    24_000
}
const fn default_wandering_trader_chance() -> i32 {
    25
}

impl Default for WanderingTraderData {
    fn default() -> Self {
        Self {
            spawn_delay: default_wandering_trader_delay(),
            spawn_chance: default_wandering_trader_chance(),
            data_version: 0,
        }
    }
}

#[must_use]
pub fn minecraft_data_dir(level_folder: &Path) -> PathBuf {
    level_folder.join("data").join("minecraft")
}

/// Finds overworld data, keeping an existing root copy authoritative over Paper's layout.
pub(super) fn find_overworld_data_file(level_folder: &Path, name: &str) -> Option<PathBuf> {
    let root = minecraft_data_dir(level_folder).join(name);
    // An inspection error is not absence; let the reader report it rather than use stale data.
    match root.try_exists() {
        Ok(true) | Err(_) => return Some(root),
        Ok(false) => {}
    }

    let overworld = level_folder
        .join("dimensions")
        .join("minecraft")
        .join("overworld");
    let path = minecraft_data_dir(&overworld).join(name);
    match path.try_exists() {
        Ok(true) | Err(_) => Some(path),
        Ok(false) => None,
    }
}

/// Ensures the `<world>/data/minecraft/` directory exists.
pub fn ensure_minecraft_data_dir(level_folder: &Path) -> Result<PathBuf, WorldInfoError> {
    let dir = minecraft_data_dir(level_folder);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub(super) fn overlay_compound(stored: &mut NbtCompound, fresh: NbtCompound) {
    for (key, tag) in fresh.child_tags {
        match (stored.child_tags.get_mut(&key), tag) {
            (Some(NbtTag::Compound(stored)), NbtTag::Compound(fresh)) => {
                overlay_compound(stored, fresh);
            }
            (_, tag) => {
                stored.child_tags.insert(key, tag);
            }
        }
    }
}

fn read_saved_data(path: &Path) -> Result<NbtCompound, WorldInfoError> {
    let mut root = read_gzip_compound_tag(File::open(path)?).map_err(|error| {
        WorldInfoError::DeserializationError(format!("{}: {error}", path.display()))
    })?;
    if path
        .file_name()
        .is_some_and(|name| name == "world_gen_settings.dat")
    {
        let payload = world_gen_settings_payload(&root).cloned().ok_or_else(|| {
            WorldInfoError::DeserializationError(format!("{}: missing seed", path.display()))
        })?;
        let mut data = root.get_compound("data").cloned().unwrap_or_default();
        overlay_compound(&mut data, payload);
        root.put_compound("data", data);
    } else if root.get_compound("data").is_none() {
        if path.file_name().is_some_and(|name| name == "weather.dat")
            && root.get_int("rain_time").is_some()
        {
            root.put_compound("data", root.clone());
        } else {
            return Err(WorldInfoError::DeserializationError(format!(
                "{}: missing data compound",
                path.display()
            )));
        }
    }
    validate_saved_data(&root, path)?;
    Ok(root)
}

fn validate_saved_data(root: &NbtCompound, path: &Path) -> Result<(), WorldInfoError> {
    let data = root
        .get_compound("data")
        .ok_or_else(|| WorldInfoError::DeserializationError("Missing data".into()))?;
    let invalid =
        || WorldInfoError::DeserializationError(format!("{}: invalid saved data", path.display()));
    match path.file_name().and_then(|name| name.to_str()) {
        Some("weather.dat") => {
            for name in ["clear_weather_time", "rain_time", "thunder_time"] {
                data.get_int(name).ok_or_else(invalid)?;
            }
            for name in ["raining", "thundering"] {
                data.get_bool(name).ok_or_else(invalid)?;
            }
        }
        Some("game_rules.dat") => {
            let defaults = GameRuleRegistry::default();
            for rule in GameRule::all() {
                let name = format!("minecraft:{rule}");
                if data.child_tags.contains_key(name.as_str()) {
                    match defaults.get(rule) {
                        GameRuleValue::Bool(_) => {
                            data.get_bool(&name).ok_or_else(invalid)?;
                        }
                        GameRuleValue::Int(_) => {
                            data.get_int(&name).ok_or_else(invalid)?;
                        }
                    }
                }
            }
        }
        Some("world_gen_settings.dat") => {
            data.get_long("seed").ok_or_else(invalid)?;
        }
        Some("world_clocks.dat") => {
            for (name, tag) in &data.child_tags {
                if name.as_ref() == "DataVersion" {
                    continue;
                }
                let NbtTag::Compound(clock) = tag else {
                    return Err(invalid());
                };
                clock.get_long("total_ticks").ok_or_else(invalid)?;
                for name in ["partial_tick", "rate"] {
                    if clock.child_tags.contains_key(name) {
                        let value = clock.get_float(name).ok_or_else(invalid)?;
                        if !value.is_finite() || (name == "rate" && value <= 0.0) {
                            return Err(invalid());
                        }
                    }
                }
                if clock.child_tags.contains_key("paused") {
                    clock.get_bool("paused").ok_or_else(invalid)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn write_saved_data(
    level_folder: &Path,
    name: &str,
    fresh: NbtCompound,
    overworld_fallback: bool,
) -> Result<(), WorldInfoError> {
    let path = minecraft_data_dir(level_folder).join(name);
    let source = if overworld_fallback {
        find_overworld_data_file(level_folder, name)
    } else if path.try_exists()? {
        Some(path.clone())
    } else {
        None
    };
    let mut root = source
        .as_deref()
        .map(read_saved_data)
        .transpose()?
        .unwrap_or_default();
    overlay_compound(&mut root, fresh);
    root.put_int("DataVersion", CURRENT_WORLD_DATA_VERSION);
    if let Some(NbtTag::Compound(data)) = root.child_tags.get_mut("data") {
        data.child_tags.remove("DataVersion");
    }
    let bytes = pumpkin_nbt::nbt_compress::write_gzip_compound_tag_to_bytes(root)
        .map_err(|error| WorldInfoError::SerializationError(error.to_string()))?;
    fs::create_dir_all(minecraft_data_dir(level_folder))?;
    super::atomic_write(&path, &bytes)?;
    Ok(())
}

pub fn synchronize_world_info(level_folder: &Path) -> Result<(), WorldInfoError> {
    for name in [
        "level.dat",
        "level.dat_old",
        "data/minecraft/game_rules.dat",
        "data/minecraft/world_gen_settings.dat",
        "data/minecraft/world_clocks.dat",
        "data/minecraft/weather.dat",
    ] {
        let path = level_folder.join(name);
        match OpenOptions::new().write(true).open(path) {
            Ok(file) => file.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    #[cfg(unix)]
    {
        File::open(minecraft_data_dir(level_folder))?.sync_all()?;
        File::open(level_folder.join("data"))?.sync_all()?;
        File::open(level_folder)?.sync_all()?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorldBorderData {
    pub center_x: f64,
    pub center_z: f64,
    pub damage_per_block: f64,
    pub safe_zone: f64,
    pub warning_blocks: i32,
    pub warning_time: i32,
    pub size: f64,
    pub lerp_time: i64,
    pub lerp_target: f64,
}

pub fn read_world_border(
    dimension_folder: &Path,
) -> Result<Option<WorldBorderData>, WorldInfoError> {
    let path = minecraft_data_dir(dimension_folder).join("world_border.dat");
    if !path.try_exists()? {
        return Ok(None);
    }
    let root = read_saved_data(&path)?;
    let data = root
        .get_compound("data")
        .ok_or_else(|| WorldInfoError::DeserializationError("Missing border data".into()))?;
    let missing = || {
        WorldInfoError::DeserializationError(format!("{}: invalid border settings", path.display()))
    };
    Ok(Some(WorldBorderData {
        center_x: data.get_double("center_x").ok_or_else(missing)?,
        center_z: data.get_double("center_z").ok_or_else(missing)?,
        damage_per_block: data.get_double("damage_per_block").ok_or_else(missing)?,
        safe_zone: data.get_double("safe_zone").ok_or_else(missing)?,
        warning_blocks: data.get_int("warning_blocks").ok_or_else(missing)?,
        warning_time: data.get_int("warning_time").ok_or_else(missing)?,
        size: data.get_double("size").ok_or_else(missing)?,
        lerp_time: data.get_long("lerp_time").ok_or_else(missing)?,
        lerp_target: data.get_double("lerp_target").ok_or_else(missing)?,
    }))
}

pub fn write_world_border(
    dimension_folder: &Path,
    border: &WorldBorderData,
    synchronize: bool,
) -> Result<(), WorldInfoError> {
    // Validate the existing codec before replacing any of its owned fields.
    read_world_border(dimension_folder)?;
    let mut data = NbtCompound::new();
    data.put_double("center_x", border.center_x);
    data.put_double("center_z", border.center_z);
    data.put_double("damage_per_block", border.damage_per_block);
    data.put_double("safe_zone", border.safe_zone);
    data.put_int("warning_blocks", border.warning_blocks);
    data.put_int("warning_time", border.warning_time);
    data.put_double("size", border.size);
    data.put_long("lerp_time", border.lerp_time);
    data.put_double("lerp_target", border.lerp_target);
    let mut root = NbtCompound::new();
    root.put_compound("data", data);
    write_saved_data(dimension_folder, "world_border.dat", root, false)?;
    if synchronize {
        OpenOptions::new()
            .write(true)
            .open(minecraft_data_dir(dimension_folder).join("world_border.dat"))?
            .sync_all()?;
        #[cfg(unix)]
        {
            File::open(minecraft_data_dir(dimension_folder))?.sync_all()?;
            File::open(dimension_folder.join("data"))?.sync_all()?;
            File::open(dimension_folder)?.sync_all()?;
        }
    }
    Ok(())
}

/// Reads weather from the root data directory, falling back to Paper's overworld directory.
///
/// Returns defaults if neither file exists or the selected file cannot be opened or decoded.
/// An unreadable root file remains authoritative; it does not trigger the Paper fallback.
pub fn read_weather(level_folder: &Path) -> WeatherData {
    let Some(path) = find_overworld_data_file(level_folder, "weather.dat") else {
        return WeatherData::default();
    };
    match read_saved_data(&path) {
        Ok(compound) => {
            let data_compound = compound.get_compound("data");
            let c = data_compound.as_ref().map_or(&compound, |v| v);
            WeatherData {
                clear_weather_time: c.get_int("clear_weather_time").unwrap_or(0),
                rain_time: c.get_int("rain_time").unwrap_or(0),
                thunder_time: c.get_int("thunder_time").unwrap_or(0),
                raining: c.get_bool("raining").unwrap_or(false),
                thundering: c.get_bool("thundering").unwrap_or(false),
                data_version: compound
                    .get_int("DataVersion")
                    .or_else(|| c.get_int("DataVersion"))
                    .unwrap_or(0),
            }
        }
        Err(error) => {
            warn!("Failed loading weather.dat: {error}");
            WeatherData::default()
        }
    }
}

pub fn write_weather(level_folder: &Path, data: &WeatherData) -> Result<(), WorldInfoError> {
    let mut data_comp = NbtCompound::new();
    data_comp.put_int("clear_weather_time", data.clear_weather_time);
    data_comp.put_int("rain_time", data.rain_time);
    data_comp.put_int("thunder_time", data.thunder_time);
    data_comp.put_bool("raining", data.raining);
    data_comp.put_bool("thundering", data.thundering);
    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data.data_version);
    root.put_compound("data", data_comp);
    write_saved_data(level_folder, "weather.dat", root, true)
}

#[must_use]
pub fn json_to_nbt_tag(val: &serde_json::Value) -> NbtTag {
    match val {
        serde_json::Value::Null => NbtTag::End,
        serde_json::Value::Bool(b) => NbtTag::Byte(i8::from(*b)),
        serde_json::Value::Number(n) => n.as_i64().map_or_else(
            || n.as_f64().map_or(NbtTag::End, NbtTag::Double),
            |i| i32::try_from(i).map_or(NbtTag::Long(i), NbtTag::Int),
        ),
        serde_json::Value::String(s) => NbtTag::String(s.clone().into()),
        serde_json::Value::Array(arr) => NbtTag::List(arr.iter().map(json_to_nbt_tag).collect()),
        serde_json::Value::Object(map) => {
            let mut compound = NbtCompound::new();
            for (k, v) in map {
                compound.put(k, json_to_nbt_tag(v));
            }
            NbtTag::Compound(compound)
        }
    }
}

#[must_use]
pub fn nbt_tag_to_json(tag: &NbtTag) -> serde_json::Value {
    match tag {
        NbtTag::Byte(b) => serde_json::Value::Number((*b).into()),
        NbtTag::Short(s) => serde_json::Value::Number((*s).into()),
        NbtTag::Int(i) => serde_json::Value::Number((*i).into()),
        NbtTag::Long(l) => serde_json::Value::Number((*l).into()),
        NbtTag::Float(f) => serde_json::Number::from_f64(*f as f64)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        NbtTag::Double(d) => serde_json::Number::from_f64(*d)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        NbtTag::String(s) => serde_json::Value::String(s.to_string()),
        NbtTag::List(list) => serde_json::Value::Array(list.iter().map(nbt_tag_to_json).collect()),
        NbtTag::Compound(comp) => {
            let mut map = serde_json::Map::new();
            for (k, v) in &comp.child_tags {
                map.insert(k.to_string(), nbt_tag_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        NbtTag::ByteArray(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|b| serde_json::Value::Number((*b).into()))
                .collect(),
        ),
        NbtTag::IntArray(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|i| serde_json::Value::Number((*i).into()))
                .collect(),
        ),
        NbtTag::LongArray(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|l| serde_json::Value::Number((*l).into()))
                .collect(),
        ),
        NbtTag::End => serde_json::Value::Null,
    }
}

#[must_use]
pub fn read_world_gen_settings(level_folder: &Path) -> Option<WorldGenSettings> {
    // Support world generation settings locations used by vanilla and Paper-derived 26.x worlds.
    find_overworld_data_file(level_folder, "world_gen_settings.dat")
        .as_deref()
        .and_then(read_world_gen_settings_file)
}

fn read_world_gen_settings_file(path: &Path) -> Option<WorldGenSettings> {
    match File::open(path) {
        Ok(f) => match read_gzip_compound_tag(f) {
            Ok(compound) => {
                let Some(c) = world_gen_settings_payload(&compound) else {
                    warn!("{} has no seed", path.display());
                    return None;
                };
                let seed = c.get_long("seed")?;
                let mut dimensions = std::collections::HashMap::new();
                if let Some(dims_comp) = c.get_compound("dimensions") {
                    for (dim_name, dim_tag) in &dims_comp.child_tags {
                        if let NbtTag::Compound(dim_c) = dim_tag {
                            let dim_type = dim_c.get_string("type").unwrap_or(dim_name).to_string();
                            if let Some(gen_c) = dim_c.get_compound("generator") {
                                let generator_type = gen_c
                                    .get_string("type")
                                    .unwrap_or("minecraft:noise")
                                    .to_string();
                                let settings = gen_c
                                    .get_string("settings")
                                    .map(|s| {
                                        crate::world_info::GeneratorSettings::Reference(
                                            s.to_string(),
                                        )
                                    })
                                    .or_else(|| {
                                        gen_c.get_compound("settings").map(|settings_c| {
                                            let json_val = nbt_tag_to_json(&NbtTag::Compound(
                                                settings_c.clone(),
                                            ));
                                            crate::world_info::GeneratorSettings::Compound(json_val)
                                        })
                                    });
                                let biome_source = gen_c.get_compound("biome_source").map(|bs_c| {
                                    let biome_type = bs_c
                                        .get_string("type")
                                        .unwrap_or("minecraft:multi_noise")
                                        .to_string();
                                    if let Some(preset) = bs_c.get_string("preset") {
                                        crate::world_info::BiomeSource::WithPreset {
                                            preset: preset.to_string(),
                                            biome_type,
                                        }
                                    } else if let Some(biome) = bs_c.get_string("biome") {
                                        crate::world_info::BiomeSource::Fixed {
                                            biome: biome.to_string(),
                                            biome_type,
                                        }
                                    } else {
                                        crate::world_info::BiomeSource::Simple { biome_type }
                                    }
                                });
                                dimensions.insert(
                                    dim_name.to_string(),
                                    crate::world_info::Dimension {
                                        generator: crate::world_info::Generator {
                                            settings,
                                            biome_source,
                                            generator_type,
                                        },
                                        dimension_type: dim_type,
                                    },
                                );
                            }
                        }
                    }
                }
                Some(WorldGenSettings { seed, dimensions })
            }
            Err(e) => {
                warn!("Failed to deserialize {}: {e}", path.display());
                None
            }
        },
        Err(e) => {
            warn!("Failed to open {}: {e}", path.display());
            None
        }
    }
}

fn world_gen_settings_payload(mut compound: &NbtCompound) -> Option<&NbtCompound> {
    loop {
        if compound.get_long("seed").is_some() {
            return Some(compound);
        }

        compound = compound
            .get_compound("data")
            .or_else(|| compound.get_compound("Data"))?;
    }
}

pub fn write_world_gen_settings(
    level_folder: &Path,
    settings: &WorldGenSettings,
    data_version: i32,
) -> Result<(), WorldInfoError> {
    let mut inner = NbtCompound::new();
    inner.put_long("seed", settings.seed);
    if find_overworld_data_file(level_folder, "world_gen_settings.dat").is_none() {
        inner.put_bool("generate_structures", true);
        inner.put_bool("bonus_chest", false);
    }

    let mut dims_comp = NbtCompound::new();
    for (dim_name, dim) in &settings.dimensions {
        let mut dim_comp = NbtCompound::new();
        dim_comp.put_string("type", dim.dimension_type.clone());

        let mut gen_comp = NbtCompound::new();
        gen_comp.put_string("type", dim.generator.generator_type.clone());
        if let Some(s) = &dim.generator.settings {
            match s {
                crate::world_info::GeneratorSettings::Reference(r) => {
                    gen_comp.put_string("settings", r.clone());
                }
                crate::world_info::GeneratorSettings::Compound(json_val) => {
                    gen_comp.put("settings", json_to_nbt_tag(json_val));
                }
            }
        }
        if let Some(bs) = &dim.generator.biome_source {
            let mut bs_comp = NbtCompound::new();
            match bs {
                crate::world_info::BiomeSource::WithPreset { preset, biome_type } => {
                    bs_comp.put_string("preset", preset.clone());
                    bs_comp.put_string("type", biome_type.clone());
                }
                crate::world_info::BiomeSource::Fixed { biome, biome_type } => {
                    bs_comp.put_string("biome", biome.clone());
                    bs_comp.put_string("type", biome_type.clone());
                }
                crate::world_info::BiomeSource::Simple { biome_type } => {
                    bs_comp.put_string("type", biome_type.clone());
                }
            }
            gen_comp.put_compound("biome_source", bs_comp);
        }
        dim_comp.put_compound("generator", gen_comp);
        dims_comp.put_compound(dim_name, dim_comp);
    }
    inner.put_compound("dimensions", dims_comp);

    let mut root = NbtCompound::new();
    root.put_compound("data", inner);
    root.put_int("DataVersion", data_version);
    write_saved_data(level_folder, "world_gen_settings.dat", root, true)
}

#[must_use]
pub fn game_rules_to_nbt(rules: &GameRuleRegistry, data_version: i32) -> NbtCompound {
    let mut inner = NbtCompound::new();
    for rule in GameRule::all() {
        let key = format!("minecraft:{rule}");
        match rules.get(rule) {
            GameRuleValue::Bool(b) => inner.put(&key, NbtTag::Byte(i8::from(*b))),
            GameRuleValue::Int(i) => inner.put(&key, NbtTag::Int(*i as i32)),
        }
    }
    let mut root = NbtCompound::new();
    root.put_compound("data", inner);
    root.put_int("DataVersion", data_version);
    root
}

pub fn game_rules_from_nbt(root: &NbtCompound) -> GameRuleRegistry {
    let mut registry = GameRuleRegistry::default();

    let Some(inner) = root.get_compound("data") else {
        warn!("game_rules.dat missing 'data' compound, using defaults");
        return registry;
    };

    for rule in GameRule::all() {
        let key = format!("minecraft:{rule}");
        match registry.get_mut(rule) {
            GameRuleValue::Bool(b) => {
                if let Some(v) = inner.get_byte(&key) {
                    *b = v != 0;
                }
            }
            GameRuleValue::Int(i) => {
                if let Some(v) = inner.get_int(&key) {
                    *i = i64::from(v);
                }
            }
        }
    }

    registry
}

/// Reads gamerules from the root data directory, falling back to Paper's overworld directory.
///
/// Missing or invalid rules retain their defaults. File access or decoding failures also
/// return defaults rather than falling back from an unreadable root file to a Paper copy.
pub fn read_game_rules(level_folder: &Path) -> GameRuleRegistry {
    let Some(path) = find_overworld_data_file(level_folder, "game_rules.dat") else {
        return GameRuleRegistry::default();
    };

    match File::open(&path) {
        Ok(f) => match read_gzip_compound_tag(f) {
            Ok(compound) => game_rules_from_nbt(&compound),
            Err(e) => {
                warn!("Failed to parse game_rules.dat: {e}");
                GameRuleRegistry::default()
            }
        },
        Err(e) => {
            warn!("Failed to open game_rules.dat: {e}");
            GameRuleRegistry::default()
        }
    }
}

pub fn write_game_rules(
    level_folder: &Path,
    rules: &GameRuleRegistry,
    data_version: i32,
) -> Result<(), WorldInfoError> {
    let compound = game_rules_to_nbt(rules, data_version);
    write_saved_data(level_folder, "game_rules.dat", compound, true)
}

pub fn read_world_clocks(level_folder: &Path) -> WorldClocksData {
    let Some(path) = find_overworld_data_file(level_folder, "world_clocks.dat") else {
        return WorldClocksData::default();
    };

    match read_saved_data(&path) {
        Ok(compound) => world_clocks_from_nbt(&compound),
        Err(error) => {
            warn!("Failed loading world_clocks.dat: {error}");
            WorldClocksData::default()
        }
    }
}

fn world_clocks_from_nbt(root: &NbtCompound) -> WorldClocksData {
    let mut result = WorldClocksData::default();

    let Some(inner) = root.get_compound("data") else {
        return result;
    };

    result.data_version = root
        .get_int("DataVersion")
        .or_else(|| inner.get_int("DataVersion"))
        .unwrap_or(0);

    for (key, tag) in &inner.child_tags {
        if key.as_ref() == "DataVersion" {
            continue;
        }
        if let NbtTag::Compound(dim_compound) = tag {
            let total_ticks = dim_compound.get_long("total_ticks").unwrap_or(0);
            result.clocks.insert(
                key.to_string(),
                DimensionClock {
                    total_ticks,
                    partial_tick: dim_compound.get_float("partial_tick").unwrap_or(0.0),
                    rate: dim_compound.get_float("rate").unwrap_or(1.0),
                    paused: dim_compound.get_bool("paused").unwrap_or(false),
                },
            );
        }
    }

    result
}

pub fn write_world_clocks(
    level_folder: &Path,
    clocks: &WorldClocksData,
) -> Result<(), WorldInfoError> {
    let mut inner = NbtCompound::new();
    for (dim_name, clock) in &clocks.clocks {
        let mut dim_compound = NbtCompound::new();
        dim_compound.put_long("total_ticks", clock.total_ticks);
        dim_compound.put_float("partial_tick", clock.partial_tick);
        dim_compound.put_float("rate", clock.rate);
        dim_compound.put_bool("paused", clock.paused);
        inner.put_compound(dim_name, dim_compound);
    }
    let mut root = NbtCompound::new();
    root.put_compound("data", inner);
    root.put_int("DataVersion", clocks.data_version);
    write_saved_data(level_folder, "world_clocks.dat", root, true)
}

/// Reads trader spawn settings, preferring root data over Paper's overworld copy.
///
/// Accepts current and legacy field names in wrapped or unwrapped payloads. Missing data
/// and file access or decoding failures use defaults without bypassing an existing root file.
pub fn read_wandering_trader(level_folder: &Path) -> WanderingTraderData {
    let Some(path) = find_overworld_data_file(level_folder, "wandering_trader.dat") else {
        return WanderingTraderData::default();
    };
    match File::open(&path) {
        Ok(f) => match read_gzip_compound_tag(f) {
            Ok(compound) => {
                let data_compound = compound.get_compound("data");
                let c = data_compound.as_ref().map_or(&compound, |v| v);
                let data_version = compound
                    .get_int("DataVersion")
                    .or_else(|| c.get_int("DataVersion"))
                    .unwrap_or(0);
                WanderingTraderData {
                    spawn_delay: c
                        .get_int("spawn_delay")
                        .or_else(|| c.get_int("WanderingTraderSpawnDelay"))
                        .unwrap_or(24_000),
                    spawn_chance: c
                        .get_int("spawn_chance")
                        .or_else(|| c.get_int("WanderingTraderSpawnChance"))
                        .unwrap_or(25),
                    data_version,
                }
            }
            Err(e) => {
                warn!("Failed to deserialize wandering_trader.dat, using defaults: {e}");
                WanderingTraderData::default()
            }
        },
        Err(e) => {
            warn!("Failed to open wandering_trader.dat, using defaults: {e}");
            WanderingTraderData::default()
        }
    }
}

pub fn write_wandering_trader(
    level_folder: &Path,
    data: &WanderingTraderData,
) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("wandering_trader.dat");
    let file = File::create(&path)?;
    let mut data_comp = NbtCompound::new();
    data_comp.put_int("spawn_delay", data.spawn_delay);
    data_comp.put_int("spawn_chance", data.spawn_chance);
    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data.data_version);
    root.put_compound("data", data_comp);
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, BufWriter::new(file))
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

pub fn write_custom_boss_events_stub(
    level_folder: &Path,
    data_version: i32,
) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("custom_boss_events.dat");
    if path.exists() {
        return Ok(());
    }

    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data_version);
    root.put_compound("data", NbtCompound::new());

    let file = File::create(&path)?;
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, file)
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

/// Creates an empty root scheduled-events file only when neither supported layout has one.
///
/// Preserves imported events without loading or executing them. A path inspection error
/// also prevents stub creation so potentially existing events are not shadowed.
///
/// # Errors
/// Returns an error if directory creation, file creation, or NBT serialization fails.
pub fn write_scheduled_events_stub(
    level_folder: &Path,
    data_version: i32,
) -> Result<(), WorldInfoError> {
    // Until events can be loaded, do not shadow imported events with an empty root file.
    if find_overworld_data_file(level_folder, "scheduled_events.dat").is_some() {
        return Ok(());
    }
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("scheduled_events.dat");

    let mut inner = NbtCompound::new();
    inner.put("events", NbtTag::List(vec![]));
    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data_version);
    root.put_compound("data", inner);

    let file = File::create(&path)?;
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, file)
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

pub fn write_random_sequences_stub(
    level_folder: &Path,
    data_version: i32,
) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("random_sequences.dat");
    if path.exists() {
        return Ok(());
    }

    let mut inner = NbtCompound::new();
    inner.put_int("salt", 0);
    inner.put_compound("sequences", NbtCompound::new());
    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data_version);
    root.put_compound("data", inner);

    let file = File::create(&path)?;
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, file)
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

pub fn write_scoreboard_stub(level_folder: &Path, data_version: i32) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("scoreboard.dat");
    if path.exists() {
        return Ok(());
    }

    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data_version);
    root.put_compound("data", NbtCompound::new());

    let file = File::create(&path)?;
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, file)
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

pub fn write_stopwatches_stub(
    level_folder: &Path,
    data_version: i32,
) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("stopwatches.dat");
    if path.exists() {
        return Ok(());
    }

    let mut inner = NbtCompound::new();
    inner.put_compound("stopwatches", NbtCompound::new());
    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data_version);
    root.put_compound("data", inner);

    let file = File::create(&path)?;
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, file)
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_nbt::nbt_compress::write_gzip_compound_tag;

    fn fixture(folder: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = minecraft_data_dir(folder).join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn vanilla_clocks_weather_and_border_preserve_unknown_fields() {
        let folder = tempfile::tempdir().unwrap();
        let clocks_path = fixture(
            folder.path(),
            "world_clocks.dat",
            include_bytes!("../../../../assets/tests/storage_26_3/world_clocks.dat"),
        );
        let weather_path = fixture(
            folder.path(),
            "weather.dat",
            include_bytes!("../../../../assets/tests/storage_26_3/weather.dat"),
        );
        let border_path = fixture(
            folder.path(),
            "world_border.dat",
            include_bytes!("../../../../assets/tests/storage_26_3/world_border.dat"),
        );
        let mut clocks = read_world_clocks(folder.path());
        assert_eq!(clocks.clocks["minecraft:overworld"].total_ticks, 2447);
        assert!(clocks.clocks["minecraft:overworld"].paused);
        assert_eq!(clocks.clocks["minecraft:the_end"].total_ticks, 275516);
        let mut weather = read_weather(folder.path());
        assert_eq!(weather.clear_weather_time, 759895);
        assert_eq!(weather.rain_time, 1);
        let mut border = read_world_border(folder.path()).unwrap().unwrap();
        assert_eq!(border.warning_time, 300);
        assert_eq!(border.size, 59_999_968.0);
        for path in [&clocks_path, &weather_path, &border_path] {
            let mut root = read_saved_data(path).unwrap();
            root.put_string("unknown_root", "kept".to_string());
            let mut data = root.get_compound("data").unwrap().clone();
            if path == &clocks_path {
                let mut clock = data.get_compound("minecraft:overworld").unwrap().clone();
                clock.put_int("unknown_clock", 42);
                data.put_compound("minecraft:overworld", clock);
            } else {
                data.put_int("unknown_field", 42);
            }
            root.put_compound("data", data);
            write_gzip_compound_tag(root, File::create(path).unwrap()).unwrap();
        }
        let clock = clocks.clocks.get_mut("minecraft:overworld").unwrap();
        clock.total_ticks = 90001;
        clock.partial_tick = 0.25;
        clock.rate = 0.5;
        clock.paused = false;
        weather.raining = true;
        weather.rain_time = 1234;
        border.center_x = -42.25;
        border.size = 100.0;
        border.lerp_time = 80;
        border.lerp_target = 500.0;
        write_world_clocks(folder.path(), &clocks).unwrap();
        write_weather(folder.path(), &weather).unwrap();
        write_world_border(folder.path(), &border, true).unwrap();
        synchronize_world_info(folder.path()).unwrap();
        assert_eq!(read_world_clocks(folder.path()), clocks);
        assert_eq!(read_weather(folder.path()), weather);
        assert_eq!(read_world_border(folder.path()).unwrap().unwrap(), border);
        for path in [&clocks_path, &weather_path, &border_path] {
            let root = read_saved_data(path).unwrap();
            assert_eq!(root.get_int("DataVersion"), Some(5023));
            assert_eq!(root.get_string("unknown_root"), Some("kept"));
            let data = root.get_compound("data").unwrap();
            assert!(!data.child_tags.contains_key("DataVersion"));
            if path == &clocks_path {
                assert_eq!(
                    data.get_compound("minecraft:overworld")
                        .unwrap()
                        .get_int("unknown_clock"),
                    Some(42)
                );
            } else {
                assert_eq!(data.get_int("unknown_field"), Some(42));
            }
        }
    }

    #[test]
    fn unreadable_saved_data_is_preserved_and_can_be_retried() {
        let folder = tempfile::tempdir().unwrap();
        let path = fixture(folder.path(), "weather.dat", b"corrupt imported weather");
        let original = fs::read(&path).unwrap();
        assert!(write_weather(folder.path(), &WeatherData::default()).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        let mut root = NbtCompound::new();
        let mut data = NbtCompound::new();
        data.put_string("rain_time", "invalid".to_string());
        root.put_compound("data", data);
        write_gzip_compound_tag(root, File::create(&path).unwrap()).unwrap();
        let original = fs::read(&path).unwrap();
        assert!(write_weather(folder.path(), &WeatherData::default()).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::write(
            &path,
            include_bytes!("../../../../assets/tests/storage_26_3/weather.dat"),
        )
        .unwrap();
        let weather = WeatherData {
            rain_time: 5432,
            data_version: 5023,
            ..Default::default()
        };
        write_weather(folder.path(), &weather).unwrap();
        assert_eq!(read_weather(folder.path()), weather);
    }
}
