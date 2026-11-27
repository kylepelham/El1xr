<div align="center">

# El1xr

**在 Windows PC 上运行 Grand Theft Auto VI（Xbox Series，版本 1.0.2.1）。**<br>
RTX 5090 上 4K 约 25 fps，并且仍在提升。

[![许可证: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](../../LICENSE)
[![平台: Windows](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078D6?logo=windows&logoColor=white)](../running-gta6.md)
[![使用 Rust 构建](https://img.shields.io/badge/built%20with-Rust-B7410E?logo=rust&logoColor=white)](../../third_party/victory)
[![Discord](https://img.shields.io/badge/Discord-AnyXboxX-5865F2?logo=discord&logoColor=white)](https://discord.gg/TcX42vRyPU)

[English](../../README.md) · [Français](README.fr.md) · [Español](README.es.md) · [Deutsch](README.de.md) · [Italiano](README.it.md) · [Português](README.pt-BR.md) · [日本語](README.ja.md) · **中文** · [Русский](README.ru.md)

</div>

---

El1xr 是 Bone/Nevaa 的 GTA VI 模拟项目：它是 [AnyXboxX](https://github.com/LooperSalty/AnyXboxX) 的分支，构建于 [VIctory](https://github.com/Marvisak/VIctory) 以及各个 VIctory 分支的最佳成果之上（所有来源都在 [THIRD_PARTY.md](../../THIRD_PARTY.md) 中注明）。游戏自身的 x86-64 代码以原生方式运行；它的主机系统调用、Direct3D 12.X 命令和 RDNA 2 着色器则被实时翻译为 PC 可用的形式。

> [!NOTE]
> **现状，2026 年 11 月 27 日**
>
> - 🎮 **在 RTX 5090 上，世界已可在 4K 下游玩，稳定后 24-25 fps**（三次无其他负载的基准测试：24.9、25.2 和 24.2 fps）。目标是稳定的 30 fps。
> - ✅ 基准测试运行中 **被拒绝的 GPU 管线为 0**，也没有出现 GPU 设备丢失。
> - ✅ 过场动画可以完整播放，最初的几个任务已经通关；启动后 46-52 秒出现世界的第一帧。
> - 🚧 另有约 **60 项提速改动已经写好，正在测试中**（见[测试中](#测试中)）。

## 概览

| 指标（RTX 5090，4K） | 当前 | 目标 |
|---|---|---|
| 稳定后的帧率 | **24-25 fps** | 30+ fps |
| 帧时间中位数 | ~40 ms | ≤ 33.3 ms |
| 第 99 百分位帧 | 68-73 ms | 无卡顿 |
| 准备一帧的 CPU 时间 | 25-26 ms | 低于 GPU 时间 |
| 被拒绝的 GPU 管线 | **0** | 0 |
| 从启动到世界第一帧 | 46-52 s | |
| 进入世界后的流式加载 | 52-63 s | 几秒 |

测量方式：`python tools/dev.py bench --quiet --seconds 200`，从固定的存档点快速启动，机器上不运行其他任何程序，数据读取自模拟器自身的帧日志。

## 测试机器

| 部件 | 型号 |
|---|---|
| GPU | NVIDIA GeForce RTX 5090，32 GB，驱动 621.24 |
| CPU | AMD Ryzen 9 9950X3D，16 核 / 32 线程 |
| 内存 | 64 GB DDR5-6000 |
| 主板 | MSI PRO B650-P WIFI |
| 存储 | NVMe SSD（Samsung 990 EVO Plus） |
| 操作系统 | Windows 11（版本 26200） |
| 游戏 | Xbox Series 版 GTA VI，版本 1.0.2.1（你自己的副本） |

## 一帧的时间去向

![每帧 GPU 时间：游戏渲染通道 22.8 ms，模拟器工作 7.0 ms，未计时 3.8 ms，空闲 6.2 ms](../images/frame-budget.svg)

稳定后的一帧约需 40 ms。游戏自身的渲染通道占用 22.8 ms 的 GPU 时间，因此 30 fps 触手可及。其余时间花在：

- **我们自己的 GPU 工作**（7 ms）：间接绘制的边界检查、不同格式之间的纹理复制、对 GPU 写入内容的检查；
- **GPU 空闲**（6-10 ms）：显卡在等待一个 CPU 线程准备下一批工作。

测试中的工作正是针对这两项。

## 本周变化

**修复**
- 监狱窗户上的霜不再绘制在 Lucia 的头发上。
- 阳光下“所有东西上都有线条”的问题已消失：着色器翻译器曾把某些值当作在 64 通道 wave 的前后两半中相同，而实际上并不相同（31 个着色器）。
- 餐馆玻璃上的斑块已消失：光线追踪玻璃通道现在按“未命中”运行，而不是被跳过。
- 音频：更安全的 XMA 解码，以及干净的静音和关闭流程。

**更快**
- 纹理上传移出了游戏的绘制线程，纹理检查改为并行执行，已释放的列表在专用线程上释放：进入世界时最严重的卡死从 9.5 秒降到 6.6 秒。
- 在这台机器上，CPU 端由驱动分配一张纹理需要 382-529 µs，而从预先分配好的内存块中分配只需 5-8 µs；纹理和缓冲区的池化正在测试中。

**工具**
- 测试运行的快速启动：跳过片头动画，从冻结的存档点开始。
- 每次基准测试都会报告 fps、帧时间百分位、超过 100 ms 和 250 ms 的卡顿、启动和流式加载时间、GPU 驱动重置次数，并保存截图。
- 每个渲染通道的 GPU 开销、每个线程的帧阶段，以及带栈采样的卡顿日志。
- 着色器翻译器和 GPU 宿主被拆分为更小的模块；翻译器在游戏全部 26,148 个着色器程序上的输出保持逐字节一致。

## 已知问题

- **每次运行都可能不同的画面错误**：在某些运行中，Lucia 在餐馆时的上衣纹理错误、人脸发黑或出现斑块，或者在监狱窗边头发变成橙色。从其他分支移植的修复正在测试中。
- **流式加载缓慢**：进入世界后，所有资源到齐大约需要一分钟，在此期间会出现物体突然弹出的现象。
- **GPU 等待 CPU**：每帧 6-10 ms（见上文）。
- **光线追踪光照基本关闭**：游戏在软件中构建其光线追踪结构，而这个构建器已被禁用。

## 测试中

工作并行编写，并在唯一的一块 GPU 上分批测试：每项改动都放在各自的 `VICTORY_*` 开关之后，每一批将所有新开关全开与全关的运行进行对比，再根据数据和截图决定每个开关保留还是放弃。

- **第 1 批（进行中）**：纹理和缓冲区的内存池、无锁纹理暂存、减少描述符重建、8 线程内存映射、加载时并行烘焙角色、游戏创建着色器后立即翻译、合并 GPU 提交、更廉价的别名复制，以及人脸和头发的修复。
- **第 2 批（下一批）**：以 2 MiB 分段管理游戏内存（上游的映射时间 55.5 s → 7.3 s）、将游戏缓冲区保留在 GPU 显存中（上游在 RTX 5090 上帧时间 −15 %）、DLSS 超分辨率、提高模拟器帧线程的优先级、离线预热着色器和管线，以及在 RT 核心上进行硬件光线追踪。

## 运行

你需要自己的游戏副本、Windows 10 或 11、支持 Shader Model 6.6 和网格着色器的 GPU，以及 Rust。

```powershell
tools\lancer_victory.ps1 -GameDir <游戏文件夹>
```

完整指南（安装、设置、故障排除）见 [docs/running-gta6.md](../running-gta6.md)。

> [!IMPORTANT]
> 本仓库不包含任何游戏文件、密钥或可执行文件，也不说明如何获取它们。

开发时，`tools/dev.py` 封装了常用步骤：

```powershell
python tools/dev.py build      # release 构建
python tools/dev.py gate       # 全部测试，出现任何警告即失败
python tools/dev.py prewarm    # 离线翻译全部着色器（约 45 s）
python tools/dev.py bench --quiet --seconds 200   # 带测量的游戏运行
python tools/dev.py train --plan plan.toml        # 运行一次批次计划
```

## 项目数据

| 数字 | 内容 |
|---|---|
| 1600+ 个自动化测试 | Python 工具和 Rust crate，由 `dev.py gate` 运行 |
| 26,148 个着色器程序 | 从 RDNA 2 翻译为 DXIL，缓存在磁盘上 |
| 0 个被拒绝的管线 | 在 RTX 5090 上 |

## 历史

项目是如何走到今天的，从在模拟器中加载第一批主机 DLL 到世界的第一帧：[docs/history.md](../history.md)。

## 参与贡献

欢迎提交 issue 和 pull request。提交 pull request 前请先运行 `python tools/dev.py gate`。规则见 [CONTRIBUTING.md](../../CONTRIBUTING.md)。讨论和测试报告：[Discord](https://discord.gg/TcX42vRyPU)。

## 致谢

- **El1xr**：Bone/Nevaa。
- **VIctory**：Marvisak 及其贡献者（[VIctory](https://github.com/Marvisak/VIctory)，GPL-3.0 或更高版本），以及组装它所用的各个分支的作者，其中包括 Traffic & zm0kie 的 Neptune (VIctory Edit.)。
- **AnyXboxX**：Looper_Salty 及其贡献者（[AnyXboxX](https://github.com/LooperSalty/AnyXboxX)），El1xr 即从该项目分支而来。
- **我们学习并移植过代码的 VIctory 分支**，其中包括 fxceless/victory-fork、Peppacodes、Belobug、stellarr-studio/kelvaroq、uiliannn/VItoriaBr、DoubleGhosty/cool_fox、avatarmanz、3cvil/M1LK、trustno1-coder、TaynanGT/Vice-Tay、Linox-IDK/LNX-victory 和 readmet3xt-lab/foxhound。
- 每个组件和作者都按每次改动在 [THIRD_PARTY.md](../../THIRD_PARTY.md) 中注明。

## 许可证

El1xr 以 [GNU GPL 3.0 或更高版本](../../LICENSE) 发布。第三方部分保留各自的许可证：libHttpClient 的 `Task` 模块（MIT，`runtime/third_party/libhttpclient-task`），以及根据 LLVM 公开编码测试生成的 RDNA 2 表（Apache 2.0 with LLVM exception，`PrototypeScarlett/emu/rdna2_ops.py` 和 `rdna2_shapes.py`）。
