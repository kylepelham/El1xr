<div align="center">

# El1xr

**Grand Theft Auto VI（Xbox Series、ビルド 1.0.2.1）を Windows PC で動かす。**<br>
RTX 5090 で 4K、約 25 fps、さらに向上中。

[![ライセンス: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](../../LICENSE)
[![プラットフォーム: Windows](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078D6?logo=windows&logoColor=white)](../running-gta6.md)
[![Rust 製](https://img.shields.io/badge/built%20with-Rust-B7410E?logo=rust&logoColor=white)](../../third_party/victory)
[![Discord](https://img.shields.io/badge/Discord-AnyXboxX-5865F2?logo=discord&logoColor=white)](https://discord.gg/TcX42vRyPU)

[English](../../README.md) · [Français](README.fr.md) · [Español](README.es.md) · [Deutsch](README.de.md) · [Italiano](README.it.md) · [Português](README.pt-BR.md) · **日本語** · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

---

El1xr は Bone/Nevaa による GTA VI エミュレーションプロジェクトです。[AnyXboxX](https://github.com/LooperSalty/AnyXboxX) のフォークであり、[VIctory](https://github.com/Marvisak/VIctory) と、VIctory の各フォークの優れた成果を土台にしています（すべての出典は [THIRD_PARTY.md](../../THIRD_PARTY.md) に記載）。ゲーム自身の x86-64 コードはネイティブに実行され、コンソールのシステムコール、Direct3D 12.X のコマンド、RDNA 2 シェーダーはその場で PC 向けに変換されます。

> [!NOTE]
> **現状（2026 年 11 月 27 日）**
>
> - 🎮 **RTX 5090 で、ワールドは 4K・安定後 24-25 fps でプレイ可能**（他の負荷のない状態でのベンチマーク 3 回：24.9、25.2、24.2 fps）。目標は安定した 30 fps です。
> - ✅ ベンチマーク実行で **GPU パイプラインの拒否は 0**、GPU デバイスの喪失もありません。
> - ✅ カットシーンは最後まで再生され、最初のミッションはクリアまでプレイ済みです。ワールドの最初のフレームは起動から 46-52 秒で表示されます。
> - 🚧 さらに約 **60 件の高速化が実装済みでテスト中**です（[テスト中](#テスト中)を参照）。

## 概要

| 指標（RTX 5090、4K） | 現在 | 目標 |
|---|---|---|
| 安定後のフレームレート | **24-25 fps** | 30+ fps |
| フレーム時間の中央値 | ~40 ms | ≤ 33.3 ms |
| 99 パーセンタイルのフレーム | 68-73 ms | カクつきなし |
| 1 フレームの準備にかかる CPU 時間 | 25-26 ms | GPU の時間未満 |
| 拒否された GPU パイプライン | **0** | 0 |
| 起動からワールドの最初のフレームまで | 46-52 s | |
| ワールドに入った後のストリーミング | 52-63 s | 数秒 |

計測方法：`python tools/dev.py bench --quiet --seconds 200`。固定のチェックポイントから高速起動し、マシン上で他に何も動かさず、エミュレーター自身のフレームログから数値を読み取っています。

## テスト環境

| パーツ | モデル |
|---|---|
| GPU | NVIDIA GeForce RTX 5090、32 GB、ドライバー 621.24 |
| CPU | AMD Ryzen 9 9950X3D、16 コア / 32 スレッド |
| メモリ | 64 GB DDR5-6000 |
| マザーボード | MSI PRO B650-P WIFI |
| ストレージ | NVMe SSD（Samsung 990 EVO Plus） |
| OS | Windows 11（ビルド 26200） |
| ゲーム | Xbox Series 版 GTA VI、ビルド 1.0.2.1（ご自身のコピー） |

## 1 フレームの内訳

![1 フレームあたりの GPU 時間：ゲームのパス 22.8 ms、エミュレーターの処理 7.0 ms、未計測 3.8 ms、アイドル 6.2 ms](../images/frame-budget.svg)

安定後の 1 フレームは約 40 ms かかります。ゲーム自身の描画パスが使う GPU 時間は 22.8 ms なので、30 fps は十分に手の届く範囲です。残りの内訳は次のとおりです。

- **私たち自身の GPU 処理**（7 ms）：間接描画の範囲チェック、フォーマット間のテクスチャコピー、GPU が書き込んだ内容の検査。
- **GPU のアイドル**（6-10 ms）：CPU の 1 スレッドが次の処理バッチを準備している間、グラフィックカードが待たされています。

テスト中の作業は、この 2 つを狙ったものです。

## 今週の変更

**修正**
- 刑務所の窓の霜が、ルシアの髪の上に描画されなくなりました。
- 日光の下で「あらゆるものに線が入る」問題が解消しました。シェーダートランスレーターが、64 レーンの wave の前半と後半で実際には異なる一部の値を、同じ値として扱っていました（31 シェーダー）。
- ダイナーのガラスのまだら模様が解消しました。レイトレーシングによるガラスのパスは、スキップされる代わりに「ヒットなし」として実行されるようになりました。
- オーディオ：より安全な XMA デコードと、クリーンなミュート・終了処理。

**高速化**
- テクスチャのアップロードをゲームの描画スレッドから移し、テクスチャのチェックを並列で実行し、解放されたリストを専用スレッドで破棄するようにしました。ワールドに入るときの最悪のフリーズは 9.5 秒から 6.6 秒に短縮されました。
- CPU 側でのドライバーによるテクスチャの確保は、このマシンで 382-529 µs かかりますが、事前に用意したメモリブロックからなら 5-8 µs で済みます。テクスチャとバッファーのプール化をテスト中です。

**ツール**
- テスト実行用の高速起動：イントロ動画をスキップし、固定したチェックポイントから開始します。
- すべてのベンチマークが、fps、フレーム時間のパーセンタイル、100 ms と 250 ms を超えるカクつき、起動時間とストリーミング時間、GPU ドライバーのリセットを報告し、スクリーンショットを保存します。
- パスごとの GPU コスト、スレッドごとのフレームフェーズ、スタックサンプル付きのカクつきログ。
- シェーダートランスレーターと GPU ホストを小さなモジュールに分割しました。トランスレーターの出力は、ゲームの全 26,148 個のシェーダープログラムでバイト単位まで同一のままです。

## 既知の問題

- **実行ごとに変わる表示の不具合**：実行によっては、ダイナーでのルシアのトップスのテクスチャが間違っていたり、顔が黒くなったりまだらになったり、刑務所の窓で髪がオレンジ色になったりします。他のフォークから移植した修正をテスト中です。
- **遅いストリーミング**：ワールドに入ってからすべてのアセットが揃うまで約 1 分かかり、その間はポップインが発生します。
- **CPU を待つ GPU**：1 フレームあたり 6-10 ms（上記参照）。
- **レイトレーシングによるライティングはほぼオフ**：ゲームはレイトレーシング用の構造をソフトウェアで構築しており、そのビルダーは無効になっています。

## テスト中

作業は並行して書かれ、1 枚しかない GPU でバッチ単位でテストされます。すべての変更はそれぞれ専用の `VICTORY_*` スイッチの背後に置かれ、各バッチでは新しいスイッチをすべてオンにした実行とすべてオフにした実行を比較し、数値とスクリーンショットをもとにスイッチごとに採用か破棄かを決めます。

- **バッチ 1（実行中）**：テクスチャとバッファーのメモリプール、ロックなしのテクスチャステージング、ディスクリプターの再構築の削減、8 スレッドでのメモリマッピング、ロード時のキャラクターベイクの並列化、ゲームがシェーダーを作成した直後の変換、GPU サブミッションの統合、より軽量なエイリアスコピー、そして顔と髪の修正。
- **バッチ 2（次回）**：タイトルメモリの 2 MiB 単位のセクション化（アップストリームでマッピング時間 55.5 s → 7.3 s）、ゲームのバッファーを GPU メモリに保持（アップストリームで RTX 5090 においてフレーム時間 −15 %）、DLSS アップスケーリング、エミュレーターのフレームスレッドの優先度引き上げ、シェーダーとパイプラインのオフライン事前ウォームアップ、RT コアによるハードウェアレイトレーシング。

## 実行方法

ご自身のゲームのコピー、Windows 10 または 11、Shader Model 6.6 とメッシュシェーダーに対応した GPU、そして Rust が必要です。

```powershell
tools\lancer_victory.ps1 -GameDir <ゲームのフォルダー>
```

完全なガイド（インストール、設定、トラブルシューティング）は [docs/running-gta6.md](../running-gta6.md) にあります。

> [!IMPORTANT]
> このリポジトリにはゲームファイル、鍵、実行ファイルは含まれておらず、それらの入手方法も説明していません。

開発では、`tools/dev.py` がよく使う手順をまとめています。

```powershell
python tools/dev.py build      # リリースビルド
python tools/dev.py gate       # 全テスト、警告が 1 つでもあれば失敗
python tools/dev.py prewarm    # 全シェーダーをオフラインで変換（約 45 s）
python tools/dev.py bench --quiet --seconds 200   # 計測付きのゲーム実行
python tools/dev.py train --plan plan.toml        # バッチプランの 1 回の実行
```

## 数字で見るプロジェクト

| 数値 | 内容 |
|---|---|
| 1600+ 件の自動テスト | Python のツールと Rust のクレート、`dev.py gate` で実行 |
| 26,148 個のシェーダープログラム | RDNA 2 から DXIL に変換し、ディスクにキャッシュ |
| 0 件の拒否されたパイプライン | RTX 5090 上 |

## 沿革

最初のコンソール DLL をエミュレーターに読み込んだところから、ワールドの最初のフレームまで、プロジェクトの歩み：[docs/history.md](../history.md)。

## 貢献

issue とプルリクエストを歓迎します。プルリクエストを開く前に `python tools/dev.py gate` を実行してください。ルールは [CONTRIBUTING.md](../../CONTRIBUTING.md) にあります。議論とテスト報告：[Discord](https://discord.gg/TcX42vRyPU)。

## クレジット

- **El1xr**：Bone/Nevaa。
- **VIctory**：Marvisak とそのコントリビューター（[VIctory](https://github.com/Marvisak/VIctory)、GPL-3.0 以降）、および VIctory の組み立てに使われたフォークの作者たち。その中には Traffic & zm0kie による Neptune (VIctory Edit.) も含まれます。
- **AnyXboxX**：Looper_Salty とそのコントリビューター（[AnyXboxX](https://github.com/LooperSalty/AnyXboxX)）。El1xr のフォーク元のプロジェクトです。
- **私たちが学び、移植元とした VIctory のフォーク**：fxceless/victory-fork、Peppacodes、Belobug、stellarr-studio/kelvaroq、uiliannn/VItoriaBr、DoubleGhosty/cool_fox、avatarmanz、3cvil/M1LK、trustno1-coder、TaynanGT/Vice-Tay、Linox-IDK/LNX-victory、readmet3xt-lab/foxhound など。
- すべてのコンポーネントと作者は、変更ごとに [THIRD_PARTY.md](../../THIRD_PARTY.md) に記載されています。

## ライセンス

El1xr は [GNU GPL 3.0 以降](../../LICENSE) の下で配布されています。サードパーティの部分はそれぞれ独自のライセンスを保持します：libHttpClient の `Task` モジュール（MIT、`runtime/third_party/libhttpclient-task`）と、LLVM の公開エンコーディングテストから生成された RDNA 2 のテーブル（Apache 2.0 with LLVM exception、`PrototypeScarlett/emu/rdna2_ops.py` と `rdna2_shapes.py`）。
