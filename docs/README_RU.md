<h1 align="center">fast-n-rust (<code>fnr</code>)</h1>

<p align="center">
  <b>Ускорьте тормозящие 5% вашего приложения на Rust — или соберите весь скрипт в один быстрый бинарник.</b><br>
  Каждое изменение автоматически проверяется на реальном записанном поведении, чтобы ИИ-агент ничего не сломал.
</p>

<p align="center">
  <a href="../README.md">English</a> · <a href="#быстрый-старт">Быстрый старт</a> · <a href="#3-реальных-сценария">Сценарии</a> · <a href="#когда-fnr-реально-полезен-а-когда-нет">Когда нужен</a> · <a href="#бенчмарки-и-что-под-капотом">Бенчмарки и детали</a> · <a href="../NOTICE">Атрибуция (MIT)</a>
</p>

---

**`fast-n-rust`** (`fnr`) подключается прямо к чату вашей IDE (**Cursor, Antigravity, Windsurf, Claude Code, VS Code Copilot, Codex, Gemini, OpenCode**) через MCP и скиллы — или работает из терминала.

## 3 реальных сценария

1. **Ускорить тормозящую функцию в 10–200× без переписывания всего проекта**
   - Тяжёлый парсинг CSV/JSON/логов, нечёткий поиск и дедупликация, скоринг, обработка данных, CPU-циклы в Python, TypeScript/Node, Go, Java, C# и др.
   - Промпт в чат IDE: *«Найди, что тормозит в этом проекте, и безопасно ускорь через fast-n-rust»*
2. **Ускорить код для браузера / Vercel / Edge через WebAssembly (WASM)**
   - Браузер и serverless-платформы (Vercel, Netlify, Cloudflare) не грузят нативные `.node`/`.so`-библиотеки — горячую TS/JS-функцию можно вынести в кроссплатформенный WASM (`fnr escalate <id>` + `fnr bridge . <unit> --kind wasm`).
3. **Упаковать Python/JS-скрипт или CLI-утилиту в один быстрый бинарник (`.exe`)**
   - Конечным пользователям не нужны Python или Node.js; пошаговый перенос с проверкой совпадения на реальных прогонах.
   - Промпт в чат IDE: *«Собери этот скрипт в один самостоятельный бинарник на Rust через fast-n-rust»*

## Когда `fnr` реально полезен, а когда нет

- **Даёт 10–200× ускорение:** тяжёлые CPU-циклы, парсинг, нечёткий поиск/дедупликация, скоринг, обработка данных и перенос CLI-утилит в бинарник (`fnr app`).
- **Не ускорит:** SQL-запросы к БД, внешние HTTP/OpenAI API и `async`-ожидание сети. В таких случаях `fnr scan` честно диагностирует причину (`optimize_io_first`), а не навязывает переписывание на Rust.
- **Требования:** для локальной сборки портов нужны `rustc`/`cargo` и системный C-линкер (`cc` / Xcode CLT / VS C++ Build Tools). `fnr doctor` проверяет только стек текущего проекта (для всех 14 языков — `fnr doctor --all`).

## Быстрый старт

### 1. Установка (готовые бинарники для Linux / macOS / Windows)

```sh
# Linux / macOS (сам прописывает PATH в профиль оболочки)
curl -fsSL https://raw.githubusercontent.com/terrarich2004/fast-n-rust-everything/main/install.sh | sh
```

```powershell
# Windows PowerShell (сам добавляет папку в User PATH)
irm https://raw.githubusercontent.com/terrarich2004/fast-n-rust-everything/main/install.ps1 | iex
```

Или через Cargo из GitHub:
```sh
cargo install --git https://github.com/terrarich2004/fast-n-rust-everything fast-n-rust
```

### 2. Основной сценарий — из чата вашей IDE

```sh
fnr setup        # пропишет MCP и скиллы для Cursor, Antigravity, Claude Code, Windsurf, VS Code, Codex, Gemini, OpenCode
fnr doctor       # проверит Rust и рантаймы текущего проекта
```

Откройте проект в IDE и напишите агенту:
> *«Проанализируй и ускорь этот проект с помощью fast-n-rust»*

### 3. Или через терминал

```sh
fnr go --agent claude --strict -- python3 app.py
```

Или по шагам (поддерживаются `python3`, `uv run`, `pytest`, `uvicorn`, `node`, `npm run`, `pnpm`, `yarn`, `tsx`, `ts-node`):

```sh
fnr scan -- python3 app.py                                           # профилирование + диагностика узких мест
fnr scan --duration 4 --load "curl -s http://127.0.0.1:8000/run" -- uvicorn main:app  # живые HTTP-серверы и боты
fnr port                                                             # запись до 500 вызовов + Performance Blueprint + cargo test
# Реализуйте Rust в .fast/ports/<id>/src/lib.rs (или поручите агенту в IDE)
fnr verify --gate
fnr enable
fnr bench && fnr status                                              # замер 95% CI + RSS и проверка готовности к деплою
fnr deploy --write                                                   # сгенерирует готовые Dockerfile.fnr и GitHub Actions CI
fnr commands                                                         # полный список команд (batch, fuzz, CI, package, whole-app)
```

---

## Бенчмарки и что под капотом

**`fast-n-rust`** (`fnr`) объединяет движки [**`erkamyaman/rust-anything`**](https://github.com/erkamyaman/rust-anything) и [**`EL4CTEO/fast-anything`**](https://github.com/EL4CTEO/fast-anything) в одном Cargo workspace, единой CLI-утилите и многопоточном MCP-сервере — с перекрёстной синергией и исправлением багов апстримов.

- **Режим «5% → Whole-App» и Mode Advisor**: `fnr scan` оценивает накладные расходы FFI (помечая `[direct ABI]` там, где работает бинарный C ABI) и честно подсказывает: точечный порт, `--batch` на все ядра, отказ от Rust при I/O-узком месте (`optimize_io_first`), `--load` при простое сервера (`no_workload_exercised`) или `whole-app` перенос.
- **Трёхуровневый FFI (`fa_direct` → `fa_bin` → `fa_call`)**: прямой нуль-JSON C ABI для скаляров/строк/срезов (`C`, `C++`, `Go`, `C#`, `Java FFM`, `Kotlin`, `PHP`), компактный бинарный wire v1 (`fa_bin`) без DOM-парсинга (`C++`, `Java`, `C#`, `Go`, `PHP`, `Ruby`) и JSON fallback (`fa_call`). На `micro-ffi-c` (300 тыс. вызовов) `fa_direct` (`56.95ms`) в **3.23×** быстрее JSON (`183.77ms`), а отказ от JSON DOM в `fuzzy-dedupe-cpp` снизил пиковый RSS в **3.2×** (`25.6 MB` → **`7.8 MB`**).
- **Строгий TypeScript и Python dataclasses**: для Python автоматически генерируются `#[derive(FromPyObject)]`, для TypeScript — `#[napi(object)]` из `interface`/`type` и строгие `tsc`-совместимые шимы (`export let f = function f`), полностью обратимые через `fnr disable`.
- **Дифференциальный фаззинг, `--native-cpu`, `--pgo` и `fnr deploy --write`**: фаззер сжимает контрпримеры и сохраняет их в `recordings.jsonl`; `fnr deploy --write` генерирует `Dockerfile.fnr` (с `fnr build --deploy` и `ENV FNR_STRICT=1`) и `.github/workflows/fnr-deploy.yml`.
- **Компактный MCP по умолчанию (7 инструментов, `--all-tools` для 30)**: снижает расход токенов схемы LLM на ~75%. Двойные kill-switch переменные (`FNR_DISABLE=1` и `FAST_ANYTHING_DISABLE=1` / `RUST_ANYTHING_MODE=old`).

### Замеры на живых прогонах (Linux x86_64, Ryzen 7 4800HS)

| Пример и объём задачи | Транспорт | До | После | Ускорение | Память (RSS) / заметка |
| :--- | :--- | :---: | :---: | :---: | :--- |
| `examples/fuzzy-dedupe` (Python, 700 имён) | PyO3 native | 4894.1ms | 23.4ms | **209.31×** | 13.6 MB RSS (`--pgo`: 25.2ms) |
| `examples/fuzzy-dedupe-ts` (Node, 2 000 имён) | napi-rs native | 3379.0ms | 119.3ms | **28.32×** | 75.6 MB RSS |
| `examples/fuzzy-dedupe-cs` (C# .NET 10, 4 000 имён) | `fa_bin` wire v1 | 3961.0ms | 186.3ms | **21.26×** | 38.7 MB RSS (28.8× на вызове) |
| `examples/fuzzy-dedupe-java` (Java 25 FFM, 4 000 имён) | `fa_bin` wire v1 | 4185.4ms | 1417.8ms | **2.95×** | 225.5 MB RSS (20.8× на вызове; ~1.3s — запуск и компиляция `java App.java`) |
| `examples/fuzzy-dedupe-cpp` (C++, 6 000 имён) | `fa_bin` wire v1 | 3941.7ms | 362.8ms | **10.87×** | **7.8 MB RSS** (как у чистого C++, вместо 25.6 MB с JSON DOM) |
| `examples/mandelbrot-c` (C, 8 масштабов × 2 000 итер.) | `fa_direct` ABI | 6077.2ms | 163.0ms | **37.27×** | с `--native-cpu` (`ship`: 204.9ms / 29.64×) |
| `examples/micro-ffi-c` (C, 300 тыс. микровызовов) | `fa_direct` vs `fa_call` | 54.92ms (C) | 56.95ms | 0.96× к C | **В 3.23× быстрее**, чем JSON `fa_call` (`183.77ms`); заблокировано защитой без `--force` |

**Как понимать цифры в таблице:**
- **Почему для разных языков разное время (23 мс на Python и 363 мс на C++):** размер входных данных подобран так, чтобы исходный код работал 3–5 секунд. Python перебирает **700 имён** (~245 тыс. пар), а C++ — **6 000 имён** (~18 млн пар, то есть **в 73 раза больше работы**).
- **Почему в Java итог 1.4 секунды (2.95×):** сама функция на Rust быстрее **в 20.8 раза**, но `java App.java` при каждом запуске тратит ~1.3 секунды на старт JVM и компиляцию перед началом расчёта. В `.jar` или сервере действует полный выигрыш **20.8×**.
- **Почему `micro-ffi-c` показал `0.96×`:** это защита от бессмысленного вызова крошечной функции C по одной 300 тыс. раз через FFI — `fnr` блокирует такую замену без `--force` и советует портировать внешний цикл.

**Область проверки**: вживую на Linux x86_64 прогнаны 6 установленных рантаймов (C, C++, Python, TypeScript/Node, Java 25, C# .NET 10); для Go, PHP, Ruby, R и Kotlin (нет локальных рантаймов) пройдены unit-тесты, Windows/macOS на этом хосте не запускались.

### Эскалация и перенос всего приложения

```sh
fnr escalate app.dedupe --batch dedupe:names
fnr analyze . --whole-app
fnr app record . -- python3 cli.py
fnr app front-door .
fnr app wire . <unit> --command <name>
fnr app lift .
```

## Языки

Поддерживаются **14 языков и рантаймов**: Python 3.10+ (`python`, `uv`, `poetry`), JavaScript / TypeScript (Node 22+, Bun, Deno 2+), Go 1.22+, Java 22+ (Maven/Gradle/FFM/JNI), Kotlin/JVM (+ Android NDK/JNI), Ruby 3.3+, C, C++, C#/.NET 8+, PHP 8.1+ (FFI), R 4.0+ (`Rscript`), PowerShell 7.4+ (`pwsh`).

## MCP-сервер, ресурсы и агенты

Многопоточный JSON-RPC 2.0 MCP-сервер (`fast-n-rust mcp` / `fnr mcp`) с мгновенным `ping`, `notifications/cancelled`, изолированным по потокам `notifications/progress`, сериализацией мутирующих вызовов по workspace, аннотациями, **7 компактными инструментами по умолчанию** (`inspect`, `scaffold`, `check`, `switch`, `measure`, `ship`, `app` — или `--all-tools` для всех **30 инструментов**), **4 промптами** (`fast_optimize`, `port_unit`, `escalate_port`, `whole_app_rewrite`) и **5 MCP-ресурсами** (`fnr://guide/skill`, `fnr://guide/unit-porting`, `fnr://guide/whole-app`, `fnr://project/status`, `fnr://project/port-map`).

Попробовать за 10 секунд на встроенном примере:

```sh
fnr -C examples/fuzzy-dedupe scan -- python3 app.py
fnr completions bash > ~/.local/share/bash-completion/completions/fnr
```

Поддерживаемые пресеты агентов в `fnr go --agent`: `claude`, `codex`, `gemini`, `opencode`, `aider`, либо любая команда, принимающая промпт на `stdin`.

## Лицензия и атрибуция

Лицензия MIT ([LICENSE](../LICENSE), [NOTICE](../NOTICE)). Основано на открытых проектах [**erkamyaman/rust-anything**](https://github.com/erkamyaman/rust-anything) и [**EL4CTEO/fast-anything**](https://github.com/EL4CTEO/fast-anything).
