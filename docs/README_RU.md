<h1 align="center">fast-n-rust (<code>fnr</code>)</h1>

<p align="center">
  <b>Перепишите на Rust те самые 5%, которые тормозят — или поднимите весь проект в чистый Rust.</b><br>
  Каждое изменение доказывается на реальном записанном поведении. Создано для людей и ИИ-агентов.
</p>

<p align="center">
  <a href="../README.md">English</a> · <a href="#быстрый-старт">Быстрый старт</a> · <a href="#почему-fast-n-rust">Сравнение</a> · <a href="#языки">14 языков</a> · <a href="#mcp-сервер-и-агенты">MCP и агенты</a> · <a href="../NOTICE">Атрибуция (MIT)</a>
</p>

---

**`fast-n-rust`** (`fnr`) объединяет лучшие движки [**`erkamyaman/rust-anything`**](https://github.com/erkamyaman/rust-anything) и [**`EL4CTEO/fast-anything`**](https://github.com/EL4CTEO/fast-anything) в одном Cargo workspace, единой CLI-утилите (`fast-n-rust` и короткий алиас `fnr`) и многопоточном MCP-сервере — с перекрёстной синергией и исправлением открытых багов обоих апстримов.

- Нужно быстро ускорить горячую функцию в 10–100× без переписывания всего приложения? Используйте **Fast 5% Hotspot Loop** (`fnr go`, `fnr scan`, `fnr port`, `fnr verify --gate`, `fnr enable`).
- Нужен многопоточный **Rayon `--batch`**, фаззинг (**`fuzz`**), теневой трафик (**`shadow`**) или пошаговый **Strangler-Fig перенос всего приложения в чистый Rust**? Запустите **`fnr escalate <id>`** или **`fnr app record` → `app front-door` → `app wire` → `app lift`**.

## Почему `fast-n-rust`?

- **Режим «5% → Whole-App», Mode Advisor и оценка FFI-бюджета**: `fnr scan` оценивает накладные расходы FFI (помечая `[direct ABI]` там, где работает бинарный C ABI) и подсказывает оптимальную стратегию: точечный hotspot-порт, `--batch` на все ядра или whole-app перенос.
- **Трёхуровневый FFI (`fa_direct` → `fa_bin` → `fa_call`)**: прямой нуль-JSON C ABI для скаляров/строк/срезов (`C`, `C++`, `Go`, `C#`, `Java FFM`, `Kotlin`, `PHP`), компактный бинарный wire v1 (`fa_bin`) без DOM-парсинга для коллекций и структур (`C++`, `Java`, `C#`, `Go`, `PHP`, `Ruby`) и универсальный JSON fallback (`fa_call`). На `micro-ffi-c` (300 тыс. вызовов) `fa_direct` (`56.95ms`, `189.8ns/вызов`) в **3.23×** быстрее JSON (`183.77ms`), а отказ от JSON DOM в `fuzzy-dedupe-cpp` снизил пиковый RSS в **3.2×** (`25.6 MB` → **`7.8 MB`**, на уровне чистого C++).
- **Дифференциальный фаззинг, авто-минимизация и `--save-regressions`**: для C, C++, Java, C#, Python, Node/TS, PHP, Ruby и R фаззер сравнивает порт против реального оракула исходного кода, сжимает контрпримеры и сохраняет их в `recordings.jsonl`.
- **Портативный `ship` LTO + явный `--native-cpu` и `--pgo`**: по умолчанию релизы переносимы, `--native-cpu` добавляет `-C target-cpu=native` (на `mandelbrot-c` это ускорило порт с `204.9ms` / **29.64×** до `163.0ms` / **37.27×**), а `--pgo` использует `llvm-profdata` из того же LLVM-семейства (проверено вживую; на микробенчмарках PGO оказался немного медленнее: `177.5ms` на C, `462.2ms` на C++, `25.2ms` на Python).
- **Единый `fnr status` и чистый `--json`**: показывает и `.fast`-хотспоты (транспорт, профиль сборки, verify), и `rust/`-крейты без примесей прогресса в JSON.
- **Для обычных разработчиков без ИИ**: поддержка C/C++ `compile_commands.json` (`--cdb`), генерация обычного `tests/recorded_parity.rs` (`cargo test` работает напрямую), `fnr fuzz` / `--float-tolerance` / `known-diffs` / `fnr package` прямо на `.fast/` портах, статистический `fnr bench` (95% CI, CV%, Mann-Whitney p-value, peak RSS) и `fnr ci`.
- **Обогащение `TASK.md`**: `fnr port` добавляет статические риски, `Performance Blueprint` (SIMD/ILP без FMA, work-stealing, ASCII `&[u8]` fast-path), рекомендации крейтов из `crates.toml` и чек-лист `gate`.
- **`fnr escalate <id>`**: переносит hotspot-порт из `.fast/ports/<id>` в `rust/`-крейт с автоматическим отключением активного inline-патча, автоподстановкой совместимой C/FFI-реализации в `src/port.rs`, сохранением зависимостей, `known-diffs.jsonl`, вызовов (`recorded-inputs.jsonl`) и `hotspot_reference.rs`.
- **Компактный MCP по умолчанию (7 инструментов, `--all-tools` для 29)**: снижает расход токенов схемы LLM на ~75% при сохранении полной обратной совместимости.
- **Безопасный `fnr clean` и исправленные баги апстримов**: `rust-anything` (#288–#291, #300–#301) и `fast-anything` (#76), двойные kill-switch переменные (`FNR_DISABLE=1` и `FAST_ANYTHING_DISABLE=1` / `RUST_ANYTHING_MODE=old`).

## Быстрый старт

### Установка

**Через Cargo прямо с GitHub:**
```sh
cargo install --git https://github.com/terrarich2004/fast-n-rust-everything fast-n-rust
```

**Готовые бинарники (Linux / macOS / Windows):**
```sh
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/terrarich2004/fast-n-rust-everything/main/install.sh | sh

# Windows (PowerShell)
irm https://raw.githubusercontent.com/terrarich2004/fast-n-rust-everything/main/install.ps1 | iex
```

**Из локального клона или через Node:**
```sh
cargo install --path crates/cli
# или: npx fast-n-rust --help
```

После установки проверьте окружение и при желании подключите ИИ-агентов:
```sh
fnr doctor
fnr setup        # пропишет `fast-n-rust mcp` и скиллы для Claude, Codex, Cursor, Gemini, Antigravity, OpenCode
```

### Ускорить медленные 5% одной командой

```sh
fnr go --agent claude --strict -- python3 app.py
```

Или по шагам:

```sh
fnr scan -- python3 app.py
fnr port
# Напишите Rust в .fast/ports/<id>/src/lib.rs или отдайте TASK.md агенту
fnr verify --gate
fnr enable
fnr bench && fnr report
```

### Эскалация и перенос всего приложения

```sh
fnr escalate app.dedupe --batch dedupe:names
fnr analyze . --whole-app
fnr app record . -- python3 cli.py
fnr app front-door .
fnr app wire . <unit> --command <name>
fnr app lift .
```

## Замеры на живых прогонах (Linux x86_64, Ryzen 7 4800HS)

| Пример и объём задачи | Транспорт | До | После | Ускорение | Память (RSS) / заметка |
| :--- | :--- | :---: | :---: | :---: | :--- |
| `examples/fuzzy-dedupe` (Python, 700 имён) | PyO3 native | 4894.1ms | 23.4ms | **209.31×** | 13.6 MB RSS (`--pgo`: 25.2ms) |
| `examples/fuzzy-dedupe-ts` (Node, 2 000 имён) | napi-rs native | 3379.0ms | 119.3ms | **28.32×** | 75.6 MB RSS |
| `examples/fuzzy-dedupe-cs` (C# .NET 10, 4 000 имён) | `fa_bin` wire v1 | 3961.0ms | 186.3ms | **21.26×** | 38.7 MB RSS (28.8× на вызове) |
| `examples/fuzzy-dedupe-java` (Java 25 FFM, 4 000 имён) | `fa_bin` wire v1 | 4185.4ms | 1417.8ms | **2.95×** | 225.5 MB RSS (20.8× на вызове; ~1.3s — запуск и компиляция `java App.java`) |
| `examples/fuzzy-dedupe-cpp` (C++, 6 000 имён) | `fa_bin` wire v1 | 3941.7ms | 362.8ms | **10.87×** | **7.8 MB RSS** (как у чистого C++, вместо 25.6 MB с JSON DOM) |
| `examples/mandelbrot-c` (C, 8 масштабов × 2 000 итер.) | `fa_direct` ABI | 6077.2ms | 163.0ms | **37.27×** | с `--native-cpu` (`ship`: 204.9ms / 29.64×) |
| `examples/micro-ffi-c` (C, 300 тыс. микровызовов) | `fa_direct` vs `fa_call` | 54.92ms (C) | 56.95ms | 0.96× к C | **В 3.23× быстрее**, чем JSON `fa_call` (`183.77ms`); заблокировано защитой без `--force` |

### Как понимать цифры в таблице (без сложных терминов)

- **Почему для разных языков разное время (23 мс на Python и 363 мс на C++):** размер входных данных подобран так, чтобы исходный код работал примерно 3–5 секунд. Медленный Python перебирает **700 имён** (~245 тыс. пар), а скомпилированный C++ — **6 000 имён** (~18 млн пар, то есть **в 73 раза больше работы**). Поэтому на C++ задача в 73 раза тяжелее, а по скорости передачи и расчёта он почти в 3 раза быстрее варианта с JSON у прародителя.
- **Почему в `mandelbrot-c` «До» — 6 секунд, а не 0.43 секунды:** в этом тесте объём вычислений **в 14 раз больше**, чем в старой таблице `fast-anything`. На более тяжёлой задаче Rust-версия ускорила оптимизированный `clang -O2` в **37.27 раза** (против `3.4×` в старом примере).
- **Почему в Java итог 1.4 секунды (2.95×):** сама переписанная на Rust функция работает **в 20.8 раза быстрее** (против `11×` у прародителя). Но бенчмарк замеряет весь процесс целиком от старта до выхода, а команда `java App.java` при каждом запуске тратит ~1.3 секунды просто на старт JVM и компиляцию `.java`-файлов ещё до запуска расчёта. В обычном `.jar`-приложении или сервере действует полный выигрыш **20.8×**.
- **Почему `micro-ffi-c` показал `0.96×`:** это специальная «проверка на дурака» — крошечную функцию на C вызывают 300 тысяч раз подряд по одной. Выносить настолько мелкий шаг наружу невыгодно, и `fnr` автоматически блокирует такую замену без `--force`, советуя портировать внешний цикл целиком (при этом прямой мост `fa_direct` всё равно в **3.23×** быстрее JSON).

**Область проверки**: вживую на Linux x86_64 прогнаны 6 установленных рантаймов (C, C++, Python, TypeScript/Node, Java 25, C# .NET 10); для Go, PHP, Ruby, R и Kotlin (нет локальных рантаймов) пройдены unit-тесты, Windows/macOS на этом хосте не запускались.

## Языки

Поддерживаются **14 языков и рантаймов**: Python 3.10+ (`python`, `uv`, `poetry`), JavaScript / TypeScript (Node 22+, Bun, Deno 2+), Go 1.22+, Java 22+ (Maven/Gradle/FFM/JNI), Kotlin/JVM (+ Android NDK/JNI), Ruby 3.3+, C, C++, C#/.NET 8+, PHP 8.1+ (FFI), R 4.0+ (`Rscript`), PowerShell 7.4+ (`pwsh`).

## MCP-сервер, ресурсы и агенты

Многопоточный JSON-RPC 2.0 MCP-сервер (`fast-n-rust mcp` / `fnr mcp`) с мгновенным `ping`, `notifications/cancelled`, изолированным по потокам `notifications/progress`, сериализацией мутирующих вызовов по workspace, аннотациями, **7 компактными инструментами по умолчанию** (`inspect`, `scaffold`, `check`, `switch`, `measure`, `ship`, `app` — или `--all-tools` для всех **29 инструментов**), **4 промптами** (`fast_optimize`, `port_unit`, `escalate_port`, `whole_app_rewrite`) и **5 MCP-ресурсами** (`fnr://guide/skill`, `fnr://guide/unit-porting`, `fnr://guide/whole-app`, `fnr://project/status`, `fnr://project/port-map`).

Попробовать за 10 секунд на встроенном примере:

```sh
fnr -C examples/fuzzy-dedupe scan -- python3 app.py
fnr completions bash > ~/.local/share/bash-completion/completions/fnr
```

Поддерживаемые пресеты агентов в `fnr go --agent`: `claude`, `codex`, `gemini`, `opencode`, `aider`, либо любая команда, принимающая промпт на `stdin`.

## Лицензия и атрибуция

Лицензия MIT ([LICENSE](../LICENSE), [NOTICE](../NOTICE)). Основано на открытых проектах [**erkamyaman/rust-anything**](https://github.com/erkamyaman/rust-anything) и [**EL4CTEO/fast-anything**](https://github.com/EL4CTEO/fast-anything).
