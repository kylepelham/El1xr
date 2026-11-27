<div align="center">

# El1xr

**Grand Theft Auto VI (Xbox Series, build 1.0.2.1) funcionando en un PC con Windows.**<br>
En 4K, unos 25 fps y subiendo en una RTX 5090.

[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](../../LICENSE)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078D6?logo=windows&logoColor=white)](../running-gta6.md)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-B7410E?logo=rust&logoColor=white)](../../third_party/victory)
[![Discord](https://img.shields.io/badge/Discord-AnyXboxX-5865F2?logo=discord&logoColor=white)](https://discord.gg/TcX42vRyPU)

[English](../../README.md) · [Français](README.fr.md) · **Español** · [Deutsch](README.de.md) · [Italiano](README.it.md) · [Português](README.pt-BR.md) · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

---

El1xr es el proyecto de emulación de GTA VI de Bone/Nevaa: un fork de [AnyXboxX](https://github.com/LooperSalty/AnyXboxX), construido sobre [VIctory](https://github.com/Marvisak/VIctory) y sobre lo mejor del trabajo de los forks de VIctory (cada fuente se acredita en [THIRD_PARTY.md](../../THIRD_PARTY.md)). El propio código x86-64 del juego se ejecuta de forma nativa; sus llamadas al sistema de la consola, sus comandos Direct3D 12.X y sus shaders RDNA 2 se traducen al vuelo para PC.

> [!NOTE]
> **Estado a 27 de noviembre de 2026**
>
> - 🎮 **El mundo es jugable en 4K, a 24-25 fps una vez estabilizado** en una RTX 5090 (tres pruebas de rendimiento con el equipo en reposo: 24,9; 25,2 y 24,2 fps). El objetivo son 30 fps estables.
> - ✅ **0 pipelines de GPU rechazados** y ninguna pérdida del dispositivo GPU en las pruebas de rendimiento.
> - ✅ Las cinemáticas se reproducen completas y las primeras misiones se han jugado hasta el final.
> - ⚡ **Lote 1 integrado**: del arranque al primer fotograma del mundo ahora unos 31-33 s (antes 41-54 s), y el peor congelamiento al entrar en el mundo, 1,5-1,9 s (antes 7,6-8,1 s).
> - 🚧 Unas **60 aceleraciones más están escritas y en pruebas** (ver [En pruebas](#en-pruebas)).

## De un vistazo

| Medida (RTX 5090, 4K) | Ahora | Objetivo |
|---|---|---|
| Tasa de fotogramas estabilizada | **24-25 fps** | 30+ fps |
| Tiempo de fotograma mediano | ~40 ms | ≤ 33,3 ms |
| Fotograma del percentil 99 | 68-73 ms | sin tirones |
| Tiempo de CPU para preparar un fotograma | 25-26 ms | por debajo del tiempo de la GPU |
| Pipelines de GPU rechazados | **0** | 0 |
| Del arranque al primer fotograma del mundo | ~31-33 s | |
| Streaming tras entrar en el mundo | 52-63 s | unos pocos segundos |

Medido con `python tools/dev.py bench --quiet --seconds 200`: arranque rápido desde un punto de control fijo, nada más ejecutándose en la máquina, cifras leídas del propio registro de fotogramas del emulador. El tiempo de arranque sale de las pruebas del lote 1, hechas mientras se compilaba otro trabajo, así que una máquina en reposo debería lograr algo mejor.

## Equipo de prueba

| Componente | Modelo |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, controlador 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 núcleos / 32 hilos |
| Memoria | 64 GB DDR5-6000 |
| Placa base | MSI PRO B650-P WIFI |
| Almacenamiento | SSD NVMe (Samsung 990 EVO Plus) |
| Sistema operativo | Windows 11 (build 26200) |
| Juego | GTA VI para Xbox Series, build 1.0.2.1 (tu propia copia) |

## En qué se va un fotograma

![Tiempo de GPU por fotograma: pasadas del juego 22,8 ms, trabajo del emulador 7,0 ms, sin medir 3,8 ms, inactiva 6,2 ms](../images/frame-budget.svg)

Un fotograma estabilizado tarda unos 40 ms. Las pasadas de renderizado del propio juego ocupan 22,8 ms de tiempo de GPU, así que los 30 fps están al alcance. El resto se va en:

- **nuestro propio trabajo de GPU** (7 ms): comprobaciones de límites en los draws indirectos, copias de texturas entre formatos, verificaciones de lo que ha escrito la GPU;
- **GPU inactiva** (6-10 ms): la tarjeta espera mientras un solo hilo de CPU prepara el siguiente lote de trabajo.

Ambos son el objetivo del trabajo en pruebas.

## Qué ha cambiado esta semana

**Corregido**
- La escarcha de la ventana de la prisión ya no se dibuja sobre el pelo de Lucia.
- Han desaparecido las «líneas sobre todo» a la luz del sol: el traductor de shaders trataba algunos valores como iguales en las dos mitades de una wave de 64 lanes cuando no lo eran (31 shaders).
- Han desaparecido las manchas en el cristal del diner: la pasada de cristal con ray tracing ahora se ejecuta como «sin impacto» en lugar de saltarse.
- Audio: decodificación XMA más segura y una ruta limpia de silenciado y apagado.

**Más rápido**
- Las subidas de texturas salieron de los hilos de dibujo del juego, las comprobaciones de texturas se ejecutan en paralelo y las listas liberadas se liberan en su propio hilo: el peor congelamiento al entrar en el mundo bajó de 9,5 s a 6,6 s.
- La asignación de una textura por el controlador en la CPU cuesta 382-529 µs en esta máquina y 5-8 µs desde un bloque de memoria preasignado; los pools de texturas y búferes están en pruebas.

**Herramientas**
- Arranque rápido para las pruebas: se salta el vídeo de introducción y empieza desde un punto de control congelado.
- Cada bench informa de los fps, los percentiles de tiempo de fotograma, los tirones de más de 100 y 250 ms, los tiempos de arranque y de streaming y los reinicios del controlador de la GPU, y guarda capturas de pantalla.
- Coste de GPU por pasada, fases del fotograma por hilo y un registro de tirones con muestras de pila.
- El traductor de shaders y el host de GPU se dividieron en módulos más pequeños; la salida del traductor se mantuvo idéntica byte a byte en los 26.148 programas de shaders del juego.

## Problemas conocidos

- **Fallos visuales que varían entre pruebas**: en algunas pruebas, la camiseta de Lucia en el diner tiene la textura equivocada, las caras salen negras o con manchas, o el pelo se vuelve naranja en la ventana de la prisión. Hay correcciones portadas de otros forks en pruebas.
- **Streaming lento**: tras entrar en el mundo, todos los recursos tardan aproximadamente un minuto en llegar, con elementos que aparecen de golpe mientras tanto.
- **GPU esperando a la CPU**: 6-10 ms por fotograma (ver arriba).
- **La iluminación con ray tracing está casi toda desactivada**: el juego construye sus estructuras de ray tracing por software, y ese constructor está desactivado.

## En pruebas

El trabajo se escribe en paralelo y se prueba por lotes en la única GPU: cada cambio va detrás de su propio interruptor `VICTORY_*`, un lote ejecuta todos los interruptores nuevos activados frente a todos desactivados, y cada interruptor se conserva o se descarta según las cifras y las capturas de pantalla.

- **Lote 1 (terminado, 20 interruptores conservados)**: preparación de texturas sin bloqueos, menos reconstrucciones de descriptores y esperas de bloqueos, precálculo de personajes en paralelo durante la carga, shaders traducidos en cuanto el juego los crea, cargas más rápidas de bibliotecas de pipelines, mapeo de memoria más ligero, y las correcciones de ropa, caras y pelo (la camiseta de Lucia en el diner salió bien en 4 de 5 pruebas, frente a 0 de 2 antes). Descartados por ahora: los pools de memoria para texturas y búferes (ralentizaban la liberación de los mapeos de memoria) y dos interruptores que provocaban cierres; sus autores los están corrigiendo.
- **Lote 2 (en marcha)**: memoria del título en secciones de 2 MiB (tiempo de mapeo 55,5 s → 7,3 s en el proyecto de origen), búferes del juego mantenidos en la memoria de la GPU (−15 % de tiempo de fotograma en el proyecto de origen en una RTX 5090), escalado DLSS, mayor prioridad para los hilos de fotograma del emulador, precalentamiento sin conexión de shaders y pipelines, y ray tracing por hardware en los núcleos RT.

## Ejecútalo

Necesitas tu propia copia del juego, Windows 10 u 11, una GPU con Shader Model 6.6 y mesh shaders, y Rust.

```powershell
tools\lancer_victory.ps1 -GameDir <carpeta del juego>
```

La guía completa (instalación, ajustes, solución de problemas) está en [docs/running-gta6.md](../running-gta6.md).

> [!IMPORTANT]
> Este repositorio no contiene archivos del juego, claves ni ejecutables, y no explica cómo conseguirlos.

Para el desarrollo, `tools/dev.py` agrupa los pasos habituales:

```powershell
python tools/dev.py build      # compilación release
python tools/dev.py gate       # todos los tests, falla ante cualquier aviso
python tools/dev.py prewarm    # traduce todos los shaders sin conexión (unos 45 s)
python tools/dev.py bench --quiet --seconds 200   # partida medida
python tools/dev.py train --plan plan.toml        # una ejecución de un plan de lote
```

## El proyecto en cifras

| Cifra | Qué |
|---|---|
| 1600+ pruebas automatizadas | herramientas de Python y crates de Rust, ejecutadas por `dev.py gate` |
| 26.148 programas de shaders | traducidos de RDNA 2 a DXIL, en caché en disco |
| 0 pipelines rechazados | en la RTX 5090 |

## Historia

Cómo ha llegado el proyecto hasta aquí, desde las primeras DLL de consola cargadas en un emulador hasta los primeros fotogramas del mundo: [docs/history.md](../history.md).

## Contribuir

Las issues y las pull requests son bienvenidas. Ejecuta `python tools/dev.py gate` antes de abrir una pull request. Las normas están en [CONTRIBUTING.md](../../CONTRIBUTING.md). Debate e informes de pruebas: [Discord](https://discord.gg/TcX42vRyPU).

## Créditos

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak y sus colaboradores ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 o posterior), y los autores de los forks a partir de los que se ensambló, entre ellos Neptune (VIctory Edit.) de Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty y sus colaboradores ([AnyXboxX](https://github.com/LooperSalty/AnyXboxX)), el proyecto del que El1xr es un fork.
- **Los forks de VIctory de los que hemos aprendido y portado código**, entre ellos fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory y readmet3xt-lab/foxhound.
- Cada componente y cada autor, cambio por cambio, se acredita en [THIRD_PARTY.md](../../THIRD_PARTY.md).

## Licencia

El1xr se distribuye bajo la [GNU GPL 3.0 o posterior](../../LICENSE). Las partes de terceros conservan su propia licencia: el módulo `Task` de libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) y las tablas RDNA 2 generadas a partir de las pruebas públicas de codificación de LLVM (Apache 2.0 con excepción LLVM, `PrototypeScarlett/emu/rdna2_ops.py` y `rdna2_shapes.py`).
