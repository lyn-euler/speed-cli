# speed-cli

Rust 实现的网络测速命令行工具，覆盖下载、上传、延迟与抖动测试，并内置历史记录与导出能力。

## 功能

- 下载 / 上传 / 延迟 / 抖动测速
- 自动选服（并发探测最低延迟）与手动指定服务器
- 交互式测试过程 UI（阶段进度条、彩色结果）
- SQLite 历史记录查询
- JSON / CSV 导出
- TOML 配置管理

## 快速开始

### 1) 构建与运行

```bash
cargo run --
```

默认会执行 `test` 子命令。

### 2) 常用命令

```bash
# 全量测速（默认）
speed-test

# 指定测试参数
speed-test test --duration 15 --concurrency 16 --timeout 10

# 仅测某一项
speed-test test --unit latency
speed-test test --unit download
speed-test test --unit upload

# 指定服务器（id 或 name）
speed-test test --server zju-ipv4

# 自定义 URL（下载/上传/延迟都使用该 URL）
speed-test test --server http://example.com/ping

# 面向脚本
speed-test test --quiet
speed-test test --json
speed-test test --no-save
```

## 服务器管理

```bash
# 查看服务器列表
speed-test servers list

# 探测所有服务器延迟/抖动
speed-test servers probe
```

当前默认内置服务器包含：

- `zju-ipv4` / `zju-ipv6`
- `cloudflare`
- `fsn1-hetzner`

## 配置管理

```bash
# 查看完整配置
speed-test config show

# 读取单项配置
speed-test config get test.concurrency

# 修改配置
speed-test config set test.duration_seconds 15
speed-test config set test.concurrency 16
```

支持的 `config key`：

- `test.duration_seconds`
- `test.concurrency`
- `test.timeout_seconds`
- `test.chunk_size_bytes`
- `test.max_download_bytes`
- `test.max_upload_bytes`
- `test.save_history`
- `test.latency_probe_count`

默认测试参数：

- `duration_seconds = 10`
- `concurrency = 16`
- `timeout_seconds = 15`
- `chunk_size_bytes = 65536`
- `max_download_bytes = 2000000000`
- `max_upload_bytes = 1000000000`
- `save_history = true`
- `latency_probe_count = 12`

程序首次启动会自动生成配置文件；若检测到旧版本配置（如并发为 4 或 8），会自动升级到新的默认并发 16。

## 历史记录与导出

```bash
# 最近 20 条（默认）
speed-test history

# 指定条数 / 时间范围 / JSON 输出
speed-test history --limit 50
speed-test history --since 2026-03-01T00:00:00+08:00 --until 2026-03-31T23:59:59+08:00
speed-test history --json

# 导出
speed-test export --format json --out ./out/history.json
speed-test export --format csv --out ./out/history.csv
```

`since/until` 使用 RFC3339 时间格式。

## 交互输出说明

- 交互模式（默认）：显示阶段进度条、服务器探测信息、彩色结果提示
- `--quiet`：输出紧凑键值对，适合 shell 脚本
- `--json`：输出结构化 JSON，适合集成系统

## 数据存储

- 配置：系统用户目录下的应用配置路径（由 `ProjectDirs` 决定）
- 数据库：`speed-test.sqlite`（同样位于系统应用数据目录）

## 说明

- 自动选服基于延迟探测结果，实际最优吞吐可能与最低延迟服务器不完全一致。
- 高带宽场景建议提升 `--duration` 与 `--concurrency`，以减少短时波动对结果的影响。
