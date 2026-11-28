//! Console wave64 compute programs a 32-lane host (RTX) refuses today, given forms we already
//! have by hash (offline, GTA VI 1.0.2.1, the RTX 5090 run's settings: each comes out with
//! `WaveSize(64)`, which the driver refuses, so every dispatch of it is dropped).
//!
//! `VICTORY_WAVE_WIDE_MORE` adds programs to the wave-wide list (`VICTORY_WAVE_WIDE=last` for
//! them, as `VICTORY_WATER_WIDE` does): one console wave a group (64 threads) with LDS read
//! and written in one lane-branch region, which the wave-wide branches take to 32-lane waves
//! (2 to 24 branches each, two group barriers per branch). `1`: trustno1-coder/VIctory-NVIDIA
//! PR #101's four (three `GpuParticleEmit` programs and the water's `_VolumePortalsOnly`). `2`:
//! those and the other twelve that form takes (the other `GpuParticleEmit` programs, the two hair
//! screen-space shadow masks and six `vfx_proj_decal_trail` programs).
//!
//! `VICTORY_WAVE_PAIRS_MORE=1` adds the four left, groups of 128 to 1024 threads that the
//! wave-wide form never takes, to the paired list (`VICTORY_WAVE_PAIRS_ONLY`'s): three
//! `vfx_proj_decal_trail` programs and `vfx_triangleClip:CS_CollectTriangles`. The paired form
//! already runs `CS_CollectTriangles_Meshlets` and the face relocations this way.

/// trustno1-coder/VIctory-NVIDIA PR #101.
const WIDE_101: [&str; 4] = [
    // GpuParticleEmit:CS_Emit_Dynamic_Model_AttachMesh_Skinned
    "5e3dd99e0f067aee",
    // GpuParticleEmit:CS_Emit_Dynamic_Model_Skinned
    "ae0deeafe219c15c",
    // water_shading:CS_TransmissionVolumetrics_VolumePortalsOnly
    "a10179ab7eda0dc3",
    // GpuParticleEmit:CS_Emit_Dynamic_Model_EdgeHashGrid
    "57f1b81de55c2842",
];

const WIDE_REST: [&str; 12] = [
    // GpuParticleEmit: CS_Emit_Dynamic_Model, _AttachMesh, _Edge, CS_EmitCounts_Dynamic_Edge
    "18d957e62926ed3e",
    "407eca669212d400",
    "9fe711566bec0d0a",
    "a40ae27594631121",
    // shadowmask: CS_GenerateShadowMasks_SampleHairScreenSpace, ..HairOnly_SampleHairScreenSpace
    "2889c21328c69e58",
    "888af37a584acf98",
    // vfx_proj_decal_trail: CS_SpawnLiquidTrail_SampleDecalRaycastTriangleTest(_Nodes),
    // CS_UpdateProjDecalTrailAttachment(_Nodes), CS_PackProjDecalTrailVertexBuffer,
    // CS_UpdateProjDecalTrailMeshIndices
    "ffa3a7645374474b",
    "98fef92f7a6d825e",
    "aeac9d880daff3ba",
    "a09f0c1fae2b5ca3",
    "a4cb3ce527ad8569",
    "f38f83439083b9ef",
];

const PAIRS: [&str; 4] = [
    // vfx_proj_decal_trail: CS_BuildProjDecalTrailProjectionBoxInstances (512 threads),
    // CS_BuildProjDecalTrailVertexBufferFromNodes (512), CS_DefragHeap_ComputeRelocations (128)
    "3b02415f67c6b0d8",
    "1d434307d981352a",
    "22338086ea4c5364",
    // vfx_triangleClip:CS_CollectTriangles (1024)
    "6832a9da15c2e584",
];

/// The hashes `VICTORY_WAVE_WIDE_MORE=<value>` adds to the wave-wide list.
pub(crate) fn wide(value: Option<&str>) -> Vec<&'static str> {
    match value.map(str::trim) {
        Some("1") => WIDE_101.to_vec(),
        Some("2") => WIDE_101.iter().chain(&WIDE_REST).copied().collect(),
        _ => Vec::new(),
    }
}

/// The hashes `VICTORY_WAVE_PAIRS_MORE=<value>` adds to the paired list.
pub(crate) fn pairs(value: Option<&str>) -> Vec<&'static str> {
    match value.map(str::trim) {
        Some("1") => PAIRS.to_vec(),
        _ => Vec::new(),
    }
}

/// `list` with `more` appended, each hash once.
pub(crate) fn extend(mut list: Vec<String>, more: &[&str]) -> Vec<String> {
    for h in more {
        if !list.iter().any(|s| s == h) {
            list.push((*h).to_owned());
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_adds_nothing() {
        for v in [None, Some("0"), Some(""), Some("3"), Some("yes")] {
            assert!(wide(v).is_empty(), "{v:?}");
            assert!(pairs(v).is_empty(), "{v:?}");
        }
        let list = vec!["02384a60fda6f2d0".to_owned()];
        assert_eq!(extend(list.clone(), &wide(None)), list);
        assert_eq!(extend(list.clone(), &pairs(None)), list);
    }

    #[test]
    fn values_name_their_programs_once() {
        assert_eq!(wide(Some("1")), WIDE_101);
        let all = wide(Some(" 2 "));
        assert_eq!(all.len(), 16);
        assert_eq!(all[..4], WIDE_101);
        assert_eq!(pairs(Some("1")), PAIRS);
        let mut every: Vec<&str> = all.iter().chain(&PAIRS).copied().collect();
        every.sort_unstable();
        every.dedup();
        assert_eq!(every.len(), 20);
        assert!(
            every
                .iter()
                .all(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        );
        let water = vec!["a10179ab7eda0dc3".to_owned()];
        assert_eq!(extend(water, &wide(Some("1"))).len(), 4);
    }
}
