# `config.rs` & `profiles.rs`

These two modules handle startup configuration: what the terminal looks like
(`config.rs`) and which shell it runs (`profiles.rs`).

## `config.rs` — the TOML config

On startup giest reads `%APPDATA%\giest\config.toml` (override the path with the
`GIEST_CONFIG` environment variable). All keys are optional; missing ones fall
back to the bundled defaults, which include the full ANSI 16 + 256-color
palette.

```mermaid
classDiagram
    class Config {
        font_points: f32
        fg, bg: Rgb
        palette: [Rgb; 256]
        padding_x, padding_y: f32
        text_gamma: f32
        cursor: Option~Rgb~
        scrollback_limit: usize
        selection_bg: Rgb
        selection_fg: Option~Rgb~
        copy_on_select: bool
        shell: Option~String~
        load() Config
    }
```

```mermaid
flowchart LR
    env["GIEST_CONFIG env var"]
    appdata["%APPDATA%\giest\config.toml"]
    parse["toml + serde → Config"]
    defaults["bundled defaults<br/>(ANSI 16 + 256 palette)"]

    env -->|if set| parse
    appdata -->|else| parse
    defaults -->|fill missing| parse

    classDef o fill:#1f2933,stroke:#7c5cff,color:#e6e6e6
    class env,appdata o
```

Where each field is consumed:

| Field | Consumed by |
| --- | --- |
| `font_points`, `text_gamma` | `render::init` (atlas + shader) |
| `fg`, `bg`, `palette` | `engine.apply_theme` |
| `cursor` | `engine.set_cursor_color` |
| `padding_x/y` | `app::render_active` (per-pane inset) |
| `scrollback_limit` | `GhosttyVtEngine::new` |
| `selection_bg/fg`, `copy_on_select` | `app::render_active` / `TermFrame` |
| `shell` | `profiles::detect` |

See [Configuration](../guides/configuration.md) for the full annotated TOML.

## `profiles.rs` — shell profiles

A `Profile` is a launchable shell: a display name plus the program + args to
spawn. `detect` probes what's installed and picks a default, Windows-Terminal
style.

```mermaid
flowchart TB
    start["detect(config_shell)"]
    pwsh{"pwsh.exe on PATH?"}
    addpwsh["+ PowerShell (pwsh.exe)"]
    always["+ Windows PowerShell (powershell.exe)<br/>+ Command Prompt (cmd.exe)"]
    wsl{"wsl.exe on PATH?"}
    addwsl["+ WSL (wsl.exe)"]
    override{"config shell set?"}
    match{"matches an existing profile?"}
    selidx["default = that index"]
    prepend["prepend custom command as default (index 0)"]
    def0["default = 0 (pwsh if present)"]

    start --> pwsh
    pwsh -- yes --> addpwsh --> always
    pwsh -- no --> always
    always --> wsl
    wsl -- yes --> addwsl --> override
    wsl -- no --> override
    override -- yes --> match
    override -- no --> def0
    match -- yes --> selidx
    match -- no --> prepend
```

- `which()` splits `PATH` to find `pwsh.exe` / `wsl.exe`. `powershell.exe` and
  `cmd.exe` are assumed always present on Windows.
- `Profile::matches` is case- and `.exe`-insensitive, matching by display name
  or program, so `shell = "cmd"`, `"CMD"`, or `"cmd.exe"` all select Command
  Prompt.
- An **unknown** `shell` value (e.g. a full path to `bash.exe`) is **prepended**
  as a new default profile.

The profile picker (`▾` menu in the tab strip) lets you open a tab running any
detected profile; the `+` button and Ctrl+Shift+T use the default.
