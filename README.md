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
4. **导出**：面板「导出到客户端」里五处一键配置——WorkBuddy CN（`~/.workbuddy/models.json`）、WorkBuddy 国际版（`~/.workbuddy-ai/models.json`）、Qoder CN（`~/.qoder-cn/settings.json`）、Qoder 国际版（`~/.qoder/settings.json`）、DeepSeek Harness（`~/.dsh/profiles/desktop/cordis.patch.yml` + `.credentials.yaml`）。全部自动备份、只动自己标记（`creditFerryOwner: credit-ferry-v1` / 成对注释块）的条目，退出托盘菜单会一并摘除。
5. **DSH 连接器**：同一张面板可以按 DSH 内核版本安装/升级/启停/卸载 `dsh-workbuddy-connect` 与 `dsh-qoder-connect` 两个社区插件（详见下一节）。

## 接进客户端

**WorkBuddy**：`~/.workbuddy/models.json` 由工具合并写入（`creditFerryOwner: "credit-ferry-v1"` 标记，退出时自动摘除，用户手写条目不动）。

**Qoder**（CN `~/.qoder-cn/settings.json`、国际版 `~/.qoder/settings.json`）：在 `providers` 下写一个 `openai-compatible` 提供方，`model` 一律用面板里的完整模型 id（如 `Loomy · DeepSeek V4.1 Flash(x2.0)`），密钥复用入口密钥。

**DeepSeek Harness**（`~/.dsh/profiles/desktop/`）：一键把提供方注入 `cordis.patch.yml` 的 `llm-pi-ai → providers:`（成对注释块 `# >>> credit-ferry >>>` 包住，可重复执行不叠加），密钥写进 `~/.dsh/.credentials.yaml` 的 `refs: CREDITFERRY_API_KEY`。

## DSH 连接器平台（反方向）

`~/.dsh` 里还可以装两个社区插件，把 **WorkBuddy / Qoder 自己的模型接进 DSH**——方向和上面相反，CreditFerry 只做安装管理与状态读取：

| 平台 | 包名 | 凭据 | 数据位置 |
|---|---|---|---|
| WorkBuddy 连接器 | `dsh-workbuddy-connect` | 直接复用 WorkBuddy 桌面 App 登录（国内版/国际版两组） | `$DSH_HOME/.workbuddy*.json` |
| Qoder 连接器 | `dsh-qoder-connect` | Qoder 个人访问令牌 PAT（CN `qoder` / 国际版 `qoder-global`） | `<profile>/.dsh-qoder-connect/` |

插件激活 = profile 的 `package.json` 里有该依赖 **且** 名字在 `dsh.profile.bundles` 里，DSH 才会套用插件自带的 `cordis.patch.yml`（loader `llm-workbuddy` / `llm-qoder`）。所以本工具只碰这两处和依赖安装本身，loader 树与凭据都由插件自己保管；面板只读「存了没有 / 目录来自哪里 / 多少个模型 / 什么时候签到的」，绝不回显令牌。安装用 DSH 自带的 pnpm 运行时（`~/.dsh/dsh-runtimes/*/dependencies/{node,pnpm}`），找不到时才退回系统 pnpm，两者都没有就在报错里给出 `dsh plugin --profile <p> add <pkg>@<ver>` 手动命令。

版本按各自官方兼容表挑（`dsh_connect.rs::recommended`）：内核 `0.2.0-rc.2` → WorkBuddy `0.7.5` / Qoder `0.2.3`；`0.2.0-rc.1` → `0.7.0`；`0.1.5–0.1.7` → `0.6.5`；Qoder 需要 `>=0.1.7-rc.1`，`0.1.5` 线只能用 `0.2.0`。内核版本来源：桌面 profile 读 `runtime.json` 的 `desktopVersion`，其它 profile 从 `pnpm-lock.yaml` 里 `'@deepseek-ai/dsh-llm': <具体版本>` 解析（范围行跳过）。

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
  maodao.rs     码道（华为 CodeArts Doer）：SDK-HMAC-SHA256 签名、DPoP/PKCE 登录、客户端密钥库导入
  probe.rs      可调用性验证：逐模型探活 + TTL 结论（available/timeout/unavailable）
  export.rs     WorkBuddy / Qoder / DSH 配置合并与摘除
  dsh_connect.rs DSH 连接器平台：profile 发现、内核匹配版本、安装/启停/卸载与状态读取
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
