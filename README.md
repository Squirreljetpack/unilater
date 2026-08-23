# slate-rs [![Crates.io](https://img.shields.io/crates/v/slate-rs)](https://crates.io/crates/slate-rs)[![License](https://img.shields.io/github/license/squirreljetpack/slate-rs)](https://github.com/squirreljetpack/slate-rs/blob/main/LICENSE)

Fork of https://github.com/znx3p0/vielsprachig, with extra functionality for systemd, Podman Quadlet generation, and Tera templating.


# Installation

```shell
cargo install slate-rs

# Convert yml -> toml
slate $HOME/.config/alacritty.yml -o $HOME/.config/alacritty.toml
```

# Features
- Convert between different input and output serialized data formats
- Tera templating (auto-detected for `.tera` files or via `--tera`)
- Special modes for generating systemd timers/services and Podman quadlet files (see examples below)

## Supported formats
The current input options and their inferred extensions are:

| Input | Output       | Extensions               |
|-------|--------------|--------------------------|
| Json  | Json         | `.json`                  |
|       | PrettyJson   | `.hjson`                 |
| Yaml  | Yaml         | `.yaml`, `.yml`          |
| Cbor  | Cbor         | `.cb`, `.cbor`           |
| Ron   | Ron          | `.ron`                   |
|       | PrettyRon    | `.hron`                  |
| Toml  | Toml         | `.toml`                  |
| Bson  | Bson         | `.bson`, `.bs`           |
|       | Pickle       | `.pickle`, `.pkl`        |
|       | Bincode      | `.bc`, `.bincode`        |
|       | Postcard     | `.pc`, `.postcard`       |
|       | Flexbuffers  | `.fb`, `.flexbuffers`    |
|       | Systemd      | (use `--to systemd`)     |
|       | Quadlet      | (use `--to quadlet`)     |

---

## Systemd Service & Timer Mode (`--to systemd` / `-t systemd`)

Convert YAML or Tera-templated YAML configs into native systemd `.service` and `.timer` unit files.

### What `slate` Does Automatically:
1. **Renders Tera templates & maps sections**: Splits single YAML service declarations containing a `Timer:` block into separate `.service` and `.timer` INI files.
2. **Injects standard logging defaults**: Automatically sets `StandardOutput=journal` and `StandardError=journal` for services, and sets `Type=oneshot` for services managed by timers.
3. **Runs unit verification**: Validates generated unit files with `systemd-analyze verify`.
4. **Prompts activation in interactive mode**: Runs `systemctl --user daemon-reload` and `systemctl --user enable --now <unit>` for newly generated services/timers.

### Example:

Given `sysd.yaml`:
```yaml
git_obsidian:
    Unit:
        Description: "Syncs Obsidian"
    Service:
        ExecStart: /bin/zsh -c "$HOME/.zsh/cron.sh; $HOME/bin/git_obsidian.zsh;"
    Timer:
        OnCalendar: "*:7/15"

startup_server:
    Unit:
        Description: "Caddy reverse proxy"
        After: network-online.target
    Service:
        Type: exec
        ExecStart: /usr/bin/caddy run --config /etc/caddy/Caddyfile
        Restart: on-failure
    Install:
        WantedBy: default.target
```

Run `slate` to generate and install units:
```shell
slate sysd.yaml -t systemd -o ~/.config/systemd/user/
```

---

## Podman Quadlet Mode (`--to quadlet` / `-t quadlet`)

Convert Docker Compose files (`compose.yaml`) directly into native Podman Quadlet files (`.container`, `.pod`, `.volume`, `.network`).

### What `slate` Does Automatically:
1. **Preprocesses Compose Specs**:
   - **Image Qualification**: Resolves unqualified image names to fully qualified registry URLs using `docker manifest inspect`.
   - **Environment Variable Resolution**: Interpolates variables from `.env` files and environment.
   - **Path Canonicalization**: Resolves relative volume mounts and `env_file` paths relative to the compose file location.
2. **Quadlet Conversion & Tuning**: Generates Quadlet INI units, injecting `AutoUpdate=registry`, service restart policies (`Restart=always`), and network/dependency targets (`After=local-fs.target network-online.target`).
3. **Service Activation**: In interactive mode, triggers `systemctl --user daemon-reload` and starts quadlet services.

### Example (`@dufs` workflow):

The repository includes a concrete example in `examples/compose.yaml`:

```shell
# Using the cargo alias
cargo @dufs

# Or running slate directly
slate examples/compose.yaml -t quadlet -o examples/outputs/
```

This generates `dufs.pod` and `dufs-app.container` with resolved volume paths (relative to `examples/compose.yaml`) and environment variables, ready to be placed in `~/.config/containers/systemd/`.

---

# See also

- https://matduggan.com/replace-compose-with-quadlet/
- https://chasingsunlight.netlify.app/posts/homelab-with-docker-and-tailscale-2/
