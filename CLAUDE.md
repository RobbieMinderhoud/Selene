# CLAUDE.md — Selene

Guidance for AI agents (and humans) working in this repository.

## What Selene is

Selene is a modern, sleek, cross-platform **desktop SQL editor** (in the spirit of
Beekeeper Studio), built in **Rust + Tauri**. It targets **Microsoft SQL Server**
first but is architected so additional drivers (PostgreSQL/MySQL/SQLite via sqlx)
slot in later without rework. macOS-first; Windows/Linux kept in mind.

> Naming: the product is **Selene**. The repo/folder is still named `SELECT`
> (the original codename) and will be renamed later. Crates are already `selene-*`.
> Bundle id placeholder: `com.selene.app`.

## Status — v0.1 (in progress)

- ✅ **`selene-core`** data layer: driver abstraction, MSSQL driver (tiberius),
  value conversion, streaming execution, schema introspection, OS-keychain secret
  storage, CSV/JSON/XLSX exporters, a streaming CSV importer (coerce/infer +
  bound-parameter insert / create-table), and a SQL safety guard.
- ✅ **Tauri IPC layer** (`src-tauri`): 16 commands, streaming result channel,
  app state, dialog + log plugins, locked-down CSP.
- ✅ **Frontend** (`src/`): the real UI — connection sidebar + connect dialog,
  lazy/virtualized schema tree, multi-tab CodeMirror 6 editor (MSSQL dialect,
  Cmd/Ctrl+Enter run), virtualized results grid with multi-result-set sub-tabs +
  status bar, the guard confirm/block flow, and CSV/JSON/XLSX export with a
  progress toast. Dark-theme-first with a light toggle (crossfaded via the View
  Transitions API); Zustand (UI/stream) + TanStack Query (cached reads); typed
  IPC wrappers over all 14 commands. A token-driven **motion system** animates
  the whole UI (modals, toasts, tabs, schema tree, results, status) with a
  global `prefers-reduced-motion` guard — see _Motion & animation_ below.
- ✅ Integration tests (testcontainers MSSQL): `crates/selene-core/tests/mssql_integration.rs`
  exercises the public driver API against a real SQL Server (connect/test_connection,
  typed scalar mapping, batched streaming + ordering, `max_rows` truncation, multiple
  result sets, cooperative cancellation, introspection, CSV export). All `#[ignore]`-d;
  run via `just test-integration` (Docker required). Frontend tests run on Vitest:
  unit tests for the pure pieces (CellValue formatting, IPC error narrowing) plus
  component/integration tests in jsdom + React Testing Library (editor-store result
  reducers, the `runQuery` guard→stream orchestration with a mocked `Channel`,
  ConnectionDialog incl. the password-handling contract, the lazy SchemaTree, and
  the guard/toast UI). Real Tauri WebDriver E2E is still deferred (to land with CI).

## Tech stack

| Concern                | Choice                                                     |
| ---------------------- | ---------------------------------------------------------- |
| Shell / UI             | Tauri 2 + React + TypeScript + Vite                        |
| MSSQL driver           | `tiberius` (Tokio, `tokio-util` compat, `rustls` TLS, on by default) |
| Future drivers         | `sqlx` (pg/mysql/sqlite) behind the same trait             |
| Credentials            | `keyring` (OS keychain)                                    |
| Export                 | `csv`, `serde_json`, `rust_xlsxwriter`                     |
| SQL editor             | CodeMirror 6 + `@codemirror/lang-sql` (MSSQL dialect)      |
| Results grid           | TanStack Table + `@tanstack/react-virtual` (MIT)\*         |
| FE state               | Zustand (UI/stream) + TanStack Query (cached reads)        |
| Motion                 | In-house CSS tokens + `usePresence` hook (no animation lib)\*\* |

\* **Grid: TanStack, not Glide.** Glide Data Grid 6.x peer-deps cap at React 18
(`^16.12.0 || 17.x || 18.x`) and this app is on React 19, so it does not
install/run cleanly. TanStack Table (column model) + `@tanstack/react-virtual`
(row/column windowing) are both MIT and React-19-safe, keep the bundle lean (no
canvas/lodash deps), and virtualize cleanly for large streamed result sets. The
body cells are read straight from the in-place-mutated row buffer rather than
from the table's row model (which memoizes on the `data` reference and would go
stale on append). Revisit Glide if/when it ships React 19 support.

\*\* **Motion: in-house, not Framer Motion.** Animation is CSS transitions +
keyframes driven by the `--dur-*` / `--ease-*` tokens, plus one ~40-line
`usePresence` hook (`src/lib/motion.ts`) for animating elements out before they
unmount. This adds **zero** bundle weight (vs ~50 kB+ for `motion`), matches the
existing CSS-Modules-+-tokens architecture, and stays React-19-safe. Reach for a
library only if a future feature genuinely needs layout/FLIP or shared-element
transitions that CSS can't express — and weigh the bundle cost first.

## Architecture

A Cargo **workspace**. `selene-core` holds all DB/query/export logic and has
**zero Tauri dependency** (so it's plain-`cargo test`-able and reusable by a
future CLI). `src-tauri` is a thin IPC/state adapter. `src/` is the React app.

```
SELECT/
├─ Cargo.toml                  # [workspace]; [workspace.package] version is the source of truth
├─ justfile                    # dev/build/test/lint/format/version recipes
├─ scripts/sync-version.sh     # propagates the version to tauri.conf.json + package.json
├─ crates/selene-core/src/
│  ├─ value.rs                 # CellValue, Column, LogicalType, TemporalKind (driver-neutral)
│  ├─ connection_spec.rs       # ConnectionSpec, AuthMethod (open enum), DriverId, TlsConfig
│  ├─ capabilities.rs          # DriverCapabilities (UI feature gating)
│  ├─ error.rs                 # CoreError (redaction-safe)
│  ├─ secret.rs                # Secret (redacts Debug, zeroizes on drop)
│  ├─ driver/                  # DatabaseDriver/Connection/RowSink traits + driver_for()
│  │  └─ mssql/                # tiberius impl: config, convert, stream, introspect, error
│  ├─ introspect.rs            # DatabaseInfo/SchemaInfo/TableInfo/ColumnInfo
│  ├─ export/                  # Exporter (RowSink) → csv/json/xlsx
│  ├─ import/                  # CsvRowSource (RowSource) + coerce/infer ← csv
│  ├─ guard/                   # SQL safety classifier (classify → GuardVerdict)
│  └─ secrets/                 # keyring wrapper (KeychainStore)
├─ src-tauri/src/
│  ├─ lib.rs                   # Tauri builder, plugins, AppState, command registration
│  ├─ state.rs                 # AppState: ConnectionStore + sessions + running queries
│  ├─ error.rs                 # IpcError (serde) ← CoreError, sanitized
│  └─ commands/{connection,session,introspect,query,export,import}.rs
└─ src/                        # React + Vite frontend (presentation only)
```

### Driver abstraction (the core contract)

`crates/selene-core/src/driver/mod.rs` defines `DatabaseDriver`, `Connection`,
and `RowSink` (`#[async_trait]`). Dispatch is dynamic (`Box<dyn Connection>`);
Cargo **features** gate which drivers compile (`default = ["mssql"]`). Adding a
backend = new `driver/<name>/` module + a feature + a `DriverId` variant + a
registration arm in `driver_for()`. **No IPC or frontend change needed.**

- `CellValue` is driver-neutral; **decimals are strings** (no precision loss);
  unknown types become `CellValue::Unsupported` (lossless).
- Cancellation is **cooperative** (`CancelToken`, checked between row batches).

### Tauri IPC contract

Commands return `Result<T, IpcError>` where `IpcError = { message, kind }`
(sanitized; never contains secrets). Tauri 2 maps camelCase JS argument keys to
the snake_case Rust parameters.

| Command              | Args                                                                           | Returns                                           |
| -------------------- | ------------------------------------------------------------------------------ | ------------------------------------------------- |
| `connections_list`   | –                                                                              | `ConnectionSpec[]`                                |
| `connection_save`    | `{ spec, password? }`                                                          | `ConnectionSpec`                                  |
| `connection_delete`  | `{ id }`                                                                       | –                                                 |
| `connection_test`    | `{ spec, password? }`                                                          | `TestReport`                                      |
| `session_connect`    | `{ connectionId, password? }`                                                  | `SessionInfo { sessionId, driver, capabilities }` |
| `session_disconnect` | `{ sessionId }`                                                                | –                                                 |
| `databases_list`     | `{ sessionId }`                                                                | `DatabaseInfo[]`                                  |
| `schemas_list`       | `{ sessionId, database }`                                                      | `SchemaInfo[]`                                    |
| `tables_list`        | `{ sessionId, database, schema }`                                              | `TableInfo[]`                                     |
| `columns_list`       | `{ sessionId, database, schema, table }`                                       | `ColumnInfo[]`                                    |
| `guard_check`        | `{ sql, readOnly }`                                                            | `GuardVerdict`                                    |
| `query_run`          | `{ sessionId, sql, maxRows?, onEvent: Channel<QueryEvent> }`                   | `{ queryId }`                                     |
| `query_cancel`       | `{ queryId }`                                                                  | –                                                 |
| `export_result`      | `{ sessionId, sql, format, path, maxRows?, onProgress: Channel<ExportEvent> }` | `ExportSummary`                                   |
| `import_csv_analyze` | `{ path, options? }`                                                           | `CsvAnalysis` (headers, sampleRows, inferred[])   |
| `import_csv`         | `{ sessionId, path, target, mapping, options, onProgress: Channel<ImportEvent> }` | `ImportSummary`                                |

**`query_run` streams over a `tauri::ipc::Channel<QueryEvent>`.** `QueryEvent` is
tagged by a `kind` field with **camelCase** fields:
`{kind:"started", queryId}` → (`{kind:"meta", setIndex, columns}`,
`{kind:"rows", setIndex, rows}`\*, `{kind:"setEnd", setIndex, affected}`)+ →
`{kind:"finished", outcome, elapsedMs}`, or terminal `{kind:"cancelled"}` /
`{kind:"failed", message}`. `ExportEvent`: `{kind:"progress", rows}`,
`{kind:"done", rows}`, `{kind:"failed", message}`. `ImportEvent`:
`{kind:"progress", rows}`, `{kind:"done", inserted, skipped}`,
`{kind:"failed", message}`.

**CSV import** (`import_csv`) inserts via **multi-row bound-parameter** `INSERT`s
(never spliced cell values), sub-batched to stay under SQL Server's 2100-param
limit, and wraps everything (incl. an "import as new table" `CREATE TABLE`) in a
transaction when `options.atomic`. It is refused on a read-only connection. The
parse/coerce/infer logic is DB-agnostic in `selene-core/src/import/`
(`CsvRowSource` is a `RowSource` — the inverse of `export`'s `Exporter: RowSink`);
the write path is `Connection::{create_table, import_rows}`.
`import_csv`/`import_csv_analyze` arg sub-objects (`target`, `mapping`,
`options`) use **camelCase** fields (`csvIndex`, `targetColumn`, `sqlType`,
`hasHeader`, …); `ImportSummary { rows_inserted, rows_skipped }` stays snake_case.

> ⚠️ **Field-casing nuance for the frontend:** the IPC _envelope/event_ types use
> camelCase (`queryId`, `setIndex`, `elapsedMs`), but the embedded **core domain
> types keep snake_case** field names: `Column { name, ordinal, db_type, logical,
nullable }`, `CellValue` is adjacently tagged `{ "t": …, "v": … }`,
> `ConnectionSpec { …, read_only, … }`, `DatabaseInfo { name, is_system }`,
> `ColumnInfo { …, data_type, is_primary_key, max_length }`, etc. Match these
> exactly in hand-written TS types. (ts-rs auto-generation is deferred to a later
> phase — see Roadmap.)

`query_run` enforces the SQL guard server-side: a `Block` verdict (e.g. a
non-SELECT on a read-only connection) is refused before execution.

## Build, run, test

Prefer `just` (run `just` to list recipes). The Tauri CLI is provided by the
local `@tauri-apps/cli` dev-dependency, so use `pnpm tauri …` (no global install).

```
just dev            # run the app (Vite + Tauri, hot reload)
just build          # production bundle
just check          # cargo check --workspace
just test           # cargo test --workspace (+ frontend tests when present)
just test-core      # selene-core unit tests
just test-integration  # dockerized-MSSQL integration tests (Docker required)
just lint           # clippy -D warnings (+ frontend lint)
just format         # rustfmt (+ prettier)
just version 0.2.0  # bump + sync version across all manifests
just version-check  # verify versions are in sync
```

### Local MSSQL for development

The app binds **no ports** (it's a client) — point it at any host:port. For tests
and manual checks, do **not** assume port 1433 is free:

- Integration tests use **testcontainers** → ephemeral host ports, no conflict.
- For a throwaway server, map a non-default port, e.g.
  `docker run -d --name selene-mssql-dev -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD=<fictitious-strong-pw> -p 14333:1433 mcr.microsoft.com/mssql/server`,
  then connect to `localhost:14333`.

### macOS keychain prompt during development

`just dev` ad-hoc-signs the binary, and that signature changes on every rebuild,
so macOS treats each run as a new app and re-shows the _"Selene wants to use your
confidential information stored in com.selene.app"_ keychain prompt — "Always
Allow" can't stick because there's no stable signing identity to remember. This
is a **dev-only artifact**: a signed/notarized release (see Roadmap) reads its
own keychain items silently, so end users never see it.

- **`just dev-signed`** builds and launches a debug `.app` signed with a _stable_
  self-signed certificate (default name `Selene Dev`, set in
  `src-tauri/tauri.dev-signed.conf.json`, applied only via that recipe's
  `--config` so normal `tauri build` is unaffected). Because the signing identity
  is constant across rebuilds, a single "Always Allow" persists. One-time setup:
  create a self-signed _Code Signing_ certificate named `Selene Dev` via Keychain
  Access → Certificate Assistant → Create a Certificate. Tradeoff: it runs a
  bundle, so there's **no hot reload** — keep `just dev` for fast iteration.
- Within a single run, only the **first** connect (or test) of a connection reads
  the keychain; the password is then cached in-process
  (`AppState::secret_cache`), so reconnecting it neither re-reads the keychain nor
  re-prompts. The cache is updated on `connection_save` and cleared on
  `connection_delete` (see `state.rs`).

## Conventions

- **Rust:** `#![forbid(unsafe_code)]` in every crate; `cargo clippy -- -D warnings`
  must pass; `cargo fmt`. Idiomatic, comment the _why_ of non-obvious logic.
- **Motion & animation (always-on):** Selene should feel fluid, not static.
  Every new UI surface ships _with_ motion as part of the work — it is not
  optional polish to add "later". Treat a feature with no transitions the way
  you'd treat one with no styling.
  - **Use the tokens, never raw values.** Timing comes from `--dur-1..4` and the
    `--ease-*` curves in `src/styles/tokens.css`. Don't hard-code `0.2s ease` in
    a component. If you need a new curve/duration, add a token.
  - **Animate out, not just in.** Anything that mounts/unmounts (modals, toasts,
    menus, popovers) must animate _both_ directions. Use the `usePresence` hook
    in `src/lib/motion.ts`: it keeps the node mounted across its exit, and you
    drive the CSS with `data-state="open" | "closed"` (see `Modal`, `Toasts`).
  - **State, hover, and focus transition** — don't snap. Add `transition` on the
    specific properties that change (background, color, border, transform). The
    base `button`/`input` styles already do this; match that bar.
  - **Performance: animate only `transform` and `opacity`** (GPU-cheap). Never
    transition layout properties (width/height/top/left/margin) in hot paths, and
    **never** add a per-row mount animation in the virtualized results grid — it
    replays on every scroll recycle. Animate the _container_ once instead (see
    `ResultsGrid` `.scroll`). The theme crossfade (colour) is the one sanctioned
    exception and is handled via the View Transitions API in `themeStore`.
  - **Respect reduced motion.** A global `@media (prefers-reduced-motion: reduce)`
    guard in `global.css` neutralizes CSS animation/transition durations; any
    JS-driven timing (exit delays) must honour `prefersReducedMotion()` the way
    `usePresence` and the toast store do. Motion must never be the _only_ carrier
    of meaning. Keep `--dur-*` (CSS) and `MOTION` (JS, ms) in sync.
  - **Taste:** subtle and quick (~90–360 ms). `ease-out` for entrances, `ease-in`
    for exits, `ease-spring` sparingly for "pops". This is a precision tool — read
    as responsive, not showy.
- **Security (always-on):**
  - Passwords/tokens live **only** in the OS keychain via `keyring`; never on disk,
    never in logs. Use the `Secret` newtype (redacted `Debug`, zeroized on drop).
  - **Never log** SQL text or row/cell data above `DEBUG`/`TRACE`, and never log
    secrets. IPC errors are sanitized in `src-tauri/src/error.rs`.
  - TLS is on by default; `trust_server_certificate` is an explicit per-connection
    opt-in. SQL identifiers spliced into introspection are bracket-quoted; all
    other user values are bound parameters.
  - tiberius uses the **`rustls`** TLS backend (not `native-tls`): on macOS
    (`aarch64-apple-darwin`) the native-tls/Secure Transport backend fails the TLS
    handshake against SQL Server's self-signed certificate (`connection closed via
    error`), which breaks the macOS-first desktop target. `rustls` connects across
    macOS/Windows/Linux and is exercised by `crates/selene-core/tests/mssql_integration.rs`.
    Do not switch back to native-tls without re-validating on macOS.
  - The SQL guard + per-connection read-only mode are defence-in-depth, not a
    substitute for least-privilege DB logins.
  - In tests/fixtures use **only fictitious** sample data — no real credentials,
    customer data, or personal data.
- **Settings discoverability (always-on):** When implementing any new feature
  that introduces user-visible behaviour — defaults, formatting, toggles,
  thresholds, density, shortcuts — ALWAYS ask the user (via `ask-user`)
  whether the feature should be exposed in `SettingsModal` before finishing.
  If yes, add the field to `src/state/settingsStore.ts` (with a sensible
  default in `DEFAULTS`), wire it into the feature's read site via a
  `useSettingsStore` selector, and add a control to the appropriate section
  of `SettingsModal`. The store is localStorage-backed and deep-merges over
  `DEFAULTS` on load, so adding a new field never breaks existing users. If
  no, briefly note why in the PR/commit message (e.g. "fixed behaviour, not
  a preference"). Do not silently add hardcoded constants that a user might
  reasonably want to change.

## Versioning

- **SemVer.** The `[workspace.package]` version in the root `Cargo.toml` is the
  single source of truth; `scripts/sync-version.sh` propagates it to
  `tauri.conf.json` and `package.json` (run `just version-check` to detect drift).
- **`CHANGELOG.md`** follows _Keep a Changelog_ and lists **only functional
  (user-facing) changes**.
- **Deferred until the project is under git:** Conventional Commits, git-cliff
  changelog automation, pre-commit hooks, CI (GitLab), signed/notarized releases,
  and auto-update.

## Roadmap (high level)

v0.2 editor/results polish + Windows/Linux • v0.3 sqlx drivers (pg/mysql/sqlite) +
schema-aware autocomplete + ts-rs type generation • v0.4 transactions + query
plans • v0.5 in-grid data editing + import • v0.6 SSH tunnel + Windows/Entra auth •
v1.0 hardening. (See the full plan for details.)
