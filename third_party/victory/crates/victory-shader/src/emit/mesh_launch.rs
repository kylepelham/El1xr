//! What translated mesh shaders declare for the host's launch stage
//! (`VICTORY_MESH_LAUNCH=1`, off by default; `victory-gpu`'s `launch`): an
//! amplification shader of the host's in front of every translated mesh
//! shader reads an indirect mesh record on the card and launches its groups,
//! with no pass of ours before the draw.
//!
//! Source: fxceless/victory-fork `b1d777a` (`mesh_payload_bytes`,
//! `LAUNCH_SPACE`), after fork PR #1381.

/// The bytes of payload every translated mesh shader declares and never
/// reads: 4 with `VICTORY_MESH_LAUNCH=1`, none otherwise (as before). An
/// amplification shader's payload must have the size the mesh shader
/// declares (a mesh shader that declares none is refused with one: the
/// RTX 5080's driver returns `E_INVALIDARG`), and one that declares a
/// payload is still taken without an amplification shader. The switch is
/// part of the shader cache's key (`victory-gpu`'s `shader_cache`).
pub fn mesh_payload_bytes() -> u32 {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    payload_bytes(
        *ON.get_or_init(|| std::env::var_os("VICTORY_MESH_LAUNCH").is_some_and(|v| v == "1")),
    )
}

fn payload_bytes(on: bool) -> u32 {
    if on { 4 } else { 0 }
}

/// The register space the host's amplification shader binds in (`b0`,
/// `t0`, `u0`): no translated shader binds there.
pub const LAUNCH_SPACE: u32 = 9;

#[cfg(test)]
mod tests {
    use super::{LAUNCH_SPACE, payload_bytes};
    use crate::emit::shader::{ROOT_CBV_SPACE, SAMPLER_SPACE, USER_DATA_SPACE};

    #[test]
    fn the_payload_is_declared_only_with_the_switch() {
        assert_eq!(payload_bytes(false), 0);
        assert_eq!(payload_bytes(true), 4);
    }

    #[test]
    fn the_launch_space_is_no_translated_shaders() {
        for space in [USER_DATA_SPACE, SAMPLER_SPACE, ROOT_CBV_SPACE] {
            assert_ne!(LAUNCH_SPACE, space);
        }
    }
}
