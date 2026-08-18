# fusion-storage

对象存储机制层：opendal `Operator` 工厂（feature 透传后端）+ 预签名 URL（云后端
native presign / fs 后端 HMAC 本地路由兜底）。语义真相源（capability gate 行为、
presign 边界、错误分级）见 [docs/designs/storage.md](../../docs/designs/storage.md)。

## Feature 映射表

后端分支的编译面由 crate feature 决定，每个 feature 映射 opendal 的 `services-*`：

| feature | opendal feature | 后端 | 说明 |
| --- | --- | --- | --- |
| `fs`（default） | `services-fs` | 本地文件系统 | 无云厂商依赖，最小接入与单测可用 |
| `oss` | `services-oss` | 阿里云 OSS | `presign_endpoint` 支持 CDN/反代 |
| `s3` | `services-s3` | S3 兼容（含 MinIO，自定义 `endpoint`） | |
| `obs` | `services-obs` | 华为云 OBS | |

消费方按需启用（例）：

```toml
fusion-storage = { path = "...", features = ["fs", "oss"] }
```

未启用的后端在 `build_operator` 返回指向 feature 的错误（区别于「后端不支持」，
防排障误导）。

## 边界（crate 刻意不收）

storage key 约定、公开访问 base 的 env 语义、fs 路由挂载与验签 handler、HMAC
密钥装配与默认密钥、默认 root、TTL 策略——全部归消费方。fs 路由 URL 形态经
`FsPresignRoutes` 注入，HMAC 密钥以参数传入（进程不变假设，见 `PresignCacheKey`
文档）。
