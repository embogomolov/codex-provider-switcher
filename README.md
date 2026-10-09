# Codex provider switcher

A terminal user interface (TUI) for switching providers of saved Codex
conversations and forks. Runs on Windows and macOS
(Apple Silicon and Intel).

![Conversations with example data](assets/terminal.png)

## Installation

On Windows, extract `codex-provider-switcher-windows-x64.zip` and run
`codex-provider-switcher.exe`, or start it from PowerShell:

```powershell
.\codex-provider-switcher.exe
```

On macOS, install the executable in your PATH:

```sh
tar -xzf codex-provider-switcher-macos-universal.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 755 codex-provider-switcher "$HOME/.local/bin/codex-provider-switcher"
export PATH="$HOME/.local/bin:$PATH"
codex-provider-switcher
```

Add the `export` line to `~/.zshrc` to keep the PATH setting. The macOS build is
not notarized by Apple.

## Usage

Close Codex Desktop and CLI before saving changes.

Click conversations, providers and actions with the mouse, or use the keyboard:

1. In Conversations, type part of a name to find it.
2. Choose a result with Up/Down and press Enter. For several conversations,
   mark them with Space and choose Change provider.
3. Choose a provider, then Apply.
4. Reopen Codex when the change finishes.

![Provider selection, with the current provider in cyan](assets/provider-selection.png)

Each conversation shows its provider. Forks have separate rows marked `Fork` and
can be switched independently. A selection includes its subagents and edited
histories, preserves message content, and leaves unselected conversations and the
default for new conversations on their current providers. Shared history pointers
are kept consistent when referenced files change length.

Tab opens Providers. Choose Add provider and enter a name, a Responses API base
URL and your API key. Use as default selects the provider for new conversations.
`openai` is built in. Adding a provider does not test its connection.

To switch by ID from the command line:

```sh
codex-provider-switcher --apply --select ID --provider openai --yes
```

This command also sets the default provider. Run `--help` for other options.

## Notes

- Checked with Codex `0.153.2` and `0.162.0-alpha.2`. Unsupported history layouts
  are rejected.
- API keys are saved as plain text in Codex `config.toml`. You can choose
  environment-variable authentication or no authentication for a local server.
- After an interrupted change, keep Codex closed and run the switcher again to
  recover. Leave pending files in place. Recovery files are temporary; the
  application does not keep permanent backups.
- To use a different Codex directory, set `CODEX_HOME` or pass `--home PATH`.

## Development

Requires Rust 1.96+, MSVC tools on Windows or Xcode command-line tools on macOS.

```sh
cargo build --locked --release
cargo test --locked
```

Distribute the executable from `target/release`.

License: [MIT](LICENSE).
