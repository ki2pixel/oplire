Ce rapport présente l'audit architectural, sécuritaire et de performance de **oplire v3.0.0** (crate `oplire-reset`, commit `de18c46`) couvrant l'intégralité des modules backend (`src/main.rs`, `src/config.rs`, `src/pool.rs`, `src/proxy/mod.rs`, `src/proxy/server.rs`, `src/proxy/tunnel.rs`, `src/watch.rs`) ainsi que la chaîne de packaging et CI.

---

## 1. Synthèse Exécutive

* **Note globale de sécurité : 5,8 / 10 (Risque Modéré à Élevé en environnement partagé ou mal configuré)** — L'isolation locale par défaut sur `127.0.0.1:8080` et l'absence de bloc `unsafe` limitent la surface d'exposition immédiate. Toutefois, le serveur souffre d'une absence de protection contre le Server-Side Request Forgery (SSRF) vers les réseaux privés/loopback, d'une vulnérabilité au déni de service par épuisement de descripteurs (Slowloris sur les lectures sans timeout et absence de limite de concurrence), d'un stockage des identifiants de proxys en clair avec des permissions système trop permissives (`0644` par défaut), et d'une exposition potentielle de `PROXIWARE_API_KEY` via l'aide CLI Clap.


* **Note globale de robustesse et concurrence Tokio : 6,2 / 10** — L'architecture asynchrone repose sur des primitives adaptées (`Arc<RwLock<Vec<ProxyEntry>>>`, `AtomicUsize`, `tokio::sync::broadcast`), mais présente des conditions de course critiques : maintien de verrous d'écriture `RwLock` à travers des points `.await` d'entrées/sorties disque, appels système `metadata` bloquants à chaque requête `CONNECT`, fenêtre TOCTOU entre la sélection du proxy et l'abonnement au canal de rotation, perte de descripteur d'inode lors d'un `logrotate`, et instanciations multiples de runtimes Tokio dans `src/main.rs`.


* **Couverture de tests : 2,5 / 10** — Seules 8 fonctions de tests unitaires existent dans `src/pool.rs` (lignes 527-621), laissant `src/proxy/server.rs`, `src/proxy/tunnel.rs`, `src/watch.rs`, `src/config.rs` et `src/main.rs` sans couverture automatisée ni validation dans le workflow GitHub Actions `.github/workflows/build.yml`.



---

## 2. Cartographie Architecturale et Flux de Données

### 2.1 Cycle de vie d'une connexion TCP entrante

1. **Acceptation TCP (`src/proxy/server.rs`, lignes 84-98)** : `TcpListener::bind(addr)` écoute sur `127.0.0.1:8080` par défaut. Chaque connexion entrante déclenche immédiatement un `tokio::spawn` non borné qui clone la structure `ProxyPool` (clonage léger de 4 `Arc` et d'un `broadcast::Sender`).


2. **Discrimination de protocole par pré-lecture (`src/proxy/server.rs`, lignes 10-30)** : La fonction `is_connect_request(&socket)` effectue un `socket.peek(&mut peek_buf)` de 7 octets dans une boucle de scrutation (sommeil de 5 ms par itération) bornée par un `tokio::time::timeout` de 250 ms. Si les 7 premiers octets correspondent exactement à `b"CONNECT"`, le flux est routé vers `handle_connect_tunnel` ; sinon, il est traité par `handle_plain_http` (endpoint `/health` en `GET` ou `HEAD`, ou rejet `400 Bad Request`).


3. **Lecture de l'en-tête `CONNECT` (`src/proxy/tunnel.rs`, lignes 14-47)** : `handle_connect_tunnel` consomme les octets du socket client par tranches de 1 024 octets dans `req_buf` jusqu'à détecter la séquence `\r\n\r\n` via `find_double_crlf` ou dépasser 8 192 octets (`413 Payload Too Large`). La cible `host:port` est extraite du second jeton de la première ligne (`parts[1]`).


4. **Sélection du proxy et vérification disque (`src/proxy/tunnel.rs`, ligne 49 et `src/pool.rs`, lignes 334-406)** : L'appel `pool.get_active().await` invoque systématiquement `reload_if_modified().await` (qui interroge le système de fichiers via `tokio::fs::metadata` sur `proxies.txt` et `active_proxy.idx`), puis acquiert un verrou de lecture `self.proxies.read().await` et calcule l'index effectif via `self.active_index.load(Ordering::Relaxed) % guard.len()`.


5. **Établissement du tunnel amont (`src/proxy/tunnel.rs`, lignes 51-141)** :
* **Branche `Some(proxy)` (lignes 52-167)** : Connexion TCP vers `proxy.host:proxy.port` sous timeout de 8 secondes (`502 Bad Gateway` sur erreur, `504 Gateway Timeout` sur expiration). Envoi d'une requête `CONNECT <target> HTTP/1.1` enrichie de `Proxy-Authorization: Basic <base64>` si des identifiants sont présents (`src/pool.rs`, lignes 150-157). Lecture de la réponse du proxy amont dans `resp_buf` (limite 4 096 octets), validation de la présence de `"200"` sur la première ligne, envoi de `HTTP/1.1 200 Connection Established\r\n\r\n` au client OpenCode, puis transfert des éventuels octets résiduels post-en-tête (`&req_buf[req_end..]` et `&resp_buf[resp_end..]`).


* **Branche `None` / Mode direct (lignes 169-221)** : Connexion TCP directe vers `target` sous timeout de 8 secondes, envoi de `200 Connection Established` au client et purge du reliquat de `req_buf`.




6. **Relais bidirectionnel et interruption sur rotation (`src/proxy/tunnel.rs`, lignes 146-162 et 204-219)** : Le tunnel s'abonne au canal de diffusion via `pool.subscribe_rotation()` et exécute un `tokio::select!` mettant en concurrence `copy_bidirectional(&mut client, &mut upstream)` et `rotation_rx.recv()`. Lorsqu'un événement de rotation est reçu, le bloc `select!` se termine, annulant `copy_bidirectional` et fermant les deux sockets TCP par drop afin de forcer la reconnexion d'OpenCode sur la nouvelle IP de sortie.



### 2.2 Schéma de concurrence et synchronisation Tokio

```
+-----------------------------------------------------------------------+
|                       Processus oplire (proxy / daemon)               |
|                                                                       |
|  [Tâche Watcher : watch.rs]              [Tâche Listener : server.rs] |
|   ├─ monitor_log_file (poll 500ms)        └─ TcpListener::accept()    |
|   └─ monitor_http_upstream (poll 30s)            │                    |
|            │                                     ▼ (tokio::spawn x N) |
|            │ (429 / Quota détecté)       [Tâches Tunnel : tunnel.rs]  |
|            ▼                                     │                    |
|     pool.rotate().await                   pool.get_active().await     |
|            │                                     │                    |
|            ▼                                     ▼                    |
|  +-----------------------------------------------------------------+  |
|  |                     ProxyPool (src/pool.rs)                     |  |
|  |  • proxies: Arc<RwLock<Vec<ProxyEntry>>>                        |  |
|  |  • active_index: Arc<AtomicUsize>                               |  |
|  |  • last_mtime / last_index_mtime: Arc<RwLock<Option<SystemTime>>>| |
|  |  • rotation_tx: broadcast::Sender<usize> (capacité = 32)        |  |
|  +-----------------------------------------------------------------+  |
|            │                                     ▲                    |
|            └─────── rotation_tx.send(idx) ───────┘                    |
|                     (Interruption du tokio::select!)                  |
+-----------------------------------------------------------------------+

```

* **Contention sur les `RwLock` et verrous maintenus pendant l'I/O (`src/pool.rs`, lignes 334-396 et 408-445)** :
* Chaque appel à `get_active()` (à chaque connexion `CONNECT`) exécute `reload_if_modified()`.


* Dans `reload_if_modified()` (lignes 354-365), le code acquiert `let mut p_guard = self.proxies.write().await;` puis `let mut m_guard = self.last_mtime.write().await;` et appelle ensuite `Self::read_saved_index_async().await` **tout en détenant les deux verrous exclusifs d'écriture**. Pendant toute la durée de cet appel disque asynchrone, l'ensemble des nouvelles connexions TCP sur le serveur proxy sont bloquées en attente sur `self.proxies.read().await`.


* De même, dans `rotate()` (lignes 411-427), le verrou `let guard = self.proxies.read().await;` est maintenu pendant l'écriture disque `Self::save_index_async(next_idx).await`.




* **Incohérence de l'`AtomicUsize` sous rotations concurrentes (`src/pool.rs`, lignes 424-426 et `src/watch.rs`, lignes 130-133)** :
* Dans `rotate()`, la mise à jour de l'index n'est pas atomique : `let current_idx = self.active_index.load(Ordering::SeqCst); let next_idx = (current_idx + 1) % total; self.active_index.store(next_idx, Ordering::SeqCst);`. Comme seul un verrou de lecture (`proxies.read().await`) est détenu, deux appels concurrents à `rotate()` (par exemple déclenchés simultanément par `monitor_log_file` et `monitor_http_upstream` dans `src/watch.rs`, lignes 130-133) peuvent lire le même `current_idx` ou provoquer un double saut d'index.


* En outre, `get_active()` lit `active_index` avec `Ordering::Relaxed` (ligne 404) tandis que `rotate()` et `reload_if_modified()` écrivent avec `Ordering::SeqCst` (lignes 359, 390, 426).




* **Fenêtre TOCTOU et saturation du canal `broadcast` (`src/proxy/tunnel.rs`, lignes 49-162)** :
* Le proxy actif est résolu à la ligne 49 (`let active_proxy = pool.get_active().await;`), mais l'abonnement `let mut rotation_rx = pool.subscribe_rotation();` n'intervient qu'à la ligne 146 (ou 204 en mode direct), soit **après** la connexion TCP amont (jusqu'à 8 s) et la négociation `CONNECT`. Toute rotation diffusée pendant cette fenêtre d'établissement n'est jamais reçue par le tunnel entrant, qui reste attaché à l'ancien proxy épuisé.


* Le canal `broadcast::channel(32)` (`src/pool.rs`, ligne 183) produit une erreur `Err(RecvError::Lagged(n))` si plus de 32 événements sont émis avant consommation. Or, la branche `Ok(new_idx) = rotation_rx.recv()` (`src/proxy/tunnel.rs`, lignes 153 et 211) ignore le cas `Err(RecvError::Lagged(_))`, désactivant définitivement l'écoute de rotation pour ce tunnel si un décalage survient.





---

## 3. Tableau Récapitulatif des Vulnérabilités et Faiblesses

| ID | Sévérité | Composant & Lignes | Vecteur / Mécanisme | Impact |
| --- | --- | --- | --- | --- |
| **VULN-01** | Critique | `src/proxy/server.rs` (84), `src/proxy/tunnel.rs` (28, 173) | Binding `0.0.0.0` possible sans authentification ni filtrage d'IP cible (`target`) en mode proxy ou direct | Relais proxy ouvert exposant les proxys ISP payants et SSRF vers `127.0.0.1` / RFC 1918 / métadonnées cloud |
| **VULN-02** | Haute | `src/proxy/server.rs` (37), `src/proxy/tunnel.rs` (19, 93), `server.rs` (88) | Lectures TCP sans timeout (`handle_plain_http`, `handle_connect_tunnel`) et `tokio::spawn` sans sémaphore | Déni de service (Slowloris) et épuisement des descripteurs de fichiers (`EMFILE`) |
| **VULN-03** | Haute | `src/pool.rs` (201, 234, 330), `src/main.rs` (175) | Création de `proxies.txt` et `config.json` avec permissions par défaut (`0644`) sans restriction `0600` | Lecture des mots de passe de proxys ISP en clair par tout utilisateur ou processus local non privilégié |
| **VULN-04** | Haute | `src/watch.rs` (176-183) | Allocation `buffer.resize((file_len - offset) as usize, 0)` non bornée sur croissance de fichier de log | Déni de service par épuisement mémoire (OOM crash) si le fichier de log grossit brutalement |
| **VULN-05** | Haute | `src/watch.rs` (154-183) | Descripteur `File::open` conservé sur l'ancien inode après un renommage `logrotate` alors que `metadata(&path)` lit le nouveau | Lecture erronée de l'ancien fichier de log depuis l'offset 0 et déclenchement de rotations fantômes |
| **VULN-06** | Haute | `src/proxy/tunnel.rs` (49, 146, 153, 204, 211) | Abonnement `subscribe_rotation()` tardif (post-handshake) et filtrage de `Err(RecvError::Lagged)` | Tunnels maintenus sur l'IP épuisée après une rotation pendant le handshake ou lors d'un burst |
| **VULN-07** | Moyenne | `src/main.rs` (112, 148-164), `src/pool.rs` (14, 193-204) | Exposition de `PROXIWARE_API_KEY` via `--help` Clap et CLI ; repli sur `.` sous Windows pour les chemins de config | Fuite de clé d'API dans le terminal/table des processus et écriture de `proxies.txt` dans le répertoire courant sous Windows |
| **VULN-08** | Moyenne | `src/pool.rs` (354-365, 411-430), `src/watch.rs` (130-133) | Verrous `RwLock` maintenus durant des `.await` disque ; exécution simultanée de `monitor_log_file` et `monitor_http_upstream` | Contention globale sur les tunnels pendant l'I/O disque et doubles rotations simultanées sur erreur 429 |
| **VULN-09** | Moyenne | `src/pool.rs` (57, 134) vs `src/proxy/tunnel.rs` (53-85) | Parsing de `socks5://` dans `ProxyEntry` mais envoi inconditionnel d'une requête HTTP `CONNECT` en amont | Échec systématique (`502 Bad Gateway`) de tous les proxys SOCKS5 pourtant annoncés comme supportés |
| **VULN-10** | Moyenne | `src/proxy/server.rs` (88), `src/main.rs` (400, 426, 468, 485, 559, 576) | Arrêt du démon sur erreur transitoire `listener.accept()?` ; double instanciation de `Runtime::new().unwrap()` par commande | Crash complet du serveur proxy sur erreur socket transitoire et gaspillage de threads OS au démarrage |
| **VULN-11** | Basse | `src/proxy/tunnel.rs` (27, 76, 100), `src/proxy/server.rs` (16, 54) | Validation laxiste du code HTTP `"200"`, casse stricte sur `peek("CONNECT")`, corps `"OK"` envoyé sur `HEAD /health` | Non-conformité RFC 9110/9112, risque d'injection d'en-têtes si `target` est malformé, faux positifs `200` |
| **VULN-12** | Info | `.github/workflows/build.yml`, `aur/PKGBUILD` (14), `winget/oplire.yaml` (15) | Absence de `cargo test` / `clippy` en CI ; `sha256sums=('SKIP')` et `InstallerSha256: REPLACEME` | Risque de régression non détectée en CI et absence de vérification d'intégrité cryptographique des paquets |

---

## 4. Audit Détaillé par Domaine

### 4.1 Sécurité Réseau et Contrôle d'Accès

#### A. Politique de binding (`127.0.0.1` vs `0.0.0.0`) et repli silencieux

* **Analyse (`src/proxy/server.rs`, lignes 70-72 et 84)** :
* L'adresse d'écoute par défaut est `127.0.0.1:8080` (`src/config.rs`, ligne 16).


* Toutefois, l'utilisateur peut passer `--listen 0.0.0.0:8080` ou configurer `oplire config set --listen 0.0.0.0:8080`. Comme le serveur proxy local n'exige aucune authentification entrante (`Proxy-Authorization` n'est utilisé que vers l'amont à la ligne 78 de `src/proxy/tunnel.rs`), écouter sur une interface non-loopback (`!addr.ip().is_loopback()`) transforme immédiatement la machine hôte en **proxy ouvert non authentifié**, exposant les crédits des proxys résidentiels/ISP configurés à tout le réseau local ou public.


* En outre, `README.md` (ligne 213) documente la commande `oplire config set --listen :8080`. Or, `":8080".parse::<SocketAddr>()` échoue en Rust standard : dans `start_proxy_server` (`src/proxy/server.rs`, lignes 70-72), cet échec est masqué silencieusement par `.unwrap_or_else(|_| "127.0.0.1:8080".parse().unwrap())`, tandis que dans `oplire doctor` (`src/main.rs`, ligne 489), `std::net::TcpListener::bind(":8080")` échoue et affiche à tort `Listen address: IN USE (:8080)`.





#### B. Risques de Server-Side Request Forgery (SSRF)

* **Analyse (`src/proxy/tunnel.rs`, lignes 27-29 et 169-221)** :
* Lorsqu'une requête `CONNECT <target> HTTP/1.1` est reçue, `target` est extrait par `parts[1].to_string()` sans aucune validation syntaxique ni filtrage d'adresse IP.


* En **mode direct** (`active_proxy == None`, lignes 169-221, activé automatiquement si aucun fichier de proxy n'est chargé ou s'il est vide), `TcpStream::connect(&target)` ouvre un tunnel TCP direct depuis le processus `oplire` vers n'importe quelle destination.


* Si `oplire` écoute sur `0.0.0.0` (ou si un processus local non privilégié/sandboxé comme un navigateur ou une extension locale interroge `127.0.0.1:8080`), un attaquant peut émettre `CONNECT 127.0.0.1:3000 HTTP/1.1`, `CONNECT 169.254.169.254:80 HTTP/1.1` (service de métadonnées cloud IMDS) ou cibler des sous-réseaux internes RFC 1918 (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`).


* De plus, un client qui omettrait `NO_PROXY` et enverrait `CONNECT 127.0.0.1:8080 HTTP/1.1` en mode direct créerait une **boucle infinie d'auto-connexion** épuisant instantanément tous les sockets et la RAM du processus.





#### C. Parsing HTTP manuel et résistance au déni de service (DoS)

* **Discrimination `is_connect_request` (`src/proxy/server.rs`, lignes 10-30)** :
* La fonction attend jusqu'à 250 ms avec une boucle active `tokio::time::sleep(Duration::from_millis(5))` tant que moins de 7 octets sont reçus et qu'ils correspondent au préfixe de `b"CONNECT"`.


* La comparaison `&peek_buf[..7] == b"CONNECT"` (ligne 16) est sensible à la casse alors que `tunnel.rs` (ligne 28) utilise `eq_ignore_ascii_case("CONNECT")`.




* **Attaque Slowloris sur `handle_plain_http` (`src/proxy/server.rs`, lignes 32-47) et `handle_connect_tunnel` (`src/proxy/tunnel.rs`, lignes 18-47 et 92-115)** :
* Si un client envoie `"GET /"` sans `\r\n\r\n`, `is_connect_request` retourne `false` en 5 ms et passe la main à `handle_plain_http`, où `socket.read(&mut temp).await` (ligne 37) attend **sans aucun timeout**.


* De même, si un client envoie `"CONNECT "` (7 octets) puis cesse d'écrire, `is_connect_request` retourne `true` immédiatement et `handle_connect_tunnel` bloque indéfiniment sur `client.read(&mut temp).await` (ligne 19).


* Enfin, lors de la lecture de la réponse du proxy amont (`src/proxy/tunnel.rs`, ligne 93 `upstream.read(&mut u_temp).await`), **aucun timeout** n'est appliqué après l'établissement du socket TCP : un proxy résidentiel défaillant qui accepte le SYN/ACK TCP mais ne répond jamais bloque la tâche indéfiniment.




* **Complexité quadratique et débordement de buffer d'en-tête** :
* `req_buf` et `buf` sont vérifiés avec `buf.len() > 8192` après l'extension d'un bloc de 1 024 octets (taille maximale effective de 9 216 octets), et `find_double_crlf` reparcourt l'intégralité du vecteur depuis l'indice 0 à chaque lecture partielle.


* Dans `src/proxy/tunnel.rs` (lignes 225-232), `find_double_crlf` est réimplémenté manuellement au lieu de partager la fonction `windows(4)` de `src/proxy/server.rs` (lignes 61-63).





#### D. Gestion des données sensibles et permissions disque

* **Stockage en clair avec permissions par défaut (`src/pool.rs`, lignes 201 et 330 ; `src/main.rs`, lignes 175-180)** :
* `ProxyPool::save_to_config()` écrit les identifiants complets (`host:port:username:password`) dans `~/.config/oplire/proxies.txt` via `fs::write` sans restreindre les permissions du répertoire (`0700`) ni du fichier (`0600`) sous Unix.




* **Fuite potentielle de `PROXIWARE_API_KEY` et des mots de passe (`src/main.rs`, ligne 112 ; `src/pool.rs`, ligne 14)** :
* Dans `src/main.rs` (lignes 110-115), `#[arg(long, env = "PROXIWARE_API_KEY")]` sur `api_key: String` ne comporte pas `hide_env_values = true`. Si `PROXIWARE_API_KEY` est définie dans l'environnement, exécuter `oplire proxies fetch --help` affiche la clé secrète en clair dans la sortie standard.


* Les structures `ProxyEntry` (`src/pool.rs`, ligne 14) et `ProxiesAction` (`src/main.rs`, ligne 93) dérivent `Debug` de manière standard : tout formatage `{:?}` dans un log de debug exposerait `password` et `api_key` en clair.


* Point positif : les appels `info!`, `warn!` et `debug!` actuels dans `src/proxy/tunnel.rs` et `src/pool.rs` ne journalisent que `proxy.provider`, `proxy.host` et `proxy.port`, sans inclure `username`, `password` ni l'en-tête `Proxy-Authorization`.





---

### 4.2 Fiabilité Asynchrone et Runtime Tokio

#### A. Sûreté d'annulation (*Cancellation Safety*) dans `tokio::select!`

* **Dans `handle_connect_tunnel` (`src/proxy/tunnel.rs`, lignes 147-162 et 205-219)** :
* `tokio::select!` met en compétition `copy_bidirectional(&mut client, &mut upstream)` et `rotation_rx.recv()`.


* Lorsque `rotation_rx.recv()` se déclenche, le future `copy_bidirectional` est abandonné (*dropped*). Bien que `copy_bidirectional` ne soit pas *cancellation-safe* si l'on souhaitait réutiliser les flux (des octets lus dans son buffer interne mais non encore écrits seraient perdus), l'objectif ici est précisément la **destruction immédiate du tunnel**. En revanche, les flux `client` et `upstream` sont simplement détruits par sortie de portée sans appel explicite à `shutdown().await`, ce qui peut retarder la notification de fermeture TLS côté client si des buffers OS sont en attente.




* **Dans `start_watch_mode` (`src/watch.rs`, lignes 130-133)** :
* Lorsque `log_path` est `Some(path)`, le code exécute :
```rust
tokio::select! {
    res = monitor_log_file(path, pool.clone(), debounce_ms) => res,
    res = monitor_http_upstream(upstream, pool, debounce_ms) => res,
}

```


* Contrairement à la documentation (`src/watch.rs`, ligne 100 et `src/main.rs`, ligne 44) qui décrit `monitor_http_upstream` comme un mode de repli (*fallback*) lorsque les logs sont indisponibles, ce `tokio::select!` exécute **les deux boucles simultanément** dès qu'un fichier de log existe.


* Conséquences :
1. Si `monitor_log_file` échoue avec une erreur d'entrée/sortie initiale (`File::open(&path).await?`), `tokio::select!` termine immédiatement `start_watch_mode` et annule `monitor_http_upstream` au lieu de basculer sur le fallback HTTP.


2. Lors d'une erreur 429, le log local et le endpoint HTTP `/v1/models` peuvent tous deux détecter l'erreur dans la même fenêtre et appeler `rotate_proxy(&pool).await` chacun de leur côté (leurs compteurs/temporisations de debounce étant indépendants), provoquant une **double rotation** qui saute un proxy valide.







#### B. Surveillance du fichier de logs (`src/watch.rs`, lignes 149-221)

* **Gestion défaillante de la rotation de logs (`logrotate`)** :
* `File::open(&path).await?` (ligne 154) ouvre un descripteur sur l'inode initial une seule fois avant la boucle `loop`.


* Dans la boucle, `tokio::fs::metadata(&path).await` (ligne 164) inspecte la taille du chemin `path` sur le système de fichiers. Si `opencode` ou `logrotate` renomme `opencode.log` en `opencode.log.1` et recrée un nouveau `opencode.log` plus court (`file_len < offset`), `monitor_log_file` remet `offset = 0` (ligne 171) et fait `file.seek(SeekFrom::Start(0))` **sur l'ancien descripteur ouvert (`opencode.log.1`)** au lieu de rouvrir `File::open(&path)` ! Il relit alors les premières lignes historiques de l'ancien fichier et ne lit jamais le nouveau fichier `opencode.log`.




* **Allocation mémoire non bornée et troncature de lignes** :
* `let bytes_to_read = (file_len - offset) as usize; buffer.resize(bytes_to_read, 0);` (lignes 176-177) alloue en mémoire vive la totalité du delta du fichier d'un seul tenant, sans plafond (ex. 64 Ko max par lecture) ni réduction de capacité (`shrink_to`).


* La lecture par `read_exact` suivie de `text.lines()` (ligne 188) ne conserve pas le fragment de ligne incomplet en fin de buffer si une écriture d'OpenCode est interceptée au milieu d'un flush (absence de délimiteur `\n` final), ce qui peut scinder un motif `"status=429"` en deux lectures successives et empêcher sa détection.





#### C. Gestion des clients HTTP `reqwest` et des Runtimes Tokio

* **Multiples instanciations de `tokio::runtime::Runtime` et `reqwest::blocking` (`src/main.rs`)** :
* `main()` est synchrone et instancie pas moins de **8 runtimes Tokio multi-threadés distincts** via `tokio::runtime::Runtime::new().unwrap()` (lignes 400, 426, 468, 485, 559, 576, 598, 629, 661, 714), tout en mélangeant `futures::executor::block_on` aux lignes 692 et 721 et `reqwest::blocking::Client` aux lignes 183 et 250.


* Dans les commandes `Watch`, `Daemon` et `Proxy`, un premier runtime complet (`rt_test`) est créé uniquement pour lire le proxy actif à afficher dans la bannière (lignes 400, 468, 559), détruit, puis un second runtime (`rt`) est créé quelques lignes plus bas (lignes 426, 485, 576).




* **Configuration des timeouts `reqwest` (`src/pool.rs`, lignes 450-453 et 469-471 ; `src/watch.rs`, lignes 228-230)** :
* Les clients `reqwest::Client` définissent un `.timeout(...)` global (6 s ou 10 s), mais pas de `.connect_timeout(...)` distinct, et `monitor_http_upstream` ne force pas `.no_proxy()` lorsqu'il interroge l'instance locale OpenCode Zen (`http://localhost:3000/v1/models`).





---

### 4.3 Robustesse, Gestion d'Erreurs, Paniques et Conformité HTTP

#### A. Inventaire exhaustif des `.unwrap()`, indexations de slices et conversions risquées

| Fichier & Ligne | Expression | Risque de Panic / Comportement |
| --- | --- | --- |
| `src/proxy/server.rs:16` | `&peek_buf[..7]` | Sûr (protégé par la garde `Ok(n) if n >= 7`), mais sensible à la casse.

 |
| `src/proxy/server.rs:18` | `peek_buf[..n] != b"CONNECT"[..n]` | Sûr car `peek_buf` est de taille 7 et le cas `n >= 7` est filtré à la ligne 16.

 |
| `src/proxy/server.rs:72` | `"127.0.0.1:8080".parse().unwrap()` | Littéral statique valide, mais masque l'erreur de parsing de `listen_addr`.

 |
| `src/proxy/tunnel.rs:230` | `&buf[i..i + 4]` | Sûr grâce à `buf.len().saturating_sub(3)`, mais redondant avec `windows(4)`.

 |
| `src/pool.rs:175` | `saved_idx % proxies.len()` | Sûr (protégé par `if proxies.is_empty()`), division par zéro évitée.

 |
| `src/pool.rs:404` | `idx % guard.len()` | Sûr (protégé par `if guard.is_empty()` à la ligne 401).

 |
| `src/pool.rs:417` | `guard[0].host`, `guard[0].port` | Sûr (protégé par `if guard.len() <= 1` et `else` de `guard.is_empty()`).

 |
| `src/watch.rs:176` | `(file_len - offset) as usize` | Risque de troncature `u64` vers `usize` sur cible 32 bits et **panic OOM** sur `buffer.resize(bytes_to_read, 0)` (ligne 177) si le fichier de log est massif.

 |
| `src/main.rs:400, 426, 468, 485, 559, 576, 598, 629, 661, 714` | `tokio::runtime::Runtime::new().unwrap()` | **10 appels `.unwrap()**` en code de production : provoque un panic immédiat si l'OS refuse la création d'un thread ou descripteur epoll/kqueue.

 |
| `src/main.rs:605` | `... % proxies.len()` | Sûr (protégé par `if proxies.is_empty() { return; }` ligne 600).

 |
| `src/main.rs:664-665` | `let total = pool.proxies.read().await.len(); ... % total` | **Risque de panic (division par zéro)** : le verrou `read()` est relâché entre `pool.rotate().await` et la ligne 664 ; si `proxies.txt` est vidé entre-temps, `total == 0` provoque un panic.

 |

#### B. Conformité HTTP/1.1 (RFC 9110 / RFC 9112) des réponses synthétiques

* **`src/proxy/server.rs` (ligne 54)** : Sur une requête `HEAD /health HTTP/1.1`, le serveur renvoie `"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nOK"`. Or, la RFC 9110 §9.3.2 interdit formellement d'inclure un corps de message (`"OK"`) dans la réponse à une requête `HEAD` (ce qui désynchronise les parseurs HTTP stricts).


* **`src/proxy/tunnel.rs` (lignes 31, 37, 63, 68, 84, 164, 179, 184)** : Les réponses d'erreur synthétiques (`400 Bad Request`, `413 Payload Too Large`, `502 Bad Gateway`, `504 Gateway Timeout`) sont émises sous la forme minimale `HTTP/1.1 <code> <reason>\r\n\r\n` sans les en-têtes `Content-Length: 0` ni `Connection: close`. Sans `Content-Length: 0` ni `Connection: close`, certains clients HTTP/1.1 persistent en attente de la fermeture explicite du flux pour délimiter la fin de la réponse d'erreur.


* **`src/proxy/tunnel.rs` (ligne 100)** : La vérification `if first_line.contains("200")` accepte à tort une ligne de statut amont telle que `HTTP/1.1 500 Error 2000` ou `HTTP/1.1 400 Port 2000 Closed`. Conformément à la RFC 9112 §4, le code de statut est le second jeton séparé par un espace (`parts.get(1) == Some(&"200")`).



---

## 5. Évaluation de la Testabilité et Matrice de Couverture

### 5.1 Chemins critiques actuellement dépourvus de tests

L'examen du dépôt montre que seuls 8 tests unitaires existent dans `src/pool.rs` (couvrant le parsing basique et deux cas simples de `rotate()`). Aucun test n'existe pour :

1. **`src/proxy/server.rs`** : Détection `is_connect_request` (casse mixte `connect`, paquets fragmentés, timeout 250 ms), réponses `GET /health` vs `HEAD /health` vs `400 Bad Request`.


2. **`src/proxy/tunnel.rs`** : Rejet des cibles `CONNECT` invalides ou SSRF (`127.0.0.1`, `169.254.169.254`, `192.168.1.1`), gestion du timeout sur lecture lente (Slowloris), injection de `Proxy-Authorization: Basic`, interruption effective d'un tunnel actif lors d'un signal `rotation_tx.send()`, et gestion de `RecvError::Lagged`.


3. **`src/pool.rs`** : Mots de passe contenant `:` ou `#`, rechargement à chaud (`reload_if_modified`), préservation d'un `--proxy-file` personnalisé face à `~/.config/oplire/proxies.txt`, permissions `0600` à l'écriture de `proxies.txt`, et rotations concurrentes sous forte contention Tokio.


4. **`src/watch.rs`** : Détection des motifs `is_rate_limit_line`, lecture incrémentale bornée, survie à une troncature ou un remplacement d'inode (`logrotate`), assemblage des lignes fragmentées entre deux lectures de 500 ms, et résolution multiplateforme des chemins (`XDG_DATA_HOME`, `HOME`, `LOCALAPPDATA`, `APPDATA`).


5. **`src/config.rs`** : Sérialisation/désérialisation JSON, tolérance aux clés obsolètes de la v2 (`api_key`, `max_retries`, `warp_delay`), et validation de l'adresse `listen`.



### 5.2 Matrice de tests unitaires et d'intégration recommandée

| ID Test | Type | Module Cible | Scénario vérifié | Résultat attendu |
| --- | --- | --- | --- | --- |
| **T-POOL-01** | Unitaire | `src/pool.rs` | Parsing `host:port:user:p:a:s:s` et URL avec caractères spéciaux | Mot de passe complet préservé, protocole validé |
| **T-POOL-02** | Unitaire | `src/pool.rs` | 50 tâches Tokio appelant `pool.rotate()` en parallèle | `active_index` exact, aucune famine ni panic |
| **T-POOL-03** | Unitaire | `src/pool.rs` | Écriture `save_to_config()` sous Unix | Fichier créé avec mode `0600` (`rw-------`) |
| **T-PROXY-01** | Intégration | `src/proxy/server.rs` | `GET /health`, `HEAD /health` et `POST /invalid` | `200 OK` (avec corps), `200 OK` (0 octet de corps), `400` |
| **T-PROXY-02** | Intégration | `src/proxy/tunnel.rs` | `CONNECT 127.0.0.1:22` et `CONNECT 169.254.169.254:80` | Rejet immédiat `403 Forbidden` (protection SSRF) |
| **T-PROXY-03** | Intégration | `src/proxy/tunnel.rs` | Client Slowloris envoyant `"CONNECT "` sans `\r\n\r\n` | Fermeture automatique par timeout (`408`/`400`) |
| **T-PROXY-04** | Intégration | `src/proxy/tunnel.rs` | Tunnel actif + appel `pool.rotate().await` | Interruption immédiate de `copy_bidirectional` |
| **T-WATCH-01** | Intégration | `src/watch.rs` | Écriture fragmentée `"status="` puis `"429\n"` + `logrotate` | Détection du 429 et réouverture du nouvel inode |

---

## 6. Recommandations Concrètes et Plan de Remédiation

### Priorité 1 (Critique & Haute) : Sécurisation du Tunneling, Anti-SSRF, Timeouts et Annulation (`src/proxy/tunnel.rs` & `src/proxy/server.rs`)

#### Modifications recommandées pour `src/proxy/tunnel.rs`

1. **S'abonner au canal de rotation dès l'entrée dans `handle_connect_tunnel**` (avant `pool.get_active().await`) afin d'éliminer la fenêtre TOCTOU.


2. **Traiter tout résultat de `rotation_rx.recv()**` (y compris `Err(RecvError::Lagged(_))`) comme un signal d'interruption du tunnel, et exécuter un `shutdown().await` propre sur les deux flux TCP.


3. **Encadrer la lecture des en-têtes client et de la réponse amont par un `tokio::time::timeout**` (ex. 5 s pour le client, 8 s pour la réponse du proxy amont) pour neutraliser les attaques Slowloris et les proxys fantômes.


4. **Valider la cible `target` contre les rebonds SSRF** (interdire les hôtes `localhost`, loopback `127.0.0.0/8`, `::1`, link-local `169.254.0.0/16` et réseaux privés RFC 1918), interdire les caractères de contrôle/CRLF dans `target`, et vérifier que le code de statut amont est exactement `"200"` sur le second jeton HTTP.



```rust
//! Correctif recommandé pour `src/proxy/tunnel.rs`

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::{lookup_host, TcpStream};
use tracing::{debug, info, warn};

use crate::pool::ProxyPool;
use crate::proxy::server::find_double_crlf;

const CLIENT_HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const UPSTREAM_HEADER_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_CLIENT_HEADER_BYTES: usize = 8192;
const MAX_UPSTREAM_HEADER_BYTES: usize = 4096;

/// Vérifie que la cible CONNECT est de la forme `host:port` valide et ne pointe pas
/// vers une adresse interne, loopback ou link-local (protection anti-SSRF).
pub async fn validate_connect_target(target: &str, allow_private: bool) -> bool {
    if target.is_empty()
        || target.bytes().any(|b| b <= b' ' || b == b'\r' || b == b'\n')
    {
        return false;
    }

    let Some((host, port_str)) = target.rsplit_once(':') else {
        return false;
    };
    let Ok(port) = port_str.parse::<u16>() else {
        return false;
    };
    if port == 0 || host.is_empty() {
        return false;
    }

    if allow_private {
        return true;
    }

    let clean_host = host.trim_start_matches('[').trim_end_matches(']');
    if clean_host.eq_ignore_ascii_case("localhost")
        || clean_host.ends_with(".localhost")
        || clean_host.ends_with(".local")
        || clean_host.ends_with(".internal")
    {
        return false;
    }

    if let Ok(ip) = clean_host.parse::<IpAddr>() {
        return is_public_ip(ip);
    }

    // En mode direct ou pour éviter le DNS rebinding vers loopback, vérifier la résolution locale
    if let Ok(addrs) = tokio::time::timeout(Duration::from_secs(3), lookup_host(target)).await {
        if let Ok(iter) = addrs {
            let mut found = false;
            for sock in iter {
                found = true;
                if !is_public_ip(sock.ip()) {
                    return false;
                }
            }
            return found;
        }
    }

    false
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_private()
                && !v4.is_link_local()
                && !v4.is_broadcast()
                && !v4.is_documentation()
                && !v4.is_unspecified()
                && v4.octets()[0] != 100 // CGNAT 100.64.0.0/10
        }
        IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_unique_local()
                && !v6.is_unicast_link_local()
                && !v6.to_ipv4_mapped().is_some_and(|v4| !is_public_ip(IpAddr::V4(v4)))
        }
    }
}

async fn read_headers_bounded(
    stream: &mut TcpStream,
    max_len: usize,
    timeout_dur: Duration,
) -> Result<(Vec<u8>, usize), &'static str> {
    let mut buf = Vec::with_capacity(1024);
    let mut temp = [0u8; 1024];

    let read_fut = async {
        loop {
            let search_start = buf.len().saturating_sub(3);
            match stream.read(&mut temp).await {
                Ok(0) => return Err("eof"),
                Ok(n) => {
                    buf.extend_from_slice(&temp[..n]);
                    if let Some(rel_pos) = find_double_crlf(&buf[search_start..]) {
                        return Ok(search_start + rel_pos);
                    }
                    if buf.len() > max_len {
                        return Err("too_large");
                    }
                }
                Err(_) => return Err("io_error"),
            }
        }
    };

    match tokio::time::timeout(timeout_dur, read_fut).await {
        Ok(Ok(pos)) => Ok((buf, pos)),
        Ok(Err(reason)) => Err(reason),
        Err(_) => Err("timeout"),
    }
}

pub async fn handle_connect_tunnel(mut client: TcpStream, pool: ProxyPool) {
    let (req_buf, req_end) =
        match read_headers_bounded(&mut client, MAX_CLIENT_HEADER_BYTES, CLIENT_HEADER_TIMEOUT).await
        {
            Ok(res) => res,
            Err("too_large") => {
                let _ = client
                    .write_all(b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }
            Err("timeout") => {
                let _ = client
                    .write_all(b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }
            Err(_) => return,
        };

    let header_str = String::from_utf8_lossy(&req_buf[..req_end]);
    let first_line = header_str.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();

    if parts.len() < 2 || !parts[0].eq_ignore_ascii_case("CONNECT") {
        warn!("Invalid CONNECT request: {}", first_line);
        let _ = client
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    }

    let target = parts[1].to_string();
    if !validate_connect_target(&target, false).await {
        warn!("Rejected forbidden or invalid CONNECT target: {}", target);
        let _ = client
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    }

    // Abonnement AVANT la sélection du proxy pour éviter toute condition de course TOCTOU
    let mut rotation_rx = pool.subscribe_rotation();
    let active_proxy = pool.get_active().await;

    match active_proxy {
        Some(proxy) => {
            if !proxy.protocol.eq_ignore_ascii_case("http")
                && !proxy.protocol.eq_ignore_ascii_case("https")
            {
                warn!(
                    "Unsupported upstream proxy protocol '{}' for {}:{}",
                    proxy.protocol, proxy.host, proxy.port
                );
                let _ = client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }

            let proxy_addr = format!("{}:{}", proxy.host, proxy.port);
            debug!("Tunneling CONNECT {} via proxy {}", target, proxy_addr);

            let mut upstream = match tokio::time::timeout(
                UPSTREAM_CONNECT_TIMEOUT,
                TcpStream::connect(&proxy_addr),
            )
            .await
            {
                Ok(Ok(stream)) => stream,
                Ok(Err(e)) => {
                    warn!("Failed to connect to upstream proxy {}: {}", proxy_addr, e);
                    let _ = client
                        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
                Err(_) => {
                    warn!("Timeout connecting to upstream proxy {}", proxy_addr);
                    let _ = client
                        .write_all(b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
            };

            let mut upstream_req = format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n", target, target);
            if let Some(auth_header) = proxy.basic_auth_header() {
                upstream_req.push_str(&format!("Proxy-Authorization: {}\r\n", auth_header));
            }
            upstream_req.push_str("Proxy-Connection: Keep-Alive\r\n\r\n");

            if let Err(e) = upstream.write_all(upstream_req.as_bytes()).await {
                warn!("Failed to write CONNECT to upstream proxy: {}", e);
                let _ = client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }

            let (resp_buf, resp_end) = match read_headers_bounded(
                &mut upstream,
                MAX_UPSTREAM_HEADER_BYTES,
                UPSTREAM_HEADER_TIMEOUT,
            )
            .await
            {
                Ok(res) => res,
                Err("timeout") => {
                    warn!("Timeout reading CONNECT response from {}", proxy_addr);
                    let _ = client
                        .write_all(b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
                Err(_) => {
                    let _ = client
                        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
            };

            let resp_header = String::from_utf8_lossy(&resp_buf[..resp_end]);
            let status_line = resp_header.lines().next().unwrap_or("");
            let status_code = status_line.split_whitespace().nth(1).unwrap_or("");

            if status_code != "200" {
                warn!("Upstream proxy rejected CONNECT {}: {}", target, status_line);
                let _ = client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }

            if client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }

            if req_end < req_buf.len() && upstream.write_all(&req_buf[req_end..]).await.is_err() {
                return;
            }
            if resp_end < resp_buf.len() && client.write_all(&resp_buf[resp_end..]).await.is_err() {
                return;
            }

            info!(
                "Tunnel established: {} ↔ {} ({})",
                target, proxy.host, proxy.provider
            );
            tokio::select! {
                res = copy_bidirectional(&mut client, &mut upstream) => {
                    if let Err(e) = res {
                        debug!("Tunnel {} finished: {}", target, e);
                    }
                }
                rot = rotation_rx.recv() => {
                    info!(
                        "Proxy rotation signal ({:?}): aborting active CONNECT tunnel to {}",
                        rot, target
                    );
                    let _ = client.shutdown().await;
                    let _ = upstream.shutdown().await;
                }
            }
        }
        None => {
            debug!("Direct CONNECT tunnel to {}", target);
            let mut upstream =
                match tokio::time::timeout(UPSTREAM_CONNECT_TIMEOUT, TcpStream::connect(&target))
                    .await
                {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(e)) => {
                        warn!("Direct connect to {} failed: {}", target, e);
                        let _ = client
                            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                    Err(_) => {
                        warn!("Timeout connecting directly to {}", target);
                        let _ = client
                            .write_all(b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                };

            if client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }

            if req_end < req_buf.len() && upstream.write_all(&req_buf[req_end..]).await.is_err() {
                return;
            }

            info!("Direct tunnel established: {}", target);
            tokio::select! {
                res = copy_bidirectional(&mut client, &mut upstream) => {
                    if let Err(e) = res {
                        debug!("Direct tunnel {} finished: {}", target, e);
                    }
                }
                rot = rotation_rx.recv() => {
                    info!(
                        "Proxy rotation signal ({:?}): aborting active direct tunnel to {}",
                        rot, target
                    );
                    let _ = client.shutdown().await;
                    let _ = upstream.shutdown().await;
                }
            }
        }
    }
}

```

#### Modifications recommandées pour `src/proxy/server.rs`

1. **Limiter la concurrence** via un `Arc<tokio::sync::Semaphore>` (ex. 512 connexions simultanées max).


2. **Survivre aux erreurs transitoires d'acceptation TCP** (`listener.accept().await`) en journalisant l'erreur et en patientant 50 ms au lieu de faire crasher le démon avec `?`.


3. **Corriger `HEAD /health` (RFC 9110)**, ajouter un timeout de lecture de 5 s sur `handle_plain_http`, supporter `eq_ignore_ascii_case` dans `is_connect_request` et émettre un avertissement explicite si l'adresse d'écoute n'est pas loopback.



```rust
//! Correctif recommandé pour `src/proxy/server.rs`

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::pool::ProxyPool;
use crate::proxy::tunnel::handle_connect_tunnel;

const MAX_CONCURRENT_CONNECTIONS: usize = 512;

pub(crate) fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

async fn is_connect_request(socket: &TcpStream) -> bool {
    let mut peek_buf = [0u8; 8];
    let check = async {
        loop {
            match socket.peek(&mut peek_buf).await {
                Ok(0) => return false,
                Ok(n) if n >= 8 => return peek_buf[..8].eq_ignore_ascii_case(b"CONNECT "),
                Ok(n) => {
                    if !peek_buf[..n].eq_ignore_ascii_case(&b"CONNECT "[..n]) {
                        return false;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(_) => return false,
            }
        }
    };
    tokio::time::timeout(Duration::from_millis(250), check)
        .await
        .unwrap_or(false)
}

async fn handle_plain_http(mut socket: TcpStream) {
    let mut buf = Vec::with_capacity(512);
    let mut temp = [0u8; 1024];

    let read_res = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.read(&mut temp).await {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&temp[..n]);
                    if find_double_crlf(&buf).is_some() || buf.len() > 8192 {
                        break;
                    }
                }
                Err(_) => return Err(()),
            }
        }
        Ok(())
    })
    .await;

    if !matches!(read_res, Ok(Ok(()))) {
        return;
    }

    let head = String::from_utf8_lossy(&buf);
    let first_line = head.lines().next().unwrap_or("");

    let response: &[u8] = if first_line.starts_with("GET /health ") {
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nOK"
    } else if first_line.starts_with("HEAD /health ") {
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n"
    } else {
        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    };

    let _ = socket.write_all(response).await;
    let _ = socket.shutdown().await;
}

pub fn parse_listen_addr(raw: &str) -> SocketAddr {
    let normalized = if let Some(port) = raw.strip_prefix(':') {
        format!("127.0.0.1:{}", port)
    } else {
        raw.to_string()
    };

    normalized.parse().unwrap_or_else(|e| {
        warn!(
            "Invalid listen address '{} ({})', falling back to 127.0.0.1:8080",
            raw, e
        );
        SocketAddr::from(([127, 0, 0, 1], 8080))
    })
}

pub async fn start_proxy_server(
    listen_addr: String,
    pool: Option<ProxyPool>,
) -> anyhow::Result<()> {
    let addr = parse_listen_addr(&listen_addr);
    if !addr.ip().is_loopback() {
        warn!(
            "SECURITY WARNING: Proxy server is binding to non-loopback address {} without authentication!",
            addr
        );
    }

    let pool = pool.unwrap_or_default();
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Proxy server listening on http://{}", addr);

    loop {
        let (socket, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                warn!("Transient TCP accept error: {}", e);
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };

        let Ok(permit) = semaphore.clone().try_acquire_owned() else {
            warn!("Connection limit ({}) reached, dropping socket", MAX_CONCURRENT_CONNECTIONS);
            continue;
        };

        let pool_clone = pool.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if is_connect_request(&socket).await {
                handle_connect_tunnel(socket, pool_clone).await;
            } else {
                handle_plain_http(socket).await;
            }
        });
    }
}

```

---

### Priorité 2 (Haute) : Concurrence sans contention, Permissions `0600` et Parsing Robuste (`src/pool.rs`)

1. **Masquer le mot de passe dans `Debug**` pour `ProxyEntry` afin d'éviter toute fuite accidentelle dans les traces.


2. **Supprimer le maintien de `RwLockWriteGuard` à travers `.await**` dans `reload_if_modified()` et limiter la vérification `metadata` disque au moyen d'un horodatage atomique (`last_check_ms` espacé d'au moins 1 000 ms) afin d'éliminer les appels système sur chaque requête `CONNECT`.


3. **Utiliser `fetch_add(1, Ordering::SeqCst)**` dans `rotate()` et mettre à jour `last_index_mtime` lors de la sauvegarde pour éviter les rechargements redondants.


4. **Appliquer des permissions strictes (`0700` sur le dossier, `0600` sur `proxies.txt`)** sous Unix et supporter `APPDATA` / `USERPROFILE` sous Windows ainsi que `Library/Application Support` sous macOS.


5. **Utiliser `splitn(4, ':')**` dans `ProxyEntry::parse` afin de supporter les mots de passe contenant des deux-points `:`.



```rust
//! Correctifs ciblés recommandés pour `src/pool.rs`

impl std::fmt::Debug for ProxyEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyEntry")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***REDACTED***"))
            .field("provider", &self.provider)
            .field("protocol", &self.protocol)
            .finish()
    }
}

/// Écriture atomique et sécurisée (permissions 0600 sous Unix)
fn write_Restricted_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

```

Dans `ProxyEntry::parse` (`src/pool.rs`, lignes 85-110), remplacer `clean_line.split(':').collect()` par `splitn(4, ':')` :

```rust
let parts: Vec<&str> = clean_line.splitn(4, ':').collect();
match parts.len() {
    4 => {
        let host = parts[0].to_string();
        let port = parts[1].parse::<u16>().ok()?;
        let username = Some(parts[2].to_string());
        let password = Some(parts[3].to_string());
        let provider = infer_provider(&host, port);
        Some(Self {
            host,
            port,
            username,
            password,
            provider,
            protocol: "http".to_string(),
        })
    }
    2 => {
        let host = parts[0].to_string();
        let port = parts[1].parse::<u16>().ok()?;
        let provider = infer_provider(&host, port);
        Some(Self {
            host,
            port,
            username: None,
            password: None,
            provider,
            protocol: "http".to_string(),
        })
    }
    _ => None,
}

```

Dans `ProxyPool::rotate` (`src/pool.rs`, lignes 408-445), éviter la condition de course sur l'index et ne pas maintenir le verrou de lecture pendant l'écriture disque :

```rust
pub async fn rotate(&self) -> Option<ProxyEntry> {
    self.reload_if_modified().await;

    let (next_idx, total, entry) = {
        let guard = self.proxies.read().await;
        if guard.len() <= 1 {
            if guard.is_empty() {
                tracing::warn!("Cannot rotate: proxy pool is empty");
            } else {
                tracing::warn!(
                    "Cannot rotate: proxy pool only has 1 proxy ({}:{})",
                    guard[0].host,
                    guard[0].port
                );
            }
            return None;
        }
        let total = guard.len();
        let prev = self.active_index.fetch_add(1, Ordering::SeqCst);
        let next_idx = prev.wrapping_add(1) % total;
        self.active_index.store(next_idx, Ordering::SeqCst);
        (next_idx, total, guard.get(next_idx).cloned())
    };

    Self::save_index_async(next_idx).await;
    if let Ok(meta) = tokio::fs::metadata(Self::index_file_path()).await {
        if let Ok(mtime) = meta.modified() {
            *self.last_index_mtime.write().await = Some(mtime);
        }
    }

    let _ = self.rotation_tx.send(next_idx);

    if let Some(ref p) = entry {
        info!(
            "Proxy rotated to index [{}/{}]: {} ({}:{})",
            next_idx + 1,
            total,
            p.provider,
            p.host,
            p.port
        );
    }
    entry
}

```

---

### Priorité 3 (Haute) : Surveillance Résiliente des Logs, Plafond Mémoire et Rotation d'Inode (`src/watch.rs`)

1. **Corriger la résolution de chemins sous Windows** en remplaçant `.join("opencode\\log\\opencode.log")` par `.join("opencode").join("log").join("opencode.log")` (`src/watch.rs`, lignes 45 et 52).


2. **Plafonner la lecture incrémentale** à `MAX_READ_CHUNK = 64 * 1024` octets (64 Ko) pour prévenir tout OOM DoS (VULN-04) et conserver le reliquat de ligne incomplet (`pending_line`) entre deux scrutations de 500 ms.


3. **Rouvrir le fichier (`File::open(&path).await`)** lorsque `file_len < offset` ou lorsque l'identifiant du fichier (inode/ctime) change suite à un `logrotate` (VULN-05).


4. **Ne plus exécuter `monitor_http_upstream` en concurrence aveugle avec `monitor_log_file**` : si le log existe, surveiller le log et ne basculer sur le polling HTTP que si le fichier de log devient durablement inaccessible.



```rust
//! Correctif recommandé pour `monitor_log_file` dans `src/watch.rs`

const MAX_READ_CHUNK: usize = 64 * 1024; // 64 Ko maximum par itération

async fn monitor_log_file(
    path: PathBuf,
    pool: Option<ProxyPool>,
    debounce_ms: u64,
) -> anyhow::Result<()> {
    let mut file = File::open(&path).await?;
    let mut offset = file.seek(SeekFrom::End(0)).await.unwrap_or(0);
    info!(
        "Attached to log file at offset {} bytes (ignoring past history)",
        offset
    );

    let mut buffer = vec![0u8; 8192];
    let mut carry = String::new();

    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;

        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => continue,
        };

        let file_len = meta.len();
        if file_len < offset {
            info!("Log file truncated or rotated, reopening file and resetting offset to 0");
            if let Ok(new_file) = File::open(&path).await {
                file = new_file;
            }
            offset = 0;
            carry.clear();
        }

        if file_len > offset {
            let delta = (file_len - offset).min(MAX_READ_CHUNK as u64) as usize;
            if buffer.len() < delta {
                buffer.resize(delta, 0);
            }

            if let Err(e) = file.seek(SeekFrom::Start(offset)).await {
                warn!("Seek error in log file: {}", e);
                continue;
            }

            match file.read(&mut buffer[..delta]).await {
                Ok(0) => {
                    // Le descripteur pointe potentiellement vers un ancien inode renommé
                    if let Ok(new_file) = File::open(&path).await {
                        file = new_file;
                        offset = 0;
                    }
                }
                Ok(n) => {
                    offset += n as u64;
                    carry.push_str(&String::from_utf8_lossy(&buffer[..n]));

                    let mut triggered = false;
                    while let Some(newline_pos) = carry.find('\n') {
                        let line = &carry[..newline_pos];
                        if is_rate_limit_line(line) {
                            warn!("OpenCode limit detected in log: {}", line.trim());
                            println!(
                                "\n{} Limit detected in OpenCode log!",
                                "[WARN]".yellow().bold()
                            );
                            triggered = true;
                        }
                        carry.drain(..=newline_pos);
                        if triggered {
                            break;
                        }
                    }

                    // Garde-fou contre une ligne sans '\n' dépassant 16 Ko
                    if carry.len() > 16 * 1024 {
                        if is_rate_limit_line(&carry) {
                            triggered = true;
                        }
                        carry.clear();
                    }

                    if triggered {
                        carry.clear();
                        rotate_proxy(&pool).await;
                        tokio::time::sleep(Duration::from_millis(debounce_ms)).await;
                        if let Ok(new_file) = File::open(&path).await {
                            file = new_file;
                            offset = file.seek(SeekFrom::End(0)).await.unwrap_or(0);
                        }
                    }
                }
                Err(e) => {
                    warn!("Error reading new bytes from log file: {}", e);
                }
            }
        }
    }
}

```

---

### Priorité 4 (Moyenne) : Unification du Runtime Tokio, Masquage CLI et CI (`src/main.rs` & `.github/workflows/build.yml`)

1. **Annoter `async fn main()` avec `#[tokio::main]**` dans `src/main.rs` afin de supprimer les 10 appels `tokio::runtime::Runtime::new().unwrap()`, les 2 appels `futures::executor::block_on` et la feature `blocking` de `reqwest` dans `Cargo.toml`.


2. **Masquer la clé d'API dans Clap** (`src/main.rs`, ligne 112) avec `#[arg(long, env = "PROXIWARE_API_KEY", hide_env_values = true)]` pour empêcher l'affichage en clair de `PROXIWARE_API_KEY` dans `oplire proxies fetch --help`.


3. **Ajouter la validation qualité obligatoire dans `.github/workflows/build.yml**` avant l'étape `Build` (`cargo fmt -- --check`, `cargo clippy -- -D warnings`, `cargo test --all-targets`).



---

## 7. Suite de Tests de Régression à Intégrer

Les tests suivants peuvent être ajoutés dans `src/pool.rs` et `src/proxy/server.rs` (ou `tests/integration.rs`) pour valider les correctifs sans dépendance externe :

```rust
#[cfg(test)]
mod audit_regression_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn test_parse_password_with_colons() {
        let entry = ProxyEntry::parse(
            "188.220.163.167:1337:alice:p@ss:w0rd:123 # proxiware-isp",
            "custom",
        )
        .expect("Should parse password containing colons");
        assert_eq!(entry.host, "188.220.163.167");
        assert_eq!(entry.port, 1337);
        assert_eq!(entry.username.as_deref(), Some("alice"));
        assert_eq!(entry.password.as_deref(), Some("p@ss:w0rd:123"));
        assert_eq!(entry.provider, "proxiware-isp");
    }

    #[test]
    fn test_debug_redacts_password() {
        let entry = ProxyEntry::parse("1.2.3.4:8080:user:supersecret", "custom").unwrap();
        let debug_str = format!("{:?}", entry);
        assert!(!debug_str.contains("supersecret"));
        assert!(debug_str.contains("***REDACTED***"));
    }

    #[tokio::test]
    async fn test_concurrent_rotations_consistency() {
        let proxies: Vec<ProxyEntry> = (1..=5)
            .map(|i| ProxyEntry::parse(&format!("10.0.0.{}:8080", i), "custom").unwrap())
            .collect();
        let pool = ProxyPool::new(proxies);
        pool.active_index.store(0, Ordering::SeqCst);

        let mut handles = Vec::new();
        for _ in 0..25 {
            let p = pool.clone();
            handles.push(tokio::spawn(async move {
                p.rotate().await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let final_idx = pool.active_index.load(Ordering::SeqCst) % 5;
        assert_eq!(final_idx, 0, "25 rotations over 5 proxies must wrap around to index 0");
    }

    #[tokio::test]
    async fn test_ssrf_target_validation() {
        use crate::proxy::tunnel::validate_connect_target;
        assert!(!validate_connect_target("127.0.0.1:8080", false).await);
        assert!(!validate_connect_target("localhost:3000", false).await);
        assert!(!validate_connect_target("169.254.169.254:80", false).await);
        assert!(!validate_connect_target("192.168.1.1:443", false).await);
        assert!(!validate_connect_target("[::1]:8080", false).await);
        assert!(!validate_connect_target("evil.com:443\r\nX-Injected: 1", false).await);
        assert!(validate_connect_target("1.1.1.1:443", false).await);
    }

    #[tokio::test]
    async fn test_head_health_has_no_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            crate::proxy::server::handle_plain_http(socket).await;
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"HEAD /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(resp_str.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp_str.ends_with("\r\n\r\n"), "HEAD response must not include body bytes");
    }
}

```