<div align="center">

![El1xr: Grand Theft Auto VI für Xbox Series auf einem Windows-PC. 23,5 fps in der Spielwelt in 4K auf einer RTX 5090, 0 abgelehnte Pipelines, 32 s vom Start bis zur Spielwelt, spielbar seit dem 27. November 2026](../images/hero.svg)

![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-9b5cff?style=flat-square&labelColor=170f2b)
![Platform: Windows 10 and 11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2de2e6?style=flat-square&labelColor=170f2b)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-ff8a3d?style=flat-square&labelColor=170f2b&logo=rust&logoColor=white)
![Status: playable](https://img.shields.io/badge/status-playable-ff3d8b?style=flat-square&labelColor=170f2b)
![23.5 fps at 4K on RTX 5090](https://img.shields.io/badge/4K%20on%20RTX%205090-23.5%20fps-ffd36e?style=flat-square&labelColor=170f2b)
![Discord: bonemeld](https://img.shields.io/badge/Discord-bonemeld-5865F2?style=flat-square&labelColor=170f2b&logo=discord&logoColor=white)

[English](../../README.md) · [Français](README.fr.md) · [Español](README.es.md) · **Deutsch** · [Italiano](README.it.md) · [Português](README.pt-BR.md) · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

El1xr führt die Xbox-Series-Version von Grand Theft Auto VI (Build 1.0.2.1) auf einem Windows-PC aus. Es ist das Projekt von Bone/Nevaa. Der x86-64-Code des Spiels selbst läuft nativ auf deinem Prozessor. Seine Konsolen-Systemaufrufe, Direct3D-12.X-Befehle und RDNA-2-Shader werden zur Laufzeit für den PC übersetzt. Es gibt keine CPU-Emulation und keine Spielinhalte in diesem Repository: Du bringst deine eigene Kopie mit.

## Status

> [!NOTE]
> **Stand 27. November 2026: GTA VI (Xbox Series, Build 1.0.2.1) startet, erreicht die Menüs und ist unter Windows in der offenen Welt spielbar.** In den jüngsten Testläufen auf ruhigem Rechner (Batch 3, 4K, RTX 5090) läuft die Spielwelt im Median mit 23,5 fps, bei 0 abgelehnten GPU-Pipelines. Das liegt noch unter dem nächsten Ziel, stabilen 30 fps. Danach folgen: keine Ruckler und keine Grafikfehler.

![Meilensteine: Start, Hauptmenü und Spielwelt bis 24. November 2026; spielbar mit 23,5 fps am 27. November 2026; als Nächstes 30 fps; Stabilität in Arbeit](../images/milestones.svg)

| Meilenstein | Status | Beleg | Datum |
|---|---|---|---|
| Programm läuft | Erledigt | Die ausführbare Datei der Xbox-Series-Version 1.0.2.1 startet unter Windows. Ihr x86-64-Code läuft nativ. | bis 24. Nov. 2026 |
| Erstes Bild | Erledigt | Die Startanimation und die rechtlichen Hinweise werden ohne Flackern dargestellt. | bis 24. Nov. 2026 |
| Hauptmenü | Erledigt | Profilbildschirm, Ersteinrichtung, rechtliche Hinweise, Ladebildschirm und Pausenmenü funktionieren. Die Menüs laufen mit 30 fps (RTX 4070 SUPER). | bis 24. Nov. 2026 |
| Im Spiel | Erledigt | Das Spiel erreicht die offene Welt, und die Figur lässt sich steuern. Eine 13-minütige Sitzung im Gefängnis lief mit etwa 4 fps (RTX 4070 SUPER). | bis 24. Nov. 2026 |
| Spielbar | Erledigt, unter der Zielgeschwindigkeit | Die Spielwelt läuft in 4K mit 23,5 fps (RTX 5090; Median des Abschnitts in der Spielwelt eines 200-Sekunden-Laufs aus Batch 3 auf ruhigem Rechner). | 27. Nov. 2026 |
| Story-Fortschritt | Erste zwei Missionen abgeschlossen | Ein Mitwirkender hat die ersten beiden Missionen in einer 45-minütigen Sitzung mit etwa 2 fps abgeschlossen (RTX 4070 SUPER; gemeldet, nicht aus Logs gemessen). Alle Zwischensequenzen laufen vollständig. Unter Linux (Proton, RX 9070 XT) kam ein 23-minütiger Lauf über den Prolog hinaus bis in Kapitel 1. | 25. Nov. 2026 |
| 30 fps | Noch nicht | 23,5 fps in der Spielwelt, Median der Frametime 42 ms gegenüber dem Ziel von 33,3 ms (RTX 5090, 4K). | 27. Nov. 2026 |
| Stabil | In Arbeit | Mit den Standardeinstellungen enden 200-Sekunden-Testläufe ohne Absturz, ohne Reset des GPU-Treibers und ohne Geräteverlust. Beim Betreten der Spielwelt steht das Spiel noch 3-9 s still ([#78](https://github.com/kylepelham/El1xr/issues/78)), und Grafikfehler wechseln noch von Lauf zu Lauf (siehe Bekannte Probleme). | 27. Nov. 2026 |

## Leistung

![Auf einen Blick: 23,5 fps in der Spielwelt, 0 abgelehnte GPU-Pipelines, 32 s vom Start bis zur Spielwelt, 7 Ruckler über 1 s pro Lauf](../images/stats.svg)

| Messgröße (RTX 5090, 4K) | Jetzt | Ziel |
|---|---|---|
| Bildrate in der Spielwelt (Median) | **23,5 fps** | 30+ fps |
| Median der Frametime | 42 ms | höchstens 33,3 ms |
| Frame im 99. Perzentil im eingependelten Spiel | ~70 ms | keine Ruckler |
| CPU-Zeit zur Vorbereitung eines Frames | 28 ms | unter der GPU-Zeit |
| Abgelehnte GPU-Pipelines | **0** | 0 |
| Vom Start bis zum ersten Bild der Spielwelt | 32 s | |
| Vom Start bis zum eingependelten Spiel | 49 s | |
| Stillstand beim Betreten der Spielwelt | 3-9 s ([#78](https://github.com/kylepelham/El1xr/issues/78)) | keiner |
| Frames über 250 ms in der Spielwelt | 20 pro Lauf | keine |
| Ruckler über 1 s | 7 pro Lauf | keine |
| Streaming nach dem Betreten der Spielwelt | 74 s | wenige Sekunden |

Diese Werte stammen aus den Läufen von Batch 3 auf ruhigem Rechner (`python tools/dev.py bench --quiet --seconds 200`, 27. November 2026). Jeder Lauf startet per Schnellstart von einem festen Checkpoint, während die übrige Arbeit auf dem Rechner pausiert, und die Werte werden aus dem eigenen Frame-Log des Emulators gelesen. In jedem Lauf war ein Teil der Diagnose-Logs eingeschaltet. Der Stillstand beim Betreten der Spielwelt ist die Spanne über die Läufe von Batch 3.

![Wohin ein Frame geht: Spiel-Passes 22,8 ms, GPU-Arbeit von El1xr 7,0 ms, nicht erfasst 3,8 ms, GPU-Leerlauf 6,2 ms, gemessen an der 33,3-ms-Linie für 30 fps](../images/frame-budget.svg)

Das letzte GPU-Zeitprofil, aufgenommen vor Batch 3, ergab etwa 40 ms für einen eingependelten Frame. Die Render-Passes des Spiels selbst brauchen 22,8 ms GPU-Zeit, 30 fps sind also in Reichweite. Der Rest entfällt auf die eigene GPU-Arbeit von El1xr (7 ms: Bereichsprüfungen bei indirekten Draws, Texturkopien zwischen Formaten, Uploads) und auf Leerlauf der GPU (6-10 ms), während ein einzelner CPU-Thread den nächsten Arbeitsstapel vorbereitet. Genau diese beiden Posten nehmen sich die nächsten Updates vor.

![Batch 3: Mit gepoolten Blöcken sank die Zeit vom Start bis zum ersten Bild der Spielwelt von 58-67 s auf 32 s, vom Start bis zum eingependelten Spiel von 117-120 s auf 49 s, und die Frames über 250 ms in der Spielwelt von 51-74 auf 20](../images/load-times.svg)

Batch 3 (27. November 2026) hat zwei Gruppen von Schaltern behalten, die der Launcher jetzt standardmäßig einschaltet: Speicher in 2-MiB-Abschnitten, Spiel-Buffer im GPU-Speicher gehalten und höhere Priorität für die Frame-Threads (die fg_rt-Gruppe aus sieben Schaltern) sowie Staging-, GPU-, gemeinsame und Upload-Buffer aus gepoolten Blöcken (vier Schalter). Die 4-KiB-Schreibwächter sind ebenfalls an; sie erwiesen sich als neutral. Gegenüber demselben Build ohne gepoolte Blöcke, direkt nacheinander gemessen, sank die Zeit vom Start bis zum ersten Bild der Spielwelt von 58-67 s auf 32 s, bis zum eingependelten Spiel von 117-120 s auf 49 s, die Frames über 250 ms in der Spielwelt von 51-74 auf 20, die Ruckler über 1 s von 26-35 auf 7 und das Streaming von 80-87 s auf 74 s. Eine Gruppe aus sieben Schaltern, die die Wände der Gefängniszelle zerriss, wurde verworfen. Der Lauf des Batches ganz ohne diese Schalter kam auf 14 fps und einen Start in 100 s, doch der Rechner war dabei stärker ausgelastet, daher ist das kein sauberer Vorher-nachher-Vergleich.

### Testrechner

| Komponente | Modell |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, Treiber 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 Kerne / 32 Threads |
| Arbeitsspeicher | 64 GB DDR5-6000 |
| Mainboard | MSI PRO B650-P WIFI |
| Speicher | NVMe-SSDs (Samsung 990 EVO Plus) |
| Betriebssystem | Windows 11 (Build 26200) |
| Spiel | GTA VI für Xbox Series, Build 1.0.2.1 (deine eigene Kopie) |

Alle aktuellen Werte stammen von diesem Rechner. Ältere Werte in diesem README wurden auf einer RTX 4070 SUPER (Windows) oder einer RX 9070 XT (Linux) gemessen, und das ist jeweils angegeben.

## Funktionsweise

![Funktionsweise: Der x86-64-Code des Spiels läuft nativ auf der CPU; Konsolen-Systemaufrufe gehen an den HLE-Kernel unter Windows; D3D12.X-Befehle werden als Direct3D 12 des Host-Systems neu aufgebaut; RDNA-2-Shader werden nach DXIL übersetzt und zwischengespeichert](../images/how-it-works.svg)

- **Spielcode** wird unverändert geladen und ausgeführt. Xbox Series und Windows-PC teilen sich den x86-64-Befehlssatz, daher wird kein einziger Befehl emuliert.
- **Systemaufrufe** an den Konsolen-Kernel und die GDK-Bibliotheken bedient die High-Level-Emulation (HLE) von El1xr auf Basis von Windows: Speicher, Threads, Dateien und Systemdienste.
- **Grafik**: Die Direct3D-12.X-Befehlslisten, der Speicher und die Ressourcen der Konsole werden als Standard-Direct3D-12 auf deiner GPU neu aufgebaut.
- **Shader**, die für die RDNA-2-GPU der Konsole kompiliert wurden, werden nach DXIL übersetzt. Alle 26.148 Shader-Programme des Spiels lassen sich übersetzen, und jedes wird nach seiner ersten Verwendung auf der Festplatte zwischengespeichert.

## Ausführen

Du brauchst deine eigene Kopie des Spiels, Windows 10 oder 11, eine GPU mit Shader Model 6.6 und Mesh-Shadern sowie Rust.

```powershell
tools\lancer_victory.ps1 -GameDir <Spielordner>
```

Die vollständige Anleitung (Installation, Einstellungen, Fehlerbehebung) steht in [docs/running-gta6.md](../running-gta6.md).

> [!IMPORTANT]
> Dieses Repository enthält keine Spieldateien, Schlüssel oder ausführbaren Dateien und erklärt nicht, wie man sie bekommt.

<details>
<summary>Befehle für Entwickler</summary>

```powershell
python tools/dev.py build      # Release-Build
python tools/dev.py prewarm    # alle Shader offline übersetzen (etwa 45 s)
python tools/dev.py bench --quiet --seconds 200   # gemessener Spiellauf
python tools/dev.py gate       # alle Tests, schlägt bei jeder Warnung fehl
```

</details>

## Bekannte Probleme

- **Grafikfehler, die von Lauf zu Lauf wechseln.** In etwa der Hälfte der Testläufe werden Gesichter schwarz oder fleckig (daran wird in [#35](https://github.com/kylepelham/El1xr/issues/35) gearbeitet), Lucia trägt im Diner das falsche Outfit, oder ihre Haare werden am Gefängnisfenster orange.
- **Ein Stillstand beim Betreten der Spielwelt.** Kurz nachdem die Spielwelt erscheint, friert das Spiel 3-9 s ein, während die GPU einen Stapel von Befehlen abarbeitet. Die Ursache wird in [#78](https://github.com/kylepelham/El1xr/issues/78) untersucht.
- **Langsames Streaming.** Nach dem Betreten der Spielwelt dauert es etwa 74 s, bis alle Assets geladen sind; bis dahin ploppen Objekte auf.
- **Die GPU wartet auf die CPU**, 6-10 ms pro Frame (siehe die Aufteilung eines Frames oben).
- **Raytracing-Beleuchtung ist größtenteils aus.** Das Spiel baut seine Raytracing-Strukturen in Software, und dieser Builder ist vorerst deaktiviert.
- **Ein einziger Referenzrechner.** Die aktuellen Werte stammen von der oben genannten RTX 5090. Andere GPUs wurden mit dem aktuellen Build noch nicht gemessen.

## Wie es weitergeht

Ziel sind stabile 30 fps in 4K auf der RTX 5090, danach keine Ruckler und keine Grafikfehler. Rund 60 weitere Beschleunigungen sind geschrieben und warten darauf, gemessen zu werden. Jede bleibt standardmäßig ausgeschaltet, bis sie sich bewährt hat.

- **Der Stillstand beim Betreten der Spielwelt** ([#78](https://github.com/kylepelham/El1xr/issues/78)): herausfinden, welcher Pass in diesem Stapel von GPU-Befehlen Sekunden braucht.
- **Weniger Warten und weniger Overhead**: die 6-10 ms verkürzen, die die GPU in jedem Frame auf die CPU wartet, sowie die 7 ms eigene GPU-Arbeit von El1xr.
- **DLSS-Upscaling** und **Hardware-Raytracing auf den RT-Kernen**, womit die Raytracing-Beleuchtung zurückkäme.
- **Offline-Vorwärmen von Shadern und Pipelines**, damit während des Spielens kein Shader kompiliert wird.
- **Korrekturen für die schwarzen Gesichter ([#35](https://github.com/kylepelham/El1xr/issues/35)) und die von Lauf zu Lauf wechselnden Outfits**.

## In Zahlen

| Zahl | Was |
|---|---|
| 1600+ automatisierte Tests | Python-Werkzeuge und Rust-Crates, ausgeführt von `dev.py gate` |
| 26.148 Shader-Programme | von RDNA 2 nach DXIL übersetzt, auf der Festplatte zwischengespeichert |
| 0 abgelehnte Pipelines | auf der RTX 5090 |

## Mitwirken

Issues und Pull Requests sind willkommen. Führe `python tools/dev.py gate` aus, bevor du einen Pull Request öffnest. Die Regeln stehen in [CONTRIBUTING.md](../../CONTRIBUTING.md). Fragen und Testberichte gehen an Bone/Nevaa auf Discord: **bonemeld**.

## Danksagungen

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak und seine Mitwirkenden ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 oder später) sowie die Autoren der Forks, aus denen es zusammengesetzt wurde, darunter Neptune (VIctory Edit.) von Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty und seine Mitwirkenden, das Projekt, von dem El1xr abgespalten wurde.
- **Die VIctory-Forks, von denen wir gelernt und Code portiert haben**, darunter fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory und readmet3xt-lab/foxhound.
- Jede Komponente und jeder Autor ist, Änderung für Änderung, in [THIRD_PARTY.md](../../THIRD_PARTY.md) genannt. Die frühere Geschichte des Projekts steht in [docs/history.md](../history.md).

## Lizenz

El1xr wird unter der [GNU GPL 3.0 oder später](../../LICENSE) verbreitet. Teile von Dritten behalten ihre eigene Lizenz: das `Task`-Modul von libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) und die RDNA-2-Tabellen, die aus den öffentlichen Encoding-Tests von LLVM erzeugt wurden (Apache 2.0 mit LLVM-Ausnahme, `PrototypeScarlett/emu/rdna2_ops.py` und `rdna2_shapes.py`).
