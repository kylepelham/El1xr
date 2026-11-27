<div align="center">

# El1xr

**Grand Theft Auto VI (Xbox Series, build 1.0.2.1) rodando em um PC com Windows.**<br>
4K, cerca de 25 fps e subindo, em uma RTX 5090.

[![Licença: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](../../LICENSE)
[![Plataforma: Windows](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078D6?logo=windows&logoColor=white)](../running-gta6.md)
[![Feito em Rust](https://img.shields.io/badge/built%20with-Rust-B7410E?logo=rust&logoColor=white)](../../third_party/victory)
[![Discord](https://img.shields.io/badge/Discord-AnyXboxX-5865F2?logo=discord&logoColor=white)](https://discord.gg/TcX42vRyPU)

[English](../../README.md) · [Français](README.fr.md) · [Español](README.es.md) · [Deutsch](README.de.md) · [Italiano](README.it.md) · **Português** · [日本語](README.ja.md) · [中文](README.zh-CN.md) · [Русский](README.ru.md)

</div>

---

El1xr é o projeto de emulação de GTA VI de Bone/Nevaa: um fork do [AnyXboxX](https://github.com/LooperSalty/AnyXboxX), construído sobre o [VIctory](https://github.com/Marvisak/VIctory) e sobre o melhor trabalho dos forks do VIctory (todas as fontes são creditadas em [THIRD_PARTY.md](../../THIRD_PARTY.md)). O próprio código x86-64 do jogo roda nativamente; as chamadas de sistema do console, os comandos Direct3D 12.X e os shaders RDNA 2 são traduzidos para o PC em tempo real.

> [!NOTE]
> **Status, 27 de novembro de 2026**
>
> - 🎮 **O mundo é jogável em 4K, a 24-25 fps depois de estabilizado**, em uma RTX 5090 (três execuções de benchmark com a máquina sem outra carga: 24,9; 25,2 e 24,2 fps). A meta é 30 fps estáveis.
> - ✅ **0 pipelines de GPU recusados** e nenhuma perda do dispositivo de GPU nas execuções de benchmark.
> - ✅ As cutscenes são reproduzidas por inteiro e as primeiras missões já foram jogadas até o fim.
> - ⚡ **Lote 1 integrado**: da inicialização ao primeiro quadro do mundo agora cerca de 31-33 s (antes 41-54 s), e o pior congelamento ao entrar no mundo 1,5-1,9 s (antes 7,6-8,1 s).
> - 🚧 Cerca de **60 outras otimizações de desempenho estão escritas e em teste** (veja [Em teste](#em-teste)).

## Visão geral

| Medida (RTX 5090, 4K) | Agora | Meta |
|---|---|---|
| Taxa de quadros estabilizada | **24-25 fps** | 30+ fps |
| Tempo de quadro mediano | ~40 ms | ≤ 33,3 ms |
| Quadro no percentil 99 | 68-73 ms | sem engasgos |
| Tempo de CPU para preparar um quadro | 25-26 ms | abaixo do tempo da GPU |
| Pipelines de GPU recusados | **0** | 0 |
| Da inicialização ao primeiro quadro do mundo | ~31-33 s | |
| Streaming após entrar no mundo | 52-63 s | poucos segundos |

Medido com `python tools/dev.py bench --quiet --seconds 200`: inicialização rápida a partir de um checkpoint fixo, nada mais rodando na máquina, números lidos do próprio log de quadros do emulador. O tempo de inicialização vem das execuções do lote 1, feitas enquanto outros trabalhos compilavam, então uma máquina ociosa deve se sair um pouco melhor.

## Máquina de teste

| Componente | Modelo |
|---|---|
| GPU | NVIDIA GeForce RTX 5090, 32 GB, driver 621.24 |
| CPU | AMD Ryzen 9 9950X3D, 16 núcleos / 32 threads |
| Memória | 64 GB DDR5-6000 |
| Placa-mãe | MSI PRO B650-P WIFI |
| Armazenamento | SSDs NVMe (Samsung 990 EVO Plus) |
| Sistema operacional | Windows 11 (build 26200) |
| Jogo | GTA VI para Xbox Series, build 1.0.2.1 (sua própria cópia) |

## Para onde vai um quadro

![Tempo de GPU por quadro: passes do jogo 22,8 ms, trabalho do emulador 7,0 ms, não medido 3,8 ms, ociosa 6,2 ms](../images/frame-budget.svg)

Um quadro estabilizado leva cerca de 40 ms. Os passes de renderização do próprio jogo consomem 22,8 ms de tempo de GPU, então 30 fps estão ao alcance. O restante vai para:

- **o nosso próprio trabalho de GPU** (7 ms): verificação de limites em draws indiretos, cópias de texturas entre formatos, verificações do que a GPU escreveu;
- **GPU ociosa** (6-10 ms): a placa espera enquanto uma única thread da CPU prepara o próximo lote de trabalho.

É exatamente isso que o trabalho em teste ataca.

## O que mudou nesta semana

**Corrigido**
- A geada da janela da prisão não é mais desenhada sobre o cabelo da Lucia.
- As "linhas em tudo" sob a luz do sol sumiram: o tradutor de shaders tratava alguns valores como iguais nas duas metades de uma wave de 64 lanes quando não eram (31 shaders).
- As manchas no vidro da lanchonete sumiram: o passe de vidro com ray tracing agora roda como "sem acerto" em vez de ser pulado.
- Áudio: decodificação XMA mais segura e um caminho limpo de silenciamento e encerramento.

**Mais rápido**
- Os envios de texturas saíram das threads de desenho do jogo, as verificações de texturas rodam em paralelo e as listas liberadas são desalocadas em uma thread própria: o pior congelamento ao entrar no mundo caiu de 9,5 s para 6,6 s.
- A alocação de uma textura pelo driver, do lado da CPU, custa 382-529 µs nesta máquina e 5-8 µs a partir de um bloco de memória pré-alocado; texturas e buffers em pools estão em teste.

**Ferramentas**
- Inicialização rápida para execuções de teste: pula o vídeo de abertura e começa a partir de um checkpoint congelado.
- Cada benchmark informa fps, percentis de tempo de quadro, engasgos acima de 100 e 250 ms, tempos de inicialização e de streaming, resets do driver de GPU, e salva capturas de tela.
- Custo de GPU por passe, fases do quadro por thread e um log de engasgos com amostras de pilha.
- O tradutor de shaders e o host de GPU foram divididos em módulos menores; a saída do tradutor permaneceu idêntica byte a byte em todos os 26.148 programas de shader do jogo.

## Problemas conhecidos

- **Falhas visuais que variam de uma execução para outra**: em algumas execuções a blusa da Lucia na lanchonete fica com a textura errada, rostos aparecem pretos ou manchados, ou o cabelo fica laranja na janela da prisão. Correções portadas de outros forks estão em teste.
- **Streaming lento**: depois de entrar no mundo, leva cerca de um minuto para todos os assets chegarem, com pop-in enquanto isso.
- **GPU esperando pela CPU**: 6-10 ms por quadro (veja acima).
- **Iluminação com ray tracing quase toda desligada**: o jogo constrói suas estruturas de ray tracing em software, e esse construtor está desativado.

## Em teste

O trabalho é escrito em paralelo e testado em lotes na única GPU: cada mudança fica atrás de sua própria chave `VICTORY_*`, um lote roda todas as chaves novas ligadas contra todas desligadas, e cada chave é mantida ou descartada com base nos números e nas capturas de tela.

- **Lote 1 (concluído, 20 chaves mantidas)**: staging de texturas sem lock, menos reconstruções de descritores e esperas por locks, baking de personagens em paralelo no carregamento, shaders traduzidos assim que o jogo os cria, carregamento mais rápido das bibliotecas de pipelines, mapeamento de memória mais enxuto e as correções de roupas, rostos e cabelo (a blusa da Lucia na lanchonete saiu certa em 4 de 5 execuções, contra 0 de 2 antes). Descartados por enquanto: os pools de memória para texturas e buffers (deixavam mais lento o desmapeamento de memória) e duas chaves que travavam o jogo; seus autores estão corrigindo.
- **Lote 2 (em execução)**: memória do título em seções de 2 MiB (tempo de mapeamento 55,5 s → 7,3 s no upstream), buffers do jogo mantidos na memória da GPU (−15 % de tempo de quadro no upstream em uma RTX 5090), upscaling DLSS, prioridade mais alta para as threads de quadro do emulador, pré-aquecimento offline de shaders e pipelines, e ray tracing por hardware nos RT cores.

## Como rodar

Você precisa da sua própria cópia do jogo, Windows 10 ou 11, uma GPU com Shader Model 6.6 e mesh shaders, e Rust.

```powershell
tools\lancer_victory.ps1 -GameDir <pasta do jogo>
```

O guia completo (instalação, configurações, solução de problemas) está em [docs/running-gta6.md](../running-gta6.md).

> [!IMPORTANT]
> Este repositório não contém arquivos do jogo, chaves nem executáveis, e não explica como obtê-los.

Para desenvolvimento, `tools/dev.py` reúne as etapas mais comuns:

```powershell
python tools/dev.py build      # build de release
python tools/dev.py gate       # todos os testes, falha com qualquer aviso
python tools/dev.py prewarm    # traduz todos os shaders offline (cerca de 45 s)
python tools/dev.py bench --quiet --seconds 200   # execução medida do jogo
python tools/dev.py train --plan plan.toml        # uma execução de um plano de lote
```

## O projeto em números

| Número | O quê |
|---|---|
| 1600+ testes automatizados | ferramentas em Python e crates em Rust, executados por `dev.py gate` |
| 26.148 programas de shader | traduzidos de RDNA 2 para DXIL, em cache no disco |
| 0 pipelines recusados | na RTX 5090 |

## Histórico

Como o projeto chegou até aqui, das primeiras DLLs do console carregadas em um emulador aos primeiros quadros do mundo: [docs/history.md](../history.md).

## Como contribuir

Issues e pull requests são bem-vindos. Rode `python tools/dev.py gate` antes de abrir um pull request. As regras estão em [CONTRIBUTING.md](../../CONTRIBUTING.md). Discussões e relatos de testes: [Discord](https://discord.gg/TcX42vRyPU).

## Créditos

- **El1xr**: Bone/Nevaa.
- **VIctory**: Marvisak e seus colaboradores ([VIctory](https://github.com/Marvisak/VIctory), GPL-3.0 ou posterior), e os autores dos forks a partir dos quais ele foi montado, entre eles Neptune (VIctory Edit.), de Traffic & zm0kie.
- **AnyXboxX**: Looper_Salty e seus colaboradores ([AnyXboxX](https://github.com/LooperSalty/AnyXboxX)), o projeto do qual o El1xr é um fork.
- **Os forks do VIctory com os quais aprendemos e dos quais portamos código**, entre eles fxceless/victory-fork, Peppacodes, Belobug, stellarr-studio/kelvaroq, uiliannn/VItoriaBr, DoubleGhosty/cool_fox, avatarmanz, 3cvil/M1LK, trustno1-coder, TaynanGT/Vice-Tay, Linox-IDK/LNX-victory e readmet3xt-lab/foxhound.
- Cada componente e autor, mudança por mudança, é creditado em [THIRD_PARTY.md](../../THIRD_PARTY.md).

## Licença

O El1xr é distribuído sob a [GNU GPL 3.0 ou posterior](../../LICENSE). Partes de terceiros mantêm sua própria licença: o módulo `Task` da libHttpClient (MIT, `runtime/third_party/libhttpclient-task`) e as tabelas RDNA 2 geradas a partir dos testes públicos de codificação do LLVM (Apache 2.0 com exceção LLVM, `PrototypeScarlett/emu/rdna2_ops.py` e `rdna2_shapes.py`).
