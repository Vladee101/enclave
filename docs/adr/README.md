# Architecture Decision Records

Здесь записаны значимые архитектурные решения по Enclave — десктопному
приложению корпоративного RAG + LoRA. Формат — [MADR](https://adr.github.io/madr/):
один файл на решение, нумерация сквозная. Принятая запись неизменяема: решение,
которое поменялось, *замещается* новым ADR, а не правится на месте, — так
сохраняется след рассуждений (ADR-0001).

Каждая запись содержит контекст, само решение, рассмотренные и отклонённые
альтернативы и последствия — включая издержки, на которые мы сознательно пошли.

## Журнал

| #    | Решение                                                         | Статус  |
|------|-----------------------------------------------------------------|---------|
| [0001](0001-record-architecture-decisions.md) | Вести Architecture Decision Records             | Принято |
| [0002](0002-on-prem-data-sovereign-desktop-app.md) | On-prem десктопное приложение с суверенитетом данных | Принято |
| [0003](0003-bundle-llama-server-sidecar.md) | llama-server как sidecar в составе Tauri        | Принято |
| [0004](0004-compose-rag-with-per-department-lora.md) | RAG + LoRA-адаптеры по департаментам            | Принято |
| [0005](0005-postgresql-pgvector-single-datastore.md) | PostgreSQL + pgvector как единое хранилище      | Принято |
| [0006](0006-hybrid-retrieval-rrf-fusion.md) | Гибридный поиск со слиянием по RRF              | Принято |
| [0007](0007-embeddings-table-model-registry.md) | Отдельная таблица эмбеддингов и реестр моделей  | Принято |
| [0008](0008-department-scoped-row-level-security.md) | Row-Level Security в разрезе департаментов      | Принято |
| [0009](0009-denormalize-department-id.md) | Денормализация department_id в chunks и chunk_embeddings | Принято |
| [0010](0010-async-document-ingestion.md) | Асинхронная загрузка документов                 | Принято |
| [0011](0011-defer-dormant-membership-policy-cleanup.md) | Отложенная чистка спящих RLS-политик на memberships | Замещено [0021](0021-rls-membership-once-per-query.md) |
| [0015](0015-document-deletion.md) | Удаление документов: автор или администратор, контент стирается, остаётся надгробие | Принято |
| [0016](0016-default-department-admin-assigned-membership.md) | Общий департамент по умолчанию, членство назначает администратор | Принято |
| [0017](0017-department-deletion.md) | Удаление департамента вместе с документами | Принято |
| [0018](0018-app-user-least-privilege.md) | Минимальные привилегии роли app_user | Принято |
| [0019](0019-text-extraction.md) | Извлечение текста: PDF и DOCX на чистом Rust, без OCR | Принято |
| [0020](0020-spreadsheets-and-retrieval-at-scale.md) | Таблицы (XLSX, XLS, ODS) и поиск на их масштабе | Принято |
| [0021](0021-rls-membership-once-per-query.md) | Политики RLS вычисляют членство один раз на запрос; спящие политики удалены | Принято |
| [0022](0022-calculations-over-spreadsheet-tables.md) | Расчёты по таблицам: план от модели, SQL от ядра | Принято |

Номера 0012–0014 зарезервированы за решениями из раздела ниже.

## Не записанные решения

Три решения из раздела «Open decisions» в [CLAUDE.md](../../CLAUDE.md) ещё
ждут своего ADR: граница доверия воркера загрузки (0012), контентно-адресуемое
хранилище блобов (0013) и способ поставки PostgreSQL вместе с приложением
(0014 — самый рискованный пункт, pgvector компилируемый).
