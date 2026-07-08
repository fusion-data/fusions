# Fusion Framework

> 河出图，洛出书，圣人则之。 — 《周易·系辞》

## 起源

相传上古伏羲氏时，黄河浮现龙马，背负神秘图案，史称**河图**。伏羲氏观河图之象，推演八卦，开启了华夏文明的智慧之源。

河图蕴含天地阴阳之理，万物生成之序，被誉为"宇宙魔方"。

## 设计哲学

河图以数位象，**一生水、二生火、三生木、四生金、五生土**，五行相生相克，万物由此而生。

Fusion Framework 承袭此意：

| 河图 | Fusion                       | 职责                                         |
| ---- | ---------------------------- | -------------------------------------------- |
| 一水 | `fusion-common`              | 万物之源 — 基础工具与类型                    |
| 二火 | `fusion-core`                | 运行之心 — Application/Component/Plugin 核心 |
| 三木 | `fusion-web` / `fusion-rpc` | 向外生长 — Web 与 ConnectRPC 服务            |
| 四金 | `fusion-security`            | 坚固防护 — 认证与加密                        |
| 五土 | `fusionsql` / `fusion-db`    | 厚德载物 — 数据持久化                        |

## 架构

```
                        ┌─────────────┐
                        │   fusions     │  ← 聚合入口（龙马负图）
                        └──────┬──────┘
                               │
        ┌──────────────────────┼──────────────────────┐
        │                      │                      │
   ┌────┴────┐           ┌─────┴─────┐          ┌─────┴─────┐
   │ fusion-   │           │  fusion-    │          │  fusion-    │
   │ common  │←─────────→│   core    │←─────────→│ security  │
   └─────────┘           └─────┬─────┘          └───────────┘
         ▲                     │                      ▲
         │              ┌──────┴──────┐               │
         │              │             │               │
   ┌─────┴─────┐   ┌────┴───┐   ┌────┴────┐    ┌─────┴─────┐
   │ fusionsql   │   │fusion-web│   │fusion-rpc│    │  fusion-ai  │
   │ fusion-db   │   └────────┘   └─────────┘    └───────────┘
   └───────────┘
```

## 模块

| Crate                | 说明                                                  |
| -------------------- | ----------------------------------------------------- |
| `fusion-common`      | 基础工具：错误处理、时间、序列化、上下文等            |
| `fusion-core`        | 核心框架：Application、Component、Plugin 生命周期管理 |
| `fusion-core-macros` | 过程宏：Builder 派生等编译时增强                      |
| `fusion-security`    | 安全模块：JWT、密码哈希、OAuth2                       |
| `fusionsql`          | SQL 工具：查询构建器、ORM 模式                        |
| `fusion-db`          | 数据库：连接池、事务管理                              |
| `fusion-web`         | Web 服务：Axum 封装、中间件、路由                     |
| `fusion-rpc`         | ConnectRPC 服务与客户端 transport                    |
| `fusion-ai`          | AI 集成：LLM、向量数据库                              |
| `fusions`            | 聚合包：一键引入全部功能                              |

## 特性

- **Plugin 架构** — 依赖注入、按序加载、循环检测
- **Component 系统** — 类型安全的服务注册与获取
- **配置中心** — 多源合并、环境变量、热加载
- **优雅关闭** — Shutdown Hook、信号处理
- **零成本抽象** — 编译时宏、泛型优化

## 快速开始

```rust,no_run
use fusions::core::Application;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = Application::builder()
        .run()
        .await?;

    println!("{} started at {}", app, app.start_time());

    Application::await_shutdown().await;
    Ok(())
}
```

## 依赖

```toml
# 本仓 workspace 内开发
cargo check --workspace
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-targets --all-features

# 消费仓通过 submodule + path 依赖接入
fusions = { version = "0.2.0", path = "../repos/fusions/crates/fusions" }
fusion-common = { version = "0.2.0", path = "../repos/fusions/crates/fusion-common" }
fusion-web = { version = "0.2.0", path = "../repos/fusions/crates/fusion-web" }
```

## 技术栈

- **Runtime**: Tokio 1.x
- **Web**: Axum 0.8 + Tower
- **Database**: SQLx + SeaQuery + PostgreSQL
- **Serialization**: serde + sonic-rs
- **Observability**: tracing + tracing-subscriber + init-tracing-opentelemetry + metrics
- **AI**: rig + rmcp

---

> 河图之数，天地之理，万物之源。
> Fusion 之意，框架之本，服务之基。
