# slashie

发 `/前缀 内容`，本机执行对应命令，结果回帖到原消息。

- 不建机器人、不加群、不用别人授权——用 lark-cli 的本人身份轮询搜索
- 飞书没有「本人发消息」的 callback，slashie 就是自己造的那个 callback
- Mac 菜单栏版已跑通（单条 6.8–8.8 秒回帖）；Linux 同一份内核，headless 常驻

## 文档

| 文档 | 内容 |
|---|---|
| [docs/product.md](docs/product.md) | 产品文档：问题、目标与非目标、场景、功能清单、验证口径、路线图、决策记录 |
| [docs/design.md](docs/design.md) | 技术设计：架构、平台依赖、核心链路、关键机制、配置模型、关键时序、限制 |
| [docs/index.md](docs/index.md) | 文档索引 |

## 目录结构

| 目录 | 职责 |
|---|---|
| `src/app` | 菜单栏外壳：图标、状态文案、点击动作（tauri，仅 Mac 编译） |
| `src/runtime` | 内核：轮询、匹配、执行队列、回帖、状态（无界面依赖） |
| `src/main.rs` | 入口：单实例检查，决定跑菜单栏还是无界面常驻 |

## 构建

```sh
# Mac，带菜单栏外壳
cargo build --release

# Linux / 无界面常驻
cargo build --release --no-default-features
```

## 快速上手

1. 启动后自动生成 `~/.slashie/config.json` 并回填身份
2. 配置路由（前缀 + 命令），agent 类命令用 `"$SLASHIE_PROMPT"`
3. 在飞书任意会话发 `/ping`，验证链路
