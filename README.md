# HyperCompute

Système de calcul distribué écrit en Rust. Un serveur central reçoit des tâches, les distribue aux machines du réseau selon un algorithme de scoring, et renvoie les résultats au client.

À partir de la version MPI, HyperCompute supporte les jobs parallèles distribués sur plusieurs nœuds simultanément, avec deux modes d'exécution : via le système OpenMPI/MPICH (`mpirun`), ou en mode pur sans dépendance système (chaque nœud lance son propre rang via des variables d'environnement).

```
  Client A ──┐
  Client B ──┼──► HyperCompute Server :7700 ──► Worker Node 1  rank 0  (score: 0.91)
  Client C ──┘         WebSocket + REST      ──► Worker Node 2  rank 1  (score: 0.87)
                                             ──► Worker Node 3  rank 2  (score: 0.74)
                                              └─ résultats agrégés par rank
```

---

## Table des matières

1. [Prérequis](#prérequis)
2. [Structure du projet](#structure-du-projet)
3. [Démarrage rapide](#démarrage-rapide)
4. [Commande `worker`](#commande-worker)
5. [Commande `submit`](#commande-submit)
   - [submit shell](#submit-shell)
   - [submit fetch](#submit-fetch)
   - [submit mpi — mode pur](#submit-mpi--mode-pur)
   - [submit mpi — mode mpirun](#submit-mpi--mode-mpirun)
6. [Commande `status`](#commande-status)
7. [Commande `nodes`](#commande-nodes)
8. [Commande `info`](#commande-info)
9. [Options du serveur](#options-du-serveur)
10. [Variables d'environnement MPI injectées](#variables-denvironnement-mpi-injectées)
11. [Algorithme de scoring](#algorithme-de-scoring)
12. [API REST](#api-rest)
13. [Protocole WebSocket](#protocole-websocket)
14. [Cycle de vie d'un job MPI](#cycle-de-vie-dun-job-mpi)
15. [Tolérance aux pannes](#tolérance-aux-pannes)
16. [Installation permanente](#installation-permanente)
17. [Scénario multi-machines avec MPI](#scénario-multi-machines-avec-mpi)

---

## Prérequis

- **Rust ≥ 1.75** ([installer](https://rustup.rs))

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustc --version   # rustc 1.75.0 ou supérieur
```

Pour les jobs MPI en mode `--mpirun` uniquement (optionnel) :

```bash
# Debian / Ubuntu
sudo apt install openmpi-bin libopenmpi-dev

# Fedora / RHEL
sudo dnf install openmpi openmpi-devel

# Arch
sudo pacman -S openmpi

# Vérifier
mpirun --version
```

> Le mode `--mpirun` n'est pas nécessaire pour utiliser MPI dans HyperCompute.
> Le mode pur (défaut) ne requiert aucune dépendance système supplémentaire.

---

## Structure du projet

```
hypercompute/
├── Cargo.toml                         # workspace Rust
│
├── hypercompute-proto/                # types partagés entre server et CLI
│   └── src/lib.rs
│       ├── TaskKind        (Shell, HttpFetch, Custom, Mpi)
│       ├── MpiSlot         (rank, size, peers, job_id)
│       ├── MpiPeer         (host, port, node_id)
│       ├── TaskStatus      (Queued → MpiDispatched → MpiCompleted …)
│       ├── WorkerMessage   (Register, Heartbeat, TaskResult, MpiRankResult …)
│       └── ServerMessage   (DispatchTask, DispatchMpiSlot, AbortMpiJob …)
│
├── hypercompute-server/               # coordinateur central
│   └── src/
│       ├── main.rs                    # point d'entrée, args, spawn des tâches de fond
│       ├── state.rs                   # AppState : nodes, queues, mpi_jobs (DashMap)
│       ├── scheduler.rs               # scoring, dispatch standard et MPI multi-nœuds
│       │                              #   recruit_mpi_nodes() — sélection des np meilleurs
│       │                              #   dispatch_mpi_job()  — envoi des slots par rank
│       │                              #   MpiJobState         — agrégation des résultats
│       ├── ws_handler.rs              # connexions WebSocket, traitement MpiRankResult
│       └── router.rs                  # API REST (axum)
│
└── hypercompute-cli/                  # binaire `hc`
    └── src/
        ├── main.rs                    # sous-commandes : worker, submit, status, nodes, info
        ├── worker.rs                  # mode worker : registration, heartbeat, slots MPI
        ├── executor.rs                # exécution Shell / HTTP / Custom / MPI
        │                              #   execute_mpi_slot()  — dispatch selon use_mpirun
        │                              #   execute_mpirun()    — rank-0 lance mpirun
        │                              #   execute_mpi_pure()  — chaque rang local avec env vars
        └── submitter.rs               # submit (shell/fetch/mpi), status, nodes, info
```

---

## Démarrage rapide

**Terminal 1 — serveur :**
```bash
cargo run -p hypercompute-server
```

**Terminal 2 — worker :**
```bash
cargo run -p hypercompute-cli -- worker
```

**Terminal 3 — soumettre une tâche shell :**
```bash
cargo run -p hypercompute-cli -- submit --wait shell -- echo "hello world"
```

Sortie attendue :
```
Task submitted: 3bd8305d-6952-4f69-b3be-0813d160ff6e
Status: Queued
Waiting for task 3bd8305d-...
✓ Completed in 8ms (node: a987596b-...)
--- stdout ---
hello world
Exit code: 0
```

**Terminal 3 — soumettre un job MPI sur 2 nœuds (mode pur, sans OpenMPI) :**
```bash
cargo run -p hypercompute-cli -- submit --wait mpi --np 2 -- printenv HC_MPI_RANK
```

Sortie attendue :
```
Task submitted: f1e2d3c4-...
Status: Queued
Waiting for task f1e2d3c4-...
✓ MPI job f1e2d3c4-... completed in 23ms  (2 ranks OK, 0 failed)
  rank 0 (node a987...)  exit=0
    [rank 0] 0
  rank 1 (node b123...)  exit=0
    [rank 1] 1
```

---

## Commande `worker`

Démarre cette machine comme nœud de calcul dans le cluster. Le worker se connecte au serveur via WebSocket, envoie des heartbeats périodiques, et reçoit des tâches (standard ou MPI) à exécuter.

```bash
cargo run -p hypercompute-cli -- [--server URL] worker [OPTIONS]
```

| Option | Env | Défaut | Description |
|--------|-----|--------|-------------|
| `--name <NOM>` | — | hostname | Nom affiché dans le cluster |
| `--tags <TAG,...>` | — | _(aucun)_ | Capacités déclarées (ex: `python3,ffmpeg,docker,cuda`) |
| `--region <REGION>` | `HC_REGION` | `default` | Zone géographique (ex: `eu-west`, `us-east`) |
| `--heartbeat-secs <N>` | — | `10` | Fréquence des heartbeats en secondes |
| `--max-tasks <N>` | — | `4` | Nombre max de tâches simultanées (standard + MPI confondus) |
| `--mpi-host <HOST>` | `HC_MPI_HOST` | hostname | IP/host joignable depuis les autres nœuds (pour les connexions MPI inter-ranks) |
| `--mpi-port <PORT>` | `HC_MPI_PORT` | `9900` | Port TCP de base pour le bootstrap MPI (rank N utilise `mpi_port + N`) |

Au démarrage, le worker :
- Détecte automatiquement le nombre de CPU, la RAM totale, la présence d'un GPU (`/dev/dri` ou `nvidia-smi`) et la disponibilité de `mpirun`/`mpiexec` dans le PATH
- Annonce ses capacités au serveur via le message `Register`
- Se reconnecte automatiquement en cas de coupure réseau (back-off exponentiel 1s → 60s)
- Envoie `Draining` sur `Ctrl+C` avant de se déconnecter proprement

**Exemples :**

```bash
# Worker minimal (tout auto-détecté)
cargo run -p hypercompute-cli -- worker

# Worker avec tags et région
cargo run -p hypercompute-cli -- worker --tags python3,ffmpeg --region eu-west

# Worker avec adresse MPI explicite (utile si le hostname ne résout pas sur le réseau)
cargo run -p hypercompute-cli -- worker --mpi-host 192.168.1.42 --region eu-west

# Worker sur un serveur distant
cargo run -p hypercompute-cli -- --server http://192.168.1.10:7700 worker

# Tout par variables d'environnement
HC_SERVER=http://192.168.1.10:7700 \
HC_REGION=eu-west \
HC_MPI_HOST=192.168.1.42 \
cargo run -p hypercompute-cli -- worker --tags cuda,python3
```

---

## Commande `submit`

Soumet une tâche au cluster. La tâche est immédiatement enqueued sur le serveur et sera dispatchée au prochain cycle du scheduler (toutes les 200ms).

```bash
cargo run -p hypercompute-cli -- [--server URL] submit [OPTIONS] <SOUS-COMMANDE>
```

**Options communes (avant le type de tâche) :**

| Option | Défaut | Description |
|--------|--------|-------------|
| `--wait` | false | Attendre la fin et afficher le résultat |
| `--priority <0-255>` | `128` | Priorité de la tâche (255 = le plus urgent) |
| `--min-cpu <PCT>` | `0.0` | CPU libre minimum requis sur le worker (%) |
| `--min-ram <MB>` | `0` | RAM libre minimum requise (MB) |
| `--gpu` | false | Exiger un GPU sur le worker |
| `--tags <TAG,...>` | _(aucun)_ | Tags requis sur le worker (contrainte dure) |
| `--region <REGION>` | _(aucun)_ | Région préférée (contrainte douce, bonus de score) |

> **Règle de syntaxe importante** : les options `submit` vont **avant** le type de tâche.
> Le `--` sépare les arguments du CLI des arguments passés à la commande distante.
>
> ✅ `submit --wait --priority 200 shell -- ls -la`
> ❌ `submit shell -- ls -la --wait`  ← `--wait` sera passé à `ls` !

---

### `submit shell`

Exécute une commande shell sur le nœud le mieux scoré.

```bash
cargo run -p hypercompute-cli -- submit [OPTIONS] shell [--timeout <N>] <COMMANDE> [ARGS...]
```

| Option | Défaut | Description |
|--------|--------|-------------|
| `--timeout <N>` | `60` | Timeout en secondes avant kill |

**Exemples :**

```bash
# Commande simple
cargo run -p hypercompute-cli -- submit --wait shell -- echo "hello world"

# Lister les fichiers
cargo run -p hypercompute-cli -- submit --wait shell -- ls -la /tmp

# Script Python
cargo run -p hypercompute-cli -- submit --wait shell -- python3 -c "import sys; print(sys.version)"

# Compilation distante
cargo run -p hypercompute-cli -- submit --wait shell -- make -C /srv/project all

# Timeout long pour tâche lourde
cargo run -p hypercompute-cli -- submit --wait shell --timeout 3600 -- bash /srv/scripts/train.sh

# Tâche urgente (priorité max)
cargo run -p hypercompute-cli -- submit --wait --priority 255 shell -- hostname

# Exiger Python3 et 4 Go de RAM
cargo run -p hypercompute-cli -- submit --wait --tags python3 --min-ram 4096 shell -- python3 inference.py

# Tâche GPU
cargo run -p hypercompute-cli -- submit --wait --gpu shell -- python3 gpu_bench.py

# Fire & forget (sans --wait)
cargo run -p hypercompute-cli -- submit shell -- make clean build
```

---

### `submit fetch`

Effectue une requête HTTP **depuis un nœud worker** et renvoie la réponse. Utile pour tester la connectivité interne, scraper depuis un nœud réseau particulier, ou appeler des services non accessibles depuis le client.

```bash
cargo run -p hypercompute-cli -- submit [OPTIONS] fetch <URL> [--method GET|POST|PUT|DELETE] [--body <BODY>]
```

**Exemples :**

```bash
# GET simple
cargo run -p hypercompute-cli -- submit --wait fetch https://httpbin.org/get

# POST avec body JSON
cargo run -p hypercompute-cli -- submit --wait fetch https://api.example.com/data \
  --method POST --body '{"key":"value"}'

# Ping d'un service interne depuis le réseau cluster
cargo run -p hypercompute-cli -- submit --wait fetch http://internal-db:5432/health

# Depuis un nœud d'une région spécifique
cargo run -p hypercompute-cli -- submit --wait --region eu-west fetch https://cdn.example.com/asset
```

---

### `submit mpi` — mode pur

Le mode pur est le mode par défaut (`--mpirun` absent). Chaque nœud recruté lance le programme **localement** en injectant les variables d'environnement MPI. Aucun OpenMPI/MPICH n'est nécessaire sur les workers.

Les rangs se découvrent via `HC_MPI_PEERS` (liste JSON des pairs) et communiquent entre eux directement en TCP sur les ports `HC_MPI_PORT + rank`.

```bash
cargo run -p hypercompute-cli -- submit [OPTIONS] mpi \
  [--np <N>] [--timeout <N>] [--env KEY=VALUE]... \
  <PROGRAMME> [ARGS...]
```

| Option | Défaut | Description |
|--------|--------|-------------|
| `--np <N>` | `2` | Nombre de rangs MPI (= nœuds recrutés) |
| `--timeout <N>` | `300` | Timeout en secondes pour l'ensemble du job |
| `--env KEY=VALUE` | — | Variable d'environnement injectée sur tous les rangs (répétable) |

**Exemples :**

```bash
# 2 rangs — afficher son rang
cargo run -p hypercompute-cli -- submit --wait mpi --np 2 -- printenv HC_MPI_RANK

# 4 rangs — afficher rang et pairs
cargo run -p hypercompute-cli -- submit --wait mpi --np 4 -- \
  bash -c 'echo "rank=$HC_MPI_RANK size=$HC_MPI_SIZE peers=$HC_MPI_PEERS"'

# Simulation distribuée — 8 rangs avec variables métier
cargo run -p hypercompute-cli -- submit --wait mpi --np 8 \
  --env DATASET=/data/train \
  --env EPOCHS=50 \
  --env BATCH_SIZE=256 \
  -- python3 /srv/ml/train_distributed.py

# Job de rendu 3D distribué — 16 rangs, 30 min max, GPU requis
cargo run -p hypercompute-cli -- submit --wait \
  --gpu --tags blender --min-ram 8192 \
  mpi --np 16 --timeout 1800 \
  -- blender --background /srv/scene.blend --render-output /tmp/frame_

# Calcul haute priorité sur nœuds de la région eu-west
cargo run -p hypercompute-cli -- submit --wait --priority 200 --region eu-west \
  mpi --np 4 -- /usr/local/bin/my_solver --config /etc/solver.conf
```

---

### `submit mpi` — mode mpirun

Avec `--mpirun`, le serveur recrute les nœuds comme en mode pur, mais **seul le rank-0 exécute réellement** `mpirun --host peer0:1,peer1:1,... -np N programme`. OpenMPI/MPICH se charge de spawner les autres rangs via SSH.

Ce mode nécessite :
- `mpirun` ou `mpiexec` dans le PATH de tous les workers
- Un accès SSH passwordless entre les nœuds (pour OpenMPI)
- Que le programme soit présent et dans le PATH sur chaque machine

```bash
cargo run -p hypercompute-cli -- submit [OPTIONS] mpi \
  --mpirun [--np <N>] [--timeout <N>] [--env KEY=VALUE]... \
  <PROGRAMME> [ARGS...]
```

**Exemples :**

```bash
# Training PyTorch distribué via mpirun — 4 nœuds
cargo run -p hypercompute-cli -- submit --wait \
  --tags python3 \
  mpi --np 4 --mpirun \
  --env MASTER_ADDR=auto \
  -- python3 -m torch.distributed.launch train.py

# Benchmark HPL (High Performance Linpack) — 8 nœuds
cargo run -p hypercompute-cli -- submit --wait \
  mpi --np 8 --mpirun --timeout 7200 \
  -- xhpl

# Programme MPI natif (C/Fortran compilé avec mpicc/mpif90)
cargo run -p hypercompute-cli -- submit --wait \
  mpi --np 16 --mpirun -- /usr/local/bin/cfd_solver --mesh /data/mesh.vtk
```

---

## Commande `status`

Consulte l'état d'une tâche déjà soumise. Fonctionne pour les tâches standard et les jobs MPI.

```bash
cargo run -p hypercompute-cli -- status <TASK_ID> [--wait]
```

| Option | Description |
|--------|-------------|
| `--wait` | Bloquer jusqu'à la fin et afficher le résultat |

**Exemples :**

```bash
# Consulter l'état immédiatement
cargo run -p hypercompute-cli -- status 3bd8305d-6952-4f69-b3be-0813d160ff6e

# Attendre la fin (même comportement que submit --wait)
cargo run -p hypercompute-cli -- status 3bd8305d-6952-4f69-b3be-0813d160ff6e --wait
```

**Sortie pour un job MPI terminé :**
```
Task:     f1e2d3c4-...
Priority: 128
Status:   MpiCompleted { job_id: "f1e2d3c4-...", duration_ms: 234, ranks_ok: 4, ranks_failed: 0 }
  rank 0 node a987... exit=0
  rank 1 node b123... exit=0
  rank 2 node c456... exit=0
  rank 3 node d789... exit=0
```

---

## Commande `nodes`

Liste tous les nœuds connus du cluster avec leurs métriques en temps réel. Inclut les informations MPI.

```bash
cargo run -p hypercompute-cli -- nodes
```

**Sortie :**
```
NODE ID                                NAME                 STATUS     CPU%used RAM free Tasks  Region   mpirun MPI host
------------------------------------------------------------------------------------------------------------------------
a987596b-d0a5-4096-9c8b-6dc8dc8cdc60  bazzite              Online     12.3     14823MB 0      eu-west  yes    192.168.1.42
  tags: python3, ffmpeg
b1234567-...                           gpu-rig              Busy       78.1     4096MB  2      eu-west  no     192.168.1.55
  tags: cuda, docker
c9999999-...                           compute-01           Online     5.0      31000MB 0      us-east  yes    10.0.0.3
```

Colonnes :
- **STATUS** : `Online` (disponible), `Busy` (exécute des tâches), `Draining` (arrêt en cours), `Offline`
- **mpirun** : `yes` = mpirun/mpiexec détecté au démarrage du worker
- **MPI host** : adresse IP/hostname déclarée pour les connexions inter-rangs

---

## Commande `info`

Affiche les statistiques globales du cluster.

```bash
cargo run -p hypercompute-cli -- info
```

**Sortie :**
```
HyperCompute Server v0.1.0
  Online nodes:    3
  Queued tasks:    1
  Running tasks:   2
  Completed tasks: 47
```

---

## Options du serveur

```bash
cargo run -p hypercompute-server -- [OPTIONS]
```

| Option | Env | Défaut | Description |
|--------|-----|--------|-------------|
| `--bind <ADDR>` | `HC_BIND` | `0.0.0.0:7700` | Adresse et port d'écoute |
| `--node-timeout-secs <N>` | `HC_NODE_TIMEOUT` | `30` | Secondes sans heartbeat avant de déclarer un nœud mort |
| `--task-timeout-secs <N>` | `HC_TASK_TIMEOUT` | `300` | Secondes avant de re-queuer une tâche bloquée |
| `--max-retries <N>` | `HC_MAX_RETRIES` | `3` | Tentatives avant échec définitif |

**Exemples :**

```bash
# Port personnalisé
cargo run -p hypercompute-server -- --bind 0.0.0.0:9000

# Timeout court (développement)
cargo run -p hypercompute-server -- --node-timeout-secs 10 --task-timeout-secs 30

# Via variables d'environnement
HC_BIND=0.0.0.0:9000 HC_MAX_RETRIES=5 cargo run -p hypercompute-server

# Logs détaillés
RUST_LOG=debug cargo run -p hypercompute-server

# Seulement le scheduler en debug
RUST_LOG=hc_server::scheduler=debug,hc_server=info cargo run -p hypercompute-server
```

---

## Variables d'environnement MPI injectées

Lorsqu'un worker reçoit un slot MPI (mode pur), le programme est lancé avec les variables suivantes :

| Variable | Type | Description |
|----------|------|-------------|
| `HC_MPI_RANK` | entier | Rang de ce processus (0 = root) |
| `HC_MPI_SIZE` | entier | Nombre total de rangs dans le job |
| `HC_MPI_JOB_ID` | UUID | Identifiant unique du job MPI |
| `HC_MPI_PEERS` | JSON | Array JSON de tous les pairs : `[{"host":"…","port":9900,"node_id":"…"}, …]` |
| `HC_MPI_ROOT` | string | Host du rang 0 (pratique pour les connexions maître/esclave) |
| `HC_MPI_PORT` | entier | Port TCP de ce rang (`9900 + rank`) |

**Exemple de programme Python exploitant ces variables :**

```python
import os, json, socket

rank      = int(os.environ["HC_MPI_RANK"])
size      = int(os.environ["HC_MPI_SIZE"])
job_id    = os.environ["HC_MPI_JOB_ID"]
peers     = json.loads(os.environ["HC_MPI_PEERS"])
my_port   = int(os.environ["HC_MPI_PORT"])
root_host = os.environ["HC_MPI_ROOT"]

print(f"[rank {rank}/{size}] job={job_id}")
print(f"  mes pairs: {[p['host'] for p in peers]}")

if rank == 0:
    # Le rank-0 peut écouter sur my_port et collecter les résultats des autres
    with socket.socket() as srv:
        srv.bind(("0.0.0.0", my_port))
        srv.listen(size - 1)
        results = []
        for _ in range(size - 1):
            conn, _ = srv.accept()
            results.append(conn.recv(1024).decode())
        print("Résultats collectés:", results)
else:
    # Les autres rangs envoient leur résultat au rank-0
    with socket.socket() as s:
        s.connect((root_host, 9900))  # port du rank-0
        s.send(f"rank {rank} done".encode())
```

En mode `--mpirun`, OpenMPI injecte ses propres variables (`OMPI_COMM_WORLD_RANK`, etc.) en plus des variables `HC_` via les flags `-x`.

---

## Algorithme de scoring

Le scheduler évalue chaque nœud disponible toutes les 200ms et dispatche vers le meilleur score.

**Pour une tâche standard** (1 nœud) :

```
score = CPU_libre(%)    × 0.35
      + RAM_libre(%)    × 0.25
      + charge_faible   × 0.25   (1/(1+active_tasks), vaut 1.0 si aucune tâche)
      + latence_faible  × 0.10   (1 - ms/500, vaut 0.5 si inconnue)
      + région_préférée × 0.05   (bonus si la région correspond)
```

**Pour un job MPI** (`--np N`) :
1. Tous les nœuds sont scorés individuellement
2. Les `N` nœuds avec le score le plus élevé sont recrutés simultanément
3. S'il n'y a pas assez de nœuds éligibles, le job reste en attente (`Queued`)

**Contraintes dures** (élimination immédiate si non satisfaites) :
- Nœud `Offline` ou `Draining`
- CPU libre < `min_cpu_free_pct`
- RAM libre < `min_ram_free_mb`
- GPU absent alors que `--gpu` est demandé
- Tags manquants

---

## API REST

Le serveur expose une API REST sur `/api/v1/` utilisable directement avec `curl` ou tout client HTTP.

| Méthode | Endpoint | Description |
|---------|----------|-------------|
| `POST` | `/api/v1/tasks` | Soumettre une tâche (standard ou MPI) |
| `GET` | `/api/v1/tasks/:id` | Statut + résultat (inclut `mpi_results` pour les jobs MPI) |
| `POST` | `/api/v1/tasks/:id/cancel` | Annuler une tâche en attente |
| `GET` | `/api/v1/nodes` | Liste des nœuds avec capacités et stats |
| `GET` | `/api/v1/status` | Statistiques globales du serveur |
| `GET` | `/health` | Health check (renvoie `ok`) |

**Soumettre une tâche shell :**
```bash
curl -s -X POST http://localhost:7700/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "kind": {"shell": {"command": "echo", "args": ["hello"], "timeout_secs": 30}},
    "priority": 128
  }' | jq
```

**Soumettre un job MPI en mode pur :**
```bash
curl -s -X POST http://localhost:7700/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "kind": {
      "mpi": {
        "program": "python3",
        "args": ["/srv/train.py"],
        "np": 4,
        "env": {"EPOCHS": "100", "LR": "0.001"},
        "timeout_secs": 3600,
        "use_mpirun": false
      }
    },
    "priority": 200
  }' | jq
```

**Consulter le résultat d'un job MPI :**
```bash
curl -s http://localhost:7700/api/v1/tasks/f1e2d3c4-... | jq '.mpi_results'
# [
#   {"rank": 0, "node_id": "a987...", "output": {"exit_code": 0, "stdout": "rank 0 done\n", ...}},
#   {"rank": 1, "node_id": "b123...", "output": {"exit_code": 0, "stdout": "rank 1 done\n", ...}},
#   ...
# ]
```

**Lister les nœuds :**
```bash
curl -s http://localhost:7700/api/v1/nodes | jq '.nodes[] | {name, status: .status, mpi_host: .capabilities.mpi_host, mpirun: .capabilities.has_mpirun}'
```

---

## Protocole WebSocket

Les workers se connectent sur `ws://HOST:7700/ws` et échangent des messages JSON taggés.

**Worker → Serveur :**

| Message | Moment | Contenu |
|---------|--------|---------|
| `register` | Connexion initiale | `node_id`, `name`, `capabilities` (CPU, RAM, GPU, `mpi_host`, `has_mpirun`, tags, région) |
| `heartbeat` | Toutes les N secondes | `node_id`, stats actuelles (CPU%, RAM libre, active_tasks) |
| `task_result` | Fin d'une tâche standard | `task_id`, `node_id`, `duration_ms`, `output` |
| `task_error` | Échec d'une tâche standard | `task_id`, `node_id`, `reason` |
| `mpi_rank_result` | Fin d'un rang MPI | `job_id`, `task_id`, `node_id`, `rank`, `duration_ms`, `output` |
| `mpi_rank_error` | Échec d'un rang MPI | `job_id`, `task_id`, `node_id`, `rank`, `reason` |
| `draining` | Avant arrêt | `node_id` |

**Serveur → Worker :**

| Message | Description |
|---------|-------------|
| `registered` | Accusé de réception de la registration |
| `dispatch_task` | Tâche standard à exécuter |
| `dispatch_mpi_slot` | Slot MPI : même tâche + `MpiSlot` (rank, size, peers, job_id) |
| `cancel_task` | Annuler une tâche en attente |
| `abort_mpi_job` | Aborter un job MPI (un rang a échoué) |
| `server_shutdown` | Le serveur s'arrête |

---

## Cycle de vie d'un job MPI

```
Client
  │
  ├─ POST /api/v1/tasks  (kind: mpi, np: 4)
  │
  └─► Server
        │
        ├─ TaskStatus: Queued
        │
        ├─ scheduler (toutes les 200ms):
        │    recruit_mpi_nodes(np=4) → [nodeA, nodeB, nodeC, nodeD]
        │    build peer list:
        │      rank 0 → nodeA:192.168.1.42:9900
        │      rank 1 → nodeB:192.168.1.55:9901
        │      rank 2 → nodeC:10.0.0.3:9902
        │      rank 3 → nodeD:10.0.0.7:9903
        │    DispatchMpiSlot(rank=0) → nodeA
        │    DispatchMpiSlot(rank=1) → nodeB
        │    DispatchMpiSlot(rank=2) → nodeC
        │    DispatchMpiSlot(rank=3) → nodeD
        │
        ├─ TaskStatus: MpiDispatched { node_ids: [A,B,C,D] }
        │
        ├─ Chaque worker exécute son rang (avec HC_MPI_RANK, HC_MPI_PEERS, …)
        │    Les rangs communiquent directement entre eux via TCP
        │
        ├─ MpiRankResult(rank=2) reçu → MpiJobState.record_result(2, …)
        ├─ MpiRankResult(rank=0) reçu → MpiJobState.record_result(0, …)
        ├─ MpiRankResult(rank=3) reçu → MpiJobState.record_result(3, …)
        ├─ MpiRankResult(rank=1) reçu → MpiJobState.record_result(1, …)
        │    → is_complete() == true → finalize_mpi_job()
        │
        └─ TaskStatus: MpiCompleted { ranks_ok: 4, ranks_failed: 0, duration_ms: 234 }

Client
  └─ GET /api/v1/tasks/:id  → mpi_results: [{rank:0,…}, {rank:1,…}, …]
```

**En cas d'échec d'un rang :**
- `MpiRankError` reçu → `MpiJobState.record_error(rank, reason)`
- `AbortMpiJob` envoyé **immédiatement** à tous les nœuds du cluster
- Les rangs encore en cours reçoivent le signal et peuvent s'arrêter proprement
- `MpiCompleted { ranks_ok: N, ranks_failed: M }` une fois tous les rangs répondus

---

## Tolérance aux pannes

| Événement | Comportement |
|-----------|-------------|
| **Nœud mort** (heartbeat timeout) | Détecté en `node_timeout_secs` (défaut 30s). Les tâches standard en cours sont requeued. Les jobs MPI qui avaient un rang sur ce nœud sont entièrement requeued. |
| **Tâche bloquée** | Re-queuée après `task_timeout_secs` (défaut 300s). |
| **Rang MPI qui échoue** | `AbortMpiJob` envoyé aux autres rangs. Job requeued si des tentatives restent. |
| **Retries** | Chaque tâche (standard ou MPI) est retentée jusqu'à `max_retries` fois (défaut 3). |
| **Coupure réseau worker** | Le worker se reconnecte avec back-off exponentiel (1s → 60s max). |
| **Drain gracieux** | `Ctrl+C` sur un worker → message `Draining` → le serveur arrête de lui dispatcher de nouvelles tâches, les tâches en cours terminent. |
| **Canal WebSocket mort** | Le nœud est marqué `Offline`, ses tâches sont requeued. |

---

## Installation permanente

Pour éviter de retaper `cargo run` à chaque fois :

```bash
# Compiler en release (binaires optimisés, beaucoup plus rapides)
cargo build --release

# Option A — via cargo install (recommandé, ajoute dans ~/.cargo/bin/)
cargo install --path hypercompute-server
cargo install --path hypercompute-cli

# Option B — copie manuelle système
sudo cp target/release/hc-server /usr/local/bin/
sudo cp target/release/hc        /usr/local/bin/
```

Une fois installé, toutes les commandes se simplifient :

```bash
# Serveur
hc-server --bind 0.0.0.0:7700

# Worker
hc worker --tags python3,cuda --region eu-west --mpi-host 192.168.1.42

# Tâche standard
hc submit --wait shell -- echo "hello"

# Job MPI pur — 4 rangs
hc submit --wait mpi --np 4 -- python3 /srv/train.py

# Job MPI via mpirun — 8 rangs
hc submit --wait mpi --np 8 --mpirun -- /usr/local/bin/solver

# Voir le cluster
hc nodes
hc info
```

---

## Scénario multi-machines avec MPI

**Machine A — serveur (IP: 192.168.1.10) :**
```bash
hc-server --bind 0.0.0.0:7700
```

**Machines B, C, D, E — workers (dans ce réseau, OpenMPI installé) :**
```bash
# Machine B (192.168.1.42)
hc --server http://192.168.1.10:7700 worker \
  --name "node-b" --tags python3,openmpi --region eu-west \
  --mpi-host 192.168.1.42

# Machine C (192.168.1.55)
hc --server http://192.168.1.10:7700 worker \
  --name "node-c" --tags python3,openmpi,cuda --region eu-west \
  --mpi-host 192.168.1.55

# Machines D et E similaires…
```

**Depuis n'importe quelle machine :**

```bash
# Voir le cluster
hc --server http://192.168.1.10:7700 nodes

# Tâche standard → dispatché vers le nœud le plus libre
hc --server http://192.168.1.10:7700 submit --wait shell -- python3 preprocess.py

# Job MPI pur sur 4 nœuds (aucune dépendance SSH)
hc --server http://192.168.1.10:7700 submit --wait \
  mpi --np 4 -- python3 /srv/distributed_training.py

# Job MPI via mpirun sur 4 nœuds (SSH passwordless requis entre nœuds)
hc --server http://192.168.1.10:7700 submit --wait \
  --tags openmpi \
  mpi --np 4 --mpirun -- mpirun_wrapper.sh

# Job GPU MPI — exige CUDA sur les 2 nœuds recrutés
hc --server http://192.168.1.10:7700 submit --wait \
  --gpu --tags cuda \
  mpi --np 2 --env CUDA_VISIBLE_DEVICES=0 -- python3 gpu_sim.py

# Benchmark avec env vars sur 8 nœuds, timeout 2h, priorité max
hc --server http://192.168.1.10:7700 submit --wait \
  --priority 255 --region eu-west \
  mpi --np 8 --timeout 7200 \
  --env PROBLEM_SIZE=10000 --env ITERATIONS=1000 \
  -- /usr/local/bin/hpl_benchmark
```
