# fusion-storage 语义真相源

> `fusion-storage` 是对象存储机制层：opendal `Operator` 工厂 + 预签名 URL。本文收
> capability gate 行为、presign 边界、feature 面与错误分级的框架级语义；业务侧的
> storage key 约定、路由挂载、密钥装配、TTL 策略归消费仓（framework-conventions §7）。

## 1. feature 透传（opendal services-* 归属）

- crate feature `fs`（default）/ `oss` / `s3` / `obs` 一一映射 opendal `services-*`
  （映射表见 [crate README](../../crates/fusion-storage/README.md)）。
- opendal 在 crate 内**直声明、不走 workspace 模板**：模板会把固定 feature 集带进
  所有消费闭包，透传失效。版本与消费仓捆绑同锁（0.57；pin 注释在 crate
  `Cargo.toml`），MUST NOT 在本 crate 单独 bump。
- 各消费方后端能力错位（如某仓显式裁剪 s3）是**显式接受的既定事实**，非待消除项。

## 2. `build_operator` 行为

- 统一挂 opendal `LoggingLayer`。
- 错误三分级，MUST NOT 混淆：
  1. 后端已知但 feature 未启用 → `enable the fusion-storage cargo feature '<name>'`
     （排障时定位到消费方 Cargo.toml，而非误判后端不支持）；
  2. 后端未知 → `Unsupported storage backend: '<name>'`；
  3. 分支内构造失败 → 各后端具体原因（fs 建 root 失败 / Operator::new 失败等）。
- fs 后端 `root` 必填：crate 不设默认 root（各消费方取值不同，装配期注入）；云后端
  `root` 缺省 `/`（对象存储自然命名空间根）。
- `StorageConfig` 为后端超集平铺（全 Option 除 `backend`），无 `Default`（backend 与
  root 均无隐式值）；携密 Debug 脱敏（framework-conventions §2）。

## 3. presign 边界与 capability gate

- **分流**：读 = `cap.presign || cap.presign_read` 走 native `presign_read_with`，否则
  fs HMAC 路由 URL；写 = `cap.presign_write` 走 native `presign_write_with`，否则
  fs HMAC 路由 URL。gate 让 fs→云后端切换零代码改动。
- **测试锚只锚分流行为**（fs → HMAC 路由 / 云 → native 的路径选择）；云厂商 native
  presign 的签名正确性是 opendal/reqsign 上游测试责任，本仓不复刻 regression。
  opendal bump 时按其 changelog 复审 capability 面与 presign 行为。
- **wire 契约锁定**：HMAC 消息格式（读 `{creator_id}:{key}:{expires}`、上传
  `PUT:{creator_id}:{key}:{expires}`）与 fs URL 形态（token/expires/creator_id query
  序列）是签发↔验签跨版本互认契约，MUST NOT 变更；金样测试锚在
  `crates/fusion-storage/src/{hmac,presign}.rs`。
- **签名缓存**：同一输入在同一 ttl 桶内复用签好的 URL（4096 上限懒清理）。
  `hmac_secret` 刻意不在缓存键——按进程不变假设工作，进程内轮换密钥会复用旧 URL；
  换密钥必须换进程（滚动重启）。
- **桶对齐**：fs 路径 expires = `(now / ttl + 2) * ttl`（轮询类消费方的响应稳定前提，
  桶内最短剩余有效期 ≥ ttl）；云 native presign 内部取时间戳，同桶稳定性由缓存提供。
- `presign_write` 丢弃 content_type（opendal 0.55+ 行为）：两步直传的 confirm 步骤
  MUST 调 `op.stat` 重新校验，不得信任预签名 URL 的约束。

## 4. 消费方注入面（crate 刻意不收）

| 注入面 | 形态 |
| --- | --- |
| fs 公开访问 base | 消费方自行从 env / 请求 header 推导后传入 `FsPresignRoutes` |
| fs 路由 URL 形态 | `FsPresignRoutes { authority, scheme, read, write }`（与挂载点一致） |
| HMAC 密钥 | 参数传入（env 装配、默认密钥、`init` fail-closed 语义归消费方） |
| 默认 root / backend 缺省 | 消费方配置装配层（如缺段 fallback）注入 |
| TTL 策略 | 调用方按资产类型传 `ttl_secs` |
