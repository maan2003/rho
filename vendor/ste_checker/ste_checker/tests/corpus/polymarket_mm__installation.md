### Nix (Recommended)

**Run directly without installation:**
```bash
nix run github:valeratrades/polymarket_mm -- --help
```

**Install permanently:**
```bash
nix profile install github:valeratrades/polymarket_mm
polymarket_mm --help
```

### pip (alternative)

Requires Python 3.12+ and Rust toolchain:

```bash
pip install 'git+https://github.com/valeratrades/polymarket_mm.git'
polymarket_mm --help
```
