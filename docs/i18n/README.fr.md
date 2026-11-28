<div align="center">

![El1xr : Grand Theft Auto VI pour Xbox Series, sur un PC Windows. 21,6-26,0 fps en 4K sur une RTX 5090, 1 pipeline refusé, 24,6 s du lancement au monde, jouable au 28 novembre 2026](../images/hero.svg)

![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-9b5cff?style=flat-square&labelColor=170f2b)
![Platform: Windows 10 and 11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2de2e6?style=flat-square&labelColor=170f2b)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-ff8a3d?style=flat-square&labelColor=170f2b&logo=rust&logoColor=white)
![Status: playable](https://img.shields.io/badge/status-playable-ff3d8b?style=flat-square&labelColor=170f2b)
![21.6-26.0 fps at 4K on RTX 5090](https://img.shields.io/badge/4K%20on%20RTX%205090-21.6--26.0%20fps-ffd36e?style=flat-square&labelColor=170f2b)
![Discord: bonemeld](https://img.shields.io/badge/Discord-bonemeld-5865F2?style=flat-square&labelColor=170f2b&logo=discord&logoColor=white)

[English](../../README.md) · **Français** · [Español](README.es.md) · [Deutsch](README.de.md) · [Italiano](README.it.md) · [Português](README.pt-BR.md) · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

El1xr fait tourner la version Xbox Series de Grand Theft Auto VI (build 1.0.2.1) sur un PC Windows. C'est le projet de Bone/Nevaa. Le code x86-64 du jeu lui-même s'exécute nativement sur votre processeur. Ses appels système de console, ses commandes Direct3D 12.X et ses shaders RDNA 2 sont traduits pour le PC pendant que le jeu tourne. Le projet n'émule pas le CPU et ce dépôt ne contient aucun contenu du jeu : vous fournissez votre propre copie.

## État

> [!NOTE]
> **Au 28 novembre 2026, GTA VI (Xbox Series, build 1.0.2.1) démarre, atteint les menus et se joue en monde ouvert sous Windows.** Lors des derniers essais sur machine au repos (lot 5, 4K, RTX 5090), le jeu tourne entre 21,6 et 22,4 fps sur des essais de 110 secondes depuis le lancement, et entre 25,8 et 26,0 fps sur des essais de 180 et 460 secondes. C'est encore en dessous du prochain objectif, un 30 fps stable. Viendront ensuite l'absence d'à-coups et l'absence de défauts visuels.

![Étapes : démarrage, menu principal et en jeu au plus tard le 24 novembre 2026 ; jouable le 27 novembre 2026, aujourd'hui à 21,6-26,0 fps ; 30 fps ensuite ; stabilité en cours](../images/milestones.svg)

| Étape | État | Éléments de preuve | Date |
|---|---|---|---|
| L'exécutable tourne | Fait | L'exécutable Xbox Series 1.0.2.1 démarre sous Windows. Son code x86-64 s'exécute nativement. | au plus tard le 24 nov. 2026 |
| Première image | Fait | L'animation de démarrage et les écrans légaux s'affichent sans scintillement. | au plus tard le 24 nov. 2026 |
| Menu principal | Fait | L'écran de profil, les réglages du premier lancement, les écrans légaux, l'écran de chargement et le menu pause fonctionnent tous. Les menus tournent à 30 fps (RTX 4070 SUPER). | au plus tard le 24 nov. 2026 |
| En jeu | Fait | Le jeu atteint le monde ouvert et le personnage se déplace sous le contrôle du joueur. Une session de 13 minutes dans la prison a tourné à environ 4 fps (RTX 4070 SUPER). | au plus tard le 24 nov. 2026 |
| Jouable | Fait, sous la vitesse visée | Le jeu tourne à 21,6-26,0 fps en 4K (RTX 5090 ; essais du lot 5 de 110 à 460 secondes, sur machine au repos). | 28 nov. 2026 |
| Progression de l'histoire | Deux premières missions terminées | Un contributeur a terminé les deux premières missions au cours d'une session de 45 minutes, à environ 2 fps (RTX 4070 SUPER ; rapporté, non mesuré dans les journaux). Toutes les cinématiques se jouent en entier. Sous Linux (Proton, RX 9070 XT), un essai de 23 minutes a dépassé le prologue et atteint le chapitre 1. | 25 nov. 2026 |
| 30 fps | Pas encore | 21,6-26,0 fps, soit 38 à 46 ms par image, pour un objectif de 33,3 ms (RTX 5090, 4K). | 28 nov. 2026 |
| Stabilité | En cours | Avec les réglages par défaut, les essais du lot 5 n'ont connu ni réinitialisation du pilote GPU ni perte du périphérique. Le blocage de 8 à 9 s à l'entrée dans le monde, observé dans 5 essais sur 5 au début du lot 5, n'est pas revenu dans les 47 lancements examinés depuis que les enregistrements indirects bruts sont vérifiés. Le jeu peut encore planter quand Windows atteint sa limite de mémoire validée (commit), et les défauts visuels varient encore d'un essai à l'autre (voir Problèmes connus). | 28 nov. 2026 |

## Performances

![En bref : 21,6-26,0 fps, 24,6 s du lancement à la première image du monde (contre 26,7 s), 0 flash sombre d'une image au diner (contre 12 à 19 par essai), 2,9 Go de mémoire de buffers GPU après 180 s (contre 4,5 Go)](../images/stats.svg)

| Mesure (RTX 5090, 4K) | Aujourd'hui | Objectif |
|---|---|---|
| Fréquence d'images, essais de 110 s depuis le lancement | **21,6-22,4 fps** | 30+ fps |
| Fréquence d'images, essais de 180 s et de 460 s | **25,8-26,0 fps** | 30+ fps |
| Temps CPU pour préparer une image | 15,7-21,2 ms | inférieur au temps GPU |
| Pipelines GPU refusés | 1 | 0 |
| Du lancement à la première image du monde | 23,1-24,6 s | |
| Blocage à l'entrée dans le monde | aucun en 47 lancements ; pire image d'un essai 771-832 ms | aucun à-coup |
| Images de plus de 250 ms dans le monde | 3-6 par essai de 110 s | aucune |
| Flashs sombres d'une image au diner | 0 (contre 12-19 par essai) | aucun |
| Mémoire de buffers GPU après 180 s | 2,9 Go (contre 4,5 Go) | |

Ces chiffres proviennent du lot 5 (terminé le 28 novembre 2026), avec les switches que le lanceur active désormais. Tous les essais ont eu lieu sur machine au repos : le reste du travail de la machine était suspendu ou retenu, le jeu démarrait rapidement depuis un point de sauvegarde fixe, et les chiffres ont été lus dans le journal d'images de l'émulateur lui-même (la fréquence d'images et le temps CPU sont des médianes sur la seconde moitié des images d'un essai passées dans le monde). Les chiffres sur 110 secondes viennent de deux paires d'essais de chargement consécutifs, celui sur 180 secondes de la paire `CARD_BLOCK_REUSE`, et celui sur 460 secondes d'un lancement de test CPU. La fréquence d'images dépend de la scène et de la durée de l'essai : ne comparez des chiffres qu'au sein d'une même paire. Une partie de la journalisation de diagnostic était active dans chaque essai.

![Où passe une image : passes du jeu 22,8 ms, travail GPU d'El1xr 7,0 ms, non mesuré 3,8 ms, GPU inactif 6,2 ms, face à la limite de 33,3 ms pour 30 fps](../images/frame-budget.svg)

Le dernier profil de temps GPU, mesuré avant le lot 3, donnait environ 40 ms pour une image stabilisée, dont 22,8 ms pour les passes de rendu du jeu lui-même. Le lot 5 a montré que l'image est désormais limitée par le travail CPU qui la prépare et l'envoie, et non par le GPU : avec le DLSS rendant aux deux tiers de la 4K, la fréquence d'images réelle est restée à 21,8-22,0 fps, la même qu'en 4K native (20,1-22,2 fps). La plupart des changements conservés au lot 5 réduisent donc le temps CPU par image.

![Lot 5 : avec le groupe de chargement, du lancement à la première image du monde de 26,2-26,7 s à 23,1-24,6 s ; avec TABLE_NULL_ONCE, flashs sombres d'une image au diner de 12-19 par essai à 0 ; avec HEAP_WARM, première image du monde en 614-702 ms au lieu de 781-1048 ms](../images/load-times.svg)

Le lot 5 a conservé ces changements, désormais activés par défaut dans le lanceur. Les switches des lots 3 et 4 restent activés.

- **Un chargement plus rapide** (`VICTORY_TEXTURE_HEAPS=1`, `VICTORY_TEXTURE_BLOCK_THREAD=1`, `VICTORY_DSTORAGE_STAGING_MB=256`, `VICTORY_DSTORAGE_POLL_WAIT=1`, `VICTORY_BAKE_STAGE_ALL=1`) : les textures streamées partagent de grands blocs de mémoire créés sur un thread dédié, jusqu'à 256 Mio de lectures DirectStorage sont en cours au lieu de 32, l'interrogation par le jeu des lectures disque terminées rend la main dès qu'elles le sont, et les passes de bake sont préparées ensemble pendant le chargement. Dans la paire de confirmation au repos, le temps du lancement à la première image du monde est passé de 26,7 à 24,6 s, celui du lancement à la première image complète du monde de 29,2 à 26,2 s, et la pire image de 1049 à 832 ms, à fréquence d'images égale. La première paire allait dans le même sens (26,2 à 23,1 s).
- **Scintillement du diner corrigé** (`VICTORY_TABLE_NULL_ONCE=1`) : au diner, l'arrière-plan devenait sombre pendant une image environ toutes les 2 s, 12 à 19 fois par essai. Une table de descripteurs vidait l'emplacement d'une texture chaque fois que le travail de préparation de cette texture était élagué ; désormais, un emplacement n'est vidé qu'une fois au plus. Avec ce switch, aucune image de ce type n'est apparue sur un essai de 110 secondes ni sur deux segments de test de 18 secondes, sans coût mesuré.
- **Plus de blocage à l'entrée dans le monde** (`VICTORY_RAW_CHECK=2`) : les enregistrements de draws indirects écrits par les shaders du jeu sont vérifiés avant leur exécution. Au début du lot 5, chaque essai se figeait 8,2 à 9,0 s environ 4 s après l'entrée dans le monde (5 sur 5) ; aucun des 47 lancements examinés après ce changement ne s'est bloqué.
- **Moins de temps CPU par image** :
  - `VICTORY_SKIP_WC_BACK=1` : les grandes écritures de shaders en mémoire write-combining restent sur la carte au lieu d'être recopiées. 24,6 fps sur 2 essais sur 2, contre 21,5 fps pour le meilleur essai au repos sans ce switch, et 7,3 ms de temps CPU en moins par image.
  - `VICTORY_CARD_BLOCK_REUSE=1` : les morceaux de mémoire GPU sont remis à zéro et réutilisés au lieu d'être alloués de nouveau. De 23,6 à 26,0 fps sur une paire de 180 secondes au repos, avec 2,9 Go de mémoire de buffers GPU à la fin au lieu de 4,5 Go.
  - `VICTORY_SEND_THREAD=1` et `VICTORY_CARD_RECORD_SEND=1` : les listes de commandes sont envoyées, et les listes de synchronisation GPU enregistrées, sur un thread dédié. Le temps d'envoi sur le thread d'image est passé de 1,56 à 0,11 ms.
  - `VICTORY_FLUSH_BESIDE=1`, `VICTORY_RESET_ASIDE=1`, `VICTORY_COPIED_SHARED=1`, `VICTORY_TEXTURE_HASH_BEHIND=1`, `VICTORY_RESULT_RING=1`, et `VICTORY_WIDE_WAKE_ONE=1` avec `VICTORY_WAIT_SPIN=50` : des gains plus modestes, de 0,5 à 1,8 ms de temps CPU par image chacun, ou jusqu'à 1,4 fps, mesurés en activant puis en désactivant chacun au cours d'un même essai.
- **Une entrée dans le monde plus rapide** (`VICTORY_HEAP_WARM=1`) : les copies hôtes des heaps de vues du jeu sont créées pendant l'écran de chargement ; la première image du monde a ainsi pris 614 ms au lieu de 1048 ms (seconde paire ; première paire de 781 à 702 ms).
- **Moins d'images lentes** (`VICTORY_AMBIENT_ONCE=1`) : le substitut de l'éclairage ambiant du jeu ne compte qu'un rebond. Le temps d'image au 99e centile a baissé de 11 ms lors d'un test au sein d'un même essai.

L'upscaling en 4K ne plante plus : trois lancements avec upscaling se sont déroulés sans division par zéro (0xc0000094). L'upscaling DLSS donne une image nette mais n'apporte aucun gain de fréquence d'images, car l'image attend le CPU. La génération d'images DLSS (`-FrameGen`, ou Frame generation = dlss dans le menu F9) présente environ 44 à 47 fps à partir de 22 à 24 images réelles ; elle reste désactivée par défaut tant que les images générées n'ont pas été vérifiées à l'œil. Le lot 5 a aussi abandonné des switches qui cassaient quelque chose ou coûtaient de la vitesse, dont `PIPELINE_PIECES` (écran noir), `BAKE_SPREAD=12` (1,2 à 2,2 fps plus lent) et `GUARD_PUBLISH_TRY` (un plantage). Le constructeur de ray tracing du jeu fonctionne quand on l'active, mais tourne à 0,6-1,1 fps ; il reste donc désactivé.

### Machine de test

| Composant | Modèle |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 Go, pilote 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 cœurs / 32 threads |
| Mémoire | 64 Go DDR5-6000 |
| Fichier d'échange | 64 Go (limite de mémoire validée : 142 Go) |
| Carte mère | MSI PRO B650-P WIFI |
| Stockage | SSD NVMe (Samsung 990 EVO Plus) |
| Système d'exploitation | Windows 11 (build 26200) |
| Jeu | GTA VI pour Xbox Series, build 1.0.2.1 (votre propre copie) |

Tous les chiffres actuels proviennent de cette machine. Les chiffres plus anciens de ce README ont été mesurés sur une RTX 4070 SUPER (Windows) ou sur une RX 9070 XT (Linux), et chacun le précise.

## Fonctionnement

![Fonctionnement : le code x86-64 du jeu s'exécute nativement sur le CPU ; les appels système de console vont au noyau HLE sous Windows ; les commandes D3D12.X sont reconstruites en Direct3D 12 sur l'hôte ; les shaders RDNA 2 sont traduits en DXIL et mis en cache](../images/how-it-works.svg)

- **Le code du jeu** est chargé et exécuté tel quel. La Xbox Series et un PC Windows partagent le jeu d'instructions x86-64 : aucune instruction n'est émulée.
- **Les appels système** au noyau de la console et aux bibliothèques GDK sont pris en charge par l'émulation de haut niveau (HLE) d'El1xr, au-dessus de Windows : mémoire, threads, fichiers et services système.
- **Lectures disque** : les requêtes DirectStorage du jeu sont servies par les threads de travail propres à El1xr, avec jusqu'à 256 Mio en cours. Les textures streamées sont placées dans des blocs de mémoire partagés : environ 115 000 textures dans 116 blocs lors d'un essai typique.
- **Graphismes** : les listes de commandes Direct3D 12.X de la console, sa mémoire et ses ressources sont reconstruites en Direct3D 12 standard sur votre GPU. Les listes de commandes sont envoyées depuis un thread dédié, ce qui allège d'autant le thread d'image du jeu.
- **Heaps de descripteurs** : le jeu adresse jusqu'à 1 245 184 emplacements de descripteurs (sa table de vues de textures et de buffers). El1xr les reflète dans un unique heap hôte d'environ 1 000 000 d'emplacements, partagé avec ses propres vues, et crée les copies hôtes pendant l'écran de chargement. Faire tenir le reste est encore un problème ouvert (voir Problèmes connus).
- **Les shaders** compilés pour le GPU RDNA 2 de la console sont traduits en DXIL. Sur les 26 148 programmes de shaders du jeu, tous sauf 6 se traduisent, et chaque traduction est mise en cache sur disque. `dev.py prewarm` construit les 25 751 pipelines GPU du jeu avant un essai, pour qu'ils ne soient pas compilés en cours de partie.

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

- **Le rebord de la fenêtre de la prison s'assombrit après le prologue.** Dans le plan de la cellule qui suit le titre PROLOGUE, le rebord ensoleillé devient sombre et perd ses détails d'une image à l'autre, avec la même caméra. Reproduit deux fois au lot 5 ; la cause est à l'étude.
- **La peau peut devenir noire.** Dans certains essais, la peau d'un personnage, le plus souvent au diner, devient d'un noir uni pendant quelques secondes, puis revient ([#35](https://github.com/kylepelham/El1xr/issues/35)). La chemise de la serveuse change aussi de couleur d'un essai à l'autre.
- **Des carrés sombres sur une fenêtre.** Dans la cinématique d'ouverture, vers 44 s, des carrés sombres en escalier couvrent la fenêtre derrière Jason. Le lot 5 a écarté deux causes soupçonnées ; la recherche continue.
- **Limite du heap de descripteurs.** Le jeu adresse jusqu'à 1 245 184 emplacements de descripteurs, mais le heap hôte de la RTX 5090 en contient environ 1 000 000. Dans les essais du lot 5, 313 000 à 504 000 des vues actives du jeu se trouvaient au-delà de la partie que le heap hôte peut contenir. Aucune façon de les faire tenir n'a encore été conservée.
- **L'écran du téléphone est noir** dans le plan de la table au diner.
- **Une forte consommation de mémoire.** Le processus du jeu culmine à 33-35 Go. Avec un fichier d'échange trop petit, il plante quand Windows atteint sa limite de mémoire validée. Le PC de test utilise un fichier d'échange de 64 Go.
- **Streaming lent.** Après l'entrée dans le monde, il faut environ 72 s pour que toutes les ressources arrivent, avec des apparitions tardives d'objets entre-temps.
- **L'image attend le CPU.** Préparer et envoyer une image demande 15,7 à 21,2 ms de temps CPU, et c'est pour cette raison que l'upscaling n'apporte aucun gain de fréquence d'images.
- **L'éclairage en ray tracing est désactivé.** Le jeu construit ses structures de ray tracing en logiciel ; ce constructeur fonctionne mais tourne à 0,6-1,1 fps, il reste donc désactivé.
- **Une seule machine de référence.** Les chiffres actuels proviennent de la RTX 5090 décrite plus haut. Les autres GPU n'ont pas été mesurés avec le build actuel.

## La suite

L'objectif est un 30 fps stable en 4K sur la RTX 5090, puis l'absence d'à-coups et de défauts visuels. Chaque nouveau changement reste désactivé par défaut tant que des essais sur machine au repos n'en ont pas fait la preuve.

- **Moins de temps CPU par image** : l'image est limitée par le CPU, et les prochains lots continuent donc de sortir du thread d'image le travail d'envoi et de vérification des textures.
- **Le rebord et les carrés de la fenêtre**, ainsi que **la peau qui devient noire** ([#35](https://github.com/kylepelham/El1xr/issues/35)).
- **Le heap de descripteurs** : faire tenir les 1 245 184 emplacements du jeu dans le heap hôte.
- **Le ray tracing matériel sur les cœurs RT**, qui permettrait de rétablir l'éclairage en ray tracing.
- **La génération d'images DLSS par défaut**, une fois ses images générées vérifiées à l'œil.

## Le projet en chiffres

| Chiffre | Quoi |
|---|---|
| 4000+ tests réussis | crates Rust (`cargo test`) et outils Python (`pytest`), lancés par `dev.py gate` |
| 26 148 programmes de shaders | traduits de RDNA 2 en DXIL et mis en cache sur disque ; 6 ne se traduisent pas encore |
| 25 751 pipelines GPU | construits avant la partie par `dev.py prewarm` |
| 1 pipeline refusé | sur la RTX 5090, lors des derniers essais |

## Contribuer

Les issues et les pull requests sont les bienvenues. Lancez `python tools/dev.py gate` avant d'ouvrir une pull request. Les règles sont dans [CONTRIBUTING.md](../../CONTRIBUTING.md). Pour les questions et les rapports d'essai, contactez Bone/Nevaa sur Discord : **bonemeld**.

## Crédits

- **El1xr** : Bone/Nevaa.
- **VIctory** : Marvisak et ses contributeurs ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 ou ultérieure), ainsi que les auteurs des forks à partir desquels il a été assemblé, dont Neptune (VIctory Edit.) de Traffic & zm0kie.
- **AnyXboxX** : Looper_Salty et ses contributeurs, le projet dont El1xr est un fork.
- **Les forks de VIctory dont nous avons appris et dont nous avons porté du code**, parmi lesquels fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory, readmet3xt-lab/foxhound, RipcuBAN/victory-fork et Kemerd/VIctory-NVIDIA-RT.
- Chaque composant et chaque auteur, changement par changement, est crédité dans [THIRD_PARTY.md](../../THIRD_PARTY.md). L'historique antérieur du projet se trouve dans [docs/fr/history.md](../fr/history.md).

## Licence

El1xr est distribué sous la [GNU GPL 3.0 ou ultérieure](../../LICENSE). Les parties tierces gardent leur propre licence : le module `Task` de libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) et les tables RDNA 2 générées à partir des tests d'encodage publics de LLVM (Apache 2.0 avec exception LLVM, `PrototypeScarlett/emu/rdna2_ops.py` et `rdna2_shapes.py`).
