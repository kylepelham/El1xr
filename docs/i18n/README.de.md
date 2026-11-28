<div align="center">

![El1xr: Grand Theft Auto VI für Xbox Series auf einem Windows-PC. 21,6-26,0 fps in 4K auf einer RTX 5090, 1 abgelehnte Pipeline, 24,6 s vom Start bis zur Spielwelt, spielbar, Stand 28. November 2026](../images/hero.svg)

![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-9b5cff?style=flat-square&labelColor=170f2b)
![Platform: Windows 10 and 11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2de2e6?style=flat-square&labelColor=170f2b)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-ff8a3d?style=flat-square&labelColor=170f2b&logo=rust&logoColor=white)
![Status: playable](https://img.shields.io/badge/status-playable-ff3d8b?style=flat-square&labelColor=170f2b)
![21.6-26.0 fps at 4K on RTX 5090](https://img.shields.io/badge/4K%20on%20RTX%205090-21.6--26.0%20fps-ffd36e?style=flat-square&labelColor=170f2b)
![Discord: bonemeld](https://img.shields.io/badge/Discord-bonemeld-5865F2?style=flat-square&labelColor=170f2b&logo=discord&logoColor=white)

[English](../../README.md) · [Français](README.fr.md) · [Español](README.es.md) · **Deutsch** · [Italiano](README.it.md) · [Português](README.pt-BR.md) · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

El1xr führt die Xbox-Series-Version von Grand Theft Auto VI (Build 1.0.2.1) auf einem Windows-PC aus. Es ist das Projekt von Bone/Nevaa. Der x86-64-Code des Spiels selbst läuft nativ auf deinem Prozessor. Seine Konsolen-Systemaufrufe, Direct3D-12.X-Befehle und RDNA-2-Shader werden zur Laufzeit für den PC übersetzt. Es gibt keine CPU-Emulation und keine Spielinhalte in diesem Repository: Du bringst deine eigene Kopie mit.

## Status

> [!NOTE]
> **Stand 28. November 2026: GTA VI (Xbox Series, Build 1.0.2.1) startet, erreicht die Menüs und ist unter Windows in der offenen Welt spielbar.** In den jüngsten Testläufen auf ruhigem Rechner (Batch 5, 4K, RTX 5090) läuft das Spiel in 110-Sekunden-Läufen ab dem Start mit 21,6-22,4 fps und in Läufen von 180 und 460 Sekunden mit 25,8-26,0 fps. Das liegt noch unter dem nächsten Ziel, stabilen 30 fps. Danach folgen: keine Ruckler und keine Grafikfehler.

![Meilensteine: Start, Hauptmenü und Spielwelt bis 24. November 2026; spielbar seit 27. November 2026, jetzt 21,6-26,0 fps; als Nächstes 30 fps; Stabilität in Arbeit](../images/milestones.svg)

| Meilenstein | Status | Beleg | Datum |
|---|---|---|---|
| Programm läuft | Erledigt | Die ausführbare Datei der Xbox-Series-Version 1.0.2.1 startet unter Windows. Ihr x86-64-Code läuft nativ. | bis 24. Nov. 2026 |
| Erstes Bild | Erledigt | Die Startanimation und die rechtlichen Hinweise werden ohne Flackern dargestellt. | bis 24. Nov. 2026 |
| Hauptmenü | Erledigt | Profilbildschirm, Ersteinrichtung, rechtliche Hinweise, Ladebildschirm und Pausenmenü funktionieren. Die Menüs laufen mit 30 fps (RTX 4070 SUPER). | bis 24. Nov. 2026 |
| Im Spiel | Erledigt | Das Spiel erreicht die offene Welt, und die Figur lässt sich steuern. Eine 13-minütige Sitzung im Gefängnis lief mit etwa 4 fps (RTX 4070 SUPER). | bis 24. Nov. 2026 |
| Spielbar | Erledigt, unter der Zielgeschwindigkeit | Das Spiel läuft in 4K mit 21,6-26,0 fps (RTX 5090; Läufe aus Batch 5 auf ruhigem Rechner, 110 bis 460 Sekunden lang). | 28. Nov. 2026 |
| Story-Fortschritt | Erste zwei Missionen abgeschlossen | Ein Mitwirkender hat die ersten beiden Missionen in einer 45-minütigen Sitzung mit etwa 2 fps abgeschlossen (RTX 4070 SUPER; gemeldet, nicht aus Logs gemessen). Alle Zwischensequenzen laufen vollständig. Unter Linux (Proton, RX 9070 XT) kam ein 23-minütiger Lauf über den Prolog hinaus bis in Kapitel 1. | 25. Nov. 2026 |
| 30 fps | Noch nicht | 21,6-26,0 fps, also 38-46 ms pro Frame, gegenüber dem Ziel von 33,3 ms (RTX 5090, 4K). | 28. Nov. 2026 |
| Stabil | In Arbeit | Mit den Standardeinstellungen gab es in den Testläufen von Batch 5 keinen Reset des GPU-Treibers und keinen Geräteverlust. Der Stillstand von 8-9 s beim Betreten der Spielwelt, der früh in Batch 5 in 5 von 5 Läufen auftrat, kam in den 47 Starts, die seit Einführung der Prüfung roher indirekter Records kontrolliert wurden, nicht wieder. Das Spiel kann noch abstürzen, wenn Windows der Commit-Speicher ausgeht, und Grafikfehler wechseln noch von Lauf zu Lauf (siehe Bekannte Probleme). | 28. Nov. 2026 |

## Leistung

![Auf einen Blick: 21,6-26,0 fps, 24,6 s vom Start bis zum ersten Bild der Spielwelt (vorher 26,7 s), 0 dunkle Ein-Frame-Blitze im Diner (vorher 12-19 pro Lauf), 2,9 GB GPU-Pufferspeicher nach 180 s (vorher 4,5 GB)](../images/stats.svg)

| Messgröße (RTX 5090, 4K) | Jetzt | Ziel |
|---|---|---|
| Bildrate, 110-s-Läufe ab dem Start | **21,6-22,4 fps** | 30+ fps |
| Bildrate, Läufe von 180 s und 460 s | **25,8-26,0 fps** | 30+ fps |
| CPU-Zeit zur Vorbereitung eines Frames | 15,7-21,2 ms | unter der GPU-Zeit |
| Abgelehnte GPU-Pipelines | 1 | 0 |
| Vom Start bis zum ersten Bild der Spielwelt | 23,1-24,6 s | |
| Stillstand beim Betreten der Spielwelt | keiner in 47 Starts; schlechtester Frame eines Laufs 771-832 ms | keine Ruckler |
| Frames über 250 ms in der Spielwelt | 3-6 pro 110-s-Lauf | keine |
| Dunkle Ein-Frame-Blitze im Diner | 0 (vorher 12-19 pro Lauf) | keine |
| GPU-Pufferspeicher nach 180 s | 2,9 GB (vorher 4,5 GB) | |

Diese Werte stammen aus Batch 5 (abgeschlossen am 28. November 2026), mit den Schaltern, die der Launcher jetzt einschaltet. Jeder Lauf war ruhig: Die übrige Arbeit auf dem Rechner war pausiert oder zurückgehalten, das Spiel startete per Schnellstart von einem festen Checkpoint, und die Werte wurden aus dem eigenen Frame-Log des Emulators gelesen (Bildrate und CPU-Zeit sind Mediane über die zweite Hälfte der Frames eines Laufs in der Spielwelt). Die 110-Sekunden-Werte stammen aus zwei direkt aufeinanderfolgenden Lade-Paaren, der 180-Sekunden-Wert aus dem Paar zu `CARD_BLOCK_REUSE` und der 460-Sekunden-Wert aus einem CPU-Teststart. Die Bildrate hängt von der Szene und von der Länge des Laufs ab, daher sind Werte nur innerhalb eines Paares vergleichbar. In jedem Lauf war ein Teil der Diagnose-Logs eingeschaltet.

![Wohin ein Frame geht: Spiel-Passes 22,8 ms, GPU-Arbeit von El1xr 7,0 ms, nicht erfasst 3,8 ms, GPU-Leerlauf 6,2 ms, gemessen an der 33,3-ms-Linie für 30 fps](../images/frame-budget.svg)

Das letzte GPU-Zeitprofil, aufgenommen vor Batch 3, ergab etwa 40 ms für einen eingependelten Frame, wovon die Render-Passes des Spiels selbst 22,8 ms brauchen. Batch 5 zeigte, dass der Frame jetzt durch die CPU-Arbeit begrenzt ist, die ihn vorbereitet und abschickt, nicht durch die GPU: Mit DLSS, das mit zwei Dritteln der 4K-Auflösung rendert, blieb die echte Bildrate bei 21,8-22,0 fps, genauso wie in nativem 4K (20,1-22,2 fps). Die meisten in Batch 5 behaltenen Änderungen senken daher die CPU-Zeit pro Frame.

![Batch 5: Mit dem Ladepaket sank die Zeit vom Start bis zum ersten Bild der Spielwelt von 26,2-26,7 s auf 23,1-24,6 s; mit TABLE_NULL_ONCE sanken die dunklen Ein-Frame-Blitze im Diner von 12-19 pro Lauf auf 0; mit HEAP_WARM dauerte das erste Bild der Spielwelt 614-702 ms statt 781-1048 ms](../images/load-times.svg)

Batch 5 hat diese Änderungen behalten, die der Launcher jetzt standardmäßig einschaltet. Die Schalter aus Batch 3 und 4 bleiben an.

- **Schnelleres Laden** (`VICTORY_TEXTURE_HEAPS=1`, `VICTORY_TEXTURE_BLOCK_THREAD=1`, `VICTORY_DSTORAGE_STAGING_MB=256`, `VICTORY_DSTORAGE_POLL_WAIT=1`, `VICTORY_BAKE_STAGE_ALL=1`): Gestreamte Texturen teilen sich große Speicherblöcke, die auf einem eigenen Thread angelegt werden, bis zu 256 MiB an DirectStorage-Lesevorgängen sind gleichzeitig unterwegs statt 32, die Abfrage des Spiels nach fertigen Lesevorgängen kehrt zurück, sobald diese fertig sind, und Bake-Passes werden beim Laden gemeinsam bereitgestellt. Im bestätigenden ruhigen Paar sank die Zeit vom Start bis zum ersten Bild der Spielwelt von 26,7 auf 24,6 s, bis zum ersten vollständigen Bild der Spielwelt von 29,2 auf 26,2 s und der schlechteste Frame von 1049 auf 832 ms, bei gleicher Bildrate. Das erste Paar stimmte damit überein (26,2 auf 23,1 s).
- **Diner-Flackern behoben** (`VICTORY_TABLE_NULL_ONCE=1`): Im Diner wurde der Hintergrund etwa alle 2 s für einen Frame dunkel, 12-19 Mal pro Lauf. Eine Deskriptortabelle leerte den Slot einer Textur jedes Mal, wenn die Vorbereitungsarbeit dieser Textur verworfen wurde; jetzt wird ein Slot höchstens einmal geleert. Mit eingeschaltetem Schalter trat weder in einem 110-Sekunden-Lauf noch in zwei 18-Sekunden-Testabschnitten ein solcher Frame auf, ohne messbare Kosten.
- **Kein Stillstand mehr beim Betreten der Spielwelt** (`VICTORY_RAW_CHECK=2`): Indirekte Draw-Records, die die Shader des Spiels schreiben, werden geprüft, bevor sie ausgeführt werden. Früh in Batch 5 fror jeder Lauf etwa 4 s nach dem Betreten der Spielwelt für 8,2-9,0 s ein (5 von 5); keiner der 47 Starts, die nach dieser Änderung geprüft wurden, blieb stehen.
- **Weniger CPU-Zeit pro Frame**:
  - `VICTORY_SKIP_WC_BACK=1`: Große Shader-Schreibvorgänge in Write-Combining-Speicher bleiben auf der Grafikkarte, statt zurückkopiert zu werden. 24,6 fps in 2 von 2 Läufen, gegenüber 21,5 fps im besten ruhigen Lauf ohne den Schalter, und 7,3 ms weniger CPU-Zeit pro Frame.
  - `VICTORY_CARD_BLOCK_REUSE=1`: GPU-Speicherstücke werden genullt und wiederverwendet, statt neu angelegt zu werden. Von 23,6 auf 26,0 fps in einem ruhigen 180-Sekunden-Paar, mit 2,9 GB GPU-Pufferspeicher am Ende statt 4,5 GB.
  - `VICTORY_SEND_THREAD=1` und `VICTORY_CARD_RECORD_SEND=1`: Befehlslisten werden auf einem eigenen Thread abgeschickt und GPU-Synchronisationslisten dort aufgezeichnet. Die Sendezeit auf dem Frame-Thread sank von 1,56 auf 0,11 ms.
  - `VICTORY_FLUSH_BESIDE=1`, `VICTORY_RESET_ASIDE=1`, `VICTORY_COPIED_SHARED=1`, `VICTORY_TEXTURE_HASH_BEHIND=1`, `VICTORY_RESULT_RING=1` sowie `VICTORY_WIDE_WAKE_ONE=1` mit `VICTORY_WAIT_SPIN=50`: kleinere Einsparungen von je 0,5-1,8 ms CPU-Zeit pro Frame oder bis zu 1,4 fps, gemessen, indem jeder Schalter innerhalb eines einzigen Laufs ein- und ausgeschaltet wurde.
- **Schnelleres Betreten der Spielwelt** (`VICTORY_HEAP_WARM=1`): Die Host-Kopien der View-Heaps des Spiels werden auf dem Ladebildschirm angelegt, daher dauerte das erste Bild der Spielwelt 614 ms statt 1048 ms (zweites Paar; erstes Paar 781 auf 702 ms).
- **Weniger langsame Frames** (`VICTORY_AMBIENT_ONCE=1`): Der Ersatz für die Umgebungsbeleuchtung des Spiels zählt nur eine Lichtreflexion. Die Frametime im 99. Perzentil lag in einem Test innerhalb desselben Laufs 11 ms niedriger.

Upscaling in 4K stürzt nicht mehr ab: Drei Starts mit Upscaling liefen ohne Division durch null (0xc0000094). DLSS-Upscaling sieht scharf aus, bringt aber keine höhere Bildrate, weil der Frame auf die CPU wartet. DLSS-Frame-Generation (`-FrameGen` oder Frame generation = dlss im F9-Menü) zeigt etwa 44-47 fps aus 22-24 echten Frames an; sie bleibt standardmäßig aus, bis die erzeugten Frames mit eigenen Augen geprüft wurden. Batch 5 hat außerdem Schalter verworfen, die etwas kaputt machten oder Geschwindigkeit kosteten, darunter `PIPELINE_PIECES` (schwarzer Bildschirm), `BAKE_SPREAD=12` (1,2-2,2 fps langsamer) und `GUARD_PUBLISH_TRY` (ein Absturz). Der Raytracing-Builder des Spiels funktioniert, wenn er eingeschaltet wird, läuft dann aber mit 0,6-1,1 fps und bleibt daher aus.

### Testrechner

| Komponente | Modell |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, Treiber 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 Kerne / 32 Threads |
| Arbeitsspeicher | 64 GB DDR5-6000 |
| Auslagerungsdatei | 64 GB (Commit-Limit 142 GB) |
| Mainboard | MSI PRO B650-P WIFI |
| Speicher | NVMe-SSDs (Samsung 990 EVO Plus) |
| Betriebssystem | Windows 11 (Build 26200) |
| Spiel | GTA VI für Xbox Series, Build 1.0.2.1 (deine eigene Kopie) |

Alle aktuellen Werte stammen von diesem Rechner. Ältere Werte in diesem README wurden auf einer RTX 4070 SUPER (Windows) oder einer RX 9070 XT (Linux) gemessen, und das ist jeweils angegeben.

## Funktionsweise

![Funktionsweise: Der x86-64-Code des Spiels läuft nativ auf der CPU; Konsolen-Systemaufrufe gehen an den HLE-Kernel unter Windows; D3D12.X-Befehle werden als Direct3D 12 des Host-Systems neu aufgebaut; RDNA-2-Shader werden nach DXIL übersetzt und zwischengespeichert](../images/how-it-works.svg)

- **Spielcode** wird unverändert geladen und ausgeführt. Xbox Series und Windows-PC teilen sich den x86-64-Befehlssatz, daher wird kein einziger Befehl emuliert.
- **Systemaufrufe** an den Konsolen-Kernel und die GDK-Bibliotheken bedient die High-Level-Emulation (HLE) von El1xr auf Basis von Windows: Speicher, Threads, Dateien und Systemdienste.
- **Lesezugriffe auf die Festplatte**: Die DirectStorage-Anfragen des Spiels bedienen eigene Worker-Threads von El1xr, mit bis zu 256 MiB gleichzeitig unterwegs. Gestreamte Texturen liegen in gemeinsamen Speicherblöcken: in einem typischen Lauf etwa 115.000 Texturen in 116 Blöcken.
- **Grafik**: Die Direct3D-12.X-Befehlslisten, der Speicher und die Ressourcen der Konsole werden als Standard-Direct3D-12 auf deiner GPU neu aufgebaut. Befehlslisten werden von einem eigenen Thread abgeschickt, sodass der Frame-Thread des Spiels weniger von dieser Arbeit erledigt.
- **Deskriptor-Heaps**: Das Spiel adressiert bis zu 1.245.184 Deskriptor-Slots (seine Tabelle der Textur- und Puffer-Views). El1xr spiegelt sie in einen einzigen Host-Heap mit etwa 1.000.000 Slots, den es mit seinen eigenen Views teilt, und legt die Host-Kopien auf dem Ladebildschirm an. Wie der Rest hineinpasst, ist noch offen (siehe Bekannte Probleme).
- **Shader**, die für die RDNA-2-GPU der Konsole kompiliert wurden, werden nach DXIL übersetzt. Bis auf 6 der 26.148 Shader-Programme des Spiels lassen sich alle übersetzen, und jede Übersetzung wird auf der Festplatte zwischengespeichert. `dev.py prewarm` baut die 25.751 GPU-Pipelines des Spiels vor einem Lauf, sodass sie nicht während des Spielens kompiliert werden.

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

- **Die Fensterbank im Gefängnis wird nach dem Prolog dunkel.** In der Zellenaufnahme nach dem Titel PROLOGUE wird die sonnenbeschienene Fensterbank von einem Frame zum nächsten dunkel und verliert ihre Details, bei gleicher Kamera. In Batch 5 zweimal reproduziert; die Ursache wird untersucht.
- **Haut kann schwarz werden.** In manchen Läufen wird die Haut einer Figur, meist im Diner, für einige Sekunden komplett schwarz und kehrt dann zurück ([#35](https://github.com/kylepelham/El1xr/issues/35)). Auch das Oberteil der Kellnerin wechselt von Lauf zu Lauf die Farbe.
- **Dunkle Quadrate über einem Fenster.** In der Eröffnungs-Zwischensequenz bedecken nach etwa 44 s treppenförmig angeordnete dunkle Quadrate das Fenster hinter Jason. Batch 5 hat zwei vermutete Ursachen ausgeschlossen; die Suche geht weiter.
- **Grenze des Deskriptor-Heaps.** Das Spiel adressiert bis zu 1.245.184 Deskriptor-Slots, der Host-Heap auf der RTX 5090 fasst aber etwa 1.000.000. In Läufen aus Batch 5 lagen 313.000-504.000 der aktiven Views des Spiels jenseits des Teils, den der Host-Heap fassen kann. Ein Weg, sie unterzubringen, wurde bisher nicht behalten.
- **Der Telefonbildschirm ist schwarz** in der Tischaufnahme im Diner.
- **Hoher Speicherverbrauch.** Der Spielprozess erreicht Spitzen von 33-35 GB. Mit einer zu kleinen Auslagerungsdatei stürzt er ab, wenn Windows der Commit-Speicher ausgeht. Der Test-PC läuft mit einer 64-GB-Auslagerungsdatei.
- **Langsames Streaming.** Nach dem Betreten der Spielwelt dauert es etwa 72 s, bis alle Assets geladen sind; bis dahin ploppen Objekte auf.
- **Der Frame wartet auf die CPU.** Einen Frame vorzubereiten und abzuschicken kostet 15,7-21,2 ms CPU-Zeit, und deshalb bringt Upscaling keine höhere Bildrate.
- **Raytracing-Beleuchtung ist aus.** Das Spiel baut seine Raytracing-Strukturen in Software; dieser Builder funktioniert, läuft aber mit 0,6-1,1 fps und bleibt daher aus.
- **Ein einziger Referenzrechner.** Die aktuellen Werte stammen von der oben genannten RTX 5090. Andere GPUs wurden mit dem aktuellen Build noch nicht gemessen.

## Wie es weitergeht

Ziel sind stabile 30 fps in 4K auf der RTX 5090, danach keine Ruckler und keine Grafikfehler. Jede neue Änderung bleibt standardmäßig ausgeschaltet, bis ruhige Testläufe sie bestätigen.

- **Weniger CPU-Zeit pro Frame**: Der Frame ist CPU-gebunden, daher verlagern die nächsten Batches weiter Arbeit für das Abschicken und die Texturprüfung vom Frame-Thread weg.
- **Die Fensterbank und die Quadrate am Fenster** sowie **Haut, die schwarz wird** ([#35](https://github.com/kylepelham/El1xr/issues/35)).
- **Der Deskriptor-Heap**: die 1.245.184 Slots des Spiels im Host-Heap unterbringen.
- **Hardware-Raytracing auf den RT-Kernen**, womit die Raytracing-Beleuchtung zurückkäme.
- **DLSS-Frame-Generation als Standard**, sobald ihre erzeugten Frames mit eigenen Augen geprüft sind.

## In Zahlen

| Zahl | Was |
|---|---|
| 4000+ bestandene Tests | Rust-Crates (`cargo test`) und Python-Werkzeuge (`pytest`), ausgeführt von `dev.py gate` |
| 26.148 Shader-Programme | von RDNA 2 nach DXIL übersetzt und auf der Festplatte zwischengespeichert; 6 lassen sich noch nicht übersetzen |
| 25.751 GPU-Pipelines | vor dem Spielen von `dev.py prewarm` gebaut |
| 1 abgelehnte Pipeline | auf der RTX 5090, in den jüngsten Läufen |

## Mitwirken

Issues und Pull Requests sind willkommen. Führe `python tools/dev.py gate` aus, bevor du einen Pull Request öffnest. Die Regeln stehen in [CONTRIBUTING.md](../../CONTRIBUTING.md). Fragen und Testberichte gehen an Bone/Nevaa auf Discord: **bonemeld**.

## Danksagungen

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak und seine Mitwirkenden ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 oder später) sowie die Autoren der Forks, aus denen es zusammengesetzt wurde, darunter Neptune (VIctory Edit.) von Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty und seine Mitwirkenden, das Projekt, von dem El1xr abgespalten wurde.
- **Die VIctory-Forks, von denen wir gelernt und Code portiert haben**, darunter fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory, readmet3xt-lab/foxhound, RipcuBAN/victory-fork und Kemerd/VIctory-NVIDIA-RT.
- Jede Komponente und jeder Autor ist, Änderung für Änderung, in [THIRD_PARTY.md](../../THIRD_PARTY.md) genannt. Die frühere Geschichte des Projekts steht in [docs/history.md](../history.md).

## Lizenz

El1xr wird unter der [GNU GPL 3.0 oder später](../../LICENSE) verbreitet. Teile von Dritten behalten ihre eigene Lizenz: das `Task`-Modul von libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) und die RDNA-2-Tabellen, die aus den öffentlichen Encoding-Tests von LLVM erzeugt wurden (Apache 2.0 mit LLVM-Ausnahme, `PrototypeScarlett/emu/rdna2_ops.py` und `rdna2_shapes.py`).
