<div align="center">

![El1xr : Grand Theft Auto VI pour Xbox Series, sur un PC Windows. 23,5 fps dans le monde en 4K sur une RTX 5090, 0 pipeline refusé, 32 s du lancement au monde, jouable depuis le 27 novembre 2026](../images/hero.svg)

![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-9b5cff?style=flat-square&labelColor=170f2b)
![Platform: Windows 10 and 11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2de2e6?style=flat-square&labelColor=170f2b)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-ff8a3d?style=flat-square&labelColor=170f2b&logo=rust&logoColor=white)
![Status: playable](https://img.shields.io/badge/status-playable-ff3d8b?style=flat-square&labelColor=170f2b)
![23.5 fps at 4K on RTX 5090](https://img.shields.io/badge/4K%20on%20RTX%205090-23.5%20fps-ffd36e?style=flat-square&labelColor=170f2b)
![Discord: bonemeld](https://img.shields.io/badge/Discord-bonemeld-5865F2?style=flat-square&labelColor=170f2b&logo=discord&logoColor=white)

[English](../../README.md) · **Français** · [Español](README.es.md) · [Deutsch](README.de.md) · [Italiano](README.it.md) · [Português](README.pt-BR.md) · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

El1xr fait tourner la version Xbox Series de Grand Theft Auto VI (build 1.0.2.1) sur un PC Windows. C'est le projet de Bone/Nevaa. Le code x86-64 du jeu lui-même s'exécute nativement sur votre processeur. Ses appels système de console, ses commandes Direct3D 12.X et ses shaders RDNA 2 sont traduits pour le PC pendant que le jeu tourne. Le projet n'émule pas le CPU et ce dépôt ne contient aucun contenu du jeu : vous fournissez votre propre copie.

## État

> [!NOTE]
> **Au 27 novembre 2026, GTA VI (Xbox Series, build 1.0.2.1) démarre, atteint les menus et se joue en monde ouvert sous Windows.** Lors des derniers essais sur machine au repos (lot 3, 4K, RTX 5090), le monde tourne à 23,5 fps en médiane, avec 0 pipeline GPU refusé. C'est encore en dessous du prochain objectif, un 30 fps stable. Viendront ensuite l'absence d'à-coups et l'absence de défauts visuels.

![Étapes : démarrage, menu principal et en jeu au plus tard le 24 novembre 2026 ; jouable à 23,5 fps le 27 novembre 2026 ; 30 fps ensuite ; stabilité en cours](../images/milestones.svg)

| Étape | État | Éléments de preuve | Date |
|---|---|---|---|
| L'exécutable tourne | Fait | L'exécutable Xbox Series 1.0.2.1 démarre sous Windows. Son code x86-64 s'exécute nativement. | au plus tard le 24 nov. 2026 |
| Première image | Fait | L'animation de démarrage et les écrans légaux s'affichent sans scintillement. | au plus tard le 24 nov. 2026 |
| Menu principal | Fait | L'écran de profil, les réglages du premier lancement, les écrans légaux, l'écran de chargement et le menu pause fonctionnent tous. Les menus tournent à 30 fps (RTX 4070 SUPER). | au plus tard le 24 nov. 2026 |
| En jeu | Fait | Le jeu atteint le monde ouvert et le personnage se déplace sous le contrôle du joueur. Une session de 13 minutes dans la prison a tourné à environ 4 fps (RTX 4070 SUPER). | au plus tard le 24 nov. 2026 |
| Jouable | Fait, sous la vitesse visée | Le monde se joue à 23,5 fps en 4K (RTX 5090 ; médiane de la partie dans le monde d'un essai de 200 secondes du lot 3, sur machine au repos). | 27 nov. 2026 |
| Progression de l'histoire | Deux premières missions terminées | Un contributeur a terminé les deux premières missions au cours d'une session de 45 minutes, à environ 2 fps (RTX 4070 SUPER ; rapporté, non mesuré dans les journaux). Toutes les cinématiques se jouent en entier. Sous Linux (Proton, RX 9070 XT), un essai de 23 minutes a dépassé le prologue et atteint le chapitre 1. | 25 nov. 2026 |
| 30 fps | Pas encore | 23,5 fps dans le monde, soit un temps d'image médian de 42 ms pour un objectif de 33,3 ms (RTX 5090, 4K). | 27 nov. 2026 |
| Stabilité | En cours | Avec les réglages par défaut, les essais de 200 secondes se terminent sans plantage, sans réinitialisation du pilote GPU et sans perte du périphérique. L'entrée dans le monde bloque encore 3 à 9 s ([#78](https://github.com/kylepelham/El1xr/issues/78)), et les défauts visuels varient encore d'un essai à l'autre (voir Problèmes connus). | 27 nov. 2026 |

## Performances

![En bref : 23,5 fps dans le monde, 0 pipeline GPU refusé, 32 s du lancement au monde, 7 à-coups de plus de 1 s par essai](../images/stats.svg)

| Mesure (RTX 5090, 4K) | Aujourd'hui | Objectif |
|---|---|---|
| Fréquence d'images dans le monde (médiane) | **23,5 fps** | 30+ fps |
| Temps d'image médian | 42 ms | 33,3 ms ou moins |
| Image au 99e centile en jeu stabilisé | ~70 ms | aucun à-coup |
| Temps CPU pour préparer une image | 28 ms | inférieur au temps GPU |
| Pipelines GPU refusés | **0** | 0 |
| Du lancement à la première image du monde | 32 s | |
| Du lancement au jeu stabilisé | 49 s | |
| Blocage à l'entrée dans le monde | 3-9 s ([#78](https://github.com/kylepelham/El1xr/issues/78)) | aucun |
| Images de plus de 250 ms dans le monde | 20 par essai | aucune |
| À-coups de plus de 1 s | 7 par essai | aucun |
| Streaming après l'entrée dans le monde | 74 s | quelques secondes |

Ces chiffres proviennent des essais sur machine au repos du lot 3 (`python tools/dev.py bench --quiet --seconds 200`, 27 novembre 2026). Chaque essai démarre rapidement depuis un point de sauvegarde fixe pendant que le reste du travail de la machine est suspendu, et les chiffres sont lus dans le journal d'images de l'émulateur lui-même. Une partie de la journalisation de diagnostic était active dans chaque essai. Le blocage à l'entrée dans le monde est la fourchette observée sur les essais du lot 3.

![Où passe une image : passes du jeu 22,8 ms, travail GPU d'El1xr 7,0 ms, non mesuré 3,8 ms, GPU inactif 6,2 ms, face à la limite de 33,3 ms pour 30 fps](../images/frame-budget.svg)

Le dernier profil de temps GPU, mesuré avant le lot 3, donnait environ 40 ms pour une image stabilisée. Les passes de rendu du jeu lui-même demandent 22,8 ms de temps GPU, ce qui laisse de la marge pour les 30 fps. Le reste se partage entre le travail GPU propre à El1xr (7 ms : contrôles de limites sur les draws indirects, copies de textures entre formats, envois de données) et le temps pendant lequel le GPU reste inactif (6 à 10 ms), tandis qu'un seul thread CPU prépare le lot de travail suivant. Ce sont ces deux postes que visent les prochaines mises à jour.

![Lot 3 : avec les blocs mutualisés, du lancement à la première image du monde de 58-67 s à 32 s, du lancement au jeu stabilisé de 117-120 s à 49 s, et images de plus de 250 ms dans le monde de 51-74 à 20](../images/load-times.svg)

Le lot 3 (27 novembre 2026) a conservé deux groupes de switches, désormais activés par défaut dans le lanceur : la mémoire en sections de 2 Mio, les buffers du jeu gardés en mémoire GPU et une priorité plus élevée pour les threads d'image (le groupe fg_rt de sept switches), ainsi que les buffers de staging, GPU, partagés et d'envoi pris dans des blocs mutualisés (quatre switches). Les gardes d'écriture de 4 Kio sont aussi activées ; elles se sont révélées neutres. Face au même build sans blocs mutualisés, en essais consécutifs, le temps entre le lancement et la première image du monde est passé de 58-67 s à 32 s, le jeu stabilisé de 117-120 s à 49 s, les images de plus de 250 ms dans le monde de 51-74 à 20, les à-coups de plus de 1 s de 26-35 à 7, et le streaming de 80-87 s à 74 s. Un groupe de sept switches qui déchirait les murs de la cellule de prison a été abandonné. L'essai du lot sans aucun de ces switches a donné 14 fps et un lancement de 100 s, mais la machine était plus chargée pendant cet essai : ce n'est donc pas une comparaison avant-après propre.

### Machine de test

| Composant | Modèle |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 Go, pilote 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 cœurs / 32 threads |
| Mémoire | 64 Go DDR5-6000 |
| Carte mère | MSI PRO B650-P WIFI |
| Stockage | SSD NVMe (Samsung 990 EVO Plus) |
| Système d'exploitation | Windows 11 (build 26200) |
| Jeu | GTA VI pour Xbox Series, build 1.0.2.1 (votre propre copie) |

Tous les chiffres actuels proviennent de cette machine. Les chiffres plus anciens de ce README ont été mesurés sur une RTX 4070 SUPER (Windows) ou sur une RX 9070 XT (Linux), et chacun le précise.

## Fonctionnement

![Fonctionnement : le code x86-64 du jeu s'exécute nativement sur le CPU ; les appels système de console vont au noyau HLE sous Windows ; les commandes D3D12.X sont reconstruites en Direct3D 12 sur l'hôte ; les shaders RDNA 2 sont traduits en DXIL et mis en cache](../images/how-it-works.svg)

- **Le code du jeu** est chargé et exécuté tel quel. La Xbox Series et un PC Windows partagent le jeu d'instructions x86-64 : aucune instruction n'est émulée.
- **Les appels système** au noyau de la console et aux bibliothèques GDK sont pris en charge par l'émulation de haut niveau (HLE) d'El1xr, au-dessus de Windows : mémoire, threads, fichiers et services système.
- **Graphismes** : les listes de commandes Direct3D 12.X de la console, sa mémoire et ses ressources sont reconstruites en Direct3D 12 standard sur votre GPU.
- **Les shaders** compilés pour le GPU RDNA 2 de la console sont traduits en DXIL. Les 26 148 programmes de shaders du jeu se traduisent tous, et chacun est mis en cache sur disque après sa première utilisation.

## Lancer le jeu

Il vous faut votre propre copie du jeu, Windows 10 ou 11, un GPU prenant en charge le Shader Model 6.6 et les mesh shaders, et Rust.

```powershell
tools\lancer_victory.ps1 -GameDir <dossier du jeu>
```

Le guide complet (installation, réglages, dépannage) se trouve dans [docs/fr/lancer-gta6.md](../fr/lancer-gta6.md).

> [!IMPORTANT]
> Ce dépôt ne contient aucun fichier du jeu, aucune clé ni aucun exécutable, et n'explique pas comment les obtenir.

<details>
<summary>Commandes de développement</summary>

```powershell
python tools/dev.py build      # build release
python tools/dev.py prewarm    # traduit tous les shaders hors ligne (environ 45 s)
python tools/dev.py bench --quiet --seconds 200   # essai de jeu mesuré
python tools/dev.py gate       # tous les tests, échoue au moindre avertissement
```

</details>

## Problèmes connus

- **Défauts visuels variables d'un essai à l'autre.** Dans environ la moitié des essais, des visages deviennent noirs ou tachés (correctif en cours dans [#35](https://github.com/kylepelham/El1xr/issues/35)), Lucia porte la mauvaise tenue au diner, ou ses cheveux deviennent orange à la fenêtre de la prison.
- **Un blocage à l'entrée dans le monde.** Juste après la première apparition du monde, le jeu se fige 3 à 9 s pendant que le GPU traite un lot de commandes. La cause est à l'étude dans [#78](https://github.com/kylepelham/El1xr/issues/78).
- **Streaming lent.** Après l'entrée dans le monde, il faut environ 74 s pour que toutes les ressources arrivent, avec des apparitions tardives d'objets entre-temps.
- **Le GPU attend le CPU** 6 à 10 ms par image (voir le budget d'image plus haut).
- **L'éclairage en ray tracing est en grande partie coupé.** Le jeu construit ses structures de ray tracing en logiciel, et ce constructeur est désactivé pour l'instant.
- **Une seule machine de référence.** Les chiffres actuels proviennent de la RTX 5090 décrite plus haut. Les autres GPU n'ont pas été mesurés avec le build actuel.

## La suite

L'objectif est un 30 fps stable en 4K sur la RTX 5090, puis l'absence d'à-coups et de défauts visuels. Environ 60 accélérations supplémentaires sont écrites et attendent d'être mesurées. Chacune reste désactivée par défaut tant qu'elle n'a pas fait ses preuves.

- **Le blocage à l'entrée dans le monde** ([#78](https://github.com/kylepelham/El1xr/issues/78)) : trouver quelle passe de ce lot de commandes GPU prend plusieurs secondes.
- **Moins d'attente et moins de surcoût** : réduire les 6 à 10 ms pendant lesquelles le GPU attend le CPU à chaque image, ainsi que les 7 ms de travail GPU propre à El1xr.
- **L'upscaling DLSS** et **le ray tracing matériel sur les cœurs RT**, qui permettrait de rétablir l'éclairage en ray tracing.
- **Le préchauffage hors ligne des shaders et des pipelines**, pour qu'aucun shader ne soit compilé en cours de partie.
- **Des correctifs pour les visages noirs ([#35](https://github.com/kylepelham/El1xr/issues/35)) et les tenues qui varient d'un essai à l'autre**.

## Le projet en chiffres

| Chiffre | Quoi |
|---|---|
| 1800+ tests automatisés | outils Python et crates Rust, lancés par `dev.py gate` |
| 26 148 programmes de shaders | traduits de RDNA 2 en DXIL, mis en cache sur disque |
| 0 pipeline refusé | sur la RTX 5090 |

## Contribuer

Les issues et les pull requests sont les bienvenues. Lancez `python tools/dev.py gate` avant d'ouvrir une pull request. Les règles sont dans [CONTRIBUTING.md](../../CONTRIBUTING.md). Pour les questions et les rapports d'essai, contactez Bone/Nevaa sur Discord : **bonemeld**.

## Crédits

- **El1xr** : Bone/Nevaa.
- **VIctory** : Marvisak et ses contributeurs ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 ou ultérieure), ainsi que les auteurs des forks à partir desquels il a été assemblé, dont Neptune (VIctory Edit.) de Traffic & zm0kie.
- **AnyXboxX** : Looper_Salty et ses contributeurs, le projet dont El1xr est un fork.
- **Les forks de VIctory dont nous avons appris et dont nous avons porté du code**, parmi lesquels fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory et readmet3xt-lab/foxhound.
- Chaque composant et chaque auteur, changement par changement, est crédité dans [THIRD_PARTY.md](../../THIRD_PARTY.md). L'historique antérieur du projet se trouve dans [docs/fr/history.md](../fr/history.md).

## Licence

El1xr est distribué sous la [GNU GPL 3.0 ou ultérieure](../../LICENSE). Les parties tierces gardent leur propre licence : le module `Task` de libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) et les tables RDNA 2 générées à partir des tests d'encodage publics de LLVM (Apache 2.0 avec exception LLVM, `PrototypeScarlett/emu/rdna2_ops.py` et `rdna2_shapes.py`).
