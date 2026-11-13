# Release Relay

Relais public des mises à jour d'applis [Tauri](https://tauri.app) dont le dépôt GitHub est privé.

Quand un dépôt passe en privé, les adresses `github.com/…/releases/latest/download/…` répondent 404, et un jeton ne doit jamais être embarqué dans une appli. Release Relay tient un jeton **en lecture seule** et sert de porte d'entrée publique : l'updater de Tauri, les boutons « Télécharger » et la page de présentation passent par lui. Un seul relais sert plusieurs dépôts.

Les installeurs ne transitent pas par le serveur : il redirige le client vers l'adresse signée et temporaire que donne GitHub.

## Routes

`{app}` est un nom déclaré dans la config ; toute autre valeur répond 404.

| Route | Rôle |
| --- | --- |
| `GET /{app}/latest.json` | `latest.json` de la dernière release, adresses `platforms.*.url` réécrites vers `/{app}/download/…` ; le reste (signatures comprises) est inchangé |
| `GET /{app}/download/{tag}/{fichier}` | 302 vers l'adresse signée de l'asset (le fichier doit être un asset de cette release) |
| `GET /{app}/latest/{plateforme}` | installeur de la dernière version pour une clé de `latest.json` (`windows-x86_64`…), pour les boutons « Télécharger » |
| `GET /{app}/raw/{chemin}` | fichier de la branche par défaut, seulement pour les chemins listés dans la config |
| `GET /{app}/` et `GET /{app}/{chemin}` | page de présentation : fichiers d'un dossier du dépôt (facultatif) |
| `GET /health` | le serveur répond |

Les routes `latest.json`, `download`, `latest` et `raw` passent avant la page.

### Page de présentation

Elle remplace GitHub Pages, qui ne publie pas depuis un dépôt privé avec un compte gratuit. Le relais sert les fichiers du dossier configuré, dans la version du **tag de la dernière release** (par défaut : la page décrit la version téléchargeable) ou de la branche par défaut.

- Un dossier sert son `index.html` ; `/{app}` et `/{app}/dossier` redirigent vers l'adresse avec `/` final (les liens relatifs de la page en dépendent).
- Rien en dehors du dossier, pas de fichier caché (`.quelque-chose`) ni de lien symbolique.
- Type MIME selon l'extension, `ETag` (réponse 304 quand le fichier n'a pas changé).
- Le dossier est listé d'un bloc à chaque nouvelle release, puis chaque fichier est lu une fois et gardé en mémoire : un fichier inchangé d'une release à l'autre n'est pas relu.

## Configuration

### Fichier TOML

Monté en lecture seule dans le conteneur (`/config/relay.toml`). Il ne contient aucun secret. Exemple complet et commenté : [config.example.toml](config.example.toml).

```toml
public_url = "https://releases.exemple.fr"   # adresse publique, celle du proxy HTTPS

# Facultatifs (valeurs par défaut)
# cache_ttl = 60      # secondes de cache des réponses GitHub
# proxy_hops = 1      # proxys devant le serveur (2 avec Cloudflare devant le proxy)
# rate_limit = 60     # requêtes par minute et par adresse (0 : sans limite)

[apps.twitch-desk]
repo = "powange/twitch-desk"
raw = ["operations.json"]

[apps.twitch-desk.site]   # facultatif
dir = "site"
source = "release"        # ou "branch"
```

- Nom d'appli : minuscules, chiffres et tirets (`health` est réservé).
- `raw` : chemins exacts, rien d'autre n'est servi sous `/{app}/raw/`.
- Sans section `site`, `/{app}/` répond 404.
- Une erreur dans le fichier empêche le démarrage, avec un message qui la décrit.

### Variables d'environnement

| Variable | Rôle |
| --- | --- |
| `GITHUB_TOKEN` | **obligatoire** : le jeton GitHub (voir ci-dessous) |
| `RELAY_CONFIG` | chemin du fichier TOML (`/config/relay.toml` dans l'image) |
| `RELAY_PORT` | port d'écoute (`8080`) |
| `RUST_LOG` | niveau des journaux (`info`) |

### Créer le jeton GitHub

Un jeton *fine-grained*, en lecture seule, limité aux dépôts relayés :

1. GitHub → Settings → Developer settings → Personal access tokens → **Fine-grained tokens** → *Generate new token*.
2. Nom : `release-relay`. Expiration : au choix (noter la date pour le renouveler).
3. *Resource owner* : ton compte. *Repository access* : **Only select repositories**, puis choisir les dépôts déclarés dans la config.
4. *Permissions* → *Repository permissions* : **Contents : Read-only** (*Metadata : Read-only* est ajouté d'office). Rien d'autre.
5. Générer, copier le jeton et le poser directement dans la variable `GITHUB_TOKEN` de la pile Portainer.

Le jeton n'est envoyé qu'à l'API GitHub, jamais aux adresses signées. Il n'apparaît ni dans les journaux ni dans les réponses, y compris en cas d'erreur. Au démarrage, le relais vérifie que chaque dépôt est visible et le note dans les journaux (`dépôt … accessible` ou `inaccessible`).

Pour ajouter un dépôt plus tard : l'ajouter aux dépôts du jeton (*Edit* sur le jeton), puis à la config, et redémarrer la pile.

## Déploiement (Portainer)

### 1. Fichier de config sur l'hôte

Créer `/opt/release-relay/relay.toml` d'après [config.example.toml](config.example.toml). Pour le placer ailleurs, donner son chemin dans la variable `RELAY_CONFIG_FILE` de la pile.

### 2. Pile

Stacks → Add stack → **Repository** :

- Repository URL : `https://github.com/powange/release-relay`
- Compose path : `docker-compose.yml`
- Environment variables : `GITHUB_TOKEN` (obligatoire), et au besoin `RELAY_CONFIG_FILE`, `RUST_LOG`

Le conteneur ne publie aucun port sur l'hôte : il rejoint le réseau `proxy-network`, par lequel Nginx Proxy Manager le joint. Il tourne avec un système de fichiers en lecture seule, sans privilège.

### 3. Nginx Proxy Manager

Proxy host `releases.exemple.fr` → scheme `http`, forward hostname `release-relay`, port `8080`, avec un certificat SSL. L'adresse doit correspondre à `public_url`.

Le relais lit l'adresse du client dans `X-Forwarded-For` (ajouté par NPM) : `proxy_hops = 1` par défaut ; `2` si Cloudflare est devant NPM.

### 4. Redéploiement automatique

À chaque modification sur `main`, le workflow [image.yml](.github/workflows/image.yml) vérifie le code (clippy, tests), construit l'image pour amd64 et arm64, la publie sur `ghcr.io/powange/release-relay` (étiquettes `latest`, version, et version-commit), puis appelle le webhook de la pile Portainer.

- Dans Portainer, sur la pile : activer *GitOps updates*, mécanisme **Webhook**, et copier l'adresse affichée. Cocher aussi *Re-pull image* (ou *Pull latest image*) pour que le redéploiement télécharge la nouvelle image `latest`.
- Dans le dépôt GitHub : Settings → Secrets and variables → Actions → *New repository secret* `PORTAINER_WEBHOOK_URL`, avec cette adresse. Ou en ligne de commande : `gh secret set PORTAINER_WEBHOOK_URL -R powange/release-relay` (la valeur est demandée sans être affichée).

Sans ce secret, l'image est publiée mais le job « Déploiement » échoue pour le signaler.

Dependabot propose chaque semaine les mises à jour des actions, et chaque mois celles des dépendances Rust et des images Docker.

## Côté appli Tauri

- Updater (`tauri.conf.json`) : `"endpoints": ["https://releases.exemple.fr/{app}/latest.json"]`. L'updater suit les redirections (client reqwest par défaut), pour le manifeste comme pour les installeurs.
- Boutons « Télécharger » de la page : `https://releases.exemple.fr/{app}/latest/windows-x86_64`, etc.
- Fichiers lus par l'appli : `https://releases.exemple.fr/{app}/raw/{chemin}`.

La release doit publier un asset `latest.json` (ce que fait `tauri-action`). Les adresses qu'il contient doivent désigner des assets de la même release : sinon le relais répond 502 au lieu de servir un manifeste faux.

## Fonctionnement et limites

- **Cache** : réponses GitHub gardées `cache_ttl` secondes (60). Si GitHub ne répond plus, la dernière réponse valide est resservie. Les adresses signées ne sont jamais gardées (elles expirent).
- **Redirections** : seules sont acceptées celles vers `release-assets.githubusercontent.com` et `objects.githubusercontent.com`. Le relais ne relaie aucune adresse arbitraire.
- **Limites** : 60 requêtes par minute et par adresse (au-delà : 429 avec `Retry-After`), 64 requêtes traitées en même temps, 30 s par requête, 15 s par appel à GitHub. Une adresse IPv6 compte pour tout son réseau /64.
- **Méthodes** : `GET` (et `HEAD`) seulement.
- **Releases** : `latest` est la dernière release publiée (ni brouillon, ni préversion). `/download/{tag}/…` accepte aussi les releases plus anciennes, pour un client qui a lu le manifeste juste avant une nouvelle release.

## Développement

```sh
cargo test                                   # tests unitaires et de bout en bout (faux GitHub local)
cargo clippy --all-targets -- -D warnings
GITHUB_TOKEN=… RELAY_CONFIG=relay.toml cargo run
docker build -t release-relay .
```

## Licence

[MIT](LICENSE)
