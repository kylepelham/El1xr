<div align="center">

![El1xr: Grand Theft Auto VI para Xbox Series, funcionando en un PC con Windows. 21,6-26,0 fps en 4K en una RTX 5090, 1 pipeline rechazado, 24,6 s desde el arranque hasta el mundo, jugable a 28 de noviembre de 2026](../images/hero.svg)

![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-9b5cff?style=flat-square&labelColor=170f2b)
![Platform: Windows 10 and 11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2de2e6?style=flat-square&labelColor=170f2b)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-ff8a3d?style=flat-square&labelColor=170f2b&logo=rust&logoColor=white)
![Status: playable](https://img.shields.io/badge/status-playable-ff3d8b?style=flat-square&labelColor=170f2b)
![21.6-26.0 fps at 4K on RTX 5090](https://img.shields.io/badge/4K%20on%20RTX%205090-21.6--26.0%20fps-ffd36e?style=flat-square&labelColor=170f2b)
![Discord: bonemeld](https://img.shields.io/badge/Discord-bonemeld-5865F2?style=flat-square&labelColor=170f2b&logo=discord&logoColor=white)

[English](../../README.md) · [Français](README.fr.md) · **Español** · [Deutsch](README.de.md) · [Italiano](README.it.md) · [Português](README.pt-BR.md) · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

El1xr ejecuta la versión de Xbox Series de Grand Theft Auto VI (build 1.0.2.1) en un PC con Windows. Es el proyecto de Bone/Nevaa. El propio código x86-64 del juego se ejecuta de forma nativa en tu procesador. Sus llamadas al sistema de la consola, sus comandos Direct3D 12.X y sus shaders RDNA 2 se traducen para PC mientras el juego se ejecuta. No hay emulación de CPU ni contenido del juego en este repositorio: tienes que aportar tu propia copia.

## Estado

> [!NOTE]
> **A 28 de noviembre de 2026, GTA VI (Xbox Series, build 1.0.2.1) arranca, llega a los menús y es jugable en el mundo abierto en Windows.** En las últimas pruebas con el equipo en reposo (lote 5, 4K, RTX 5090), el juego funciona a 21,6-22,4 fps en pruebas de 110 segundos desde el arranque y a 25,8-26,0 fps en pruebas de 180 y 460 segundos. Aún queda por debajo del siguiente objetivo, 30 fps estables. Después vendrá eliminar los tirones y los fallos visuales.

![Hitos: arranque, menú principal y partida a más tardar el 24 de noviembre de 2026; jugable el 27 de noviembre de 2026, ahora a 21,6-26,0 fps; 30 fps a continuación; estabilidad en curso](../images/milestones.svg)

| Hito | Estado | Pruebas | Fecha |
|---|---|---|---|
| El ejecutable funciona | Hecho | El ejecutable de Xbox Series 1.0.2.1 arranca en Windows. Su código x86-64 se ejecuta de forma nativa. | a más tardar el 24 nov. 2026 |
| Primer fotograma | Hecho | La animación de inicio y las pantallas legales se muestran sin parpadeos. | a más tardar el 24 nov. 2026 |
| Menú principal | Hecho | La pantalla de perfil, los ajustes del primer arranque, las pantallas legales, la pantalla de carga y el menú de pausa funcionan. Los menús van a 30 fps (RTX 4070 SUPER). | a más tardar el 24 nov. 2026 |
| En partida | Hecho | El juego llega al mundo abierto y el personaje se mueve bajo el control del jugador. Una sesión de 13 minutos en la prisión funcionó a unos 4 fps (RTX 4070 SUPER). | a más tardar el 24 nov. 2026 |
| Jugable | Hecho, por debajo de la velocidad objetivo | El juego funciona a 21,6-26,0 fps en 4K (RTX 5090; pruebas del lote 5 de 110 a 460 segundos con el equipo en reposo). | 28 nov. 2026 |
| Avance en la historia | Dos primeras misiones completadas | Un colaborador completó las dos primeras misiones en una sesión de 45 minutos a unos 2 fps (RTX 4070 SUPER; según su informe, no medido a partir de los registros). Todas las cinemáticas se reproducen completas. En Linux (Proton, RX 9070 XT), una prueba de 23 minutos superó el prólogo y llegó al capítulo 1. | 25 nov. 2026 |
| 30 fps | Todavía no | 21,6-26,0 fps, es decir, 38-46 ms por fotograma, frente al objetivo de 33,3 ms (RTX 5090, 4K). | 28 nov. 2026 |
| Estable | En curso | Con los ajustes por defecto, las pruebas del lote 5 no tuvieron ningún reinicio del controlador de la GPU ni pérdida del dispositivo. El parón de 8-9 s al entrar en el mundo, visto en 5 de 5 pruebas al principio del lote 5, no ha vuelto en los 47 arranques revisados desde que se comprueban los registros indirectos en bruto. El juego todavía puede cerrarse cuando Windows se queda sin memoria confirmada (commit), y los fallos visuales todavía varían de una prueba a otra (ver Problemas conocidos). | 28 nov. 2026 |

## Rendimiento

![De un vistazo: 21,6-26,0 fps, 24,6 s desde el arranque hasta el primer fotograma del mundo (antes 26,7 s), 0 destellos oscuros de un fotograma en el diner (antes 12-19 por prueba), 2,9 GB de memoria de búferes de GPU tras 180 s (antes 4,5 GB)](../images/stats.svg)

| Medida (RTX 5090, 4K) | Ahora | Objetivo |
|---|---|---|
| Tasa de fotogramas, pruebas de 110 s desde el arranque | **21,6-22,4 fps** | 30+ fps |
| Tasa de fotogramas, pruebas de 180 s y 460 s | **25,8-26,0 fps** | 30+ fps |
| Tiempo de CPU para preparar un fotograma | 15,7-21,2 ms | por debajo del tiempo de la GPU |
| Pipelines de GPU rechazados | 1 | 0 |
| Del arranque al primer fotograma del mundo | 23,1-24,6 s | |
| Parón al entrar en el mundo | ninguno en 47 arranques; peor fotograma de una prueba 771-832 ms | sin tirones |
| Fotogramas de más de 250 ms en el mundo | 3-6 por prueba de 110 s | ninguno |
| Destellos oscuros de un fotograma en el diner | 0 (antes 12-19 por prueba) | ninguno |
| Memoria de búferes de GPU tras 180 s | 2,9 GB (antes 4,5 GB) | |

Estas cifras proceden del lote 5 (terminado el 28 de noviembre de 2026), con los switches que el lanzador activa ahora. Todas las pruebas se hicieron con el equipo en reposo: el resto del trabajo de la máquina quedaba en pausa o retenido, el juego hacía un arranque rápido desde un punto de control fijo, y las cifras se leyeron del propio registro de fotogramas del emulador (la tasa de fotogramas y el tiempo de CPU son medianas sobre la segunda mitad de los fotogramas de cada prueba en el mundo). Las cifras de 110 segundos proceden de dos pares consecutivos de pruebas de carga, la de 180 segundos del par de `CARD_BLOCK_REUSE`, y la de 460 segundos de un arranque de prueba de CPU. La tasa de fotogramas depende de la escena y de la duración de la prueba, así que solo deben compararse cifras dentro de un mismo par. En todas las pruebas había parte del registro de diagnóstico activado.

![En qué se va un fotograma: pasadas del juego 22,8 ms, trabajo de GPU de El1xr 7,0 ms, sin medir 3,8 ms, GPU inactiva 6,2 ms, frente a la línea de 33,3 ms de los 30 fps](../images/frame-budget.svg)

El último perfil de tiempos de GPU, tomado antes del lote 3, situaba un fotograma estabilizado en unos 40 ms, de los que las pasadas de renderizado del propio juego necesitan 22,8 ms. El lote 5 mostró que ahora el fotograma está limitado por el trabajo de CPU que lo prepara y lo envía, no por la GPU: con DLSS renderizando a dos tercios de 4K, la tasa de fotogramas real se mantuvo en 21,8-22,0 fps, la misma que en 4K nativo (20,1-22,2 fps). Por eso la mayoría de los cambios que conservó el lote 5 recortan el tiempo de CPU por fotograma.

![Lote 5: con el paquete de carga, del arranque al primer fotograma del mundo pasó de 26,2-26,7 s a 23,1-24,6 s; con TABLE_NULL_ONCE, los destellos oscuros de un fotograma en el diner bajaron de 12-19 por prueba a 0; con HEAP_WARM, el primer fotograma del mundo tardó 614-702 ms en lugar de 781-1048 ms](../images/load-times.svg)

El lote 5 conservó estos cambios, ahora activados por defecto en el lanzador. Los switches de los lotes 3 y 4 siguen activados.

- **Carga más rápida** (`VICTORY_TEXTURE_HEAPS=1`, `VICTORY_TEXTURE_BLOCK_THREAD=1`, `VICTORY_DSTORAGE_STAGING_MB=256`, `VICTORY_DSTORAGE_POLL_WAIT=1`, `VICTORY_BAKE_STAGE_ALL=1`): las texturas que llegan por streaming comparten grandes bloques de memoria creados en un hilo propio, hay hasta 256 MiB de lecturas DirectStorage en curso en lugar de 32, la consulta del juego por las lecturas de disco terminadas responde en cuanto terminan, y las pasadas de bake se preparan juntas durante la carga. En el par de confirmación en reposo, el tiempo desde el arranque hasta el primer fotograma del mundo bajó de 26,7 a 24,6 s, desde el arranque hasta el primer fotograma completo del mundo de 29,2 a 26,2 s, y el peor fotograma de 1049 a 832 ms, con la misma tasa de fotogramas. El primer par coincidió (de 26,2 a 23,1 s).
- **Parpadeo del diner corregido** (`VICTORY_TABLE_NULL_ONCE=1`): en el diner, el fondo se oscurecía durante un fotograma más o menos cada 2 s, 12-19 veces por prueba. Una tabla de descriptores vaciaba la ranura de una textura cada vez que se descartaba el trabajo de preparación de esa textura; ahora una ranura se vacía como mucho una vez. Con el switch activado no apareció ningún fotograma así en una prueba de 110 segundos ni en dos segmentos de prueba de 18 segundos, sin coste medido.
- **Se acabó el parón al entrar en el mundo** (`VICTORY_RAW_CHECK=2`): los registros de draws indirectos que escriben los shaders del juego se comprueban antes de ejecutarse. Al principio del lote 5, todas las pruebas se congelaban 8,2-9,0 s unos 4 s después de entrar en el mundo (5 de 5); ninguno de los 47 arranques revisados después de este cambio se paró.
- **Menos tiempo de CPU por fotograma**:
  - `VICTORY_SKIP_WC_BACK=1`: las escrituras grandes de shaders en memoria write-combining se quedan en la tarjeta en lugar de copiarse de vuelta. 24,6 fps en 2 de 2 pruebas, frente a 21,5 fps en la mejor prueba en reposo sin el switch, y 7,3 ms menos de tiempo de CPU por fotograma.
  - `VICTORY_CARD_BLOCK_REUSE=1`: los fragmentos de memoria de GPU se ponen a cero y se reutilizan en lugar de volver a asignarse. De 23,6 a 26,0 fps en un par de 180 segundos en reposo, con 2,9 GB de memoria de búferes de GPU al final en lugar de 4,5 GB.
  - `VICTORY_SEND_THREAD=1` y `VICTORY_CARD_RECORD_SEND=1`: las listas de comandos se envían, y las listas de sincronización de GPU se graban, en un hilo propio. El tiempo de envío en el hilo del fotograma bajó de 1,56 a 0,11 ms.
  - `VICTORY_FLUSH_BESIDE=1`, `VICTORY_RESET_ASIDE=1`, `VICTORY_COPIED_SHARED=1`, `VICTORY_TEXTURE_HASH_BEHIND=1`, `VICTORY_RESULT_RING=1`, y `VICTORY_WIDE_WAKE_ONE=1` con `VICTORY_WAIT_SPIN=50`: recortes menores, de 0,5-1,8 ms de tiempo de CPU por fotograma cada uno, o de hasta 1,4 fps, medidos activando y desactivando cada uno dentro de una misma prueba.
- **Entrada en el mundo más rápida** (`VICTORY_HEAP_WARM=1`): las copias del sistema anfitrión de los heaps de vistas del juego se crean en la pantalla de carga, así que el primer fotograma del mundo tardó 614 ms en lugar de 1048 ms (segundo par; primer par de 781 a 702 ms).
- **Menos fotogramas lentos** (`VICTORY_AMBIENT_ONCE=1`): el sustituto de la iluminación ambiental del juego cuenta un solo rebote. El tiempo de fotograma del percentil 99 bajó 11 ms en una comparación dentro de una misma prueba.

El escalado en 4K ya no cierra el juego: tres arranques con escalado funcionaron sin división por cero (0xc0000094). El escalado DLSS se ve nítido pero no aporta tasa de fotogramas, porque el fotograma espera a la CPU. La generación de fotogramas DLSS (`-FrameGen`, o Frame generation = dlss en el menú F9) presenta unos 44-47 fps a partir de 22-24 fotogramas reales; sigue desactivada por defecto hasta que se revisen a ojo los fotogramas generados. El lote 5 también descartó switches que rompían algo o restaban velocidad, entre ellos `PIPELINE_PIECES` (pantalla negra), `BAKE_SPREAD=12` (1,2-2,2 fps más lento) y `GUARD_PUBLISH_TRY` (un cierre). El constructor de ray tracing del juego funciona al activarlo, pero va a 0,6-1,1 fps, así que sigue desactivado.

### Equipo de prueba

| Componente | Modelo |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, controlador 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 núcleos / 32 hilos |
| Memoria | 64 GB DDR5-6000 |
| Archivo de paginación | 64 GB (límite de memoria confirmada: 142 GB) |
| Placa base | MSI PRO B650-P WIFI |
| Almacenamiento | SSD NVMe (Samsung 990 EVO Plus) |
| Sistema operativo | Windows 11 (build 26200) |
| Juego | GTA VI para Xbox Series, build 1.0.2.1 (tu propia copia) |

Todas las cifras actuales proceden de este equipo. Las cifras más antiguas de este README se midieron en una RTX 4070 SUPER (Windows) o en una RX 9070 XT (Linux), y en cada caso se indica.

## Cómo funciona

![Cómo funciona: el código x86-64 del juego se ejecuta de forma nativa en la CPU; las llamadas al sistema de la consola van al kernel HLE sobre Windows; los comandos D3D12.X se reconstruyen como Direct3D 12 del sistema anfitrión; los shaders RDNA 2 se traducen a DXIL y se guardan en caché](../images/how-it-works.svg)

- **El código del juego** se carga y se ejecuta tal cual. La Xbox Series y un PC con Windows comparten el conjunto de instrucciones x86-64, así que no se emula ninguna instrucción.
- **Las llamadas al sistema** dirigidas al kernel de la consola y a las bibliotecas GDK las atiende la emulación de alto nivel (HLE) de El1xr sobre Windows: memoria, hilos, archivos y servicios del sistema.
- **Lecturas de disco**: las peticiones DirectStorage del juego las atienden hilos de trabajo propios de El1xr, con hasta 256 MiB en curso. Las texturas que llegan por streaming se colocan en bloques de memoria compartidos: unas 115.000 texturas en 116 bloques en una prueba típica.
- **Gráficos**: las listas de comandos Direct3D 12.X de la consola, su memoria y sus recursos se reconstruyen como Direct3D 12 estándar en tu GPU. Las listas de comandos se envían desde un hilo propio, de modo que el hilo del fotograma del juego hace menos de ese trabajo.
- **Heaps de descriptores**: el juego direcciona hasta 1.245.184 ranuras de descriptores (su tabla de vistas de texturas y búferes). El1xr las refleja en un único heap del sistema anfitrión de aproximadamente 1.000.000 de ranuras, compartido con sus propias vistas, y crea las copias del sistema anfitrión en la pantalla de carga. Hacer caber el resto sigue pendiente (ver Problemas conocidos).
- **Los shaders** compilados para la GPU RDNA 2 de la consola se traducen a DXIL. De los 26.148 programas de shaders del juego se traducen todos salvo 6, y cada traducción se guarda en caché en disco. `dev.py prewarm` construye los 25.751 pipelines de GPU del juego antes de una prueba, para que no se compilen durante la partida.

## Ejecútalo

Necesitas tu propia copia del juego, Windows 10 u 11, una GPU con Shader Model 6.6 y mesh shaders, y Rust.

```powershell
tools\lancer_victory.ps1 -GameDir <carpeta del juego>
```

La guía completa (instalación, ajustes, solución de problemas) está en [docs/running-gta6.md](../running-gta6.md).

> [!IMPORTANT]
> Este repositorio no contiene archivos del juego, claves ni ejecutables, y no explica cómo conseguirlos.

<details>
<summary>Comandos de desarrollo</summary>

```powershell
python tools/dev.py build      # compilación release
python tools/dev.py prewarm    # traduce todos los shaders sin conexión (unos 45 s)
python tools/dev.py bench --quiet --seconds 200   # partida medida
python tools/dev.py gate       # todos los tests, falla ante cualquier aviso
```

</details>

## Problemas conocidos

- **El alféizar de la ventana de la prisión se oscurece después del prólogo.** En el plano de la celda que sigue al título PROLOGUE, el alféizar iluminado por el sol se vuelve oscuro y pierde detalle de un fotograma a otro, con la misma cámara. Se reprodujo dos veces en el lote 5; la causa se está investigando.
- **La piel puede volverse negra.** En algunas pruebas, la piel de un personaje, casi siempre en el diner, se vuelve negra por completo durante unos segundos y después vuelve a la normalidad ([#35](https://github.com/kylepelham/El1xr/issues/35)). La camisa de la camarera también cambia de color de una prueba a otra.
- **Cuadrados oscuros sobre una ventana.** En la cinemática inicial, hacia el segundo 44, unos cuadrados oscuros escalonados cubren la ventana detrás de Jason. El lote 5 descartó dos causas sospechosas; la búsqueda continúa.
- **Límite del heap de descriptores.** El juego direcciona hasta 1.245.184 ranuras de descriptores, pero el heap del sistema anfitrión en la RTX 5090 admite aproximadamente 1.000.000. En las pruebas del lote 5, entre 313.000 y 504.000 de las vistas activas del juego quedaban más allá de la parte que cabe en el heap del sistema anfitrión. Todavía no se ha conservado ninguna forma de hacerlas caber.
- **La pantalla del teléfono se ve negra** en el plano de la mesa del diner.
- **Uso de memoria elevado.** El proceso del juego llega a 33-35 GB. Con un archivo de paginación demasiado pequeño, se cierra cuando Windows se queda sin memoria confirmada. El PC de prueba funciona con un archivo de paginación de 64 GB.
- **Streaming lento.** Tras entrar en el mundo, todos los recursos tardan unos 72 s en llegar, con elementos que aparecen de golpe mientras tanto.
- **El fotograma espera a la CPU.** Preparar y enviar un fotograma cuesta 15,7-21,2 ms de tiempo de CPU, y por eso el escalado no aporta tasa de fotogramas.
- **La iluminación con ray tracing está desactivada.** El juego construye sus estructuras de ray tracing por software; ese constructor funciona, pero va a 0,6-1,1 fps, así que sigue desactivado.
- **Un único equipo de referencia.** Las cifras actuales proceden de la RTX 5090 indicada arriba. Las demás GPU no se han medido con la build actual.

## Próximos pasos

El objetivo son 30 fps estables en 4K en la RTX 5090, y después eliminar los tirones y los fallos visuales. Cada cambio nuevo permanece desactivado por defecto hasta que pruebas con el equipo en reposo lo validen.

- **Menos tiempo de CPU por fotograma**: el fotograma está limitado por la CPU, así que los próximos lotes seguirán sacando del hilo del fotograma el trabajo de envío y de comprobación de texturas.
- **El alféizar y los cuadrados de la ventana**, y **la piel que se vuelve negra** ([#35](https://github.com/kylepelham/El1xr/issues/35)).
- **El heap de descriptores**: hacer caber las 1.245.184 ranuras del juego en el heap del sistema anfitrión.
- **Ray tracing por hardware en los núcleos RT**, que permitiría recuperar la iluminación con ray tracing.
- **Generación de fotogramas DLSS por defecto**, cuando se hayan revisado a ojo sus fotogramas generados.

## El proyecto en cifras

| Cifra | Qué |
|---|---|
| 4000+ pruebas superadas | crates de Rust (`cargo test`) y herramientas de Python (`pytest`), ejecutadas por `dev.py gate` |
| 26.148 programas de shaders | traducidos de RDNA 2 a DXIL y en caché en disco; 6 todavía no se traducen |
| 25.751 pipelines de GPU | construidos antes de jugar por `dev.py prewarm` |
| 1 pipeline rechazado | en la RTX 5090, en las últimas pruebas |

## Contribuir

Las issues y las pull requests son bienvenidas. Ejecuta `python tools/dev.py gate` antes de abrir una pull request. Las normas están en [CONTRIBUTING.md](../../CONTRIBUTING.md). Las preguntas y los informes de pruebas van dirigidos a Bone/Nevaa en Discord: **bonemeld**.

## Créditos

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak y sus colaboradores ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 o posterior), y los autores de los forks a partir de los que se ensambló, entre ellos Neptune (VIctory Edit.) de Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty y sus colaboradores, el proyecto del que El1xr es un fork.
- **Los forks de VIctory de los que hemos aprendido y portado código**, entre ellos fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory, readmet3xt-lab/foxhound, RipcuBAN/victory-fork y Kemerd/VIctory-NVIDIA-RT.
- Cada componente y cada autor, cambio por cambio, se acredita en [THIRD_PARTY.md](../../THIRD_PARTY.md). La historia anterior del proyecto está en [docs/history.md](../history.md).

## Licencia

El1xr se distribuye bajo la [GNU GPL 3.0 o posterior](../../LICENSE). Las partes de terceros conservan su propia licencia: el módulo `Task` de libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) y las tablas RDNA 2 generadas a partir de las pruebas públicas de codificación de LLVM (Apache 2.0 con excepción LLVM, `PrototypeScarlett/emu/rdna2_ops.py` y `rdna2_shapes.py`).
