# repos/fusions — Fusion Framework

业务无关的 Rust 应用框架（`fusion-*` / `fusions` / `hetuflow-*` crate 集合）。crate 清单、设计隐喻与接入方式见 [README.md](./README.md)。框架横切约定 SSOT 见 [docs/designs/framework-conventions.md](./docs/designs/framework-conventions.md)。

## Gate 命令

所有命令在本仓根执行。改 PR 前 4 条全绿才算完成（CI = [.github/workflows/rust.yml](./.github/workflows/rust.yml)）：

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
```

## Gotchas

这些是非显而易见、不读就会做错的本仓取值：

- **rustfmt 用 2 空格缩进**（[rustfmt.toml](./rustfmt.toml)，非 Rust 默认 4）。改代码后 MUST `cargo fmt`，否则 fmt --check 红。
- **clippy 把 warning 当 fail**（`-D warnings`）。workspace lints 另允许了 `result_large_err` / `too_many_arguments` / `refining_impl_trait`（[Cargo.toml](./Cargo.toml) `[workspace.lints]`，均有注释说明原因），不要误判为漏标。
- **`fusion-mq` Postgres 集成测默认跳过**：需 `FUSION_MQ_TEST_URL=postgres://...` 才跑；无该环境变量时这组测不参与 CI 结果。
- **关键依赖有 pin + 注释**（[Cargo.toml](./Cargo.toml) `[workspace.dependencies]`：sqlx 0.8 / rig 0.39 / connectrpc 0.8 等）。升级前 MUST 读 pin 注释理解原因，MUST NOT 盲目 bump。

## 框架约定红线

[docs/designs/framework-conventions.md](./docs/designs/framework-conventions.md) 是横切约定唯一真实源（§1–§7：错误语义 / 安全默认 / 并发资源 / 数据访问 / DI 命名 / 宏卫生 / **框架业务无关**）。新增或改框架代码前 MUST 读对应小节。

**§7「框架业务无关（消费方标识符零泄漏）」是最高频红线**——fusions 是业务无关 lib/framework，可被多个项目以 path / git / submodule 等任一方式依赖。框架代码 / 默认值 / 日志 metric / 测试 fixture / 注释 doc MUST NOT 硬编码任何消费方标识符（产品名 / 服务名 / proto 包名 / cookie 名 / 库名 / 角色码 / metric 名等）。改完跑该节验证锚点：

```bash
grep -rniE 'hetuos|hetu-creative|hylx|careos|chiying|hetu-chiying|gongshu' --include='*.rs' --include='*.toml' --include='*.md' . | grep -v '^./target' | grep -v 'grep -rniE' | grep -v 'docs/exec-plans/archived/' | grep -vE '^Cargo\.toml:[0-9]+:authors'
# MUST 零命中（模式内词均为消费方名）；豁免 = 锚点命令行自身 / 归档 exec-plan 历史叙述 / 根 Cargo.toml authors 行——详见 framework-conventions §7 验证锚点注
```

## 文档边界

什么留本仓 vs 什么归消费仓，见 [docs/README.md](./docs/README.md#文档边界)：通用 API / crate 设计 / framework-level BDD / provider adapter 形状留本仓；应用流程 / 产品规格 / UI / 权限码 / 业务 schema / 业务集成测试归消费仓。

## 本仓 skills

`.agents/skills/` 下 `fusions` / `axum-tower` / `committing` / `rust-best-practices` 是框架自带 skill（其中 `rust-best-practices` 是 Apollo MIT 上游副本，attribution 须保留，内容不可改）。改这些 skill = 改本仓文件。
