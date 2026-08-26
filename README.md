# mihomo-versions

为 Clash Verge Self 等应用提供 mihomo 内核版本索引和下载能力的独立 Rust 项目。

- `mihomo-versions`：解析索引、识别当前平台、筛选版本和 asset、下载并校验内核。
- `mihomo-versions-sync`：从 GitHub Releases 同步版本，按分类规则生成精简索引。

项目把 GitHub API 访问集中在同步器中，客户端只读取索引并下载 asset：

```
GitHub Releases API -> mihomo-versions-sync -> mihomo-releases.json -> 客户端
```

## 快速开始

### 生成索引

默认同步 `MetaCubeX/mihomo`，输出 `mihomo-releases.json`：

```bash
cargo run --bin mihomo-versions-sync -- \
  --repo MetaCubeX/mihomo \
  --out mihomo-releases.json
```

GitHub token 不是必需的，但可以提高 API 限流额度。CLI 参数优先于环境变量：

```bash
MIHOMO_VERSION_TOKEN=ghp_xxx cargo run --bin mihomo-versions-sync -- \
  --token ghp_cli_token
```

`--out` 和 `--classifier` 的路径解析规则相同：绝对路径和当前目录中已存在的文件按原路径使用，其他相对路径相对于项目根目录解析。

同步器是增量式的：读取已有的 plain JSON 索引，release 的时间字段、版本派生字段和保留 asset 均未变化时复用旧条目；新增或变化的 release 重新处理。GitHub 已删除的 release 会从新索引中移除。

常用选项：

```text
--max-versions <N>    只保留最新的 N 个版本
--per-page <N>        GitHub Releases 每页数量，默认 100
--compact             输出单行 JSON
--emit-gz             同时输出 plain JSON 和 gzip 副本，自动追加 .gz
--print-classifier    打印内置分类配置模板并退出
```

`--gz` 仍可读取但已废弃。它只输出 gzip 文件，并会打印迁移警告；新脚本应使用 `--emit-gz`。

### 批量同步多个仓库

用 `--config` 一次同步多个仓库。配置中不要保存 token，应通过 `--token` 或 `MIHOMO_VERSION_TOKEN` 提供：

```bash
cargo run --bin mihomo-versions-sync -- --config sync-config/sync-all.json
```

配置示例：

```json
{
  "jobs": [
    { "repo": "MetaCubeX/mihomo", "out": "index/mihomo-releases.json" },
    {
      "repo": "madeye/meow-rs",
      "out": "index/meow-rs-releases.json",
      "classifier": "classifier/meow-rs.json"
    },
    {
      "repo": "Watfaq/clash-rs",
      "out": "index/clash-rs-releases.json",
      "classifier": "classifier/clash-rs.json",
      "max_versions": 10
    }
  ]
}
```

批量模式会继续执行剩余 job；repo 无效、分类器加载失败或同步失败都会记录为失败，任意 job 失败时进程最终以非零状态退出。`--compact` 和 `--emit-gz` 会应用于所有 job。

### 自定义分类器

内置规则适用于 `MetaCubeX/mihomo`。其他仓库需要通过 `--classifier` 提供 JSON 配置：

```bash
cp classifier/mihomo.json classifier/my-repo.json
cargo run --bin mihomo-versions-sync -- \
  --repo owner/my-repo \
  --classifier classifier/my-repo.json \
  --out index/my-repo-releases.json
```

分类器支持 `keep_extensions`、`exclude_names` 和按顺序匹配的 `platforms[].patterns`。平台名称必须是客户端支持的规范名称；详细格式见 [`classifier/README.md`](classifier/README.md)。

## 客户端集成

### 获取索引并下载

```rust,no_run
use std::time::Duration;

use mihomo_versions::{
    download, fetch_index, fetch_index_cached, pick_asset_by_name, DownloadOptions, HttpClient, IndexCache,
};

let client = HttpClient::new()?;
let urls = [
    "https://cdn.example/mihomo-releases.json.gz",
    "https://github.com/owner/repo/releases/latest/download/mihomo-releases.json",
];

// 依次尝试镜像；URL 以 .gz 结尾时自动解压。
let index = fetch_index(&client, &urls).await?;
let asset = pick_asset_by_name(&index, "mihomo-darwin-arm64-v1.19.9.gz", None)?;

download(
    &client,
    asset,
    "/usr/local/bin/mihomo",
    DownloadOptions::default(),
    |done, total| println!("{done}/{total:?}"),
)
.await?;

// 也可以使用本地缓存：新鲜时不访问网络，过期时使用条件请求。
let cached = fetch_index_cached(
    &client,
    &urls,
    &IndexCache {
        path: "cache/mihomo-releases.json".into(),
        max_age: Duration::from_secs(600),
    },
)
.await?;
```

`fetch_index` 按 URL 顺序 failover。`fetch_index_cached` 会保存 plain JSON 和 ETag/Last-Modified 元数据；缓存过期后进行条件请求，所有镜像失败时回退到可用的 stale 缓存。

asset 必须使用精确名称选择。可以先遍历 `sorted_versions(&index)` 查看各版本的 `assets[].name`，再调用 `pick_asset_by_name`；传入版本号时会只在该版本中查找，不传则按 newest-first 查找。

### 下载选项

`DownloadOptions` 支持：

- `resume`：基于 HTTP `Range` 的断点续传，默认开启；断点文件为目标路径旁的 `.part` 和 `.part.meta`。
- `cancel`：传入 `CancellationToken` 取消下载。
- `idle_timeout`：单次读取的空闲超时。
- `total_timeout`：单次尝试的总超时，默认不限制；超时会按下载重试策略再次尝试。

下载流会先对归档字节计算 SHA-256，再执行解压。索引缺少 digest 时会跳过校验并记录警告。支持的 `format` 为：

- `gz`：gzip 解压；
- `zip`：读取 zip 中的文件；
- `zst`：zstd 解压；
- `tar.gz`：先 gunzip，再从 tar 中提取文件；
- `raw`：原样复制。

需要后台非阻塞下载时使用 `download_async`：

```rust,no_run
let handle = mihomo_versions::download_async(
    &client,
    asset,
    dest,
    mihomo_versions::DownloadOptions::default(),
    |done, total| println!("{done}/{total:?}"),
);

handle.cancel();
let result = handle.wait().await?;
```

`HttpClient` 提供 `with_proxy`、`with_token` 和 `with_token_and_proxy`，支持 HTTP、HTTPS 和 SOCKS5 代理。

调用方自行决定已完成下载的缓存路径，例如把版本号、平台和 asset 名编码到目标文件名中；库只负责目标文件、`.part` 文件和原子安装过程。

### 查询版本和平台 asset

```rust,no_run
use mihomo_versions::{assets_for_platform, list_versions, Channel, Platform, VersionFilter};

let filter = VersionFilter {
    channel: Some(Channel::Stable),
    prerelease: Some(false),
    search: Some("1.19".into()),
};

for version in list_versions(&index, Some(&filter)) {
    println!("{} ({})", version.tag, version.channel);
}

for version in assets_for_platform(&index, Platform::DarwinAarch64, Some(&filter)) {
    for asset in &version.assets {
        println!("{} -> {}", version.tag, asset.name);
    }
}
```

`Channel` 支持 `Stable`、`Alpha` 和 `Nightly`。`search` 不区分大小写，会匹配 tag 或 normalized semver。`assets_for_platform` 返回每个版本一次，并将该平台的全部 asset 放在 `version.assets` 中。

## 索引格式

当前 schema 版本为 `1`：

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
          "sha256": "abcd1234...",
          "created_at": "2026-07-29T01:00:00Z",
          "updated_at": "2026-07-30T00:00:00Z",
          "url": "https://github.com/..."
        }
      ]
    }
  ]
}
```

说明：

- `semver` 是去掉 tag 前缀后的规范版本；非 semver tag 使用 `null`，例如 `Prerelease-Alpha`。
- `channel` 由同步器根据 tag 和 prerelease 状态归类为 `stable`、`alpha` 或 `nightly`。
- `sha256` 是 GitHub asset 归档字节的 SHA-256 digest，不是解压后文件的摘要；字段缺失时为 `null`。
- `platform` 使用规范平台名：`darwin-x86_64`、`darwin-aarch64`、`windows-x86_64`、`windows-aarch64`、`windows-x86`、`windows-arm`、`linux-x86_64`、`linux-aarch64`、`linux-x86`、`linux-arm`。
- `created_at`、`updated_at` 和 `published_at` 是 GitHub API 返回的 RFC3339 时间戳，均可能为空。
- 客户端会忽略未知字段；未来不兼容的格式变化通过递增 `schema_version` 表示。

## 示例和责任边界

完整客户端流程见 [`examples/usage.rs`](examples/usage.rs)：支持在线或本地索引、当前平台识别、精确 asset 选择、dry-run 和下载。

```bash
# 在线下载
cargo run --example usage -- \
  https://your-cdn/mihomo-releases.json \
  --asset-name mihomo-darwin-arm64-v1.19.9.gz \
  --dest /usr/local/bin/mihomo

# 离线读取本地索引并打印选择结果
cargo run --example usage -- \
  path/to/mihomo-releases.json \
  --asset-name mihomo-darwin-arm64-v1.19.9.gz \
  --dry-run
```

本地索引路径先按当前目录解析；文件不存在时回退到项目根目录。库只负责索引、平台选择、下载和校验，不负责 mihomo 的启动、停止、安装位置或运行参数，这些职责属于上层 `mihomo-manager`。

## 开发

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo test -- --ignored
```

普通集成测试使用本地 `wiremock` mock server，不访问 GitHub API。`cargo test -- --ignored` 会运行基于仓库根目录 `github-release.json` 快照的真实 dump 冒烟测试。
