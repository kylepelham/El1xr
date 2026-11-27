<div align="center">

![El1xr: Grand Theft Auto VI for Xbox Series, running on a Windows PC. 23.5 fps in the world at 4K on an RTX 5090, 0 refused pipelines, 32 s from launch to the world, playable as of 27 November 2026](docs/images/hero.svg)

![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-9b5cff?style=flat-square&labelColor=170f2b)
![Platform: Windows 10 and 11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2de2e6?style=flat-square&labelColor=170f2b)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-ff8a3d?style=flat-square&labelColor=170f2b&logo=rust&logoColor=white)
![Status: playable](https://img.shields.io/badge/status-playable-ff3d8b?style=flat-square&labelColor=170f2b)
![23.5 fps at 4K on RTX 5090](https://img.shields.io/badge/4K%20on%20RTX%205090-23.5%20fps-ffd36e?style=flat-square&labelColor=170f2b)
![Discord: bonemeld](https://img.shields.io/badge/Discord-bonemeld-5865F2?style=flat-square&labelColor=170f2b&logo=discord&logoColor=white)

**English** · [Français](docs/i18n/README.fr.md) · [Español](docs/i18n/README.es.md) · [Deutsch](docs/i18n/README.de.md) · [Italiano](docs/i18n/README.it.md) · [Português](docs/i18n/README.pt-BR.md) · [日本語](docs/i18n/README.ja.md) · [中文](docs/i18n/README.zh-CN.md) · [Русский](docs/i18n/README.ru.md)

</div>

El1xr runs the Xbox Series version of Grand Theft Auto VI (build 1.0.2.1) on a Windows PC. It is Bone/Nevaa's project. The game's own x86-64 code runs natively on your processor. Its console system calls, Direct3D 12.X commands and RDNA 2 shaders are translated for the PC as the game runs. There is no CPU emulation and no game content in this repository: you bring your own copy.

## Status

> [!NOTE]
> **As of 27 November 2026, GTA VI (Xbox Series, build 1.0.2.1) boots, reaches the menus and is playable in the open world on Windows.** In the latest quiet test runs (batch 3, 4K, RTX 5090) the world runs at a median 23.5 fps, with 0 refused GPU pipelines. That is still short of the next goal, a steady 30 fps. After that come no hitches and no visual faults.

![Milestones: boots, main menu and in-game by 24 November 2026; playable at 23.5 fps on 27 November 2026; 30 fps next; stable in progress](docs/images/milestones.svg)

| Milestone | Status | Evidence | Date |
|---|---|---|---|
| Executable runs | Done | The Xbox Series 1.0.2.1 executable starts on Windows. Its x86-64 code runs natively. | by 24 Nov 2026 |
| First frame | Done | The startup animation and legal screens render without flicker. | by 24 Nov 2026 |
| Main menu | Done | Profile screen, first-run settings, legal screens, loading screen and pause menu all work. Menus run at 30 fps (RTX 4070 SUPER). | by 24 Nov 2026 |
| In-game | Done | The game reaches the open world and the character moves under player control. A 13-minute session in the prison ran at about 4 fps (RTX 4070 SUPER). | by 24 Nov 2026 |
| Playable | Done, below target speed | The world plays at 23.5 fps at 4K (RTX 5090; median of the in-world part of a quiet 200-second batch 3 run). | 27 Nov 2026 |
| Story progress | First two missions completed | A contributor completed the first two missions in a 45-minute session at about 2 fps (RTX 4070 SUPER; reported, not measured from logs). All cutscenes play in full. On Linux (Proton, RX 9070 XT), a 23-minute run got past the prologue into chapter 1. | 25 Nov 2026 |
| 30 fps | Not yet | 23.5 fps in the world, 42 ms median frame time against the 33.3 ms target (RTX 5090, 4K). | 27 Nov 2026 |
| Stable | In progress | With the default settings, 200-second test runs finish with no crash, no GPU driver reset and no device loss. Entering the world still stalls for 3-9 s ([#78](https://github.com/kylepelham/El1xr/issues/78)), and visual faults still vary from run to run (see Known issues). | 27 Nov 2026 |

## Performance

![At a glance: 23.5 fps in the world, 0 refused GPU pipelines, 32 s from launch to the world, 7 hitches over 1 s per run](docs/images/stats.svg)

| Measure (RTX 5090, 4K) | Now | Goal |
|---|---|---|
| In-world frame rate (median) | **23.5 fps** | 30+ fps |
| Median frame time | 42 ms | 33.3 ms or less |
| 99th-percentile frame in steady play | ~70 ms | no hitches |
| CPU time to prepare a frame | 28 ms | under the GPU's time |
| Refused GPU pipelines | **0** | 0 |
| Launch to first world frame | 32 s | |
| Launch to settled play | 49 s | |
| Stall when entering the world | 3-9 s ([#78](https://github.com/kylepelham/El1xr/issues/78)) | none |
| Frames over 250 ms in the world | 20 per run | none |
| Hitches over 1 s | 7 per run | none |
| Streaming after entering the world | 74 s | a few seconds |

These figures come from batch 3's quiet runs (`python tools/dev.py bench --quiet --seconds 200`, 27 November 2026). Each run fast-boots from a fixed checkpoint while other work on the machine is paused, and the numbers are read from the emulator's own frame log. Some diagnostic logging was on in every run. The stall when entering the world is the range over batch 3's runs.

![Where a frame goes: game passes 22.8 ms, El1xr GPU work 7.0 ms, untimed 3.8 ms, GPU idle 6.2 ms, against the 33.3 ms line for 30 fps](docs/images/frame-budget.svg)

The last GPU timing profile, taken before batch 3, put a settled frame at about 40 ms. The game's own rendering passes need 22.8 ms of GPU time, which leaves room for 30 fps. The rest is El1xr's own GPU work (7 ms: bounds checks on indirect draws, texture copies between formats, uploads) and time the GPU sits idle (6-10 ms) while one CPU thread prepares the next batch of work. Those two are what the next updates go after.

![Batch 3: with pooled blocks, launch to the first world frame went from 58-67 s to 32 s, launch to settled play from 117-120 s to 49 s, and frames over 250 ms in the world from 51-74 to 20](docs/images/load-times.svg)

Batch 3 (27 November 2026) kept two groups of switches, now on by default in the launcher: memory in 2 MiB sections, game buffers kept in GPU memory and higher priority for the frame threads (the seven-switch fg_rt set), and staging, GPU, shared and upload buffers taken from pooled blocks (four switches). The 4 KiB write guards are on too; they measured neutral. Against the same build without pooled blocks, run back to back, launch to the first world frame dropped from 58-67 s to 32 s, settled play from 117-120 s to 49 s, frames over 250 ms in the world from 51-74 to 20, hitches over 1 s from 26-35 to 7, and streaming from 80-87 s to 74 s. A seven-switch group that tore the walls of the prison cell was dropped. The batch's run with none of these switches got 14 fps and a 100 s launch, but the machine was under heavier load during it, so it is not a clean before-and-after comparison.

### Test machine

| Part | Model |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, driver 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 cores / 32 threads |
| Memory | 64 GB DDR5-6000 |
| Motherboard | MSI PRO B650-P WIFI |
| Storage | NVMe SSDs (Samsung 990 EVO Plus) |
| Operating system | Windows 11 (build 26200) |
| Game | GTA VI for Xbox Series, build 1.0.2.1 (your own copy) |

All current figures come from this machine. Older figures in this README were measured on an RTX 4070 SUPER (Windows) or an RX 9070 XT (Linux), and each one says so.

## How it works

![How it works: the game's x86-64 code runs natively on the CPU; console system calls go to the HLE kernel on Windows; D3D12.X commands are rebuilt as host Direct3D 12; RDNA 2 shaders are translated to DXIL and cached](docs/images/how-it-works.svg)

- **Game code** is loaded and run as is. The Xbox Series and a Windows PC share the x86-64 instruction set, so no instruction is emulated.
- **System calls** to the console kernel and GDK libraries are served by El1xr's high-level emulation (HLE) on top of Windows: memory, threads, files and system services.
- **Graphics**: the console's Direct3D 12.X command lists, memory and resources are rebuilt as standard Direct3D 12 on your GPU.
- **Shaders** compiled for the console's RDNA 2 GPU are translated to DXIL. All 26,148 shader programs of the game translate, and each one is cached on disk after its first use.

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

- **Visual faults that vary from run to run.** In about half of test runs, faces turn black or blotched (being worked on in [#35](https://github.com/kylepelham/El1xr/issues/35)), Lucia wears the wrong outfit in the diner, or her hair turns orange at the prison window.
- **A stall when entering the world.** Right after the world first appears, the game freezes for 3-9 s while the GPU works through one batch of commands. The cause is being investigated in [#78](https://github.com/kylepelham/El1xr/issues/78).
- **Slow streaming.** After entering the world it takes about 74 s for every asset to arrive, with pop-in meanwhile.
- **The GPU waits on the CPU** for 6-10 ms per frame (see the frame budget above).
- **Ray-traced lighting is mostly off.** The game builds its ray-tracing structures in software, and that builder is disabled for now.
- **One reference machine.** Current figures are from the RTX 5090 above. Other GPUs have not been measured with the current build.

## What's next

The target is a steady 30 fps at 4K on the RTX 5090, then no hitches and no visual faults. About 60 more speed-ups are written and waiting to be measured. Each one is off by default until it is proven.

- **The world-entry stall** ([#78](https://github.com/kylepelham/El1xr/issues/78)): finding which pass in that batch of GPU commands takes seconds.
- **Less waiting and less overhead**: cutting the 6-10 ms the GPU waits on the CPU each frame, and El1xr's own 7 ms of GPU work.
- **DLSS upscaling** and **hardware ray tracing on the RT cores**, which would bring back ray-traced lighting.
- **Offline shader and pipeline pre-warming**, so that no shader is compiled during play.
- **Fixes for the black faces ([#35](https://github.com/kylepelham/El1xr/issues/35)) and the run-to-run outfits**.

## In numbers

| Figure | What |
|---|---|
| 1600+ automated tests | Python tools and Rust crates, run by `dev.py gate` |
| 26,148 shader programs | translated from RDNA 2 to DXIL, cached on disk |
| 0 refused pipelines | on the RTX 5090 |

## Contributing

Issues and pull requests are welcome. Run `python tools/dev.py gate` before you open a pull request. The rules are in [CONTRIBUTING.md](CONTRIBUTING.md). Questions and test reports go to Bone/Nevaa on Discord: **bonemeld**.

## Credits

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak and its contributors ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 or later), and the authors of the forks it was assembled from, including Neptune (VIctory Edit.) by Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty and its contributors, the project El1xr was forked from.
- **The VIctory forks we learned from and ported from**, among them fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory and readmet3xt-lab/foxhound.
- Every component and author, change by change, is credited in [THIRD_PARTY.md](THIRD_PARTY.md). The project's earlier history is in [docs/history.md](docs/history.md).

## License

El1xr is distributed under the [GNU GPL 3.0 or later](LICENSE). Third-party parts keep their own license: the `Task` module of libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) and the RDNA 2 tables generated from LLVM's public encoding tests (Apache 2.0 with LLVM exception, `PrototypeScarlett/emu/rdna2_ops.py` and `rdna2_shapes.py`).
