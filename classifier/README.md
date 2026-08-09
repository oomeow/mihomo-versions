# classify 目录说明

该目录存放 `mihomo-versions-sync` 的 **资产分类规则**(`ClassifierConfig`)。

## 为什么需要分类规则

不同 GitHub 仓库对 binary 资产有不同的命名方式(前缀、平台标识、变体后缀等)。
`mihomo-versions-sync` 不把任何仓库的命名规则硬编码进代码,而是由 JSON 配置驱动:

- **`mihomo.json`** — MetaCubeX/mihomo 的默认规则,编译期嵌入 sync 二进制(`mihomo_config()`),
  开箱即用;
- **其他仓库** — 复制 `mihomo.json` 并按其命名修改,通过 `--classifier <path>` 传入。

## 配置格式

```json
{
  "keep_extensions": ["gz", "zip"],
  "exclude_names": [],
  "platforms": [
    { "name": "darwin-aarch64", "patterns": ["darwin-arm64", "darwin-aarch64"] }
  ]
}
```

| 字段 | 含义 |
|------|------|
| `keep_extensions` | **保留的文件扩展名列表**,按 asset 名**最后一段扩展名**匹配(如 `gz` / `zip`);**空数组 `[]` = 接受所有类型**(含无扩展名的直接可执行文件)。复合归档按末段匹配:`.tar.gz` 的末段是 `gz`,配置 `"gz"` 即可保留,写入索引的 `format` 为 `tar.gz`。旧字段名 `keep_formats` 仍被接受(向后兼容) |
| `exclude_names` | 按精确名跳过的资产(如辅助文件) |
| `platforms[]` | 平台规则,按顺序匹配:`patterns` 中任一子串出现在资产名中 → 归为该 `name`(写入索引的 platform 标识) |

索引中 `format` 的取值:`gz`/`zip`/`tar.gz`/`zst` 表示下载后需解压(`tar.gz` 为 gunzip 后解出 tar 内的二进制,`zst` 为 zstd 解压),其余一律为 `raw`
(直接复制,如无扩展名的可执行文件或 `deb` 等非归档文件)。`keep_extensions` 按末段扩展名筛选,与 `format` 不必一一对应。

> **平台名必须受限于 `Platform` 枚举**:`platforms[].name` 只能是客户端可解析的
> 十个值之一 —— `darwin-x86_64`、`darwin-aarch64`、`windows-x86_64`、
> `windows-aarch64`、`windows-x86`、`windows-arm`、`linux-x86_64`、
> `linux-aarch64`、`linux-x86`、`linux-arm`。sync 加载配置时(`--classifier`)
> 会做 `validate()` 校验,枚举外的名字会直接报错;可配置的是 `patterns`(如何
> 把该仓库的命名映射到这些平台),而不是平台本身。

### 分类流程

1. 名称在 `exclude_names` 中 → 跳过;
2. 末段扩展名不在 `keep_extensions` 中(空 `keep_extensions` 不过滤)→ 跳过;
3. 匹配首个 `platforms[].patterns` 子串 → 得到 platform。

## 为其他仓库派生规则

```bash
cp classifier/mihomo.json classifier/<repo>.json
# 编辑:按该仓库的资产命名修改 platforms[].patterns、keep_extensions、exclude_names
#       (platforms[].name 必须仍为 Platform 枚举的六个值之一)
./target/release/mihomo-versions-sync \
  --repo <owner>/<repo> \
  --classifier classifier/<repo>.json \
  --out mihomo-releases.json
```

> 提示:`patterns` 子串不需要覆盖完整命名规则,只要在该仓库的资产名中稳定出现即可。
> 例如 mihomo 老版本用 `Clash.Meta-` 前缀、新版本用 `mihomo-`,平台 token `darwin-arm64`
> 两种写法下都存在,因此只靠平台子串即可归类。
