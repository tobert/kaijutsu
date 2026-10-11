//! Sensor for `assets/shaders/`: every shader is composed the way Bevy composes
//! it at runtime and the result is validated, without a GPU.
//!
//! Bevy's `ShaderCache` is the real composition path: it resolves WESL
//! imports, applies shader defs and `@if` conditions, and produces the WGSL
//! that goes to wgpu. This test drives that same `ShaderCache`, with a loader
//! that parses and validates the WGSL with naga instead of creating a GPU
//! module. Bevy's own library shaders (`bevy_pbr::render::forward_io`, ...)
//! are read from the bevy crate sources that `Cargo.lock` pins, found in the
//! cargo registry (`CARGO_HOME`, default `~/.cargo`).
//!
//! It catches: syntax errors, imports of modules or items that do not exist
//! in the pinned Bevy, `constants::` names that nothing defines, uses of a
//! struct member that the vertex-attribute defs below do not enable,
//! type errors and other naga validation failures, a missing entry point, and
//! a Rust shader-path literal that names no file.
//!
//! It does not catch: defs Bevy adds for a pipeline this test does not
//! model (the `*_DEFS` lists below stand in for the mesh and UI material
//! pipelines), bind group layouts that disagree with the Rust `AsBindGroup`
//! types, wgpu limits or driver differences, or how the shader looks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bevy::asset::AssetId;
use bevy::shader::{
    Shader, ShaderCache, ShaderCacheError, ShaderCacheSource, ShaderDefVal, ValidateShader,
};

type Cache = ShaderCache<String, ()>;

/// What `MaterialPlugin` and the mesh pipeline define for a `Mesh3d` with
/// position, normal and UV attributes.
fn mesh_material_defs() -> Vec<ShaderDefVal> {
    vec![
        ShaderDefVal::UInt("MATERIAL_BIND_GROUP".into(), 3),
        ShaderDefVal::Bool("VERTEX_POSITIONS".into(), true),
        ShaderDefVal::Bool("VERTEX_NORMALS".into(), true),
        ShaderDefVal::Bool("VERTEX_UVS".into(), true),
        ShaderDefVal::Bool("VERTEX_UVS_A".into(), true),
    ]
}

/// What the UI material pipeline defines: nothing beyond the file's own
/// imports.
fn ui_material_defs() -> Vec<ShaderDefVal> {
    Vec::new()
}

/// Bevy library defs that `bevy_pbr` attaches when it loads a library shader
/// (`ShaderSettings` in its plugin build); the values are the defaults of the
/// settings those calls compute.
fn library_defs(module: &str) -> Vec<ShaderDefVal> {
    match module {
        "bevy_pbr/render/mesh_view_types.wesl" => vec![
            ShaderDefVal::UInt("MAX_DIRECTIONAL_LIGHTS".into(), 10),
            ShaderDefVal::UInt("MAX_CASCADES_PER_LIGHT".into(), 4),
        ],
        _ => Vec::new(),
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `(crate name, version)` for every `bevy_*` package in Cargo.lock.
fn locked_bevy_crates() -> BTreeMap<String, String> {
    let lock = std::fs::read_to_string(workspace_root().join("Cargo.lock"))
        .expect("read Cargo.lock");
    let mut crates = BTreeMap::new();
    let mut name: Option<String> = None;
    for line in lock.lines() {
        if let Some(n) = line.strip_prefix("name = \"") {
            name = Some(n.trim_end_matches('"').to_string());
        } else if let (Some(v), Some(n)) = (line.strip_prefix("version = \""), name.take())
            && n.starts_with("bevy_")
        {
            crates.insert(n, v.trim_end_matches('"').to_string());
        }
    }
    crates
}

fn registry_src_dirs() -> Vec<PathBuf> {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").expect("HOME or CARGO_HOME")).join(".cargo")
        });
    let src = cargo_home.join("registry/src");
    std::fs::read_dir(&src)
        .unwrap_or_else(|e| panic!("read {}: {e}", src.display()))
        .map(|entry| entry.expect("registry entry").path())
        .collect()
}

fn collect_wesl(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read crate source dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_wesl(&path, out);
        } else if path.extension().is_some_and(|e| e == "wesl") {
            out.push(path);
        }
    }
}

fn asset_id(n: u128) -> AssetId<Shader> {
    AssetId::Uuid {
        uuid: bevy::asset::uuid::Uuid::from_u128(n),
    }
}

/// naga parse + validate, standing in for wgpu's shader-module creation.
fn validate_wgsl(
    _: &(),
    source: ShaderCacheSource,
    _: &ValidateShader,
) -> Result<String, ShaderCacheError> {
    let ShaderCacheSource::Wgsl(wgsl) = source else {
        panic!("expected composed WGSL");
    };
    let module = naga::front::wgsl::parse_str(&wgsl).map_err(|e| {
        ShaderCacheError::ProcessShaderError(format!(
            "naga parse: {}",
            e.emit_to_string(&wgsl)
        ))
    })?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|e| {
        ShaderCacheError::ProcessShaderError(format!(
            "naga validate: {}",
            e.emit_to_string(&wgsl)
        ))
    })?;
    Ok(wgsl)
}

/// A cache holding every Bevy library shader of the pinned release, as Bevy
/// registers them (`embedded://<crate>/<path under src>.wesl`).
fn cache_with_bevy_libraries() -> (Cache, u128) {
    let mut cache: Cache = ShaderCache::new((), validate_wgsl);
    let mut next_id = 1u128;
    let locked = locked_bevy_crates();
    assert!(
        locked.contains_key("bevy_pbr"),
        "Cargo.lock has no bevy_pbr"
    );
    for (krate, version) in &locked {
        let dir_name = format!("{krate}-{version}");
        let Some(src) = registry_src_dirs()
            .into_iter()
            .map(|r| r.join(&dir_name).join("src"))
            .find(|p| p.is_dir())
        else {
            // Only crates with shaders matter; a locked crate absent from the
            // registry is a failure only if it is one Bevy shaders import.
            continue;
        };
        let mut files = Vec::new();
        collect_wesl(&src, &mut files);
        for file in files {
            let rel = file.strip_prefix(&src).unwrap().to_string_lossy().replace('\\', "/");
            let embedded = format!("{krate}/{rel}");
            let source = std::fs::read_to_string(&file).expect("read library shader");
            let mut shader = Shader::from_wesl(source, format!("embedded://{embedded}"));
            shader.shader_defs = library_defs(&embedded);
            cache.set_shader(asset_id(next_id), shader);
            next_id += 1;
        }
    }
    assert!(
        next_id > 50,
        "found only {} Bevy library shaders; is the cargo registry source present?",
        next_id - 1
    );
    (cache, next_id)
}

/// Composes one shader under `defs`, with all sibling `.wesl` assets
/// registered at their asset paths (`shaders/<name>.wesl`).
fn compose(file: &str, defs: &[ShaderDefVal]) -> Result<Arc<String>, ShaderCacheError> {
    let (mut cache, mut next_id) = cache_with_bevy_libraries();
    let shader_dir = workspace_root().join("assets/shaders");
    let mut root_id = None;
    for entry in std::fs::read_dir(&shader_dir).expect("read assets/shaders") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "wesl") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let source = std::fs::read_to_string(&path).expect("read shader");
        let id = asset_id(next_id);
        next_id += 1;
        if name == file {
            root_id = Some(id);
        }
        cache.set_shader(id, Shader::from_wesl(source, format!("shaders/{name}")));
    }
    let root_id = root_id.unwrap_or_else(|| panic!("no assets/shaders/{file}"));
    cache.get(0, root_id, defs)
}

fn assert_composes(file: &str, defs: &[ShaderDefVal], entry_points: &[&str]) {
    let wgsl = compose(file, defs).unwrap_or_else(|e| {
        let hint = if matches!(e, ShaderCacheError::ShaderImportNotYetAvailable) {
            " (an import names a module no shader registers; Bevy would wait for it forever)"
        } else {
            ""
        };
        panic!("{file}: {e}{hint}")
    });
    for entry in entry_points {
        assert!(
            wgsl.contains(&format!("fn {entry}(")),
            "{file}: composed output has no `{entry}` entry point"
        );
    }
}

#[test]
fn mesh_material_shaders_compose() {
    for file in [
        "well_rings.wesl",
        "track_ray.wesl",
        "terrace_ring.wesl",
        "trace_glow.wesl",
        "well_card.wesl",
    ] {
        assert_composes(file, &mesh_material_defs(), &["fragment"]);
    }
}

#[test]
fn ui_material_shader_composes() {
    assert_composes("block_fx.wesl", &ui_material_defs(), &["fragment"]);
}

#[test]
fn plain_wgsl_shaders_validate() {
    let shader_dir = workspace_root().join("assets/shaders");
    let mut checked = 0;
    for entry in std::fs::read_dir(&shader_dir).expect("read assets/shaders") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "wgsl") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read shader");
        assert!(
            !source.lines().any(|l| l.trim_start().starts_with("#import")
                || l.trim_start().starts_with("#if")
                || l.contains("#{")),
            "{}: preprocessor directives belong in a .wesl file",
            path.display()
        );
        validate_wgsl(&(), ShaderCacheSource::Wgsl(source.clone()), &ValidateShader::Disabled)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        for entry_point in ["vertex", "fragment"] {
            assert!(
                source.contains(&format!("fn {entry_point}(")),
                "{}: no `{entry_point}` entry point",
                path.display()
            );
        }
        checked += 1;
    }
    assert!(checked >= 5, "expected the plain WGSL shaders, saw {checked}");
}

/// Every `"shaders/<name>.wesl|wgsl"` literal in the Rust sources names a file.
#[test]
fn shader_path_literals_name_files() {
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    rust_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    let mut seen = 0;
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read source");
        for line in text.lines() {
            let code = line.split("//").next().unwrap_or("");
            let mut rest = code;
            while let Some(start) = rest.find("\"shaders/") {
                let tail = &rest[start + 1..];
                let end = tail.find('"').expect("closing quote");
                let literal = &tail[..end];
                if literal.ends_with(".wesl") || literal.ends_with(".wgsl") {
                    assert!(
                        workspace_root().join("assets").join(literal).is_file(),
                        "{}: shader path literal `{literal}` names no file",
                        file.display()
                    );
                    seen += 1;
                }
                rest = &tail[end..];
            }
        }
    }
    assert!(seen >= 11, "expected the 11 shader path literals, saw {seen}");
}
