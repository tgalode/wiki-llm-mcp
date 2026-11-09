# wiki-llm-mcp

An MCP server that lets Claude read, search and maintain your Obsidian or
Markdown vault. Every change is a Git commit, and notes can never be deleted
through it.

> Early development. Local (stdio) mode works; remote mode is in progress.

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

## Development

`./dev.sh test` runs cargo in a container: only Docker is required.

## License

MIT
