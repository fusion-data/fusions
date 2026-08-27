---
status: active
version: v1  # 2026-08-27
---

# 栈适配层：HarmonyOS + ArkTS

> **适配对象**：[`../references/frontend-conventions.md`](../references/frontend-conventions.md)（全册——ArkTS 分栈下的形态替换）+ [`../references/SPECIFICATION.md` §13.1](../references/SPECIFICATION.md#131-测试分层)（E2E / UI 测试层落地形态）
> **规范语言**：BCP 14（RFC 2119/8174）
> **本层职责**：ArkTS / hvigor / napi / DevEco Testing Hypium 的生态细则。**MUST NOT 写项目路径、包名、设备名、模拟器形态**（属项目 overlay）；**MUST NOT 复制 references 条款正文**（违反 SSOT，改为链接）

## 0. Agent 执行协议

1. **Trigger**：项目含 HarmonyOS ArkTS 端，且命中 frontend-conventions 或 [SPECIFICATION §13.1](../references/SPECIFICATION.md#131-测试分层) 时，MUST 一并加载本文。
2. **Load**：只读命中节；MUST NOT 预读全文。
3. **Apply**：职责边界与硬要求以 `references/` 为准，本文只覆盖 ArkTS / Hypium 的具体形态；设备形态、模拟器启停、磁盘门禁、路径与命令以项目 overlay 为准。
4. **Conflict / Stop**：本文与 `references/` 原则冲突时，MUST 以 `references/` 为准并报告本文需修订；项目实现与本文冲突时，MUST 以实现为事实并停止报告。
5. **Output**：交付说明 MUST 点名依据的适配节号与跑过的门禁（hvigor 构建等）。
6. **MUST NOT**：MUST NOT 把本文条款当作其它端（Web / React / RN）的依据；MUST NOT 在 Hypium 生态（arkxtest + DevEco Testing Hypium）之外另引 UI 自动化栈。

---

## 1. 本栈采纳的结论

对应 [frontend-conventions §1](../references/frontend-conventions.md#1-采纳的结论)。下表只记**本栈做出的选择**：

| 主题 | 本栈采纳 |
| --- | --- |
| UI 分栈 | ArkTS 为独立 UI 分栈，MUST NOT 假设与 Web / React 端共享 UI 代码；跨端复用 MUST 落在纯逻辑包与契约生成物（类型 / 常量 / manifest 单源派生），UI 层按端重写 |
| 构建 | hvigor（`hvigorw assembleHap`）；构建产物与依赖目录可再生、MUST NOT 入库 |
| 原生桥 | Rust 内核经 napi `.so` 通道；导出面由编译期 TS 模块解析保证——import 面与 `.so` 导出面不一致则无法构建 / 启动（桥面漂移在编译期拦截，对照 iOS 手工桥的运行时漂移形态） |
| 测试栈 | 两层分立：端内测试（单元 + UI 组件）= **arkxtest**（JsUnit + UiTest，ohpm 包 `@ohos/hypium`）；宿主驱动 UI 自动化 / 性能 = **DevEco Testing Hypium**（Python，xdevice driver + `UiDriver`）。两层职责与纪律见 §2 |
| 设备驱动 | hdc（模拟器 / 真机同通道）；设备清单与模拟器环境由项目 overlay 登记 |

### 1.1 官方文档

- DevEco Testing Hypium（应用 UI 测试·基于 Python）：[Hypium Python 指南](https://developer.huawei.com/consumer/cn/doc/harmonyos-guides/hypium-python-guidelines-V5)
- arkxtest（自动化测试框架 = JsUnit + UiTest）：[arkXtest User Guide](https://developer.huawei.com/consumer/en/doc/harmonyos-guides-V5/arkxtest-guidelines-V5)
- UiTest API（控件查找 / 模拟输入 / 断言）：[UITest User Guide](https://developer.huawei.com/consumer/en/doc/harmonyOS-guides/uitest-guidelines)

---

## 2. 测试通道形态（对应 [SPECIFICATION §13.1](../references/SPECIFICATION.md#131-测试分层)）

两层通道分立，MUST NOT 用一者冒充另一者的覆盖面（端内 Instrument Test ≠ E2E）：

- **端内测试（arkxtest）**：JsUnit（单元）+ UiTest（UI 组件查找 / 操作 / 断言），ohpm 包 `@ohos/hypium`；Instrument Test 跑在设备 / 模拟器，Local Test 不上设备。
- **宿主驱动 UI 自动化 / 性能（DevEco Testing Hypium，Python）**：用例 = `TestCase`（setup / process / teardown）+ 用例配置 JSON（声明 xdevice driver 类型与用例文件）；框架自动生成执行报告（步骤截图 + 设备日志）。

定位与断言纪律（宿主驱动通道）：

- 控件定位官方优先序 = **控件属性定位（`BY.text` / `BY.id` 等）＞ 图像匹配 ＞ 比例坐标**；MUST NOT 以截图估位 / 比例坐标为默认定位方式（文案锚取自文案真相源）。
- **程序化文本注入不触发 ArkUI onChange**：注入只作视觉输入，state 可能与显示脱节——断言 MUST 以控件属性 / 布局树为准，MUST NOT 以注入后的视觉文本判定 state。
- ArkUI 渲染坑：Builder 调用裸块紧跟 if-else 复合语句存在整块零渲染实例（无异常日志）——复杂结果区优先已实证形态（内联表达式 + 方法内判空），交付前 MUST 以布局 dump 验证输出节点存在。

---

## 3. 远程数据接入形态（对应 [frontend-conventions §6](../references/frontend-conventions.md#6-远程数据约定)）

- 手写 HTTP 消费 Connect JSON wire 的坑：proto 字段无显式 `json_name` 时**序列化侧只输出 lowerCamelCase**（官方 JSON 映射；解析侧 camelCase 与原始字段名皆收）——手写解析按 snake_case 读序列化输出必得 undefined，且 ArkTS `as` 断言不抛异常，静默降级不触发兜底分支。消费 Connect JSON MUST 优先生成客户端；手写解析 MUST 对齐 wire casing 并以实跑 fixture 校验（[Proto3 JSON 映射](https://protobuf.dev/programming-guides/proto3/#json)）。

---

## 4. 依赖开关与生态细则

| 能力 | 依赖 / 开关 | 缺失时的失败形态 |
| --- | --- | --- |
| napi `.so` 内核桥 | 支持显式 `NapiModule` 注册的 napi 生成器（上游缺失时换支持 ohos 的通道） | hilog `Name mismatch`，模块加载失败 |
| 多语言资源分目录 | 跨目录键集一致性检查随构建门禁 | 构建期检出（漂移键不入产物） |

---

## 5. 换栈映射判据

换掉本栈时，被适配条款中**哪些要改、哪些不能改**：

| 条款 | 性质 | 换栈时 |
| --- | --- | --- |
| UI 分栈、跨端共享只走纯逻辑层与契约生成物 | 硬要求 | **不变**（平台矩阵本身属项目决策） |
| E2E 断言以布局 dump / 属性断言为准，显式定位锚 | 硬要求 | **不变** |
| 手写 JSON 消费 MUST 对齐 wire casing 并以 fixture 校验 | 硬要求 | **不变** |
| ArkTS / hvigor / napi `.so` / arkxtest / DevEco Testing Hypium（Python）/ `BY.*` 定位 API | 形态 | **替换**为目标平台等价物（测试框架、定位 API、构建链） |
