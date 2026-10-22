# Мультиязычные cross-encoder реранкеры для CPU-only Linux x86_64: проверяемые факты

Все данные — из HF API (`?full=true&blobs=true`), `config.json`, README карточек и arXiv, сняты 2026-10-18.
Размеры: MB = 10⁶ байт (как в HF API `siblings[].size`).
Пометки: **[замер]** — измерено мной на этом хосте; **[оценка]** — экстраполяция; **[не проверено]** — источник не найден или не подтверждает.

## 1. Сводка: параметры, архитектура, лицензия, контекст

| Модель | Параметры (safetensors) | Архитектура / слоёв / hidden | Лицензия | Max tokens (заявлено) | cross-encoder |
|---|---|---|---|---|---|
| BAAI/bge-reranker-v2-m3 | 567 755 777 (F32) | XLMRobertaForSequenceClassification / 24 / 1024 | apache-2.0 | 8192 (`max_position_embeddings` 8194; пример в карточке режет 512) | да, 1 логит |
| jinaai/jina-reranker-v2-base-multilingual | 278 437 633 (BF16) | XLMRobertaForSequenceClassification / 12 / 768 | **cc-by-nc-4.0** | 1024, длиннее — sliding window | да, 1 логит |
| mixedbread-ai/mxbai-rerank-base-v2 | 494 032 768 (F16) | Qwen2ForCausalLM / 24 / 896 | apache-2.0 | 8k заявлено в блоге, 32k «compatible»; `max_position_embeddings` 32768 | да, по логитам yes/no |
| mixedbread-ai/mxbai-rerank-large-v2 | 1 543 714 304 (F16) | Qwen2ForCausalLM / 28 / 1536 | apache-2.0 | то же | да |
| Qwen/Qwen3-Reranker-0.6B | 595 776 512 (BF16) | Qwen3ForCausalLM / 28 / 1024 | apache-2.0 | 32k (`max_position_embeddings` 40960) | да, логиты yes/no |
| Alibaba-NLP/gte-multilingual-reranker-base | 305 959 681 (F16) | NewForSequenceClassification (`trust_remote_code`) / 12 / 768 | apache-2.0 | 8192 | да, 1 логит |
| BAAI/bge-m3 | **[не проверено]**: safetensors нет, `pytorch_model.bin` 2271.15 MB fp32 → ~568M косвенно | XLMRobertaModel / 24 / 1024 | **mit** | 8192 | **нет** — dense + sparse + ColBERT late-interaction |
| Horizon-Labs/multilingual-reranker-base (2026-10) | 307 531 009 (F32) | ModernBertForSequenceClassification (`jhu-clsp/mmBERT-base`) | apache-2.0 | 384 токена на пару (обучение) | да |

Источники по строкам (все проверяемы):
- bge-reranker-v2-m3: https://huggingface.co/api/models/BAAI/bge-reranker-v2-m3?full=true&blobs=true · https://huggingface.co/BAAI/bge-reranker-v2-m3/raw/main/config.json · карточка: «Lightweight reranker model, possesses strong multilingual capabilities» https://huggingface.co/BAAI/bge-reranker-v2-m3
- jina-reranker-v2: https://huggingface.co/api/models/jinaai/jina-reranker-v2-base-multilingual?full=true&blobs=true · config: `max_position_embeddings: 1026` · «context length of up to `1024` tokens… uses a sliding window approach» https://huggingface.co/jinaai/jina-reranker-v2-base-multilingual/raw/main/README.md
- mxbai base/large v2: https://huggingface.co/api/models/mixedbread-ai/mxbai-rerank-base-v2?full=true&blobs=true · https://huggingface.co/api/models/mixedbread-ai/mxbai-rerank-large-v2?full=true&blobs=true · «support long contexts up to 8k tokens (32k-compatible)» https://mixedbread.com/blog/mxbai-rerank-v2
- Qwen3-Reranker-0.6B: https://huggingface.co/api/models/Qwen/Qwen3-Reranker-0.6B?full=true&blobs=true · «Number of Paramaters: 0.6B», «Context Length: 32k», таблица «Sequence Length 32K», «Layers 28» https://huggingface.co/Qwen/Qwen3-Reranker-0.6B/raw/main/README.md
- gte-multilingual-reranker-base: https://huggingface.co/api/models/Alibaba-NLP/gte-multilingual-reranker-base?full=true&blobs=true · «Model Size: 306M», «Max Input Tokens: 8192» https://huggingface.co/Alibaba-NLP/gte-multilingual-reranker-base/raw/main/README.md
- bge-m3: https://huggingface.co/api/models/BAAI/bge-m3?full=true&blobs=true · «spanning from short sentences to long documents of up to 8192 tokens», «dense retrieval, multi-vector retrieval, and sparse retrieval» https://huggingface.co/BAAI/bge-m3/raw/main/README.md
- Horizon-Labs: https://huggingface.co/api/models/Horizon-Labs/multilingual-reranker-base?full=true&blobs=true · «Max length: trained with 384 tokens per pair» https://huggingface.co/Horizon-Labs/multilingual-reranker-base

## 2. ONNX: что есть в официальных репозиториях, а что — стороннее

| Модель | ONNX в официальном репо | Сторонние ONNX-репо (размеры, MB) |
|---|---|---|
| bge-reranker-v2-m3 | **нет** (0 файлов `*.onnx`) | onnx-community/bge-reranker-v2-m3-ONNX: `model.onnx` 0.7 + `model.onnx_data` 2271.1 (fp32), `model_fp16` 1136.2, `model_int8`/`quantized`/`uint8` **570.7**, `model_q4` 1252.5, `model_q4f16` 702.1, `model_bnb4` 1233.7. Также tss-deposium/...-onnx-int8 (569.7, лицензия mit), keisuke-miyako/...-int8 (569.7) |
| jina-reranker-v2-base-multilingual | **да**, в официальном репо: `onnx/model.onnx` 1114.0, `model_fp16` 557.2, `model_int8`/`quantized`/`uint8` **279.6**, `model_q4` 827.4, `model_bnb4` 822.1 | сторонние не нужны; Hamdy005/...-int8-onnx отдаёт 404 **[не проверено]** |
| mxbai-rerank-base-v2 | **нет** | keisuke-miyako/mxbai-rerank-base-v2-onnx-int8: 495.7; ...-onnx-f16: `model.onnx` 1977.1 (видимо fp32-экспорт); tss-deposium/...-onnx-fp16 |
| mxbai-rerank-large-v2 | **нет** | mradermacher/...-GGUF и -i1-GGUF (только GGUF, ONNX нет) |
| Qwen3-Reranker-0.6B | **нет** | onnx-community/Qwen3-Reranker-0.6B-ONNX: только `model_quantized` 1219.3 и `model_q4` 995.2; thomasht86/...-int8-ONNX: `model.onnx` 598.2; keisuke-miyako/...-onnx-int8: 600.7 |
| gte-multilingual-reranker-base | **нет** | onnx-community/gte-multilingual-reranker-base: `model.onnx` fp32 1257.8, `model_fp16` 629.1, `model_int8`/`quantized`/`uint8` **340.9**, `model_q4` 875.6, `model_q4f16` 466.3, `model_bnb4` 868.5; keisuke-miyako/...-onnx-int8 339.8 |
| bge-m3 | **да**, fp32: `onnx/model.onnx` 0.72 + `onnx/model.onnx_data` 2266.82; квантованных нет | gpahal/bge-m3-onnx-int8: 570.0 (лицензия mit) |
| Horizon-Labs/multilingual-reranker-base | **да**: `onnx/model.onnx` 1230.7, `onnx/model_quantized.onnx` **641.1** (+`quantization_check.json`) | — |

URL: https://huggingface.co/api/models/onnx-community/bge-reranker-v2-m3-ONNX?full=true&blobs=true ·
https://huggingface.co/api/models/onnx-community/gte-multilingual-reranker-base?full=true&blobs=true ·
https://huggingface.co/api/models/onnx-community/Qwen3-Reranker-0.6B-ONNX?full=true&blobs=true ·
https://huggingface.co/api/models/keisuke-miyako/mxbai-rerank-base-v2-onnx-int8?full=true&blobs=true ·
https://huggingface.co/api/models/gpahal/bge-m3-onnx-int8?full=true&blobs=true ·
https://huggingface.co/api/models/jinaai/jina-reranker-v2-base-multilingual?full=true&blobs=true

Замечания:
- int8 у bge-reranker-v2-m3 (570.7 MB) **больше**, чем int8 у gte (340.9 MB) — при 568M против 306M параметров доля сжатия разная (у bge часть весов эмбеддингов/классификатора не квантуется одинаково); детали квантизации в репо не описаны — **[не проверено]**.
- У onnx-community/Qwen3-Reranker-0.6B-ONNX «quantized» = 1219.3 MB — это не int8 (int8-версия того же автора отсутствует); int8-варианты у сторонних (598–601 MB).
- Все сторонние ONNX — автоматические конверсии, авторами моделей не заверены; лицензия у onnx-community-репо в тегах **не указана**.

## 3. Качество: цифры и цитаты

### 3.1 Единая таблица (одни и те же прогоны, top-100 от Qwen3-Embedding-0.6B)
Qwen/Qwen3-Reranker-0.6B README, раздел «## Evaluation»:

| Model | Param | MTEB-R | CMTEB-R | MMTEB-R | MLDR | MTEB-Code | FollowIR |
|---|---|---|---|---|---|---|---|
| Jina-multilingual-reranker-v2-base | 0.3B | 58.22 | 63.37 | 63.73 | 39.66 | 58.98 | -0.68 |
| gte-multilingual-reranker-base | 0.3B | 59.51 | 74.08 | 59.44 | 66.33 | 54.18 | -1.64 |
| BGE-reranker-v2-m3 | 0.6B | 57.03 | 72.16 | 58.36 | 59.51 | 41.38 | -0.01 |
| **Qwen3-Reranker-0.6B** | 0.6B | 65.80 | 71.31 | 66.36 | 67.28 | 73.42 | 5.41 |
| Qwen3-Reranker-4B | 4B | 69.76 | 75.94 | 72.74 | 69.97 | 81.20 | 14.84 |

Цитата: «All scores are our runs based on the top-100 candidates retrieved by dense embedding model Qwen3-Embedding-0.6B.»
https://huggingface.co/Qwen/Qwen3-Reranker-0.6B/raw/main/README.md · препринт: https://arxiv.org/html/2506.05176v3

### 3.2 gte-multilingual-reranker-base — таблица авторов (mGTE, Table 5)
Та же таблица в карточке — **только картинкой** `images/mgte-reranker.png`; текстом — в препринте.
Метрика: Avg / MLDR nDCG@10 (13 языков) / MIRACL nDCG@10 (18) / MKQA recall@20 (25) / BEIR nDCG@10 (1):

| Model | Params | Seq | Avg | MLDR | MIRACL | MKQA | BEIR |
|---|---|---|---|---|---|---|---|
| jina-reranker-v2-multilingual | 278M | 8192 | 59.4 | 53.2 | 65.8 | 68.8 | 49.7 |
| bge-reranker-v2-m3 | 568M | 8192 | 65.7 | 66.8 | 72.6 | 68.7 | 54.6 |
| gte-multilingual-reranker-base | 304M | 8192 | **67.4** | **78.7** | 68.5 | 67.2 | **55.4** |

«Table 5: Results of reranking based on the candidates retrieved by our TRM dense model» — https://arxiv.org/html/2407.19669v2 · https://arxiv.org/abs/2407.19669

### 3.3 jina-reranker-v2-base-multilingual — карточка
«| jina-reranker-v2-multilingual | 278M | MKQA(nDCG@10, 26 langs) **54.83** | BEIR(nDCG@10, 17 datasets) **53.17** | MLDR(recall@10, 13 langs) **68.95** | CodeSearchNet (MRR@10) 71.36 | AirBench 61.33 | ToolBench 77.75 | TableSearch 93.31 |»
Строка bge-reranker-v2-m3 в той же таблице: MKQA 54.17, BEIR 53.65, MLDR 59.73.
https://huggingface.co/jinaai/jina-reranker-v2-base-multilingual/raw/main/README.md

### 3.4 mxbai-rerank-v2 — карточка и блог
«| Model | BEIR Avg | Multilingual | Chinese | Code Search | Latency (s) |»
- mxbai-rerank-large-v2: 57.49 / 29.79 / 84.16 / 32.05 / 0.89
- mxbai-rerank-base-v2: 55.57 / 28.56 / 83.70 / 31.73 / 0.67
- mxbai-rerank-large-v1: 49.32 / 21.88 / 72.53 / 30.72 / 2.24

«*Latency measured on A100 GPU» — https://huggingface.co/mixedbread-ai/mxbai-rerank-base-v2/raw/main/README.md · https://mixedbread.com/blog/mxbai-rerank-v2 · препринт ProRank: https://arxiv.org/abs/2506.03487
Методика метрик «Multilingual/Chinese» в карточке не расшифрована — **[не проверено]**.

### 3.5 bge-m3 (не cross-encoder, а dense+sparse+ColBERT)
Карточка даёт цифры только картинками; значения ниже — с картинок репозитория:
- MIRACL dev, nDCG@10, Avg: Dense 69.2 · Sparse 53.9 · Multi-vec 70.5 · Dense+Sparse 70.4 · **All 71.5** — https://huggingface.co/BAAI/bge-m3/blob/main/imgs/miracl.jpg (препринт, Table 1: https://arxiv.org/html/2402.03216v5)
- MKQA, Recall@100, Avg: Dense 75.1 · Sparse 45.3 · Multi-vec 75.3 · **All 75.5** — https://huggingface.co/BAAI/bge-m3/blob/main/imgs/mkqa.jpg
- MLDR test, nDCG@10, Avg: Dense 52.5 · Sparse 62.2 · Multi-vec 57.6 · Dense+Sparse 64.8 · **All 65.0** — https://huggingface.co/BAAI/bge-m3/blob/main/imgs/long.jpg
- Препринт подтверждает: «multi-vector retrieval is also impressive, which brings 5.1+ points improvement over M3 (Dense)… combination of different retrieval methods leads to a remarkable average performance of 65.0» (MLDR) — https://arxiv.org/html/2402.03216v5

### 3.6 Новое 2025–2026 (кратко, с цитатами)
| Модель | Параметры | Лицензия | ONNX | Заявленное качество (цитата) |
|---|---|---|---|---|
| jinaai/jina-reranker-v3 | 596 836 352 BF16 | **cc-by-nc-4.0** | нет | «BEIR 61.94, MIRACL 66.83, MKQA 67.92, CoIR 70.64 (nDCG@10)»; 131K контекст, до 64 документов за проход — https://huggingface.co/jinaai/jina-reranker-v3 |
| jinaai/jina-reranker-v3.5 | 596 836 352 BF16 | **cc-by-nc-4.0** | нет (есть GGUF) | «BEIR 63.20, MIRACL 74.11, RTEB 70.95»; «1.22×–1.56× faster listwise inference… (A100, FlashAttention-2)» — https://huggingface.co/jinaai/jina-reranker-v3.5 |
| zeroentropy/zerank-2-reranker | 4 022 468 096 BF16 | apache-2.0 | нет | NDCG@10: Web 0.6346, Conversational 0.6140, STEM 0.6521, Code 0.6528, Legal 0.6644; мультиязычность явно не заявлена — https://huggingface.co/zeroentropy/zerank-2-reranker |
| Horizon-Labs/multilingual-reranker-base | 307 531 009 F32 | apache-2.0 | **да** fp32 1230.7 / int8 641.1 | nDCG@10 mean: **0.855** (MIRACL 0.800 / Wiki 0.967 / other 0.798); bge-reranker-v2-m3 0.866, Qwen3-Reranker-4B 0.875 — https://huggingface.co/Horizon-Labs/multilingual-reranker-base |
| nvidia/Nemotron-3.5-Rerank-8B-BF16 | 8 381 159 424 BF16 | other (OpenMDW-1.1) | нет | таблиц со значениями в карточке нет; «max sequence length is 32768 tokens»; требует CUDA — https://huggingface.co/nvidia/Nemotron-3.5-Rerank-8B-BF16 |

mMARCO nDCG@10: ни в одной из проверенных карточек/препринтов цифры **не найдены** — **[не проверено]**.

## 4. Скорость на CPU: что есть в источниках и что измерено

### 4.1 Из README/бенчмарков (все — GPU)
- mxbai v2: 0.67 s (base) / 0.89 s (large) на запрос, NFCorpus, **A100 80GB** — https://mixedbread.com/blog/mxbai-rerank-v2
- ProRank (препринт mxbai): «Model | Params | Load (s) | Avg. (ms): bge-v2-gemma 2B … 64.0; rankllama-7b 49.7; ProRank-0.5B 0.5B 1.29 **27.2**; ProRank-1.5B **32.7**», Table 6 — https://arxiv.org/html/2506.03487v3 (железо в таблице не указано — **[не проверено]**)
- bge-reranker-v2-m3, jina-reranker-v2, Qwen3-Reranker-0.6B, gte, bge-m3: **замеров на CPU в карточках нет** (слово «cpu» в README gte встречается только в docker-команде TEI). У gte есть только безцифровое заявление: «offering a 10x increase in inference speed» (база — decode-only LLM-реранкер; железо не указано).
- jina-reranker-v3.5: «1.22×–1.56× faster listwise inference… (A100, FlashAttention-2)».

Итого: **ни одного проверяемого CPU-замера в документации нет** — все цифры латентности GPU-шные.

### 4.2 [замер] собственный, этот хост
Железо (из `/proc/cpuinfo`, `nproc`): Intel Xeon Skylake, **2 vCPU**, 3 GB RAM, Linux x86_64, onnxruntime 1.31.0, `intra_op_num_threads=2`, CPUExecutionProvider.
Методика: int8-ONNX сторонних репо, вход — **200 случайных токенов** (не реальный текст), batch=1, 5 прогонов после прогрева, среднее.

| Модель (int8 ONNX) | файл | ms/пара (200 ток., 2 vCPU) | 30 пар, 2 vCPU |
|---|---|---|---|
| gte-multilingual-reranker-base (keisuke-miyako) | 339.8 MB | **1569** | **47.1 s** |
| bge-reranker-v2-m3 (keisuke-miyako) | 569.7 MB | **3586** | **107.6 s** (batch=30: 107.6 s) |
| Qwen3-Reranker-0.6B (keisuke-miyako) | 600.7 MB | **3927** | **117.8 s** |

Контроль: 1 vs 2 потока на gte — 1569 vs 1579 ms/пара, т.е. на 2 vCPU масштабирования нет.
Оговорки: случайные токены дают «худший» для attention путь, чем реальный русский текст; квантование стороннее, не авторское; одиночные прогоны на общем хосте, ±10–20% воспроизводимость **[не проверено]**.

### 4.3 [оценка] время на 30 пар × 200 токенов на 8-ядерном CPU
Основание: замеры 4.2 на 2 vCPU и отсутствие масштабирования 1→2 потоков. Сценарии:

| Модель (int8) | Линейный скейл 2→8 ядер | Реалистичный скейл (2.5–3× на 8 ядер) |
|---|---|---|
| gte-multilingual-reranker-base | 47.1 s ÷ 4 ≈ **12 s** | ~16–19 s |
| bge-reranker-v2-m3 | 107.6 s ÷ 4 ≈ **27 s** | ~36–43 s |
| Qwen3-Reranker-0.6B | 117.8 s ÷ 4 ≈ **29 s** | ~39–47 s |

Это **[оценка]**, не измерение: источника с 8-ядерными CPU-замерами этих моделей не найдено; линейный скейл int8-ядер onnxruntime на 8 ядрах завышает ожидание (упирается в память/AVX-VNNI).
Для сравнения порядков: fp32-варианты (в 3–4 раза больше по весам и без int8-ядер) на CPU будут медленнее; цифр из источников нет — **[не проверено]**.

## 5. arXiv / препринты

| Модель | arXiv | Название |
|---|---|---|
| bge-reranker-v2-m3 (BGE) | 2312.15503 (тег карточки), 2402.03216 | C-Pack: Packed Resources For General Chinese Embeddings; M3-Embedding |
| bge-m3 | 2402.03216 | M3-Embedding: Multi-Linguality, Multi-Functionality, Multi-Granularity… |
| gte-multilingual-reranker-base | 2407.19669 | mGTE: Generalized Long-Context Text Representation and Reranking Models… (EMNLP 2024 Industry) |
| Qwen3-Reranker-0.6B | 2506.05176 | Qwen3 Embedding: Advancing Text Embedding and Reranking Through Foundation Models |
| mxbai-rerank-v2 | 2506.03487 | ProRank: Prompt Warmup via Reinforcement Learning for Small Language Models Reranking |
| jina-reranker-v2 | — в карточке arXiv нет **[не проверено]** | |
| jina-reranker-v3/3.5 | — в карточке arXiv нет **[не проверено]** | |

Проверка номеров: https://export.arxiv.org/api/query?id_list=2402.03216,2506.05176,2407.19669,2506.03487 (id и заголовки совпали).

## 6. Сводка «не проверено»
1. Точное число параметров bge-m3 (safetensors нет; ~568M — из размера fp32-бинаря).
2. Любые CPU-замеры этих моделей из официальных источников — не найдены.
3. mMARCO nDCG@10 — ни у одной модели цифры не найдены.
4. Лицензии сторонних ONNX-репозиториев (onnx-community — лицензия не указана; keisuke-miyako — не указана) и корректность их квантования.
5. Числовые бенчмарки nvidia/Nemotron-3.5-Rerank-8B-BF16 и мультиязычность zerank-2 — карточка не даёт подтверждения.
6. Методика метрик «Multilingual / Chinese» в карточке mxbai v2.
7. Воспроизводимость моих замеров 4.2 (случайные токены, общий хост, сторонние int8-веса).
