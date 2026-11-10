# wiki-llm-mcp

[English](README.md) · **Français**

Un serveur MCP qui permet à Claude de lire, chercher et maintenir votre coffre
(vault) Obsidian ou Markdown. Chaque changement est un commit Git, et il est
impossible de supprimer une note par son intermédiaire.

> En début de développement : pas encore testé avec tous les clients Claude.

## Sommaire

- [Ce que ça fait](#ce-que-ça-fait)
  - [Comment un changement est écrit](#comment-un-changement-est-écrit)
- [Installation](#installation)
  - [Configurer avec un agent IA](#configurer-avec-un-agent-ia)
- [Utilisation](#utilisation)
- [Mode distant (Claude mobile et web)](#mode-distant-claude-mobile-et-web)
  - [Avec Docker Compose](#avec-docker-compose)
  - [Sans Docker](#sans-docker)
  - [Connecter Claude](#connecter-claude)
  - [Changer le secret](#changer-le-secret)
- [Développement](#développement)
- [Licence](#licence)

## Ce que ça fait

- **Lire et chercher** dans les pages Markdown d'un coffre.
- **Créer, modifier et déplacer** des pages : remplacer un passage, ajouter à la
  fin, ou réécrire une page entière. Un appel produit un commit, ou n'écrit rien.
- **Ne jamais écraser une version plus récente** : réécrire une page entière
  exige la révision de la page telle qu'elle a été lue, et est refusé si la page
  a changé depuis (dans Obsidian, ou sur un autre appareil).
- **Garder les liens valides** quand une page est déplacée (`[[wikilinks]]` et
  liens Markdown).
- **Protéger votre travail** : vos modifications non commitées sont enregistrées
  dans leur propre commit avant que le LLM n'écrive, et ses commits ont leur
  propre auteur.
- **Synchroniser par Git** quand le coffre a un dépôt distant : pull avant
  d'écrire, push après.

Il n'y a pas d'outil de suppression. `.git`, `.obsidian` et les fichiers cachés
sont hors d'atteinte, comme tout ce qui se trouve en dehors du coffre.

### Comment un changement est écrit

Chaque écriture suit les mêmes étapes, un seul écrivain à la fois. Si une étape
échoue avant le commit, les fichiers sont remis dans leur état d'origine.

```mermaid
flowchart TD
    A["Claude appelle apply_changes<br/>(changements + raison)"] --> B["Prise du verrou d'écriture"]
    B --> C["Commit des modifications non commitées<br/>du propriétaire, dans un commit à part"]
    C --> D{"Le coffre a un<br/>dépôt distant ?"}
    D -- oui --> E["Fetch et rebase"]
    D -- non --> F
    E -- "conflit" --> X["Arrêt : rien n'est écrit"]
    E -- "à jour, ou dépôt injoignable (avertissement)" --> F["Vérification de chaque<br/>changement sur le coffre"]
    F -- "invalide" --> X
    F -- "valide" --> G["Écriture des pages, correction des liens<br/>vers les pages déplacées,<br/>régénération de WIKI-INDEX.md"]
    G --> H["Un seul commit, signé par l'outil"]
    H --> I{"Dépôt distant ?"}
    I -- oui --> J["Push<br/>(en cas d'échec : gardé en local,<br/>envoyé à la prochaine écriture)"]
    I -- non --> K["Terminé"]
    J --> K
```

## Installation

Il faut `git` sur la machine qui sert le coffre. Ensuite, au choix :

**L'extension Claude Desktop**, sur macOS et Windows : téléchargez
[`wiki-llm-mcp.mcpb`](https://github.com/tgalode/wiki-llm-mcp/releases/latest/download/wiki-llm-mcp.mcpb),
ouvrez-le avec Claude Desktop (ou déposez-le dans Réglages > Extensions), puis
choisissez votre dossier de notes. Rien d'autre à installer ni à configurer : le
dossier est préparé au premier démarrage.

**Un binaire précompilé**, pour Linux x86_64 (statique, toutes distributions),
macOS sur puce Apple, ou Windows x86_64
([`wiki-llm-mcp-x86_64-pc-windows-msvc.zip`](https://github.com/tgalode/wiki-llm-mcp/releases/latest/download/wiki-llm-mcp-x86_64-pc-windows-msvc.zip),
avec [Git pour Windows](https://git-scm.com/download/win) installé) :

```sh
target=x86_64-unknown-linux-musl   # macOS : aarch64-apple-darwin
curl -L "https://github.com/tgalode/wiki-llm-mcp/releases/latest/download/wiki-llm-mcp-$target.tar.gz" | tar -xz
sudo install wiki-llm-mcp /usr/local/bin/
wiki-llm-mcp --version
```

**Depuis les sources**, sur toute plateforme disposant d'une toolchain Rust :

```sh
cargo install --git https://github.com/tgalode/wiki-llm-mcp --locked
```

**L'image Docker**, `ghcr.io/tgalode/wiki-llm-mcp`, à lancer comme indiqué dans
[Mode distant](#mode-distant-claude-mobile-et-web).

Suivez ensuite [Utilisation](#utilisation), ou laissez un agent le faire pour vous.

### Configurer avec un agent IA

Collez ce texte dans un agent de code (Claude Code ou équivalent) qui tourne sur
la machine où se trouve votre coffre :

```text
Installe et configure wiki-llm-mcp pour moi : un serveur MCP qui permet à
Claude de lire, chercher et maintenir un coffre Markdown/Obsidian, en commitant
chaque changement dans Git.
Sources et documentation : https://github.com/tgalode/wiki-llm-mcp (lis d'abord
son README, README.fr.md en français ; il fait référence pour toutes les
commandes ci-dessous).

Avant de faire quoi que ce soit, demande-moi :
1. Le chemin du coffre : un dossier de notes existant, ou un nouveau dossier à
   créer.
2. Si je veux seulement le mode local (Claude Desktop / Claude Code sur cette
   machine) ou aussi le mode distant (Claude mobile et web, qui demande une
   adresse HTTPS publique que je dois fournir avec un reverse proxy ou un
   tunnel).

Ensuite :
1. Vérifie que git est installé. Installe le binaire comme le décrit la
   section Installation du README : le binaire précompilé s'il existe pour
   cette plateforme, sinon `cargo install`, ou l'image Docker. N'installe ni
   toolchain ni paquet système sans me demander.
2. Lance `wiki-llm-mcp init <coffre>`. La commande laisse mes notes existantes
   à leur place et commite d'abord leur état d'origine. Si elle refuse (le
   dossier est dans un autre dépôt Git, ou il existe déjà un WIKI-INDEX.md
   qu'elle n'a pas généré), arrête-toi et explique-moi pourquoi : ne déplace,
   ne renomme et ne supprime rien pour contourner le refus.
3. Mode local : déclare le serveur dans mon client MCP avec la commande
   `wiki-llm-mcp serve <chemin absolu du coffre>`, en chemins absolus (Claude
   Desktop : `claude_desktop_config.json` ; Claude Code :
   `claude mcp add wiki -- <binaire> serve <coffre>`).
4. Mode distant, seulement si je l'ai demandé : démarre le serveur comme le
   décrit la section Mode distant du README (Docker Compose si Docker est
   présent, sinon le binaire avec `--public-url`). Il génère lui-même son
   secret : donne-moi la commande qui l'affiche, et n'affiche jamais le secret
   dans la conversation ni dans un commit.
   N'expose rien publiquement toi-même : dis-moi vers quelle adresse et quel
   port mon proxy ou mon tunnel doit pointer, puis donne-moi l'URL du
   connecteur (`<url publique>/mcp`) à ajouter dans Claude.
5. Vérifie : `wiki-llm-mcp --version` fonctionne, le `git log` du coffre montre
   le commit de mise en place, et le serveur démarre sans erreur.

Ne modifie et ne supprime jamais mes notes pendant l'installation. Termine en
me disant ce que tu as installé et où, comment le serveur est lancé, où le
secret est rangé (s'il y en a un), et que le fichier WIKI.md du coffre contient
les règles que Claude suit, que je peux modifier.
```

## Utilisation

```sh
wiki-llm-mcp init ./vault    # dossier vide ou coffre existant
wiki-llm-mcp serve ./vault   # le sert sur stdio
```

`init` ajoute `raw/`, `wiki/`, `WIKI.md` et un `WIKI-INDEX.md` généré, et met en
place Git. Les notes existantes restent où elles sont ; leur état d'origine est
commité en premier. `serve --init` le fait au premier démarrage, si le dossier
n'est pas encore prêt.

`WIKI.md` contient les règles données au LLM quand il se connecte. Modifiez-le
pour changer la façon dont votre wiki est maintenu. Le LLM ne peut pas le
modifier.

Dans ce mode, tout tourne sur votre machine : le client lance lui-même le
serveur et lui parle sur stdio. Aucun port n'est ouvert et aucun secret n'est
nécessaire.

```mermaid
flowchart LR
    subgraph machine["Votre machine"]
        client["Claude Desktop<br/>ou Claude Code"] -- "stdio" --> server["wiki-llm-mcp serve"]
        server -- "fichiers + git" --> vault[("Coffre<br/>(dépôt Git)")]
    end
    vault -. "pull / push (facultatif)" .-> remote[("Dépôt Git distant")]
```

Pour Claude Desktop, dans `claude_desktop_config.json` :

```json
{
  "mcpServers": {
    "wiki": {
      "command": "/chemin/vers/wiki-llm-mcp",
      "args": ["serve", "/chemin/vers/le/coffre"]
    }
  }
}
```

## Mode distant (Claude mobile et web)

Les applications Claude joignent les connecteurs depuis les serveurs
d'Anthropic : le coffre doit donc être servi à une adresse HTTPS publique. C'est
à vous de fournir cette adresse avec un reverse proxy ou un tunnel ; TLS n'est
pas géré ici.

```mermaid
flowchart LR
    app["Claude mobile,<br/>web ou desktop"] --> anthropic["Serveurs d'Anthropic"]
    anthropic -- "HTTPS /mcp<br/>+ jeton d'accès" --> proxy["Votre reverse proxy<br/>ou tunnel (TLS)"]
    subgraph host["Votre serveur"]
        proxy -- "HTTP 127.0.0.1:8787" --> server["wiki-llm-mcp serve<br/>--public-url ..."]
        server -- "fichiers + git" --> vault[("Coffre<br/>(dépôt Git)")]
    end
    vault -. "pull / push (facultatif)" .-> remote[("Dépôt Git distant")]
```

### Avec Docker Compose

```sh
mkdir wiki-llm-mcp && cd wiki-llm-mcp
curl -LO https://raw.githubusercontent.com/tgalode/wiki-llm-mcp/main/deploy/compose.yaml
curl -L -o .env https://raw.githubusercontent.com/tgalode/wiki-llm-mcp/main/deploy/.env.example
nano .env                 # renseigner PUBLIC_URL
mkdir -p vault            # ou pointer VAULT vers vos notes existantes
docker compose up -d
docker compose exec wiki wiki-llm-mcp secret /vault
```

Le premier démarrage prépare le coffre et génère le secret ; la dernière
commande l'affiche. Pointez votre reverse proxy vers `127.0.0.1:8787`.

Sans reverse proxy, le même fichier peut lancer un
[tunnel Cloudflare](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/) :
renseignez `TUNNEL_TOKEN` dans `.env`, donnez au nom d'hôte public du tunnel le
service `http://wiki:8787`, puis démarrez avec
`docker compose --profile tunnel up -d`. Aucun port n'est à ouvrir sur le
serveur.

Pour construire l'image vous-même : `docker build -t wiki-llm-mcp .`

### Sans Docker

```sh
wiki-llm-mcp serve ./vault --init --public-url https://wiki.example.com
wiki-llm-mcp secret ./vault    # affiche le secret généré au premier démarrage
```

Le serveur écoute sur `127.0.0.1:8787` (`--bind` pour changer) : pointez votre
reverse proxy ou votre tunnel dessus.

Pour choisir vous-même le secret, définissez la variable d'environnement
`WIKI_LLM_MCP_SECRET`. Elle a la priorité, et rien n'est stocké.

### Connecter Claude

Dans Claude,
ajoutez un connecteur personnalisé avec l'URL `https://wiki.example.com/mcp`,
choisissez l'enregistrement automatique du client OAuth (**Register
automatically**), puis saisissez le secret sur la page qui s'ouvre.

Le secret est saisi une seule fois, dans votre navigateur, et n'arrive jamais
jusqu'à Claude :

```mermaid
sequenceDiagram
    actor Vous
    participant Claude
    participant Serveur as wiki-llm-mcp
    Vous->>Claude: Ajout du connecteur https://wiki.example.com/mcp
    Claude->>Serveur: Découverte des points d'accès OAuth, enregistrement d'un client
    Claude-->>Vous: Ouverture de la page d'autorisation
    Vous->>Serveur: Saisie du secret
    Serveur-->>Claude: Code d'autorisation (PKCE)
    Claude->>Serveur: Échange du code
    Serveur-->>Claude: Jeton d'accès + jeton de rafraîchissement
    Claude->>Serveur: Requêtes /mcp avec le jeton d'accès
```

- Le secret généré est stocké dans `.git/wiki-llm-mcp/secret`, lisible par son
  seul propriétaire et hors des fichiers du coffre. Un secret fourni par
  `WIKI_LLM_MCP_SECRET` doit faire au moins 20 caractères. Sous Windows, ce
  fichier et les jetons ne sont protégés que par les permissions du dossier.
- Seules l'adresse de retour de Claude et les adresses locales peuvent recevoir
  une autorisation. `--allow-redirect-uri` permet d'autoriser un autre client.
- Les jetons sont stockés hachés dans `.git/wiki-llm-mcp/`, jamais dans les
  fichiers du coffre.
- `wiki-llm-mcp revoke ./vault` déconnecte tous les clients immédiatement. La
  commande ne change pas le secret.
- Après 10 échecs de connexion en 15 minutes, les nouvelles connexions sont
  bloquées jusqu'à la fin de cette fenêtre. Les clients déjà connectés
  continuent de fonctionner.

### Changer le secret

Le secret n'est que le mot de passe demandé sur la page d'autorisation. Les
jetons sont aléatoires et n'en dépendent pas : il y a donc deux actions
distinctes.

1. **Changer le secret** : `wiki-llm-mcp secret ./vault --new` en génère un
   nouveau et l'affiche (ou mettez une nouvelle valeur dans
   `WIKI_LLM_MCP_SECRET` si vous l'utilisez), puis redémarrez le serveur. Les
   clients déjà connectés continuent de fonctionner ; le nouveau secret sera
   demandé à la prochaine connexion d'un client.
2. **Révoquer les sessions** : `wiki-llm-mcp revoke ./vault`. Chaque client doit
   alors se reconnecter, avec le secret en vigueur.

Pour un changement de routine, la première suffit. Si le secret a pu fuiter,
faites les deux, dans cet ordre : quelqu'un a pu obtenir des jetons avec
l'ancien.

## Développement

`./dev.sh test` lance cargo dans un conteneur : seul Docker est nécessaire.

## Licence

MIT
