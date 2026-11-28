//! Where a program the stand-in for the indirect light follows
//! (`crate::ambient`: [`Role::Resolve`], [`Role::Radiance`], [`Role::Air`])
//! has its image and its depth.
//!
//! The title has six variants of the program that resolves the diffuse light
//! in `shaders.rpf`: `CS_ScreenProbes_TemporalResolve_` `a911b49c`,
//! `757be8ff` and `CB_a1c765aa`, each also `_ReflectiveCaustics` (read
//! statically with `victory shader-info --disasm`). Every one reads the depth
//! at register t213 (table 0, dword 464) and writes the light at u36, but the
//! `_CB` pair has one more image before it: u36 is at (1, 184) there, and at
//! (1, 176), the place the stand-in has always used, is u40. So the stand-in
//! either skipped those two (another format) or raised u40 and left the light
//! as the title had it, black indoors without rays.
//!
//! (zivyn/codec-d--cool_fox `c6bad97798`, GPL-3.0-or-later) With
//! `VICTORY_AMBIENT_VARIANTS=1` (live, off by default) the places are taken
//! from each program's own resource list by shader register. Off: the fixed
//! places, as before, and [`Places::elsewhere`] says when the fixed image is
//! not the one at the register (counted by `crate::ambient_cover`).

use victory_shader::{ResourceType, RootType, UsedResource};

use crate::ambient::Role;

/// Root parameter and place in its table, in dwords.
pub(crate) type Place = (u8, u32);

/// Where the resolve writes the diffuse light (R9G9B9E5 as 32-bit integers)
/// and reads the scene's depth, where the other writes the light of visible
/// surfaces, and where the light the air scatters is: in the variants the
/// fixed places were found in.
const LIGHT: Place = (1, 176);
const DEPTH: Place = (0, 464);
const RADIANCE: Place = (1, 16);
const AIR: Place = (1, 32);

/// Their shader registers, the same in every variant.
const LIGHT_REGISTER: u32 = 36;
const DEPTH_REGISTER: u32 = 213;
const RADIANCE_REGISTER: u32 = 45;
const AIR_REGISTER: u32 = 44;

/// Where a program has the image the stand-in raises and (for the resolve)
/// the depth it reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Places {
    pub(crate) image: Place,
    pub(crate) depth: Option<Place>,
    /// The image at the fixed place is not at the register the role writes
    /// (the `_CB` resolves' u40): only with the switch off.
    pub(crate) elsewhere: bool,
}

/// `VICTORY_AMBIENT_VARIANTS=1`: places by register.
pub(crate) fn variants() -> bool {
    static SET: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_AMBIENT_VARIANTS");
    SET.get_bool()
}

fn fixed(role: Role) -> Option<(Place, u32)> {
    match role {
        Role::Resolve => Some((LIGHT, LIGHT_REGISTER)),
        Role::Radiance => Some((RADIANCE, RADIANCE_REGISTER)),
        Role::Air => Some((AIR, AIR_REGISTER)),
        Role::Miss | Role::Reflect | Role::Knee => None,
    }
}

fn table(used: &[UsedResource], kind: ResourceType) -> impl Iterator<Item = &UsedResource> {
    used.iter()
        .filter(move |u| u.kind == kind && u.root == RootType::Table)
}

fn at(used: &[UsedResource], kind: ResourceType, place: Place) -> Option<&UsedResource> {
    table(used, kind).find(|u| (u.slot, u.table_offset_dwords) == place)
}

fn by_register(
    used: &[UsedResource],
    kind: ResourceType,
    register: u32,
    [none, twice]: [&'static str; 2],
) -> Result<Place, &'static str> {
    let mut found = table(used, kind).filter(|u| u.lower_bound == register);
    let first = found.next().ok_or(none)?;
    if found.next().is_some() {
        return Err(twice);
    }
    Ok((first.slot, first.table_offset_dwords))
}

/// Where the program with these used resources has what the stand-in of
/// `role` needs (by register when `registers`), or why it has not.
pub(crate) fn places(
    role: Role,
    used: &[UsedResource],
    registers: bool,
) -> Result<Places, &'static str> {
    let (place, register) = fixed(role).ok_or("not a role of the indirect light")?;
    if !registers {
        let image = at(used, ResourceType::Uav, place).ok_or("missing image UAV")?;
        let depth = match role {
            Role::Resolve => Some(at(used, ResourceType::Srv, DEPTH).ok_or("missing depth SRV")?),
            _ => None,
        };
        return Ok(Places {
            image: place,
            depth: depth.map(|d| (d.slot, d.table_offset_dwords)),
            elsewhere: image.lower_bound != register,
        });
    }
    let image = by_register(
        used,
        ResourceType::Uav,
        register,
        [
            "no image UAV at the register it writes",
            "two image UAVs at the register it writes",
        ],
    )?;
    let depth = match role {
        Role::Resolve => Some(by_register(
            used,
            ResourceType::Srv,
            DEPTH_REGISTER,
            [
                "no depth SRV at the register it reads",
                "two depth SRVs at the register it reads",
            ],
        )?),
        _ => None,
    };
    Ok(Places {
        image,
        depth,
        elsewhere: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn used(kind: ResourceType, slot: u8, offset: u32, register: u32) -> UsedResource {
        UsedResource {
            kind,
            root: RootType::Table,
            table_offset_dwords: offset,
            lower_bound: register,
            stride: 4,
            dwords: 8,
            user_data: slot + 4,
            slot,
        }
    }

    /// A resolve's resources as the title's variants have them (registers
    /// and places as `shader-info --disasm` lists them): `cb` adds the
    /// image before the light, which moves u40 to 176 and u36 to 184.
    fn resolve(cb: bool) -> Vec<UsedResource> {
        let shift = if cb { 8 } else { 0 };
        let mut list = vec![
            used(ResourceType::Srv, 0, 184, 92),
            used(ResourceType::Srv, 0, 464, DEPTH_REGISTER),
            used(ResourceType::Srv, 1, 16, 397),
            used(ResourceType::Srv, 1, 32, 368),
        ];
        for (i, register) in [37, 31, 38, 33, 35, 40, 36, 32, 41, 39]
            .into_iter()
            .enumerate()
        {
            list.push(used(
                ResourceType::Uav,
                1,
                128 + shift + 8 * i as u32,
                register,
            ));
        }
        // Another kind at the light's register is no match.
        list.push(used(ResourceType::Srv, 1, 300, LIGHT_REGISTER));
        list
    }

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live("VICTORY_AMBIENT_VARIANTS", "1", variants, false, true);
    }

    #[test]
    fn fixed_places_find_the_plain_variants() {
        let p = places(Role::Resolve, &resolve(false), false).unwrap();
        assert_eq!(
            p,
            Places {
                image: (1, 176),
                depth: Some((0, 464)),
                elsewhere: false
            }
        );
    }

    #[test]
    fn fixed_places_take_u40_in_the_cb_variants() {
        let p = places(Role::Resolve, &resolve(true), false).unwrap();
        assert_eq!(
            p,
            Places {
                image: (1, 176),
                depth: Some((0, 464)),
                elsewhere: true
            }
        );
    }

    #[test]
    fn registers_find_every_variant() {
        let plain = places(Role::Resolve, &resolve(false), true).unwrap();
        assert_eq!(
            plain,
            Places {
                image: (1, 176),
                depth: Some((0, 464)),
                elsewhere: false
            }
        );
        let cb = places(Role::Resolve, &resolve(true), true).unwrap();
        assert_eq!(
            cb,
            Places {
                image: (1, 184),
                depth: Some((0, 464)),
                elsewhere: false
            }
        );
    }

    #[test]
    fn the_other_roles_have_their_image_alone() {
        let radiance = [used(ResourceType::Uav, 1, 16, RADIANCE_REGISTER)];
        let air = [
            used(ResourceType::Srv, 0, 432, 45),
            used(ResourceType::Srv, 1, 16, 317),
            used(ResourceType::Uav, 1, 32, AIR_REGISTER),
        ];
        for registers in [false, true] {
            assert_eq!(
                places(Role::Radiance, &radiance, registers).unwrap(),
                Places {
                    image: (1, 16),
                    depth: None,
                    elsewhere: false
                }
            );
            assert_eq!(
                places(Role::Air, &air, registers).unwrap(),
                Places {
                    image: (1, 32),
                    depth: None,
                    elsewhere: false
                }
            );
        }
    }

    #[test]
    fn what_is_missing_is_said() {
        let no_image: Vec<_> = resolve(false)
            .into_iter()
            .filter(|u| !(u.kind == ResourceType::Uav && u.lower_bound == LIGHT_REGISTER))
            .collect();
        assert_eq!(
            places(Role::Resolve, &no_image, true),
            Err("no image UAV at the register it writes")
        );
        assert_eq!(
            places(Role::Resolve, &no_image, false),
            Err("missing image UAV")
        );
        let no_depth: Vec<_> = resolve(true)
            .into_iter()
            .filter(|u| u.lower_bound != DEPTH_REGISTER)
            .collect();
        assert_eq!(
            places(Role::Resolve, &no_depth, true),
            Err("no depth SRV at the register it reads")
        );
        assert_eq!(
            places(Role::Resolve, &no_depth, false),
            Err("missing depth SRV")
        );
        let mut twice = resolve(false);
        twice.push(used(ResourceType::Uav, 1, 400, LIGHT_REGISTER));
        assert_eq!(
            places(Role::Resolve, &twice, true),
            Err("two image UAVs at the register it writes")
        );
        let mut root = vec![used(ResourceType::Uav, 1, 16, RADIANCE_REGISTER)];
        root[0].root = RootType::Descriptor;
        assert_eq!(
            places(Role::Radiance, &root, true),
            Err("no image UAV at the register it writes")
        );
        assert!(places(Role::Miss, &resolve(false), true).is_err());
    }
}
