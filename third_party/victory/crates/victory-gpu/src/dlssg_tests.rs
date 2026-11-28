use super::*;
use crate::camera::{ViewRecord, derive, mul};
use windows::Win32::Graphics::Dxgi::Common::*;

const RENDER: [u32; 2] = [1280, 720];

fn upscaled() -> Upscaled {
    Upscaled {
        render: RENDER,
        jitter: [0.25, -0.5],
        motion_scale: [-1280.0, 720.0],
        near: 1000.0,
        far: 0.1,
        reset: false,
    }
}

/// A camera looking down +z with a jittered projection, standing still.
fn still_camera() -> FrameCamera {
    let record = ViewRecord {
        lin: [0.1, 1.0e-5],
        ray: [[0.8, 0.0, 0.01], [0.0, 0.45, -0.02], [0.0, 0.0, 1.0]],
        far: 10_000.0,
        viewport: [1280.0, 720.0],
    };
    let first = derive(&record, &sl::IDENTITY).expect("a camera");
    let mut proj = first.proj;
    proj[0][2] = 0.0;
    proj[1][2] = 0.0;
    let prev = mul(&proj, &first.view);
    FrameCamera {
        valid: true,
        camera: derive(&record, &prev),
        ..FrameCamera::default()
    }
}

#[test]
fn constants_need_a_valid_camera_and_an_upscale() {
    assert!(
        constants(None, Some(&upscaled()))
            .unwrap_err()
            .contains("VICTORY_CAMERA_CAPTURE")
    );
    let mut cam = still_camera();
    assert!(
        constants(Some(&cam), None)
            .unwrap_err()
            .contains("no upscale")
    );
    cam.valid = false;
    cam.why = "fov mismatch".into();
    assert!(
        constants(Some(&cam), Some(&upscaled()))
            .unwrap_err()
            .contains("fov mismatch")
    );
    let mut up = upscaled();
    up.render = [0, 0];
    assert!(constants(Some(&still_camera()), Some(&up)).is_err());
}

#[test]
fn constants_from_the_camera() {
    let cam = still_camera();
    let d = cam.camera.unwrap();
    let c = constants(Some(&cam), Some(&upscaled())).expect("constants");
    // Row vectors: Streamline's [r][c] is our [c][r]; no jitter in the projection.
    assert_eq!(c.camera_view_to_clip[2][3], d.proj[3][2]);
    assert_eq!(c.camera_view_to_clip[3][2], d.proj[2][3]);
    assert_eq!(c.camera_view_to_clip[2][0], 0.0);
    assert_eq!(c.camera_view_to_clip[2][1], 0.0);
    assert_eq!(c.camera_view_to_clip[0][0], d.proj[0][0]);
    // A camera standing still: clip to previous clip is the identity.
    for r in 0..4 {
        for col in 0..4 {
            let want = if r == col { 1.0 } else { 0.0 };
            assert!(
                (c.clip_to_prev_clip[r][col] - want).abs() < 1e-4,
                "{r},{col}"
            );
            assert!(
                (c.prev_clip_to_clip[r][col] - want).abs() < 1e-4,
                "{r},{col}"
            );
        }
    }
    // clip-to-view is the inverse of view-to-clip.
    let p: Mat4 = transpose(&c.camera_view_to_clip);
    let pi: Mat4 = transpose(&c.clip_to_camera_view);
    let id = mul(&p, &pi);
    for (r, row) in id.iter().enumerate() {
        for (col, v) in row.iter().enumerate() {
            assert!((v - if r == col { 1.0 } else { 0.0 }).abs() < 1e-3);
        }
    }
    assert_eq!(c.mvec_scale, [-1.0, 1.0]);
    assert_eq!(c.jitter_offset, [0.25, -0.5]);
    assert_eq!((c.camera_near, c.camera_far), (0.1, 1000.0));
    assert_eq!(c.depth_inverted, sl::TRUE);
    assert_eq!(c.camera_motion_included, sl::TRUE);
    assert_eq!(c.motion_vectors_3d, sl::FALSE);
    assert_eq!(c.reset, sl::FALSE);
    assert_eq!(c.camera_pos, [0.0; 3]);
    assert_eq!(c.camera_fwd, [0.0, 0.0, 1.0]);
    assert!((c.camera_fov - d.fov_y).abs() < 1e-6);
}

/// Without `VICTORY_UPSCALER=7` (no upscale dispatch), the camera's velocity
/// pass gives the constants: its mv scale (UV per pixel) and jitter, and its
/// reversed depth read as inverted.
#[test]
fn constants_without_an_upscale_come_from_the_camera() {
    let mut cam = still_camera();
    cam.mv_scale = [1.0 / RENDER[0] as f32, 1.0 / RENDER[1] as f32];
    cam.jitter_px = [0.125, -0.25];
    assert!(constants(Some(&cam), None).is_err(), "nothing tagged: paused");
    let up = from_camera(&cam, RENDER).expect("upscale values from the camera");
    assert_eq!(up.render, RENDER);
    assert!(up.near > up.far, "reversed depth");
    let c = constants(Some(&cam), Some(&up)).expect("constants");
    assert_eq!(c.mvec_scale, cam.mv_scale);
    assert_eq!(c.jitter_offset, [0.125, -0.25]);
    assert_eq!(c.depth_inverted, sl::TRUE);
    assert!(c.camera_near > 0.0 && c.camera_near < c.camera_far);
    // A camera without matrices gives nothing.
    assert!(from_camera(&FrameCamera::default(), RENDER).is_none());
}

/// Camera r3 on Series X (no upscale hook): `PS_VelocityFill` noted motion
/// (RT0, R16G16_FLOAT) and depth (DSV, R32G8X24_TYPELESS) at 2560x1440, the
/// frame record's scales come from that motion size (`camera::finish`), and
/// the first Scaleform draw after `PS_TAA2_SpatialUpscale` tagged them at the
/// motion texture's size. The present's constants are this camera's, with
/// motion vectors in UV per pixel of that size.
#[test]
fn a_series_x_velocity_fill_frame_gives_constants_end_to_end() {
    const SERIES_X: [u32; 2] = [2560, 1440];
    let mut cam = still_camera();
    let d = cam.camera.unwrap();
    cam.motion = Some(crate::camera::Surface {
        title: 0x2c0c_6f94_ec0,
        format: DXGI_FORMAT_R16G16_FLOAT.0,
        size: SERIES_X,
    });
    cam.depth = Some(crate::camera::Surface {
        title: 0x2c0c_6f94_740,
        format: DXGI_FORMAT_R32G8X24_TYPELESS.0,
        size: SERIES_X,
    });
    (cam.mv_scale, cam.jitter_px) = crate::camera::frame_scales(Some(&d), cam.motion.unwrap().size);
    let up = frame_upscale(false, Some(SERIES_X), None, Some(&cam)).expect("the camera's values");
    let c = constants(Some(&cam), Some(&up)).expect("constants");
    assert_eq!(c.mvec_scale, [1.0 / 2560.0, 1.0 / 1440.0]);
    assert_eq!(c.jitter_offset, cam.jitter_px);
    assert_eq!(c.depth_inverted, sl::TRUE);
    assert!(c.camera_near > 0.0 && c.camera_near < c.camera_far);
    assert!((c.camera_fov - d.fov_y).abs() < 1e-6);
    // The record's jitter, in pixels of the tagged size (y down).
    let want = [d.jitter_ndc[0] * 1280.0, -d.jitter_ndc[1] * 720.0];
    assert!((cam.jitter_px[0] - want[0]).abs() < 1e-4 && (cam.jitter_px[1] - want[1]).abs() < 1e-4);
}

/// Which values the present takes: the upscale's when it tagged this frame,
/// the camera's when only the camera did, none when nothing did (not an
/// earlier frame's upscale: Streamline would have no tags for this frame).
#[test]
fn the_constants_follow_what_tagged_this_frame() {
    let mut cam = still_camera();
    cam.mv_scale = [1.0 / RENDER[0] as f32, 1.0 / RENDER[1] as f32];
    let earlier = upscaled();
    assert_eq!(frame_upscale(true, None, Some(&earlier), Some(&cam)), Some(earlier));
    assert_eq!(
        frame_upscale(true, Some(RENDER), Some(&earlier), Some(&cam)),
        Some(earlier),
        "the upscale wins"
    );
    let from_cam = frame_upscale(false, Some(RENDER), Some(&earlier), Some(&cam)).unwrap();
    assert_eq!(from_cam, from_camera(&cam, RENDER).unwrap());
    assert_eq!(frame_upscale(false, None, Some(&earlier), Some(&cam)), None);
    assert_eq!(frame_upscale(false, Some(RENDER), None, None), None);
    let why = constants(Some(&cam), None).unwrap_err();
    assert!(why.contains("tagged this frame"), "{why}");
}

/// `VICTORY_DLSSG_EVERY_FRAME=1`: a present whose frame had no velocity pass
/// (it fell into the previous frame) still gets constants, from the last
/// valid camera, for the depth and motion the scene pass tagged; batch 5
/// part C turned DLSS-G off there (`camera not valid: no velocity pass`).
#[test]
fn a_frame_without_a_velocity_pass_keeps_generating_on_the_held_camera() {
    let mut good = still_camera();
    good.frame = 41;
    good.mv_scale = [1.0 / RENDER[0] as f32, 1.0 / RENDER[1] as f32];
    let missed = FrameCamera {
        frame: 42,
        why: "no velocity pass this frame".into(),
        ..FrameCamera::default()
    };
    let mut hold = crate::camera_tag::CameraHold::default();
    hold.pick(Some(std::sync::Arc::new(good)), crate::camera_tag::HOLD_FRAMES);
    let latest = Some(std::sync::Arc::new(missed));
    let (cam, held) = hold.pick(latest.clone(), crate::camera_tag::HOLD_FRAMES);
    assert!(held);
    let up = frame_upscale(false, Some(RENDER), None, cam.as_deref()).expect("the held camera's values");
    let c = constants(cam.as_deref(), Some(&up)).expect("constants");
    assert_eq!(c.mvec_scale, [1.0 / RENDER[0] as f32, 1.0 / RENDER[1] as f32]);
    // Switch off (hold 0): round 3, off for that present.
    let (cam, held) = crate::camera_tag::CameraHold::default().pick(latest, 0);
    assert!(!held);
    let why = constants(cam.as_deref(), Some(&up)).unwrap_err();
    assert!(why.contains("no velocity pass"), "{why}");
}

#[test]
fn no_hudless_without_a_copy_of_this_frame() {
    assert!(hudless_for(None).is_none());
    // No VICTORY_CAMERA_HUDLESS in tests, and this camera noted no copy.
    assert!(hudless_for(Some(&still_camera())).is_none());
}

#[test]
fn transpose_swaps_rows_and_columns() {
    let mut m = [[0.0; 4]; 4];
    m[0][3] = 7.0;
    m[2][1] = -3.0;
    let t = transpose(&m);
    assert_eq!(t[3][0], 7.0);
    assert_eq!(t[1][2], -3.0);
    assert_eq!(t[0][3], 0.0);
}

#[test]
fn without_the_proxy_everything_is_a_no_op() {
    assert!(!active());
    before_present();
    after_present();
    assert!(SESSION.lock().unwrap().is_none());
    assert_eq!(frames_to_generate().clamp(1, 3), frames_to_generate());
}

/// The microbenchmark (not the game; take the `gpu` lock): Streamline from
/// `VICTORY_STREAMLINE_DIR`, DLSS-G support on this adapter, the proxy swap
/// chain on a test window, then 6 s of presents with tagged depth and motion
/// vectors and a still camera, and the frames DLSS-G showed for the last one
/// (2: one generated).
/// `cargo test -p victory-gpu streamline_probe -- --ignored --nocapture`
#[test]
#[ignore = "needs an RTX card, the Streamline DLLs (VICTORY_STREAMLINE_DIR) and the gpu lock; run by hand"]
fn streamline_probe() {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::w;

    let api = sl::api().expect("Streamline");
    println!("interposer: {}", api.path.display());
    // SAFETY: plain D3D12/DXGI object creation and Streamline calls with live objects.
    unsafe {
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).unwrap();
        let device = device.unwrap();
        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .unwrap();
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("victory streamline probe"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            100,
            100,
            RENDER[0] as i32,
            RENDER[1] as i32,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let _ = SetForegroundWindow(hwnd);
        let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: RENDER[0],
            Height: RENDER[1],
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 3,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            ..Default::default()
        };
        let started = Instant::now();
        let chain = start(api, &factory, &device, &queue, hwnd, &desc)
            .unwrap_or_else(|why| panic!("DLSS-G: {why} (Streamline's log: %TEMP%/victory-streamline)"));
        println!("proxy swap chain made in {:?}", started.elapsed());
        let texture = |format: DXGI_FORMAT| {
            let mut r: Option<ID3D12Resource> = None;
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                        Width: u64::from(RENDER[0]),
                        Height: RENDER[1],
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        Format: format,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        ..Default::default()
                    },
                    D3D12_RESOURCE_STATES(SHADER_READ as i32),
                    None,
                    &mut r,
                )
                .unwrap();
            r.unwrap()
        };
        let (depth, motion) = (
            texture(DXGI_FORMAT_R32_FLOAT),
            texture(DXGI_FORMAT_R16G16_FLOAT),
        );
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        list.Close().unwrap();
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        let cam = std::sync::Arc::new(still_camera());
        let mut failed_presents = 0;
        let started = Instant::now();
        let mut presents = 0u64;
        for i in 1u64.. {
            // DLSS-G engages after NGX makes its feature (1-2 s): run past that.
            if i > 300 && started.elapsed() > Duration::from_secs(6) {
                break;
            }
            presents = i;
            allocator.Reset().unwrap();
            list.Reset(&allocator, None).unwrap();
            let _ = chain.GetCurrentBackBufferIndex();
            tag_inputs(
                &list,
                &crate::framegen::Inputs {
                    depth: Some(&depth),
                    motion: Some(&motion),
                    render: RENDER,
                    max_render: RENDER,
                    jitter: [0.0, 0.0],
                    motion_scale: [RENDER[0] as f32, RENDER[1] as f32],
                    frame_ms: 16.7,
                    reset: i == 1,
                    near: 1000.0,
                    far: 0.1,
                    fov_y: 1.0,
                    metres_per_unit: 1.0,
                },
            );
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            before_present_with(Some(cam.clone()));
            if chain.Present(0, DXGI_PRESENT(0)).is_err() {
                failed_presents += 1;
            }
            after_present();
            queue.Signal(&fence, i).unwrap();
            while fence.GetCompletedValue() < i.saturating_sub(2) {
                std::thread::yield_now();
            }
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let secs = started.elapsed().as_secs_f64();
        let guard = SESSION.lock().unwrap();
        let s = guard.as_ref().unwrap();
        let mut state = sl::DlssgState::new();
        let r = (s.get_state)(&viewport(), &mut state, std::ptr::null());
        println!(
            "{presents} presents in {secs:.2} s ({:.0}/s), {failed_presents} failed; slDLSSGGetState {}: frames shown for the last present {}, status {}, max generated {}, vram {} MB, vsync available {}",
            presents as f64 / secs,
            sl::result_name(r),
            state.num_frames_actually_presented,
            sl::dlssg_status(state.status),
            state.num_frames_to_generate_max,
            state.estimated_vram_usage_in_bytes >> 20,
            state.is_vsync_support_available
        );
        println!(
            "device removed reason: {:?}",
            device.GetDeviceRemovedReason()
        );
        assert_eq!(failed_presents, 0);
        assert_eq!(r, sl::OK);
        // DLSS-G interpolates only while its window has the focus.
        let focused = GetForegroundWindow() == hwnd;
        println!("window focused at the end: {focused}");
        if focused {
            assert_eq!(state.num_frames_actually_presented, 2, "DLSS-G did not generate");
        }
        assert!(device.GetDeviceRemovedReason().is_ok());
    }
}

