# CreditFerry 积分摆渡

把 **Loomy（讯飞）**、**小浣熊（商汤）**、**OpenCode 免费模型池** 三个来源的模型，以本机 OpenAI 兼容口（`http://127.0.0.1:6666/v1`）摆渡给 **WorkBuddy / Qwen Code / DeepSeekHarness** 等客户端。Tauri 2（Rust + WebView）桌面应用，凭据只存本机。

![three sources](https://img.shields.io/badge/sources-Loomy%20%C2%B7%20Raccoon%20%C2%B7%20OpenCode-2563eb) ![tests](https://img.shields.io/badge/tests-4%20passed-047857)

## 它能做什么

| 上游 | 计费 | 登录方式 | 转发形态 |
|---|---|---|---|
| Loomy | 积分（响应 `usage.points_consumed` 真实回传） | 扫描本机客户端登录态 / 手动粘贴会话 | OpenAI 兼容直通 |
| 小浣熊 | 积分（网关不回传单次积分，按余额快照展示） | 微信扫码（二维码本地生成） | OpenAI 兼容直通 |
| OpenCode | 免费池（不扣积分） | 无需登录（隔离运行时自动就绪） | 信封协议翻译 |

客户端经 `POST /v1/chat/completions` 调用，模型 id 形如 `Loomy · Spark-X2.5` / `Raccoon · DeepSeek V4.1 Flash` / `OC · Big Pickle`。

## 快速开始

```bash
npm install
npm run build          # tauri build（NSIS 安装包 + portable）
# 或开发运行：
npm run dev
```

首次启动：自动拉起转发服务 → 自动启动隔离 OpenCode 运行时（首次下载二进制约一两分钟）→ 后台刷新各上游模型。面板里：

1. **Loomy**：点「扫描本机客户端」（读取 `C:\Users\Public\Loomy\*\userData\auth-session.json`，只读）或手动粘贴会话；
2. **小浣熊**：点「微信扫码登录」，手机扫码确认；
3. **OpenCode**：自动就绪，无需操作；
4. **导出**：点「写入 WorkBuddy models.json」（CN 版 `~/.workbuddy`、国际版 `~/.workbuddy-ai` 各一个按钮）、或「写入 Qoder CN」（`~/.qoder-cn/settings.json` 的 `providers` 下写一个 `openai-compatible` 提供方）。全部自动备份、只动自己标记（`creditFerryOwner: credit-ferry-v1`）的条目；Qwen Code / DSH 的配置片段在下方标签页，复制即用。

## 接进三个客户端

**WorkBuddy**：`~/.workbuddy/models.json` 由工具合并写入（`creditFerryOwner: "credit-ferry-v1"` 标记，退出时自动摘除，用户手写条目不动）。

**Qwen Code**（`~/.qwen/settings.json`）：

```json
{
  "modelProviders": {
    "openai": [{ "id": "credit-ferry", "name": "CreditFerry",
                 "baseUrl": "http://127.0.0.1:6666/v1", "envKey": "CREDITFERRY_API_KEY" }]
  },
  "env": { "CREDITFERRY_API_KEY": "<数据目录>/api-key" }
}
```

**DeepSeekHarness**：经自定义 provider / 插件机制注册 OpenAI 兼容 provider（`baseUrl` 同上），或用 `OPENAI_BASE_URL` + `OPENAI_API_KEY` 环境变量指向本机。

## 安全设计

- **转发口只听 127.0.0.1**：Bearer 密钥常量时间比较；任何带 `Origin` 头的请求 403；请求体 8 MB 上限；并发上限 4。
- **凭据不出本机**：Loomy 会话、小浣熊令牌、入口密钥全部落在 `%APPDATA%\CreditFerry\`，不明文外传；小浣熊 refresh token 单次轮换、过期前自动续期。
- **OpenCode 隔离运行时**：托管二进制（sha512 校验下载）、环境变量白名单、四个 XDG 目录重定向、随机 Basic 口令、自动更新/分享/项目配置/外部技能全关。
- **信封协议 + 审批围栏**：OpenCode 侧模型只能以 JSON 信封「提议」动作（schema 由本次请求工具表焊死），本机动作一律自动拒绝，执行权始终在客户端。
- **写用户配置**：归属标记只删自己的、id 冲突让位用户条目、写前备份（保留 5 份）、原子替换。

## 代码结构

```
src-tauri/src/
  main.rs       入口：窗口/托盘/自启/状态装配
  commands.rs   Tauri 命令层（面板 ↔ 核心）
  server.rs     本机 HTTP 面：围栏、路由、SSE tee、积分记账
  loomy.rs      Loomy 上游：会话头（traceparent 必需）、/models、/points/records、会话导入
  raccoon.rs    小浣熊上游：QR 登录/轮换、/model_catalog、/balance、chat
  opencode.rs   OpenCode：运行时获取/隔离启动、/provider 免费判定、信封翻译、审批拒绝
  export.rs     WorkBuddy 合并 / Qwen / DSH 片段
  store.rs      本地 JSON 存储、JWT 过期解析、积分账本
src-frontend/   控制面板（原生 HTML/JS，无构建步骤）
```

## 已知边界

- 积分额度由各平台发放与回收，「不可用」只代表当时没通过校验；余额以平台客户端为准。
- 小浣熊网关不回传单次积分消耗，面板与账本对该来源只记 token 与余额快照。
- OpenCode 免费池随上游上下线；判定不缓存（每次启动重拉目录）。
- 系统代理只支持手动 HTTP/HTTPS。
- 本工具调用你在各平台自己的积分权益，请遵守对应服务条款。

## 开发

```bash
cargo test                                    # Rust 单元测试（配置合并/归属标记）
cargo check                                   # 类型检查
./node_modules/.bin/tauri build --no-bundle   # 只出 exe，不打安装包
```
