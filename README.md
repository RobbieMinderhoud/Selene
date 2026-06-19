# Selene

A modern desktop **SQL editor** (Rust + Tauri), Microsoft SQL Server first.
**v0.1 — in progress** (data layer + IPC done; UI being built).

**Stack:** Tauri 2 · React + TS + Vite · `tiberius` (MSSQL) · `keyring`.
Rust does all DB/query/export logic; the web layer is presentation only.

## Quick start

Prereqs: Rust (stable), Node 20+, pnpm, [`just`](https://github.com/casey/just).

```sh
pnpm install
just dev          # Vite dev server + Tauri, hot reload
```

## Commands

```
just dev               run the app (hot reload)
just build             production desktop bundle
just check             cargo check --workspace
just test              all tests (Rust + frontend)
just test-core         core unit tests
just test-integration  dockerized-MSSQL tests (needs Docker)
just lint              clippy (warnings = errors)
just format            rustfmt (+ prettier)
just version 0.2.0     bump + sync version across manifests
```

## Layout

- `crates/selene-core/` — UI-agnostic data layer: drivers, query/export, SQL guard, keychain (no Tauri dep).
- `src-tauri/` — thin Tauri IPC + state adapter.
- `src/` — React frontend.

## A SQL Server to develop against

The app binds no ports — point it at any `host:port`. For a throwaway server, avoid 1433 if it's busy:

```sh
docker run -d --name selene-mssql-dev -e ACCEPT_EULA=Y \
  -e MSSQL_SA_PASSWORD='<fictitious-strong-pw>' -p 14333:1433 \
  mcr.microsoft.com/mssql/server   # then connect to localhost:14333
```

## More

Architecture, the full IPC contract, conventions, and security notes live in
[`CLAUDE.md`](./CLAUDE.md). Functional changes: [`CHANGELOG.md`](./CHANGELOG.md).
