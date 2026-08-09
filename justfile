[windows]
set shell := ["nu", "-c"]

set dotenv-load

sync:
    cargo run --bin mihomo-versions-sync -- --out mihomo-releases.json --repo MetaCubeX/mihomo --token $TOKEN

sync-mihomo-smart:
    cargo run --bin mihomo-versions-sync -- --out mihomo-smart-releases.json --repo vernesong/mihomo --token $TOKEN

sync-meow-rs:
    cargo run --bin mihomo-versions-sync -- --out meow-rs-releases.json --repo madeye/meow-rs --classifier ./classifier/meow-rs.json --token $TOKEN

sync-clash-rs:
    cargo run --bin mihomo-versions-sync -- --out clash-rs-releases.json --repo Watfaq/clash-rs --classifier ./classifier/clash-rs.json --token $TOKEN

sync-all:
    cargo run --bin mihomo-versions-sync -- --config sync-config/sync-all.json --token $TOKEN --compact --gz

download:
    cargo run --example usage -- https://github.com/oomeow/mihomo-versions/releases/download/index/mihomo-releases.json.gz --index-cache mihomo-index-cache.json --dest ./target/mihomo --asset-name mihomo-darwin-arm64-go124-v1.19.29.gz --channel stable
    chmod +x ./target/mihomo
    ./target/mihomo -v
