# Dots

Fast, native SSH and SFTP workspace written entirely in Rust. Dots uses an immediate-mode native GPU UI (eframe/egui), not Electron or a bundled VS Code/web app.

## What works

- saved SSH server profiles (host, port, user, private-key path)
- SSH public-key authentication with `libssh2`
- command execution with a dedicated result pane
- remote SFTP directory browsing
- native release optimization (thin LTO, stripped binary)
- GitHub Actions builds for Windows x64 and Apple Silicon macOS

## Run locally

```bash
cargo run --release
```

Create a server profile, point it at an existing private key, then connect. Profile metadata is stored in the platform configuration directory; private keys and passwords are never copied into it.

## CI artifacts

Every push/PR builds downloadable binaries. Pushing a tag such as `v0.1.0` attaches both binaries to a GitHub Release.
