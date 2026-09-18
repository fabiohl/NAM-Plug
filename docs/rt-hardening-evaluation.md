<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Avaliação Arquitetural Formal — `rt-hardening` para Plugins CLAP (NAM-Plug)

**Documento:** `NAM-Plug/docs/rt-hardening-evaluation.md`  
**Sprint:** S6 — Epic E (Migração do NAM-Plug para APIs generalizadas do engine)  
**Tarefa:** S6-T5 / E.5 / R5 / D.4  
**Subprojeto:** `NAM-Plug` (`GPL-3.0-or-later`)  
**Data:** 2026-09-18  
**Status:** Aprovado — decisão formal documentada.

---

## 1. Contexto e Objetivo

O `NeuralAmpModeler-rs` expõe, desde S4-T4, o módulo `rt-hardening` (`src/rt_hardening/`), uma feature opt-in (`Cargo.toml`: `rt-hardening = []`) que fornece utilitários de hardening de tempo real para hosts Linux:

- `disable_thp()`, `mlockall_current()` — controle de Transparent Huge Pages e bloqueio de memória (`thread.rs`).
- `promote_sched_fifo()`, `set_cpu_affinity()` — escalonamento `SCHED_FIFO` e afinidade de CPU (`thread.rs`, `affinity.rs`).
- `request_cpu_dma_latency()` — guarda `PM-QoS` via `/dev/cpu_dma_latency` (`pm_qos.rs`).
- `set_ftz_daz()` — configuração local de FTZ/DAZ (`thread.rs`).

A questão arquitetural central (residual E.5, vinculada a R5 e D.4) é se `NAM-Plug` — um plugin CLAP que executa dentro do processo da DAW hospedeira — deve consumir ou ativar esse módulo.

**Veredito formal:** `NAM-Plug` **não consome** `rt-hardening`. A responsabilidade de controle de realtime é delegada integralmente ao host (DAW / `NAM-Audio-Pipe`), conforme a separação de fronteiras entre plugin e host na especificação CLAP.

---

## 2. Análise por Fronteira de Responsabilidade

### 2.1 Isolamento de Espaço de Endereçamento (`mlockall`)

- **Módulo do engine:** `mlockall_current()` chama `mlockall(MCL_CURRENT | MCL_FUTURE)`.
- **Problema no plugin CLAP:** `NAM-Plug` é compilado como `cdylib` (`crate-type = ["cdylib", "rlib"]` em `Cargo.toml`). Chamar `mlockall` a partir de uma biblioteca compartilhada carregada no processo da DAW travaria páginas de memória de **todos** os outros plugins, instrumentos de amostras e processos internos do host.
- **Consequência direta:** Em hosts com instrumentos de amostras grandes (`Kontakt` com 32 GB de RAM), `mlockall` provocaria `ENOMEM` ou falha catastrófica de paginação no sistema operacional, afetando todos os plugins — não apenas o `NAM-Plug`.
- **Decisão:** Não ativar. A responsabilidade de `mlockall` pertence ao host (DAW ou `NAM-Audio-Pipe`), que controla o processo inteiro.

### 2.2 Prioridade e Escalonamento de Threads (`sched_setscheduler`)

- **Módulo do engine:** `promote_sched_fifo()` chama `sched_setscheduler(SCHED_FIFO)`.
- **Problema no plugin CLAP:** A thread de processamento de áudio é criada e supervisionada pela DAW (`clap_plugin_audio_ports_activation`, `process()` callback). A especificação CLAP não concede ao plugin autoridade sobre a política de escalonamento do processo.
- **Riscos:**
  1. Falha por ausência de permissão `CAP_SYS_NICE` no processo do usuário.
  2. Perturbação do balanceamento multicore da DAW — um plugin que eleva a própria thread para `SCHED_FIFO` pode privar outras pistas de CPU, causando xruns globais no host.
  3. Violação da especificação CLAP (não há extensão CLAP para alteração de política de scheduler pelo plugin).
- **Decisão:** Não ativar. A thread RT é criada pelo host; se o host desejar `SCHED_FIFO`, ele aplica diretamente (`NAM-Audio-Pipe` já consome `rt-hardening` para isso, ver `NAM-Audio-Pipe/Cargo.toml`).

### 2.3 Afinidade de CPU (`pthread_setaffinity_np`)

- **Módulo do engine:** `set_cpu_affinity()` e `select_optimal_cpu()` fixam threads a núcleos específicos com base em `/proc/irq/`.
- **Problema no plugin CLAP:** Fixar a thread de áudio do plugin a um núcleo específico interfere no grafo de renderização de pistas do host. A DAW já gerencia afinidade de pista (thread pool por track, balanceamento de carga entre núcleos). Um plugin que impõe afinidade própria quebra esse balanceamento, podendo concentrar carga em um núcleo enquanto outros permanecem ociosos.
- **Decisão:** Não ativar. A afinidade é responsabilidade do host; o plugin apenas consome o buffer fornecido pelo host (`process()`), sem impor restrições de topologia.

### 2.4 PM-QoS (`/dev/cpu_dma_latency`)

- **Módulo do engine:** `request_cpu_dma_latency()` abre e guarda `/dev/cpu_dma_latency` com `PmQosGuard` (`Drop` libera o handle).
- **Problema no plugin CLAP:** `PM-QoS` (`cpu_dma_latency`) é um recurso global do sistema operacional (`/dev/cpu_dma_latency`). Se múltiplos plugins abrirem e solicitarem valores conflitantes de latência, o último a escrever vence — criando uma corrida de configuração entre plugins no mesmo host. Além disso, o arquivo requer permissões de sistema que podem não existir no contexto do usuário da DAW.
- **Decisão:** Não ativar. `NAM-Audio-Pipe` (host independente) consome `rt-hardening` e, portanto, gerencia `PM-QoS` em nível de sistema. `NAM-Plug` não deve competir por esse recurso global.

---

## 3. Práticas RT Aplicáveis Mantidas pelo Plugin

Embora `NAM-Plug` **não ative** `rt-hardening`, ele mantém — de forma independente e sem dependência do módulo — as práticas de tempo real aplicáveis ao escopo de um plugin CLAP:

| Prática | Implementação no `NAM-Plug` | Referência no código |
|---|---|---|
| **FTZ / DAZ local** (`_MM_SET_FLUSH_ZERO_MODE`) | Ativado na primeira execução do `process()` (`Subnormal & Denormal Setup`), via registros SSE locais — não altera o estado do processo. | `src/clap/processor/mod.rs` (§6.1) |
| **Zero alocações na thread de áudio** | Verificado por `heap-audit` (`CountingAllocator`); todos os buffers são pré-alocados em `activate()` e reutilizados no hot-path. | `docs/architecture.md` §5.2, `tests/` |
| **Zero bloqueios de I/O** | Nenhuma chamada de sistema bloqueante (`open`, `read`, `write`) no `process()`; comunicação com host via SPSC lock-free (`rtrb`) e atomics. | `docs/architecture.md` §5 |
| **Zero mutexes no hot-path** | Nenhum `std::sync::Mutex` ou `RwLock` no caminho de áudio; sincronização via `Arc<AtomicBool>` (`alive_fence`) e canais `rtrb`. | `docs/architecture.md` §5.1 |
| **Telemetria RT** | `RtStatusFlags` publica `rt_affinity_err`, `rt_sched_err`, `rt_target_cpu`, `rt_cpu`, `rt_tid`, `rt_priority` — sem depender de `rt-hardening`, apenas refletindo o que o host define. | `NeuralAmpModeler-rs/src/common/spsc/status.rs` |

Essas práticas são suficientes para garantir o contrato RT do plugin dentro do escopo CLAP, sem invadir a fronteira de responsabilidade do host.

---

## 4. Decisão Arquitetural Formal

> **`NAM-Plug` não consome `rt-hardening`.** A responsabilidade de controle de tempo real (`SCHED_FIFO`, `mlockall`, afinidade de CPU, `PM-QoS`) é delegada integralmente ao host da DAW (`Bitwig Studio`, `REAPER`, etc.) e ao subprojeto `NAM-Audio-Pipe` (host independente que já consome `rt-hardening`, conforme `NAM-Audio-Pipe/Cargo.toml`).

### 4.1 Justificativa Resumida

1. **Isolamento de processo:** Plugin `cdylib` não pode alterar políticas de processo global (`mlockall`, `SCHED_FIFO`) sem afetar todos os outros plugins no host.
2. **Especificação CLAP:** Não existe extensão CLAP que autorize o plugin a modificar scheduler, afinidade ou `PM-QoS` do host.
3. **Separação de responsabilidades:** `rt-hardening` é projetado para **hosts** (`NAM-Audio-Pipe`); o plugin (`NAM-Plug`) consome a API pública do engine (`NeuralAmpModeler-rs`) sem ativar a feature.
4. **Manutenção independente:** `NAM-Plug` mantém FTZ/DAZ local, zero-alloc e telemetria `RtStatusFlags` — práticas suficientes para o contrato RT sem dependência do módulo.

---

## 5. Impacto e Referências

### 5.1 Arquivos Referenciados

- `NeuralAmpModeler-rs/src/rt_hardening/mod.rs` — documentação do módulo (`opt-in`, `Result`-only, off-RT exclusivamente).
- `NeuralAmpModeler-rs/src/rt_hardening/thread.rs`, `affinity.rs`, `pm_qos.rs` — implementações de `disable_thp`, `mlockall_current`, `promote_sched_fifo`, `set_cpu_affinity`, `request_cpu_dma_latency`.
- `NAM-Plug/Cargo.toml` — `crate-type = ["cdylib", "rlib"]`; `features` (`dual-mono`, `testing`, `heap-audit`); nenhuma referência a `rt-hardening` (correto por design).
- `NAM-Audio-Pipe/Cargo.toml` — consome `rt-hardening` (`features = ["rt-hardening"]`), confirmando a separação host/plugin.

### 5.2 Atualizações de Documentação Requeridas

- Este documento (`NAM-Plug/docs/rt-hardening-evaluation.md`) — criado e aprovado.
- `NAM-Plug/README.md` — adicionada referência a este documento (§5.4, nota arquitetural).
- `NAM-Plug/Cargo.toml` — adicionado comentário explícito sobre a não-ativação de `rt-hardening` e a separação de responsabilidades host/plugin.

---

## 6. Conclusão

A avaliação arquitetural formal confirma que `NAM-Plug` **não deve** e **não ativa** a feature `rt-hardening` do `NeuralAmpModeler-rs`. A decisão preserva o isolamento de processo, respeita a especificação CLAP, evita interferência no balanceamento do host e mantém a separação clara de responsabilidades: o plugin executa DSP neural com contrato RT estrito (zero-alloc, zero-lock, FTZ/DAZ local, telemetria); o host (`NAM-Audio-Pipe` ou a DAW) gerencia `mlockall`, `SCHED_FIFO`, afinidade e `PM-QoS`.

**Status:** S6-T5 concluída. Residual E.5 e D.4 formalmente fechados.
