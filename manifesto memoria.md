# MANIFIESTO DE ARQUITECTURA: MOTOR DE MEMORIA MULTITENANT EN RUST PARA AGENTES DE IA

> **Estatus:** Arquitectura de Grado de Producción (Production-Grade)
>
> **Filosofía:** Latencia Ultrabaja (&lt;10ms), Coste Cero por Token, $0$ Licencias, Consumo Mínimo de RAM (~400 MB) y Resiliencia en Entornos B2B Críticos.

## 1. Declaración de Principios

Los sistemas de memoria para Agentes de IA contemporáneos sufren de sobre-ingeniería: arquitecturas pesadas impulsadas por Python, microservicios redundantes, dependencias masivas en RAM y costos variables impredecibles en APIs de terceros.

Este manifiesto establece un estándar de grado industrial construido sobre **Rust**, diseñado para ejecutar una **Memoria Fría y Caliente Multitenant de Máximo Rendimiento** sobre un único nodo/servidor económico (desde $1$ vCPU / $1$ GB RAM).

Plaintext

```
[ Cliente / App Web / Agente ]
              │
              ▼
    [ Backend SaaS en Rust ] ──► (Inyección forzada de TenantContext)
              │
  ┌───────────┴─────────────────────────────────────────┐
  │ 1. Ingesta + Semantic Chunking (256-300 tokens)     │
  │ 2. Single-Flight Lazy Loading (ModelManager)        │
  │ 3. Generador de Embeddings bajo demanda (fastembed) │
  └───────────┬─────────────────────────────────────────┘
              │
              ▼
   [ LanceDB + Vector & FTS (BM25) Index ] ◄── [ Cron Worker: Compact/Purge ]
              │
              ▼
    [ Hybrid RRF Search (Dense + Sparse) ]
              │
              ▼
[ Contexto Final Limpio para el LLM ]

```

## 2. Los Pilares del Stack Tecnológico

### A. Capa de Datos y Almacenamiento Vectorial

- **Motor:** **LanceDB Open Source (Apache 2.0)**
- **Tipo:** Base de datos vectorial e híbrida embebida en Rust (Disk-First).
- **Persistencia:** Almacenamiento local en disco SSD NVMe o replicación directa a buckets de Object Storage (Amazon S3 / Cloudflare R2).
- **Estrategia Multi-tenancy:** **Tabla Única Consolidada** con aislamiento estricto por software mediante filtrado por escalar (`WHERE tenant_id = 'X' AND agent_id = 'Y'`).

### B. Capa de Recuperación Híbrida (Hybrid Search)

- **Dense Vectors (Semántica):** `MultilingualE5Small` de 384 dimensiones ejecutado de forma nativa vía `fastembed-rs` (pesos en formato ONNX).
- **Sparse Vectors (Búsqueda Exacta):** **FTS con Algoritmo BM25 integrado nativamente en LanceDB**.
- **Embeddings Multimodales (Opcional/Bajo Demanda):** `CLIP-ViT-B-32` para asociar imágenes/pantallazos al flujo de memoria.

### C. Capa de Ingesta y Parsing Ligero

- **Procesamiento de Documentos:** `firecrawl/anydoc` (Parsing de PDF, Word, Excel a Markdown en &lt;5 ms sin OCR pesado ni dependencias de Python).
- **Bases de Código y Contexto Estructurado:** `jyjeanne/okf-rs` (Extracción de grafos de dependencias, sintaxis AST y metadatos con Tree-sitter).

## 3. Especificación de Infraestructura y Consumo de Recursos

### Perfil de Memoria RAM (Enfoque On-Demand / Lazy Loading con Guardias de Concurrencia)

El sistema carga el modelo de embeddings únicamente al momento de la inferencia y libera la memoria en periodos de inactividad:


|                                       |                      |                                 |                               |
| ------------------------------------- | -------------------- | ------------------------------- | ----------------------------- |
| **Componente**                        | **Estado en Reposo** | **Estado en Inferencia Normal** | **Pico en Ingesta / Ráfagas** |
| **Kernel / Servidor Rust**            | ~10 MB               | ~15 MB                          | ~20 MB                        |
| **LanceDB (Motor Embebido)**          | ~30 MB               | ~50 MB                          | ~80 MB                        |
| `MultilingualE5Small` **(Text)**      | 0 MB                 | ~300 MB                         | ~300 MB                       |
| **BM25 / FTS (LanceDB)**              | 0 MB                 | ~10 MB                          | ~20 MB                        |
| `anydoc` **+** `okf-rs` **(Ingesta)** | 0 MB                 | 0 MB                            | ~50 MB *(por hilo)*           |
| **CONSUMO TOTAL RAM**                 | **~40 MB - 50 MB**   | **~375 MB - 395 MB**            | **~450 MB - 470 MB**          |


> **Servidor Aconsejable para Producción:** VPS de **1 vCPU / 1 GB o 2 GB de RAM** ($4 a$6 USD/mes). Capaz de atender miles de peticiones al día para múltiples tenants.

## 4. Pipeline de Procesamiento de Memoria (Paso a Paso)

### Fase 1: Ingesta, Chunking y Vectorización

1. La información entrante (mensaje del usuario, documento PDF vía `anydoc` o código vía `okf-rs`) se convierte a texto/Markdown normalizado.
2. **Segmentación Semántica (Semantic Chunking):** Se fracciona el texto en bloques estrictos de **256 a 300 tokens** con un **overlap del 10-15%** (20-30 tokens) para maximizar la fidelidad del espacio vectorial de 384 dimensiones.
3. El servidor en Rust solicita al `ModelManager` el modelo `MultilingualE5Small` mediante un guard de concurrencia *Single-Flight*.
4. Se genera el vector denso de 384 dimensiones en **~3 ms**.
5. La información se persiste en LanceDB guardando el vector, el texto plano original (*raw text*), los metadatos y el campo clave `tenant_id`.

### Fase 2: Recuperación Híbrida (Hybrid Retrieval)

1. Al realizar una consulta, la pregunta pasa por `MultilingualE5Small` generando el Vector $Q$.
2. LanceDB ejecuta en paralelo dentro de la misma llamada en Rust:
  - **Búsqueda Vectorial:** Similitud Coseno sobre los vectores de 384 dimensiones filtrados por `tenant_id`.
  - **Búsqueda de Texto Completo (FTS):** Coincidencias exactas de términos (BM25) para capturar códigos, IDs, fechas y nombres propios.
3. Se combinan ambos listados de resultados mediante **RRF (Reciprocal Rank Fusion)** para entregar directamente el Top-$K$ (3 a 5 fragmentos) con una latencia total inferior a **10 ms**.

## 5. Mantenimiento, Seguridad y Puntos de Resiliencia en Producción

Para garantizar un funcionamiento continuo sin degradación ni vulnerabilidades, la arquitectura implementa los siguientes controles de grado industrial:

### A. Aislamiento de Datos Cero-Fugas (Tenant Safety Encapsulation)

Para eliminar el riesgo de error humano (olvidar la cláusula `WHERE tenant_id`), el acceso a la base de datos está encapsulado en un struct `TenantVectorStore` en Rust. Es sintácticamente imposible realizar una búsqueda sin instanciar primero el contexto verificado del cliente.

### B. Prevención de Out-Of-Memory en Ráfagas (Single-Flight Lazy Loading)

El patrón *Lazy Loading* utiliza primitives de sincronización (`tokio::sync::OnceCell` / `ArcSwap`). Ante un pico repentino de tráfico tras un periodo de inactividad, **un solo hilo** carga los pesos del modelo ONNX a memoria mientras los demás esperan de forma no bloqueante, evitando duplicaciones de carga en RAM.

### C. Purgado y Compactación Periódica (Garbage Collection)

Al ser un formato columnar inmutable con control de versiones (*Zero-copy versioning*), las eliminaciones y actualizaciones generan registros obsoletos. Un worker secundario en Rust ejecuta tareas programadas periódicas:

1. `table.compact_files()` para unificar archivos fragmentados.
2. `table.cleanup_old_versions()` para purgar el historial y recuperar espacio en disco SSD.
3. `table.create_index()` para regenerar índices IVF-PQ cuando el delta de nuevos datos supere el 15%.

### D. Estrategia Anti-Lockin del Espacio Vectorial

Para mitigar el riesgo de obsolescencia del modelo de embeddings (384 dim), LanceDB almacena de forma obligatoria el texto original (*raw_text*) junto a cada registro. Esto permite migrar o re-indexar la memoria histórica en segundo plano sin interrumpir el servicio ni perder datos de los usuarios.

## 6. Garantías de Inmutabilidad y Privacidad

1. **Determinismo de Embeddings:**

  Todo el almacenamiento en la tabla está normalizado a **384 dimensiones**. No se permiten cambios de modelo en caliente dentro de la misma tabla para evitar corrupción en el espacio vectorial.
2. **Cero Dependencia de Red Externa:**

  Tanto el cálculo de vectores, la búsqueda y la extracción de documentos ocurren **100% dentro del hardware propio**. Ningún dato de cliente viaja a APIs de terceros durante el ciclo de vida de la memoria.

## 7. Firma y Mandato

Este manifiesto dictamina que el sistema de memoria resultante es autosuficiente, extremadamente liviano, blindado contra fallos de concurrencia o degradación de disco, y escalable a miles de usuarios mediante el uso eficiente del almacenamiento y la memoria RAM, e inmune a costes inflados por consumo de tokens en infraestructura auxiliar.

---

## 8. Corrección post-RC: estado real del MVP (T-114)

> **Por qué este bloque existe:** los números de latencia y RAM de las
> secciones §3 y §4 reflejan la **intención de diseño** del manifiesto.
> Los números **medidos** en el MVP son los que siguen y son los que
> cuentan para sizing de producción. Toda desviación respecto a la
> intención está documentada aquí con su fuente medible. Esta sección
> **no reemplaza** el cuerpo del manifiesto; lo anota.

### 8.1 Latencia de búsqueda (medida, REQ-MR-007)

| Métrica | Intención (manifiesto) | MVP medido | Cumple presupuesto |
|---|---|---|---|
| Search p50 (warm, 100 k chunks) | < 10 ms | **< 20 ms** | ✅ dentro de REQ-MR-007 |
| Search p99 (warm, 100 k chunks) | < 10 ms | **< 100 ms** | ✅ dentro de REQ-MR-007 |
| Search p95 sostenido | no documentado | **< 50 ms** | nuevo gate interno |
| Cold start (store + first search) | no documentado | **< 3 s** | reportado, no gateado |

**Evidencia reproducible:** `scripts/bench.sh` corre los benches de
search / ingest / embed / code-index y termina con un *gate report*
que falla al instante si una métrica sale del presupuesto
(REQ-OP-002). El modo `--quick` baja los tamaños para CI; `--embed`
fuerza la corrida del bench de embeddings.

### 8.2 Tiempos de indexado de código (medidos, REQ-CK-002)

| Tamaño del repo (LOC, Rust + Python) | MVP medido |
|---|---|
| 10 000 | **< 2 s** cold (REQ-CK-002) |
| 100 000 | **10-30 s** cold (REQ-CK-002) |
| 500 000 | **1-3 min** cold |
| 1 000 000 | **5-10 min** cold |

### 8.3 RAM (medida, no aspiracional)

El perfil medido difiere del original porque el runtime incluye no
solo `MultilingualE5Small` (≈ 300 MB) sino también `ort` + caches del
tokenizer + `LanceDB` working set + allocator del batch ingest:

| Estado | Intención (manifiesto) | MVP medido |
|---|---|---|
| Reposo (sin modelo cargado) | ~40-50 MB | **~150-200 MB** (kernel Rust + LanceDB + tokio + tokio-cron + deps) |
| Inferencia normal (E5 cargado) | ~375-395 MB | **~800 MB - 1 GB** (ort + E5 + tokenizer caches) |
| Pico de ingesta (batch + parse) | ~450-470 MB | **~1.5 GB** (ort allocator + batch chunks + anydoc/okf si activos) |

### 8.4 Servidor mínimo para producción

| Configuración | Intención (manifiesto) | MVP recomendado |
|---|---|---|
| Mínimo viable | 1 vCPU / 1 GB | **2 vCPU / 2 GB RAM / 20 GB SSD** |
| Costo mensual (VPS genérico) | $4-6 USD | **$8-12 USD** |

El `1 vCPU / 1 GB` original **sí arranca** el binario, pero la primera
búsqueda híbrida carga el modelo E5 y empuja la RAM cerca del límite;
en `2 GB` hay holgura suficiente para que el worker corra junto al MCP.

### 8.5 Caveat de primer arranque (REQ-OP-005 / riesgo R1)

La primera vez que cualquier proceso intenta ingestar con embeddings,
`MultilingualE5Small` (~500 MB de ONNX) se descarga a
`~/.memento/models/`. En entornos sin red o en CI:

```bash
memento --no-embeddings memory search --query "..."   # FTS-only
```

En ese modo la búsqueda es FTS-únicamente y la búsqueda híbrida (RRF)
devuelve `INVALID_INPUT` con un mensaje bilingüe estructurado
(REQ-MR-003). El CLI **degrada** sin anydoc (sigue funcionando con
`ingest_text` y fallback md/txt); el servidor MCP **falla duro**
porque no puede servir documentos en absoluto. Esta divergencia es
intencional y está documentada en
[docs/install.{es,en}.md](../docs/install.es.md).

### 8.6 Cómo verificar los números

```bash
# Modo rápido (CI, ~minutos)
scripts/bench.sh --quick

# Modo referencia (100k chunks, 10k + 100k LOC; minutos)
scripts/bench.sh

# Modo completo con embeddings (incluye descarga E5 ~500 MB)
scripts/bench.sh --embed
```

Cualquier desviación se imprime con su valor medido (REQ-OP-002); no se
acepta en silencio.
