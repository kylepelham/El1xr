use super::*;
use crate::native::HwStage;

/// A walk of the shadow traces' shape, as words: the comparison and skip, three copies of the
/// direction, the node test (NSA: node v31, extent v15, origin v11 v14 v13, direction v28 v20
/// v59, inverse v23 v22 v21; from Checkerboard), the backward branch, the end.
fn walk_words() -> Vec<u32> {
    let mut w = vec![
        0xbf06_8016, // s_cmp_eq_u32 s22, 0
        0,           // s_cbranch_scc1 past (below)
        0x7e38_0312, // v_mov_b32 v28, v18
        0x7e28_0313, // v_mov_b32 v20, v19
        0x7e76_0335, // v_mov_b32 v59, v53
        0x7e2a_5535, // v_rcp_f32 v21, v53
    ];
    let head = w.len();
    w.extend([
        0xf198_9f07,
        0x0001_1f1f,
        0x0d0e_0b0f,
        0x173b_141c,
        0x0000_1516,
    ]);
    let back = w.len();
    w.push(0xbf84_0000 | (((head as i32 - back as i32 - 1) as u32) & 0xffff)); // s_cbranch_scc0 head
    let past = w.len();
    w[1] = 0xbf85_0000 | ((past - 2) as u32); // s_cbranch_scc1 past
    w.push(0xbf81_0000); // s_endpgm
    w
}

fn bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

#[test]
fn a_walk_of_the_shape_is_found_with_its_ray_at_the_skip() {
    let words = walk_words();
    let found = walks(&bytes(&words));
    assert_eq!(
        found,
        vec![Found {
            cmp: 0xbf06_8016,
            skip: 4,
            past: 4 * 12,
            head: 4 * 6,
            end: 4 * 11,
            lanes: 22,
            tests: vec![4 * 6],
            wide: false,
            origin: [11, 14, 13],
            direction: [18, 19, 53],
            extent: 15,
        }]
    );
}

#[test]
fn a_loop_without_the_skip_is_not_a_walk() {
    let mut words = walk_words();
    // The comparison no longer tests a lane mask against 0 (s_cmp_eq_u32 s22, 1).
    words[0] = 0xbf06_8116;
    assert!(walks(&bytes(&words)).is_empty());
    // A skip that lands inside the loop.
    let mut words = walk_words();
    words[1] = 0xbf85_0005;
    assert!(walks(&bytes(&words)).is_empty());
}

/// The title's own programs (not in the repository: set `SHADOW_TRACE_DIR` to a folder of
/// their containers, `<name>.<hash>.cdxil`): every hand-found shadow walk
/// ([`super::super::shadow_patch`]) is found with the same skip, comparison, lanes, merge and
/// ray, its head at or after the hand-chosen one; then every program in the folder is listed
/// with the walks found in it.
#[test]
fn the_hand_found_shadow_walks_are_found() {
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
    let code = |path: &std::path::Path| {
        let bytes = std::fs::read(path).expect("the container");
        let c = crate::dxbc::Container::parse(&bytes).expect("a container");
        let part = super::super::program_part(&c).expect("a program part");
        crate::native_programs(part.data)
            .expect("native programs")
            .into_iter()
            .find(|p| p.stage == HwStage::Cs)
            .expect("a compute program")
            .code
    };
    for trace in super::super::shadow_patch::TRACES {
        let Some(path) = files
            .iter()
            .find(|p| p.to_string_lossy().contains(&format!(".{}.", trace.hash)))
        else {
            eprintln!("{}: no container in the folder", trace.hash);
            continue;
        };
        let found = walks(&code(path));
        for w in trace.walks {
            let f = found.iter().find(|f| f.skip == w.skip).unwrap_or_else(|| {
                panic!(
                    "{}: the walk at {:#x} not found: {found:#x?}",
                    trace.hash, w.skip
                )
            });
            assert_eq!(
                (f.cmp, f.past, f.lanes, f.origin, f.direction, f.extent),
                (w.cmp, w.past, w.lanes, w.origin, w.direction, w.extent),
                "{} walk at {:#x}",
                trace.hash,
                w.skip
            );
            assert!(
                w.skip < w.head && w.head <= f.head,
                "{} walk at {:#x}",
                trace.hash,
                w.skip
            );
        }
    }
    for path in &files {
        let found = walks(&code(path));
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let spliced = super::super::shadow_patch::TRACES
            .iter()
            .any(|t| name.contains(&format!(".{}.", t.hash)));
        eprintln!(
            "walk_find: {name}: {} walks{} {:x?}",
            found.len(),
            if spliced { " (spliced by hand)" } else { "" },
            found
                .iter()
                .map(|f| (f.skip, f.head, f.past, f.tests.len()))
                .collect::<Vec<_>>()
        );
    }
}
