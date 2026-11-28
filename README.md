<div align="center">

![El1xr: Grand Theft Auto VI for Xbox Series, running on a Windows PC. 21.6-26.0 fps at 4K on an RTX 5090, 1 refused pipeline, 24.6 s from launch to the world, playable as of 28 November 2026](docs/images/hero.svg)

![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-9b5cff?style=flat-square&labelColor=170f2b)
![Platform: Windows 10 and 11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2de2e6?style=flat-square&labelColor=170f2b)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-ff8a3d?style=flat-square&labelColor=170f2b&logo=rust&logoColor=white)
![Status: playable](https://img.shields.io/badge/status-playable-ff3d8b?style=flat-square&labelColor=170f2b)
![21.6-26.0 fps at 4K on RTX 5090](https://img.shields.io/badge/4K%20on%20RTX%205090-21.6--26.0%20fps-ffd36e?style=flat-square&labelColor=170f2b)
![Discord: bonemeld](https://img.shields.io/badge/Discord-bonemeld-5865F2?style=flat-square&labelColor=170f2b&logo=discord&logoColor=white)

**English** · [Français](docs/i18n/README.fr.md) · [Español](docs/i18n/README.es.md) · [Deutsch](docs/i18n/README.de.md) · [Italiano](docs/i18n/README.it.md) · [Português](docs/i18n/README.pt-BR.md) · [日本語](docs/i18n/README.ja.md) · [中文](docs/i18n/README.zh-CN.md) · [Русский](docs/i18n/README.ru.md)

</div>

El1xr runs the Xbox Series version of Grand Theft Auto VI (build 1.0.2.1) on a Windows PC. It is Bone/Nevaa's project. The game's own x86-64 code runs natively on your processor. Its console system calls, Direct3D 12.X commands and RDNA 2 shaders are translated for the PC as the game runs. There is no CPU emulation and no game content in this repository: you bring your own copy.

## Status

> [!NOTE]
> **As of 28 November 2026, GTA VI (Xbox Series, build 1.0.2.1) boots, reaches the menus and is playable in the open world on Windows.** In the latest quiet test runs (batch 5, 4K, RTX 5090) the game runs at 21.6-22.4 fps in 110-second runs from launch and at 25.8-26.0 fps in 180- and 460-second runs. That is still short of the next goal, a steady 30 fps. After that come no hitches and no visual faults.

![Milestones: boots, main menu and in-game by 24 November 2026; playable on 27 November 2026, now 21.6-26.0 fps; 30 fps next; stable in progress](docs/images/milestones.svg)

| Milestone | Status | Evidence | Date |
|---|---|---|---|
| Executable runs | Done | The Xbox Series 1.0.2.1 executable starts on Windows. Its x86-64 code runs natively. | by 24 Nov 2026 |
| First frame | Done | The startup animation and legal screens render without flicker. | by 24 Nov 2026 |
| Main menu | Done | Profile screen, first-run settings, legal screens, loading screen and pause menu all work. Menus run at 30 fps (RTX 4070 SUPER). | by 24 Nov 2026 |
| In-game | Done | The game reaches the open world and the character moves under player control. A 13-minute session in the prison ran at about 4 fps (RTX 4070 SUPER). | by 24 Nov 2026 |
| Playable | Done, below target speed | The game plays at 21.6-26.0 fps at 4K (RTX 5090; quiet batch 5 runs of 110 to 460 seconds). | 28 Nov 2026 |
| Story progress | First two missions completed | A contributor completed the first two missions in a 45-minute session at about 2 fps (RTX 4070 SUPER; reported, not measured from logs). All cutscenes play in full. On Linux (Proton, RX 9070 XT), a 23-minute run got past the prologue into chapter 1. | 25 Nov 2026 |
| 30 fps | Not yet | 21.6-26.0 fps, or 38-46 ms a frame, against the 33.3 ms target (RTX 5090, 4K). | 28 Nov 2026 |
| Stable | In progress | With the default settings, batch 5's test runs had no GPU driver reset and no device loss. The 8-9 s stall when entering the world, seen in 5 of 5 runs early in batch 5, did not come back in the 47 launches checked after raw indirect records began to be checked. The game can still crash when Windows runs out of memory commit, and visual faults still vary from run to run (see Known issues). | 28 Nov 2026 |

## Performance

![At a glance: 21.6-26.0 fps, 24.6 s from launch to the first world frame (was 26.7 s), 0 one-frame dark flashes in the diner (was 12-19 per run), 2.9 GB of GPU buffer memory after 180 s (was 4.5 GB)](docs/images/stats.svg)

| Measure (RTX 5090, 4K) | Now | Goal |
|---|---|---|
| Frame rate, 110 s runs from launch | **21.6-22.4 fps** | 30+ fps |
| Frame rate, 180 s and 460 s runs | **25.8-26.0 fps** | 30+ fps |
| CPU time to prepare a frame | 15.7-21.2 ms | under the GPU's time |
| Refused GPU pipelines | 1 | 0 |
| Launch to first world frame | 23.1-24.6 s | |
| Stall when entering the world | none in 47 launches; worst frame of a run 771-832 ms | no hitches |
| Frames over 250 ms in the world | 3-6 per 110 s run | none |
| One-frame dark flashes in the diner | 0 (was 12-19 per run) | none |
| GPU buffer memory after 180 s | 2.9 GB (was 4.5 GB) | |

These figures come from batch 5 (finished 28 November 2026), with the switches the launcher now turns on. Every run was quiet: other work on the machine was paused or held back, the game fast-booted from a fixed checkpoint, and the numbers were read from the emulator's own frame log (frame rate and CPU time are medians over the second half of a run's frames in the world). The 110-second figures come from two back-to-back loading pairs, the 180-second figure from the `CARD_BLOCK_REUSE` pair, and the 460-second figure from a CPU test launch. Frame rate depends on the scene and on the length of the run, so compare figures only within a pair. Some diagnostic logging was on in every run.

![Where a frame goes: game passes 22.8 ms, El1xr GPU work 7.0 ms, untimed 3.8 ms, GPU idle 6.2 ms, against the 33.3 ms line for 30 fps](docs/images/frame-budget.svg)

The last GPU timing profile, taken before batch 3, put a settled frame at about 40 ms, of which the game's own rendering passes need 22.8 ms. Batch 5 showed that the frame is now limited by the CPU work that prepares and sends it, not by the GPU: with DLSS rendering at two thirds of 4K, the real frame rate stayed at 21.8-22.0 fps, the same as at native 4K (20.1-22.2 fps). Most of batch 5's kept changes therefore cut CPU time per frame.

![Batch 5: with the loading bundle, launch to the first world frame went from 26.2-26.7 s to 23.1-24.6 s; with TABLE_NULL_ONCE, one-frame dark flashes in the diner went from 12-19 per run to 0; with HEAP_WARM, the first world frame took 614-702 ms instead of 781-1048 ms](docs/images/load-times.svg)

Batch 5 kept these changes, now on by default in the launcher. The switches of batches 3 and 4 stay on.

- **Faster loading** (`VICTORY_TEXTURE_HEAPS=1`, `VICTORY_TEXTURE_BLOCK_THREAD=1`, `VICTORY_DSTORAGE_STAGING_MB=256`, `VICTORY_DSTORAGE_POLL_WAIT=1`, `VICTORY_BAKE_STAGE_ALL=1`): streamed textures share large memory blocks made on a thread of their own, up to 256 MiB of DirectStorage reads are in flight instead of 32, the game's poll for finished disk reads returns as soon as they finish, and bake passes are staged together while loading. In the confirming quiet pair, launch to the first world frame went from 26.7 to 24.6 s, launch to the first full world frame from 29.2 to 26.2 s, and the worst frame from 1049 to 832 ms, at the same frame rate. The first pair agreed (26.2 to 23.1 s).
- **Diner flicker fixed** (`VICTORY_TABLE_NULL_ONCE=1`): in the diner, the background went dark for one frame about every 2 s, 12-19 times per run. A descriptor table emptied a texture's slot each time that texture's prepare work was pruned; now a slot is emptied at most once. With the switch on, no such frame came in a 110-second run or in two 18-second test segments, at no measured cost.
- **No more stall at world entry** (`VICTORY_RAW_CHECK=2`): indirect draw records written by the game's shaders are checked before they run. Early in batch 5 every run froze for 8.2-9.0 s about 4 s after entering the world (5 of 5); none of the 47 launches checked after this change stalled.
- **Less CPU time per frame**:
  - `VICTORY_SKIP_WC_BACK=1`: large shader writes to write-combining memory stay on the card instead of being copied back. 24.6 fps in 2 of 2 runs, against 21.5 fps in the best quiet run without it, and 7.3 ms less CPU time per frame.
  - `VICTORY_CARD_BLOCK_REUSE=1`: GPU memory pieces are zeroed and reused instead of allocated again. 23.6 to 26.0 fps in a quiet 180-second pair, with 2.9 GB of GPU buffer memory at the end instead of 4.5 GB.
  - `VICTORY_SEND_THREAD=1` and `VICTORY_CARD_RECORD_SEND=1`: command lists are sent, and GPU sync lists recorded, on a thread of their own. Sending time on the frame thread went from 1.56 to 0.11 ms.
  - `VICTORY_FLUSH_BESIDE=1`, `VICTORY_RESET_ASIDE=1`, `VICTORY_COPIED_SHARED=1`, `VICTORY_TEXTURE_HASH_BEHIND=1`, `VICTORY_RESULT_RING=1`, and `VICTORY_WIDE_WAKE_ONE=1` with `VICTORY_WAIT_SPIN=50`: smaller cuts of 0.5-1.8 ms of CPU time per frame each, or up to 1.4 fps, measured by turning each one on and off within a single run.
- **Faster world entry** (`VICTORY_HEAP_WARM=1`): the host copies of the game's view heaps are made on the loading screen, so the first world frame took 614 ms instead of 1048 ms (second pair; first pair 781 to 702 ms).
- **Fewer slow frames** (`VICTORY_AMBIENT_ONCE=1`): the stand-in for the game's ambient lighting counts one bounce. The 99th-percentile frame time was 11 ms lower in a same-run test.

Upscaling at 4K no longer crashes: three upscaling launches ran with no divide by zero (0xc0000094). DLSS upscaling looks sharp but adds no frame rate, because the frame waits on the CPU. DLSS frame generation (`-FrameGen`, or Frame generation = dlss in the F9 menu) presents about 44-47 fps from 22-24 real frames; it stays off by default until generated frames have been checked by eye. Batch 5 also dropped switches that broke something or cost speed, among them `PIPELINE_PIECES` (black screen), `BAKE_SPREAD=12` (1.2-2.2 fps slower) and `GUARD_PUBLISH_TRY` (a crash). The game's ray-tracing builder works when turned on but runs at 0.6-1.1 fps, so it stays off.

### Test machine

| Part | Model |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, driver 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 cores / 32 threads |
| Memory | 64 GB DDR5-6000 |
| Page file | 64 GB (commit limit 142 GB) |
| Motherboard | MSI PRO B650-P WIFI |
| Storage | NVMe SSDs (Samsung 990 EVO Plus) |
| Operating system | Windows 11 (build 26200) |
| Game | GTA VI for Xbox Series, build 1.0.2.1 (your own copy) |

All current figures come from this machine. Older figures in this README were measured on an RTX 4070 SUPER (Windows) or an RX 9070 XT (Linux), and each one says so.

## How it works

![How it works: the game's x86-64 code runs natively on the CPU; console system calls go to the HLE kernel on Windows; D3D12.X commands are rebuilt as host Direct3D 12; RDNA 2 shaders are translated to DXIL and cached](docs/images/how-it-works.svg)

- **Game code** is loaded and run as is. The Xbox Series and a Windows PC share the x86-64 instruction set, so no instruction is emulated.
- **System calls** to the console kernel and GDK libraries are served by El1xr's high-level emulation (HLE) on top of Windows: memory, threads, files and system services.
- **Disk reads**: the game's DirectStorage requests are served by El1xr's own worker threads, with up to 256 MiB in flight. Streamed textures are placed in shared memory blocks: about 115,000 textures in 116 blocks in a typical run.
- **Graphics**: the console's Direct3D 12.X command lists, memory and resources are rebuilt as standard Direct3D 12 on your GPU. Command lists are sent from a thread of their own, so the game's frame thread does less of that work.
- **Descriptor heaps**: the game addresses up to 1,245,184 descriptor slots (its table of texture and buffer views). El1xr mirrors them into one host heap of about 1,000,000 slots, shared with its own views, and makes the host copies on the loading screen. Fitting the rest is still open (see Known issues).
- **Shaders** compiled for the console's RDNA 2 GPU are translated to DXIL. All but 6 of the game's 26,148 shader programs translate, and each translation is cached on disk. `dev.py prewarm` builds the game's 25,751 GPU pipelines ahead of a run, so they are not compiled during play.

## Run it

You need your own copy of the game, Windows 10 or 11, a GPU with Shader Model 6.6 and mesh shaders, and Rust.

```powershell
tools\lancer_victory.ps1 -GameDir <game folder>
```

The full guide (install, settings, troubleshooting) is in [docs/running-gta6.md](docs/running-gta6.md).

> [!IMPORTANT]
> This repository contains no game files, keys or executables, and does not explain how to get them.

<details>
<summary>Developer commands</summary>

```powershell
python tools/dev.py build      # release build
python tools/dev.py prewarm    # translate all shaders offline (about 45 s)
python tools/dev.py bench --quiet --seconds 200   # measured game run
python tools/dev.py gate       # every test, fails on any warning
```

</details>

## Known issues

- **The prison window sill goes dark after the prologue.** In the cell shot after the PROLOGUE title, the sunlit sill turns dark and loses its detail from one frame to the next, with the same camera. Reproduced twice in batch 5; the cause is under investigation.
- **Skin can turn black.** In some runs a character's skin, most often in the diner, turns solid black for a few seconds and then comes back ([#35](https://github.com/kylepelham/El1xr/issues/35)). The waitress's shirt also changes colour from run to run.
- **Dark squares over a window.** In the opening cutscene, about 44 s in, stair-stepped dark squares cover the window behind Jason. Batch 5 ruled out two suspected causes; the search goes on.
- **Descriptor heap limit.** The game addresses up to 1,245,184 descriptor slots, but the host heap on the RTX 5090 holds about 1,000,000. In batch 5 runs, 313,000-504,000 of the game's live views sat past the part the host heap can hold. No way of fitting them has been kept yet.
- **The phone screen is black** in the diner's table shot.
- **High memory use.** The game process peaks at 33-35 GB. With too small a page file, it crashes when Windows runs out of memory commit. The test PC runs with a 64 GB page file.
- **Slow streaming.** After entering the world it takes about 72 s for every asset to arrive, with pop-in meanwhile.
- **The frame waits on the CPU.** Preparing and sending a frame takes 15.7-21.2 ms of CPU time, and upscaling gives no frame rate for that reason.
- **Ray-traced lighting is off.** The game builds its ray-tracing structures in software; that builder works but runs at 0.6-1.1 fps, so it stays off.
- **One reference machine.** Current figures are from the RTX 5090 above. Other GPUs have not been measured with the current build.

## What's next

The target is a steady 30 fps at 4K on the RTX 5090, then no hitches and no visual faults. Each new change is off by default until quiet test runs prove it.

- **Less CPU time per frame**: the frame is CPU-bound, so the next batches keep moving submission and texture-check work off the frame thread.
- **The window sill and the window squares**, and **skin that turns black** ([#35](https://github.com/kylepelham/El1xr/issues/35)).
- **The descriptor heap**: fitting the game's 1,245,184 slots into the host heap.
- **Hardware ray tracing on the RT cores**, which would bring back ray-traced lighting.
- **DLSS frame generation by default**, once its generated frames have been checked by eye.

## In numbers

| Figure | What |
|---|---|
| 4000+ tests passing | Rust crates (`cargo test`) and Python tools (`pytest`), run by `dev.py gate` |
| 26,148 shader programs | translated from RDNA 2 to DXIL and cached on disk; 6 do not translate yet |
| 25,751 GPU pipelines | built ahead of play by `dev.py prewarm` |
| 1 refused pipeline | on the RTX 5090, in the latest runs |

## Contributing

Issues and pull requests are welcome. Run `python tools/dev.py gate` before you open a pull request. The rules are in [CONTRIBUTING.md](CONTRIBUTING.md). Questions and test reports go to Bone/Nevaa on Discord: **bonemeld**.

## Credits

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak and its contributors ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 or later), and the authors of the forks it was assembled from, including Neptune (VIctory Edit.) by Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty and its contributors, the project El1xr was forked from.
- **The VIctory forks we learned from and ported from**, among them fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory, readmet3xt-lab/foxhound, RipcuBAN/victory-fork and Kemerd/VIctory-NVIDIA-RT.
- Every component and author, change by change, is credited in [THIRD_PARTY.md](THIRD_PARTY.md). The project's earlier history is in [docs/history.md](docs/history.md).

## License

El1xr is distributed under the [GNU GPL 3.0 or later](LICENSE). Third-party parts keep their own license: the `Task` module of libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) and the RDNA 2 tables generated from LLVM's public encoding tests (Apache 2.0 with LLVM exception, `PrototypeScarlett/emu/rdna2_ops.py` and `rdna2_shapes.py`).
