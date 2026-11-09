# wiki-llm-mcp

An MCP server that lets Claude read, search and maintain your Obsidian or
Markdown vault. Every change is a Git commit, and notes can never be deleted
through it.

> Early development: not yet tested against every Claude client.

## What it does

- **Read and search** the Markdown pages of a vault.
- **Create, update and move** pages. One call makes one commit, or writes nothing.
- **Keep links valid** when a page moves (`[[wikilinks]]` and Markdown links).
- **Protect your work**: uncommitted edits are saved in their own commit before
  the LLM writes, and its commits have their own author.
- **Sync through Git** when the vault has a remote: pull before writing, push after.

There is no delete tool. `.git`, `.obsidian` and hidden files are out of reach,
as is anything outside the vault.

## Usage

```sh
wiki-llm-mcp init ./vault    # empty folder or existing vault
wiki-llm-mcp serve ./vault   # serve it over stdio
```

`init` adds `raw/`, `wiki/`, `WIKI.md` and a generated `WIKI-INDEX.md`, and sets
up Git. Existing notes stay where they are; their original state is committed first.

`WIKI.md` holds the rules given to the LLM when it connects. Edit it to change
how your wiki is maintained. The LLM cannot modify it.

For Claude Desktop, in `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "wiki": {
      "command": "/path/to/wiki-llm-mcp",
      "args": ["serve", "/path/to/vault"]
    }
  }
}
```

## Remote mode (Claude mobile and web)

Claude's apps reach connectors from Anthropic's servers, so the vault must be
served at a public HTTPS address. You provide that address with a reverse proxy
or a tunnel; TLS is not handled here.

```sh
export WIKI_LLM_MCP_SECRET="$(openssl rand -base64 32)"   # keep it somewhere safe
wiki-llm-mcp serve ./vault --public-url https://wiki.example.com
```

The server listens on `127.0.0.1:8787` (`--bind` to change it). In Claude, add a
custom connector with the URL `https://wiki.example.com/mcp`, choose
**Register automatically** for the OAuth client, and enter the secret on the
page that opens.

- Without a secret of at least 20 characters, remote mode refuses to start.
- Only Claude's callback and local addresses can receive an authorization. Use
  `--allow-redirect-uri` to allow another client.
- Tokens are stored hashed inside `.git/wiki-llm-mcp/`, never in the vault's files.
- `wiki-llm-mcp revoke ./vault` disconnects every client immediately. It does
  not change the secret.
- After 10 failed logins in 15 minutes, new logins are blocked for the rest of
  that window. Clients already connected keep working.

With Docker:

```sh
docker build -t wiki-llm-mcp .
docker run --rm -u "$(id -u):$(id -g)" --read-only --cap-drop ALL \
  --security-opt no-new-privileges --tmpfs /tmp \
  -v "$PWD/vault:/vault" -e WIKI_LLM_MCP_SECRET -p 127.0.0.1:8787:8787 \
  wiki-llm-mcp serve /vault --public-url https://wiki.example.com --bind 0.0.0.0:8787
```

## Development

`./dev.sh test` runs cargo in a container: only Docker is required.

## License

MIT
