//! `VICTORY_MESH_LINK=1` (off by default): a mesh pipeline whose mesh program's outputs pass
//! D3D12's 32 KiB is made with the pair translated for it (`victory_shader::emit::link_mesh`):
//! the mesh program writes only the outputs its pixel program reads, the pixel program
//! declares only the inputs it reads. 71 of the title's mesh programs (pedestrian and prop
//! transforms with fire, spatter, decal and transmission variants: `MS_VS_PedTransform*`,
//! `MSNC_VS_PedTransformSkin*`, `MS(R)_VS_Transform*Spatter*Fire*`) fail the DXIL validator
//! alone ("vertex plus primitive output size is greater than 32768"); NVIDIA's driver makes
//! their pipelines anyway, from invalid programs. The pipeline's root signature still comes
//! from the shared translations (`link_mesh` keeps a pair only when it binds the same), and
//! the pipeline library names the linked code, so its entries are its own.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use victory_shader::emit::HostShader;

use super::Shader;

/// Whether `VICTORY_MESH_LINK=1`.
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_MESH_LINK").is_ok_and(|v| v.trim() == "1"))
}

type Pair = Option<(Arc<Vec<u8>>, Arc<Vec<u8>>)>;

/// The code a mesh pipeline is made with: `dxils` (mesh, pixel) as translated, or with `on` the
/// pair `link` makes of them.
pub(super) fn code(
    on: bool,
    dxils: Vec<Arc<Vec<u8>>>,
    link: impl FnOnce(&Arc<Vec<u8>>, &Arc<Vec<u8>>) -> Pair,
) -> Vec<Arc<Vec<u8>>> {
    let made = match &dxils[..] {
        [m, p] if on => link(m, p),
        _ => None,
    };
    made.map_or(dxils, |(m, p)| vec![m, p])
}

/// Linked pairs by mesh and pixel hash: one translation per pair, whatever the states.
static LINKED: Mutex<Option<HashMap<(String, String), Pair>>> = Mutex::new(None);

/// The code of the mesh program `ms` and the pixel program `ps` linked for one pipeline, when
/// `ms_dxil` (its shared translation) passes 32 KiB and the pair links; `None` keeps the
/// shared code.
pub(super) fn linked(
    ms: &Shader,
    ms_dxil: &Arc<Vec<u8>>,
    ps: &Shader,
    ps_dxil: &Arc<Vec<u8>>,
) -> Pair {
    if !matches!(victory_shader::emit::mesh_fits(ms_dxil), Ok(false)) {
        return None;
    }
    let key = (ms.hash.clone(), ps.hash.clone());
    if let Some(done) = LINKED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .get(&key)
    {
        return done.clone();
    }
    let (ms_t, ps_t) = (ms.translated()?, ps.translated()?);
    let with = |host: &HostShader, dxil: &Arc<Vec<u8>>| HostShader {
        dxil: dxil.to_vec(),
        ..host.clone()
    };
    let made = victory_shader::emit::link_mesh(
        &ms.container,
        &with(&ms_t.host, ms_dxil),
        &ps.container,
        &with(&ps_t.host, ps_dxil),
    );
    let pair = match made {
        Ok(Some((m, p))) => {
            tracing::info!(mesh = %ms.hash, pixel = %ps.hash, mesh_bytes = m.dxil.len(), "mesh link: pair translated for its pipeline");
            Some((Arc::new(m.dxil), Arc::new(p.dxil)))
        }
        Ok(None) => {
            tracing::warn!(mesh = %ms.hash, pixel = %ps.hash, "mesh link: the pair does not link; shared code kept");
            None
        }
        Err(e) => {
            tracing::warn!(mesh = %ms.hash, pixel = %ps.hash, error = %e, "mesh link: translation failed; shared code kept");
            None
        }
    };
    LINKED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(key, pair.clone());
    pair
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::code;

    #[test]
    fn a_linked_pair_replaces_the_shared_code_only_when_on() {
        let shared = vec![Arc::new(vec![1u8]), Arc::new(vec![2u8])];
        let linked = || Some((Arc::new(vec![3u8]), Arc::new(vec![4u8])));
        let bytes = |v: Vec<Arc<Vec<u8>>>| v.iter().map(|d| d[0]).collect::<Vec<u8>>();
        assert_eq!(bytes(code(true, shared.clone(), |_, _| linked())), [3, 4]);
        assert_eq!(bytes(code(false, shared.clone(), |_, _| linked())), [1, 2]);
        assert_eq!(bytes(code(true, shared.clone(), |_, _| None)), [1, 2]);
        // a mesh pipeline without a pixel program keeps its code
        assert_eq!(bytes(code(true, vec![shared[0].clone()], |_, _| linked())), [1]);
    }
}
