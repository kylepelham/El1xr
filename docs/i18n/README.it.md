<div align="center">

# El1xr

**Grand Theft Auto VI (Xbox Series, build 1.0.2.1) in esecuzione su un PC Windows.**<br>
In 4K, circa 25 fps e in crescita su una RTX 5090.

[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](../../LICENSE)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078D6?logo=windows&logoColor=white)](../running-gta6.md)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-B7410E?logo=rust&logoColor=white)](../../third_party/victory)
[![Discord](https://img.shields.io/badge/Discord-AnyXboxX-5865F2?logo=discord&logoColor=white)](https://discord.gg/TcX42vRyPU)

[English](../../README.md) · [Français](README.fr.md) · [Español](README.es.md) · [Deutsch](README.de.md) · **Italiano** · [Português](README.pt-BR.md) · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

---

El1xr è il progetto di emulazione di GTA VI di Bone/Nevaa: un fork di [AnyXboxX](https://github.com/LooperSalty/AnyXboxX), costruito su [VIctory](https://github.com/Marvisak/VIctory) e sul meglio del lavoro dei fork di VIctory (ogni fonte è citata in [THIRD_PARTY.md](../../THIRD_PARTY.md)). Il codice x86-64 del gioco viene eseguito in modo nativo; le sue chiamate di sistema della console, i comandi Direct3D 12.X e gli shader RDNA 2 vengono tradotti al volo per PC.

> [!NOTE]
> **Stato al 27 novembre 2026**
>
> - 🎮 **Il mondo è giocabile in 4K, a 24-25 fps una volta stabilizzato** su una RTX 5090 (tre sessioni di benchmark a macchina scarica: 24,9; 25,2 e 24,2 fps). L'obiettivo sono 30 fps stabili.
> - ✅ **0 pipeline GPU rifiutate** e nessuna perdita del dispositivo GPU nelle sessioni di benchmark.
> - ✅ Le cutscene vengono riprodotte per intero e le prime missioni sono state giocate fino in fondo.
> - ⚡ **Lotto 1 integrato**: dall'avvio al primo fotogramma del mondo ora circa 31-33 s (prima 41-54 s), e il blocco peggiore all'ingresso nel mondo 1,5-1,9 s (prima 7,6-8,1 s).
> - 🚧 Circa **60 ulteriori accelerazioni sono scritte e in fase di test** (vedi [In fase di test](#in-fase-di-test)).

## In breve

| Misura (RTX 5090, 4K) | Ora | Obiettivo |
|---|---|---|
| Frame rate stabilizzato | **24-25 fps** | 30+ fps |
| Tempo per fotogramma mediano | ~40 ms | ≤ 33,3 ms |
| Fotogramma al 99° percentile | 68-73 ms | nessuno scatto |
| Tempo CPU per preparare un fotogramma | 25-26 ms | sotto il tempo della GPU |
| Pipeline GPU rifiutate | **0** | 0 |
| Dall'avvio al primo fotogramma del mondo | ~31-33 s | |
| Streaming dopo l'ingresso nel mondo | 52-63 s | pochi secondi |

Misurato con `python tools/dev.py bench --quiet --seconds 200`: avvio rapido da un checkpoint fisso, nient'altro in esecuzione sulla macchina, valori letti dal registro dei fotogrammi dell'emulatore stesso. Il tempo di avvio viene dalle sessioni del lotto 1, eseguite mentre altro lavoro veniva compilato, quindi una macchina a riposo dovrebbe fare un po' meglio.

## Macchina di prova

| Componente | Modello |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, driver 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 core / 32 thread |
| Memoria | 64 GB DDR5-6000 |
| Scheda madre | MSI PRO B650-P WIFI |
| Archiviazione | SSD NVMe (Samsung 990 EVO Plus) |
| Sistema operativo | Windows 11 (build 26200) |
| Gioco | GTA VI per Xbox Series, build 1.0.2.1 (la tua copia) |

## Dove va il tempo di un fotogramma

![Tempo GPU per fotogramma: passate del gioco 22,8 ms, lavoro dell'emulatore 7,0 ms, non misurato 3,8 ms, inattiva 6,2 ms](../images/frame-budget.svg)

Un fotogramma stabilizzato richiede circa 40 ms. Le passate di rendering del gioco stesso occupano 22,8 ms di tempo GPU, quindi i 30 fps sono a portata di mano. Il resto se ne va in:

- **il nostro lavoro sulla GPU** (7 ms): controlli dei limiti sui draw indiretti, copie di texture tra formati, verifiche di ciò che la GPU ha scritto;
- **GPU inattiva** (6-10 ms): la scheda attende mentre un solo thread della CPU prepara il lotto di lavoro successivo.

Sono entrambi l'obiettivo del lavoro in fase di test.

## Cosa è cambiato questa settimana

**Corretto**
- La brina della finestra della prigione non viene più disegnata sopra i capelli di Lucia.
- Sono sparite le «linee su tutto» alla luce del sole: il traduttore di shader trattava alcuni valori come uguali nelle due metà di una wave a 64 lane quando non lo erano (31 shader).
- Sono sparite le chiazze sul vetro del diner: la passata del vetro in ray tracing ora viene eseguita come «nessuna intersezione» invece di essere saltata.
- Audio: decodifica XMA più sicura e un percorso pulito per la disattivazione dell'audio e lo spegnimento.

**Più veloce**
- I caricamenti delle texture sono stati spostati fuori dai thread di disegno del gioco, i controlli delle texture vengono eseguiti in parallelo e le liste rilasciate vengono liberate su un thread dedicato: il blocco peggiore all'ingresso nel mondo è sceso da 9,5 s a 6,6 s.
- L'allocazione di una texture da parte del driver, lato CPU, costa 382-529 µs su questa macchina e 5-8 µs da un blocco di memoria preallocato; i pool di texture e buffer sono in fase di test.

**Strumenti**
- Avvio rapido per le sessioni di prova: salta il filmato introduttivo e parte da un checkpoint congelato.
- Ogni bench riporta fps, percentili del tempo per fotogramma, scatti oltre 100 e 250 ms, tempi di avvio e di streaming, reset del driver GPU, e salva degli screenshot.
- Costo GPU per passata, fasi del fotogramma per thread e un registro degli scatti con campioni dello stack.
- Il traduttore di shader e l'host GPU sono stati divisi in moduli più piccoli; l'output del traduttore è rimasto identico byte per byte su tutti i 26.148 programmi shader del gioco.

## Problemi noti

- **Difetti visivi che cambiano da una sessione all'altra**: in alcune sessioni il top di Lucia nel diner ha la texture sbagliata, i volti sono neri o macchiati, oppure i capelli diventano arancioni alla finestra della prigione. Correzioni portate da altri fork sono in fase di test.
- **Streaming lento**: dopo l'ingresso nel mondo serve circa un minuto perché arrivino tutti gli asset, con elementi che compaiono all'improvviso nel frattempo.
- **GPU in attesa della CPU**: 6-10 ms per fotogramma (vedi sopra).
- **L'illuminazione in ray tracing è in gran parte disattivata**: il gioco costruisce le sue strutture di ray tracing via software, e quel builder è disattivato.

## In fase di test

Il lavoro viene scritto in parallelo e testato a lotti sull'unica GPU: ogni modifica sta dietro un proprio interruttore `VICTORY_*`, un lotto confronta tutti i nuovi interruttori attivi con tutti disattivati, e ogni interruttore viene tenuto o scartato in base ai numeri e agli screenshot.

- **Lotto 1 (concluso, 20 interruttori tenuti)**: staging delle texture senza lock, meno ricostruzioni dei descrittori e attese sui lock, precalcolo parallelo dei personaggi durante il caricamento, shader tradotti non appena il gioco li crea, caricamento più rapido delle librerie di pipeline, mappatura della memoria più snella, e le correzioni di abiti, volti e capelli (il top di Lucia nel diner era corretto in 4 sessioni su 5, contro 0 su 2 prima). Scartati per ora: i pool di memoria per texture e buffer (rallentavano l'unmap della memoria) e due interruttori che causavano crash; i loro autori li stanno correggendo.
- **Lotto 2 (in corso)**: memoria del titolo in sezioni da 2 MiB (tempo di mappatura 55,5 s → 7,3 s nel progetto originale), buffer del gioco mantenuti nella memoria della GPU (−15 % di tempo per fotogramma nel progetto originale su una RTX 5090), upscaling DLSS, priorità più alta per i thread dei fotogrammi dell'emulatore, preriscaldamento offline di shader e pipeline, e ray tracing hardware sui core RT.

## Avvialo

Servono la tua copia del gioco, Windows 10 o 11, una GPU con Shader Model 6.6 e mesh shader, e Rust.

```powershell
tools\lancer_victory.ps1 -GameDir <cartella del gioco>
```

La guida completa (installazione, impostazioni, risoluzione dei problemi) è in [docs/running-gta6.md](../running-gta6.md).

> [!IMPORTANT]
> Questo repository non contiene file del gioco, chiavi né eseguibili, e non spiega come ottenerli.

Per lo sviluppo, `tools/dev.py` raggruppa i passaggi più comuni:

```powershell
python tools/dev.py build      # build release
python tools/dev.py gate       # tutti i test, fallisce a ogni avviso
python tools/dev.py prewarm    # traduce tutti gli shader offline (circa 45 s)
python tools/dev.py bench --quiet --seconds 200   # sessione di gioco misurata
python tools/dev.py train --plan plan.toml        # un'esecuzione di un piano di lotto
```

## Il progetto in numeri

| Dato | Cosa |
|---|---|
| 1600+ test automatizzati | strumenti Python e crate Rust, eseguiti da `dev.py gate` |
| 26.148 programmi shader | tradotti da RDNA 2 a DXIL, in cache su disco |
| 0 pipeline rifiutate | sulla RTX 5090 |

## Storia

Come il progetto è arrivato fin qui, dalle prime DLL della console caricate in un emulatore ai primi fotogrammi del mondo: [docs/history.md](../history.md).

## Contribuire

Issue e pull request sono benvenute. Esegui `python tools/dev.py gate` prima di aprire una pull request. Le regole sono in [CONTRIBUTING.md](../../CONTRIBUTING.md). Discussioni e resoconti dei test: [Discord](https://discord.gg/TcX42vRyPU).

## Riconoscimenti

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak e i suoi collaboratori ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 o successiva), e gli autori dei fork da cui è stato assemblato, tra cui Neptune (VIctory Edit.) di Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty e i suoi collaboratori ([AnyXboxX](https://github.com/LooperSalty/AnyXboxX)), il progetto da cui El1xr è stato derivato.
- **I fork di VIctory da cui abbiamo imparato e portato codice**, tra cui fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory e readmet3xt-lab/foxhound.
- Ogni componente e ogni autore, modifica per modifica, è citato in [THIRD_PARTY.md](../../THIRD_PARTY.md).

## Licenza

El1xr è distribuito sotto la [GNU GPL 3.0 o successiva](../../LICENSE). Le parti di terzi mantengono la propria licenza: il modulo `Task` di libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) e le tabelle RDNA 2 generate dai test pubblici di codifica di LLVM (Apache 2.0 con eccezione LLVM, `PrototypeScarlett/emu/rdna2_ops.py` e `rdna2_shapes.py`).
