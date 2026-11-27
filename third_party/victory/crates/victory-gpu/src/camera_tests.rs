use super::*;

/// A camera turned `yaw` about world up (z), with the given tangents, depth
/// linearisation and jitter, as the title's per-view record bytes.
fn record_bytes(yaw: f32, tan: [f32; 2], lin: [f32; 2], jitter_ndc: [f32; 2]) -> Vec<u8> {
    let (s, c) = yaw.sin_cos();
    let forward = [c, s, 0.0];
    let right = [s, -c, 0.0];
    let up = [0.0, 0.0, 1.0];
    // col2 = forward - jx tx right - jy ty up (so ndc gains +jitter).
    let col0 = scale(right, tan[0]);
    let col1 = scale(up, tan[1]);
    let col2 = sub(
        sub(forward, scale(right, jitter_ndc[0] * tan[0])),
        scale(up, jitter_ndc[1] * tan[1]),
    );
    let mut b = vec![0u8; VIEW_STRIDE];
    let mut put = |at: usize, v: f32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    put(0x08, lin[0]);
    put(0x0c, lin[1]);
    for i in 0..3 {
        put(0x10 + i * 12, col0[i]);
        put(0x14 + i * 12, col1[i]);
        put(0x18 + i * 12, col2[i]);
    }
    put(0x38, 10_000.0);
    put(0x44, 2560.0);
    put(0x48, 1440.0);
    b
}

/// A column-vector matrix as the title's row-vector bytes.
fn title_bytes(m: &Mat4) -> Vec<u8> {
    let mut b = Vec::new();
    for c in 0..4 {
        for row in m {
            b.extend_from_slice(&row[c].to_le_bytes());
        }
    }
    b
}

/// near 0.1, far 10000 in `z = A / (B + 1 - d)`.
fn lin(near: f32, far: f32) -> [f32; 2] {
    // near = A/B (depth 1), far = A/(B+1) (depth 0): both negative.
    [near * far / (near - far), far / (near - far)]
}

fn camera(
    yaw: f32,
    prev_yaw: f32,
    moved: [f32; 3],
    jitter: [f32; 2],
) -> (ViewRecord, Mat4, Derived) {
    let tan = [(0.5_f32).tan() * 16.0 / 9.0, (0.5_f32).tan()];
    let l = lin(0.1, 10_000.0);
    let rec = ViewRecord::parse(&record_bytes(yaw, tan, l, jitter)).unwrap();
    let prev_rec = ViewRecord::parse(&record_bytes(prev_yaw, tan, l, [0.0; 2])).unwrap();
    // The previous camera's view-projection, seen from the current camera's
    // origin: it stood at -moved.
    let p = derive(&prev_rec, &IDENTITY).unwrap();
    let mut view = p.view;
    let t = transform(&view, [moved[0], moved[1], moved[2], 0.0]);
    view[0][3] = t[0];
    view[1][3] = t[1];
    view[2][3] = t[2];
    let prev = mul(&p.proj, &view);
    let prev = matrix_from_title(&title_bytes(&prev)).unwrap();
    let d = derive(&rec, &prev).unwrap();
    (rec, prev, d)
}

const IDENTITY: Mat4 = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

fn close(a: f32, b: f32, eps: f32) -> bool {
    (a - b).abs() <= eps
}

#[test]
fn title_matrix_is_read_in_row_vector_order() {
    let mut bytes = Vec::new();
    for i in 0..16 {
        bytes.extend_from_slice(&(i as f32).to_le_bytes());
    }
    let m = matrix_from_title(&bytes).unwrap();
    // clip.x = m0 x + m4 y + m8 z + m12.
    assert_eq!(m[0], [0.0, 4.0, 8.0, 12.0]);
    assert_eq!(m[3], [3.0, 7.0, 11.0, 15.0]);
    assert!(matrix_from_title(&bytes[..60]).is_none());
}

#[test]
fn view_record_parses_the_words_the_velocity_shaders_read() {
    let b = record_bytes(0.0, [1.0, 0.5], [0.25, 0.5], [0.0; 2]);
    let r = ViewRecord::parse(&b).unwrap();
    assert_eq!(r.lin, [0.25, 0.5]);
    assert_eq!(r.far, 10_000.0);
    assert_eq!(r.viewport, [2560.0, 1440.0]);
    // Right (0, -1, 0) x tan 1 in column 0.
    assert!(close(r.ray[1][0], -1.0, 1e-6));
    assert!(ViewRecord::parse(&b[..0x40]).is_none());
}

#[test]
fn a_still_camera_derives_and_passes_every_check() {
    let (_, prev, d) = camera(0.3, 0.3, [0.0; 3], [0.0; 2]);
    assert!(close(d.fov_y, 1.0, 1e-4), "fov {}", d.fov_y);
    assert!(close(d.aspect, 16.0 / 9.0, 1e-4));
    assert!(close(d.near, 0.1, 1e-4), "near {}", d.near);
    // (f32: `1 + B` keeps few digits; 0.1% here.)
    assert!(close(d.far / 10_000.0, 1.0, 1e-2), "far {}", d.far);
    assert!(close(d.depth_scale, 1.0, 1e-5));
    assert!(
        d.moved.iter().all(|m| m.abs() < 1e-3),
        "moved {:?}",
        d.moved
    );
    // Still: clip to previous clip is the identity.
    for r in 0..4 {
        for c in 0..4 {
            let want = if r == c { 1.0 } else { 0.0 };
            assert!(
                close(d.clip_to_prev_clip[r][c], want, 1e-3),
                "{:?}",
                d.clip_to_prev_clip
            );
        }
    }
    assert_eq!(d.prev_view_proj, prev);
    check(&d, None).unwrap();
}

#[test]
fn a_moving_turning_camera_recovers_its_move() {
    let (_, _, d) = camera(0.30, 0.28, [1.5, -0.25, 0.1], [0.0; 2]);
    check(&d, None).unwrap();
    for (got, want) in d.moved.iter().zip([1.5, -0.25, 0.1]) {
        assert!(close(*got, want, 1e-3), "moved {:?}", d.moved);
    }
}

#[test]
fn reversed_depth_projection_maps_near_to_one_and_far_to_zero() {
    let (_, _, d) = camera(0.0, 0.0, [0.0; 3], [0.0; 2]);
    let at = |z: f32| {
        let p = transform(
            &d.view_proj,
            [z * d.forward[0], z * d.forward[1], z * d.forward[2], 1.0],
        );
        p[2] / p[3]
    };
    assert!(close(at(0.1), 1.0, 1e-4));
    assert!(close(at(10_000.0), 0.0, 1e-4));
}

fn dispatch_for(d: &Derived) -> Dispatch {
    Dispatch {
        fov: d.fov_y,
        near: 10_000.0,
        far: 0.1,
        render: [2560, 1440],
        jitter: [d.jitter_ndc[0] * 1280.0, -d.jitter_ndc[1] * 720.0],
        motion_scale: [1.0, 1.0],
        ..Dispatch::default()
    }
}

#[test]
fn projection_matches_the_dispatch_and_catches_each_mismatch() {
    let (_, _, d) = camera(0.1, 0.1, [0.0; 3], [0.25 / 1280.0, -0.125 / 720.0]);
    let good = dispatch_for(&d);
    check(&d, Some(&good)).unwrap();
    // Jitter given with the other sign convention is accepted.
    let flipped = Dispatch {
        jitter: [-good.jitter[0], -good.jitter[1]],
        ..good
    };
    check_projection(&d, &flipped).unwrap();

    let fov = Dispatch { fov: 1.2, ..good };
    assert!(check_projection(&d, &fov).unwrap_err().starts_with("fov"));
    let planes = Dispatch { near: 0.5, ..good };
    assert!(
        check_projection(&d, &planes)
            .unwrap_err()
            .starts_with("near/far")
    );
    let aspect = Dispatch {
        render: [1920, 1440],
        ..good
    };
    assert!(
        check_projection(&d, &aspect)
            .unwrap_err()
            .starts_with("aspect")
    );
    let jitter = Dispatch {
        jitter: [0.4, 0.1],
        ..good
    };
    assert!(
        check_projection(&d, &jitter)
            .unwrap_err()
            .starts_with("jitter")
    );
}

#[test]
fn an_infinite_far_plane_matches_a_huge_dispatch_far() {
    let tan = [0.8, 0.45];
    // B = -1: far at infinity.
    let rec = ViewRecord::parse(&record_bytes(0.0, tan, [-0.1, -1.0], [0.0; 2])).unwrap();
    let d = derive(&rec, &IDENTITY).unwrap();
    assert!(close(d.near, 0.1, 1e-6));
    assert!(d.far.is_infinite());
    assert!(near_rel(d.far, 3.4e38, 0.01));
    assert!(!near_rel(d.far, 1000.0, 0.01));
}

#[test]
fn a_skewed_view_or_bad_product_is_reported() {
    let mut m = IDENTITY;
    m[0] = [0.8, 0.6, 0.0, 0.0];
    assert!(check_orthonormal(&m, 1e-3).unwrap_err().contains("dot"));
    m = IDENTITY;
    m[2][2] = 1.2;
    assert!(check_orthonormal(&m, 1e-3).unwrap_err().contains("length"));
    let (_, _, d) = camera(0.2, 0.2, [0.0; 3], [0.0; 2]);
    let mut vp = d.view_proj;
    vp[1][2] += 0.5;
    assert!(check_product(&vp, &d.proj, &d.view, 1e-4).is_err());
}

#[test]
fn a_previous_matrix_that_is_not_a_rigid_view_fails() {
    let (rec, mut prev, _) = camera(0.2, 0.2, [0.0; 3], [0.0; 2]);
    prev[0][0] *= 1.5;
    let d = derive(&rec, &prev).unwrap();
    assert!(check(&d, None).unwrap_err().starts_with("previous"));
}

#[test]
fn motion_vectors_agree_with_clip_to_previous_clip() {
    let (_, _, d) = camera(0.30, 0.29, [0.5, 0.2, 0.0], [0.0; 2]);
    let render = [2560u32, 1440];
    let mut samples = Vec::new();
    for (i, &(x, y, depth)) in [
        (100.0, 200.0, 0.001),
        (1280.0, 720.0, 0.01),
        (2000.0, 1300.0, 0.5),
    ]
    .iter()
    .enumerate()
    {
        let ndc = [x / 2560.0 * 2.0 - 1.0, 1.0 - y / 1440.0 * 2.0];
        let p = transform(&d.clip_to_prev_clip, [ndc[0], ndc[1], depth, 1.0]);
        let prev = [
            (p[0] / p[3] * 0.5 + 0.5) * 2560.0,
            (0.5 - p[1] / p[3] * 0.5) * 1440.0,
        ];
        // The title's vectors here are in UV (scale = render size).
        let mut motion = [(prev[0] - x) / 2560.0, (prev[1] - y) / 1440.0];
        if i == 2 {
            // A moving car: its own motion on top.
            motion[0] += 0.01;
        }
        samples.push(MotionSample {
            pixel: [x, y],
            depth,
            motion,
        });
    }
    let share = motion_agreement(
        &d.clip_to_prev_clip,
        &samples,
        render,
        [2560.0, 1440.0],
        0.5,
    );
    assert!(close(share, 2.0 / 3.0, 1e-6), "share {share}");
    // A still matrix for a moving camera: nothing matches.
    let none = motion_agreement(&IDENTITY, &samples[..2], render, [2560.0, 1440.0], 0.5);
    assert!(none < 0.5, "{none}");
    assert_eq!(
        motion_agreement(&IDENTITY, &[], render, [1.0, 1.0], 0.5),
        0.0
    );
}

#[test]
fn finish_reads_noted_addresses_and_publishes() {
    let (_, prev, _) = camera(0.4, 0.4, [0.0; 3], [0.0; 2]);
    let rec = record_bytes(
        0.4,
        [(0.5_f32).tan() * 16.0 / 9.0, (0.5_f32).tan()],
        lin(0.1, 10_000.0),
        [0.0; 2],
    );
    let mut cb = vec![0u8; 0x200];
    cb[0x1a0..0x1e0].copy_from_slice(&title_bytes(&prev));
    let (rec_at, cb_at) = (0x1000_u64, 0x8000_u64);
    let read = |at: u64, out: &mut [u8]| {
        let (base, src) = if at >= cb_at {
            (cb_at, &cb)
        } else {
            (rec_at, &rec)
        };
        let o = (at - base) as usize;
        out.copy_from_slice(&src[o..o + out.len()]);
        true
    };
    let none = finish(read);
    assert!(!none.valid);
    assert_eq!(none.why, "no velocity pass this frame");

    note_view((rec_at, 288), (cb_at, 0x200));
    note_ui(Ui {
        texture: 7,
        target: 9,
    });
    let motion = Surface {
        title: 4,
        format: 34,
        size: [2560, 1440],
    };
    note_velocity_targets(None, Some(motion));
    note_velocity_targets(
        Some(Surface {
            title: 3,
            format: 20,
            size: [2560, 1440],
        }),
        None,
    );
    note_hudless(Surface {
        title: 0,
        format: 24,
        size: [3840, 2160],
    });
    assert!(hudless_noted());
    frame_end(read);
    assert!(!hudless_noted());
    let cam = latest().unwrap();
    assert!(cam.valid, "{}", cam.why);
    assert_eq!(cam.sources, [rec_at, cb_at]);
    assert_eq!(cam.ui.unwrap().texture, 7);
    assert_eq!(cam.motion, Some(motion));
    assert_eq!(cam.depth.unwrap().title, 3);
    assert_eq!(cam.hudless.unwrap().size, [3840, 2160]);
    assert_eq!(cam.mv_scale, [1.0 / 2560.0, 1.0 / 1440.0]);
    assert!(cam.frame > none.frame);

    note_view((rec_at, 288), (cb_at, 0x100));
    assert!(finish(read).why.starts_with("short views"));
}

#[test]
fn frame_scales_turn_pixels_into_uv_and_jitter_into_pixels() {
    let (_, _, d) = camera(0.3, 0.3, [0.0; 3], [0.002, -0.004]);
    let (scale, jitter) = frame_scales(Some(&d), [2000, 1000]);
    assert_eq!(scale, [1.0 / 2000.0, 1.0 / 1000.0]);
    assert!(close(jitter[0], 2.0, 1e-3) && close(jitter[1], 2.0, 1e-3), "{jitter:?}");
    assert_eq!(frame_scales(None, [0, 0]), ([0.0; 2], [0.0; 2]));
}
