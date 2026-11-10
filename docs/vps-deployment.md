# VPS deployment

MemoryOS runs on the GateOS VPS for agents on that machine, such as AgentOS. It is private: the
server listens on `127.0.0.1:8280` only, Postgres is not published, and there is no domain or nginx
site. `.github/workflows/deploy.yml` deploys every merge to main: it builds
`ghcr.io/feryadialoi/memoryos:<sha>`, SSHes in as `deploy` and runs `deploy/docker-compose.yml` from
the checkout in `/home/deploy/memoryos`. Nothing here needs sudo.

## One-time setup

The `deploy` user, its SSH key (`~/.ssh/gateos_deploy` on the Mac) and Docker come from
[GateOS's VPS setup](https://github.com/feryadialoi/gateos/blob/main/docs/vps-deployment.md).

```bash
ssh -i ~/.ssh/gateos_deploy deploy@ssh.feryadi.dev
git clone https://github.com/feryadialoi/memoryos.git ~/memoryos
cat > ~/memoryos/deploy/.env <<ENV
POSTGRES_PASSWORD=$(openssl rand -hex 32)
MEMORYOS_PUBLISH=127.0.0.1:8280
MEMORYOS_RERANK_MODEL=jina-reranker-v1-turbo-en
MEMORYOS_MEM_LIMIT=1536m
ENV
chmod 600 ~/memoryos/deploy/.env
```

GitHub secrets on this repository: `VPS_HOST` = `ssh.feryadi.dev` (the DNS record Cloudflare does
not proxy), `VPS_USER` = `deploy`, `VPS_SSH_KEY` = the whole `~/.ssh/gateos_deploy`. No registry
token is stored: the deploy job hands the VPS its own short-lived `GITHUB_TOKEN`.

`jina-reranker-v1-turbo-en` keeps memory low: on a Mac the server measured 976 MB with it, 1531 MB
with `bge-reranker-base` and 820 MB with no reranker. It answered 13 of the 15 bench questions; the
two misses were a Chinese question and "what is my nickname?". Not yet measured in the Linux
container. Change the line and deploy again to switch.

## Keys for an agent

```bash
cd ~/memoryos
docker compose -f deploy/docker-compose.yml exec memoryos memoryos key create \
  --tenant agentos --name agentos --scopes 'agent:agentos.*' --max-trust agent
```

It prints the token once. A process on the VPS reaches the server at `http://127.0.0.1:8280`. A
container reaches it by joining the `memoryos_default` network and using `http://memoryos:8080`,
since the host's loopback is not visible from inside a container.

## Operating it

```bash
cd ~/memoryos
docker compose -f deploy/docker-compose.yml ps
docker compose -f deploy/docker-compose.yml logs -f memoryos
curl -s http://127.0.0.1:8280/readyz
```

Backups and incidents follow [`runbook.md`](runbook.md) with this Compose file.
