<div align="center">

# El1xr

**Grand Theft Auto VI (Xbox Series, build 1.0.2.1) running on a Windows PC.**<br>
4K, about 25 fps and climbing on an RTX 5090.

[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](LICENSE)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078D6?logo=windows&logoColor=white)](docs/running-gta6.md)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-B7410E?logo=rust&logoColor=white)](third_party/victory)
[![Discord](https://img.shields.io/badge/Discord-AnyXboxX-5865F2?logo=discord&logoColor=white)](https://discord.gg/TcX42vRyPU)

**English** · [Français](docs/i18n/README.fr.md) · [Español](docs/i18n/README.es.md) · [Deutsch](docs/i18n/README.de.md) · [Italiano](docs/i18n/README.it.md) · [Português](docs/i18n/README.pt-BR.md) · [日本語](docs/i18n/README.ja.md) · [中文](docs/i18n/README.zh-CN.md) · [Русский](docs/i18n/README.ru.md)

</div>

---

El1xr is Bone/Nevaa's GTA VI emulation project: a fork of [AnyXboxX](https://github.com/LooperSalty/AnyXboxX), built on [VIctory](https://github.com/Marvisak/VIctory) and on the best work of the VIctory forks (every source is credited in [THIRD_PARTY.md](THIRD_PARTY.md)). The game's own x86-64 code runs natively; its console system calls, Direct3D 12.X commands and RDNA 2 shaders are translated for a PC on the fly.

> [!NOTE]
> **Status, 27 November 2026**
>
> - 🎮 **The world is playable at 4K, 24-25 fps once settled** on an RTX 5090 (three quiet benchmark runs: 24.9, 25.2 and 24.2 fps). The goal is a steady 30 fps.
> - ✅ **0 refused GPU pipelines** and no GPU device loss in benchmark runs.
> - ✅ Cutscenes play in full and the first missions have been played through.
> - ⚡ **Batch 1 merged**: launch to the first world frame now about 31-33 s (was 41-54 s), and the worst freeze when entering the world 1.5-1.9 s (was 7.6-8.1 s).
> - 🚧 About **60 more speed-ups are written and in testing** (see [In testing](#in-testing)).

## At a glance

| Measure (RTX 5090, 4K) | Now | Goal |
|---|---|---|
| Settled frame rate | **24-25 fps** | 30+ fps |
| Median frame time | ~40 ms | ≤ 33.3 ms |
| 99th-percentile frame | 68-73 ms | no hitches |
| CPU time to prepare a frame | 25-26 ms | under the GPU's time |
| Refused GPU pipelines | **0** | 0 |
| Launch to first world frame | ~31-33 s | |
| Streaming after entering the world | 52-63 s | a few seconds |

Measured with `python tools/dev.py bench --quiet --seconds 200`: fast boot from a fixed checkpoint, nothing else running on the machine, numbers read from the emulator's own frame log. The launch time comes from batch 1's runs, made while other work was building, so a quiet machine should do a little better.

## Test machine

| Part | Model |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, driver 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 cores / 32 threads |
| Memory | 64 GB DDR5-6000 |
| Motherboard | MSI PRO B650-P WIFI |
| Storage | NVMe SSDs (Samsung 990 EVO Plus) |
| Operating system | Windows 11 (build 26200) |
| Game | GTA VI for Xbox Series, build 1.0.2.1 (your own copy) |

## Where a frame goes

![GPU time per frame: game passes 22.8 ms, emulator work 7.0 ms, untimed 3.8 ms, idle 6.2 ms](docs/images/frame-budget.svg)

A settled frame takes about 40 ms. The game's own rendering passes take 22.8 ms of GPU time, so 30 fps is within reach. The rest goes to:

- **our own GPU work** (7 ms): bounds checks on indirect draws, texture copies between formats, checks of what the GPU wrote;
- **GPU idle** (6-10 ms): the card waits while one CPU thread prepares the next batch of work.

Both are what the work in testing goes after.

## What changed this week

**Fixed**
- Frost from the prison window is no longer drawn over Lucia's hair.
- The "lines on everything" in sunlight are gone: the shader translator treated some values as equal across both halves of a 64-lane wave when they were not (31 shaders).
- The blotches on the diner's glass are gone: the ray-traced glass pass now runs as "no hit" instead of being skipped.
- Audio: safer XMA decoding and a clean mute and shutdown path.

**Faster**
- Texture uploads moved off the game's drawing threads, texture checks run in parallel, and released lists are freed on their own thread: the worst freeze when entering the world dropped from 9.5 s to 6.6 s.
- The CPU-side driver allocation of a texture costs 382-529 µs on this machine and 5-8 µs from a pre-made memory block; pooled textures and buffers are in testing.

**Tooling**
- Fast boot for test runs: skips the intro movie and starts from a frozen checkpoint.
- Every bench reports fps, frame-time percentiles, hitches over 100 and 250 ms, launch and streaming times, GPU driver resets, and saves screenshots.
- Per-pass GPU cost, per-thread frame phases and a hitch log with stack samples.
- The shader translator and the GPU host were split into smaller modules; the translator's output stayed byte-identical on all 26,148 shader programs of the game.

## Known issues

- **Run-to-run visual faults**: in some runs Lucia's diner top has the wrong texture, faces are black or blotched, or hair turns orange at the prison window. Fixes ported from other forks are in testing.
- **Slow streaming**: after entering the world it takes about a minute for every asset to arrive, with pop-in meanwhile.
- **GPU waiting on the CPU**: 6-10 ms per frame (see above).
- **Ray-traced lighting is mostly off**: the game builds its ray-tracing structures in software, and that builder is disabled.

## In testing

Work is written in parallel and tested in batches on the one GPU: every change sits behind its own `VICTORY_*` switch, a batch runs all new switches on against all off, and each switch is kept or dropped on the numbers and the screenshots.

- **Batch 1 (done, 20 switches kept)**: unlocked texture staging, fewer descriptor rebuilds and lock waits, parallel character baking at load, shaders translated as soon as the game creates them, faster pipeline-library loads, leaner memory mapping, and the outfit, face and hair fixes (Lucia's diner top was right in 4 of 5 runs, against 0 of 2 before). Dropped for now: the texture and buffer memory pools (they slowed memory unmaps) and two switches that crashed; their writers are fixing them.
- **Batch 2 (running)**: title memory in 2 MiB sections (mapping time 55.5 s → 7.3 s upstream), game buffers kept in GPU memory (−15 % frame time upstream on an RTX 5090), DLSS upscaling, higher priority for the emulator's frame threads, offline shader and pipeline pre-warming, and hardware ray tracing on the RT cores.

## Run it

You need your own copy of the game, Windows 10 or 11, a GPU with Shader Model 6.6 and mesh shaders, and Rust.

```powershell
tools\lancer_victory.ps1 -GameDir <game folder>
```

The full guide (install, settings, troubleshooting) is [docs/running-gta6.md](docs/running-gta6.md).

> [!IMPORTANT]
> This repository contains no game files, keys or executables, and does not explain how to get them.

For development, `tools/dev.py` wraps the common steps:

```powershell
python tools/dev.py build      # release build
python tools/dev.py gate       # every test, fails on any warning
python tools/dev.py prewarm    # translate all shaders offline (about 45 s)
python tools/dev.py bench --quiet --seconds 200   # measured game run
python tools/dev.py train --plan plan.toml        # one run of a batch plan
```

## Project in numbers

| Figure | What |
|---|---|
| 1600+ automated tests | Python tools and Rust crates, run by `dev.py gate` |
| 26,148 shader programs | translated from RDNA 2 to DXIL, cached on disk |
| 0 refused pipelines | on the RTX 5090 |

## History

How the project got here, from the first console DLLs loaded in an emulator to the first frames of the world: [docs/history.md](docs/history.md).

## Contributing

Issues and pull requests are welcome. Run `python tools/dev.py gate` before opening a pull request. The rules are in [CONTRIBUTING.md](CONTRIBUTING.md). Discussion and test reports: [Discord](https://discord.gg/TcX42vRyPU).

## Credits

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak and its contributors ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 or later), and the authors of the forks it was assembled from, including Neptune (VIctory Edit.) by Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty and its contributors ([AnyXboxX](https://github.com/LooperSalty/AnyXboxX)), the project El1xr was forked from.
- **The VIctory forks we learned from and ported from**, among them fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory and readmet3xt-lab/foxhound.
- Every component and author, change by change, is credited in [THIRD_PARTY.md](THIRD_PARTY.md).

## License

El1xr is distributed under the [GNU GPL 3.0 or later](LICENSE). Third-party parts keep their own license: the `Task` module of libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) and the RDNA 2 tables generated from LLVM's public encoding tests (Apache 2.0 with LLVM exception, `PrototypeScarlett/emu/rdna2_ops.py` and `rdna2_shapes.py`).
