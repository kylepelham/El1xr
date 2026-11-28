<div align="center">

![El1xr: Grand Theft Auto VI per Xbox Series in esecuzione su un PC Windows. 23,1-25,7 fps in 4K su una RTX 5090, 0 pipeline rifiutate, 27-28 s dall'avvio al mondo di gioco, giocabile dal 27 novembre 2026](../images/hero.svg)

![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-9b5cff?style=flat-square&labelColor=170f2b)
![Platform: Windows 10 and 11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2de2e6?style=flat-square&labelColor=170f2b)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-ff8a3d?style=flat-square&labelColor=170f2b&logo=rust&logoColor=white)
![Status: playable](https://img.shields.io/badge/status-playable-ff3d8b?style=flat-square&labelColor=170f2b)
![23.1-25.7 fps at 4K on RTX 5090](https://img.shields.io/badge/4K%20on%20RTX%205090-23.1--25.7%20fps-ffd36e?style=flat-square&labelColor=170f2b)
![Discord: bonemeld](https://img.shields.io/badge/Discord-bonemeld-5865F2?style=flat-square&labelColor=170f2b&logo=discord&logoColor=white)

[English](../../README.md) · [Français](README.fr.md) · [Español](README.es.md) · [Deutsch](README.de.md) · **Italiano** · [Português](README.pt-BR.md) · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

El1xr esegue la versione Xbox Series di Grand Theft Auto VI (build 1.0.2.1) su un PC Windows. È il progetto di Bone/Nevaa. Il codice x86-64 del gioco viene eseguito in modo nativo sul tuo processore. Le sue chiamate di sistema della console, i comandi Direct3D 12.X e gli shader RDNA 2 vengono tradotti per PC durante l'esecuzione. Non c'è alcuna emulazione della CPU e nessun contenuto del gioco in questo repository: la copia del gioco la porti tu.

## Stato

> [!NOTE]
> **Al 27 novembre 2026, GTA VI (Xbox Series, build 1.0.2.1) si avvia, raggiunge i menu ed è giocabile nel mondo aperto su Windows.** Nelle ultime sessioni di prova a macchina scarica (batch 4, 4K, RTX 5090) il gioco gira a 23,1-25,7 fps su una sessione di 200 secondi, con 0 pipeline GPU rifiutate. È ancora sotto il prossimo obiettivo, 30 fps stabili. Poi vengono l'assenza di scatti e di difetti visivi.

![Traguardi: avvio, menu principale e mondo di gioco entro il 24 novembre 2026; giocabile a 23,1-25,7 fps il 27 novembre 2026; prossimo traguardo i 30 fps; stabilità in corso](../images/milestones.svg)

| Traguardo | Stato | Riscontro | Data |
|---|---|---|---|
| Eseguibile avviato | Fatto | L'eseguibile Xbox Series 1.0.2.1 si avvia su Windows. Il suo codice x86-64 viene eseguito in modo nativo. | entro il 24 nov 2026 |
| Primo fotogramma | Fatto | L'animazione iniziale e le schermate legali vengono visualizzate senza sfarfallio. | entro il 24 nov 2026 |
| Menu principale | Fatto | Schermata del profilo, impostazioni del primo avvio, schermate legali, schermata di caricamento e menu di pausa funzionano tutti. I menu girano a 30 fps (RTX 4070 SUPER). | entro il 24 nov 2026 |
| In gioco | Fatto | Il gioco raggiunge il mondo aperto e il personaggio si muove sotto il controllo del giocatore. Una sessione di 13 minuti nella prigione è girata a circa 4 fps (RTX 4070 SUPER). | entro il 24 nov 2026 |
| Giocabile | Fatto, sotto la velocità obiettivo | Il gioco gira a 23,1-25,7 fps in 4K (RTX 5090; due sessioni da 200 secondi del batch 4 a macchina scarica). | 27 nov 2026 |
| Avanzamento della storia | Prime due missioni completate | Un collaboratore ha completato le prime due missioni in una sessione di 45 minuti a circa 2 fps (RTX 4070 SUPER; riferito, non misurato dai log). Tutte le cutscene vengono riprodotte per intero. Su Linux (Proton, RX 9070 XT) una sessione di 23 minuti ha superato il prologo ed è arrivata al capitolo 1. | 25 nov 2026 |
| 30 fps | Non ancora | 23,1-25,7 fps, tempo per fotogramma mediano di 40-43 ms contro l'obiettivo di 33,3 ms (RTX 5090, 4K). | 27 nov 2026 |
| Stabile | In corso | Con le impostazioni predefinite, le sessioni di prova da 200 secondi terminano senza crash, senza reset del driver GPU e senza perdita del dispositivo. Il blocco all'ingresso nel mondo è diventato più raro (1 sessione su 8 a macchina scarica nel batch 4), ma non è sparito ([#78](https://github.com/kylepelham/El1xr/issues/78)), e i difetti visivi cambiano ancora da una sessione all'altra (vedi Problemi noti). | 27 nov 2026 |

## Prestazioni

![In breve: 23,1-25,7 fps, 27-28 s dall'avvio al mondo, 0 volti neri in 24 istantanee del diner con gli input dei bake serviti per primi, 31 fotogrammi neri mostrati durante il caricamento invece di 152](../images/stats.svg)

| Misura (RTX 5090, 4K) | Ora | Obiettivo |
|---|---|---|
| Frame rate su una sessione di 200 s | **23,1-25,7 fps** | 30+ fps |
| Tempo per fotogramma mediano | 40-43 ms | 33,3 ms o meno |
| Fotogramma al 99° percentile a regime | 64-69 ms | nessuno scatto |
| Tempo CPU per preparare un fotogramma | 26-28 ms | sotto il tempo della GPU |
| Pipeline GPU rifiutate | **0** | 0 |
| Dall'avvio al primo fotogramma del mondo | 27-28 s | |
| Blocco all'ingresso nel mondo | 6,6 s in 1 sessione su 8 a macchina scarica; fotogramma peggiore di 1,2-3,0 s nelle altre ([#78](https://github.com/kylepelham/El1xr/issues/78)) | nessuno |
| Fotogrammi oltre 250 ms nel mondo | 10-12 per sessione | nessuno |
| Fotogrammi neri mostrati nei primi 90 s | 31 | nessuno |
| Streaming dopo l'ingresso nel mondo | 71-72 s | pochi secondi |

Questi valori vengono dal batch 4 (27 novembre 2026), con gli switch che il launcher ora attiva. Frame rate, tempi per fotogramma, tempo CPU, fotogrammi oltre 250 ms e streaming vengono da due sessioni da 200 secondi a macchina scarica: ognuna parte con l'avvio rapido da un checkpoint fisso mentre il resto del lavoro sulla macchina è sospeso, e i valori vengono letti dal registro dei fotogrammi dell'emulatore stesso. Il tempo di avvio viene da due sessioni con i nuovi switch di caricamento, alternate a due sessioni senza (32-33 s), mentre su altri core della CPU giravano delle compilazioni. I fotogrammi neri sono stati contati in una sessione di 90 secondi a macchina scarica. In ogni sessione era attiva una parte dei log diagnostici.

![Dove va un fotogramma: passate del gioco 22,8 ms, lavoro GPU di El1xr 7,0 ms, non misurato 3,8 ms, GPU inattiva 6,2 ms, rispetto alla soglia di 33,3 ms per i 30 fps](../images/frame-budget.svg)

L'ultimo profilo dei tempi GPU, misurato prima del batch 3, indicava circa 40 ms per un fotogramma stabilizzato. Le passate di rendering del gioco stesso occupano 22,8 ms di tempo GPU, il che lascia spazio per i 30 fps. Il resto è il lavoro di El1xr sulla GPU (7 ms: controlli dei limiti sui draw indiretti, copie di texture tra formati, caricamenti) e il tempo in cui la GPU resta inattiva (6-10 ms) mentre un solo thread della CPU prepara il lotto di lavoro successivo. Sono queste due voci il bersaglio dei prossimi aggiornamenti.

![Batch 4: con gli switch di caricamento, dall'avvio al primo fotogramma del mondo da 32-33 s a 27-28 s; tenendo l'ultimo fotogramma, fotogrammi neri mostrati nei primi 90 s da 152 a 31; con gli input dei bake serviti per primi, istantanee del diner con un volto nero da 0-3 per sessione a 0](../images/load-times.svg)

Il batch 4 (27 novembre 2026) ha tenuto quattro modifiche, ora attive per impostazione predefinita nel launcher. Gli switch del batch 3 restano attivi.

- **Prima gli input dei bake** (`VICTORY_BAKE_INPUTS=1`): i bake di volti e pelle ricevono le loro texture di input prima di tutto il resto. Nel diner nessuna delle 24 istantanee di due sessioni mostrava un volto nero, contro 0-3 per sessione senza lo switch, senza costi misurabili sul frame rate.
- **Caricamento più rapido** (`VICTORY_LOAD_FPS=60`, `VICTORY_LOAD_END=world`, `VICTORY_LOAD_UNPACED=1`): la schermata di caricamento gira fino a 60 fps finché non compare il mondo. Il tempo dall'avvio al primo fotogramma del mondo è sceso da 32-33 s a 27-28 s in sessioni consecutive, senza nuovi blocchi all'ingresso nel mondo.
- **Tenere l'ultimo fotogramma** (`VICTORY_HOLD_LAST_FRAME=1`): quando il gioco presenta un fotogramma tutto nero, sullo schermo resta l'ultimo fotogramma buono. In una sessione di 90 secondi a macchina scarica ha coperto 40 dei 71 fotogrammi neri presentati dal gioco; i 31 rimasti arrivano all'inizio dell'avvio, quando non c'è ancora un fotogramma da tenere. Una sessione analoga senza lo switch ha mostrato 152 fotogrammi neri.
- **Menu delle impostazioni in gioco** (`VICTORY_MENU=1`): premi F9, o tieni premuti View e Menu per un secondo, con la finestra del gioco in primo piano. Un autotest nel batch successivo lo ha aperto due volte sopra il gioco in corso, ha disegnato tutte le 10 schede e alla chiusura ha restituito i comandi al gioco.

Rispetto a una sessione con tutti gli switch spenti, quelli del batch 3 hanno anche ridotto i fotogrammi neri dei primi 90 s da 116 a 77, e il fotogramma peggiore all'ingresso nel mondo da 5,8 s a 1,7-2,7 s. Altri otto switch sono stati scartati: uno strappava il muro e la branda della cella della prigione (`SPLIT_AHEAD`), due rendevano volti o la cella neri o rumorosi (`EXCHANGE_PAIRS`, `GPU_DETILE`), uno riportava i volti neri all'ingresso nel mondo pur facendo risparmiare 5 s di caricamento (`BAKE_ONE_BUDGET`), uno aggiungeva 13 ms di tempo CPU per fotogramma (`FLUSH_ONE_PASS`), uno perdeva modifiche alle texture (`TEXTURE_HASH_SAMPLED`), e due non cambiavano nulla di misurabile (`CARD_DECL_JOIN`, `STALE_BATCH`). Le prove hanno anche mostrato che l'upscaling in 4K mandava in crash il gioco (una divisione per zero, 0xc0000094); una correzione è scritta e attende la sua prova. Dopo aver giocato con questa build, il maintainer l'ha descritta come «molto più stabile, meno scatti, frame rate più regolare, meno sfarfallio»: è un'impressione di gioco, non una misura.

### Macchina di prova

| Componente | Modello |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, driver 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 core / 32 thread |
| Memoria | 64 GB DDR5-6000 |
| Scheda madre | MSI PRO B650-P WIFI |
| Archiviazione | SSD NVMe (Samsung 990 EVO Plus) |
| Sistema operativo | Windows 11 (build 26200) |
| Gioco | GTA VI per Xbox Series, build 1.0.2.1 (la tua copia) |

Tutti i dati attuali provengono da questa macchina. I dati meno recenti in questo README sono stati misurati su una RTX 4070 SUPER (Windows) o su una RX 9070 XT (Linux), e ciascuno lo indica.

## Come funziona

![Come funziona: il codice x86-64 del gioco viene eseguito in modo nativo sulla CPU; le chiamate di sistema della console passano al kernel HLE su Windows; i comandi D3D12.X vengono ricostruiti come Direct3D 12 del sistema host; gli shader RDNA 2 vengono tradotti in DXIL e messi in cache](../images/how-it-works.svg)

- **Il codice del gioco** viene caricato ed eseguito così com'è. Xbox Series e PC Windows condividono il set di istruzioni x86-64, quindi nessuna istruzione viene emulata.
- **Le chiamate di sistema** al kernel della console e alle librerie GDK vengono gestite dall'emulazione ad alto livello (HLE) di El1xr sopra Windows: memoria, thread, file e servizi di sistema.
- **Grafica**: le liste di comandi Direct3D 12.X, la memoria e le risorse della console vengono ricostruite come Direct3D 12 standard sulla tua GPU.
- **Gli shader** compilati per la GPU RDNA 2 della console vengono tradotti in DXIL. Tutti i 26.148 programmi shader del gioco vengono tradotti, e ciascuno viene messo in cache su disco dopo il primo utilizzo.

## Avvialo

Servono la tua copia del gioco, Windows 10 o 11, una GPU con Shader Model 6.6 e mesh shader, e Rust.

```powershell
tools\lancer_victory.ps1 -GameDir <cartella del gioco>
```

La guida completa (installazione, impostazioni, risoluzione dei problemi) è in [docs/running-gta6.md](../running-gta6.md).

> [!IMPORTANT]
> Questo repository non contiene file del gioco, chiavi né eseguibili, e non spiega come ottenerli.

<details>
<summary>Comandi per sviluppatori</summary>

```powershell
python tools/dev.py build      # build release
python tools/dev.py prewarm    # traduce tutti gli shader offline (circa 45 s)
python tools/dev.py bench --quiet --seconds 200   # sessione di gioco misurata
python tools/dev.py gate       # tutti i test, fallisce a ogni avviso
```

</details>

## Problemi noti

- **Difetti visivi che cambiano da una sessione all'altra.** I volti neri o macchiati sono più rari con gli input dei bake serviti per primi, ma compaiono ancora in alcune sessioni ([#35](https://github.com/kylepelham/El1xr/issues/35)). Lucia può ancora indossare l'abito sbagliato nel diner, e i suoi capelli possono diventare arancioni alla finestra della prigione.
- **Un blocco all'ingresso nel mondo.** È più raro che nel batch 3, dove quasi ogni sessione si bloccava per 3-9 s, ma una delle otto sessioni a macchina scarica del batch 4 si è ancora bloccata per 6,6 s mentre la GPU elaborava un lotto di comandi. La causa è in esame in [#78](https://github.com/kylepelham/El1xr/issues/78).
- **Streaming lento.** Dopo l'ingresso nel mondo servono circa 71 s perché arrivino tutti gli asset, con elementi che compaiono all'improvviso nel frattempo.
- **L'upscaling va in crash in 4K.** Con l'upscaling DLSS o FSR attivo, il gioco va in crash appena compare il mondo (una divisione per zero nel codice FSR 4 del gioco). Una correzione attende la sua prova; l'upscaling è spento per impostazione predefinita.
- **La GPU attende la CPU** per 6-10 ms a fotogramma (vedi la ripartizione del fotogramma qui sopra).
- **L'illuminazione in ray tracing è in gran parte disattivata.** Il gioco costruisce le sue strutture di ray tracing via software, e quel builder per ora è disattivato.
- **Una sola macchina di riferimento.** I dati attuali vengono dalla RTX 5090 indicata sopra. Le altre GPU non sono ancora state misurate con la build attuale.

## Prossimi passi

L'obiettivo sono 30 fps stabili in 4K sulla RTX 5090, poi nessuno scatto e nessun difetto visivo. Circa 60 ulteriori accelerazioni sono già scritte e in attesa di essere misurate. Ciascuna resta disattivata per impostazione predefinita finché non è dimostrata.

- **Il blocco all'ingresso nel mondo** ([#78](https://github.com/kylepelham/El1xr/issues/78)): capire quale passata di quel lotto di comandi GPU impiega secondi.
- **Meno attese e meno overhead**: ridurre i 6-10 ms che la GPU passa ad attendere la CPU a ogni fotogramma, e i 7 ms di lavoro GPU propri di El1xr.
- **Upscaling DLSS** (una volta provata la correzione del crash) e **ray tracing hardware sui core RT**, che riporterebbe l'illuminazione in ray tracing.
- **Preriscaldamento offline di shader e pipeline**, in modo che nessuno shader venga compilato durante il gioco.
- **Correzioni per i volti neri ([#35](https://github.com/kylepelham/El1xr/issues/35)) e gli abiti che cambiano da una sessione all'altra**.

## In numeri

| Dato | Cosa |
|---|---|
| 1800+ test automatizzati | strumenti Python e crate Rust, eseguiti da `dev.py gate` |
| 26.148 programmi shader | tradotti da RDNA 2 a DXIL, in cache su disco |
| 0 pipeline rifiutate | sulla RTX 5090 |

## Contribuire

Issue e pull request sono benvenute. Esegui `python tools/dev.py gate` prima di aprire una pull request. Le regole sono in [CONTRIBUTING.md](../../CONTRIBUTING.md). Domande e resoconti dei test vanno a Bone/Nevaa su Discord: **bonemeld**.

## Riconoscimenti

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak e i suoi collaboratori ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 o successiva), e gli autori dei fork da cui è stato assemblato, tra cui Neptune (VIctory Edit.) di Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty e i suoi collaboratori, il progetto da cui El1xr è stato derivato.
- **I fork di VIctory da cui abbiamo imparato e portato codice**, tra cui fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory e readmet3xt-lab/foxhound.
- Ogni componente e ogni autore, modifica per modifica, è citato in [THIRD_PARTY.md](../../THIRD_PARTY.md). La storia precedente del progetto è in [docs/history.md](../history.md).

## Licenza

El1xr è distribuito sotto la [GNU GPL 3.0 o successiva](../../LICENSE). Le parti di terzi mantengono la propria licenza: il modulo `Task` di libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) e le tabelle RDNA 2 generate dai test pubblici di codifica di LLVM (Apache 2.0 con eccezione LLVM, `PrototypeScarlett/emu/rdna2_ops.py` e `rdna2_shapes.py`).
