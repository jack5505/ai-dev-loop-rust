# ai-dev — оркестратор AI dev loop на Rust

Один бинарник вместо двух bash-скриптов (`orchestrator-ci.sh`, 802 строки, и
`backlog-keeper.sh`). Делает одну итерацию разработки: берёт задачу из очереди
GitHub issues → получает реализацию от агента → открывает PR → ждёт CI →
проводит авто-ревью → ставит на merge либо зовёт человека.

Перенос выполнен по спецификации «AI dev loop → Rust» от 2026-09-22. Источник
истины поведения — установленный на сервере `orchestrator-ci.sh`; все 11
контрактов C1–C11 из спецификации сохранены, каждый оплачен инцидентом.
Осознанные отличия от bash перечислены в [PORT-NOTES.md](PORT-NOTES.md).

## Подкоманды

| Команда | Что делает |
| --- | --- |
| `ai-dev run [--instance N] [--once] [--dry-run] [--issue N]` | одна итерация цикла |
| `ai-dev backlog [--instance N] [--force] [--dry-run]` | смотритель бэклога (раз в неделю) |
| `ai-dev queue [--instance N] [--json]` | очередь — тем же запросом, что итерация (C1) |
| `ai-dev unblock [--instance N] [--dry-run]` | вернуть в очередь задачи с закрытым блокером |
| `ai-dev watch-prs [--instance N] [--json]` | сводка по зависшим и конфликтующим AI-PR |
| `ai-dev status [--instance N]` | замок, отметки, состояние итераций |
| `ai-dev config check [--instance N] [--json]` | все действующие значения и валидация, включая C9 |
| `ai-dev doctor` | gh/git/claude, авторизация, инстансы, юниты |

Коды возврата: `0` — штатное завершение (включая «очередь пуста», «замок занят»
и «ушло человеку»), `1` — авария, `2` — ошибка конфигурации (не повод для
алерта «оркестратор упал»).

`--dry-run` печатает каждое изменяющее действие и не выполняет его; читающие
запросы идут как обычно. Это основной инструмент сверки с bash-версией. Замок
в этом режиме не захватывается намеренно: неделя сухих прогонов по таймеру идёт
рядом с боевым bash по тому же `LOCK_FILE`, и отбирать у него круг нельзя. То
же касается `backlog --dry-run`.

## Конфигурация

Формат тот же, что у bash: файл `KEY=value`, права 0600 — его читают и systemd
(`EnvironmentFile=`), и `ai-dev`. Комментарии `#` и префикс `export`
понимаются, неизвестные ключи игнорируются.

Файл ищется в первом подходящем месте:

1. `$AI_DEV_CONFIG_DIR/ai-dev-<инстанс>.env`
2. `/etc/ai-dev-<инстанс>.env` — раскладка DEPLOY.md
3. `$XDG_CONFIG_HOME/ai-dev-<инстанс>.env`
4. `~/.config/ai-dev-<инстанс>.env` — rootless-раскладка (у пользователя нет sudo)

Какой инстанс берётся, если `--instance` не указан: из `AI_DEV_INSTANCE`, а
когда на машине настроен ровно один конфиг — из него. Под systemd не нужно ни
то, ни другое: переменные уже в окружении. Когда инстансов несколько и ни один
не назван, `ai-dev` печатает их список вместо невнятной ошибки.

**Переменная окружения важнее файла.** Под systemd так и работает, а при ручном
запуске это первая причина «правлю файл, ничего не меняется» — `config check`
помечает такие значения словами «← из окружения».

### Значения

Обязательна только `REPO_DIR`. Ровно одна из `CLAUDE_CODE_OAUTH_TOKEN` /
`ANTHROPIC_API_KEY` должна быть непустой; `--max-budget-usd` добавляется агенту
только с API-ключом.

| Ключ | По умолчанию | Смысл |
| --- | --- | --- |
| `REPO_DIR` | — | клон репозитория, единственный обязательный |
| `BASE_BRANCH` | `main` | база для PR |
| `DEV_MODE` | `local` | `local` — агент работает на сервере; `github-app` — `@claude` на раннерах Actions |
| `AUTO_MERGE` | `false` | `true` → одобренный ревью PR уходит в squash-авто-merge |
| `CLAUDE_MODEL` | `sonnet` | модель агента |
| `MAX_ITERATIONS` | `3` | попыток починить красный CI |
| `MAX_REVIEW_ROUNDS` | `2` | кругов авто-ревью до вызова человека |
| `APP_WAIT_MIN` | `180` | минут ждать PR или фикс от `@claude` (см. C9) |
| `CI_START_WAIT` | `30` | секунд на старт Actions перед опросом чеков |
| `PR_STALE_DAYS` | `3` | порог «PR завис» для сторожа открытых PR |
| `TASK_LABEL` | `ai-task` | метка очереди |
| `HUMAN_LABEL` | `needs-human` | метка «нужен человек» |
| `BLOCKED_LABEL` | `blocked` | метка межрепозиторной блокировки |
| `ALLOWED_AUTHORS` | владелец gh-токена | доверенные авторы issue через пробел (защита от prompt injection) |
| `PARTNER_REPO` | пусто | второй репозиторий проекта, `owner/name` |
| `MAX_NEW_TASKS` | `5` | потолок новых задач смотрителя за один проход |
| `KEEPER_INTERVAL_DAYS` | `7` | как часто смотритель реально работает |
| `MAX_BUDGET_USD` | `5` | лимит на вызов, действует только с `ANTHROPIC_API_KEY` |
| `LOCK_FILE` | `/tmp/ai-dev-<repo>.lock` | замок, общий у оркестратора и смотрителя (C10) |
| `TELEGRAM_BOT_TOKEN` + `TELEGRAM_CHAT_ID` | пусто | задаются вместе либо оба пустые |
| `CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY` | — | аутентификация агента, ровно одно из двух |

### Поменять значение

```sh
$EDITOR ~/.config/ai-dev-backend.env       # правим строку, например AUTO_MERGE=true
ai-dev config check --instance backend     # проверяем, что получилось
```

`daemon-reload` не нужен: systemd читает `EnvironmentFile` при каждом старте
службы, поэтому новое значение подхватится со следующего тика таймера.
Перезагрузка юнитов нужна только при правке самого `.service` — например
`TimeoutStartSec`.

Разовый запуск с другим значением, не трогая файл:

```sh
CLAUDE_MODEL=sonnet MAX_ITERATIONS=1 ai-dev run --instance backend
```

### Посмотреть

```sh
ai-dev config check --instance backend          # все действующие значения + валидация
ai-dev config check --instance backend --json   # то же машинно-читаемо
ai-dev status --instance backend                # замок, отметки, состояние итераций
ai-dev queue --instance backend                 # что сейчас в очереди (C1)
ai-dev watch-prs --instance backend --json      # открытые AI-PR: конфликты и зависшие
ai-dev doctor                                   # оба инстанса разом, gh/git/claude, юниты
```

`config check` показывает и незаданные ключи — со значением по умолчанию,
чего не даёт `cat` конфига, — и не печатает секреты: про токены сообщается
только, какой из двух задан. Код возврата 2 означает, что итерацию с такой
конфигурацией запускать нельзя.

Если нужно загрузить весь конфиг в текущую оболочку (осторожно: в окружение
попадут и токены):

```sh
set -a; . ~/.config/ai-dev-backend.env; set +a
```

## Эксплуатация

```sh
systemctl --user list-timers --all                   # когда следующий запуск
journalctl --user -u 'ai-dev@*' -n 200 --no-pager -q # лог итераций (rootless)
journalctl -u 'ai-dev@*' -n 200 --no-pager -q        # он же для системных юнитов
systemctl --user start ai-dev@backend.service        # прогнать итерацию сейчас
```

Юниты шаблонные: `ai-dev.service` без `@<инстанс>` не существует, и рецепт
`journalctl -u ai-dev.service` каждый раз заводит диагностику в ложный тупик.

Смотритель бэклога тикает ежечасно, а работает раз в `KEEPER_INTERVAL_DAYS`
по отметке `.ai-logs/.backlog-keeper-last`. Включение и выключение:

```sh
systemctl --user enable --now ai-backlog@backend.timer
systemctl --user disable --now ai-backlog@backend.timer
ai-dev backlog --instance backend --force --dry-run   # посмотреть, что он сделает
```

Первый запуск после включения происходит сразу: отметки нет, значит проход
«не наш круг» не срабатывает. Если очередь при этом пуста, смотритель дойдёт
до второго поручения и заведёт до `MAX_NEW_TASKS` задач. Чтобы отложить первый
проход на неделю, достаточно создать отметку заранее:
`touch $REPO_DIR/.ai-logs/.backlog-keeper-last`.

## Сборка

На сервере с оркестратором может не быть ни линковщика, ни прав на `apt`,
поэтому сборка идёт в контейнере, а на сервер приезжает готовый статический
бинарник:

```sh
./build.sh release   # target/musl/release/ai-dev, static-pie, без зависимостей
./build.sh test      # юнит- и сценарные тесты
./build.sh clippy    # clippy -D warnings
```

Обычный `cargo build` тоже работает, если в системе есть `cc`.

## Установка

```sh
# системная раскладка
sudo install -m 0755 ai-dev /opt/ai-dev/bin/ai-dev
sudo cp systemd/system/*.service systemd/system/*.timer /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now ai-dev@backend.timer

# rootless (нет пароля sudo)
install -m 0755 ai-dev ~/.local/bin/ai-dev
cp systemd/user/*.service systemd/user/*.timer ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now ai-dev@backend.timer
```

Перед включением таймера: `ai-dev doctor` и `ai-dev config check --instance <N>`.
**C9**: `TimeoutStartSec` юнита обязан быть заметно больше `APP_WAIT_MIN` —
`config check` это проверяет и не даёт запустить итерацию с плохим запасом.
Команды наблюдения за работающим циклом — в разделе «Эксплуатация» выше.

## Откат

Состояние цикла целиком живёт в GitHub — метки, комментарии, маркеры
`BLOCKED-BY`/`ORIGIN`/`AI-TASK` — плюс каталог `.ai-logs` в клоне. Поэтому
переключение между bash и Rust безопасно в любой момент между итерациями:
верните `ExecStart` на `orchestrator-ci.sh`, `daemon-reload`, и цикл продолжит
с того же места. Замок (`flock` на `LOCK_FILE`) у обеих реализаций общий, так
что одновременный запуск исключён даже во время переключения.

## Порядок ввода в строй

Спецификация разводит этапы намеренно: сначала читающие команды, потом
`github-app` (агент работает на раннерах, цена ошибки ниже), потом `local`.

1. `ai-dev queue` даёт то же число, что bash, на обоих инстансах.
2. `ai-dev unblock --dry-run`, `watch-prs`, `status` совпадают с поведением bash.
3. Неделя `run --dry-run` по таймеру рядом с боевым bash — решения в логах
   должны совпадать.
4. Переключение `github-app`-инстанса, затем `local`.
5. `backlog`, затем отмена по SIGTERM и `state-<issue>.json`.

## Тесты

* `markers.rs` — фикстуры реальных инцидентов: маркер в середине строки не
  ловится, маркер старше момента поручения не ловится, `Claude finished` от
  автора `claude` ловится.
* `tests/scenarios.rs` — итерация целиком на фейковых `GitHub`/`Agent`/`Clock`/
  `Vcs`: очередь пуста; агент без коммитов; ответ за 36 секунд без PR; PR найден
  до обращения к `@claude`; `CONFLICTING` → rework → merge; два круга
  `REQUEST_CHANGES` → человек; SIGTERM посреди ожидания → метка проставлена.
