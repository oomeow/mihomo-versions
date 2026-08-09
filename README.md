# mihomo-versions

为 Clash Verge Self 等应用提供 mihomo 内核版本管理能力的独立 Rust 项目:

- **library** (`mihomo-versions`):解析版本索引、识别平台、选择下载文件、下载并校验。
- **binary** (`mihomo-versions-sync`):从 GitHub Releases 全量同步,生成精简版本索引。

mihomo 版本通过索引分发,而非客户端直连 GitHub API:

```
GitHub Releases API → mihomo-versions-sync → mihomo-releases.json → GitHub Release(唯一版本 tag) → 客户端
```

## 用法

### 生成索引(CI / 手动)

```bash
cargo run --bin mihomo-versions-sync -- \
  --out mihomo-releases.json \
  --repo MetaCubeX/mihomo \
  --token ghp_xxx   # 可选:提升 API 限流
  # --classifier path/to/classifier.json   # 可选:其他仓库的自定义分类规则
```

默认使用内置的 MetaCubeX/mihomo 分类规则;对其他 GitHub 仓库,用
`--classifier <path>` 提供该仓库的命名规则(`keep_extensions` / `platforms`)。
`--out` 与 `--classifier` 均支持相对路径:绝对路径与当前目录已存在的文件原样使用,
其余相对路径解析到项目根目录。同步为**增量式**:读取已有索引,release 的
`updated_at`、版本级派生字段(`semver` / `channel` / `prerelease` /
`published_at`)及其保留资产(按名称逐 asset 比对 `updated_at`、`platform`、
`format`、`size`)都未变化的直接复用,新增/变更的重新处理——分类规则升级
后旧条目会被重新处理而非带着陈旧字段复用,GitHub 上已删除的自动移除
(索引由 API 响应整体重建)。加 `--compact` 可输出**单行 JSON**(体积更小,
适合分发);加
`--emit-gz` 可同时产出 **gzip 压缩副本**(路径自动追加 `.gz`,与
`--compact` 可组合;旧 `--gz` 已废弃,仅输出 gz 时会告警);
`--print-classifier` 打印内置默认分类配置模板(便于派生新仓库规则)。
每 6 小时定时同步的示例见 `.github/workflows/sync.yml`。

**多仓库批量同步**:`--config <path>` 一份配置一次跑全部(任一 job 失败——
包括 repo 无效、classifier 加载失败、同步失败——都不中断其他 job,任一失败
退出码非 0)。**配置中不要放 token**(会被提交进仓库),用
`--token` 或环境变量 `MIHOMO_VERSION_TOKEN`(CLI 优先):

```json
{
  "jobs": [
    { "repo": "MetaCubeX/mihomo", "out": "index/mihomo-releases.json" },
    { "repo": "madeye/meow-rs", "out": "index/meow-rs-releases.json", "classifier": "classifier/meow-rs.json" },
    { "repo": "Watfaq/clash-rs", "out": "index/clash-rs-releases.json", "classifier": "classifier/clash-rs.json", "max_versions": 10 }
  ]
}
```

### 客户端集成

```rust,no_run
use std::time::Duration;
use mihomo_versions::{
    DownloadOptions, HttpClient, download, fetch_index, fetch_index_cached, pick_asset_by_name, IndexCache,
};
use tokio_util::sync::CancellationToken;

let client = HttpClient::new()?;

// 多镜像 failover:依次尝试每个索引 URL(以 .gz 结尾的会自动解压),任一成功即返回
let urls = [
    "https://cdn1.example/mihomo-releases.json.gz",
    "https://github.com/owner/repo/releases/latest/download/mihomo-releases.json",
];
let index = fetch_index(&client, &urls).await?;

// 或带本地缓存:新鲜期内不打网络,过期走 ETag/Last-Modified 条件请求,
// 全部镜像失败时回退 stale 缓存
let index = fetch_index_cached(
    &client,
    &urls,
    &IndexCache { path: "cache/mihomo-releases.json".into(), max_age: Duration::from_secs(600) },
).await?;

// 按 asset 名选择(可指定版本,缺省时 newest-first)
let asset = pick_asset_by_name(&index, "mihomo-darwin-arm64-v1.19.9.gz", None)?;

// 下载 → 校验(缺失 sha256 时降级警告)→ 解压安装
download(&client, asset, "/usr/local/bin/mihomo", DownloadOptions::default(), |done, total| {
    println!("{done}/{total}");
}).await?;
```

`DownloadOptions` 支持:**断点续传**(`resume`,默认开启,基于 `Range`)、
**取消**(`cancel: Option<CancellationToken>`)、**空闲超时**(`idle_timeout`)、
**总超时**(`total_timeout: Option<Duration>`,默认 `None` 不限制——下载路径
不受 HTTP 客户端 300 秒硬上限约束,慢链路可完整下载,由 `idle_timeout` 与
断点续传兜底;设置后单次尝试超时返回 `Timeout`,可重试);
`HttpClient` 可用 `with_proxy` / `with_token_and_proxy` 走 HTTP/HTTPS/SOCKS5 代理。
sha256 在下载流中边下边校验。

需要**后台非阻塞下载**时用 `download_async`:

```rust,no_run
let handle = mihomo_versions::download_async(
    &client, asset, dest, DownloadOptions::default(), |done, total| { /* 进度 */ },
);
// handle.cancel();   // 随时取消
let result = handle.wait().await?;   // 等待完成
```

asset 必须通过**精确名称**选择(`pick_asset_by_name` / `select_asset_by_name`);
先列出 `sorted_versions()` 查看各版本的 `assets[].name`,再据此下载。

### 下载缓存(按版本变体)

缓存复用逻辑由**消费方**自行实现:先检查 `dir/<base>` 是否存在,不存在再调 `download`;
`<base>` 由 `asset_base_name(asset)` 得到(资产名去掉 `.tar.gz`/`.gz`/`.zip`/`.zst`
压缩扩展名),`.part` 断点文件与 SHA256 校验约定由库统一维护:

```rust,no_run
use std::path::Path;
use mihomo_versions::{DownloadOptions, asset_base_name, download};

let dir = Path::new("/var/lib/mihomo/cache");
let dest = dir.join(asset_base_name(asset));
if !dest.exists() {
    download(client, asset, &dest, DownloadOptions::default(), |done, total| { /* 进度 */ }).await?;
}
// 此时 dest 即为最终二进制路径
```

配套的缓存管理辅助函数:

- `list_cached_downloads(dir)` — 列出目录中已下载变体的基名(跳过 `.part` / `.part.meta`);
- `remove_cached_download(dir, asset_name)` — 删除某变体的二进制及可能的断点文件。

### 查询:过滤 / 按平台列 asset

```rust,no_run
use mihomo_versions::{Channel, Platform, VersionFilter};

// 过滤 + 排序:channel / prerelease / 搜索(tag·semver 子串,大小写不敏感);
// 传 None 则列出全部版本
let filter = Some(VersionFilter {
    channel: Some(Channel::Stable),
    prerelease: Some(false),
    search: Some("1.19".into()),
    ..Default::default()
});
for v in list_versions(&index, filter.as_ref()) {
    println!("{} ({})", v.tag, v.channel);
}

// 按平台列出各版本的可用 asset:每版本一次,version.assets 即该平台全部资产
// (保持 newest-first;filter 可传 None)
for version in assets_for_platform(&index, Platform::DarwinAarch64, None) {
    println!("{} -> {}", version.tag, version.assets.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "));
}
```

## 索引格式(schema_version = 1)

```json
{
  "schema_version": 1,
  "source": { "owner": "MetaCubeX", "repo": "mihomo" },
  "generated_at": "2026-08-02T00:00:00Z",
  "versions": [
    {
      "semver": "1.19.9",
      "tag": "v1.19.9",
      "prerelease": false,
      "channel": "stable",
      "published_at": "2026-07-30T00:00:00Z",
      "created_at": "2026-07-29T00:00:00Z",
      "updated_at": "2026-07-30T00:00:00Z",
      "assets": [
        {
          "name": "mihomo-darwin-arm64-v1.19.9.gz",
          "platform": "darwin-aarch64",
          "format": "gz",
          "size": 18021733,
          "sha256": "abcd…",
          "created_at": "2026-07-29T01:00:00Z",
          "updated_at": "2026-07-30T01:00:00Z",
          "url": "https://github.com/…"
        }
      ]
    }
  ]
}
```

要点:

- `semver` 可为 `null`:非 semver tag(如 `Prerelease-Alpha`,即最新构建)会保留并按发布时间置顶。
- `channel` 为 `stable` / `alpha` / `nightly`(sync 侧按 prerelease/tag 归类);可用 `sorted_versions_by_channel(index, Channel::Stable)` 过滤。
- `sha256` 为下载的 **archive 字节**摘要(取自 GitHub API 的 asset `digest` 字段),缺失时客户端降级为警告。
- `format` 为 `gz`/`zip`/`zst`/`tar.gz`(解压)或 `raw`(直接复制);asset 通过名称精确选择,无默认优先级。
- `created_at` / `updated_at`(version 与 asset 均有)取自 GitHub API,为 RFC3339 时间戳;增量同步比较 version 与各 asset 的 `updated_at` 判断是否复用旧条目。
- 客户端对未知字段忽略;未来 schema 变更通过递增 `schema_version` 兼容。

## 责任边界

`mihomo-versions` 只负责版本索引解析、平台选择、下载与校验,**不负责 mihomo 生命周期管理**。
启动/停止、安装位置、运行参数等由上层(`mihomo-manager`)负责:

```
Application → mihomo-manager → mihomo-versions → Network / File System
```

## 示例

`examples/usage.rs` 演示完整客户端流程(加载索引 → 识别平台 → 选择 asset → 下载校验):

```bash
# 在线:按 asset 名从托管索引下载
cargo run --example usage -- https://your-cdn/mihomo-versions --asset-name mihomo-darwin-arm64-v1.19.9.gz --dest /usr/local/bin/mihomo

# 离线:只读本地索引、打印选择结果,不下载
cargo run --example usage -- path/to/mihomo-releases.json --asset-name mihomo-darwin-arm64-v1.19.9.gz --dry-run
```

本地模式相对路径按当前目录解析,文件不存在时回退到**项目根目录**(如 `mihomo-releases.json`)。

## 开发

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo test -- --ignored   # 真实 dump(10.8MB)冒烟测试
```

集成测试使用本地 mock server(wiremock),CI 不打 GitHub API。
