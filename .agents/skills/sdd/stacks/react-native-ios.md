---
status: active
version: v2  # 2026-08-30
---

# 栈适配层：React Native + iOS

> **适配对象**：[`../references/frontend-conventions.md`](../references/frontend-conventions.md)（全册——RN 壳层下的形态替换）+ [`../references/SPECIFICATION.md` §13.1](../references/SPECIFICATION.md#131-测试分层)（测试分层落地形态：jest 单测 + 模拟器冒烟 + 产物级实证）
> **规范语言**：BCP 14（RFC 2119/8174）
> **本层职责**：RN / Hermes / Metro / Xcode 构建链的生态细则。**MUST NOT 写项目路径、包名、设备名**（属项目 overlay）；**MUST NOT 复制 references 条款正文**（违反 SSOT，改为链接）

## 0. Agent 执行协议

1. **Trigger**：项目含 React Native 端（iOS 壳），且命中 frontend-conventions 或 [SPECIFICATION §13.1](../references/SPECIFICATION.md#131-测试分层) 时，MUST 一并加载本文。
2. **Load**：只读命中节；MUST NOT 预读全文。
3. **Apply**：职责边界与硬要求以 `references/` 为准，本文只覆盖 RN / iOS 构建链的具体形态；路径、版本钉版、模拟器设备与磁盘纪律以项目 overlay 为准。
4. **Conflict / Stop**：本文与 `references/` 原则冲突时，MUST 以 `references/` 为准并报告本文需修订；项目实现与本文冲突时，MUST 以实现为事实并停止报告。
5. **Output**：交付说明 MUST 点名依据的适配节号与跑过的门禁（tsc / jest / lint / format:check / xcodebuild / pod install）。
6. **MUST NOT**：MUST NOT 把本文条款当作 Web 或其它原生端的依据；MUST NOT 混入非目标架构切片的构建产物。

---

## 1. 本栈采纳的结论

对应 [frontend-conventions §1](../references/frontend-conventions.md#1-采纳的结论)。下表只记**本栈做出的选择**：

| 主题 | 本栈采纳 |
| --- | --- |
| 形态 | 单 RN 代码库出 iOS 壳；Metro bundler + Hermes 引擎 |
| 原生内核 | Swift / FFI 侧实现，RN 经 `.m` 桥（`RCT_EXTERN_METHOD`）导出调用 |
| 构建架构 | Apple Silicon 宿主 **arm64 单架构**：xcodebuild 显式 `-destination 'generic/platform=iOS Simulator' ARCHS=arm64 ONLY_ACTIVE_ARCH=YES` + `EXCLUDED_ARCHS=x86_64`；依赖产物（XCFramework 等）可再生、MUST NOT 入库，拉取同口径只取 arm64 变体 |
| 测试 | jest 单测（Hermes 语义）+ 模拟器冒烟；模拟器不可用时降级**产物级实证**（对构建产物断言，MUST NOT 宣称运行时验证） |
| lint / format | oxc 系（oxlint / oxfmt）替换模板默认 ESLint + Prettier；配置单源复用仓根 |

### 1.1 官方文档

- React Native：[官方文档](https://reactnative.dev/docs/getting-started)
- Hermes：[引擎文档](https://hermesengine.dev/)
- `react-native-get-random-values`（随机 polyfill 事实标准，入口文件副作用 import）：[GitHub](https://github.com/LinusU/react-native-get-random-values)
- Xcode build settings：[Xcode Build Setting Reference](https://developer.apple.com/documentation/xcode/build-settings-reference)

---

## 2. Hermes 运行时约束

- Hermes 可能缺 `globalThis.crypto.getRandomValues`（MUST NOT 依赖「较新 RN 内置 Web Crypto」的认知）——随机依赖 MUST 显式注入 polyfill（如 `react-native-get-random-values`），且入口文件 MUST 先于业务 import。
- 纯逻辑库的随机 fallback MUST NOT 静默确定性降级——确定性字节兜底会输出可复现的「随机」值，属安全缺陷；且 node / jest 环境有 crypto、单测测不出，随机族 MUST 以真机 / 模拟器实跑冒烟覆盖。

---

## 3. Monorepo 装配形态

- **双 React 崩溃**：共享包裸 `import react` 向上解析命中另一份 react（如仓根 node_modules）→ 运行时 `Cannot read property 'useState' of null`（官方口径：单 app 内 React 多版本必然运行时报错）。app 的 Metro resolver MUST 把 react 系列重定向到 app 自身 node_modules，且全仓 react 版本 MUST 单一钉版——可经根 package.json `resolutions` / `overrides` 强制。
- Metro 对 pnpm 默认软链隔离结构支持不全（pnpm 官方将 React Native 列为需 hoisted `node_modules` 的典型场景）。两种已验证形态**任选其一**并在项目 overlay 登记，MUST NOT 未实证混搭：① RN app 独立于 workspace、用自带锁版的包管理器安装（npm 等），workspace 显式排除；② 全仓 pnpm 切 hoisted 安装策略（`.npmrc` `node-linker=hoisted` 或 `pnpm-workspace.yaml` `nodeLinker: hoisted`）。

### 3.1 lint / format 工具链形态

- RN 官方模板默认携带 ESLint（`.eslintrc.js` extends `@react-native`）+ Prettier（`.prettierrc.js`）局部配置。替换为 oxc 系（oxlint / oxfmt）时两者**整删**，MUST NOT 另放局部 `.oxlintrc.json` / `.oxfmtrc.json`——两工具从子目录向上查找配置，monorepo 下自动复用仓根单源，局部第二份配置必与仓根漂移。
- RN app 的 devDependencies MUST 自带 oxlint / oxfmt，版本与仓根对齐（独立安装形态下仓根二进制不可达；版本漂移则两处格式化输出可能分叉）。
- 工具链迁移批 MUST 保持零格式 diff：切换前先 `oxfmt --check` 实证目标源码全绿（模板 prettier 风格与 oxfmt 通常可零 diff 达成），MUST NOT 让迁移混入海量格式重排。
- 存量 `eslint-disable` 注释族零 diff 保留——oxlint 识别该注释族（含 `react-hooks/` 等 eslint 惯例前缀别名）：命中已启用规则时抑制**实际生效**（如 `react/exhaustive-deps` 默认启用），未启用规则名静默无害。删除这类注释会立刻暴露被抑制的告警，MUST NOT 当死注释清理。
- scripts：`lint` = `oxlint .`，补齐 `format` = `oxfmt --write .` / `format:check` = `oxfmt --check .`（模板无 format script）；monorepo 门禁链路 MUST 同批切换——门禁 script 内联的 `eslint` 调用（如 `exec eslint <paths>`）是残留高发点，依赖移除后门禁必失败。
- 规则集语义：oxlint 启用集与 `@react-native/eslint-config` 不等价（如 RN 特有规则 `react-native/no-inline-styles` 无对应，hooks 族规则反而默认启用）——迁移后规则面以仓根 oxlint 配置为准，lint 覆盖变宽变窄都属预期而非回归。

---

## 4. 原生桥一致性

- `.m` 桥导出面是手工维护的平行清单：tsc 不查 `.m`、构建门禁只验编译——导出面漂移的失败形态 = 端内 `TypeError: undefined is not a function`（**构建全绿**）。
- 新增 / 变更内核方法 MUST 同步桥导出；桥面与协议面（IDL / manifest）的一致性 MUST 有机械校验（生成器或对拍），MUST NOT 依赖手工同步。
- 壳层 MUST 为页面级内容装配 ErrorBoundary（resetKey 随路由切换重置）——单页崩溃 MUST NOT 白屏整 app。

---

## 5. 测试通道形态（对应 [SPECIFICATION §13.1](../references/SPECIFICATION.md#131-测试分层)）

- iOS 模拟器程序化文本注入（idb / HID）受输入法态（自动大写 / 自动纠正）污染——注入前 MUST 清理模拟器输入法；判读以截图 / dump 为准。
- 同机多模拟器栈（iOS CoreSimulator 与 HarmonyOS Emulator 等）MUST 串行联调（磁盘约束）；启停与缓存清理命令由项目 overlay 登记。

---

## 6. 换栈映射判据

换掉本栈时，被适配条款中**哪些要改、哪些不能改**：

| 条款 | 性质 | 换栈时 |
| --- | --- | --- |
| 随机源缺失 MUST 显式失败或显式注入，MUST NOT 静默确定性降级 | 硬要求 | **不变** |
| 桥面与协议面一致性 MUST 有机械校验 | 硬要求 | **不变** |
| 页面级 ErrorBoundary（崩溃不殃及整 app） | 硬要求 | **不变** |
| Metro / Hermes / `.m` RCT 桥 / arm64 构建参数 / xcodebuild + CocoaPods | 形态 | **替换**为目标构建链与运行时 |
| oxc 系 lint / format 工具链与配置单源复用 | 形态 | **替换**为目标语言 / 构建链的 lint / format 工具 |
