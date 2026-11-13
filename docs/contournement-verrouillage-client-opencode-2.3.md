# Analyse et Contournement du Verrouillage Client OpenCode 2.3+ (Free Limit Lockout)
**Document de Référence Technique — Projet `oplire` (v3.0.0)**
**Auteur :** Équipe d'Ingénierie `oplire`
**Date :** 13 Novembre 2026
**Cible :** OpenCode Desktop v2.3.6 / OpenCode CLI v2.3.0

---

## 1. Contexte et Symptomatologie

Lors de la mise à jour vers **OpenCode 2.3.0 (Desktop 2.3.6)**, un nouveau comportement bloquant a été introduit côté client lors de l'utilisation des modèles gratuits (`opencode/deepseek-v4.1-flash-free`, `opencode/glm-5.3-flash-free`, etc.).

### Symptôme Observé
Lorsqu'un proxy ou une IP source atteint son quota quotidien (~60 000 tokens d'output, HTTP 429), l'interface bureau d'OpenCode n'affiche plus une simple notification d'erreur invitant à réessayer :
1. Une bannière/popup d'upsell intitulée **"Free limit reached"** s'affiche :
   > *"Free usage resets at [date/heure]. Connect a provider or subscribe to Go to continue."*
   > Boutons : `Subscribe to Go · $10/mo` | `Connect provider`
2. **La zone de saisie du chat (Composer / Input prompt) disparaît intégralement du DOM**, sans aucun bouton de réessai ni possibilité de poursuivre ou de relancer un prompt.
3. Même après rotation du proxy par `oplire` (qui bascule sur une IP fraîche ayant 100% de quota disponible), la session et toutes les autres sessions utilisant un modèle gratuit restent **totalement verrouillées** jusqu'à l'heure indiquée (souvent 5 à 9 heures plus tard).

---

## 2. Rétro-Ingénierie du Mécanisme de Blocage

L'analyse du code source d'OpenCode CLI (`/opt/OpenCode/resources/opencode-cli` et `~/.config/ai.opencode.desktop/cli/2.3.0-x64/opencode-cli`) ainsi que du bundle Electron (`/opt/OpenCode/resources/app.asar`) a permis d'isoler la chaîne de blocage exacte.

### Étape A : Détection backend et conversion d'erreur (`opencode-cli`)
Dans le backend HTTP local (`opencode-cli serve --service`), la fonction de routage des erreurs (`ZD(e)`) inspecte le corps de réponse 429 renvoyé par les serveurs d'OpenCode :

```javascript
function ZD(e) {
  if (e instanceof Ke) {
    switch (e.reason._tag) {
      case "RateLimit":
        return ar("provider.rate-limit", e.reason);
      case "QuotaExceeded":
        if (e.reason.body?.includes("FreeUsageLimitError"))
          return { ...ar("provider.free-limit", e.reason), message: t2 };
        if (e.reason.body?.includes("BudgetLimitError"))
          return s2(e.reason);
        if (e.reason.body?.includes("GoUsageLimitError"))
          return o2(e.reason);
        return ar("provider.quota", e.reason);
      // ...
    }
  }
}
```

Lorsque l'erreur contient `"FreeUsageLimitError"`, l'erreur est spécifiquement typée en **`provider.free-limit`** et accompagnée du délai de recharge glissant (`retryAfterMs`).

### Étape B : Persistance globale côté Desktop (`useFreeLimit`)
Dans le frontend Solid.js de l'application bureau (fichier `route-*.js` dans `app.asar`), un hook dédié `useFreeLimit` écoute l'événement d'échec :

```typescript
const FreeLimitState = Persistence.struct({ resetAt: Schema.NullOr(Schema.Finite) })

export function useFreeLimit(paid: () => boolean) {
  const location = useWorkspaceLocation()
  const [state, setState] = persisted(Persist.global("free-limit"), FreeLimitState, { resetAt: null })
  const [now, setNow] = createSignal(Date.now())

  onCleanup(
    location().event.on("session.step.failed", (event) => {
      if (event.data.error.type !== "provider.free-limit") return
      setState(
        "resetAt",
        limitResetAt(event.created, event.data.error.retryAfterMs) ?? event.created + FALLBACK_RESET_MS,
      )
    }),
  )

  const resetAt = () => state.resetAt

  return {
    resetAt,
    active: () => {
      const at = resetAt()
      return !paid() && at !== null && at > now()
    },
  }
}
```

Cette persistance est enregistrée dans la base SQLite locale de l'application :
- **Chemin** : `~/.config/ai.opencode.desktop/drafts.sqlite`
- **Table** : `state`
- **Enregistrement** :
  ```sql
  name = 'opencode.global.dat'
  key = 'free-limit'
  value = '{"resetAt": 1794615995837}'
  ```

### Étape C : Éviction du Composer dans l'Arbre de Rendu
Dans le composant `SessionComposerRegion` :

```tsx
<Show
  when={!freeLimit.active()}
  fallback={
    <SessionFreeLimitPanel
      resetAt={freeLimit.resetAt()}
      selection={() => props.model.composer.model.selection}
    />
  }
>
  <VoiceComposer session={props.session.data.info()}>
    <Composer
      model={props.model.composer}
      borderUnderlay
      // ...
    />
  </VoiceComposer>
</Show>
```

Tant que `resetAt > now()`, `freeLimit.active()` renvoie `true`. Le composant `<Composer />` est **démonté du DOM** et substitué par `<SessionFreeLimitPanel />`, privant l'utilisateur de toute interface de saisie.

---

## 3. Pourquoi les Gestionnaires de Proxy Pools Sont Impactés

Le gestionnaire de rotation `oplire` surveille en continu les logs d'OpenCode (`~/.local/share/opencode/log/opencode.log`). Lors d'une coupure 429 :
1. OpenCode enregistre la ligne d'erreur dans son log (`Free usage limit reached`).
2. `oplire` intercepte la ligne, pivote immédiatement sur l'IP suivante dans le pool et coupe les tunnels HTTP CONNECT existants pour forcer la reconnexion vers la nouvelle IP.
3. **Le problème :** OpenCode Desktop a déjà reçu l'erreur 429 *avant* ou en même temps que la rotation. L'application mémorise alors le blocage global jusqu'à l'heure de réinitialisation. Même si le proxy suivant est frais et opérationnel, l'UI d'OpenCode refuse d'afficher la boîte de saisie.

---

## 4. Stratégie de Résolution à Trois Niveaux

Pour garantir une utilisation transparente et ininterrompue des proxys ISP dans OpenCode, une solution architecturale à 3 niveaux a été mise en œuvre.

### Niveau 1 : Déverrouillage Préventif dans le Lanceur (`opencode-proxy`)
Dans `/home/kidpixel/.local/bin/opencode-proxy`, une purge systématique de la clé `free-limit` est exécutée avant de lancer OpenCode :

```bash
SQLITE_DB="$HOME/.config/ai.opencode.desktop/drafts.sqlite"
if [ -f "$SQLITE_DB" ] && command -v sqlite3 >/dev/null 2>&1; then
    sqlite3 "$SQLITE_DB" "UPDATE state SET value = '{\"resetAt\":null}' WHERE name = 'opencode.global.dat' AND key = 'free-limit';" 2>/dev/null
fi
```
Cela garantit que l'application ne s'ouvre jamais dans un état verrouillé persistant.

### Niveau 2 : Déverrouillage Dynamique dans le Watcher `oplire` (`src/watch.rs`)
Dans `src/watch.rs`, la fonction `rotate_proxy()` appelle désormais `unlock_opencode_desktop_state()` dès qu'une rotation de proxy est validée :

```rust
async fn unlock_opencode_desktop_state() {
    let mut candidate_dbs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidate_dbs.push(home.join(".config").join("ai.opencode.desktop").join("drafts.sqlite"));
        candidate_dbs.push(home.join("Library").join("Application Support").join("ai.opencode.desktop").join("drafts.sqlite"));
    }
    if let Some(app_data) = std::env::var_os("APPDATA") {
        candidate_dbs.push(PathBuf::from(app_data).join("ai.opencode.desktop").join("drafts.sqlite"));
    }

    for db in candidate_dbs {
        if db.exists() {
            let _ = tokio::process::Command::new("sqlite3")
                .arg(&db)
                .arg("UPDATE state SET value = '{\"resetAt\":null}' WHERE name = 'opencode.global.dat' AND key = 'free-limit';")
                .output()
                .await;
        }
    }
}
```

### Niveau 3 : Neutralisation Chirurgicale de l'UI (`app.asar`)
Pour éviter que l'état mémoire d'une fenêtre déjà ouverte ne bascule en mode verrouillé avant un rechargement, un patch binaire de longueur constante (conservant la taille d'octets exacte de `app.asar`) est appliqué sur `/opt/OpenCode/resources/app.asar` :

1. **Garder le Composer toujours actif :**
   - *Cible :* `return!e()&&t!==null&&t>i()` (27 octets)
   - *Remplacement :* `return !1 &&t!==null&&t>i()` (27 octets)
   - *Effet :* `freeLimit.active()` renvoie toujours `false`. Le Composer reste affiché en permanence.

2. **Désactiver la mise en cache de `resetAt` :**
   - *Cible :* `e.data.error.type===`provider.free-limit`&&r(`resetAt`` (55 octets)
   - *Remplacement :* `e.data.error.type===`provider.free-xxxxx`&&r(`resetAt`` (55 octets)
   - *Effet :* Le setter `r("resetAt", ...)` n'est jamais exécuté.

3. **Désactiver la popup modale d'upsell :**
   - *Cible :* `if(t.type===`provider.free-limit`)return h(m,e);` (47 octets)
   - *Remplacement :* `if(t.type===`provider.free-xxxxx`)return h(m,e);` (47 octets)
   - *Effet :* Aucune boîte de dialogue modale intrusive ne vient bloquer l'écran.

4. **Préservation de la restauration du prompt :**
   Le mécanisme d'OpenCode qui réinsère le prompt de l'utilisateur dans la boîte de saisie en cas d'erreur de turn n'est pas altéré. L'utilisateur peut ainsi simplement presser `Entrée` pour relancer son prompt immédiatement sur le proxy suivant.

---

## 5. Procédure lors d'une Future Mise à Jour d'OpenCode

Si une mise à jour ultérieure d'OpenCode écrase `/opt/OpenCode/resources/app.asar` :

1. **Vérifier l'état SQLite :**
   Vérifier si la clé `free-limit` est présente dans `drafts.sqlite` :
   ```bash
   sqlite3 ~/.config/ai.opencode.desktop/drafts.sqlite \
     "SELECT * FROM state WHERE name = 'opencode.global.dat' AND key = 'free-limit';"
   ```
   Pour débloquer immédiatement :
   ```bash
   sqlite3 ~/.config/ai.opencode.desktop/drafts.sqlite \
     "UPDATE state SET value = '{\"resetAt\":null}' WHERE name = 'opencode.global.dat' AND key = 'free-limit';"
   ```

2. **Script Python de Patch Automatique `patch-opencode-free-limit.py` :**
   Le script suivant applique le patch en préservant l'intégrité de l'archive :

   ```python
   #!/usr/bin/env python3
   import sys, shutil

   asar_path = "/opt/OpenCode/resources/app.asar"
   backup_path = asar_path + ".backup-orig"

   # Créer une sauvegarde initiale si inexistante
   import os
   if not os.path.exists(backup_path):
       shutil.copyfile(asar_path, backup_path)
       print(f"Sauvegarde créée : {backup_path}")

   with open(asar_path, "rb") as f:
       data = f.read()

   # Patch 1: Rendre active() toujours faux
   t1_old = b"return!e()&&t!==null&&t>i()"
   t1_new = b"return !1 &&t!==null&&t>i()"

   # Patch 2: Neutraliser le setter resetAt
   t2_old = b"e.data.error.type===`provider.free-limit`&&r(`resetAt`"
   t2_new = b"e.data.error.type===`provider.free-xxxxx`&&r(`resetAt`"

   # Patch 3: Neutraliser la modale upsell
   t3_old = b"if(t.type===`provider.free-limit`)return h(m,e);"
   t3_new = b"if(t.type===`provider.free-xxxxx`)return h(m,e);"

   assert data.count(t1_old) == 1, "t1_old introuvable ou multiple"
   assert data.count(t2_old) == 1, "t2_old introuvable ou multiple"
   assert data.count(t3_old) == 1, "t3_old introuvable ou multiple"

   data = data.replace(t1_old, t1_new)
   data = data.replace(t2_old, t2_new)
   data = data.replace(t3_old, t3_new)

   with open(asar_path, "wb") as f:
       f.write(data)

   print("Patch appliqué avec succès sur app.asar !")
   ```
   Exécution :
   ```bash
   sudo python3 patch-opencode-free-limit.py
   ```
3. **Recharger la fenêtre :**
   Dans OpenCode Desktop, faire `Ctrl + R` pour charger les modifications.
