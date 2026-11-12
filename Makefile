# MemoryOS on macOS. Pick one way to run it and use only its commands.
#
# Local: Homebrew Postgres 18 + pgvector on localhost:6432 and `memoryos serve`, no Docker.
#   make local-setup     once per machine: binary, Postgres, database
#   make local-import    once, copies the data and keys of a Docker install (Docker must be running)
#   make local-start     starts Postgres and the server; also after a reboot
#   make local-upgrade   after pulling new code
#
# Docker: Postgres and the server in containers, as `memoryos setup` creates them.
#   make docker-setup    once per machine: binary, containers, keys, Claude Code hooks and MCP
#   make docker-upgrade  after pulling new code
#
# Local Postgres listens on localhost only and trusts local connections, so it needs no password.
# `memoryos serve` reads ~/.config/memoryos/server.env; `memoryos reranker` changes its reranker lines.

PG := $(shell brew --prefix postgresql@18 2>/dev/null)/bin
DIR := $(HOME)/.local/share/memoryos
PGDATA := $(DIR)/postgres
PORT ?= 6432
BIN := $(HOME)/.cargo/bin/memoryos
PSQL_ARGS := -h localhost -p $(PORT) -U memoryos
COMPOSE := docker compose -f $(HOME)/.config/memoryos/docker-compose.yml
SERVER_ENV := $(HOME)/.config/memoryos/server.env

export DATABASE_URL := postgres://memoryos@localhost:$(PORT)/memoryos
export MEMORYOS_BIND ?= 127.0.0.1:6366
export RUST_LOG ?= info,opentelemetry=warn,opentelemetry_sdk=warn,hyper=warn

.PHONY: help install local-setup local-import local-start local-db local-upgrade docker-setup docker-upgrade

help:
	@sed -n '1,13p' Makefile

# --- Local ---------------------------------------------------------------------------------------

local-setup: install
	brew install postgresql@18 pgvector
	mkdir -p $(DIR)
	$(PG)/initdb -D $(PGDATA) -U memoryos --auth=trust -E UTF8 --locale=C
	$(MAKE) local-db
	$(PG)/createdb $(PSQL_ARGS) memoryos
	mkdir -p $(dir $(SERVER_ENV))
	test -f $(SERVER_ENV) || printf 'DATABASE_URL=%s\nMEMORYOS_BIND=%s\nRUST_LOG=%s\n' \
		"$(DATABASE_URL)" "$(MEMORYOS_BIND)" "$(RUST_LOG)" > $(SERVER_ENV)
	$(BIN) migrate
	cargo clean

# Replaces the local database with the Docker one, so run it before `make local-start`.
local-import: local-db
	-docker stop memoryos-memoryos-1
	docker start memoryos-postgres-1
	until docker exec memoryos-postgres-1 pg_isready -q -U memoryos; do sleep 1; done
	$(PG)/dropdb $(PSQL_ARGS) --if-exists memoryos
	$(PG)/createdb $(PSQL_ARGS) memoryos
	$(BIN) migrate
	docker exec memoryos-postgres-1 pg_dump -U memoryos -Fc --data-only --exclude-table-data=_sqlx_migrations memoryos | \
		$(PG)/pg_restore $(PSQL_ARGS) -d memoryos --data-only --disable-triggers --exit-on-error
	docker stop memoryos-postgres-1
	$(PG)/psql $(PSQL_ARGS) -d memoryos -tAc "SELECT count(*) || ' facts copied' FROM facts"

# Starts the server, or restarts it when it is already running.
local-start: local-db
	$(BIN) restart

# Postgres refuses to start without a locale ("postmaster became multithreaded during startup"), which a
# shell started by launchd or cron may not have.
local-db:
	$(PG)/pg_ctl -D $(PGDATA) status >/dev/null || LC_ALL=C $(PG)/pg_ctl -D $(PGDATA) -o "-p $(PORT)" -l $(DIR)/postgres.log start

local-upgrade: install local-db
	$(BIN) migrate
	$(MAKE) local-start
	cargo clean

# --- Docker --------------------------------------------------------------------------------------

docker-setup: install
	$(BIN) setup --agent claude-code --global
	cargo clean

docker-upgrade: install
	$(COMPOSE) up -d --build --wait memoryos
	cargo clean

# --- Both ----------------------------------------------------------------------------------------

# The `memoryos` command the hooks, the MCP proxy and the server run. Copied, not symlinked, because
# a link into target/ would break on `cargo clean`.
install:
	cargo install --locked --force --path crates/server
