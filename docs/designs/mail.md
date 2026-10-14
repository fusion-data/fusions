# fusion-mail 语义真相源

> `fusion-mail` 是邮件发送机制层：SMTP（rustls + 连接池）/ 文件 / 记录 / 丢弃传输
> over lettre。本文收发送抽象形态、feature 面、错误分级与 re-export 纪律的框架级
> 语义；邮件文案 / 模板、场景方法（如验证邮件）、重试与告警策略、DKIM 密钥装配、
> 配置来源与装配分流归消费仓（framework-conventions §7）。

## 1. 发送抽象（双形态 trait）

- `MailSender`（同步）+ `AsyncMailSender`（tokio1，`#[async_trait]`）：窄接口
  深实现——实现隐藏传输差异，消费方只见 `send`。
- 异步签名 `send(&self, message: Message)` 按值传递（上游异步 transport 所有权
  形态，零克隆）；同步签名 `send(&self, message: &Message)` 借用。
- 消息构造直接用 lettre `Message` builder（多部分正文 / 附件 / 内嵌图 / 自定义头
  全继承），crate **不**自造 `MailMessage` 包装（每项能力都要跟随扩字段 = 参数爆炸
  演化路径）。

## 2. feature 面

| feature | 内容 | 备注 |
| --- | --- | --- |
| `file`（default） | `FileSender`：MIME 全文落盘 `.eml`，`new` 自建目录 | 本地走查 / 调试 |
| `tokio1`（default） | `AsyncMailSender` trait + 异步实现 | lettre `tokio1` + async-trait |
| `smtp` | `SmtpSender` / `AsyncSmtpSender`：rustls TLS + 凭证 | **透传集 MUST 显式含 `lettre/pool`**（pool 是 lettre default feature，workspace 模板 `default-features = false` 会将其关闭；漏补 = 每次发送重建 TCP+TLS 连接） |
| `dkim` | lettre 内建 DKIM 签名原语 | 密钥装配与签名域归消费方 |
| —（恒可用） | `RecordingSender` / `StubSender` | 零外联依赖的最小测试面 |

- lettre 走 workspace 模板（`builder` + `hostname` 基础集），增量 feature 由本
  crate 矩阵透传；与 fusion-storage opendal 直声明的差异 = lettre 基础集对全部
  组合恒需要（`Message` 是 trait 签名一部分），模板最小化后透传不失效。

## 3. 错误分级（framework-conventions §1 投射）

`MailError` 三分，MUST NOT 整类压成单一变体，源错误链一律保留（`#[source]` 装箱）：

1. `Transient`——上游 SMTP 瞬态（4xx / 网络），`is_transient() == true`，可重试；
2. `Permanent`——上游永久拒绝（5xx：认证失败 / 拒收 / 域名无效），重试无意义；
3. `Transport` / `Config`——其他传输错误 / 本地装配缺陷（feature 未启用、TLS
   参数构造失败）。

消费方重试策略按分级自取；本 crate 不内建重试（归消费方，YAGNI）。

## 4. re-export 纪律（版本偏斜防护）

- `fusion_mail::message`（模块）+ `Message` / `Mailbox` 由 crate re-export；下游
  MUST 经 `fusion_mail::` 引用，MUST NOT 自带 lettre 直接依赖——版本偏斜产出
  `expected Message, found Message` 类编译误诊（fusion-ai `bytes` / `futures`
  re-export 先例同构）。
- 携密纪律：`SmtpConfig` 手写 Debug，`password` 打印 `<REDACTED>`
  （framework-conventions §2 验证锚点 = `config.rs` 单测
  `smtp_config_debug_never_leaks_password`）。

## 5. `TlsMode` 与端口推断

`Plain` / `StartTls`（default，587）/ `Wrapper`（465 隐式直连）；`from_port(465)
→ Wrapper`、其余 → `StartTls`（25 明文需显式声明——按端口推断到明文是危险默认）。
`SmtpSender` 构造一次、发送多次（lettre pool 连接复用）。
