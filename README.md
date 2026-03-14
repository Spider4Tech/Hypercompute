# HyperCompute

Système de calcul distribué en Rust. Un serveur central reçoit des tâches, les distribue aux machines du réseau selon un algorithme de scoring, et renvoie les résultats au client.

```
  Client A ──┐
  Client B ──┼──► HyperCompute Server ──► Worker Node 1  (score: 0.91)
  Client C ──┘         :7700          ──► Worker Node 2  (score: 0.43)
                                      ──► Worker Node 3  (score: 0.71)
```

---

## Prérequis

- Rust ≥ 1.75 ([installer](https://rustup.rs))

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustc --version   # rustc 1.75.0 ou supérieur
```

---

## Structure du projet

```
hypercompute/
├── Cargo.toml                    # workspace
├── hypercompute-proto/           # types partagés (Task, Node, messages)
├── hypercompute-server/          # serveur de coordination
│   └── src/
│       ├── main.rs               # point d'entrée, args CLI
│       ├── state.rs              # état partagé (DashMap, queues)
│       ├── scheduler.rs          # scoring et dispatch des tâches
│       ├── ws_handler.rs         # connexions WebSocket des workers
│       └── router.rs             # API REST (axum)
└── hypercompute-cli/             # binaire hc (worker + client)
    └── src/
        ├── main.rs               # sous-commandes
        ├── worker.rs             # mode worker : heartbeat, exécution
        ├── executor.rs           # exécution Shell / HTTP / Custom
        └── submitter.rs          # submit, status, nodes, info
```

---

## Démarrage rapide

### 1. Lancer le serveur

```bash
cargo run -p hypercompute-server
```

Sortie attendue :
```
INFO hc_server: HyperCompute Server listening on 0.0.0.0:7700
```

### 2. Lancer un worker (dans un autre terminal, ou sur une autre machine)

```bash
cargo run -p hypercompute-cli -- worker
```

Sortie attendue :
```
INFO hc::worker: Connecting to ws://localhost:7700/ws as 'mon-pc' (a987596b-...)
INFO hc::worker: Connected to server
INFO hc::worker: Registered with server as a987596b-...
```

### 3. Soumettre une tâche et attendre le résultat

```bash
cargo run -p hypercompute-cli -- submit --wait shell -- echo "hello world"
```

Sortie attendue :
```
Task submitted: 3bd8305d-...
Status: Queued
Waiting for task 3bd8305d-...
✓ Completed in 7ms (node: a987596b-...)
--- stdout ---
hello world
Exit code: 0
```

---

## Référence complète des commandes

> **Note** : avec `cargo run`, placez toujours `--` avant les arguments du binaire.
> Exemples ci-dessous avec `cargo run -p hypercompute-cli --` comme préfixe.
> Une fois compilé et installé, remplacez par `hc` directement.

---

### `worker` — démarrer cette machine comme nœud de calcul

```bash
cargo run -p hypercompute-cli -- worker [OPTIONS]
```

| Option | Défaut | Description |
|--------|--------|-------------|
| `--name <NOM>` | hostname | Nom affiché dans le cluster |
| `--tags <TAG,...>` | _(aucun)_ | Capacités déclarées (ex: `python3,ffmpeg,docker`) |
| `--region <REGION>` | `default` | Zone géographique (ex: `eu-west`, `us-east`) |
| `--heartbeat-secs <N>` | `10` | Fréquence des heartbeats en secondes |
| `--max-tasks <N>` | `4` | Nombre max de tâches simultanées |

**Exemples :**

```bash
# Worker minimal
cargo run -p hypercompute-cli -- worker

# Worker avec tags et région
cargo run -p hypercompute-cli -- worker --tags python3,ffmpeg --region eu-west

# Worker avec nom custom et 8 tâches max
cargo run -p hypercompute-cli -- worker --name "gpu-server-01" --max-tasks 8

# Worker sur un serveur distant
cargo run -p hypercompute-cli -- --server http://192.168.1.10:7700 worker --region eu-west

# Avec variable d'environnement
HC_SERVER=http://192.168.1.10:7700 HC_REGION=eu-west cargo run -p hypercompute-cli -- worker
```

---

### `submit` — soumettre une tâche

```bash
cargo run -p hypercompute-cli -- submit [OPTIONS] <SOUS-COMMANDE>
```

**Options communes à tous les types de tâches :**

| Option | Défaut | Description |
|--------|--------|-------------|
| `--wait` | false | Attendre et afficher le résultat |
| `--priority <0-255>` | `128` | Priorité (255 = urgent) |
| `--min-cpu <PCT>` | `0` | CPU libre minimum requis sur le worker (%) |
| `--min-ram <MB>` | `0` | RAM libre minimum requise sur le worker (MB) |
| `--gpu` | false | Exiger un GPU sur le worker |
| `--tags <TAG,...>` | _(aucun)_ | Tags requis sur le worker |
| `--region <REGION>` | _(aucun)_ | Région préférée (contrainte douce) |

> ⚠️ Les options `submit` doivent être placées **avant** le type de tâche (`shell` ou `fetch`).
> Le `--` sépare les arguments du CLI des arguments de la commande distante.

#### `submit shell` — exécuter une commande shell

```bash
cargo run -p hypercompute-cli -- submit [OPTIONS] shell <COMMANDE> [ARGS...]
```

```bash
# Commande simple
cargo run -p hypercompute-cli -- submit --wait shell -- echo "hello world"

# Lister les fichiers d'un répertoire
cargo run -p hypercompute-cli -- submit --wait shell -- ls -la /tmp

# Script Python
cargo run -p hypercompute-cli -- submit --wait shell -- python3 -c "print(2**32)"

# Commande longue avec timeout personnalisé
cargo run -p hypercompute-cli -- submit --wait shell --timeout 120 -- sleep 5

# Tâche urgente (priorité max)
cargo run -p hypercompute-cli -- submit --wait --priority 255 shell -- hostname

# Exiger un nœud avec Python et au moins 2 Go de RAM libre
cargo run -p hypercompute-cli -- submit --wait --tags python3 --min-ram 2048 shell -- python3 script.py

# Envoyer sans attendre (fire & forget)
cargo run -p hypercompute-cli -- submit shell -- make build
```

#### `submit fetch` — requête HTTP sur le worker

```bash
cargo run -p hypercompute-cli -- submit [OPTIONS] fetch <URL> [--method GET|POST|PUT|DELETE] [--body <BODY>]
```

```bash
# GET simple
cargo run -p hypercompute-cli -- submit --wait fetch https://httpbin.org/get

# POST avec body
cargo run -p hypercompute-cli -- submit --wait fetch https://httpbin.org/post \
  --method POST --body '{"key":"value"}'

# Ping d'un service interne depuis un nœud du cluster
cargo run -p hypercompute-cli -- submit --wait fetch http://internal-api:8080/health
```

---

### `status` — consulter l'état d'une tâche

```bash
cargo run -p hypercompute-cli -- status <TASK_ID> [--wait]
```

```bash
# Consulter l'état
cargo run -p hypercompute-cli -- status 3bd8305d-6952-4f69-b3be-0813d160ff6e

# Attendre la fin
cargo run -p hypercompute-cli -- status 3bd8305d-6952-4f69-b3be-0813d160ff6e --wait
```

---

### `nodes` — lister les nœuds du cluster

```bash
cargo run -p hypercompute-cli -- nodes
```

Sortie :
```
NODE ID                                NAME                 STATUS     CPU%used RAM free Tasks  Region
--------------------------------------------------------------------------------------------------------
a987596b-d0a5-4096-9c8b-6dc8dc8cdc60  bazzite              Online     12.3     14823MB 0      eu-west
  tags: python3, ffmpeg
f1e2d3c4-...                           gpu-server-01        Busy       78.1     4096MB  3      us-east
  tags: docker, cuda
```

---

### `info` — statistiques du cluster

```bash
cargo run -p hypercompute-cli -- info
```

Sortie :
```
HyperCompute Server v0.1.0
  Online nodes:    3
  Queued tasks:    0
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
| `--bind <ADDR>` | `HC_BIND` | `0.0.0.0:7700` | Adresse d'écoute |
| `--node-timeout-secs <N>` | `HC_NODE_TIMEOUT` | `30` | Délai avant de déclarer un nœud mort |
| `--task-timeout-secs <N>` | `HC_TASK_TIMEOUT` | `300` | Délai avant de re-queuer une tâche bloquée |
| `--max-retries <N>` | `HC_MAX_RETRIES` | `3` | Nombre de tentatives avant échec définitif |

```bash
# Serveur sur port personnalisé
cargo run -p hypercompute-server -- --bind 0.0.0.0:9000

# Timeout nœud plus court (utile en dev)
cargo run -p hypercompute-server -- --node-timeout-secs 10

# Via variables d'environnement
HC_BIND=0.0.0.0:9000 HC_MAX_RETRIES=5 cargo run -p hypercompute-server
```

---

## Logs et debug

Les logs utilisent `RUST_LOG` (format `tracing`).

```bash
# Niveau par défaut (debug serveur, info tower)
cargo run -p hypercompute-server

# Tout en debug
RUST_LOG=debug cargo run -p hypercompute-server

# Seulement les infos importantes
RUST_LOG=info cargo run -p hypercompute-server

# Debug uniquement le scheduler
RUST_LOG=hc_server::scheduler=debug cargo run -p hypercompute-server

# Silencieux (erreurs seulement)
RUST_LOG=error cargo run -p hypercompute-server
```

---

## Algorithme de scoring

Quand une tâche est soumise, le serveur évalue chaque nœud disponible et dispatche vers le **score le plus élevé**.

```
score = CPU_libre    × 0.35
      + RAM_libre    × 0.25
      + charge_faible × 0.25
      + latence_faible × 0.10
      + région_préférée × 0.05
```

Un nœud est **éliminé** (score = None) si :
- Il est `Offline` ou `Draining`
- Son CPU libre est inférieur à `min_cpu_free_pct`
- Sa RAM libre est inférieure à `min_ram_free_mb`
- Il n'a pas de GPU alors que `requires_gpu = true`
- Il ne possède pas tous les `required_tags`

---

## API REST

Le serveur expose une API REST sur `/api/v1/` :

| Méthode | Endpoint | Description |
|---------|----------|-------------|
| `POST` | `/api/v1/tasks` | Soumettre une tâche |
| `GET` | `/api/v1/tasks/:id` | Statut + résultat d'une tâche |
| `POST` | `/api/v1/tasks/:id/cancel` | Annuler une tâche en attente |
| `GET` | `/api/v1/nodes` | Liste des nœuds |
| `GET` | `/api/v1/status` | Statistiques globales |
| `GET` | `/health` | Health check |

**Exemple avec curl :**

```bash
# Soumettre une tâche shell
curl -s -X POST http://localhost:7700/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "kind": {"shell": {"command": "echo", "args": ["hello"], "timeout_secs": 30}},
    "priority": 128
  }' | jq

# Consulter une tâche
curl -s http://localhost:7700/api/v1/tasks/3bd8305d-6952-4f69-b3be-0813d160ff6e | jq

# Lister les nœuds
curl -s http://localhost:7700/api/v1/nodes | jq

# Statut global
curl -s http://localhost:7700/api/v1/status | jq
```

---

## Installation permanente

Pour avoir `hc` et `hc-server` disponibles sans `cargo run` :

```bash
# Compiler en mode release
cargo build --release

# Option A — installer via cargo (recommandé)
cargo install --path hypercompute-cli
cargo install --path hypercompute-server

# Option B — copie manuelle
sudo cp target/release/hc /usr/local/bin/
sudo cp target/release/hc-server /usr/local/bin/

# Vérifier
hc --help
hc-server --help
```

Une fois installé, les commandes deviennent :

```bash
hc-server                                        # démarrer le serveur
hc worker --tags python3 --region eu-west        # démarrer un worker
hc submit --wait shell -- echo "hello"           # soumettre une tâche
hc nodes                                         # voir les nœuds
hc info                                          # voir les stats
```

---

## Scénario multi-machines

**Machine A (serveur, IP: 192.168.1.10) :**
```bash
cargo run -p hypercompute-server -- --bind 0.0.0.0:7700
```

**Machine B (worker) :**
```bash
cargo run -p hypercompute-cli -- \
  --server http://192.168.1.10:7700 \
  worker --name "machine-b" --tags python3 --region eu-west
```

**Machine C (worker GPU) :**
```bash
cargo run -p hypercompute-cli -- \
  --server http://192.168.1.10:7700 \
  worker --name "gpu-rig" --tags cuda,python3 --region eu-west
```

**Depuis n'importe quelle machine du réseau :**
```bash
# Tâche sans contrainte → dispatché vers le nœud le plus libre
cargo run -p hypercompute-cli -- \
  --server http://192.168.1.10:7700 \
  submit --wait shell -- python3 train.py

# Tâche qui exige un GPU
cargo run -p hypercompute-cli -- \
  --server http://192.168.1.10:7700 \
  submit --wait --gpu shell -- python3 gpu_inference.py

# Voir tous les nœuds du cluster
cargo run -p hypercompute-cli -- \
  --server http://192.168.1.10:7700 \
  nodes
```

---

## Tolérance aux pannes

- **Nœud qui tombe** : détecté en `node_timeout_secs` (défaut 30s), les tâches en cours sont automatiquement re-queuées
- **Tâche bloquée** : re-queuée après `task_timeout_secs` (défaut 300s)
- **Retries** : chaque tâche est retentée jusqu'à `max_retries` fois (défaut 3) avant d'être marquée `Failed`
- **Reconnexion worker** : le worker tente de se reconnecter avec back-off exponentiel (1s → 60s max)
- **Drain gracieux** : `Ctrl+C` sur un worker envoie un message `Draining` au serveur qui arrête de lui dispatcher de nouvelles tâches
