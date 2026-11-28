<div align="center">

![El1xr：在 Windows PC 上运行 Xbox Series 版 Grand Theft Auto VI。RTX 5090 上 4K 23.1-25.7 fps，0 个被拒绝的管线，从启动到进入世界 27-28 秒，截至 2026 年 11 月 27 日已可游玩](../images/hero.svg)

![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-9b5cff?style=flat-square&labelColor=170f2b)
![Platform: Windows 10 and 11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2de2e6?style=flat-square&labelColor=170f2b)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-ff8a3d?style=flat-square&labelColor=170f2b&logo=rust&logoColor=white)
![Status: playable](https://img.shields.io/badge/status-playable-ff3d8b?style=flat-square&labelColor=170f2b)
![23.1-25.7 fps at 4K on RTX 5090](https://img.shields.io/badge/4K%20on%20RTX%205090-23.1--25.7%20fps-ffd36e?style=flat-square&labelColor=170f2b)
![Discord: bonemeld](https://img.shields.io/badge/Discord-bonemeld-5865F2?style=flat-square&labelColor=170f2b&logo=discord&logoColor=white)

[English](../../README.md) · [Français](README.fr.md) · [Español](README.es.md) · [Deutsch](README.de.md) · [Italiano](README.it.md) · [Português](README.pt-BR.md) · [日本語](README.ja.md) · **中文** · [Русский](README.ru.md)

</div>

El1xr 让 Xbox Series 版 Grand Theft Auto VI（版本 1.0.2.1）在 Windows PC 上运行，是 Bone/Nevaa 的项目。游戏自身的 x86-64 代码在你的处理器上以原生方式运行；它的主机系统调用、Direct3D 12.X 命令和 RDNA 2 着色器则在游戏运行时被翻译为 PC 可用的形式。本项目不做 CPU 模拟，本仓库也不包含任何游戏内容：你需要使用自己的游戏副本。

## 现状

> [!NOTE]
> **截至 2026 年 11 月 27 日，GTA VI（Xbox Series，版本 1.0.2.1）已能在 Windows 上启动、进入菜单，并在开放世界中游玩。** 在最近一批无其他负载的测试运行中（第 4 批，4K，RTX 5090），游戏在整个 200 秒运行中达到 23.1-25.7 fps，被拒绝的 GPU 管线为 0。这仍未达到下一个目标：稳定的 30 fps。之后是消除卡顿和画面错误。

![里程碑：2026 年 11 月 24 日前实现启动、主菜单和进入游戏；2026 年 11 月 27 日以 23.1-25.7 fps 可玩；下一步 30 fps；稳定性进行中](../images/milestones.svg)

| 里程碑 | 状态 | 依据 | 日期 |
|---|---|---|---|
| 可执行文件运行 | 已完成 | Xbox Series 1.0.2.1 的可执行文件可在 Windows 上启动，其 x86-64 代码以原生方式运行。 | 2026 年 11 月 24 日前 |
| 第一帧 | 已完成 | 启动动画和法律声明画面渲染正常，没有闪烁。 | 2026 年 11 月 24 日前 |
| 主菜单 | 已完成 | 档案画面、首次运行设置、法律声明画面、加载画面和暂停菜单全部正常工作。菜单以 30 fps 运行（RTX 4070 SUPER）。 | 2026 年 11 月 24 日前 |
| 进入游戏 | 已完成 | 游戏可进入开放世界，角色可由玩家操控移动。在监狱中进行的一次 13 分钟游戏约为 4 fps（RTX 4070 SUPER）。 | 2026 年 11 月 24 日前 |
| 可游玩 | 已完成，速度未达目标 | 游戏可在 4K 下以 23.1-25.7 fps 游玩（RTX 5090；第 4 批两次无其他负载的 200 秒运行）。 | 2026 年 11 月 27 日 |
| 剧情进度 | 已完成前两个任务 | 一位贡献者在一次约 2 fps 的 45 分钟游戏中完成了前两个任务（RTX 4070 SUPER；为本人报告，并非根据日志测得）。所有过场动画均可完整播放。在 Linux（Proton，RX 9070 XT）上，一次 23 分钟的运行通过了序章，进入第 1 章。 | 2026 年 11 月 25 日 |
| 30 fps | 尚未达到 | 23.1-25.7 fps，帧时间中位数 40-43 ms，目标为 33.3 ms（RTX 5090，4K）。 | 2026 年 11 月 27 日 |
| 稳定 | 进行中 | 使用默认设置时，200 秒的测试运行可以完整结束，没有崩溃、GPU 驱动重置或设备丢失。进入世界时的停顿已变少（第 4 批 8 次无负载运行中出现 1 次），但尚未消失（[#78](https://github.com/kylepelham/El1xr/issues/78)），画面错误也仍会因每次运行而异（见“已知问题”）。 | 2026 年 11 月 27 日 |

## 性能

![一览：23.1-25.7 fps，从启动到进入世界 27-28 秒，先准备烘焙输入后餐馆 24 张截图中黑脸为 0，加载期间显示的黑帧由 152 降至 31](../images/stats.svg)

| 指标（RTX 5090，4K） | 当前 | 目标 |
|---|---|---|
| 200 秒运行的帧率 | **23.1-25.7 fps** | 30+ fps |
| 帧时间中位数 | 40-43 ms | 不超过 33.3 ms |
| 稳定游玩时的第 99 百分位帧 | 64-69 ms | 无卡顿 |
| 准备一帧的 CPU 时间 | 26-28 ms | 低于 GPU 时间 |
| 被拒绝的 GPU 管线 | **0** | 0 |
| 从启动到世界第一帧 | 27-28 s | |
| 进入世界时的停顿 | 8 次无负载运行中 1 次停顿 6.6 s；其余运行最差帧 1.2-3.0 s（[#78](https://github.com/kylepelham/El1xr/issues/78)） | 无 |
| 世界内超过 250 ms 的帧 | 每次运行 10-12 帧 | 无 |
| 前 90 秒显示的黑帧 | 31 | 无 |
| 进入世界后的流式加载 | 71-72 s | 几秒 |

这些数据来自第 4 批（2026 年 11 月 27 日），使用启动器现在默认开启的开关。帧率、帧时间、CPU 时间、超过 250 ms 的帧和流式加载来自两次无其他负载的 200 秒运行：每次运行都从固定的存档点快速启动，期间机器上的其他工作暂停，数据读取自模拟器自身的帧日志。启动时间来自两次开启新加载开关的运行，与两次不开启的运行（32-33 秒）交替进行，期间其他 CPU 核心上有编译任务在运行。黑帧在一次无其他负载的 90 秒运行中统计。每次运行都开启了部分诊断日志。

![一帧的时间去向：游戏渲染通道 22.8 ms，El1xr 的 GPU 工作 7.0 ms，未计时 3.8 ms，GPU 空闲 6.2 ms，对照 30 fps 对应的 33.3 ms 线](../images/frame-budget.svg)

在第 3 批之前采集的最近一次 GPU 时间剖析中，稳定后的一帧约需 40 ms。游戏自身的渲染通道占用 22.8 ms 的 GPU 时间，因此 30 fps 仍有充足余地。其余时间是 El1xr 自己的 GPU 工作（7 ms：间接绘制的边界检查、不同格式之间的纹理复制、上传），以及 GPU 在一个 CPU 线程准备下一批工作时的空闲时间（6-10 ms）。接下来的更新正是针对这两项。

![第 4 批：使用加载开关后，从启动到世界第一帧由 32-33 秒缩短至 27-28 秒；保留最后一帧后，前 90 秒显示的黑帧由 152 降至 31；先准备烘焙输入后，出现黑脸的餐馆截图由每次运行 0-3 张降至 0 张](../images/load-times.svg)

第 4 批（2026 年 11 月 27 日）保留了四项改动，现已在启动器中默认开启。第 3 批的开关继续保持开启。

- **先准备烘焙输入**（`VICTORY_BAKE_INPUTS=1`）：面部和皮肤烘焙会最先拿到它们的输入纹理。在餐馆中，两次运行的 24 张截图里没有一张出现黑脸，而不开此开关时每次运行有 0-3 张，帧率上没有可测量的代价。
- **更快的加载**（`VICTORY_LOAD_FPS=60`、`VICTORY_LOAD_END=world`、`VICTORY_LOAD_UNPACED=1`）：在世界出现之前，加载画面以最高 60 fps 运行。在连续对比的运行中，从启动到世界第一帧由 32-33 秒缩短至 27-28 秒，进入世界时也没有出现新的停顿。
- **保留最后一帧**（`VICTORY_HOLD_LAST_FRAME=1`）：当游戏呈现一帧全黑画面时，屏幕上改为保留最后一帧正常画面。在一次无其他负载的 90 秒运行中，它遮住了游戏呈现的 71 帧黑帧中的 40 帧；剩下的 31 帧出现在启动最开始，那时还没有可以保留的画面。不开此开关的同类运行中，屏幕上出现了 152 帧黑帧。
- **游戏内设置菜单**（`VICTORY_MENU=1`）：在游戏窗口位于前台时按 F9，或按住 View 和 Menu 一秒。下一批的自检在运行中的游戏上方两次打开了它，绘制了全部 10 个标签页，并在关闭时把输入交还给游戏。

与所有开关关闭的运行相比，第 3 批的开关还把前 90 秒的黑帧由 116 减少到 77，并把进入世界时的最差帧由 5.8 秒缩短到 1.7-2.7 秒。另有八个开关被弃用：一个撕裂了监狱牢房的墙和床铺（`SPLIT_AHEAD`），两个让人脸或牢房变黑或出现噪点（`EXCHANGE_PAIRS`、`GPU_DETILE`），一个虽然节省了 5 秒加载时间，却让进入世界时的黑脸再次出现（`BAKE_ONE_BUDGET`），一个让每帧 CPU 时间增加 13 ms（`FLUSH_ONE_PASS`），一个漏掉了纹理变化（`TEXTURE_HASH_SAMPLED`），还有两个没有带来可测量的变化（`CARD_DECL_JOIN`、`STALE_BATCH`）。测试还发现，在 4K 下开启超分辨率会导致游戏崩溃（除以零，0xc0000094）；修复已经写好，正在等待测试运行。维护者玩过这个版本后的评价是“稳定得多，卡顿更少，帧率更稳定，闪烁更少”；这是游玩时的印象，而不是测量结果。

### 测试机器

| 部件 | 型号 |
|---|---|
| GPU | NVIDIA GeForce RTX 5090，32 GB，驱动 621.24 |
| CPU | AMD Ryzen 9 9950X3D，16 核 / 32 线程 |
| 内存 | 64 GB DDR5-6000 |
| 主板 | MSI PRO B650-P WIFI |
| 存储 | NVMe SSD（Samsung 990 EVO Plus） |
| 操作系统 | Windows 11（版本 26200） |
| 游戏 | Xbox Series 版 GTA VI，版本 1.0.2.1（你自己的副本） |

当前所有数据均来自这台机器。本 README 中较早的数据是在 RTX 4070 SUPER（Windows）或 RX 9070 XT（Linux）上测得的，每处都已注明。

## 工作原理

![工作原理：游戏的 x86-64 代码在 CPU 上原生运行；主机系统调用交给 Windows 上的 HLE 内核处理；D3D12.X 命令被重建为宿主机的 Direct3D 12；RDNA 2 着色器被翻译为 DXIL 并缓存](../images/how-it-works.svg)

- **游戏代码**按原样加载和运行。Xbox Series 与 Windows PC 使用相同的 x86-64 指令集，因此无需模拟任何指令。
- **系统调用**：对主机内核和 GDK 库的调用由 El1xr 基于 Windows 的高层模拟（HLE）处理，涵盖内存、线程、文件和系统服务。
- **图形**：主机的 Direct3D 12.X 命令列表、内存和资源在你的 GPU 上被重建为标准的 Direct3D 12。
- **着色器**：为主机 RDNA 2 GPU 编译的着色器会被翻译为 DXIL。游戏全部 26,148 个着色器程序都能翻译，每个在首次使用后都会缓存到磁盘。

## 运行

你需要自己的游戏副本、Windows 10 或 11、支持 Shader Model 6.6 和网格着色器的 GPU，以及 Rust。

```powershell
tools\lancer_victory.ps1 -GameDir <游戏文件夹>
```

完整指南（安装、设置、故障排除）见 [docs/running-gta6.md](../running-gta6.md)。

> [!IMPORTANT]
> 本仓库不包含任何游戏文件、密钥或可执行文件，也不说明如何获取它们。

<details>
<summary>开发者命令</summary>

```powershell
python tools/dev.py build      # release 构建
python tools/dev.py prewarm    # 离线翻译全部着色器（约 45 s）
python tools/dev.py bench --quiet --seconds 200   # 带测量的游戏运行
python tools/dev.py gate       # 全部测试，出现任何警告即失败
```

</details>

## 已知问题

- **每次运行都可能不同的画面错误**：先准备烘焙输入后，人脸发黑或出现斑块的情况变少了，但在部分运行中仍会出现（[#35](https://github.com/kylepelham/El1xr/issues/35)）。Lucia 在餐馆里仍可能穿错服装，在监狱窗边头发也可能变成橙色。
- **进入世界时的停顿**：比第 3 批少了（那时几乎每次运行都会冻结 3-9 秒），但第 4 批 8 次无负载运行中仍有 1 次在 GPU 处理一批命令时冻结了 6.6 秒。原因正在 [#78](https://github.com/kylepelham/El1xr/issues/78) 中调查。
- **流式加载缓慢**：进入世界后，所有资源到齐大约需要 71 秒，在此期间会出现物体突然弹出的现象。
- **4K 下开启超分辨率会崩溃**：开启 DLSS 或 FSR 超分辨率时，游戏会在世界出现时崩溃（游戏的 FSR 4 代码中除以零）。修复正在等待测试运行；超分辨率默认关闭。
- **GPU 等待 CPU**：每帧 6-10 ms（见上文一帧的时间去向）。
- **光线追踪光照基本关闭**：游戏在软件中构建其光线追踪结构，而这个构建器目前已被禁用。
- **仅有一台参考机器**：当前数据来自上述 RTX 5090，其他 GPU 尚未用当前版本测量。

## 下一步

目标是在 RTX 5090 上以 4K 稳定运行在 30 fps，然后消除卡顿和画面错误。另有约 60 项提速改动已经写好，正等待测量。每一项在得到验证之前都默认关闭。

- **进入世界时的停顿**（[#78](https://github.com/kylepelham/El1xr/issues/78)）：找出那批 GPU 命令中是哪个渲染通道耗时数秒。
- **更少的等待和开销**：削减 GPU 每帧等待 CPU 的 6-10 ms，以及 El1xr 自身 7 ms 的 GPU 工作。
- **DLSS 超分辨率**（待崩溃修复测试通过后）和**在 RT 核心上进行硬件光线追踪**，后者将恢复光线追踪光照。
- **离线预热着色器和管线**，使游戏过程中不再编译任何着色器。
- **修复黑脸问题（[#35](https://github.com/kylepelham/El1xr/issues/35)）以及因每次运行而异的服装问题**。

## 项目数据

| 数字 | 内容 |
|---|---|
| 1800+ 个自动化测试 | Python 工具和 Rust crate，由 `dev.py gate` 运行 |
| 26,148 个着色器程序 | 从 RDNA 2 翻译为 DXIL，缓存在磁盘上 |
| 0 个被拒绝的管线 | 在 RTX 5090 上 |

## 参与贡献

欢迎提交 issue 和 pull request。提交 pull request 前请先运行 `python tools/dev.py gate`。规则见 [CONTRIBUTING.md](../../CONTRIBUTING.md)。问题和测试报告请在 Discord 上发给 Bone/Nevaa：**bonemeld**。

## 致谢

- **El1xr**：Bone/Nevaa。
- **VIctory**：Marvisak 及其贡献者（[VIctory](https://github.com/Marvisak/VIctory)，GPL-3.0 或更高版本），以及组装它所用的各个分支的作者，其中包括 Traffic & zm0kie 的 Neptune (VIctory Edit.)。
- **AnyXboxX**：Looper_Salty 及其贡献者，El1xr 即从该项目分支而来。
- **我们学习并移植过代码的 VIctory 分支**，其中包括 fxceless/victory-fork、Peppacodes、Belobug、stellarr-studio/kelvaroq、uiliannn/VItoriaBr、DoubleGhosty/cool_fox、avatarmanz、3cvil/M1LK、trustno1-coder、TaynanGT/Vice-Tay、Linox-IDK/LNX-victory 和 readmet3xt-lab/foxhound。
- 每个组件和作者都按每次改动在 [THIRD_PARTY.md](../../THIRD_PARTY.md) 中注明。项目早期的历史见 [docs/history.md](../history.md)。

## 许可证

El1xr 以 [GNU GPL 3.0 或更高版本](../../LICENSE) 发布。第三方部分保留各自的许可证：libHttpClient 的 `Task` 模块（MIT，`runtime/third_party/libhttpclient-task`），以及根据 LLVM 公开编码测试生成的 RDNA 2 表（Apache 2.0 with LLVM exception，`PrototypeScarlett/emu/rdna2_ops.py` 和 `rdna2_shapes.py`）。
