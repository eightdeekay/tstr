# tstr Editor Support

## IntelliJ / IDEA

1. Open **Settings → Editor → TextMate Bundles**
2. Click **+** and select the `editor/textmate` directory
3. `.tstr` files will now have syntax highlighting

Alternatively, copy `textmate/tstr.tmLanguage.json` to your IntelliJ TextMate bundles directory.

## Neovim (native vim syntax — no plugins needed)

Symlink or copy the vim runtime files:

```bash
ln -s ~/dev/tstr/editor/vim/syntax/tstr.vim ~/.config/nvim/syntax/tstr.vim
ln -s ~/dev/tstr/editor/vim/ftdetect/tstr.vim ~/.config/nvim/ftdetect/tstr.vim
```

Or if using a plugin manager, add `~/dev/tstr/editor/vim` as a local plugin.

`.tstr` files will have syntax highlighting immediately.

## Neovim (Tree-sitter)

Tree-sitter grammar not yet available. The vim syntax file above covers the same highlighting.

## What gets highlighted

- **Comments**: `//` and `/* */`
- **Strings**: `"..."` with escape sequences and `{{interpolation}}`
- **Keywords**: `if`, `else`, `return`, `-->`, `<--`, `js:`
- **HTTP methods**: `get(`, `post(`, `put(`, `patch(`, `delete(`
- **Built-ins**: `$.uuid()`, `$.log()`, `$.string()`, etc.
- **Operators**: `==`, `!=`, `~`, `~?`, `&&`, `||`, `|`
- **Status checks**: `? 2xx`, `? 200 201`, `? <500`
- **Numbers**, **booleans**, **null**
- **File references**: `@path/to/file.json`
- **Regex literals**: `/pattern/`
