<div align="center">

![El1xr: Grand Theft Auto VI für Xbox Series auf einem Windows-PC. 24-25 fps in 4K auf einer RTX 5090, 0 abgelehnte Pipelines, 31-33 s vom Start bis zur Spielwelt, spielbar seit dem 27. November 2026](../images/hero.svg)

![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-9b5cff?style=flat-square&labelColor=170f2b)
![Platform: Windows 10 and 11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2de2e6?style=flat-square&labelColor=170f2b)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-ff8a3d?style=flat-square&labelColor=170f2b&logo=rust&logoColor=white)
![Status: playable](https://img.shields.io/badge/status-playable-ff3d8b?style=flat-square&labelColor=170f2b)
![24-25 fps at 4K on RTX 5090](https://img.shields.io/badge/4K%20on%20RTX%205090-24--25%20fps-ffd36e?style=flat-square&labelColor=170f2b)
![Discord: bonemeld](https://img.shields.io/badge/Discord-bonemeld-5865F2?style=flat-square&labelColor=170f2b&logo=discord&logoColor=white)

[English](../../README.md) · [Français](README.fr.md) · [Español](README.es.md) · **Deutsch** · [Italiano](README.it.md) · [Português](README.pt-BR.md) · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

El1xr führt die Xbox-Series-Version von Grand Theft Auto VI (Build 1.0.2.1) auf einem Windows-PC aus. Es ist das Projekt von Bone/Nevaa. Der x86-64-Code des Spiels selbst läuft nativ auf deinem Prozessor. Seine Konsolen-Systemaufrufe, Direct3D-12.X-Befehle und RDNA-2-Shader werden zur Laufzeit für den PC übersetzt. Es gibt keine CPU-Emulation und keine Spielinhalte in diesem Repository: Du bringst deine eigene Kopie mit.

## Status

> [!NOTE]
> **Stand 27. November 2026: GTA VI (Xbox Series, Build 1.0.2.1) startet, erreicht die Menüs und ist unter Windows in der offenen Welt spielbar.** Nach dem Einpendeln läuft es in 4K auf einer RTX 5090 mit 24-25 fps, bei 0 abgelehnten GPU-Pipelines. Das nächste Ziel sind stabile 30 fps. Danach folgen: keine Ruckler und keine Grafikfehler.

![Meilensteine: Start, Hauptmenü und Spielwelt bis 24. November 2026; spielbar mit 24-25 fps am 27. November 2026; als Nächstes 30 fps; Stabilität in Arbeit](../images/milestones.svg)

| Meilenstein | Status | Beleg | Datum |
|---|---|---|---|
| Programm läuft | Erledigt | Die ausführbare Datei der Xbox-Series-Version 1.0.2.1 startet unter Windows. Ihr x86-64-Code läuft nativ. | bis 24. Nov. 2026 |
| Erstes Bild | Erledigt | Die Startanimation und die rechtlichen Hinweise werden ohne Flackern dargestellt. | bis 24. Nov. 2026 |
| Hauptmenü | Erledigt | Profilbildschirm, Ersteinrichtung, rechtliche Hinweise, Ladebildschirm und Pausenmenü funktionieren. Die Menüs laufen mit 30 fps (RTX 4070 SUPER). | bis 24. Nov. 2026 |
| Im Spiel | Erledigt | Das Spiel erreicht die offene Welt, und die Figur lässt sich steuern. Eine 13-minütige Sitzung im Gefängnis lief mit etwa 4 fps (RTX 4070 SUPER). | bis 24. Nov. 2026 |
| Spielbar | Erledigt, unter der Zielgeschwindigkeit | Die Spielwelt läuft nach dem Einpendeln in 4K mit 24-25 fps (RTX 5090; drei Benchmark-Läufe auf ruhigem Rechner mit 24,9, 25,2 und 24,2 fps). | 27. Nov. 2026 |
| Story-Fortschritt | Erste zwei Missionen abgeschlossen | Ein Mitwirkender hat die ersten beiden Missionen in einer 45-minütigen Sitzung mit etwa 2 fps abgeschlossen (RTX 4070 SUPER; gemeldet, nicht aus Logs gemessen). Alle Zwischensequenzen laufen vollständig. Unter Linux (Proton, RX 9070 XT) kam ein 23-minütiger Lauf über den Prolog hinaus bis in Kapitel 1. | 25. Nov. 2026 |
| 30 fps | Noch nicht | 24-25 fps nach dem Einpendeln, Median der Frametime etwa 40 ms gegenüber dem Ziel von 33,3 ms (RTX 5090, 4K). | 27. Nov. 2026 |
| Stabil | In Arbeit | Mit den Standardeinstellungen enden 200-Sekunden-Benchmark-Läufe ohne Absturz, ohne Reset des GPU-Treibers und ohne Geräteverlust. Grafikfehler wechseln noch von Lauf zu Lauf (siehe Bekannte Probleme). | 27. Nov. 2026 |

## Leistung

![Auf einen Blick: 24-25 fps nach dem Einpendeln, 0 abgelehnte GPU-Pipelines, 31-33 s vom Start bis zur Spielwelt, schlimmstes Einfrieren beim Betreten der Spielwelt 1,5-1,9 s](../images/stats.svg)

| Messgröße (RTX 5090, 4K) | Jetzt | Ziel |
|---|---|---|
| Eingependelte Bildrate | **24-25 fps** | 30+ fps |
| Median der Frametime | ~40 ms | höchstens 33,3 ms |
| Frame im 99. Perzentil | 68-73 ms | keine Ruckler |
| CPU-Zeit zur Vorbereitung eines Frames | 25-26 ms | unter der GPU-Zeit |
| Abgelehnte GPU-Pipelines | **0** | 0 |
| Vom Start bis zum ersten Bild der Spielwelt | ~31-33 s | |
| Schlimmstes Einfrieren beim Betreten der Spielwelt | 1,5-1,9 s | keines |
| Streaming nach dem Betreten der Spielwelt | 52-63 s | wenige Sekunden |

Bildrate und Frametimes stammen aus `python tools/dev.py bench --quiet --seconds 200`. Jeder Lauf startet per Schnellstart von einem festen Checkpoint, sonst läuft nichts auf dem Rechner, und die Werte werden aus dem eigenen Frame-Log des Emulators gelesen. Startzeit und Einfrieren beim Betreten der Spielwelt stammen aus Läufen, die stattfanden, während der Rechner mit Kompilieren beschäftigt war; auf einem ruhigen Rechner sollten sie etwas besser ausfallen.

![Wohin ein Frame geht: Spiel-Passes 22,8 ms, GPU-Arbeit von El1xr 7,0 ms, nicht erfasst 3,8 ms, GPU-Leerlauf 6,2 ms, gemessen an der 33,3-ms-Linie für 30 fps](../images/frame-budget.svg)

Ein eingependelter Frame dauert etwa 40 ms. Die Render-Passes des Spiels selbst brauchen 22,8 ms GPU-Zeit, 30 fps sind also in Reichweite. Der Rest entfällt auf die eigene GPU-Arbeit von El1xr (7 ms: Bereichsprüfungen bei indirekten Draws, Texturkopien zwischen Formaten, Uploads) und auf Leerlauf der GPU (6-10 ms), während ein einzelner CPU-Thread den nächsten Arbeitsstapel vorbereitet. Genau diese beiden Posten nehmen sich die nächsten Updates vor.

![Ladezeiten: vom Start bis zum ersten Bild der Spielwelt von 41-54 s auf 31-33 s, das schlimmste Einfrieren beim Betreten der Spielwelt von 7,6-8,1 s auf 1,5-1,9 s](../images/load-times.svg)

Das Update vom 27. November 2026 bereitet Texturen ohne Sperren vor, berechnet Figuren beim Laden parallel vor und übersetzt jeden Shader, sobald das Spiel ihn anlegt. Die Zeit vom Start bis zum ersten Bild der Spielwelt sank von 41-54 s auf 31-33 s, das schlimmste Einfrieren beim Betreten der Spielwelt von 7,6-8,1 s auf 1,5-1,9 s.

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

- **Grafikfehler, die von Lauf zu Lauf wechseln.** In etwa der Hälfte der Testläufe werden Gesichter schwarz oder fleckig, Lucia trägt im Diner das falsche Outfit, oder ihre Haare werden am Gefängnisfenster orange.
- **Langsames Streaming.** Nach dem Betreten der Spielwelt dauert es etwa eine Minute, bis alle Assets geladen sind; bis dahin ploppen Objekte auf.
- **Die GPU wartet auf die CPU**, 6-10 ms pro Frame (siehe die Aufteilung eines Frames oben).
- **Raytracing-Beleuchtung ist größtenteils aus.** Das Spiel baut seine Raytracing-Strukturen in Software, und dieser Builder ist vorerst deaktiviert.
- **Ein einziger Referenzrechner.** Die aktuellen Werte stammen von der oben genannten RTX 5090. Andere GPUs wurden mit dem aktuellen Build noch nicht gemessen.

## Wie es weitergeht

Ziel sind stabile 30 fps in 4K auf der RTX 5090, danach keine Ruckler und keine Grafikfehler. Rund 60 weitere Beschleunigungen sind geschrieben und warten darauf, gemessen zu werden. Jede bleibt standardmäßig ausgeschaltet, bis sie sich bewährt hat.

- **Speicher in 2-MiB-Abschnitten, Spiel-Buffer im GPU-Speicher gehalten, höhere Priorität für die Frame-Threads.** *In Erprobung, ein einzelner Lauf (27. Nov. 2026, RTX 5090):* 26,5 fps gegenüber 23,8 fps, eingependelt nach 92 s statt 115 s, und das Unmapping von Speicher 8- bis 12-mal günstiger. Zwei Läufe, die dies mit drei Shader-Korrekturen kombinierten, brachten die GPU beim Betreten der Spielwelt zum Hängen; es bleibt daher ausgeschaltet, bis das behoben ist.
- **Weniger Warten und weniger Overhead**: die 6-10 ms verkürzen, die die GPU in jedem Frame auf die CPU wartet, sowie die 7 ms eigene GPU-Arbeit von El1xr.
- **DLSS-Upscaling** und **Hardware-Raytracing auf den RT-Kernen**, womit die Raytracing-Beleuchtung zurückkäme.
- **Offline-Vorwärmen von Shadern und Pipelines**, damit während des Spielens kein Shader kompiliert wird.
- **Korrekturen für die von Lauf zu Lauf wechselnden Gesichter und Outfits**.

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
