<div align="center">

![El1xr: Grand Theft Auto VI para Xbox Series rodando em um PC com Windows. 23,1-25,7 fps em 4K em uma RTX 5090, 0 pipelines recusados, 27-28 s da inicialização ao mundo, jogável desde 27 de novembro de 2026](../images/hero.svg)

![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-9b5cff?style=flat-square&labelColor=170f2b)
![Platform: Windows 10 and 11](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-2de2e6?style=flat-square&labelColor=170f2b)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-ff8a3d?style=flat-square&labelColor=170f2b&logo=rust&logoColor=white)
![Status: playable](https://img.shields.io/badge/status-playable-ff3d8b?style=flat-square&labelColor=170f2b)
![23.1-25.7 fps at 4K on RTX 5090](https://img.shields.io/badge/4K%20on%20RTX%205090-23.1--25.7%20fps-ffd36e?style=flat-square&labelColor=170f2b)
![Discord: bonemeld](https://img.shields.io/badge/Discord-bonemeld-5865F2?style=flat-square&labelColor=170f2b&logo=discord&logoColor=white)

[English](../../README.md) · [Français](README.fr.md) · [Español](README.es.md) · [Deutsch](README.de.md) · [Italiano](README.it.md) · **Português** · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

O El1xr roda a versão de Xbox Series de Grand Theft Auto VI (build 1.0.2.1) em um PC com Windows. É o projeto de Bone/Nevaa. O próprio código x86-64 do jogo roda nativamente no seu processador. As chamadas de sistema do console, os comandos Direct3D 12.X e os shaders RDNA 2 são traduzidos para o PC enquanto o jogo roda. Não há emulação de CPU nem conteúdo do jogo neste repositório: você usa a sua própria cópia.

## Status

> [!NOTE]
> **Em 27 de novembro de 2026, GTA VI (Xbox Series, build 1.0.2.1) inicializa, chega aos menus e é jogável no mundo aberto no Windows.** Nas últimas execuções de teste com a máquina sem outra carga (lote 4, 4K, RTX 5090), o jogo roda a 23,1-25,7 fps ao longo de uma execução de 200 segundos, com 0 pipelines de GPU recusados. Isso ainda fica abaixo da próxima meta, 30 fps estáveis. Depois disso vêm a eliminação dos engasgos e das falhas visuais.

![Marcos: inicialização, menu principal e jogo em andamento até 24 de novembro de 2026; jogável a 23,1-25,7 fps em 27 de novembro de 2026; 30 fps a seguir; estabilidade em andamento](../images/milestones.svg)

| Marco | Status | Evidência | Data |
|---|---|---|---|
| Executável roda | Concluído | O executável 1.0.2.1 de Xbox Series inicia no Windows. Seu código x86-64 roda nativamente. | até 24/11/2026 |
| Primeiro quadro | Concluído | A animação de abertura e as telas legais são renderizadas sem cintilação. | até 24/11/2026 |
| Menu principal | Concluído | Tela de perfil, configurações da primeira execução, telas legais, tela de carregamento e menu de pausa funcionam. Os menus rodam a 30 fps (RTX 4070 SUPER). | até 24/11/2026 |
| Jogo em andamento | Concluído | O jogo chega ao mundo aberto e o personagem se move sob o controle do jogador. Uma sessão de 13 minutos na prisão rodou a cerca de 4 fps (RTX 4070 SUPER). | até 24/11/2026 |
| Jogável | Concluído, abaixo da velocidade-alvo | O jogo roda a 23,1-25,7 fps em 4K (RTX 5090; duas execuções de 200 segundos do lote 4, com a máquina sem outra carga). | 27/11/2026 |
| Progresso na história | Duas primeiras missões concluídas | Um colaborador concluiu as duas primeiras missões em uma sessão de 45 minutos a cerca de 2 fps (RTX 4070 SUPER; relatado, não medido a partir dos logs). Todas as cutscenes são reproduzidas por inteiro. No Linux (Proton, RX 9070 XT), uma execução de 23 minutos passou do prólogo e chegou ao capítulo 1. | 25/11/2026 |
| 30 fps | Ainda não | 23,1-25,7 fps, tempo de quadro mediano de 40-43 ms contra a meta de 33,3 ms (RTX 5090, 4K). | 27/11/2026 |
| Estável | Em andamento | Com as configurações padrão, execuções de teste de 200 segundos terminam sem travamento, sem reset do driver de GPU e sem perda do dispositivo. A parada ao entrar no mundo ficou mais rara (1 de 8 execuções sem outra carga no lote 4), mas não sumiu ([#78](https://github.com/kylepelham/El1xr/issues/78)), e as falhas visuais ainda variam de uma execução para outra (veja Problemas conhecidos). | 27/11/2026 |

## Desempenho

![Visão geral: 23,1-25,7 fps, 27-28 s da inicialização ao mundo, 0 rostos pretos em 24 capturas da lanchonete com as entradas dos bakes primeiro, 31 quadros pretos exibidos durante o carregamento em vez de 152](../images/stats.svg)

| Medida (RTX 5090, 4K) | Agora | Meta |
|---|---|---|
| Taxa de quadros em uma execução de 200 s | **23,1-25,7 fps** | 30+ fps |
| Tempo de quadro mediano | 40-43 ms | 33,3 ms ou menos |
| Quadro no percentil 99 em jogo estabilizado | 64-69 ms | sem engasgos |
| Tempo de CPU para preparar um quadro | 26-28 ms | abaixo do tempo da GPU |
| Pipelines de GPU recusados | **0** | 0 |
| Da inicialização ao primeiro quadro do mundo | 27-28 s | |
| Parada ao entrar no mundo | 6,6 s em 1 de 8 execuções sem outra carga; pior quadro de 1,2-3,0 s nas demais ([#78](https://github.com/kylepelham/El1xr/issues/78)) | nenhuma |
| Quadros acima de 250 ms no mundo | 10-12 por execução | nenhum |
| Quadros pretos exibidos nos primeiros 90 s | 31 | nenhum |
| Streaming após entrar no mundo | 71-72 s | poucos segundos |

Esses números vêm do lote 4 (27 de novembro de 2026), com os switches que o inicializador agora liga. Taxa de quadros, tempos de quadro, tempo de CPU, quadros acima de 250 ms e streaming vêm de duas execuções de 200 segundos com a máquina sem outra carga: cada uma faz uma inicialização rápida a partir de um checkpoint fixo enquanto o resto do trabalho da máquina fica pausado, e os números são lidos do próprio log de quadros do emulador. O tempo de inicialização vem de duas execuções com os novos switches de carregamento, alternadas com duas execuções sem eles (32-33 s), enquanto compilações rodavam em outros núcleos da CPU. Os quadros pretos foram contados em uma execução de 90 segundos sem outra carga. Parte dos logs de diagnóstico estava ligada em todas as execuções.

![Para onde vai um quadro: passes do jogo 22,8 ms, trabalho de GPU do El1xr 7,0 ms, não medido 3,8 ms, GPU ociosa 6,2 ms, contra a linha de 33,3 ms dos 30 fps](../images/frame-budget.svg)

O último perfil de tempos de GPU, feito antes do lote 3, indicava cerca de 40 ms por quadro estabilizado. Os passes de renderização do próprio jogo consomem 22,8 ms de tempo de GPU, o que deixa espaço para 30 fps. O restante é o trabalho de GPU do próprio El1xr (7 ms: verificação de limites em draws indiretos, cópias de texturas entre formatos, envios) e o tempo em que a GPU fica ociosa (6-10 ms) enquanto uma única thread da CPU prepara o próximo lote de trabalho. São essas duas parcelas que as próximas atualizações atacam.

![Lote 4: com os switches de carregamento, da inicialização ao primeiro quadro do mundo caiu de 32-33 s para 27-28 s; mantendo o último quadro, os quadros pretos exibidos nos primeiros 90 s caíram de 152 para 31; com as entradas dos bakes primeiro, as capturas da lanchonete com um rosto preto caíram de 0-3 por execução para 0](../images/load-times.svg)

O lote 4 (27 de novembro de 2026) manteve quatro mudanças, agora ligadas por padrão no inicializador. Os switches do lote 3 continuam ligados.

- **Entradas dos bakes primeiro** (`VICTORY_BAKE_INPUTS=1`): os bakes de rosto e pele recebem suas texturas de entrada antes de tudo. Na lanchonete, nenhuma das 24 capturas de duas execuções mostrou um rosto preto, contra 0-3 por execução sem o switch, sem custo mensurável na taxa de quadros.
- **Carregamento mais rápido** (`VICTORY_LOAD_FPS=60`, `VICTORY_LOAD_END=world`, `VICTORY_LOAD_UNPACED=1`): a tela de carregamento roda a até 60 fps até o mundo aparecer. O tempo da inicialização ao primeiro quadro do mundo caiu de 32-33 s para 27-28 s em execuções seguidas, sem nova parada ao entrar no mundo.
- **Manter o último quadro** (`VICTORY_HOLD_LAST_FRAME=1`): quando o jogo apresenta um quadro todo preto, o último quadro bom fica na tela no lugar dele. Em uma execução de 90 segundos sem outra carga, ele cobriu 40 dos 71 quadros pretos que o jogo apresentou; os 31 restantes vêm logo no começo da inicialização, antes de existir um quadro para manter. Uma execução equivalente sem o switch mostrou 152 quadros pretos na tela.
- **Menu de configurações no jogo** (`VICTORY_MENU=1`): aperte F9, ou segure View e Menu por um segundo, com a janela do jogo em primeiro plano. Um autoteste no lote seguinte o abriu duas vezes sobre o jogo rodando, desenhou todas as 10 abas e devolveu os controles ao jogo ao fechar.

Contra uma execução com todos os switches desligados, os do lote 3 também reduziram os quadros pretos dos primeiros 90 s de 116 para 77, e o pior quadro ao entrar no mundo de 5,8 s para 1,7-2,7 s. Outros oito switches foram descartados: um rasgava a parede e o beliche da cela da prisão (`SPLIT_AHEAD`), dois deixavam rostos ou a cela pretos ou com ruído (`EXCHANGE_PAIRS`, `GPU_DETILE`), um trazia de volta os rostos pretos ao entrar no mundo, embora poupasse 5 s de carregamento (`BAKE_ONE_BUDGET`), um somava 13 ms de tempo de CPU por quadro (`FLUSH_ONE_PASS`), um deixava passar mudanças nas texturas (`TEXTURE_HASH_SAMPLED`), e dois não mudavam nada mensurável (`CARD_DECL_JOIN`, `STALE_BATCH`). Os testes também mostraram que o upscaling em 4K derrubava o jogo (uma divisão por zero, 0xc0000094); uma correção está escrita e aguarda seu teste. Depois de jogar esta build, o mantenedor a descreveu como "bem mais estável, menos engasgos, taxa de quadros mais estável, menos cintilação"; é uma impressão de jogo, não uma medição.

### Máquina de teste

| Componente | Modelo |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, driver 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 núcleos / 32 threads |
| Memória | 64 GB DDR5-6000 |
| Placa-mãe | MSI PRO B650-P WIFI |
| Armazenamento | SSDs NVMe (Samsung 990 EVO Plus) |
| Sistema operacional | Windows 11 (build 26200) |
| Jogo | GTA VI para Xbox Series, build 1.0.2.1 (sua própria cópia) |

Todos os números atuais vêm desta máquina. Os números mais antigos deste README foram medidos em uma RTX 4070 SUPER (Windows) ou em uma RX 9070 XT (Linux), e cada um deles indica isso.

## Como funciona

![Como funciona: o código x86-64 do jogo roda nativamente na CPU; as chamadas de sistema do console vão para o kernel HLE no Windows; os comandos D3D12.X são reconstruídos como Direct3D 12 do host; os shaders RDNA 2 são traduzidos para DXIL e mantidos em cache](../images/how-it-works.svg)

- **O código do jogo** é carregado e executado como está. O Xbox Series e um PC com Windows compartilham o conjunto de instruções x86-64, então nenhuma instrução é emulada.
- **As chamadas de sistema** ao kernel do console e às bibliotecas do GDK são atendidas pela emulação de alto nível (HLE) do El1xr sobre o Windows: memória, threads, arquivos e serviços do sistema.
- **Gráficos**: as listas de comandos Direct3D 12.X, a memória e os recursos do console são reconstruídos como Direct3D 12 padrão na sua GPU.
- **Os shaders** compilados para a GPU RDNA 2 do console são traduzidos para DXIL. Todos os 26.148 programas de shader do jogo são traduzidos, e cada um fica em cache no disco depois do primeiro uso.

## Como rodar

Você precisa da sua própria cópia do jogo, Windows 10 ou 11, uma GPU com Shader Model 6.6 e mesh shaders, e Rust.

```powershell
tools\lancer_victory.ps1 -GameDir <pasta do jogo>
```

O guia completo (instalação, configurações, solução de problemas) está em [docs/running-gta6.md](../running-gta6.md).

> [!IMPORTANT]
> Este repositório não contém arquivos do jogo, chaves nem executáveis, e não explica como obtê-los.

<details>
<summary>Comandos para desenvolvedores</summary>

```powershell
python tools/dev.py build      # build de release
python tools/dev.py prewarm    # traduz todos os shaders offline (cerca de 45 s)
python tools/dev.py bench --quiet --seconds 200   # execução medida do jogo
python tools/dev.py gate       # todos os testes, falha com qualquer aviso
```

</details>

## Problemas conhecidos

- **Falhas visuais que variam de uma execução para outra.** Rostos pretos ou manchados ficaram mais raros com as entradas dos bakes primeiro, mas ainda aparecem em algumas execuções ([#35](https://github.com/kylepelham/El1xr/issues/35)). A Lucia ainda pode aparecer com a roupa errada na lanchonete, e o cabelo dela pode ficar laranja na janela da prisão.
- **Uma parada ao entrar no mundo.** Ela é mais rara que no lote 3, em que quase toda execução congelava por 3-9 s, mas uma das oito execuções sem outra carga do lote 4 ainda congelou por 6,6 s enquanto a GPU processava um lote de comandos. A causa está sendo investigada em [#78](https://github.com/kylepelham/El1xr/issues/78).
- **Streaming lento.** Depois de entrar no mundo, leva cerca de 71 s para todos os assets chegarem, com pop-in enquanto isso.
- **O upscaling trava o jogo em 4K.** Com o upscaling DLSS ou FSR ligado, o jogo cai assim que o mundo aparece (uma divisão por zero no código FSR 4 do jogo). Uma correção aguarda seu teste; o upscaling vem desligado por padrão.
- **A GPU espera pela CPU** de 6 a 10 ms por quadro (veja o orçamento de quadro acima).
- **A iluminação com ray tracing está quase toda desligada.** O jogo constrói suas estruturas de ray tracing em software, e esse construtor está desativado por enquanto.
- **Uma única máquina de referência.** Os números atuais são da RTX 5090 acima. Outras GPUs ainda não foram medidas com a build atual.

## Próximos passos

A meta é 30 fps estáveis em 4K na RTX 5090 e, depois, nenhum engasgo e nenhuma falha visual. Cerca de 60 outras otimizações estão escritas e aguardando medição. Cada uma fica desligada por padrão até ser comprovada.

- **A parada ao entrar no mundo** ([#78](https://github.com/kylepelham/El1xr/issues/78)): descobrir qual passe desse lote de comandos de GPU leva segundos.
- **Menos espera e menos overhead**: reduzir os 6-10 ms em que a GPU espera pela CPU a cada quadro e os 7 ms de trabalho de GPU do próprio El1xr.
- **Upscaling DLSS** (depois que a correção da queda for testada) e **ray tracing por hardware nos RT cores**, o que traria de volta a iluminação com ray tracing.
- **Pré-aquecimento offline de shaders e pipelines**, para que nenhum shader seja compilado durante o jogo.
- **Correções para os rostos pretos ([#35](https://github.com/kylepelham/El1xr/issues/35)) e as roupas que variam de uma execução para outra**.

## Em números

| Número | O quê |
|---|---|
| 1800+ testes automatizados | ferramentas em Python e crates em Rust, executados por `dev.py gate` |
| 26.148 programas de shader | traduzidos de RDNA 2 para DXIL, em cache no disco |
| 0 pipelines recusados | na RTX 5090 |

## Como contribuir

Issues e pull requests são bem-vindos. Rode `python tools/dev.py gate` antes de abrir um pull request. As regras estão em [CONTRIBUTING.md](../../CONTRIBUTING.md). Dúvidas e relatos de testes podem ser enviados a Bone/Nevaa no Discord: **bonemeld**.

## Créditos

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak e seus colaboradores ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 ou posterior), e os autores dos forks a partir dos quais ele foi montado, entre eles Neptune (VIctory Edit.), de Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty e seus colaboradores, o projeto do qual o El1xr é um fork.
- **Os forks do VIctory com os quais aprendemos e dos quais portamos código**, entre eles fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory e readmet3xt-lab/foxhound.
- Cada componente e autor, mudança por mudança, é creditado em [THIRD_PARTY.md](../../THIRD_PARTY.md). A história anterior do projeto está em [docs/history.md](../history.md).

## Licença

O El1xr é distribuído sob a [GNU GPL 3.0 ou posterior](../../LICENSE). Partes de terceiros mantêm sua própria licença: o módulo `Task` da libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) e as tabelas RDNA 2 geradas a partir dos testes públicos de codificação do LLVM (Apache 2.0 com exceção LLVM, `PrototypeScarlett/emu/rdna2_ops.py` e `rdna2_shapes.py`).
