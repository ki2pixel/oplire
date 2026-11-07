---
description: Mettre à jour la documentation d'oplire (README.md, manifests de packaging, docstrings) à partir du code Rust et des tests
---

# Workflow Docs Updater

> Ce workflow harmonise la documentation de `oplire` (OpenCode Proxy Pool Manager)
> à partir du code réel, des options CLI (`clap`), des tests et des manifests de distribution.
> Il ne remplace pas la relecture humaine.

## Protocoles critiques

1. **Périmètre** : documentation et manifests de distribution (`README.md`, docstrings Rust dans `src/`, `aur/PKGBUILD`, `winget/oplire.yaml`). Ne pas modifier le code applicatif ou la logique des tests.
2. **Outils autorisés** : `read`, `grep`, `glob`, `edit`, `write`, `shell`
   (limité aux audits `git`, `cargo check`, `cargo test`, `rg`, `wc`).
3. **Contexte** : lire `README.md`, `Cargo.toml`, `src/main.rs` et les modules concernés au démarrage.
4. **Source de vérité** : code Rust (`src/`), tests et options CLI Clap > documentation existante > historique git.
5. **Sécurité** : ne jamais inclure d'identifiants réels de proxys ou de clés d'API (ex. `PROXIWARE_API_KEY`) dans la documentation. Toujours utiliser des exemples fictifs ou génériques (`188.220.163.167:1337:username:password`, `YOUR_PROXIWARE_API_KEY`).
6. **Multiplateforme** : `oplire` supporte Linux, Windows et macOS. Vérifier la pertinence des commandes et des chemins de configuration/logs pour chaque OS.

## Étape 1 — Audit structurel, métrique et contractuel

### Cartographie

```sh
git ls-files '*.rs' '*.toml' '*.md' '*.yml' '*.yaml' 'PKGBUILD'
```

### Volumétrie et versions

```sh
wc -l README.md Cargo.toml src/*.rs src/proxy/*.rs
rg -n 'version = ' Cargo.toml
rg -n 'const VERSION' src/main.rs
rg -n 'pkgver=' aur/PKGBUILD
rg -n 'PackageVersion:' winget/oplire.yaml
```

### Tests et dette

```sh
cargo test --all-targets
cargo clippy -- -D warnings
rg -n 'TODO|FIXME|HACK|XXX' --glob 'src/**/*.rs'
```

### Vérification des commandes CLI et options

Inspecter `src/main.rs` pour comparer les sous-commandes et flags réels avec la documentation :
- `proxy` (`--listen`, `--proxy-file`)
- `watch` (`--upstream`, `--log-file`, `--proxy-file`, `--delay-ms`)
- `daemon` (`--listen`, `--proxy-file`)
- `proxies` (`list`, `test`, `next`, `import`, `fetch`)
- `env` (`--listen`)
- `config` (`show`, `set`, `reset`)
- `doctor`, `setup`, `models`, `install`, `about`

## Étape 2 — Diagnostic triangulé

| Source | Rôle | Outil |
| --- | --- | --- |
| Intention | Le « pourquoi » | Demande de l'utilisateur, `README.md`, conventions du projet |
| Réalité | Le « quoi » et le « comment » | Code Rust (`src/`), CLI Clap (`src/main.rs`), `src/pool.rs`, `src/watch.rs`, `src/proxy/` |
| Existant | L'état documenté | `README.md`, `aur/PKGBUILD`, `winget/oplire.yaml`, docstrings |

Identifier les divergences :
- une sous-commande, un flag ou une valeur par défaut documenté qui ne correspond plus au code (`DEFAULT_DEBOUNCE_MS`, port par défaut `127.0.0.1:8080`, etc.) ;
- un format de proxy supporté non documenté ou obsolète (formats `host:port:user:pass`, `url`, tags `# proxiware-isp`) ;
- un chemin de log ou une variable d'environnement (`NO_PROXY`, `HTTPS_PROXY`, `PROXIWARE_API_KEY`, `RUST_LOG`) inexact ;
- une version désynchronisée entre `Cargo.toml`, `src/main.rs`, `aur/PKGBUILD`, `winget/oplire.yaml` et `README.md` ;
- un tableau de modèles OpenCode non actualisé dans le `README.md`.

## Étape 3 — Sélection du standard de rédaction

- **Commandes CLI** : commandes shell copiables, flags documentés avec leurs types et valeurs par défaut.
- **Architecture & Fonctionnement** :
  - Tunnel HTTP CONNECT : fonctionnement du proxy local vers les proxys amont (Basic Auth, TLS direct).
  - Watcher : détection en temps réel des erreurs 429 et quota dans les logs OpenCode selon l'OS (Linux, macOS, Windows).
  - Gestion du pool : chargement (`~/.config/oplire/proxies.txt`, `proxiware-isp.txt`), rotation atomique.
- **Plateformes & Installation** : source (Cargo), AUR (Arch Linux), Winget (Windows), Homebrew (macOS).
- **Formatage** : Markdown GitHub Flavored avec alertes (`> [!NOTE]`, `> [!TIP]`, `> [!IMPORTANT]`, `> [!WARNING]`).

## Étape 4 — Proposition de mise à jour

Produire un plan avant d'appliquer :

```markdown
## Plan de mise à jour

### Audit
- Cible : `README.md` (Section Command Reference)
- Constat : le flag `--delay-ms` de `oplire watch` n'est pas documenté avec sa valeur par défaut (5000ms).

### Modifications
- Type : documentation CLI
- Diagnostic : paramètre manquant
- Correction : ajout de la description et de l'exemple dans `README.md`.

### Vérification
- Synchronisation avec `src/main.rs` et `src/watch.rs`
- `cargo test` validé
```

## Étape 5 — Application et finalisation

1. Appliquer avec `edit` (changement ciblé) ou `write` (réécriture complète).
2. Vérifier les commandes, flags CLI, variables d'environnement et chemins cités.
3. Vérifier la cohérence des versions dans tous les fichiers concernés (`Cargo.toml`, `src/main.rs`, `aur/PKGBUILD`, `winget/oplire.yaml`, `README.md`).
4. Lancer `cargo test --all-targets`.
5. Lancer `git diff --check`.
6. Vérifier `git status` : seuls les documents prévus doivent être modifiés ; aucun fichier de proxy local (`proxiware-isp.txt` avec clés) ou secret.

## Checkpoints

Avant :
- [ ] Public cible et plateforme identifiés.
- [ ] Code source inspecté (`src/main.rs`, `src/config.rs`, `src/pool.rs`, `src/watch.rs`, `src/proxy/`).
- [ ] Version courante vérifiée.

Pendant :
- [ ] Commandes copiables et testées.
- [ ] Formatage GitHub Alerts (`[!NOTE]`, `[!TIP]`, `[!IMPORTANT]`).
- [ ] Exemples sans données sensibles (identifiants fictifs).

Après :
- [ ] Cohérence globale entre code, CLI, tests et documentation.
- [ ] Aucune donnée de proxy privée ou secret dans le diff.
- [ ] `cargo test` et `git diff --check` passent sans erreur.
