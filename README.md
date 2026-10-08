<div align="center">

<img src="build-assets/app-icon.png" alt="Switch AI" width="128" />

# Switch AI

**AI 账号多开管理一体化工作台 · 新增 WorkBuddy 国际版（workbuddy.ai）支持**

Windows 桌面端 · Tauri 2 + React 18 + Rust

</div>

---

## ⚠️ 项目来源与归属声明（必读）

本项目是 **[smart-open/TraeWorkAssistant](https://github.com/smart-open/TraeWorkAssistant)（产品名「AI Work 助手」）的二次开发分支**，按其 MIT License 要求保留原始版权与出处：

- **原项目**：AI Work 助手 / TraeWorkAssistant — <https://github.com/smart-open/TraeWorkAssistant>
- **原作者**：朱天伟（Copyright © 2026 朱天伟）
- **许可证**：[MIT](LICENSE)（本分支**原样保留** `LICENSE` 与版权声明，未做任何修改）
- **本分支定位**：以原库为基础，补齐并强化 **WorkBuddy 国际版（`www.workbuddy.ai`）** 的账号录入、登录态切换、签到/积分自动化与 API 网关接入。

> 除本 README 与下述「WorkBuddy 国际版」相关改动外，其余功能、设计与实现均来自原项目，版权与功劳归原作者。
> 原项目功能说明、用户手册与完整更新日志见 [原仓库 README](https://github.com/smart-open/TraeWorkAssistant)。

---

## 本分支新增：WorkBuddy 国际版支持

WorkBuddy 国际版是腾讯面向海外市场发布的 AI 编程/办公智能体（官网 <https://www.workbuddy.ai>），
与国内版的**账号体系与网关完全独立**：

| | 国内版（CN） | 国际版（Global） |
| --- | --- | --- |
| 站点 | `www.workbuddy.cn` / `www.codebuddy.cn` | `www.workbuddy.ai` |
| 登录方式 | 扫码 / 手机号 | **Google / GitHub OAuth** |
| 可用模型 | 国产模型（DeepSeek / GLM / 混元等） | **Claude / GPT-5 / Gemini** |
| 积分 | 国内计费体系 | 试用积分 + 每日重置 + Pro 订阅 |

> 国际版与国内版**不可混用**：把国际版账号的请求打到国内网关会失败（反之亦然）。
> 这正是本分支要解决的核心问题。

### 原项目的现状与本分支的改动

原项目已具备「区域（`domain` 含 `.workbuddy.ai` → Global）」这一概念的**部分接线**，
但区域是**从凭证 `domain` 字段推断**的，而该字段在手工录入、旧版导入、部分 OAuth 返回中**经常缺失**——
此时账号会被**静默当作国内版**，导致国际版账号的签到 / 积分 / chat 全部打到国内网关而失败。

本分支做了如下改动：

#### 1. 引入显式的区域类型与统一解析（消除 9 处分散且不一致的判断）

新增 `WbRegion { Cn, Global }`（`src-tauri/src/tasks/wb_common.rs`），并集中提供：

| 方法 | CN | Global | 用途 |
| --- | --- | --- | --- |
| `billing_base()` | `www.codebuddy.cn` | `www.workbuddy.ai` | 签到 / 成长中心 / billing meter |
| `credits_base()` | `www.workbuddy.cn` | `www.workbuddy.ai` | 积分三件套 / 官方用量 / 活动 |
| `chat_base()` | `copilot.tencent.com` | `www.workbuddy.ai` | chat 上游 / 模型目录 |
| `plugin_base()` | `copilot.tencent.com` | `www.workbuddy.ai` | OAuth（auth/state、auth/token、login/account） |
| `web_origin()` | `www.codebuddy.cn` | `www.workbuddy.ai` | OAuth Web 侧 Origin/Referer |
| `refresh_url()` | `codebuddy.cn/…` | `workbuddy.ai/…` | plugin token refresh |

**注意国内版有两个不同站点**：计费/签到在 `codebuddy.cn`，积分页在 `workbuddy.cn`——
原实现对此处理正确，本分支在重构中**保留了这一区分**（`billing_base` vs `credits_base`），未做错误合并。

同时修复了一个**真实缺陷**：原判定用 `domain.contains(".workbuddy.ai")`（**前导点**），
当 `domain` 恰为 `workbuddy.ai`（无子域前缀）时会被误判为国内版。现改为主机名后缀匹配，
并容忍协议 / 端口 / 前导点 / 大小写写法，同时**拒绝** `workbuddy.ai.evil.com` 之类的后缀伪造。

#### 2. 账号级显式区域字段（本分支最关键的改动）

`WorkBuddyAccount` 新增 `region` 字段（`"cn"` / `"global"`，空串 = 按凭证 `domain` 旧数据兼容），
并落库到 SQLite `wb_accounts` 表。区域解析优先级：

```
账号显式 region  >  凭证记录 region  >  凭证记录 domain  >  默认 CN
```

这使**国际版身份不再依赖一个可能缺失的推断字段**。凭证记录（token store）同样会写入 `region`，
供 API 网关上游取号与区域化刷新端点使用。

#### 3. 区域化 OAuth 登录（国际版账号的录入通道）

OAuth 流程改为按区域选择基址，前端可选「国内版 / 国际版」：

```
POST https://www.workbuddy.ai/v2/plugin/auth/state?platform=CLI   → 200
     data.authUrl = https://www.workbuddy.ai/login?platform=CLI&state=<uuid>
GET  /v2/plugin/auth/token?state=<uuid>                            → {"code":11217,"msg":"…login ing…"}
GET  /v2/plugin/login/account?state=<uuid>  (Bearer)               → uid / nickname
```

实测结论：**国际版与国内版 OAuth 流程同构，仅基址不同**（无 PKCE、无 client_id）。
因此本分支只是把基址参数化，未引入新的认证机制。

#### 4. 区域化的签到 / 成长中心 / 积分 / 刷新 / 网关

- 签到与成长中心：按账号区域选择 `billing_base()`
- 积分三件套与官方用量：按账号区域选择 `credits_base()`，并把凭证 `domain` 规范化为「区域权威 domain」
- token 刷新（3 处）：按账号区域选择 `refresh_url()`——国际版 refresh token 打到国内网关会被拒
- **备用域名（域名双探测）不跨区**：原实现对国际版返回国内 `codebuddy.cn` 作为备用域名，
  会把国际版 bearer token 发往国内网关——按本项目自身契约
  「令牌域与请求域不一致会被网关拒绝」**不可能成功**，且等于把凭证暴露给错误区域。
  现国际版的备用域名为**同区兄弟站** `www.codebuddy.ai`
- API 网关上游：`global_region` 改由账号区域推导（不再只看 `domain`）
- 上游健康探测：改为**按池内在用区域**探测（原先硬编码只探国内域名，纯国际版部署会得到错误的健康结论）

#### 5. 前端

- 账号管理页新增**区域选择器**（OAuth 扫码 / 扫描本机账号共用），并展示将要登录的站点
- 账号列表新增**区域徽标**（国内版 / 国际版）
- 扫描本机账号的预览弹框展示**推断出的区域**并提示如何纠正
- 扫描 / 导入命令支持显式指定区域（`region` 参数）

---

## 使用说明：录入一个 WorkBuddy 国际版账号

1. **准备**：本机安装国际版 WorkBuddy 客户端（<https://www.workbuddy.ai>），或准备好其 auth 文件。
2. 打开「Buddy → 账号管理」，把工具栏的**区域选择器切到「国际版」**。
3. 选择录入方式：
   - **OAuth 登录**：点击「OAuth登录」→ 系统浏览器打开 `www.workbuddy.ai` 登录页 →
     用 **Google / GitHub** 完成登录 → 回到应用，账号自动入池并标记为国际版。
   - **扫描本机账号**：若已在本机国际版客户端登录，点击「扫描本机账号」→
     预览弹框会显示识别到的区域 → 「确认入池」。
     > 若 auth 文件未携带 `domain` 导致区域推断为国内版，请先把区域选择器切到「国际版」再扫描/导入。
4. **切换登录态**、**签到**、**积分看板**、**API 网关**会按该账号的区域自动路由，无需额外配置。

### 数据与安全

- 区域字段仅记录 `"cn"` / `"global"`，不含任何凭证信息。
- 凭证仍按原设计存储：SQLite 中为占位符，明文经 Stronghold + DPAPI 加密（与 Trae 家族同一 vault）。
- 数据全部本地存储，不上传任何服务器。

---

## 功能一览（含原项目能力）

- **账号管理**：多账号录入 / OAuth 登录、分组、设备 ID 隔离、本机账号自动发现、auth 文件扫描入池
- **登录态切换**：按目标应用保存 / 恢复登录态并启动，支持「一键以账号打开」
- **一键签到**：批量签到、跳过已签/过期、实时进度；WorkBuddy 成长中心自动化
- **积分看板**：Trae / Buddy / Qoder 三平台统一看板，趋势图、排行、到期日历
- **本地代理**：MITM 代理捕获凭证并注入独立设备 ID，自动串联已有系统代理
- **API 网关**：内嵌 OpenAI / Anthropic / Codex Responses 三协议兼容服务，四池调度
- **定时任务**：Windows 计划任务 + 应用内调度器双轨
- **6 层设备标识重置**、**快照管理**、**暗色模式（6 套主题）**

> 完整功能说明见 [用户手册](docs/user-manual.md) 与 [技术架构](docs/tech-framework.md)。

---

## 下载与发布

Release 页面提供三类 Windows 产物（由 GitHub Actions 在 `windows-latest` 上以 MSVC 构建）：

| 文件 | 说明 |
| --- | --- |
| `*_x64-setup.exe` | NSIS 安装包（推荐，支持原地升级） |
| `*.msi` | MSI 安装包（企业分发） |
| `*_x64_portable.zip` | 便携版（解压即用） |

运行要求：Windows 10/11 x64、WebView2 Runtime（Win11 及较新 Win10 通常已内置）。

---

## 开发

```powershell
npm install
npm run tauri dev      # 开发模式
npm run tauri build    # 打包（msi + nsis）
node scripts/package_portable.mjs   # 便携版 zip
```

前置：Node.js 18+、Rust 1.85+、WebView2 Runtime、VS Build Tools (C++)

测试：

```powershell
cargo test              # Rust 单测（需 MSVC 工具链）
npm run test            # vitest 前端测试
npx tsc --noEmit        # 类型检查
```

---

## 本次改动的验证状态（如实说明）

本分支的改动经过以下**实际执行**的验证：

| 验证项 | 结果 |
| --- | --- |
| `cargo check --all-targets`（含测试代码，x86_64-pc-windows-gnu） | ✅ 通过，无 error / warning |
| `cargo test` + `npm run test`（GitHub Actions, MSVC） | ✅ 后端与前端全部通过（vitest 6 文件 / 59 用例） |
| `tsc` + `vite build` 前端构建 | ✅ 通过 |
| 区域逻辑断言（从真实源码机械提取后用 `rustc` 运行） | ✅ 45 条断言全通过（含「备用域名不跨区」） |
| 区域分类 vs **厂商自己的域名表** | ✅ 与 `product.json` 的 `internalDomain` / `externalDomain` 一致（已固化为单测） |
| 国际版端点连通性探测（未认证） | ✅ `auth/state` 返回 200 + authUrl；`billing/meter/*`、`v2/activity/growth/tasks`、`v2/chat/completions`、`plugin/auth/token/refresh` 均存在（401/400 = 需鉴权，非 404）；`www.codebuddy.ai` 同类端点同样存在 |

> **区域分类的权威依据**：国际版 CodeBuddy CLI 包内 `product.json` 明示
> `endpoint = https://www.codebuddy.ai`、`productFeatures.InternationalLogin = true`，
> 并在 `authentication.attributes` 中给出域名分组——
> 国内 `internalDomain`：`copilot.tencent.com`、`www.codebuddy.cn`、`www.workbuddy.cn` 等；
> 国际 `externalDomain`：`www.codebuddy.ai`。
> 本分支的区域判定与该表一致（把厂商域名表作为测试固化的依据，而非自行臆测）。

**未验证 / 已知限制**（请知悉）：

1. **未用真实的 WorkBuddy 国际版账号做过端到端实测**——开发环境没有国际版账号，
   因此「登录 → 签到 → 积分 → 网关调用」的完整链路**未经真实账号验证**。
   端点结构与国内版同构、域名分组均已实测/对齐厂商定义，但服务端行为仍可能随版本变化。
2. Windows 安装包由 **GitHub Actions 以 MSVC 工具链**构建（本机无 MSVC，仅有 mingw）。
   本地验证用的是 `x86_64-pc-windows-gnu` 目标，因此 `cargo test` 的**链接**步骤在本地无法执行
   （mingw 下 `libsodium` 的 `memset_explicit` 与 manifest 合并会失败），
   这是本地工具链限制，与代码无关；CI 使用 MSVC 不受影响（已实测通过）。
3. 国际版的**积分/计费响应结构与国内版是否逐字段一致未验证**——代码沿用原项目宽容解析逻辑，
   若国际版返回字段不同，可能需要后续适配。
4. **国内版**的备用域名仍沿用上游既有语义（CN → 国际镜像探测）。上游自身契约表明跨区必被拒，
   但该路径已被上游验证，本分支**有意不改动**以避免回归；如需同样收敛可后续单独评估。

---

## 免责声明

> 本工具与 WorkBuddy / Trae / CodeBuddy / 豆包 等官方产品**均无任何隶属、合作或关联关系**，系个人开源项目。

1. **非官方申明**：不代表任何官方立场。
2. **使用风险**：使用本工具可能违反相关产品的服务条款；由此产生的任何后果（包括但不限于账号封禁、
   积分清零/扣除、功能限制、数据异常等）均由使用者自行承担。
3. **责任范围**：不对因使用（或无法使用）本工具所导致的任何直接、间接、附带或后果性损失负责。
4. **合规义务**：使用前请务必仔细阅读相关服务条款并自行判断；请确保**仅用于管理本人合法持有的账号**，
   遵守所在地法律法规。
5. **侵权处理**：若您认为本工具侵犯了您的合法权益，请通过项目渠道联系，我们将在核实后及时下架处理。

**使用本工具即表示你已阅读、理解并同意上述全部免责声明。**

---

## 文档

- [更新日志](CHANGELOG.md) — 各版本变更记录（含原项目历史）
- [用户手册](docs/user-manual.md) — 功能说明与使用指南
- [产品设计](docs/product-design.md) — 需求与产品设计基线
- [待办清单](docs/backlog.md) — 全项目唯一待办依据
- [技术架构设计](docs/tech-framework.md) — 架构 / 数据模型 / 协议参考（含 WorkBuddy §5.2 区域表）

---

## License 与致谢

本项目采用 [MIT License](LICENSE)，**版权归原作者 朱天伟（Copyright © 2026 朱天伟）所有**。

- 原项目：[smart-open/TraeWorkAssistant](https://github.com/smart-open/TraeWorkAssistant)
- 本分支仅在此基础上增加 WorkBuddy 国际版支持，**未修改 LICENSE 与版权声明**。
- 引用或借鉴请注明原作者及原始仓库；派生项目须说明以原库为基础。

感谢原作者的出色工作 ❤️
