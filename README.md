# slashie

发 `/前缀 内容`，本机执行对应命令，结果回帖到原消息。

- 不建机器人、不加群、不用别人授权——用 lark-cli 的本人身份轮询搜索
- 飞书不提供「本人发消息」的 callback，slashie 就是自己造的那个 callback
- 状态：Mac 菜单栏版已跑通，`/doubao` 单条 6.8–8.8 秒回帖；测试群「fsq 斜杠测试 1119」

## 目录结构

| 目录 | 职责 |
|---|---|
| `src/app` | 菜单栏外壳：图标、状态文案、点击动作（tauri，仅 Mac 编译） |
| `src/runtime` | 内核：配置、飞书调用、命令执行、轮询节奏（lark-cli、`/bin/sh`，无界面依赖） |
| `src/main.rs` | 入口：单实例检查，决定跑菜单栏还是无界面常驻 |

## 构建

```sh
# Mac，带菜单栏外壳
cargo build --release

# Linux / 无界面常驻
cargo build --release --no-default-features
```

详细设计见 [docs/DESIGN.md](docs/DESIGN.md)。
