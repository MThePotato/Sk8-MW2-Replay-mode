//! Optional local, preconverted character mesh. Assets remain outside the game archives.
use asset_core::AssetRef;
use asset_material::{AuthoredImage, MaterialCatalog};
use bevy::{
    asset::RenderAssetUsages,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
};
use serde::Deserialize;
use std::{
    path::Path,
    sync::{Arc, OnceLock},
};

#[derive(Deserialize)]
pub struct BotModel {
    #[serde(default = "default_lighting_gain")]
    pub lighting_gain: f32,
    pub joints: Vec<BotJoint>,
    pub surfaces: Vec<BotSurface>,
    textures: Vec<BotTexture>,
}
fn default_lighting_gain() -> f32 {
    1.0
}

#[derive(Deserialize)]
pub struct BotJoint {
    pub target: String,
    pub origin: [f32; 3],
    pub end: Option<[f32; 3]>,
    pub target_child: Option<String>,
}
#[derive(Deserialize)]
pub struct BotSurface {
    pub material: String,
    pub texture: usize,
    pub vertices: Vec<BotVertex>,
    pub indices: Vec<u32>,
}
#[derive(Deserialize)]
pub struct BotVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: u32,
    pub joints: [usize; 4],
    pub weights: [f32; 4],
}
#[derive(Deserialize)]
struct BotTexture {
    width: u16,
    height: u16,
    rgba: Vec<u8>,
}

pub fn local_bot_model() -> Option<&'static BotModel> {
    static MODEL: OnceLock<Option<BotModel>> = OnceLock::new();
    MODEL
        .get_or_init(|| {
            let path = std::env::var_os("IW4L_BOT_MODEL")?;
            match load(Path::new(&path)) {
                Ok(model) => Some(model),
                Err(error) => {
                    diag::info!(World, "local bot model: {error}");
                    None
                }
            }
        })
        .as_ref()
}

fn load(path: &Path) -> Result<BotModel, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let model: BotModel = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
    if !model.lighting_gain.is_finite()
        || !(0.25..=4.0).contains(&model.lighting_gain)
        || model.joints.is_empty()
        || model.surfaces.is_empty()
        || model.textures.iter().any(|t| {
            t.width == 0
                || t.height == 0
                || t.rgba.len() != usize::from(t.width) * usize::from(t.height) * 4
        })
        || model.surfaces.iter().any(|s| {
            s.texture >= model.textures.len()
                || s.indices.len() % 3 != 0
                || s.indices.iter().any(|&i| i as usize >= s.vertices.len())
                || s.vertices.iter().any(|v| {
                    v.joints.iter().any(|&j| j >= model.joints.len())
                        || v.position
                            .iter()
                            .chain(&v.normal)
                            .chain(&v.weights)
                            .any(|x| !x.is_finite())
                })
        })
    {
        return Err("invalid mesh, weights or texture dimensions".into());
    }
    Ok(model)
}

pub fn local_skate_board() -> Option<&'static BotModel> {
    static MODEL: OnceLock<Option<BotModel>> = OnceLock::new();
    MODEL
        .get_or_init(|| {
            let root = std::env::var_os("IW4L_SKATE_ASSETS")?;
            load(&Path::new(&root).join("board.json"))
                .map_err(|e| diag::warn!(World, "skate board: {e}"))
                .ok()
        })
        .as_ref()
}

static SKIN_MODELS: OnceLock<Vec<BotModel>> = OnceLock::new();
static SKIN_NAMES: OnceLock<Vec<String>> = OnceLock::new();
static SELECTED_SKIN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Loads every `skins/*.json` under the Skate assets folder once. The first
/// entry in the returned names is the default (no custom skin).
pub fn install_skins(root: &Path) -> Vec<String> {
    SKIN_NAMES
        .get_or_init(|| {
            let mut models = Vec::new();
            let mut names = vec!["Default (Soldier)".to_owned()];
            let dir = root.join("skins");
            if let Ok(entries) = std::fs::read_dir(dir) {
                let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
                paths.sort();
                for path in paths {
                    if path.extension().is_none_or(|e| e != "json") {
                        continue;
                    }
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("skin")
                        .to_owned();
                    match load(&path) {
                        Ok(model) => {
                            models.push(model);
                            names.push(name);
                        }
                        Err(e) => diag::warn!(World, "skater skin {}: {e}", path.display()),
                    }
                }
            }
            let _ = SKIN_MODELS.set(models);
            names
        })
        .clone()
}

pub fn skin_names() -> &'static [String] {
    SKIN_NAMES.get().map(Vec::as_slice).unwrap_or(&[])
}

/// All loaded custom skins (excluding the default soldier).
pub fn skins() -> &'static [BotModel] {
    SKIN_MODELS.get().map(Vec::as_slice).unwrap_or(&[])
}

/// The loaded custom skin at index `n`; index 0 is the default soldier.
pub fn skin_model(n: usize) -> Option<&'static BotModel> {
    if n == 0 {
        return None;
    }
    SKIN_MODELS.get().and_then(|models| models.get(n - 1))
}

pub fn selected_skin() -> usize {
    SELECTED_SKIN.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn select_skin(index: usize) {
    SELECTED_SKIN.store(index, std::sync::atomic::Ordering::Relaxed);
}

pub fn cycle_skin(delta: i32) {
    let n = SKIN_NAMES
        .get()
        .map_or(1, |names| names.len().max(1));
    let current = selected_skin();
    let next = if delta >= 0 {
        (current + 1) % n
    } else {
        (current + n - 1) % n
    };
    select_skin(next);
}

pub(crate) fn install_local_bot_materials(catalog: &mut MaterialCatalog) {
    for model in [local_bot_model(), local_skate_board()]
        .into_iter()
        .flatten()
    {
        install_materials(catalog, model);
    }
}

fn install_materials(catalog: &mut MaterialCatalog, model: &BotModel) {
    let Some(template) = catalog
        .materials
        .iter()
        .filter(|m| {
            m.name.as_str().starts_with("mc/")
                && !m.name.as_str().contains("gfx_")
                && !m.name.as_str().contains("fx_")
                && catalog.takes_model_lighting(m) == Some(true)
                && m.textures
                    .iter()
                    .any(|t| t.semantic == 2 && t.image.is_some())
        })
        .min_by_key(|m| {
            // Prefer a plain diffuse model shader. In particular, do not inherit
            // metallic reflection and tint settings from effects such as brass.
            let maps = m.textures.iter().filter(|t| t.semantic != 2).count();
            let character = ["body", "head", "skin", "soldier", "militia"]
                .iter()
                .any(|part| m.name.as_str().contains(part));
            (!character, maps, m.name.as_str().to_owned())
        })
        .cloned()
    else {
        return;
    };
    let image_template = template
        .textures
        .iter()
        .find(|t| t.semantic == 2)
        .and_then(|t| t.image)
        .and_then(|i| catalog.images.get(i))
        .cloned();
    let Some(image_template) = image_template else {
        return;
    };
    // IW4's packed normal format stores its neutral XY in alpha and green.
    // Neither the donor's normal map nor its metal specular map belongs to CJ.
    let mut neutral_maps = Vec::new();
    for (semantic, label, rgba) in [
        (5, "normal", [255, 130, 255, 130]),
        (8, "specular", [0, 0, 0, 255]),
    ] {
        let image = AuthoredImage {
            name: AssetRef::Real(format!("iw4l_bot_override/{label}")),
            semantic,
            width: 1,
            height: 1,
            depth: 1,
            level_count: 1,
            decoded: Some(Arc::new(Image::new(
                Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                TextureDimension::D2,
                rgba.to_vec(),
                TextureFormat::Rgba8Unorm,
                RenderAssetUsages::RENDER_WORLD,
            ))),
            payload: Arc::new(Vec::new()),
            decoded_variant: None,
            decoded_by: None,
            pending_decode: None,
            common_owned: false,
            use_srgb_reads: false,
            ..image_template.clone()
        };
        neutral_maps.push((semantic, catalog.link_image(image)));
    }
    for surface in &model.surfaces {
        let tex = &model.textures[surface.texture];
        let decoded = Image::new(
            Extent3d {
                width: u32::from(tex.width),
                height: u32::from(tex.height),
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            tex.rgba.clone(),
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::RENDER_WORLD,
        );
        let image = AuthoredImage {
            name: AssetRef::Real(format!("{}/diffuse", surface.material)),
            width: tex.width,
            height: tex.height,
            depth: 1,
            level_count: 1,
            decoded: Some(Arc::new(decoded)),
            payload: Arc::new(Vec::new()),
            decoded_variant: None,
            decoded_by: None,
            pending_decode: None,
            common_owned: false,
            use_srgb_reads: true,
            ..image_template.clone()
        };
        let image_index = catalog.link_image(image);
        let mut material = template.clone();
        material.name = AssetRef::Real(surface.material.clone());
        for binding in &mut material.textures {
            if binding.semantic == 2 {
                binding.image = Some(image_index);
            }
            if let Some((_, image)) = neutral_maps
                .iter()
                .find(|(semantic, _)| *semantic == binding.semantic)
            {
                binding.image = Some(*image);
            }
        }
        catalog.link_material(material);
    }
    diag::info!(
        World,
        "local bot model: installed {} surfaces using {}",
        model.surfaces.len(),
        template.name.as_str()
    );
}
