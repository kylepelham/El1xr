<div align="center">

# El1xr

**Grand Theft Auto VI (Xbox Series, build 1.0.2.1) tourne sur un PC Windows.**<br>
En 4K, environ 25 fps et en hausse sur une RTX 5090.

[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](../../LICENSE)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078D6?logo=windows&logoColor=white)](../fr/lancer-gta6.md)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-B7410E?logo=rust&logoColor=white)](../../third_party/victory)
[![Discord](https://img.shields.io/badge/Discord-AnyXboxX-5865F2?logo=discord&logoColor=white)](https://discord.gg/TcX42vRyPU)

[English](../../README.md) · **Français** · [Español](README.es.md) · [Deutsch](README.de.md) · [Italiano](README.it.md) · [Português](README.pt-BR.md) · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

---

El1xr est le projet d'émulation de GTA VI de Bone/Nevaa : un fork d'[AnyXboxX](https://github.com/LooperSalty/AnyXboxX), bâti sur [VIctory](https://github.com/Marvisak/VIctory) et sur le meilleur du travail des forks de VIctory (chaque source est créditée dans [THIRD_PARTY.md](../../THIRD_PARTY.md)). Le code x86-64 du jeu s'exécute nativement ; ses appels système de console, ses commandes Direct3D 12.X et ses shaders RDNA 2 sont traduits à la volée pour le PC.

> [!NOTE]
> **État au 27 novembre 2026**
>
> - 🎮 **Le monde est jouable en 4K, à 24-25 fps une fois stabilisé** sur une RTX 5090 (trois essais de benchmark sur une machine au repos : 24,9 ; 25,2 et 24,2 fps). L'objectif est un 30 fps stable.
> - ✅ **0 pipeline GPU refusé** et aucune perte du périphérique GPU pendant les essais de benchmark.
> - ✅ Les cinématiques se jouent en entier et les premières missions ont été terminées.
> - ⚡ **Lot 1 fusionné** : du lancement à la première image du monde, environ 31 à 33 s désormais (contre 41 à 54 s), et le pire gel à l'entrée dans le monde dure 1,5 à 1,9 s (contre 7,6 à 8,1 s).
> - 🚧 Environ **60 accélérations supplémentaires sont écrites et en cours de test** (voir [En cours de test](#en-cours-de-test)).

## En bref

| Mesure (RTX 5090, 4K) | Aujourd'hui | Objectif |
|---|---|---|
| Fréquence d'images stabilisée | **24-25 fps** | 30+ fps |
| Temps d'image médian | ~40 ms | ≤ 33,3 ms |
| Image au 99e centile | 68-73 ms | aucun à-coup |
| Temps CPU pour préparer une image | 25-26 ms | inférieur au temps GPU |
| Pipelines GPU refusés | **0** | 0 |
| Du lancement à la première image du monde | ~31-33 s | |
| Streaming après l'entrée dans le monde | 52-63 s | quelques secondes |

Mesuré avec `python tools/dev.py bench --quiet --seconds 200` : démarrage rapide depuis un point de sauvegarde fixe, rien d'autre ne tourne sur la machine, chiffres lus dans le journal d'images de l'émulateur lui-même. Le temps de lancement provient des essais du lot 1, faits pendant que d'autres travaux compilaient : une machine au repos devrait faire un peu mieux.

## Machine de test

| Composant | Modèle |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 Go, pilote 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 cœurs / 32 threads |
| Mémoire | 64 Go DDR5-6000 |
| Carte mère | MSI PRO B650-P WIFI |
| Stockage | SSD NVMe (Samsung 990 EVO Plus) |
| Système d'exploitation | Windows 11 (build 26200) |
| Jeu | GTA VI pour Xbox Series, build 1.0.2.1 (votre propre copie) |

## Où passe le temps d'une image

![Temps GPU par image : passes du jeu 22,8 ms, travail de l'émulateur 7,0 ms, non mesuré 3,8 ms, inactif 6,2 ms](../images/frame-budget.svg)

Une image stabilisée prend environ 40 ms. Les passes de rendu du jeu lui-même prennent 22,8 ms de temps GPU : les 30 fps sont donc à portée. Le reste part dans :

- **notre propre travail GPU** (7 ms) : contrôles de limites sur les draws indirects, copies de textures entre formats, vérifications de ce que le GPU a écrit ;
- **l'inactivité du GPU** (6 à 10 ms) : la carte attend pendant qu'un seul thread CPU prépare le lot de travail suivant.

Ce sont ces deux postes que vise le travail en cours de test.

## Ce qui a changé cette semaine

**Corrigé**
- Le givre de la fenêtre de la prison n'est plus dessiné par-dessus les cheveux de Lucia.
- Les « lignes sur tout » en plein soleil ont disparu : le traducteur de shaders considérait certaines valeurs comme égales dans les deux moitiés d'une wave de 64 lanes alors qu'elles ne l'étaient pas (31 shaders).
- Les taches sur la vitre du diner ont disparu : la passe de verre en ray tracing s'exécute désormais comme « aucune intersection » au lieu d'être sautée.
- Audio : décodage XMA plus sûr et chemin propre pour la coupure du son et l'arrêt.

**Plus rapide**
- Les envois de textures ont quitté les threads de dessin du jeu, les vérifications de textures s'exécutent en parallèle et les listes relâchées sont libérées sur leur propre thread : le pire gel à l'entrée dans le monde est passé de 9,5 s à 6,6 s.
- L'allocation d'une texture par le pilote, côté CPU, coûte 382 à 529 µs sur cette machine, contre 5 à 8 µs depuis un bloc de mémoire préalloué ; des pools de textures et de buffers sont en cours de test.

**Outillage**
- Démarrage rapide pour les essais : la vidéo d'introduction est sautée et le jeu part d'un point de sauvegarde figé.
- Chaque bench rapporte les fps, les centiles de temps d'image, les à-coups de plus de 100 et 250 ms, les temps de lancement et de streaming, les réinitialisations du pilote GPU, et enregistre des captures d'écran.
- Coût GPU par passe, phases d'image par thread et journal des à-coups avec échantillons de pile.
- Le traducteur de shaders et l'hôte GPU ont été découpés en modules plus petits ; la sortie du traducteur est restée identique à l'octet près sur les 26 148 programmes de shaders du jeu.

## Problèmes connus

- **Défauts visuels variables d'un essai à l'autre** : dans certains essais, le haut que porte Lucia au diner a la mauvaise texture, des visages sont noirs ou tachés, ou les cheveux deviennent orange à la fenêtre de la prison. Des correctifs portés d'autres forks sont en cours de test.
- **Streaming lent** : après l'entrée dans le monde, il faut environ une minute pour que toutes les ressources arrivent, avec des apparitions tardives d'objets entre-temps.
- **GPU qui attend le CPU** : 6 à 10 ms par image (voir plus haut).
- **L'éclairage en ray tracing est en grande partie coupé** : le jeu construit ses structures de ray tracing en logiciel, et ce constructeur est désactivé.

## En cours de test

Le travail est écrit en parallèle et testé par lots sur l'unique GPU : chaque changement se trouve derrière son propre interrupteur `VICTORY_*`, un lot compare tous les nouveaux interrupteurs activés à tous désactivés, et chaque interrupteur est gardé ou abandonné au vu des chiffres et des captures d'écran.

- **Lot 1 (terminé, 20 interrupteurs gardés)** : préparation des textures sans verrou, moins de reconstructions de descripteurs et d'attentes de verrou, précalcul des personnages en parallèle au chargement, shaders traduits dès que le jeu les crée, chargement plus rapide des bibliothèques de pipelines, mappage mémoire allégé, et les correctifs des tenues, des visages et des cheveux (le haut que porte Lucia au diner était correct dans 4 essais sur 5, contre 0 sur 2 avant). Abandonnés pour l'instant : les pools de mémoire pour les textures et les buffers (ils ralentissaient le démappage de la mémoire) et deux interrupteurs qui plantaient ; leurs auteurs les corrigent.
- **Lot 2 (en cours)** : mémoire du titre en sections de 2 Mio (temps de mappage 55,5 s → 7,3 s en amont), buffers du jeu gardés en mémoire GPU (−15 % de temps d'image en amont sur une RTX 5090), upscaling DLSS, priorité plus élevée pour les threads d'image de l'émulateur, préchauffage hors ligne des shaders et des pipelines, et ray tracing matériel sur les cœurs RT.

## Lancer le jeu

Il vous faut votre propre copie du jeu, Windows 10 ou 11, un GPU prenant en charge le Shader Model 6.6 et les mesh shaders, et Rust.

```powershell
tools\lancer_victory.ps1 -GameDir <dossier du jeu>
```

Le guide complet (installation, réglages, dépannage) se trouve dans [docs/running-gta6.md](../fr/lancer-gta6.md).

> [!IMPORTANT]
> Ce dépôt ne contient aucun fichier du jeu, aucune clé ni aucun exécutable, et n'explique pas comment les obtenir.

Pour le développement, `tools/dev.py` regroupe les étapes courantes :

```powershell
python tools/dev.py build      # build release
python tools/dev.py gate       # tous les tests, échoue au moindre avertissement
python tools/dev.py prewarm    # traduit tous les shaders hors ligne (environ 45 s)
python tools/dev.py bench --quiet --seconds 200   # essai de jeu mesuré
python tools/dev.py train --plan plan.toml        # un essai d'un plan de lot
```

## Le projet en chiffres

| Chiffre | Quoi |
|---|---|
| 1600+ tests automatisés | outils Python et crates Rust, lancés par `dev.py gate` |
| 26 148 programmes de shaders | traduits de RDNA 2 en DXIL, mis en cache sur disque |
| 0 pipeline refusé | sur la RTX 5090 |

## Historique

Comment le projet en est arrivé là, des premières DLL de console chargées dans un émulateur aux premières images du monde : [docs/fr/history.md](../fr/history.md).

## Contribuer

Les issues et les pull requests sont les bienvenues. Lancez `python tools/dev.py gate` avant d'ouvrir une pull request. Les règles sont dans [CONTRIBUTING.md](../../CONTRIBUTING.md). Discussions et rapports d'essai : [Discord](https://discord.gg/TcX42vRyPU).

## Crédits

- **El1xr** : Bone/Nevaa.
- **VIctory** : Marvisak et ses contributeurs ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 ou ultérieure), ainsi que les auteurs des forks à partir desquels il a été assemblé, dont Neptune (VIctory Edit.) de Traffic & zm0kie.
- **AnyXboxX** : Looper_Salty et ses contributeurs ([AnyXboxX](https://github.com/LooperSalty/AnyXboxX)), le projet dont El1xr est un fork.
- **Les forks de VIctory dont nous avons appris et dont nous avons porté du code**, parmi lesquels fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory et readmet3xt-lab/foxhound.
- Chaque composant et chaque auteur, changement par changement, est crédité dans [THIRD_PARTY.md](../../THIRD_PARTY.md).

## Licence

El1xr est distribué sous la [GNU GPL 3.0 ou ultérieure](../../LICENSE). Les parties tierces gardent leur propre licence : le module `Task` de libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) et les tables RDNA 2 générées à partir des tests d'encodage publics de LLVM (Apache 2.0 avec exception LLVM, `PrototypeScarlett/emu/rdna2_ops.py` et `rdna2_shapes.py`).
