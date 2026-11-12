# Analyse Forensique et Modélisation Quantitative des Quotas OpenCode Free
**Document de Référence Technique — Projet `oplire` (v3.0.0)**
**Auteur :** Équipe d'Ingénierie `oplire`
**Date :** Novembre 2026

---

## 1. Résumé Exécutif

L'analyse forensique détaillée du transcript d'une session de production intensive OpenCode (`overlays-r-els-a7-et-alias.json`, Session ID `ses_e2fdc9104ffez2EITe9H8lEZXI`, 663 messages, modèle `opencode/deepseek-v4.1-flash-free`) a permis de percer la logique exacte du limiteur de débit et du système de quotas d'OpenCode sur ses modèles gratuits.

### Conclusions Majeures :
1. **Facteur Déclencheur Unique (Metric Driver) :**
   Le blocage HTTP 429 (`FreeUsageLimitError`, code `"provider.free-limit"`) est **exclusivement calibré sur les Output Tokens** (tokens de complétion générés).
2. **Plafond Empirique par IP :**
   Le quota par IP résidentielle/ISP est borné entre **60 189 et 65 147 tokens d'output** (moyenne empirique : **62 205 tokens**, soit nominalement **~60 000 tokens**).
3. **Indépendance des Autres Métriques :**
   Le quota est totalement insensible :
   - au volume d'input (150k à 353k tokens consommés sans corrélation avec la coupure),
   - au cache read (27M à 42M tokens lus),
   - au raisonnement / CoT (54k à 88k tokens),
   - au nombre de requêtes / tours de parole (149 à 247 messages assistant).
4. **Fenêtre Glissante de Recharge :**
   La durée de réactivation observée via `retryAfterMs` varie entre **8,09 h et 9,19 h** (moyenne : **~8,8 heures**).

---

## 2. Données Forensiques Détaillées

La session analysée s'est déroulée sur 4 cycles successifs de proxys résidentiels via le pool `oplire`, déclenchant 3 rotations automatiques lors des coupures de quota.

| Métrique | Proxy 1 (Cycle 1) | Proxy 2 (Cycle 2) | Proxy 3 (Cycle 3) | Proxy 4 (Cycle 4, Actif) |
|---|---|---|---|---|
| **Plage Messages** | `0` à `155` | `158` à `414` | `417` à `613` | `616` à `662` |
| **Total Messages** | 156 | 257 | 197 | 47 |
| **Messages Assistant** | 149 | 247 | 189 | 41 |
| **Durée Active** | 47,6 min | 39,9 min | 35,6 min | 7,1 min |
| **Input Tokens** | 150 782 | 308 658 | 352 914 | 26 783 |
| **Output Tokens (Coupure)** | **61 281** | **60 189** | **65 147** | **11 284** (en cours) |
| **Reasoning Tokens** | 88 889 | 54 954 | 64 939 | 12 961 |
| **Cache Read Tokens** | 27 441 024 | 42 562 816 | 36 269 952 | 10 310 528 |
| **Total Généré (Out+Reas)** | 150 170 | 115 143 | 130 086 | 24 245 |
| **Total Facturé (In+Out)** | 212 063 | 368 847 | 418 061 | 38 067 |
| **Événement Déclencheur** | 429 (`FreeUsageLimitError`) | 429 (`FreeUsageLimitError`) | 429 (`FreeUsageLimitError`) | Session Close (Actif) |
| **Délai Recharge (`retryAfterMs`)** | 33 076 000 ms (~9,19 h) | 32 881 000 ms (~9,13 h) | 29 108 000 ms (~8,09 h) | N/A (~81,9% restant) |

### Analyse de Dispersion Statistique

$$\bar{T}_{\text{output}} = \frac{61\,281 + 60\,189 + 65\,147}{3} = 62\,205.67 \approx 62\,206 \text{ tokens}$$

$$\sigma_{\text{output}} \approx 2\,133 \text{ tokens} \quad (\pm 3.4\%)$$

En comparaison avec les autres variables :
- Écart d'input : **+134 %** (de 150k à 353k)
- Écart de reasoning : **+62 %** (de 55k à 89k)
- Écart de cache read : **+55 %** (de 27,4M à 42,6M)
- Écart du nombre de requêtes : **+66 %** (de 149 à 247)
- Écart d'output tokens : **$\pm 3,4 \%$** seulement.

Cette quasi-invariance mathématique démontre de manière indiscutable que le bucket de jetons gratuit d'OpenCode alloue une enveloppe brute d'environ **60 000 tokens de complétion** par adresse IP source.

---

## 3. Formalisation Mathématique et Dimensionnement de Pool

### 3.1 Formule de Dimensionnement Minimal

Pour une charge de travail projetée nécessitant $T_{\text{output}}$ tokens de complétion :

$$N_{\text{proxys\_min}} = \left\lceil \frac{T_{\text{output}}}{60\,000} \right\rceil$$

### 3.2 Formule Opérationnelle Recommandée (+1 Réserve)

Afin d'absorber les variations de tokenisation, les micro-latences et d'éviter l'interruption inopinée d'un agent autonome en cours de tâche :

$$N_{\text{recommandé}} = \left\lceil \frac{T_{\text{output}}}{60\,000} \right\rceil + 1$$

### 3.3 Fenêtre de Rotation Temporelle

Soit $R \approx 8{,}8\text{ h}$ le temps de recharge moyen d'une IP.
Si une équipe consomme en continu un débit moyen d'output de $D_{\text{out}}$ (tokens/heure), le nombre minimal de proxys pour une boucle infinie sans jamais être bloqué est :

$$N_{\text{boucle\_infinie}} = \left\lceil \frac{D_{\text{out}} \times 8{,}8}{60\,000} \right\rceil + 1$$

*Exemple :*
Pour générer 25 000 tokens d'output par heure en continu (soit ~600k tokens/jour) :
$$N = \left\lceil \frac{25\,000 \times 8{,}8}{60\,000} \right\rceil + 1 = \lceil 3{,}67 \rceil + 1 = 4 + 1 = 5 \text{ proxys ISP}.$$

---

## 4. Outils Implémentés dans `oplire` CLI

### 4.1 Audit Automatique de Transcript : `oplire audit`

```bash
oplire audit <transcript.json>
oplire audit <transcript.json> --format json
```

Fonctionnalités :
- Analyse non-stricte et tolérante aux évolutions du format JSON OpenCode (`#[serde(default)]`).
- Segmentation automatique des cycles basée sur la détection des erreurs 429 (`FreeUsageLimitError`, `provider.free-limit`, `RateLimitError`).
- Détection du proxy résiduel actif et calcul du pourcentage de quota disponible restant.
- Sortie textuelle tabulaire avec colorisation ANSI ou JSON structuré.

### 4.2 Estimateur de Dimensionnement : `oplire estimate`

```bash
oplire estimate --tokens-out 150000 [--tokens-in 500000] [--quota-per-proxy 60000]
```

Recommande instantanément la configuration de pool adaptée à une session de travail planifiée.
