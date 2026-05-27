pub mod fabric;
pub mod forge;
pub mod forge_patcher;
pub mod neoforge;
pub mod quilt;
pub mod types;

use async_trait::async_trait;
use tokio::sync::mpsc::Sender;

use crate::error::LoaderError;
use crate::launcher::events::LaunchEvent;
use crate::launcher::options::LaunchOptions;
use crate::models::loader::LoaderType;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Persist a loader's version JSON to `<loader_dir>/versions/<id>/<id>.json`.
/// Used by Fabric/Quilt where we fetch the profile JSON ourselves; Forge and
/// NeoForge let the installer write its own version JSON.
async fn save_loader_version(
    loader_dir: &std::path::Path,
    id: &str,
    json: &impl serde::Serialize,
) -> Result<(), LoaderError> {
    let dir = loader_dir.join("versions").join(id);
    tokio::fs::create_dir_all(&dir).await?;
    let content = serde_json::to_string(json)?;
    tokio::fs::write(dir.join(format!("{id}.json")), content).await?;
    Ok(())
}

use self::fabric::{FabricMC, FabricVariant};
use self::forge::ForgeMC;
use self::neoforge::NeoForgeMC;
use self::quilt::QuiltMC;
use self::types::{LoaderInstallInput, LoaderResult};

// ── Trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ModLoader: Send + Sync {
    async fn install(
        &self,
        options: &LaunchOptions,
        input: &LoaderInstallInput,
        client: &reqwest::Client,
        event_tx: &Sender<LaunchEvent>,
    ) -> Result<LoaderResult, LoaderError>;
}

// ── Dispatcher ────────────────────────────────────────────────────────────────

pub fn create_loader(loader_type: LoaderType) -> Box<dyn ModLoader> {
    match loader_type {
        LoaderType::Forge => Box::new(ForgeMC::new()),
        LoaderType::NeoForge => Box::new(NeoForgeMC::new()),
        LoaderType::Fabric => Box::new(FabricMC::new(FabricVariant::Modern)),
        LoaderType::LegacyFabric => Box::new(FabricMC::new(FabricVariant::Legacy)),
        LoaderType::Quilt => Box::new(QuiltMC::new()),
    }
}

// ── ModLoader impls ───────────────────────────────────────────────────────────

#[async_trait]
impl ModLoader for FabricMC {
    async fn install(
        &self,
        options: &LaunchOptions,
        input: &LoaderInstallInput,
        client: &reqwest::Client,
        event_tx: &Sender<LaunchEvent>,
    ) -> Result<LoaderResult, LoaderError> {
        let loader_name = match self.loader_type() {
            LoaderType::LegacyFabric => "legacyfabric",
            _ => "fabric",
        };
        let json = self
            .download_json(&input.mc_version, &options.loader.build, client)
            .await?;
        let libraries = self
            .download_libraries(options, &json, client, event_tx)
            .await?;
        save_loader_version(&options.loader_dir(loader_name), &json.id, &json).await?;
        let extra_game_args = json.minecraft_arguments
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        Ok(LoaderResult {
            libraries,
            main_class: json.main_class,
            loader_version: json.id,
            loader_type: self.loader_type(),
            extra_game_args,
            extra_jvm_args: vec![],
        })
    }
}

#[async_trait]
impl ModLoader for QuiltMC {
    async fn install(
        &self,
        options: &LaunchOptions,
        input: &LoaderInstallInput,
        client: &reqwest::Client,
        event_tx: &Sender<LaunchEvent>,
    ) -> Result<LoaderResult, LoaderError> {
        let json = self
            .download_json(&input.mc_version, &options.loader.build, client)
            .await?;
        let libraries = self
            .download_libraries(options, &json, client, event_tx)
            .await?;
        save_loader_version(&options.loader_dir("quilt"), &json.id, &json).await?;
        let extra_game_args = json.minecraft_arguments
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        Ok(LoaderResult {
            libraries,
            main_class: json.main_class,
            loader_version: json.id,
            loader_type: LoaderType::Quilt,
            extra_game_args,
            extra_jvm_args: vec![],
        })
    }
}

#[async_trait]
impl ModLoader for ForgeMC {
    async fn install(
        &self,
        options: &LaunchOptions,
        input: &LoaderInstallInput,
        client: &reqwest::Client,
        event_tx: &Sender<LaunchEvent>,
    ) -> Result<LoaderResult, LoaderError> {
        let (version_id, main_class, libraries, extra_game_args, extra_jvm_args) = self
            .install(
                options,
                &input.mc_version,
                &input.java_path,
                &input.mc_jar,
                &input.mc_json,
                &options.loader.build,
                client,
                event_tx,
            )
            .await?;

        Ok(LoaderResult {
            libraries,
            main_class,
            loader_version: version_id,
            loader_type: LoaderType::Forge,
            extra_game_args,
            extra_jvm_args,
        })
    }
}

#[async_trait]
impl ModLoader for NeoForgeMC {
    async fn install(
        &self,
        options: &LaunchOptions,
        input: &LoaderInstallInput,
        client: &reqwest::Client,
        event_tx: &Sender<LaunchEvent>,
    ) -> Result<LoaderResult, LoaderError> {
        let (version_id, main_class, libraries, extra_game_args, extra_jvm_args) = self
            .install(
                options,
                &input.mc_version,
                &input.java_path,
                &input.mc_jar,
                &input.mc_json,
                &options.loader.build,
                client,
                event_tx,
            )
            .await?;

        Ok(LoaderResult {
            libraries,
            main_class,
            loader_version: version_id,
            loader_type: LoaderType::NeoForge,
            extra_game_args,
            extra_jvm_args,
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_loader_forge_is_dyn() {
        let _loader: Box<dyn ModLoader> = create_loader(LoaderType::Forge);
    }

    #[test]
    fn create_loader_neoforge_is_dyn() {
        let _loader: Box<dyn ModLoader> = create_loader(LoaderType::NeoForge);
    }

    #[test]
    fn create_loader_fabric_is_dyn() {
        let _loader: Box<dyn ModLoader> = create_loader(LoaderType::Fabric);
    }

    #[test]
    fn create_loader_legacy_fabric_is_dyn() {
        let _loader: Box<dyn ModLoader> = create_loader(LoaderType::LegacyFabric);
    }

    #[test]
    fn create_loader_quilt_is_dyn() {
        let _loader: Box<dyn ModLoader> = create_loader(LoaderType::Quilt);
    }

    #[test]
    fn all_loader_types_are_dispatchable() {
        let types = [
            LoaderType::Forge,
            LoaderType::NeoForge,
            LoaderType::Fabric,
            LoaderType::LegacyFabric,
            LoaderType::Quilt,
        ];
        for t in types {
            let _loader = create_loader(t);
        }
    }
}
