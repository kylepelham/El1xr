<div align="center">

# El1xr

**Grand Theft Auto VI (Xbox Series, Build 1.0.2.1) läuft auf einem Windows-PC.**<br>
In 4K, etwa 25 fps und steigend auf einer RTX 5090.

[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](../../LICENSE)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078D6?logo=windows&logoColor=white)](../running-gta6.md)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-B7410E?logo=rust&logoColor=white)](../../third_party/victory)
[![Discord](https://img.shields.io/badge/Discord-AnyXboxX-5865F2?logo=discord&logoColor=white)](https://discord.gg/TcX42vRyPU)

[English](../../README.md) · [Français](README.fr.md) · [Español](README.es.md) · **Deutsch** · [Italiano](README.it.md) · [Português](README.pt-BR.md) · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

---

El1xr ist das GTA-VI-Emulationsprojekt von Bone/Nevaa: ein Fork von [AnyXboxX](https://github.com/LooperSalty/AnyXboxX), aufgebaut auf [VIctory](https://github.com/Marvisak/VIctory) und auf der besten Arbeit der VIctory-Forks (jede Quelle ist in [THIRD_PARTY.md](../../THIRD_PARTY.md) genannt). Der x86-64-Code des Spiels selbst läuft nativ; seine Konsolen-Systemaufrufe, Direct3D-12.X-Befehle und RDNA-2-Shader werden zur Laufzeit für den PC übersetzt.

> [!NOTE]
> **Stand: 27. November 2026**
>
> - 🎮 **Die Spielwelt ist in 4K spielbar, mit 24-25 fps nach dem Einpendeln** auf einer RTX 5090 (drei Benchmark-Läufe auf ruhigem Rechner: 24,9; 25,2 und 24,2 fps). Ziel sind stabile 30 fps.
> - ✅ **0 abgelehnte GPU-Pipelines** und kein Verlust des GPU-Geräts in den Benchmark-Läufen.
> - ✅ Zwischensequenzen laufen vollständig, und die ersten Missionen wurden durchgespielt.
> - ⚡ **Stapel 1 übernommen**: Vom Start bis zum ersten Bild der Spielwelt jetzt etwa 31-33 s (vorher 41-54 s), das schlimmste Einfrieren beim Betreten der Spielwelt 1,5-1,9 s (vorher 7,6-8,1 s).
> - 🚧 Rund **60 weitere Beschleunigungen sind geschrieben und in Erprobung** (siehe [In Erprobung](#in-erprobung)).

## Auf einen Blick

| Messgröße (RTX 5090, 4K) | Jetzt | Ziel |
|---|---|---|
| Eingependelte Bildrate | **24-25 fps** | 30+ fps |
| Median der Frametime | ~40 ms | ≤ 33,3 ms |
| Frame im 99. Perzentil | 68-73 ms | keine Ruckler |
| CPU-Zeit zur Vorbereitung eines Frames | 25-26 ms | unter der GPU-Zeit |
| Abgelehnte GPU-Pipelines | **0** | 0 |
| Vom Start bis zum ersten Bild der Spielwelt | ~31-33 s | |
| Streaming nach dem Betreten der Spielwelt | 52-63 s | wenige Sekunden |

Gemessen mit `python tools/dev.py bench --quiet --seconds 200`: Schnellstart von einem festen Checkpoint, sonst läuft nichts auf dem Rechner, Werte aus dem eigenen Frame-Log des Emulators. Die Startzeit stammt aus den Läufen von Stapel 1, die liefen, während anderes kompiliert wurde; auf einem ruhigen Rechner sollte sie etwas besser ausfallen.

## Testrechner

| Komponente | Modell |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, Treiber 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 Kerne / 32 Threads |
| Arbeitsspeicher | 64 GB DDR5-6000 |
| Mainboard | MSI PRO B650-P WIFI |
| Speicher | NVMe-SSDs (Samsung 990 EVO Plus) |
| Betriebssystem | Windows 11 (Build 26200) |
| Spiel | GTA VI für Xbox Series, Build 1.0.2.1 (deine eigene Kopie) |

## Wohin die Zeit eines Frames geht

![GPU-Zeit pro Frame: Spiel-Passes 22,8 ms, Emulator-Arbeit 7,0 ms, nicht erfasst 3,8 ms, Leerlauf 6,2 ms](../images/frame-budget.svg)

Ein eingependelter Frame dauert etwa 40 ms. Die Render-Passes des Spiels selbst brauchen 22,8 ms GPU-Zeit, 30 fps sind also in Reichweite. Der Rest entfällt auf:

- **unsere eigene GPU-Arbeit** (7 ms): Bereichsprüfungen bei indirekten Draws, Texturkopien zwischen Formaten, Prüfungen dessen, was die GPU geschrieben hat;
- **GPU-Leerlauf** (6-10 ms): Die Karte wartet, während ein einzelner CPU-Thread den nächsten Arbeitsstapel vorbereitet.

Genau darauf zielt die Arbeit, die gerade erprobt wird.

## Was sich diese Woche geändert hat

**Behoben**
- Der Frost vom Gefängnisfenster wird nicht mehr über Lucias Haare gezeichnet.
- Die „Linien auf allem“ im Sonnenlicht sind verschwunden: Der Shader-Übersetzer behandelte einige Werte in beiden Hälften einer 64-Lane-Wave als gleich, obwohl sie es nicht waren (31 Shader).
- Die Flecken auf dem Glas des Diners sind verschwunden: Der Raytracing-Glas-Pass läuft jetzt als „kein Treffer“, statt übersprungen zu werden.
- Audio: sichereres XMA-Decoding und ein sauberer Weg für Stummschalten und Herunterfahren.

**Schneller**
- Textur-Uploads wurden aus den Zeichen-Threads des Spiels ausgelagert, Texturprüfungen laufen parallel, und freigegebene Listen werden in einem eigenen Thread abgebaut: Das schlimmste Einfrieren beim Betreten der Spielwelt sank von 9,5 s auf 6,6 s.
- Die CPU-seitige Treiber-Allokation einer Textur kostet auf diesem Rechner 382-529 µs, aus einem vorab angelegten Speicherblock 5-8 µs; Pools für Texturen und Buffer sind in Erprobung.

**Werkzeuge**
- Schnellstart für Testläufe: überspringt das Intro-Video und startet von einem eingefrorenen Checkpoint.
- Jeder Bench meldet fps, Frametime-Perzentile, Ruckler über 100 und 250 ms, Start- und Streaming-Zeiten sowie Resets des GPU-Treibers und speichert Screenshots.
- GPU-Kosten pro Pass, Frame-Phasen pro Thread und ein Ruckler-Log mit Stack-Samples.
- Der Shader-Übersetzer und der GPU-Host wurden in kleinere Module aufgeteilt; die Ausgabe des Übersetzers blieb bei allen 26.148 Shader-Programmen des Spiels Byte für Byte identisch.

## Bekannte Probleme

- **Grafikfehler, die von Lauf zu Lauf wechseln**: In manchen Läufen hat Lucias Oberteil im Diner die falsche Textur, Gesichter sind schwarz oder fleckig, oder die Haare werden am Gefängnisfenster orange. Aus anderen Forks portierte Korrekturen sind in Erprobung.
- **Langsames Streaming**: Nach dem Betreten der Spielwelt dauert es etwa eine Minute, bis alle Assets geladen sind, bis dahin ploppen Objekte auf.
- **GPU wartet auf die CPU**: 6-10 ms pro Frame (siehe oben).
- **Raytracing-Beleuchtung ist größtenteils aus**: Das Spiel baut seine Raytracing-Strukturen in Software, und dieser Builder ist deaktiviert.

## In Erprobung

Die Arbeit wird parallel geschrieben und in Stapeln auf der einen GPU getestet: Jede Änderung steckt hinter ihrem eigenen `VICTORY_*`-Schalter, ein Stapel lässt alle neuen Schalter eingeschaltet gegen alle ausgeschaltet laufen, und jeder Schalter wird anhand der Zahlen und der Screenshots behalten oder verworfen.

- **Stapel 1 (fertig, 20 Schalter behalten)**: Textur-Staging ohne Sperren, weniger Neuaufbauten von Deskriptoren und weniger Warten auf Sperren, paralleles Vorberechnen der Figuren beim Laden, Shader-Übersetzung, sobald das Spiel die Shader anlegt, schnelleres Laden der Pipeline-Bibliotheken, schlankeres Speicher-Mapping sowie die Korrekturen für Outfits, Gesichter und Haare (Lucias Oberteil im Diner stimmte in 4 von 5 Läufen, vorher in 0 von 2). Vorerst verworfen: die Speicherpools für Texturen und Buffer (sie verlangsamten das Unmapping von Speicher) und zwei Schalter, die abstürzten; ihre Autoren beheben das.
- **Stapel 2 (läuft)**: Titelspeicher in 2-MiB-Abschnitten (Mapping-Zeit upstream 55,5 s → 7,3 s), Spiel-Buffer im GPU-Speicher gehalten (upstream −15 % Frametime auf einer RTX 5090), DLSS-Upscaling, höhere Priorität für die Frame-Threads des Emulators, Offline-Vorwärmen von Shadern und Pipelines sowie Hardware-Raytracing auf den RT-Kernen.

## Ausführen

Du brauchst deine eigene Kopie des Spiels, Windows 10 oder 11, eine GPU mit Shader Model 6.6 und Mesh-Shadern sowie Rust.

```powershell
tools\lancer_victory.ps1 -GameDir <Spielordner>
```

Die vollständige Anleitung (Installation, Einstellungen, Fehlerbehebung) steht in [docs/running-gta6.md](../running-gta6.md).

> [!IMPORTANT]
> Dieses Repository enthält keine Spieldateien, Schlüssel oder ausführbaren Dateien und erklärt nicht, wie man sie bekommt.

Für die Entwicklung bündelt `tools/dev.py` die üblichen Schritte:

```powershell
python tools/dev.py build      # Release-Build
python tools/dev.py gate       # alle Tests, schlägt bei jeder Warnung fehl
python tools/dev.py prewarm    # alle Shader offline übersetzen (etwa 45 s)
python tools/dev.py bench --quiet --seconds 200   # gemessener Spiellauf
python tools/dev.py train --plan plan.toml        # ein Lauf eines Stapelplans
```

## Das Projekt in Zahlen

| Zahl | Was |
|---|---|
| 1600+ automatisierte Tests | Python-Werkzeuge und Rust-Crates, ausgeführt von `dev.py gate` |
| 26.148 Shader-Programme | von RDNA 2 nach DXIL übersetzt, auf der Festplatte zwischengespeichert |
| 0 abgelehnte Pipelines | auf der RTX 5090 |

## Geschichte

Wie das Projekt hierher kam, von den ersten in einen Emulator geladenen Konsolen-DLLs bis zu den ersten Bildern der Spielwelt: [docs/history.md](../history.md).

## Mitwirken

Issues und Pull Requests sind willkommen. Führe `python tools/dev.py gate` aus, bevor du einen Pull Request öffnest. Die Regeln stehen in [CONTRIBUTING.md](../../CONTRIBUTING.md). Diskussion und Testberichte: [Discord](https://discord.gg/TcX42vRyPU).

## Danksagungen

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak und seine Mitwirkenden ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 oder später) sowie die Autoren der Forks, aus denen es zusammengesetzt wurde, darunter Neptune (VIctory Edit.) von Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty und seine Mitwirkenden ([AnyXboxX](https://github.com/LooperSalty/AnyXboxX)), das Projekt, von dem El1xr abgespalten wurde.
- **Die VIctory-Forks, von denen wir gelernt und Code portiert haben**, darunter fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory und readmet3xt-lab/foxhound.
- Jede Komponente und jeder Autor ist, Änderung für Änderung, in [THIRD_PARTY.md](../../THIRD_PARTY.md) genannt.

## Lizenz

El1xr wird unter der [GNU GPL 3.0 oder später](../../LICENSE) verbreitet. Teile von Dritten behalten ihre eigene Lizenz: das `Task`-Modul von libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) und die RDNA-2-Tabellen, die aus den öffentlichen Encoding-Tests von LLVM erzeugt wurden (Apache 2.0 mit LLVM-Ausnahme, `PrototypeScarlett/emu/rdna2_ops.py` und `rdna2_shapes.py`).
