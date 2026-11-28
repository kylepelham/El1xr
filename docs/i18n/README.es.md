<div align="center">

![El1xr: Grand Theft Auto VI para Xbox Series, funcionando en un PC con Windows. 23,1-25,7 fps en 4K en una RTX 5090, 0 pipelines rechazados, 27-28 s desde el arranque hasta el mundo, jugable desde el 27 de noviembre de 2026](../images/hero.svg)

![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-9b5cff?style=flat-square&labelColor=170f2b)
![Platform: Windows 10 and 11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2de2e6?style=flat-square&labelColor=170f2b)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-ff8a3d?style=flat-square&labelColor=170f2b&logo=rust&logoColor=white)
![Status: playable](https://img.shields.io/badge/status-playable-ff3d8b?style=flat-square&labelColor=170f2b)
![23.1-25.7 fps at 4K on RTX 5090](https://img.shields.io/badge/4K%20on%20RTX%205090-23.1--25.7%20fps-ffd36e?style=flat-square&labelColor=170f2b)
![Discord: bonemeld](https://img.shields.io/badge/Discord-bonemeld-5865F2?style=flat-square&labelColor=170f2b&logo=discord&logoColor=white)

[English](../../README.md) · [Français](README.fr.md) · **Español** · [Deutsch](README.de.md) · [Italiano](README.it.md) · [Português](README.pt-BR.md) · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

El1xr ejecuta la versión de Xbox Series de Grand Theft Auto VI (build 1.0.2.1) en un PC con Windows. Es el proyecto de Bone/Nevaa. El propio código x86-64 del juego se ejecuta de forma nativa en tu procesador. Sus llamadas al sistema de la consola, sus comandos Direct3D 12.X y sus shaders RDNA 2 se traducen para PC mientras el juego se ejecuta. No hay emulación de CPU ni contenido del juego en este repositorio: tienes que aportar tu propia copia.

## Estado

> [!NOTE]
> **A 27 de noviembre de 2026, GTA VI (Xbox Series, build 1.0.2.1) arranca, llega a los menús y es jugable en el mundo abierto en Windows.** En las últimas pruebas con el equipo en reposo (lote 4, 4K, RTX 5090), el juego funciona a 23,1-25,7 fps a lo largo de una prueba de 200 segundos, con 0 pipelines de GPU rechazados. Aún queda por debajo del siguiente objetivo, 30 fps estables. Después vendrá eliminar los tirones y los fallos visuales.

![Hitos: arranque, menú principal y partida a más tardar el 24 de noviembre de 2026; jugable a 23,1-25,7 fps el 27 de noviembre de 2026; 30 fps a continuación; estabilidad en curso](../images/milestones.svg)

| Hito | Estado | Pruebas | Fecha |
|---|---|---|---|
| El ejecutable funciona | Hecho | El ejecutable de Xbox Series 1.0.2.1 arranca en Windows. Su código x86-64 se ejecuta de forma nativa. | a más tardar el 24 nov. 2026 |
| Primer fotograma | Hecho | La animación de inicio y las pantallas legales se muestran sin parpadeos. | a más tardar el 24 nov. 2026 |
| Menú principal | Hecho | La pantalla de perfil, los ajustes del primer arranque, las pantallas legales, la pantalla de carga y el menú de pausa funcionan. Los menús van a 30 fps (RTX 4070 SUPER). | a más tardar el 24 nov. 2026 |
| En partida | Hecho | El juego llega al mundo abierto y el personaje se mueve bajo el control del jugador. Una sesión de 13 minutos en la prisión funcionó a unos 4 fps (RTX 4070 SUPER). | a más tardar el 24 nov. 2026 |
| Jugable | Hecho, por debajo de la velocidad objetivo | El juego funciona a 23,1-25,7 fps en 4K (RTX 5090; dos pruebas de 200 segundos del lote 4 con el equipo en reposo). | 27 nov. 2026 |
| Avance en la historia | Dos primeras misiones completadas | Un colaborador completó las dos primeras misiones en una sesión de 45 minutos a unos 2 fps (RTX 4070 SUPER; según su informe, no medido a partir de los registros). Todas las cinemáticas se reproducen completas. En Linux (Proton, RX 9070 XT), una prueba de 23 minutos superó el prólogo y llegó al capítulo 1. | 25 nov. 2026 |
| 30 fps | Todavía no | 23,1-25,7 fps, con un tiempo de fotograma mediano de 40-43 ms frente al objetivo de 33,3 ms (RTX 5090, 4K). | 27 nov. 2026 |
| Estable | En curso | Con los ajustes por defecto, las pruebas de 200 segundos terminan sin cierres, sin reinicios del controlador de la GPU y sin pérdida del dispositivo. El parón al entrar en el mundo es menos frecuente (1 de 8 pruebas en reposo del lote 4), pero no ha desaparecido ([#78](https://github.com/kylepelham/El1xr/issues/78)), y los fallos visuales todavía varían de una prueba a otra (ver Problemas conocidos). | 27 nov. 2026 |

## Rendimiento

![De un vistazo: 23,1-25,7 fps, 27-28 s desde el arranque hasta el mundo, 0 caras negras en 24 capturas del diner con las entradas de bake primero, 31 fotogramas negros mostrados durante la carga en lugar de 152](../images/stats.svg)

| Medida (RTX 5090, 4K) | Ahora | Objetivo |
|---|---|---|
| Tasa de fotogramas en una prueba de 200 s | **23,1-25,7 fps** | 30+ fps |
| Tiempo de fotograma mediano | 40-43 ms | 33,3 ms o menos |
| Fotograma del percentil 99 en juego estabilizado | 64-69 ms | sin tirones |
| Tiempo de CPU para preparar un fotograma | 26-28 ms | por debajo del tiempo de la GPU |
| Pipelines de GPU rechazados | **0** | 0 |
| Del arranque al primer fotograma del mundo | 27-28 s | |
| Parón al entrar en el mundo | 6,6 s en 1 de 8 pruebas en reposo; peor fotograma de 1,2-3,0 s en las demás ([#78](https://github.com/kylepelham/El1xr/issues/78)) | ninguno |
| Fotogramas de más de 250 ms en el mundo | 10-12 por prueba | ninguno |
| Fotogramas negros mostrados en los primeros 90 s | 31 | ninguno |
| Streaming tras entrar en el mundo | 71-72 s | unos pocos segundos |

Estas cifras proceden del lote 4 (27 de noviembre de 2026), con los switches que el lanzador activa ahora. La tasa de fotogramas, los tiempos de fotograma, el tiempo de CPU, los fotogramas de más de 250 ms y el streaming proceden de dos pruebas de 200 segundos con el equipo en reposo: cada una hace un arranque rápido desde un punto de control fijo mientras el resto del trabajo de la máquina queda en pausa, y las cifras se leen del propio registro de fotogramas del emulador. El tiempo de arranque procede de dos pruebas con los nuevos switches de carga, alternadas con dos pruebas sin ellos (32-33 s), mientras se compilaba en otros núcleos de la CPU. Los fotogramas negros se contaron en una prueba de 90 segundos con el equipo en reposo. En todas las pruebas había parte del registro de diagnóstico activado.

![En qué se va un fotograma: pasadas del juego 22,8 ms, trabajo de GPU de El1xr 7,0 ms, sin medir 3,8 ms, GPU inactiva 6,2 ms, frente a la línea de 33,3 ms de los 30 fps](../images/frame-budget.svg)

El último perfil de tiempos de GPU, tomado antes del lote 3, situaba un fotograma estabilizado en unos 40 ms. Las pasadas de renderizado del propio juego necesitan 22,8 ms de tiempo de GPU, lo que deja margen para los 30 fps. El resto se reparte entre el trabajo de GPU propio de El1xr (7 ms: comprobaciones de límites en los draws indirectos, copias de texturas entre formatos, subidas de datos) y el tiempo que la GPU pasa inactiva (6-10 ms) mientras un solo hilo de CPU prepara el siguiente lote de trabajo. Esas dos partidas son el objetivo de las próximas actualizaciones.

![Lote 4: con los switches de carga, del arranque al primer fotograma del mundo pasó de 32-33 s a 27-28 s; reteniendo el último fotograma, los fotogramas negros mostrados en los primeros 90 s bajaron de 152 a 31; con las entradas de bake primero, las capturas del diner con una cara negra bajaron de 0-3 por prueba a 0](../images/load-times.svg)

El lote 4 (27 de noviembre de 2026) conservó cuatro cambios, ahora activados por defecto en el lanzador. Los switches del lote 3 siguen activados.

- **Primero las entradas de los bakes** (`VICTORY_BAKE_INPUTS=1`): los bakes de cara y piel reciben sus texturas de entrada antes que nada. En el diner, ninguna de las 24 capturas de dos pruebas mostró una cara negra, frente a 0-3 por prueba sin el switch, sin coste medible en la tasa de fotogramas.
- **Carga más rápida** (`VICTORY_LOAD_FPS=60`, `VICTORY_LOAD_END=world`, `VICTORY_LOAD_UNPACED=1`): la pantalla de carga va a hasta 60 fps hasta que aparece el mundo. El tiempo desde el arranque hasta el primer fotograma del mundo bajó de 32-33 s a 27-28 s en pruebas consecutivas, sin nuevos parones al entrar en el mundo.
- **Retener el último fotograma** (`VICTORY_HOLD_LAST_FRAME=1`): cuando el juego presenta un fotograma completamente negro, en pantalla se queda el último fotograma bueno. En una prueba de 90 segundos con el equipo en reposo tapó 40 de los 71 fotogramas negros que presentó el juego; los 31 restantes llegan al principio del arranque, antes de que exista un fotograma que retener. Una prueba equivalente sin el switch mostró 152 fotogramas negros.
- **Menú de ajustes dentro del juego** (`VICTORY_MENU=1`): pulsa F9, o mantén View y Menu durante un segundo, con la ventana del juego en primer plano. Una autoprueba del lote siguiente lo abrió dos veces sobre el juego en marcha, dibujó sus 10 pestañas y devolvió el control al juego al cerrarse.

Frente a una prueba con todos los switches desactivados, los del lote 3 también redujeron los fotogramas negros de los primeros 90 s de 116 a 77, y el peor fotograma al entrar en el mundo de 5,8 s a 1,7-2,7 s. Se descartaron otros ocho switches: uno rompía la pared y la litera de la celda de la prisión (`SPLIT_AHEAD`), dos dejaban caras o la celda negras o con ruido (`EXCHANGE_PAIRS`, `GPU_DETILE`), uno devolvía las caras negras al entrar en el mundo aunque ahorraba 5 s de carga (`BAKE_ONE_BUDGET`), uno añadía 13 ms de tiempo de CPU por fotograma (`FLUSH_ONE_PASS`), uno pasaba por alto cambios en las texturas (`TEXTURE_HASH_SAMPLED`), y dos no cambiaban nada medible (`CARD_DECL_JOIN`, `STALE_BATCH`). Las pruebas también mostraron que el escalado en 4K cerraba el juego (una división por cero, 0xc0000094); hay una corrección escrita a la espera de su prueba. Tras jugar con esta build, el responsable del proyecto la describió como «mucho más estable, menos tirones, una tasa de fotogramas más estable y menos parpadeo»; es una impresión de juego, no una medición.

### Equipo de prueba

| Componente | Modelo |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, controlador 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 núcleos / 32 hilos |
| Memoria | 64 GB DDR5-6000 |
| Placa base | MSI PRO B650-P WIFI |
| Almacenamiento | SSD NVMe (Samsung 990 EVO Plus) |
| Sistema operativo | Windows 11 (build 26200) |
| Juego | GTA VI para Xbox Series, build 1.0.2.1 (tu propia copia) |

Todas las cifras actuales proceden de este equipo. Las cifras más antiguas de este README se midieron en una RTX 4070 SUPER (Windows) o en una RX 9070 XT (Linux), y en cada caso se indica.

## Cómo funciona

![Cómo funciona: el código x86-64 del juego se ejecuta de forma nativa en la CPU; las llamadas al sistema de la consola van al kernel HLE sobre Windows; los comandos D3D12.X se reconstruyen como Direct3D 12 del sistema anfitrión; los shaders RDNA 2 se traducen a DXIL y se guardan en caché](../images/how-it-works.svg)

- **El código del juego** se carga y se ejecuta tal cual. La Xbox Series y un PC con Windows comparten el conjunto de instrucciones x86-64, así que no se emula ninguna instrucción.
- **Las llamadas al sistema** dirigidas al kernel de la consola y a las bibliotecas GDK las atiende la emulación de alto nivel (HLE) de El1xr sobre Windows: memoria, hilos, archivos y servicios del sistema.
- **Gráficos**: las listas de comandos Direct3D 12.X de la consola, su memoria y sus recursos se reconstruyen como Direct3D 12 estándar en tu GPU.
- **Los shaders** compilados para la GPU RDNA 2 de la consola se traducen a DXIL. Los 26.148 programas de shaders del juego se traducen todos, y cada uno se guarda en caché en disco tras su primer uso.

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

- **Fallos visuales que varían de una prueba a otra.** Las caras negras o con manchas son menos frecuentes con las entradas de bake primero, pero aún aparecen en algunas pruebas ([#35](https://github.com/kylepelham/El1xr/issues/35)). Lucia todavía puede llevar la ropa equivocada en el diner, y su pelo puede volverse naranja en la ventana de la prisión.
- **Un parón al entrar en el mundo.** Es menos frecuente que en el lote 3, donde casi todas las pruebas se congelaban 3-9 s, pero una de las ocho pruebas en reposo del lote 4 todavía se congeló 6,6 s mientras la GPU procesaba un lote de comandos. La causa se está investigando en [#78](https://github.com/kylepelham/El1xr/issues/78).
- **Streaming lento.** Tras entrar en el mundo, todos los recursos tardan unos 71 s en llegar, con elementos que aparecen de golpe mientras tanto.
- **El escalado cierra el juego en 4K.** Con el escalado DLSS o FSR activado, el juego se cierra en cuanto aparece el mundo (una división por cero en el código FSR 4 del juego). Una corrección espera su prueba; el escalado está desactivado por defecto.
- **La GPU espera a la CPU** 6-10 ms por fotograma (ver el reparto del fotograma más arriba).
- **La iluminación con ray tracing está casi toda desactivada.** El juego construye sus estructuras de ray tracing por software, y ese constructor está desactivado por ahora.
- **Un único equipo de referencia.** Las cifras actuales proceden de la RTX 5090 indicada arriba. Las demás GPU no se han medido con la build actual.

## Próximos pasos

El objetivo son 30 fps estables en 4K en la RTX 5090, y después eliminar los tirones y los fallos visuales. Hay unas 60 aceleraciones más escritas y pendientes de medir. Cada una permanece desactivada por defecto hasta que demuestre su eficacia.

- **El parón al entrar en el mundo** ([#78](https://github.com/kylepelham/El1xr/issues/78)): averiguar qué pasada de ese lote de comandos de GPU tarda segundos.
- **Menos esperas y menos sobrecarga**: recortar los 6-10 ms que la GPU espera a la CPU en cada fotograma y los 7 ms de trabajo de GPU propio de El1xr.
- **Escalado DLSS** (cuando se haya probado la corrección del cierre) y **ray tracing por hardware en los núcleos RT**, que permitiría recuperar la iluminación con ray tracing.
- **Precalentamiento sin conexión de shaders y pipelines**, para que no se compile ningún shader durante la partida.
- **Correcciones para las caras negras ([#35](https://github.com/kylepelham/El1xr/issues/35)) y la ropa que varía de una prueba a otra**.

## El proyecto en cifras

| Cifra | Qué |
|---|---|
| 1800+ pruebas automatizadas | herramientas de Python y crates de Rust, ejecutadas por `dev.py gate` |
| 26.148 programas de shaders | traducidos de RDNA 2 a DXIL, en caché en disco |
| 0 pipelines rechazados | en la RTX 5090 |

## Contribuir

Las issues y las pull requests son bienvenidas. Ejecuta `python tools/dev.py gate` antes de abrir una pull request. Las normas están en [CONTRIBUTING.md](../../CONTRIBUTING.md). Las preguntas y los informes de pruebas van dirigidos a Bone/Nevaa en Discord: **bonemeld**.

## Créditos

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak y sus colaboradores ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 o posterior), y los autores de los forks a partir de los que se ensambló, entre ellos Neptune (VIctory Edit.) de Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty y sus colaboradores, el proyecto del que El1xr es un fork.
- **Los forks de VIctory de los que hemos aprendido y portado código**, entre ellos fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory y readmet3xt-lab/foxhound.
- Cada componente y cada autor, cambio por cambio, se acredita en [THIRD_PARTY.md](../../THIRD_PARTY.md). La historia anterior del proyecto está en [docs/history.md](../history.md).

## Licencia

El1xr se distribuye bajo la [GNU GPL 3.0 o posterior](../../LICENSE). Las partes de terceros conservan su propia licencia: el módulo `Task` de libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) y las tablas RDNA 2 generadas a partir de las pruebas públicas de codificación de LLVM (Apache 2.0 con excepción LLVM, `PrototypeScarlett/emu/rdna2_ops.py` y `rdna2_shapes.py`).
