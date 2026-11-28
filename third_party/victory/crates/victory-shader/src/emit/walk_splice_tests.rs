use super::*;
use crate::native::HwStage;

/// A walk with results: the comparison and skip, a copy of the base and of the direction, the
/// node test (NSA, descriptor s[4:7]: node v31, extent v15, origin v11 v14 v13, direction v28
/// v20 v59, inverse v23 v22 v21), the loop keeping the leaf in v17, the count in v24 and the
/// extent in v15, the backward branch; after it the leaf's bit 2, the count against 0xfffe and
/// the extent read.
fn walk_words(base_copy: bool) -> Vec<u32> {
    let mut w = vec![
        0xbf06_8016, // s_cmp_eq_u32 s22, 0
        0,           // s_cbranch_scc1 past (below)
        0x7e38_0312, // v_mov_b32 v28, v18
        0x7e28_0313, // v_mov_b32 v20, v19
        0x7e76_0335, // v_mov_b32 v59, v53
        0x7e2a_5535, // v_rcp_f32 v21, v53
    ];
    if base_copy {
        w.push(0xbe84_0328); // s_mov_b32 s4, s40
    }
    let head = w.len();
    w.extend([
        0xf198_9f07,
        0x0001_1f1f,
        0x0d0e_0b0f,
        0x173b_141c,
        0x0000_1516,
    ]);
    w.extend([
        0x7e22_031f, // v_mov_b32 v17, v31
        0x4a30_3081, // v_add_nc_u32 v24, 1, v24
        0x1e1e_410f, // v_min_f32 v15, v15, v32
    ]);
    let back = w.len();
    w.push(0xbf84_0000 | (((head as i32 - back as i32 - 1) as u32) & 0xffff)); // s_cbranch_scc0 head
    let past = w.len();
    w[1] = 0xbf85_0000 | ((past - 2) as u32); // s_cbranch_scc1 past
    w.extend([
        0x3616_2284, // v_and_b32 v11, 4, v17
        0x7d82_30ff, // v_cmp_lt_u32 vcc_lo, 0xfffe, v24
        0x0000_fffe,
        0x7e18_030f, // v_mov_b32 v12, v15
        0xbf81_0000, // s_endpgm
    ]);
    w
}

fn native(words: &[u32]) -> NativeProgram {
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 64,
        layout: Vec::new(),
        used: Vec::new(),
        io: crate::native::StageIo::default(),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

#[test]
fn off_by_default_and_then_no_binding() {
    assert!(!native_shadow_auto());
    assert_eq!(shadow_auto_binding(b"anything"), None);
}

#[test]
fn a_walk_gets_its_resume_results_and_base() {
    let words = walk_words(true);
    let got = derive(&native(&words));
    assert_eq!(got.len(), 1, "{got:#x?}");
    let d = got[0].as_ref().expect("derived");
    assert_eq!(d.resume, d.found.past);
    assert_eq!(
        d.outs,
        vec![Out::HitT(15), Out::Hit(17, ONE), Out::Always(24, ZERO)]
    );
    assert_eq!(d.choose, Some(40));
    let w = d.walk();
    assert_eq!(
        (w.skip, w.head, w.resume, w.structure, w.mask),
        (4, 4 * 7, d.found.past, 0, 0xff)
    );
}

#[test]
fn without_a_copied_base_the_walk_has_no_choice() {
    let d = derive(&native(&walk_words(false)))
        .remove(0)
        .expect("derived");
    assert_eq!(d.choose, None);
}

#[test]
fn a_result_the_arm_cannot_give_refuses_the_walk() {
    let mut words = walk_words(true);
    // After the walk, the leaf is read as an address (v_add_nc_u32 v11, 52, v17).
    let at = words.iter().position(|&w| w == 0x3616_2284).unwrap();
    words[at] = 0x4a16_22b4;
    assert_eq!(derive(&native(&words)), vec![Err((4, Refused::Output(17)))]);
}

#[test]
fn a_ray_written_in_the_loop_refuses_the_walk() {
    let mut words = walk_words(true);
    // The loop writes the register the direction's first component is copied from
    // (v_mov_b32 v18, v31 for the leaf copy).
    let at = words.iter().position(|&w| w == 0x7e22_031f).unwrap();
    words[at] = 0x7e24_031f;
    assert_eq!(derive(&native(&words)), vec![Err((4, Refused::Ray(18)))]);
    // Its copy, the operand itself, may be written (and put back) in the loop.
    let mut words = walk_words(true);
    let at = words.iter().position(|&w| w == 0x1e1e_410f).unwrap();
    words.insert(at, 0x7e38_0312); // v_mov_b32 v28, v18
    let back = words.iter().rposition(|&w| w >> 16 == 0xbf84).unwrap();
    words[back] -= 1; // one word further back to the head
    words[1] += 1; // the skip one word further
    assert!(derive(&native(&words))[0].is_ok(), "{:?}", derive(&native(&words)));
}

#[test]
fn a_loop_leaving_elsewhere_refuses_the_walk() {
    let mut words = walk_words(true);
    // The count's step becomes a branch to the program's end, past the resume.
    let at = words.iter().position(|&w| w == 0x4a30_3081).unwrap();
    let end = words.iter().position(|&w| w == 0xbf81_0000).unwrap();
    words[at] = 0xbf87_0000 | (end - at - 1) as u32; // s_cbranch_vccnz s_endpgm
    let got = derive(&native(&words));
    assert!(matches!(got[..], [Err((4, Refused::Exit(_)))]), "{got:?}");
}

#[test]
fn the_splice_branches_to_a_region_and_widens_the_program() {
    let words = walk_words(true);
    let mut n = native(&words);
    let before = n.code.len();
    assert!(rewrite(&mut n));
    assert!(n.code.len() > before);
    assert_eq!(n.vgprs, 64 + shadow_patch::ARM_VGPRS);
    let skip = u32::from_le_bytes(n.code[4..8].try_into().unwrap());
    assert_eq!(skip >> 16, 0xbf82, "s_branch to the region");
    // A program without a walk is left alone.
    let mut plain = native(&[0x7e18_030f, 0xbf81_0000]);
    assert!(!rewrite(&mut plain));
}

/// Where a hand-found walk's results are only those the derivation knows, it is derived as its
/// row has it.
fn derivable(w: &Walk<'_>) -> bool {
    w.outs
        .iter()
        .all(|o| matches!(o, Out::HitT(_) | Out::Hit(_, ONE) | Out::Always(_, ZERO)))
}

/// The title's own programs (not in the repository: `SHADOW_TRACE_DIR`, a folder of their
/// containers `<name>.<hash>.cdxil`):
/// - every hand-found walk whose results the derivation knows is derived with the row's resume
///   and results; every other hand-found walk is refused;
/// - every program with a derived walk, spliced, translates and its DXIL validates;
/// - each program with a walk is listed with its derivation (a folder of every compute
///   container lists the whole corpus).
#[test]
fn the_title_walks_are_derived_as_the_hand_found_rows() {
    let Some(dir) = std::env::var_os("SHADOW_TRACE_DIR") else {
        eprintln!("SHADOW_TRACE_DIR not set: skipped");
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .expect("the folder")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "cdxil"))
        .collect();
    files.sort();
    let mut spliced = 0;
    for path in &files {
        let bytes = std::fs::read(path).expect("the container");
        let c = crate::dxbc::Container::parse(&bytes).expect("a container");
        let Some(part) = super::super::program_part(&c) else {
            continue;
        };
        let Some(mut native) = crate::native_programs(part.data)
            .ok()
            .and_then(|n| n.into_iter().find(|p| p.stage == HwStage::Cs))
        else {
            continue;
        };
        let got = derive(&native);
        if got.is_empty() {
            continue;
        }
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if let Some(trace) = shadow_patch::known(&c.digest, &native) {
            for w in trace.walks {
                let d = got.iter().find(|d| match d {
                    Ok(d) => d.found.skip == w.skip,
                    Err((skip, _)) => *skip == w.skip,
                });
                match (derivable(w), d) {
                    (true, Some(Ok(d))) => {
                        assert_eq!(d.resume, w.resume, "{name} walk at {:#x}", w.skip);
                        let mut want = w.outs.to_vec();
                        let mut have = d.outs.clone();
                        want.sort_by_key(|o| format!("{o:?}"));
                        have.sort_by_key(|o| format!("{o:?}"));
                        assert_eq!(have, want, "{name} walk at {:#x}", w.skip);
                    }
                    (false, Some(Err(_))) => {}
                    other => panic!("{name} walk at {:#x}: {other:#x?}", w.skip),
                }
            }
        } else if got.iter().any(Result::is_ok) {
            assert!(rewrite(&mut native), "{name}");
            let shader = (|| {
                let p = crate::spill::build(&native)?;
                let st = crate::structure::structure(&p)?;
                let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st))?;
                let io = crate::io::raise(&p, &native)?;
                let forced = &super::super::lower::heap_address::FORCED;
                forced.set(true);
                let shader = super::super::emit(&c, &native, &p, &st, &io, &res);
                forced.set(false);
                shader
            })();
            let shader = shader.unwrap_or_else(|e| panic!("{name}: {e:?}"));
            if let Ok(mut v) = super::super::validator::Validator::load(
                super::super::validator::Validator::default_path().as_deref(),
            ) {
                let mut copy = shader.dxil.clone();
                v.validate(&mut copy)
                    .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            }
            assert!(binding_of(&bytes).is_some(), "{name}");
            spliced += 1;
        } else {
            assert!(binding_of(&bytes).is_none(), "{name}");
        }
        let listed: Vec<String> = got
            .iter()
            .map(|d| match d {
                Ok(d) => format!(
                    "{:#x}->{:#x} {} {:?}",
                    d.found.skip,
                    d.resume,
                    d.choose.map_or("fixed".into(), |s| format!("s{s}")),
                    d.outs
                ),
                Err((skip, r)) => format!("{skip:#x} refused {r:x?}"),
            })
            .collect();
        eprintln!("walk_splice: {name}: {listed:?}");
    }
    eprintln!("walk_splice: {spliced} programs without a row spliced");
}
