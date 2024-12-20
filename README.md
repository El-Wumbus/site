## Building

```
cargo build --release
```

### Rasberry Pi 4

Prerequsites:  
- [aarch64-linux-musl-cross-bin]

```bash
cargo build --release --target aarch64-unknown-linux-musl
```

[aarch64-linux-musl-cross-bin]: https://aur.archlinux.org/packages/aarch64-linux-musl-cross-bin
