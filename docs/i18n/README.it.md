<div align="center">

![El1xr: Grand Theft Auto VI per Xbox Series in esecuzione su un PC Windows. 21,6-26,0 fps in 4K su una RTX 5090, 1 pipeline rifiutata, 24,6 s dall'avvio al mondo di gioco, giocabile al 28 novembre 2026](../images/hero.svg)

![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-9b5cff?style=flat-square&labelColor=170f2b)
![Platform: Windows 10 and 11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2de2e6?style=flat-square&labelColor=170f2b)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-ff8a3d?style=flat-square&labelColor=170f2b&logo=rust&logoColor=white)
![Status: playable](https://img.shields.io/badge/status-playable-ff3d8b?style=flat-square&labelColor=170f2b)
![21.6-26.0 fps at 4K on RTX 5090](https://img.shields.io/badge/4K%20on%20RTX%205090-21.6--26.0%20fps-ffd36e?style=flat-square&labelColor=170f2b)
![Discord: bonemeld](https://img.shields.io/badge/Discord-bonemeld-5865F2?style=flat-square&labelColor=170f2b&logo=discord&logoColor=white)

[English](../../README.md) · [Français](README.fr.md) · [Español](README.es.md) · [Deutsch](README.de.md) · **Italiano** · [Português](README.pt-BR.md) · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

El1xr esegue la versione Xbox Series di Grand Theft Auto VI (build 1.0.2.1) su un PC Windows. È il progetto di Bone/Nevaa. Il codice x86-64 del gioco viene eseguito in modo nativo sul tuo processore. Le sue chiamate di sistema della console, i comandi Direct3D 12.X e gli shader RDNA 2 vengono tradotti per PC durante l'esecuzione. Non c'è alcuna emulazione della CPU e nessun contenuto del gioco in questo repository: la copia del gioco la porti tu.

## Stato

> [!NOTE]
> **Al 28 novembre 2026, GTA VI (Xbox Series, build 1.0.2.1) si avvia, raggiunge i menu ed è giocabile nel mondo aperto su Windows.** Nelle ultime sessioni di prova a macchina scarica (batch 5, 4K, RTX 5090) il gioco gira a 21,6-22,4 fps in sessioni di 110 secondi dall'avvio e a 25,8-26,0 fps in sessioni di 180 e 460 secondi. È ancora sotto il prossimo obiettivo, 30 fps stabili. Poi vengono l'assenza di scatti e di difetti visivi.

![Traguardi: avvio, menu principale e mondo di gioco entro il 24 novembre 2026; giocabile dal 27 novembre 2026, ora a 21,6-26,0 fps; prossimo traguardo i 30 fps; stabilità in corso](../images/milestones.svg)

| Traguardo | Stato | Riscontro | Data |
|---|---|---|---|
| Eseguibile avviato | Fatto | L'eseguibile Xbox Series 1.0.2.1 si avvia su Windows. Il suo codice x86-64 viene eseguito in modo nativo. | entro il 24 nov 2026 |
| Primo fotogramma | Fatto | L'animazione iniziale e le schermate legali vengono visualizzate senza sfarfallio. | entro il 24 nov 2026 |
| Menu principale | Fatto | Schermata del profilo, impostazioni del primo avvio, schermate legali, schermata di caricamento e menu di pausa funzionano tutti. I menu girano a 30 fps (RTX 4070 SUPER). | entro il 24 nov 2026 |
| In gioco | Fatto | Il gioco raggiunge il mondo aperto e il personaggio si muove sotto il controllo del giocatore. Una sessione di 13 minuti nella prigione è girata a circa 4 fps (RTX 4070 SUPER). | entro il 24 nov 2026 |
| Giocabile | Fatto, sotto la velocità obiettivo | Il gioco gira a 21,6-26,0 fps in 4K (RTX 5090; sessioni del batch 5 a macchina scarica, da 110 a 460 secondi). | 28 nov 2026 |
| Avanzamento della storia | Prime due missioni completate | Un collaboratore ha completato le prime due missioni in una sessione di 45 minuti a circa 2 fps (RTX 4070 SUPER; riferito, non misurato dai log). Tutte le cutscene vengono riprodotte per intero. Su Linux (Proton, RX 9070 XT) una sessione di 23 minuti ha superato il prologo ed è arrivata al capitolo 1. | 25 nov 2026 |
| 30 fps | Non ancora | 21,6-26,0 fps, ossia 38-46 ms per fotogramma, contro l'obiettivo di 33,3 ms (RTX 5090, 4K). | 28 nov 2026 |
| Stabile | In corso | Con le impostazioni predefinite, le sessioni di prova del batch 5 non hanno avuto reset del driver GPU né perdite del dispositivo. Il blocco di 8-9 s all'ingresso nel mondo, visto in 5 sessioni su 5 all'inizio del batch 5, non si è ripresentato nei 47 avvii controllati da quando i record indiretti grezzi vengono verificati. Il gioco può ancora andare in crash quando Windows esaurisce la memoria di commit, e i difetti visivi cambiano ancora da una sessione all'altra (vedi Problemi noti). | 28 nov 2026 |

## Prestazioni

![In breve: 21,6-26,0 fps, 24,6 s dall'avvio al primo fotogramma del mondo (prima 26,7 s), 0 lampi scuri di un fotogramma nel diner (prima 12-19 per sessione), 2,9 GB di memoria buffer GPU dopo 180 s (prima 4,5 GB)](../images/stats.svg)

| Misura (RTX 5090, 4K) | Ora | Obiettivo |
|---|---|---|
| Frame rate, sessioni di 110 s dall'avvio | **21,6-22,4 fps** | 30+ fps |
| Frame rate, sessioni di 180 s e 460 s | **25,8-26,0 fps** | 30+ fps |
| Tempo CPU per preparare un fotogramma | 15,7-21,2 ms | sotto il tempo della GPU |
| Pipeline GPU rifiutate | 1 | 0 |
| Dall'avvio al primo fotogramma del mondo | 23,1-24,6 s | |
| Blocco all'ingresso nel mondo | nessuno in 47 avvii; fotogramma peggiore di una sessione 771-832 ms | nessuno scatto |
| Fotogrammi oltre 250 ms nel mondo | 3-6 per sessione di 110 s | nessuno |
| Lampi scuri di un fotogramma nel diner | 0 (prima 12-19 per sessione) | nessuno |
| Memoria buffer GPU dopo 180 s | 2,9 GB (prima 4,5 GB) | |

Questi valori vengono dal batch 5 (concluso il 28 novembre 2026), con gli switch che il launcher ora attiva. Ogni sessione era a macchina scarica: il resto del lavoro sulla macchina era sospeso o trattenuto, il gioco partiva con l'avvio rapido da un checkpoint fisso, e i valori sono stati letti dal registro dei fotogrammi dell'emulatore stesso (frame rate e tempo CPU sono mediane sulla seconda metà dei fotogrammi di una sessione nel mondo). I valori a 110 secondi vengono da due coppie di caricamento consecutive, quello a 180 secondi dalla coppia di `CARD_BLOCK_REUSE` e quello a 460 secondi da un avvio di prova sulla CPU. Il frame rate dipende dalla scena e dalla durata della sessione, quindi confronta i valori solo all'interno di una coppia. In ogni sessione era attiva una parte dei log diagnostici.

![Dove va un fotogramma: passate del gioco 22,8 ms, lavoro GPU di El1xr 7,0 ms, non misurato 3,8 ms, GPU inattiva 6,2 ms, rispetto alla soglia di 33,3 ms per i 30 fps](../images/frame-budget.svg)

L'ultimo profilo dei tempi GPU, misurato prima del batch 3, indicava circa 40 ms per un fotogramma stabilizzato, di cui 22,8 ms servono alle passate di rendering del gioco stesso. Il batch 5 ha mostrato che ora il fotogramma è limitato dal lavoro della CPU che lo prepara e lo invia, non dalla GPU: con DLSS che renderizza a due terzi del 4K, il frame rate reale è rimasto a 21,8-22,0 fps, lo stesso del 4K nativo (20,1-22,2 fps). Per questo la maggior parte delle modifiche tenute nel batch 5 riduce il tempo CPU per fotogramma.

![Batch 5: con il pacchetto di caricamento, dall'avvio al primo fotogramma del mondo da 26,2-26,7 s a 23,1-24,6 s; con TABLE_NULL_ONCE, lampi scuri di un fotogramma nel diner da 12-19 per sessione a 0; con HEAP_WARM, il primo fotogramma del mondo ha richiesto 614-702 ms invece di 781-1048 ms](../images/load-times.svg)

Il batch 5 ha tenuto queste modifiche, ora attive per impostazione predefinita nel launcher. Gli switch dei batch 3 e 4 restano attivi.

- **Caricamento più rapido** (`VICTORY_TEXTURE_HEAPS=1`, `VICTORY_TEXTURE_BLOCK_THREAD=1`, `VICTORY_DSTORAGE_STAGING_MB=256`, `VICTORY_DSTORAGE_POLL_WAIT=1`, `VICTORY_BAKE_STAGE_ALL=1`): le texture in streaming condividono grandi blocchi di memoria creati su un thread dedicato, fino a 256 MiB di letture DirectStorage sono in corso contemporaneamente invece di 32, il controllo del gioco sulle letture da disco completate ritorna non appena queste finiscono, e le passate di bake vengono preparate insieme durante il caricamento. Nella coppia di conferma a macchina scarica, il tempo dall'avvio al primo fotogramma del mondo è sceso da 26,7 a 24,6 s, quello fino al primo fotogramma completo del mondo da 29,2 a 26,2 s, e il fotogramma peggiore da 1049 a 832 ms, a parità di frame rate. La prima coppia concordava (da 26,2 a 23,1 s).
- **Sfarfallio del diner corretto** (`VICTORY_TABLE_NULL_ONCE=1`): nel diner lo sfondo diventava scuro per un fotogramma circa ogni 2 s, 12-19 volte per sessione. Una tabella di descrittori svuotava lo slot di una texture ogni volta che il lavoro di preparazione di quella texture veniva scartato; ora uno slot viene svuotato al massimo una volta. Con lo switch attivo non è comparso nessun fotogramma del genere né in una sessione di 110 secondi né in due segmenti di prova da 18 secondi, senza costi misurati.
- **Niente più blocco all'ingresso nel mondo** (`VICTORY_RAW_CHECK=2`): i record di draw indiretti scritti dagli shader del gioco vengono verificati prima di essere eseguiti. All'inizio del batch 5 ogni sessione si bloccava per 8,2-9,0 s circa 4 s dopo l'ingresso nel mondo (5 su 5); nessuno dei 47 avvii controllati dopo questa modifica si è bloccato.
- **Meno tempo CPU per fotogramma**:
  - `VICTORY_SKIP_WC_BACK=1`: le grandi scritture degli shader nella memoria write-combining restano sulla scheda invece di essere ricopiate indietro. 24,6 fps in 2 sessioni su 2, contro 21,5 fps nella migliore sessione a macchina scarica senza lo switch, e 7,3 ms di tempo CPU in meno per fotogramma.
  - `VICTORY_CARD_BLOCK_REUSE=1`: i pezzi di memoria GPU vengono azzerati e riutilizzati invece di essere allocati di nuovo. Da 23,6 a 26,0 fps in una coppia di 180 secondi a macchina scarica, con 2,9 GB di memoria buffer GPU alla fine invece di 4,5 GB.
  - `VICTORY_SEND_THREAD=1` e `VICTORY_CARD_RECORD_SEND=1`: le liste di comandi vengono inviate, e le liste di sincronizzazione GPU registrate, su un thread dedicato. Il tempo di invio sul thread del fotogramma è sceso da 1,56 a 0,11 ms.
  - `VICTORY_FLUSH_BESIDE=1`, `VICTORY_RESET_ASIDE=1`, `VICTORY_COPIED_SHARED=1`, `VICTORY_TEXTURE_HASH_BEHIND=1`, `VICTORY_RESULT_RING=1`, e `VICTORY_WIDE_WAKE_ONE=1` con `VICTORY_WAIT_SPIN=50`: tagli minori di 0,5-1,8 ms di tempo CPU per fotogramma ciascuno, o fino a 1,4 fps, misurati attivando e disattivando ciascuno all'interno di una singola sessione.
- **Ingresso nel mondo più rapido** (`VICTORY_HEAP_WARM=1`): le copie host degli heap di view del gioco vengono create durante la schermata di caricamento, così il primo fotogramma del mondo ha richiesto 614 ms invece di 1048 ms (seconda coppia; prima coppia da 781 a 702 ms).
- **Meno fotogrammi lenti** (`VICTORY_AMBIENT_ONCE=1`): il sostituto dell'illuminazione ambientale del gioco conta un solo rimbalzo. Il tempo per fotogramma al 99° percentile è stato inferiore di 11 ms in una prova all'interno della stessa sessione.

L'upscaling in 4K non va più in crash: tre avvii con l'upscaling sono girati senza divisioni per zero (0xc0000094). L'upscaling DLSS appare nitido ma non aumenta il frame rate, perché il fotogramma attende la CPU. La frame generation DLSS (`-FrameGen`, oppure Frame generation = dlss nel menu F9) presenta circa 44-47 fps a partire da 22-24 fotogrammi reali; resta spenta per impostazione predefinita finché i fotogrammi generati non saranno stati controllati a occhio. Il batch 5 ha anche scartato switch che rompevano qualcosa o costavano velocità, tra cui `PIPELINE_PIECES` (schermo nero), `BAKE_SPREAD=12` (1,2-2,2 fps in meno) e `GUARD_PUBLISH_TRY` (un crash). Il builder di ray tracing del gioco funziona quando viene attivato, ma gira a 0,6-1,1 fps, quindi resta spento.

### Macchina di prova

| Componente | Modello |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, driver 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 core / 32 thread |
| Memoria | 64 GB DDR5-6000 |
| File di paging | 64 GB (limite di commit 142 GB) |
| Scheda madre | MSI PRO B650-P WIFI |
| Archiviazione | SSD NVMe (Samsung 990 EVO Plus) |
| Sistema operativo | Windows 11 (build 26200) |
| Gioco | GTA VI per Xbox Series, build 1.0.2.1 (la tua copia) |

Tutti i dati attuali provengono da questa macchina. I dati meno recenti in questo README sono stati misurati su una RTX 4070 SUPER (Windows) o su una RX 9070 XT (Linux), e ciascuno lo indica.

## Come funziona

![Come funziona: il codice x86-64 del gioco viene eseguito in modo nativo sulla CPU; le chiamate di sistema della console passano al kernel HLE su Windows; i comandi D3D12.X vengono ricostruiti come Direct3D 12 del sistema host; gli shader RDNA 2 vengono tradotti in DXIL e messi in cache](../images/how-it-works.svg)

- **Il codice del gioco** viene caricato ed eseguito così com'è. Xbox Series e PC Windows condividono il set di istruzioni x86-64, quindi nessuna istruzione viene emulata.
- **Le chiamate di sistema** al kernel della console e alle librerie GDK vengono gestite dall'emulazione ad alto livello (HLE) di El1xr sopra Windows: memoria, thread, file e servizi di sistema.
- **Letture da disco**: le richieste DirectStorage del gioco vengono servite dai thread worker di El1xr, con fino a 256 MiB in corso. Le texture in streaming vengono collocate in blocchi di memoria condivisi: circa 115.000 texture in 116 blocchi in una sessione tipica.
- **Grafica**: le liste di comandi Direct3D 12.X, la memoria e le risorse della console vengono ricostruite come Direct3D 12 standard sulla tua GPU. Le liste di comandi vengono inviate da un thread dedicato, così il thread del fotogramma del gioco svolge meno di quel lavoro.
- **Heap di descrittori**: il gioco indirizza fino a 1.245.184 slot di descrittori (la sua tabella di view di texture e buffer). El1xr li rispecchia in un unico heap host di circa 1.000.000 di slot, condiviso con le proprie view, e crea le copie host durante la schermata di caricamento. Come far entrare il resto è ancora una questione aperta (vedi Problemi noti).
- **Gli shader** compilati per la GPU RDNA 2 della console vengono tradotti in DXIL. Tutti i 26.148 programmi shader del gioco tranne 6 vengono tradotti, e ogni traduzione viene messa in cache su disco. `dev.py prewarm` costruisce le 25.751 pipeline GPU del gioco prima di una sessione, così non vengono compilate durante il gioco.

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

- **Il davanzale della finestra della prigione diventa scuro dopo il prologo.** Nell'inquadratura della cella dopo il titolo PROLOGUE, il davanzale illuminato dal sole diventa scuro e perde i dettagli da un fotogramma all'altro, con la stessa telecamera. Riprodotto due volte nel batch 5; la causa è in esame.
- **La pelle può diventare nera.** In alcune sessioni la pelle di un personaggio, più spesso nel diner, diventa completamente nera per qualche secondo e poi torna normale ([#35](https://github.com/kylepelham/El1xr/issues/35)). Anche la camicia della cameriera cambia colore da una sessione all'altra.
- **Quadrati scuri su una finestra.** Nella cutscene iniziale, a circa 44 s, dei quadrati scuri disposti a gradini coprono la finestra dietro Jason. Il batch 5 ha escluso due cause sospette; la ricerca continua.
- **Limite dell'heap di descrittori.** Il gioco indirizza fino a 1.245.184 slot di descrittori, ma l'heap host sulla RTX 5090 ne contiene circa 1.000.000. Nelle sessioni del batch 5, da 313.000 a 504.000 view attive del gioco si trovavano oltre la parte che l'heap host può contenere. Finora non è stato tenuto alcun modo per farle entrare.
- **Lo schermo del telefono è nero** nell'inquadratura del tavolo nel diner.
- **Uso elevato di memoria.** Il processo del gioco raggiunge picchi di 33-35 GB. Con un file di paging troppo piccolo va in crash quando Windows esaurisce la memoria di commit. Il PC di prova usa un file di paging da 64 GB.
- **Streaming lento.** Dopo l'ingresso nel mondo servono circa 72 s perché arrivino tutti gli asset, con elementi che compaiono all'improvviso nel frattempo.
- **Il fotogramma attende la CPU.** Preparare e inviare un fotogramma richiede 15,7-21,2 ms di tempo CPU, ed è per questo che l'upscaling non aumenta il frame rate.
- **L'illuminazione in ray tracing è disattivata.** Il gioco costruisce le sue strutture di ray tracing via software; quel builder funziona ma gira a 0,6-1,1 fps, quindi resta spento.
- **Una sola macchina di riferimento.** I dati attuali vengono dalla RTX 5090 indicata sopra. Le altre GPU non sono ancora state misurate con la build attuale.

## Prossimi passi

L'obiettivo sono 30 fps stabili in 4K sulla RTX 5090, poi nessuno scatto e nessun difetto visivo. Ogni nuova modifica resta disattivata per impostazione predefinita finché sessioni di prova a macchina scarica non la confermano.

- **Meno tempo CPU per fotogramma**: il fotogramma è limitato dalla CPU, quindi i prossimi batch continuano a spostare il lavoro di invio e di controllo delle texture fuori dal thread del fotogramma.
- **Il davanzale e i quadrati sulla finestra**, e **la pelle che diventa nera** ([#35](https://github.com/kylepelham/El1xr/issues/35)).
- **L'heap di descrittori**: far entrare i 1.245.184 slot del gioco nell'heap host.
- **Ray tracing hardware sui core RT**, che riporterebbe l'illuminazione in ray tracing.
- **Frame generation DLSS attiva per impostazione predefinita**, una volta che i fotogrammi generati saranno stati controllati a occhio.

## In numeri

| Dato | Cosa |
|---|---|
| 4000+ test superati | crate Rust (`cargo test`) e strumenti Python (`pytest`), eseguiti da `dev.py gate` |
| 26.148 programmi shader | tradotti da RDNA 2 a DXIL e messi in cache su disco; 6 non vengono ancora tradotti |
| 25.751 pipeline GPU | costruite prima del gioco da `dev.py prewarm` |
| 1 pipeline rifiutata | sulla RTX 5090, nelle ultime sessioni |

## Contribuire

Issue e pull request sono benvenute. Esegui `python tools/dev.py gate` prima di aprire una pull request. Le regole sono in [CONTRIBUTING.md](../../CONTRIBUTING.md). Domande e resoconti dei test vanno a Bone/Nevaa su Discord: **bonemeld**.

## Riconoscimenti

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak e i suoi collaboratori ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 o successiva), e gli autori dei fork da cui è stato assemblato, tra cui Neptune (VIctory Edit.) di Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty e i suoi collaboratori, il progetto da cui El1xr è stato derivato.
- **I fork di VIctory da cui abbiamo imparato e portato codice**, tra cui fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory, readmet3xt-lab/foxhound, RipcuBAN/victory-fork e Kemerd/VIctory-NVIDIA-RT.
- Ogni componente e ogni autore, modifica per modifica, è citato in [THIRD_PARTY.md](../../THIRD_PARTY.md). La storia precedente del progetto è in [docs/history.md](../history.md).

## Licenza

El1xr è distribuito sotto la [GNU GPL 3.0 o successiva](../../LICENSE). Le parti di terzi mantengono la propria licenza: il modulo `Task` di libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) e le tabelle RDNA 2 generate dai test pubblici di codifica di LLVM (Apache 2.0 con eccezione LLVM, `PrototypeScarlett/emu/rdna2_ops.py` e `rdna2_shapes.py`).
