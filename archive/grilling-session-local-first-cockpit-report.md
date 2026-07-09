# Итоги grilling-session: переход от локального координатора к local-first lifecycle cockpit

Дата документа: 2026-07-06  
Аудитория: инженеры, создавшие первоначальную спецификацию проекта `orgasmic`  
Исходный анализируемый репозиторий: `theaspirational/orgasmic`, локальный clone `upstream-orgasmic`  
Проверенный upstream HEAD на момент анализа: `ad380439b69e8f53f9f55457ec316d8071d36798`

## 1. Краткий вывод

Первоначальный дизайн `orgasmic` был сильным и последовательным как дизайн **per-developer local coordination app**: локальный daemon, CLI, React UI, repo-local `.orgasmic/`, явная регистрация проектов, app-owned Org profile, Git-native collaboration, task board, decisions, architecture, glossary, activity, workers, prompts, manager, runs.

Но этот дизайн не является достаточным, если целевая амбиция меняется с “локально координировать работу агента и задач внутри проектов” на “управлять большими проектами и их полным жизненным циклом”. В старой модели отсутствовал слой, который мог бы связать несколько репозиториев, цели, риски, архитектурные изменения, задачи, агентные runs, review artifacts, evidence, release readiness и внешние сигналы в один управляемый lifecycle object.

Сильная формулировка: прежний `orgasmic` был близок к хорошему локальному agent workbench, но не к полноценному lifecycle cockpit. Это не провал первоначальной спецификации. Это нормальный результат того, что исходная спецификация сознательно выбирала v0.0.1-границы: project-centric coordination before heavier graph/lifecycle machinery.

В ходе grilling-session был принят новый product/architecture direction:

```text
Orgasmic should first become a local-first lifecycle cockpit.
```

Ключевая новая иерархия:

```text
Program
  -> Initiative
      -> Projects
      -> Decisions
      -> Architecture changes
      -> Risks
      -> Tasks
      -> Agent runs
      -> Evidence Packs
      -> Stage transitions
      -> Release evidence
```

Первый target реализации был зафиксирован как thin vertical slice:

```text
1 Program workspace
1 Initiative
linked Project(s)
typed Org event log
rebuildable projections
manual Evidence Pack
CLI/API-first implementation path
gated stage transition
minimal Initiative Control Room later
```

Эта модель уже не осталась только разговором. В workspace добавлены:

- glossary в `CONTEXT.md`;
- ADR 0001-0017 в `docs/adr/`;
- design fixture Program workspace в `orgasmic-cockpit.program/.orgasmic-program/`;
- implementation issue breakdown, test strategy, CLI/API contract и evidence packs;
- первый upstream implementation slice в `upstream-orgasmic/crates/orgasmic-core`: Program workspace parser и typed Org event validator с тестами.

Уверенность в направлении: высокая.  
Уверенность в конкретном файловом layout первого Program workspace: средняя; это design fixture, а не окончательная storage ABI.

## 2. Что было хорошего в первоначальном дизайне

Перед критикой важно зафиксировать: исходный дизайн не был хаотичным. Он имел несколько правильных инженерных решений, которые новая модель не отменяет.

### 2.1. Local-first scope был правильным

В `.orgasmic/decisions.org` исходный дизайн явно решил, что `orgasmic` не является hosted SaaS, не является Emacs workflow и не является миграцией HAR. Он является standalone local developer coordination app. Это решение остается правильным.

Причина: если сразу строить hosted team SaaS, проект утонет в identity, billing, tenancy, permissions, conflict policy, cloud sync и организационной политике раньше, чем докажет основной workflow.

Новая модель не отменяет local-first. Она усиливает его: lifecycle truth остается локальным, inspectable, Git-backable и reviewable.

### 2.2. Repo-local `.orgasmic/` как Project state был правильным

Явная регистрация проектов, отсутствие auto-scan и отказ от central database были правильными решениями. Project должен оставаться владельцем своего `.orgasmic/` состояния.

Новая модель не копирует Project state в Program. Она вводит `Project Link Manifest`, который ссылается на Project через stable id, repo identity и local path resolution.

### 2.3. App-owned Org profile был правильным

Org-style plain text как source-of-truth дает Git diffs, ручную инспекцию и отсутствие зависимости от Emacs runtime. Это остается центральным design asset.

Новая модель использует typed Org events: не freeform Org prose, но и не JSON-only database. Это прямое развитие исходной философии.

### 2.4. Daemon/CLI/UI триада была правильной

Исходный дизайн понимал, что CLI нужен людям и manager agent, daemon нужен для serialized writes/API/materialized reads, UI нужен operator surface. Новая модель сохраняет это, но меняет порядок первого lifecycle slice: сначала core model + CLI + daemon API, потом minimal UI.

### 2.5. Git-native collaboration был правильным ограничением

Решение “orgasmic never invokes git automatically” и “collaboration through Git, not own multi-user service” остается правильным. Новая модель добавляет Program workspace как отдельный Git-backable lifecycle root, но не превращает `orgasmic` в Git automation engine.

## 3. Недостатки прошлого дизайна относительно цели “управление большими проектами”

Ниже перечислены не общие вкусовые претензии, а конкретные архитектурные ограничения старой модели, которые стали видны при попытке поднять продукт до lifecycle cockpit.

### 3.1. Project был максимальной управленческой единицей

Старая модель хорошо работала для одного repo/workspace. Даже cross-project Board был, по сути, pointer/index surface: board stores project pointers, Project owns state, aggregation is derived.

Это правильно для project coordination, но недостаточно для large-project lifecycle.

Большие проекты почти всегда включают несколько репозиториев, несколько release surfaces, зависимые API contracts, документацию, UI, daemon, CLI, mobile/browser surface, CI, packaging, security concerns и migration risks. Если максимальная единица управления остается `Project`, то cross-repo initiative вынуждена расползаться по нескольким task lists и architecture files.

Симптом: невозможно естественно сказать “эта Initiative затрагивает daemon API, UI Control Room, CLI command, projection generator и evidence import”. Можно создать задачи в разных Projects, но нет объекта, который владеет общей lifecycle truth.

### 3.2. Task был слишком мелкой единицей для lifecycle управления

Исходная модель уделяла много внимания задачам, task headings, task lifecycle, worker pipeline, `:WORKER:`, `:PIPELINE:`, run sub-states и dispatch.

Это необходимо для execution, но недостаточно для lifecycle.

Task отвечает на вопрос “что нужно сделать?”. Lifecycle cockpit должен отвечать на более широкий вопрос: “какую цель мы ведем через discovery, planning, implementation, review и release readiness, какие проекты затронуты, какие риски остаются, какие архитектурные решения приняты, чем доказано состояние?”.

Если сделать Task главным объектом, `orgasmic` станет локальной Jira с агентами. Это лучше, чем хаотичный markdown, но ниже амбиции lifecycle cockpit.

### 3.3. Board и graph были полезными views, но не рабочим lifecycle cockpit

Board хорош для execution status. Graph хорош для dependency/topology inspection. Но ни board, ни graph не являются достаточным primary operational surface.

Board flatten-ит работу в колонки. Graph показывает связи, но не управляет gate readiness. Ни один из них сам по себе не показывает одновременно:

- scope;
- affected Projects;
- decisions;
- architecture deltas;
- active tasks;
- active agent runs;
- unresolved risks;
- dependencies;
- evidence packs;
- gates;
- stage transition history;
- next actions.

Поэтому был введен `Initiative Control Room` как primary view.

### 3.4. Один status не может выразить lifecycle truth

В старой модели task lifecycle был приведен к стандартной Kanban-схеме: Backlog, Todo, In Progress, In Review, Done, Cancelled. Это правильно для task board, но опасно переносить такую простую схему на lifecycle object.

Для Initiative один `status` будет врать.

Примеры:

- Initiative может быть `In Review`, но `Blocked`, потому что review artifact отсутствует.
- Initiative может быть `Planned`, но `At Risk`, потому что architecture risk не закрыт.
- Initiative может иметь все tasks `Done`, но не быть release-ready, потому что нет accepted Evidence Pack.
- Initiative может перейти дальше с waiver, но это должно ухудшать health или оставлять visible risk.

Именно поэтому принята модель `stage + health + evidence gates`.

### 3.5. Audit/logging существовали, но не как Program lifecycle event log

В исходном дизайне уже были tx files, session JSONL, run transcripts и daemon-mediated writes. Это сильная база. Но они фиксировали activity/runs/project-state changes, а не являлись Program-level causal log.

Для lifecycle management нужен append-only log, который объясняет:

- когда создан Program;
- когда linked Project добавлен;
- когда создана Initiative;
- какие Evidence Packs приняты;
- почему stage transition разрешен;
- какие waivers были сделаны;
- какие typed lifecycle edges активны;
- какие projections были построены из каких events.

Без этого lifecycle state превращается в mutable summary, который трудно audit-ить.

### 3.6. Evidence была рассеянной, не first-class

В старой модели evidence могла жить в task worklogs, run transcripts, comments, tx, review notes, test output, PR/CI links. Это полезные источники данных, но не readiness proof.

Lifecycle cockpit должен уметь сказать: “этот gate закрыт вот этим Evidence Pack, с таким verdict, такими inputs и таким reviewer/operator decision”.

Разница принципиальная:

```text
scattered links != accepted evidence
task Done != release readiness
agent claimed success != accepted outcome
passing command buried in transcript != reviewed gate proof
```

Поэтому введен `Evidence Pack`.

### 3.7. Agents были представлены как workers/runs, но не как audited automation вокруг Initiative

Исходная модель хорошо изолировала worker drivers, supervisor, run events, tool policies и task transitions. Но для lifecycle cockpit надо поднять agent execution на уровень audited automation.

Нужен не только “worker did run”. Нужен record:

- какое intent было выдано;
- какие Projects и paths разрешены;
- какой budget/scope;
- какие команды выполнены;
- какие files touched;
- какие artifacts produced;
- какой claimed outcome;
- какое test evidence;
- какой reviewer verdict;
- какой gate или Evidence Pack это поддерживает.

Поэтому агенты не были названы “team members” в core model. Это сознательный отказ от антропоморфной модели. Агент — это automation under audit.

### 3.8. Внешние системы не были явно поставлены на свое место

Для lifecycle tool нельзя игнорировать GitHub, GitLab, Jira, Linear, CI, deployment tools и release tags. Но если сделать их source of truth, local-first модель сломается.

В старой модели Git-native collaboration была зафиксирована, но не было новой Program-level роли внешних систем: они должны быть adapters/evidence sources, но не authorities.

Решение: external systems import signals and evidence; Program workspace event log owns lifecycle truth.

### 3.9. Не было Program workspace

Без explicit Program workspace есть только два плохих варианта:

1. хранить Program state внутри одного Project, делая этот Project ложным root of authority;
2. хранить Program state только в `$ORGASMIC_HOME`, теряя Git-backable/reviewable lifecycle record.

Оба варианта плохо подходят для больших проектов. Поэтому Program получает отдельный local workspace.

### 3.10. Graph был design graph, но не typed lifecycle dependency model

Исходный design graph важен: decisions -> architecture -> tasks -> code hints. Но lifecycle dependencies требуют typed edges между Initiatives, Projects, tasks, artifacts, Evidence Packs.

Free-text dependency note недостаточна. Graph view недостаточна. Нужны typed lifecycle edges как evented facts:

```text
blocks
depends_on
affects
implements
produces
evidences
supersedes
```

Graph должен быть projection, не source of truth.

## 4. Главная смена рамки

Первоначальная рамка:

```text
per-developer local coordination app
  -> explicit Projects
  -> tasks
  -> workers/runs
  -> decisions/architecture/glossary
  -> board/graph/UI
```

Новая рамка:

```text
local-first lifecycle cockpit
  -> Program workspace
  -> Initiatives
  -> linked Projects
  -> event log
  -> rebuildable projections
  -> Evidence Packs
  -> gated stage transitions
  -> Initiative Control Room
```

Это не означает, что старые сущности выбрасываются. Напротив:

- Project остается repo/workspace boundary.
- Task остается execution unit.
- Worker/run остается execution record.
- Decision/architecture/glossary остаются reasoning surfaces.
- Board и graph остаются secondary views.
- Daemon/CLI/UI остаются delivery surfaces.

Но появляется новый lifecycle layer, которого раньше не было.

## 5. Принятые решения и причины

### 5.1. Решение 1: строить local-first lifecycle cockpit, а не hosted lifecycle platform

Принятое решение: стартовая цель — `local-first lifecycle cockpit`.

Почему:

- Это сохраняет исходный local-first product DNA.
- Это избегает преждевременного SaaS scope: multi-tenancy, billing, org identity, hosted data, cloud sync.
- Это позволяет использовать существующие преимущества: daemon, CLI, UI, Org files, Git review, explicit project registration.
- Это честнее относительно текущей codebase: `orgasmic` уже близок к local coordination workbench, но не к hosted platform.

Что это не значит:

- Не строим hosted Jira replacement.
- Не строим team SaaS.
- Не делаем central project database.
- Не делаем always-on “AI project manager brain”.

### 5.2. Решение 2: добавить Program поверх Project

Принятое решение: `Program` является lifecycle aggregate над несколькими explicit `Project`.

Причина:

`Project` слишком локален. Большой lifecycle effort часто затрагивает несколько Projects. Если оставить только Project, cross-repo work будет моделироваться через неявные links, comments, task references и человеческую память.

Почему Program не заменяет Project:

- Project уже является хорошей repo-local boundary.
- Project owns `.orgasmic/`.
- Project должен оставаться единицей tasks, decisions, architecture, runs и implementation evidence.
- Замена Project на Program сломала бы исходную модель без необходимости.

Итоговая иерархия:

```text
Program -> Projects -> Goals / Tasks / Runs / Artifacts
```

Но управленчески Program ведет Initiatives, а Projects остаются execution/source boundaries.

### 5.3. Решение 3: сделать Initiative главным lifecycle object

Принятое решение: внутри Program основной объект — `Initiative`.

Почему не Task:

Task слишком мелкая единица. Task хороша для выполнения, но не владеет reasoning, risk, architecture deltas, Evidence Packs и release readiness.

Почему не Release:

Release слишком поздняя единица. Release хорошо фиксирует shipping, но плохо управляет discovery/planning/architecture/risk.

Почему Initiative:

Initiative может связать:

- scope;
- affected Projects;
- decisions;
- architecture changes;
- risks;
- dependencies;
- tasks;
- agent runs;
- review artifacts;
- release evidence.

Это делает Initiative естественным lifecycle object, а не просто work item.

### 5.4. Решение 4: хранить Program state в explicit Program workspace

Принятое решение: Program state живет в explicit Git-backable Program workspace, а не внутри одного child Project и не только в hidden `$ORGASMIC_HOME`.

Причина:

Program не принадлежит одному репозиторию. Если положить Program state в один Project, этот Project станет ложным root. Если хранить только в `$ORGASMIC_HOME`, lifecycle record будет хуже reviewable, хуже portable и хуже Git-native.

Целевая форма fixture:

```text
orgasmic-cockpit.program/
  .orgasmic-program/
    program.org
    projects.org
    events/
    projections/
    evidence/
```

Файловый layout еще может меняться. Принцип ownership уже принят: Program state имеет отдельный local workspace.

### 5.5. Решение 5: event log как write authority, projections как review/read surfaces

Принятое решение:

```text
events -> projections -> UI / CLI / review surfaces
```

Почему не один большой `initiatives.org`:

Mutable summary удобен для чтения, но плохо объясняет историю. Он не доказывает, почему состояние стало таким.

Почему не database-only:

Database упрощает queries, но ослабляет plain-text/Git-native product DNA, если становится единственной durable truth.

Почему не append-only log без projections:

Такой log audit-friendly, но плох для ежедневной работы.

Итог:

- event log — source of mutation truth;
- projections — generated current-state views;
- UI/API/CLI читают projections/read models;
- ручные edits projections не являются source-of-truth.

### 5.6. Решение 6: Initiative state = stage + health + gates

Принятое решение: разделить `Lifecycle stage`, `Health` и `Evidence gates`.

Причина:

Один `status` смешивает разные вопросы:

- где находится Initiative?
- насколько она здорова?
- доказан ли переход?
- есть ли waivers?
- чего не хватает для следующего gate?

Целевая модель:

```text
stage:
  Discovery
  Planned
  In Flight
  In Review
  Release Candidate
  Released
  Archived
  Cancelled

health:
  On Track
  At Risk
  Blocked
  Stale

gates:
  scope accepted
  affected projects linked
  risks reviewed
  architecture delta accepted
  tasks complete
  review artifacts accepted
  release evidence accepted
```

Это вводит более честную lifecycle semantics. Например, `In Review / Blocked` становится валидным и информативным состоянием.

### 5.7. Решение 7: агенты являются audited automation

Принятое решение: agents не являются team members в core lifecycle model. Они являются audited automation.

Причина:

Метафора “агенты как сотрудники” быстро приводит к нестрогим формулировкам: “агент решил”, “агент понял”, “агент владеет задачей”. Для lifecycle cockpit важнее другое:

- intent;
- scope;
- allowed Projects;
- allowed write paths;
- commands;
- touched files;
- evidence;
- claimed outcome;
- reviewer verdict.

Поэтому вводятся `Agent assignment` и `Agent run`.

### 5.8. Решение 8: primary UI view = Initiative Control Room

Принятое решение: главным working view становится `Initiative Control Room`.

Почему не Board:

Board показывает task execution, но не lifecycle reasoning.

Почему не Graph:

Graph показывает relationships, но не управляет readiness и gates.

Почему Control Room:

Он собирает на одном экране:

- stage;
- health;
- gate progress;
- scope;
- affected Projects;
- dependencies;
- risks;
- tasks;
- agent runs;
- blockers;
- decisions;
- architecture deltas;
- review artifacts;
- release evidence;
- event stream;
- next actions.

Это forcing function: если невозможно построить Control Room, значит модель не связывает lifecycle данные достаточно хорошо.

### 5.9. Решение 9: Evidence Pack как first-class object

Принятое решение: readiness и gates подтверждаются `Evidence Pack`, а не scattered links.

Причина:

Evidence для большого проекта должна быть reviewable unit. Она должна отвечать:

- какой gate закрывается;
- какие inputs использованы;
- какие tasks/runs/commands/CI/PRs/decisions входят;
- кто/что дал verdict;
- accepted, rejected или waived;
- какие notes/waivers остались.

Task Done не является evidence. Agent claim не является evidence. Test output без review verdict не является gate proof.

### 5.10. Решение 10: внешние системы являются adapters, не authorities

Принятое решение: GitHub/GitLab/Jira/Linear/CI/release tools импортируют signals и evidence, но не владеют lifecycle truth.

Причина:

Если внешняя система становится source of truth, Program workspace превращается в dashboard/cache. Это ломает local-first.

Но если внешние системы игнорировать, cockpit будет игрушкой, потому что PR status, CI checks, release tags и deployment state являются реальной lifecycle evidence.

Итог:

```text
Program event log = lifecycle truth
External adapters = signals / evidence / explicit outbound actions
```

### 5.11. Решение 11: первый target = thin vertical slice

Принятое решение: не строить сразу обзорную platform. Первый target:

```text
1 Program workspace
1 Initiative
2 linked Projects или минимум 1 linked Project на старте
event log
generated projection
Initiative Control Room
manual Evidence Pack
1 imported command/test evidence source
stage transition with gate check
```

Что намеренно исключено:

- GitHub/Jira integrations;
- multi-user roles;
- cloud sync;
- release automation;
- full graph UI;
- AI planning sophistication;
- artifact MDX renderer;
- permissions matrix.

Причина: первый slice должен доказать главный claim продукта, а не покрыть все края будущей платформы.

### 5.12. Решение 12: реализация CLI/API-first, UI later

Принятое решение:

```text
1. Core model
2. CLI
3. Daemon API
4. Minimal UI
```

Причина:

UI-first даст красивый mock Control Room, но не докажет event log, projections, Evidence Packs и gated transitions.

API-only недостаточен, потому что local-first нужно dogfood-ить через CLI against a real workspace.

Integration-first преждевременен, потому что external adapters должны ждать core loop.

### 5.13. Решение 13: typed Org event log

Принятое решение: Program events хранятся как typed Org events.

Пример:

```org
* EVENT initialize_program
:PROPERTIES:
:ID: evt_20260706_171128_program_initialized
:TYPE: program.initialized
:TIME: 2026-07-06T17:11:28+03:00
:ACTOR: local:victor
:PROGRAM_ID: prog_orgasmic_cockpit
:SCHEMA: 1
:END:

#+begin_src json
{
  "name": "Orgasmic Cockpit",
  "workspace_version": 1
}
#+end_src
```

Причина:

- JSON-only слабее для Git review и хуже вписывается в Org-native language.
- Freeform Org prose невозможно надежно валидировать.
- Typed Org сохраняет readable diff и дает strict parser contract.

### 5.14. Решение 14: versioned event schemas + rebuildable projections

Принятое решение: events версионируются, projections disposable/rebuildable.

Причина:

Event formats будут развиваться. Projections будут меняться. Если projections станут вторым источником истины, система получит drift.

Правило:

```text
If projection is stale or invalid:
  delete projection
  rebuild from event log
```

SQLite/read caches допустимы, но только как rebuildable read models.

### 5.15. Решение 15: Project Link Manifest

Принятое решение: Program ссылается на Project через Project Link Manifest.

Причина:

- Только local path плохо переносим.
- Только repo URL недостаточен для локальной работы.
- Копировать Project state в Program нельзя из-за split-brain.

Модель:

```org
* PROJECT upstream-orgasmic
:PROPERTIES:
:PROJECT_ID: proj_upstream_orgasmic
:REPO_URL: https://github.com/theaspirational/orgasmic.git
:LOCAL_PATH: ../upstream-orgasmic
:DEFAULT_BRANCH: main
:HEAD: ad380439b69e8f53f9f55457ec316d8071d36798
:STATUS: active
:SCHEMA: 1
:END:
```

### 5.16. Решение 16: typed lifecycle edges, graph as projection

Принятое решение: dependencies/relations моделируются как typed lifecycle edges, graph является projection.

Причина:

Free-text dependencies невалидируемы. Graph as source-of-truth смешивает data model и visualization. Для health, impact analysis и review нужны typed facts.

Типы первого уровня:

```text
blocks
depends_on
affects
implements
produces
evidences
supersedes
```

### 5.17. Решение 17: gates block by default, waivable by explicit event

Принятое решение: gate enforcement является policy-based.

Default:

```text
missing required evidence -> transition blocked
```

Override:

```text
allowed only with explicit waiver event
reason required
actor required
affected gate required
```

Причина:

Hard-block без waiver превращает cockpit в бюрократическую тюрьму. Pure advisory делает gates бессмысленными. Waiver сохраняет operator control, но оставляет audit trail и risk signal.

### 5.18. Решение 18: self-hosting Initiative

Принятое решение: первый dogfood target — сама Initiative “Local-first lifecycle cockpit thin slice”.

Причина:

Если `orgasmic` не может управлять собственным превращением в lifecycle cockpit, модель слишком абстрактна. Self-hosting Initiative сразу проверяет терминологию, state model, Evidence Pack и gate semantics.

Фактически создано:

```text
Program: Orgasmic Cockpit
Initiative: Local-first lifecycle cockpit thin slice
Stage: In Flight
```

## 6. Новая терминология

Этот раздел намеренно подробный. Термины вводились не ради нового словаря, а потому что старые слова перегружали разные смыслы и скрывали архитектурные границы.

### 6.1. Local-first lifecycle cockpit

Значение: локальный operator cockpit для управления lifecycle work across explicit projects. Он держит durable state локально, inspectable и reviewable.

Почему нужен термин:

“Project manager”, “dashboard”, “workbench”, “task board” и “platform” слишком расплывчаты. Новый термин фиксирует три обязательных свойства:

- local-first;
- lifecycle-oriented;
- operational cockpit, not decorative dashboard.

Что это не:

- не hosted SaaS;
- не central database;
- не Jira clone;
- не всегда включенный AI manager.

### 6.2. Program

Значение: local lifecycle aggregate над несколькими explicit Projects.

Почему нужен термин:

Project уже занят и означает repo/workspace boundary. Нужна сущность выше Project, которая владеет cross-project lifecycle. “Workspace” слишком файловый термин; “portfolio” слишком business-level; “release train” слишком поздний lifecycle stage. `Program` лучше всего описывает объединенный engineering effort.

### 6.3. Program workspace

Значение: explicit Git-backable directory, owning Program state.

Почему нужен термин:

Нужно отличать логическую сущность `Program` от ее durable local storage. Это также предотвращает ошибку “давайте положим Program в один Project” или “давайте спрячем Program в `$ORGASMIC_HOME`”.

### 6.4. Project

Значение: explicit repo/workspace enrolled into cockpit.

Почему термин сохраняется:

Он уже корректен. Новый дизайн не должен разрушать repo-local ownership. Project остается владельцем `.orgasmic/`, tasks, decisions, architecture, runs и implementation state.

### 6.5. Project Link Manifest

Значение: Program-owned manifest, linking Projects by stable id, repo identity and local path resolution.

Почему нужен термин:

Нужна граница между “Program references Project” и “Program copies Project”. Manifest предотвращает split-brain.

### 6.6. Initiative

Значение: Program-level lifecycle object, connecting scope, Projects, risks, decisions, architecture changes, tasks, runs, artifacts and evidence.

Почему нужен термин:

Task слишком мал, Release слишком поздний, Goal слишком расплывчат. Initiative — правильная единица для engineering effort, который проходит discovery -> planning -> implementation -> review -> release.

### 6.7. Event log

Значение: append-only write authority for Program and Initiative changes.

Почему нужен термин:

Нужно отделить “что произошло” от “какое сейчас состояние”. Без event log projections становятся mutable truth, а lifecycle audit теряется.

### 6.8. Typed Org event

Значение: Org heading с обязательными properties и typed JSON payload.

Почему нужен термин:

Он соединяет две потребности: human-readable Git review и machine validation. Freeform Org недостаточен. JSON-only хуже соответствует проектной философии.

### 6.9. Event schema

Значение: versioned contract для typed event family.

Почему нужен термин:

Без schema versions старые Program workspaces нельзя будет безопасно читать после изменения формата. Это особенно важно, потому что events являются durable truth.

### 6.10. Projection

Значение: generated current-state view derived from event log.

Почему нужен термин:

Нужно отделить read surface от write authority. `initiatives.org` как projection можно review-ить, но нельзя считать первичной истиной.

### 6.11. Rebuildable projection

Значение: projection, которую можно удалить и пересобрать из event log.

Почему нужен термин:

Он предотвращает drift. Если projection устарела, правильный fix — rebuild, не ручная правка.

### 6.12. Evidence

Значение: material proof attached to lifecycle state.

Почему нужен термин:

Lifecycle state без proof является декларацией. Evidence — общий класс фактов: tests, commands, reviews, CI, PRs, decisions, architecture notes, artifacts.

### 6.13. Evidence Pack

Значение: first-class package of proof for gate or transition.

Почему нужен термин:

Scattered evidence не подходит для readiness. Pack дает reviewable unit with verdict.

### 6.14. Lifecycle stage

Значение: explicit phase of Initiative.

Почему нужен термин:

Он отделяет фазу от health. `In Review` не значит healthy; `Planned` не значит safe.

### 6.15. Health

Значение: computed signal: On Track, At Risk, Blocked, Stale.

Почему нужен термин:

Operator должен видеть не только фазу, но и operational condition. Health может вычисляться из blockers, missing evidence, stale runs, failed checks, risks, waivers.

### 6.16. Evidence gate

Значение: proof condition for stage transition.

Почему нужен термин:

Stage transition должен быть justified. Gate говорит, какая evidence обязательна.

### 6.17. Waiver

Значение: explicit audited exception allowing transition despite missing/rejected gate evidence.

Почему нужен термин:

Без waiver система либо слишком жесткая, либо слишком слабая. Waiver делает исключение видимым lifecycle fact.

### 6.18. Agent assignment

Значение: audited automation request attached to Initiative.

Почему нужен термин:

Нужно зафиксировать intent, scope, allowed Projects, write paths, budget, required evidence, review gate. Это лучше, чем “попросили агента что-то сделать”.

### 6.19. Agent run

Значение: execution record for Agent assignment.

Почему нужен термин:

Run должен быть не просто transcript, а evidence-producing record: commands, files, outputs, claims, verdict.

### 6.20. Initiative Control Room

Значение: primary cockpit view for one Initiative.

Почему нужен термин:

Нужна surface, которая не сводит lifecycle к task board или graph. Control Room является рабочим экраном принятия решений.

### 6.21. External adapter

Значение: connector to outside systems that imports signals/evidence and performs explicit outbound actions.

Почему нужен термин:

Он защищает source-of-truth boundary. GitHub/CI/Jira важны, но не владеют lifecycle truth.

### 6.22. Typed lifecycle edge

Значение: evented relationship between lifecycle objects.

Почему нужен термин:

Dependencies, blockers, evidence links и impact links должны быть queryable and auditable.

### 6.23. Graph projection

Значение: generated graph view from lifecycle edges/events.

Почему нужен термин:

Graph полезен, но не является authority. Термин предотвращает путаницу между relation facts и visualization.

### 6.24. Thin vertical slice

Значение: smallest end-to-end slice proving the cockpit claim.

Почему нужен термин:

Без этого project risk уходит в широкий shallow build: много UI/integrations, но нет доказанного lifecycle loop.

### 6.25. CLI/API-first slice

Значение: implementation order: core model, CLI, daemon API, then UI.

Почему нужен термин:

Он защищает от UI-first mock и integration-first scope creep.

### 6.26. Self-hosting Initiative

Значение: first dogfood Initiative managing the implementation of the cockpit itself.

Почему нужен термин:

Он превращает architecture discussion в проверяемую практику.

## 7. Связанные изменения в документации и fixture

### 7.1. `CONTEXT.md`

Файл `CONTEXT.md` теперь содержит рабочий glossary для `better-orgasmic`. Он намеренно не является implementation spec. Его функция — стабилизировать language.

Добавлены термины:

- Local-first lifecycle cockpit
- Program
- Program workspace
- Project Link Manifest
- Typed lifecycle edge
- Graph projection
- Project
- Initiative
- Event log
- Typed Org event
- Event schema
- Projection
- Rebuildable projection
- Evidence
- Evidence Pack
- External adapter
- Lifecycle stage
- Health
- Evidence gate
- Waiver
- Agent assignment
- Agent run
- Initiative Control Room
- Thin vertical slice
- Self-hosting Initiative
- CLI/API-first slice

### 7.2. ADR 0001-0017

Созданы 17 ADR:

1. `0001-program-aggregate-over-projects.md` — Program aggregate above Projects.
2. `0002-initiative-as-primary-lifecycle-object.md` — Initiative as primary lifecycle object.
3. `0003-explicit-program-workspace.md` — explicit Program workspace.
4. `0004-event-log-plus-projections.md` — event log plus projections.
5. `0005-initiative-stage-health-gates.md` — stage, health and gates.
6. `0006-agents-as-audited-automation.md` — agents as audited automation.
7. `0007-initiative-control-room.md` — primary view.
8. `0008-evidence-pack.md` — Evidence Pack.
9. `0009-external-adapters-as-signals.md` — external adapters as signals/evidence.
10. `0010-first-thin-vertical-slice.md` — first thin slice.
11. `0011-cli-api-first-implementation-order.md` — CLI/API before UI.
12. `0012-typed-org-event-log.md` — typed Org events.
13. `0013-versioned-events-rebuildable-projections.md` — versioned events and rebuildable projections.
14. `0014-project-link-manifest.md` — Program -> Project links.
15. `0015-typed-lifecycle-edges.md` — dependency relations.
16. `0016-policy-gates-with-waivers.md` — gate policy and waivers.
17. `0017-self-hosting-initiative.md` — self-hosting first Initiative.

### 7.3. Design fixture Program workspace

Создан:

```text
orgasmic-cockpit.program/.orgasmic-program/
```

Содержит:

- `program.org`
- `projects.org`
- `events/2026-07-06.org`
- projections:
  - `initiatives.org`
  - `dependencies.org`
  - `risks.org`
  - `milestones.org`
  - `implementation-issues.org`
  - `test-strategy.org`
  - `cli-api-contract.org`
- evidence:
  - `EV-adr-set.org`
  - `EV-repo-audit.org`
  - `EV-implementation-issues.org`
  - `EV-test-strategy.org`
  - `EV-cli-api-contract.org`
  - `EV-core-parser-validator.org`

Fixture intentionally precedes full implementation. Its purpose is to make future parser, projection generator, CLI, daemon API and UI contract concrete.

### 7.4. Self-hosting Initiative state

Current fixture state:

```text
Program: Orgasmic Cockpit
Initiative: Local-first lifecycle cockpit thin slice
Stage: In Flight
Health: On Track
```

Closed gates:

```text
Discovery -> Planned:
  scope accepted
  architecture ADRs accepted
  affected projects linked

Planned -> In Flight:
  implementation issues created
  test strategy accepted
  first CLI/API contract accepted
```

First two implementation issues marked done:

- `issue_program_workspace_parse`
- `issue_typed_org_event_validation`

## 8. Связанные изменения в upstream code

В `upstream-orgasmic` реализован первый core slice.

### 8.1. Новый module

Добавлен:

```text
crates/orgasmic-core/src/program.rs
```

И экспорт:

```rust
pub mod program;
```

в:

```text
crates/orgasmic-core/src/lib.rs
```

### 8.2. Реализован `ProgramWorkspace`

`ProgramWorkspace::open(root)` обнаруживает Program workspace и проверяет минимальную структуру:

- `.orgasmic-program/`
- `program.org`
- `projects.org`
- `events/`
- `projections/`
- `evidence/`

Также возвращает discovered event/projection/evidence files.

### 8.3. Реализован `ProgramEvent`

Validator извлекает typed top-level `EVENT` headings и читает:

- `ID`
- `TYPE`
- `TIME`
- `ACTOR`
- `PROGRAM_ID`
- `SCHEMA`
- additional properties
- JSON payload block

Поддержан allowlist первого slice:

- `program.initialized`
- `project.linked`
- `initiative.created`
- `evidence_pack.created`
- `initiative.stage_transitioned`

### 8.4. Реализованы проверки event validity

Проверяется:

- missing required property;
- unknown event type;
- invalid RFC3339 timestamp;
- invalid schema number;
- missing JSON payload;
- malformed JSON;
- event time regression;
- Project linked before Program;
- Initiative created before Program;
- affected Project references;
- Evidence Pack references unknown Initiative;
- stage transition references unknown Initiative;
- stage transition references unknown Evidence Pack.

### 8.5. Тесты

Добавлен:

```text
crates/orgasmic-core/tests/program_workspace.rs
```

Покрытие:

- workspace discovery;
- parsing of valid typed event;
- valid lifecycle chain with gate evidence;
- forward Initiative reference rejection;
- timestamp regression rejection;
- missing property rejection;
- unknown event type rejection;
- invalid timestamp rejection;
- malformed JSON rejection;
- unknown gate evidence rejection.

### 8.6. Verification

Выполнено:

```text
rustfmt --edition 2021 --check crates/orgasmic-core/src/lib.rs crates/orgasmic-core/src/program.rs crates/orgasmic-core/tests/program_workspace.rs
cargo test -p orgasmic-core --test program_workspace
cargo test -p orgasmic-core
cargo test --workspace -- --test-threads=1
```

Результаты:

- touched-file rustfmt passed;
- new Program test target passed: 10/10;
- `orgasmic-core` passed;
- full workspace serial tests passed.

Ограничение:

`cargo clippy -p orgasmic-core --all-targets -- -D warnings` все еще падает на трех pre-existing diagnostics:

- `crates/orgasmic-core/src/id_repair.rs`
- `crates/orgasmic-core/src/marker.rs`
- `crates/orgasmic-core/src/org.rs`

Новых clippy errors в `program.rs` в observed report не было.

Также обычный parallel `cargo test --workspace` показал pre-existing/order-sensitive failure в `orgasmic-cli --test dispatch`: `dispatch_close_prunes_stem_dir_leaving_brief` падает в parallel target run, но проходит одиночно и проходит при `--test-threads=1`. Это зафиксировано как limitation, не как следствие Program parser.

## 9. Что это меняет в первоначальной спецификации

### 9.1. Product baseline расширяется, но не отменяется

Исходное:

```text
per-developer local coordination app
```

Новое:

```text
local-first lifecycle cockpit
```

Это не означает переход к SaaS. Это означает, что локальный продукт должен управлять не только tasks/runs, но и Initiative lifecycle.

### 9.2. Board перестает быть главным cross-project ответом

Board остается, но Program/Initiative становятся primary cross-project model. Board — execution surface, не lifecycle source.

### 9.3. Graph становится projection, не authority

Graph важен, но dependency truth должна жить в typed lifecycle edges/events.

### 9.4. Task остается execution unit

Task не удаляется. Но Task больше не главный объект жизненного цикла. Это снижает риск “local Jira with agents”.

### 9.5. Evidence становится обязательной частью lifecycle

Release readiness, review readiness и stage transitions не должны выводиться из task statuses. Они должны ссылаться на Evidence Packs.

### 9.6. Agent execution становится audit-centric

Workers/runs остаются, но должны постепенно встраиваться в Agent Assignment / Agent Run / Evidence Pack model.

### 9.7. CLI/API должны получить Program endpoints before UI

Для первого slice UI не должен быть главным началом. Нужно сначала доказать:

- Program workspace parse/write;
- event append;
- projection rebuild;
- Evidence Pack creation;
- stage transition policy.

## 10. Следующие инженерные шаги

Следующий implementation issue:

```text
issue_rebuild_initiative_projection
```

Минимальная цель:

- читать typed Program events;
- строить Initiative projection;
- регенерировать `projections/initiatives.org`;
- иметь deterministic golden test.

После этого:

1. CLI `program init` / `program link-project`.
2. CLI `initiative create` / `initiative advance`.
3. CLI `evidence add-command`.
4. Daemon API read model for Initiative Control Room.
5. Daemon API append event + rebuild projection.
6. Minimal UI Control Room read view.

Параллельно, но не раньше core loop:

- зафиксировать formal event schemas;
- решить exact projection header format;
- решить lock/serialization policy for Program workspace writes;
- решить как Program workspace будет coexist с Project `.orgasmic/` conventions;
- очистить pre-existing clippy diagnostics, если clippy станет required gate.

## 11. Основной риск после grilling-session

Главный риск теперь не в терминологии. Терминология достаточно четкая.

Главный риск — построить слишком много lateral surfaces до того, как будет доказан core loop:

```text
event -> projection -> evidence -> gate -> transition -> Control Room
```

Если снова начать с UI dashboard, GitHub/Jira adapter, graph layout или artifact renderer, продукт вернется к старой ловушке: много полезных поверхностей, но нет lifecycle authority.

Правильный инженерный критерий следующей фазы:

```text
Можно ли локально создать Program, связать Project, создать Initiative,
добавить Evidence Pack, пройти gate, пересобрать projection и увидеть
это как единое состояние?
```

Пока ответ не “да”, остальное является вторичным.
