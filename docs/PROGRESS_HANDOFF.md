# Emby Bridge 进度交接

更新日期：2026-10-22。本文记录当前实现与已取得的验证证据，不代表完整 Emby 兼容认证。

## 目标与现状

- 不依赖 Emby/Jellyfin 服务端。Rust 进程模拟直接播放所需的 Emby API，Caddy 提供统一入口，多个 worker 的媒体目录汇总到一个 gateway。
- 支持普通视频文件和 `.strm`。普通文件由 worker 经 gateway 传输；STRM 返回 302，由客户端直接访问目标 URL。没有视频/音频转码或重封装。
- 当前为单用户、直放优先的第一版。`main` 分支为交接基线，具体接口和限制以 [API_COMPATIBILITY.md](API_COMPATIBILITY.md) 为准。

## 已实现

| 领域 | 当前能力 |
| --- | --- |
| API | 48 个 Emby 路由模式、61 个 HTTP 操作；GET/HEAD 分别计数，不含 `/emby` 别名及内部接口。登录、媒体库、列表/详情、搜索、剧集、收藏、进度、会话、PlaybackInfo、图片、流和字幕有基础实现。 |
| 媒体 | 本地文件支持 HEAD、单段 Range、206/416、ETag；STRM 支持客户端可达 HTTP(S) 链接的 302 直达。`ffprobe` 后台读取普通文件时长及轨道；本地 NFO 和相邻海报可补充元数据。 |
| 多版本与轨道 | `[version=label]` 文件聚合成一个条目和多个 MediaSources；记录每个版本的音频/字幕轨道，PlaybackInfo 校验所选版本和轨道。音轨切换由客户端在原文件中完成。 |
| 字幕 | 发现相邻 SRT/ASS/SSA/VTT，提供字幕 URL、HEAD/Range、文本转换；本地内封文本字幕可提取。无字幕 `-1` 可选。 |
| 多节点 | worker 注册目录并心跳，gateway 合并条目；同版本副本按健康状态、权重和活动流数路由，节点故障时尝试其他副本。示例 `node-a`、`node-b` 挂载同一目录，仅用于演示副本。 |
| 扩展 | `src/hooks.rs` 提供扫描、注册、认证、路由、播放、字幕等事件钩子；`RoutingPolicy` 和 `SourceResolver` 分别是节点选择与资源解析的 Rust 扩展接口。 |
| 状态 | SQLite 持久化服务端 ID、登录令牌、播放进度、收藏和客户端偏好；活动会话在内存中。 |

接口勾选表为 [Emby接口覆盖勾选清单.xlsx](Emby接口覆盖勾选清单.xlsx)。表中共有 555 项（官方基线 548 项及项目补充 7 项），当前预选 75 项：61 项标记“基础实现”，14 项标记“未实现”。“基础实现”只说明有相应服务端能力，不等于 Hills、Yamby 或 SenPlayer 已逐项验收，也不是完整 Emby API 覆盖率。

## 验证记录

| 时间 | 结果 | 边界 |
| --- | --- | --- |
| 2026-10-21 | 18 个 Rust 测试通过；双 worker Docker 聚焦验证了多版本、双音轨、Range 206、外挂 SRT 转 VTT、内封文本字幕提取等。 | 临时测试素材已移除；这是接口/容器验证，不是客户端验收。 |
| 2026-10-22 | Hills Lite 1.5.5 已登录，首页显示 `Media` 库及 `Local`、`Remote`；`Local` 详情显示 2 秒 MP4、MPEG-4 视频和 AAC 音轨；实际播放器显示测试画面及约 1 秒播放进度。 | `Local.mp4` 仅约 2 秒，不足以可靠验证拖动/续播。`Remote.strm` 指向 `example.com` 占位 URL，不能作为真实网盘播放验证。搜索、字幕切换、多版本选择尚未在该客户端验收。 |
| 2026-10-22 当前检查 | `docker compose ps` 未列出本项目运行中的容器。 | 上述 Docker 与 Hills 结果是此前运行时的记录；再次验收须先启动环境。 |

Yamby 和 SenPlayer 尚无真实客户端测试结果。现有 Caddy 配置未启用访问日志，正常请求也不在 gateway 逐条记录；未处理路由会由 gateway 记录。排查客户端差异时需获取脱敏请求路径，不能据“日志为空”推断所有请求均成功。

## 本地复现

在仓库根目录的 PowerShell 中：

```powershell
Copy-Item .env.example .env
New-Item -ItemType Directory media -Force
# 修改 .env 中的 EMBY_ADMIN_PASSWORD 和 EMBY_INTERNAL_KEY
docker compose up --build -d
docker compose ps
```

把普通视频或 `.strm` 放入 `media`，客户端将 `http://127.0.0.1:8096` 添加为 Emby 服务端，用 `.env` 内账户登录。`media`、`.env`、数据库、`target` 和 `.artifact-work` 均不提交到 Git；新克隆仓库不含测试媒体。此前 Hills 验证用的 `Remote.strm` 是占位 URL，测试远端播放需使用客户端可访问且支持 Range 的真实媒体地址。

生产部署参见 [README.md](../README.md)：设置真实域名和 DNS 后，使用 `docker compose -f compose.yaml -f compose.production.yaml up --build -d` 暴露 80/443 及 Caddy HTTPS。覆盖文件依赖 Docker Compose 2.24.4+ 的 `!override`。不要把本地 HTTP 8096、worker 或 gateway 内部端口直接暴露到公网；不要将永久网盘主凭据放进 STRM URL。

## 下一步

1. 用较长的本地测试片、真实可达 STRM、双版本/多音轨/外挂字幕和两集剧集，在 Hills Lite 实测拖动、切轨、字幕开关、续播与下一集；再分别记录 Yamby、SenPlayer 的版本、平台和实际请求差异。
2. 以真实客户端失败路径为依据补齐接口。Excel 中 14 个预选待实现项只是候选范围，先核对客户端调用，再定优先级。
3. 对需要额外请求头、短期签名或内网跳转的网盘链接实现可插拔 resolver/受控代理；当前简单 302 不能覆盖这类来源。
4. 按需补用户音频/字幕偏好、更多图片规格、ASS 字体附件和字幕提取缓存。WebSocket/远程控制、多用户、直播/DVR、完整 Emby 管理接口及转码不在这一版范围内。

关键文件：`src/gateway.rs`（API/入口）、`src/worker.rs`（扫描/媒体服务）、`src/cluster.rs`（注册与路由）、`src/catalog.rs`（目录）、`src/subtitles.rs`（字幕）、`src/hooks.rs`（钩子）、`src/resolver.rs`（来源解析）。媒体命名和 NFO 规则见 [METADATA.md](METADATA.md)，客户端验收范围见 [CLIENT_SCOPE.md](CLIENT_SCOPE.md)。
