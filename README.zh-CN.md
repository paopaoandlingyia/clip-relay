# Clip Relay

[English](README.md) | 简体中文

Clip Relay 是一个自托管的剪贴板应用，用于在设备间快速分享文本、图片和文件。采用 Rust 服务端与 Next.js 静态前端，支持实时同步、拖拽上传和轻量认证，适合个人或小团队使用。

- 实时同步：通过 SSE（Server-Sent Events）广播新建/删除事件
- 拖拽上传：小文件内联存储，大文件落地到磁盘
 - 轻量认证：单一访问口令，默认保持登录 7 天（可配置），可在设置中手动退出
- 响应式 UI：基于 shadcn/ui 与 Tailwind CSS 4

## 在线演示
- 访问：https://clip-frontend.808711.xyz

## 架构概览
- 前端：Next.js App Router（React 19），静态导出（无 SSR），目录 `src/app`, `src/components/ui`
- 服务端：Rust（axum）位于 `rust-server/`，提供 API 并同时服务静态前端；SSE 实时通道位于 `/api/events`
- 数据：SQLite（`rusqlite`，自带静态链接），持久化目录 `data/`（大文件位于 `data/uploads/`）
- 认证：Rust 端 `/api/auth/verify` 验证口令并写入 Cookie

## 快速开始（本地开发）
### 依赖
- Node.js 20+（用于构建静态前端）
- npm 10+
- Rust 工具链（如需本地直接运行服务端）

### 安装依赖并运行
```bash
# 安装前端依赖
npm install

# 1) 构建静态前端到 out/
npm run build

# 2) 启动 Rust 服务（同时服务静态前端）
npm run rust:dev
```
默认监听 `http://localhost:8087`（可通过 `PORT` 覆盖）。如需前端热开发，可单独运行 Next 开发服并通过 `NEXT_PUBLIC_API_BASE` 指向 Rust API。

## 生产构建与运行
```bash
npm run build
npm start
```
- `npm start` 直接启动服务；若 SQLite 文件为空，会在首次访问前自动创建表结构。
- 监听端口 `8087`（可通过 `PORT` 覆盖）。

## 环境变量
在项目根目录创建 `.env`（最少包含以下一项）：

```
CLIPBOARD_PASSWORD="change-me"
# 可选：覆盖默认设置
# STATIC_DIR="/app/out"   # 静态前端目录
# PORT=8087                         # 监听端口
# AUTH_MAX_AGE_SECONDS=604800       # 认证 Cookie 有效期（秒），默认 7 天
# DATA_DIR="/app/data"              # SQLite + uploads 所在目录（默认：自动检测）
# HEALTH_VERBOSE=1                   # 在 /api/healthz 返回 dataDir/dbPath（便于排查）
```
- `CLIPBOARD_PASSWORD` 为访问口令。
- `STATIC_DIR` 可选；默认会优先自动探测 `out/`，并兼容旧的 `.next-export/` 目录。
- `AUTH_MAX_AGE_SECONDS` 控制登录 Cookie 的有效期（秒）。默认 7 天，设置更短/更长可按需调整。
- SQLite 位于 `data/custom.db`（首次启动自动创建）。Docker 镜像默认设置 `DATA_DIR=/app/data`。
  - 非 Docker 本地开发时，服务会自动检测数据目录（通常为项目根目录下的 `./data`）。
  - 若自动检测的目录不可写，服务会在启动阶段直接报错退出；请设置 `DATA_DIR` 指向可写路径。

### 本地构建镜像
```bash
docker build -t clip-relay:latest -f Dockerfile .
```

### 使用 Docker Compose（推荐）
1) 准备 `.env`：
```
CLIPBOARD_PASSWORD="change-me"
```
2) Compose 示例（持久化数据目录）：
```
services:
  app:
    image: <your-registry>/clip-relay:latest
    ports:
      - "8087:8087"  # Rust 服务默认 8087
    env_file: .env
    volumes:
      - /srv/clip-relay/data:/app/data
    restart: unless-stopped
```

说明：
- 在 Docker 容器中，应用工作目录为 `/app`，默认数据库路径即 `/app/data/custom.db`。上面的 `volumes` 将该目录持久化到宿主机。

## 数据存储与备份
- 数据库：`data/custom.db`（SQLite，含元数据与小文件 BLOB）
- 大文件：`data/uploads/`（启动时自动创建，API 流式读取）
- 备份/迁移时请同时备份上述两处。

### 可选：启用 S3（大文件存储）
当配置了 `S3_*` 环境变量时（`S3_ENDPOINT`、`S3_BUCKET`、`S3_REGION`）：
- 大文件由应用直接上传到 S3（对象前缀：`uploads/`）；较小的文件仍以内联方式存放在 SQLite 中。

> 注意：当前没有数据库备份机制。SQLite 数据库仅存放在 `DATA_DIR` 下，请自行备份该目录。（内置的数据库备份/恢复方案在计划中。）

## 使用提示与常见问题
- 复制按钮与 HTTP 环境
  - 浏览器的剪贴板 API 需要“安全上下文”（HTTPS 或 localhost）。在 HTTP 环境下，系统复制可能受限。
  - 本应用已内置降级与提示：HTTPS/localhost 下会直接复制；HTTP 下会提示并选中文本，便于手动复制。建议在生产启用 HTTPS 以获得最佳体验。

## 目录结构
```
src/
├─ app/              # App Router 页面、layout、全局样式
├─ components/ui/    # 可复用 UI 组件封装
├─ components/clipboard/ # 剪贴板业务组件
├─ hooks/            # 自定义 hooks（toast 等）
└─ lib/              # 鉴权、SSE、工具函数
rust-server/         # Rust (axum) API 服务（同时服务静态前端）
```

## 许可证
MIT  - 请参阅 [LICENSE](LICENSE)

