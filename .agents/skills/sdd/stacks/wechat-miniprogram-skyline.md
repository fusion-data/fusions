---
status: active
version: v5  # 2026-10-10 增补 §10 WXML 编译域（表达式禁模板字符串 / 单文件 Fatal 毒化整包 / data-* 属性名禁大写 / 静态护栏形态）与 §11 工具链会话（模拟器 Origin=servicewechat.com 与后端 CSRF 白名单交互 / automator-skyline 元素查询受限 / scroll-view 滚轮待复核）；v4 2026-10-09 增补 §6 守卫落点纪律（遮罩与出口按钮共享带守卫 handler → 按钮文案区死钮，收货地址弹窗实证）；v3 2026-10-09 更正 §7 getTabBar 形态（v2 误诊「异步回调」→ 官方仅同步签名：成员调用保 this + 判空，开发者工具 lib 3.17.2 崩溃实证）；v2 2026-10-09 增补 §5 组件样式隔离 / content-box 盒模型 / flex 列文本换行（create 页实证）；v1 2026-10-09 首版（baoming 小程序 skyline 实证回流：四件套 / 渲染与样式差异 / tap 形态 / 自定义 tabBar / 零裸模块）
---

# 栈适配层：微信小程序 Skyline + glass-easel

> **适配对象**：[`../references/frontend-conventions.md`](../references/frontend-conventions.md)（全册——skyline 分栈下的形态替换）+ [`../references/SPECIFICATION.md` §13.1](../references/SPECIFICATION.md#131-测试分层)（测试通道落地形态）
> **规范语言**：BCP 14（RFC 2119/8174）
> **本层职责**：微信小程序原生开发（无框架 TypeScript + skyline 渲染 + glass-easel 组件框架 + 开发者工具内置编译插件）的生态细则。**MUST NOT 写项目路径、包名、appid、密钥名**（属项目 overlay）；**MUST NOT 复制 references 条款正文**（违反 SSOT，改为链接）

## 0. Agent 执行协议

1. **Trigger**：项目含微信小程序端（skyline 渲染 + 无框架 TypeScript），且命中 frontend-conventions 或 [SPECIFICATION §13.1](../references/SPECIFICATION.md#131-测试分层) 时，MUST 一并加载本文。
2. **Load**：只读命中节；MUST NOT 预读全文。
3. **Apply**：职责边界与硬要求以 `references/` 为准，本文只覆盖 skyline / glass-easel / 开发者工具链的具体形态；路径、命令、appid、预览密钥通道以项目 overlay 为准。
4. **Conflict / Stop**：本文与 `references/` 原则冲突时，MUST 以 `references/` 为准并报告本文需修订；真机行为与本文冲突时，MUST 以真机为事实并停止报告。skyline 处于演进期，本文带日期的实证结论跨基础库 / 工具版本 MUST 复核后再依赖。
5. **Output**：交付说明 MUST 点名依据的适配节号与跑过的门禁（tsc type-check / 真机预览）。
6. **MUST NOT**：MUST NOT 把本文条款当作 webview 渲染模式或第三方框架（Taro / uni-app）小程序的依据。

---

## 1. 本栈采纳的结论

对应 [frontend-conventions §1](../references/frontend-conventions.md#1-采纳的结论)。下表只记**本栈做出的选择**：

| 主题 | 本栈采纳 |
| --- | --- |
| 渲染与组件框架 | skyline + glass-easel，app.json 四件套**全局声明**：`renderer: "skyline"` + `componentFramework: "glass-easel"` + `lazyCodeLoading: "requiredComponents"` + `rendererOptions.skyline`；页面 json MUST NOT 重复声明，继承全局 |
| 语言与编译 | TypeScript 由开发者工具内置编译插件**逐文件转译，无构建链**（无打包器、无模块解析） |
| worklet | 默认关闭，动效只走 CSS（本行为默认档；项目启用 worklet 属项目裁决，MUST 在项目 overlay 登记） |
| 原生导航栏 | 不可用（skyline 不支持）——`window.navigationStyle: custom` + 每页自绘导航组件（含状态栏高度占位） |

### 1.1 官方文档

- app.json 全局配置（renderer / componentFramework / lazyCodeLoading / rendererOptions）：[微信开放文档 · 全局配置](https://developers.weixin.qq.com/miniprogram/dev/reference/configuration/app.html)
- 自定义 tabBar（skyline 适配要求：根组件 `pointer-events: auto` 与定位自声明）：[微信开放文档 · 自定义 tabBar](https://developers.weixin.qq.com/miniprogram/dev/framework/ability/custom-tabbar.html)
- skyline 宿主容器根节点默认 `pointer-events: none` 的机制说明：[微信开放文档 · 全局工具栏 app-bar](https://developers.weixin.qq.com/miniprogram/dev/framework/runtime/skyline/appbar.html)

---

## 2. 测试通道形态（对应 [SPECIFICATION §13.1](../references/SPECIFICATION.md#131-测试分层)）

- 无构建链 → 静态通道 = `tsc --noEmit`（逐文件转译不做模块级分析）。
- 真机通道 = miniprogram-ci preview 出预览二维码（需正式 appid + 上传私钥，均属项目 overlay）；真机 UAT 走该二维码人工签收。
- **开发者工具模拟器与真机存在行为差**：命中测试 / 事件派发类缺陷（点击穿透、事件不触发）在模拟器上 MAY 不复现——判定 MUST 以真机为准，模拟器通过不构成证据（2026-10-09 实证：自定义 tabBar 点击穿透，模拟器正常、真机整条栏死钮）。

---

## 3. 渲染层形态（对应 [frontend-conventions §5](../references/frontend-conventions.md#5-渲染层约束)）

- 页面级滚动 MUST 用 scroll-view 承载局部滚动，MUST NOT 依赖 webview 式全局滚动。
- `scroll-view`（type=list）的子节点拉伸需父级 flex + 子级 `align-self` 显式声明，否则拉伸不生效。
- 动效只做 `transform` / `opacity`（skyline 光栅化约束；worklet 默认关闭，见 §1）。
- `aspect-ratio` 高度解析异常 → 定比容器 MUST 用显式高度。

---

## 4. 远程数据与模块接入形态（对应 [frontend-conventions §6](../references/frontend-conventions.md#6-远程数据约定)）

无构建链的模块纪律（开发者工具编译插件不做模块打包与裸模块解析）：

- 源码 import MUST 全相对路径。
- npm 依赖（含 monorepo 内共享包的生成码）MUST 经「构建 npm」（miniprogram_npm）通道或端内手写镜像模块收口，MUST NOT 源码直接裸 import——两通道的选择由项目 overlay 登记。
- 端内手写消费 Connect-JSON wire 时：字段大小写与省略语义 MUST 对齐 proto JSON 映射（空 repeated 字段序列化侧省略——列表访问 MUST 兜底空值），并以实跑 fixture 校验。

---

## 5. 样式形态（对应 [frontend-conventions §7](../references/frontend-conventions.md#7-样式约定)）

skyline WXSS 与 web CSS 的差异：

- MUST NOT 用 `clip-path`、伪元素（`::before` / `::after`）——skyline 不支持。
- CSS 变量 MUST 定义在 `page` 选择器。
- **z-index 只在同层级节点间有效**（无 Web 标准层叠上下文）——浮层梯子（遮罩 / 弹层 / 固定底栏 / tabBar）MUST 设计为同级节点以 z-index 分层，跨层级嵌套的 z-index 声明无效。
- **组件样式隔离**：glass-easel 默认 `styleIsolation: isolated`——app.wxss / 页面 wxss 的类选择器不作用于自定义组件内部节点，「组件 wxss 留空复用全局类」不成立，组件 MUST 自带其 wxml 用到的全部类（2026-10-09 实证：弹层组件裸文本沉底、无遮罩无卡片；开发者工具模拟器通道。样式匹配属框架确定性语义，非 §2 命中测试类真机差，但按本节惯例版本升级时复核）。CSS 自定义属性按树继承、不受隔离影响，组件内可消费 `page` 选择器定义的变量。
- **盒模型默认 content-box**：`rendererOptions.skyline.defaultContentBox: true`（webview 对齐推荐档）下 `width` 百分比与 `padding` 并用的节点 MUST 显式 `box-sizing: border-box`——否则 `2×(50%−gap/2)+2×padding` 超宽，`flex-wrap` 网格逐卡换行退化为单列（2026-10-09 实证：模板卡网格，模拟器通道）。
- **flex 列内文本不回绕**：`align-items: flex-start` 的 flex 列中 `<text>` 按 fit-content（最长行）排开、不换行、溢出容器——文本子节点 SHOULD 交给默认 stretch 拉满换行，需收窄的节点单独 `align-self: flex-start`（2026-10-09 实证：卡片描述溢出卡外，模拟器通道）。
- 固定底栏（含 tabBar）MUST `calc(… + env(safe-area-inset-bottom))` 让位全面屏安全区，滚动容器以 `padding-bottom` 相应让位。

---

## 6. tap 与操作出口形态（对应 [frontend-conventions §9](../references/frontend-conventions.md#9-操作出口与异步反馈跨端通用)）

skyline + glass-easel 工具链的事件派发形态（[frontend-conventions §9.2](../references/frontend-conventions.md#92-动作反馈死按钮禁止) 死按钮原则的落地）：

- tap 绑定一律 `bind:tap`；容器关闭 / 含内层动作的行 handler MUST 加冒泡守卫（`e.target !== e.currentTarget` 即 return），守卫 MUST 只落在容器 / 遮罩自己的 handler 上——动作出口按钮（叶子，子节点仅文案 text / 图标）MUST NOT 与遮罩共享带守卫的 handler；导航类 handler 加 ~400ms 节流防双触发重复入栈。
- **实证登记（待复核）**：2026-10-09 遮罩与「取消」按钮共享同一带守卫 handler → 点在按钮文案 text 上 `target` 为该子节点（glass-easel 事件对象语义，源组件 = 命中的最内节点），守卫误判为内层动作即 return，按钮标签区零反馈死钮（收货地址弹窗真机报告；JS 事件对象语义属确定性类，非 §2 命中测试类真机差）——修复与形态 = 遮罩专用带守卫 handler + 按钮独立无守卫 handler（全仓同类 10 处已按此收敛）。
- **实证登记（待复核）**：2026-10-09 工具链下组件内 `catch:tap` 不触发（自绘导航栏返回键真点死钮，改 `bind:tap` 即活；同日两例 tabBar 死钮曾误判为此，后改判 pointer-events 穿透，见 §7）——单例实证，基础库 / 工具升级时 MUST 复核，MAY 随版本修复转正或作废。

---

## 7. 自定义 tabBar 形态（对应 [frontend-conventions §10](../references/frontend-conventions.md#10-控件组件层与跨端清单口径跨端通用)）

app.json `tabBar.custom: true` + 根目录 `custom-tab-bar/index` 组件（目录名框架固定，不可改）在 skyline 下的形态：

- **根节点 MUST `pointer-events: auto`**：宿主容器默认 `pointer-events: none` 且被子节点继承——缺省则整条栏渲染正常但命中测试全穿透（真点死钮；官方自定义 tabBar 文档 skyline 适配要求原文，2026-10-09 真机实证）。
- **定位 MUST 自声明**：skyline 下框架不再代为固定底部（webview 才有框架代办的 fixed 包装容器），缺省渲染进页面流顶部；`fixed` / `absolute` 均可，安全区让位见 §5。
- **`getTabBar` 官方仅有同步签名**（文档与 typings 一致，未区分渲染引擎）：返回当前页 tabBar 组件实例，未就绪 / 非 tab 页为 `undefined`——tab 页 onShow 高亮同步 MUST 成员调用 + 判空（`this.getTabBar()?.setData(...)`）。**MUST NOT 拆成裸函数调用**：`this` 丢失即被基础库实例守卫拒绝，抛 `Method should be called on a valid component instance`（2026-10-09 实证，开发者工具模拟器通道 lib 3.17.2；JS 调用语义属确定性类，非 §2 命中测试类真机差）。v2 曾登记「异步回调形式」，系误诊（回调形态官方从未提供），v3 作废更正。
- 中央凸起钮（FAB）SHOULD 把 tap 目标绑在整段插槽而非仅视觉圆钮（扩大命中面）；负 margin 上移只改视觉，不改变命中域。

---

## 8. 依赖开关与生态细则

| 能力 | 依赖 / 开关 | 缺失时的失败形态 |
| --- | --- | --- |
| skyline 四件套 | app.json：renderer + componentFramework + lazyCodeLoading + rendererOptions.skyline | 真机白屏 / 动效失效 / 组件行为回退 webview 语义 |
| 自定义 tabBar | `tabBar.custom: true` + `custom-tab-bar/index` 组件四件 | skyline 下整条栏点击穿透（§7 pointer-events 缺省形态） |
| TypeScript 编译 | 开发者工具 `useCompilerPlugins` 含 `typescript` | `.ts` 不转译，直接报语法错 |
| 真机预览通道 | miniprogram-ci + 正式 appid + 上传私钥 | 无法出预览二维码，真机 UAT 通道不可用 |

---

## 9. 换栈映射判据

换掉本栈时，被适配条款中**哪些要改、哪些不能改**：

| 条款 | 性质 | 换栈时 |
| --- | --- | --- |
| 死按钮禁止 / 动作出口完备（frontend-conventions §9 原则） | 硬要求 | **不变** |
| 事件与命中测试差异以真机为事实，模拟器通过不构成证据 | 硬要求 | **不变** |
| 手写消费 wire MUST 对齐序列化 casing 与省略语义并以 fixture 校验 | 硬要求 | **不变** |
| `bind:tap` + 冒泡守卫 + 导航节流 / tabBar 根节点 `pointer-events: auto` / 定位自声明 / `getTabBar` 同步成员调用+判空 | 形态 | **替换**为目标平台事件派发与容器机制 |
| 四件套 / scroll-view 局部滚动 / 禁 clip-path·伪元素 / `page` 选择器变量 / z-index 同层约束 / safe-area 让位 | 形态 | **替换**为目标渲染引擎等价物 |
| 相对路径 import / miniprogram_npm 或手写镜像收口 | 形态 | **替换**为构建链下的正常模块解析（引入构建链时本组自然失效） |
| `tsc --noEmit` + miniprogram-ci preview 测试通道 | 形态 | **替换**为目标平台静态检查与真机通道 |

---

## 10. WXML 编译域与表达式语法

WXML 模板语法域（开发者工具整包编译；2026-10-10 实证，模拟器通道）：

- `{{}}` 插值表达式只支持简单 JS 表达式（三元 / 逻辑 / 算术 / 字符串拼接）。**模板字符串（反引号 + `${}`）MUST NOT 用于 WXML 表达式**——编译期 Fatal `unexpected character inside expression`；带计数插值用拼接（`'已上传 ' + n + ' 份'`）。
- **单文件编译 Fatal 毒化整包**：全部页面 wxml 编译为一个 bundle，任一文件 Fatal → 所有页面零渲染（当前页黑屏且完全无响应），报错文件与受害页面可以无关——排障 MUST 先看 Console 编译诊断，MUST NOT 只盯当前页面。编译 Note（如 `avoid uppercase letters`）不阻塞渲染（同场实证）。
- **`data-*` 属性名 MUST 全小写**：dataset 键由连字符转驼峰（`data-v-idx` → `dataset.vIdx`），属性名中的大写字母被运行时静默转小写 → TS 侧按驼峰读取必然键错位，并触发编译 Note。
- 静态护栏形态（本栈无构建链，tsc 不解析 wxml，此类语法错无静态通道）：对全部 `.wxml` grep 两模式——反引号、`data-` 属性名含大写字母——命中即 fail；命令与挂载点属项目 overlay。

---

## 11. 开发者工具会话与自动化通道

工具链会话形态（对应 [SPECIFICATION §13.1](../references/SPECIFICATION.md#131-测试分层) 测试通道的落地面；2026-10-10 实证）：

- **模拟器 wx.request MAY 携带 `Origin: https://servicewechat.com`**（微信运行时固有 origin；与会话形态相关——经 CLI 重建的项目窗口观察到，真机 wx.request 无浏览器 Origin 语义）。本地 / 同栈后端如有 CSRF Origin 校验或 CORS 显式白名单，MUST 评估放行该 origin，否则全部非 GET RPC 被 403（实证：后端日志 `csrf guard rejected` + 同请求带 / 不带该 origin 403 / 200 对照复现）。白名单取值登记属项目 overlay。
- **miniprogram-automator 在 skyline 下元素查询受限**：仅页面顶层节点（scroll-view 之外）可查；scroll-view 内部与自定义组件内部节点不可见（实证 + 行业 skyline 适配记录一致）。可靠驱动面 = 导航（switchTab / navigateTo / redirect）+ `page.data()` 断言 + `page.callMethod` + 顶层元素；scroll-view 内交互 MUST 用像素点按或 callMethod 等价 handler 驱动，UI 弹层呈现另行截图 / 人工走查。
- **实证登记（待复核）**：模拟器内 skyline scroll-view 对滚轮事件 MAY 不生效（多次滚轮零位移、拖拽手势可滚动且见回弹）——自动化滚动 MUST 验证像素确实变化后再认定到位。开发者工具 / 基础库升级时 MUST 复核，MAY 随版本修复转正或作废。
