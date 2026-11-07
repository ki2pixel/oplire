---
description: Exécuter les contrôles Rust (formatage, lint, tests), committer et pousser la branche courante
---

# Commit & Push (branche courante)

## Prérequis

- Des fichiers modifiés existent dans le dépôt.
- Le remote `origin` est configuré (dépôt git initialisé).
- Aucun secret, jeton ou proxy réel n'est présent dans le diff (`proxiware-isp.txt` avec identifiants réels, `proxies.txt`, `.env`, `.env.local`).
- Aucun artefact de build ou binaire n'est stagé (`target/`, `oplire`, `oplire.exe`, `*.tar.gz`).
- Si une version a été modifiée, s'assurer de la cohérence entre `Cargo.toml`, `src/main.rs` (`VERSION`), `aur/PKGBUILD` (`pkgver`) et `winget/oplire.yaml` (`PackageVersion`).

## Étapes

1. Vérifier l'état : `git status`, `git diff --stat`.
2. Lancer les contrôles qualité Rust :
   ```sh
   cargo fmt -- --check
   cargo clippy -- -D warnings
   cargo test --all-targets
   git diff --check
   ```
3. Stager explicitement les fichiers voulus :
   ```sh
   git add <fichiers>
   git diff --cached --check
   ```
   Éviter `git add -A` par défaut afin de ne pas inclure de fichiers locaux de configuration ou de listes de proxys.
4. Committer avec un message Conventional Commits dont le résumé est en français, généré depuis le diff :
   ```sh
   git commit -m "<type>: <résumé court en français>"
   ```
   Types usuels : `feat`, `fix`, `chore`, `docs`, `test`, `refactor`, `perf`.
5. Pousser :
   ```sh
   git push -u origin "$(git branch --show-current)"
   ```

## Exécution étape par étape

```sh
BRANCH=$(git branch --show-current)

git status
git diff --stat

cargo fmt -- --check
cargo clippy -- -D warnings
cargo test --all-targets
git diff --check

git add <fichiers>
git diff --cached --check
git commit -m "<type>: <résumé court en français>"
git push -u origin "$BRANCH"
```

## Notes

- Le CI GitHub Actions (`.github/workflows/build.yml`) valide la compilation multiplateforme (Linux, Windows, macOS x86_64 et ARM). La suite de tests et les lints locaux doivent être impeccables.
- Ne jamais committer d'identifiants de proxys réels, ni de clés d'API (ex. `PROXIWARE_API_KEY`).
- En cas de modification des arguments CLI (`clap`), s'assurer que `README.md` est synchronisé.
- En cas d'échec d'un check (format, clippy ou tests), corriger avant de committer ; ne pas contourner avec `--no-verify`.
