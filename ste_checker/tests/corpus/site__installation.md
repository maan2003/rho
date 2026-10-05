## Prerequisites
- Nix package manager with flakes enabled
- Git with LFS (`apt install git-lfs && git lfs install`)

## Local Development
```sh
git clone <repository-url> && cd site
nix develop
lw  # alias for: cargo leptos watch --hot-reload
```
Site available at `http://127.0.0.1:61156`

## Production Build
```sh
nix build
./result/bin/site
```

## Server Deployment
```sh
git clone <repository-url> ~/s/site && cd ~/s/site
nix build

sudo tee /etc/systemd/system/valeratrades.service > /dev/null <<EOF
[Unit]
Description=ValeRatrades Website
After=network.target
[Service]
Type=simple
User=$USER
WorkingDirectory=$HOME/s/site
ExecStart=$HOME/s/site/result/bin/site
Restart=on-failure
Environment="LEPTOS_SITE_ROOT=$HOME/s/site/target/site"
[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload && sudo systemctl enable --now valeratrades
```

## Troubleshooting
- **cargo-leptos not found**: `cargo install cargo-leptos`
- **WASM issues**: `rustup target add wasm32-unknown-unknown`
- **Port in use**: `sudo lsof -i :61156`
- **Nix build fails**: `nix-collect-garbage && nix build --rebuild`
