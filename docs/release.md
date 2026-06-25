# X-Kernel 版本号与发布分支规范

## 1. 目标

为了规范 X-Kernel 的版本发布、分支维护、补丁管理和版本追踪，项目采用统一的季度发布机制和语义化版本号规则。

X-Kernel 计划按照季度节奏发布版本，每个季度发布一个里程碑版本。版本分支用于稳定维护，版本号用于标识能力成熟度和发布时间窗口。

---

## 2. 基本原则

X-Kernel 版本管理遵循以下原则：

1. **主干持续演进**
   - `main` 分支作为主开发分支。
   - 新功能、架构调整、子系统重构优先在 `main` 分支完成。

2. **季度发布**
   - 每个季度创建一个发布分支。
   - 发布分支以发布年月命名， `release/2606` 表示 2026 年 6 月版本。

3. **语义版本号 + 发布窗口**
   - 版本号采用 `vMAJOR.MINOR.PATCH-YYMM` 格式。
   - 示例：`v0.1.0-2606`。

4. **发布分支只做稳定维护**
   - 发布分支创建后，原则上只接受 bugfix、稳定性修复、文档修正和必要的测试补充。
   - 新功能应继续合入 `main`，不直接进入已发布分支。

---

## 3. 分支命名规范

### 3.1 主分支

```text
main
```

说明：

- 项目主开发分支。
- 承载日常开发、架构演进、新功能开发和持续集成。
- 所有季度发布分支从 `main` 分支拉出。

---

### 3.2 发布分支

发布分支命名格式：

```text
release/YYMM
```

其中：

| 字段 | 含义 | 示例 |
|---|---|---|
| `release` | 发布分支前缀 | `release` |
| `YY` | 发布年份后两位 | `26` |
| `MM` | 发布月份 | `06` |

示例：

```text
release/2606
release/2609
release/2612
release/2703
```

含义：

| 分支名 | 含义 |
|---|---|
| `release/2606` | 2026 年 6 月季度发布分支 |
| `release/2609` | 2026 年 9 月季度发布分支 |
| `release/2612` | 2026 年 12 月季度发布分支 |
| `release/2703` | 2027 年 3 月季度发布分支 |

---

## 4. 版本号命名规范

X-Kernel 版本号采用如下格式：

```text
vMAJOR.MINOR.PATCH-YYMM
```

示例：

```text
v0.1.0-2606
```

字段说明：

| 字段 | 含义 | 示例 |
|---|---|---|
| `v` | 版本前缀 | `v` |
| `MAJOR` | 主版本号，表示重大架构稳定阶段 | `0` |
| `MINOR` | 次版本号，表示季度能力版本 | `1` |
| `PATCH` | 补丁版本号，表示修复版本 | `0` |
| `YYMM` | 发布窗口，表示年月 | `2606` |

---

## 4.1 Workspace 版本号要求

仓库根目录 `Cargo.toml` 中的 `workspace.package.version`
是 Rust workspace 级别的包版本元数据，必须和正式发布版本的
`MAJOR.MINOR.PATCH` 保持一致。

格式：

```text
MAJOR.MINOR.PATCH
```

示例：

```text
0.1.0
```

对应关系：

| 场景 | 版本值 |
|---|---|
| `workspace.package.version` | `0.1.0` |
| Git Tag / 对外发布版本 | `v0.1.0-2606` |

规则：

1. `workspace.package.version` 不带 `v` 前缀。
2. 发布分支上的 `workspace.package.version` 不带 `-YYMM` 发布窗口后缀。
3. `main` 分支在发布分支切出后，必须立即前进到下一季度目标版本，并使用 `-dev` 预发布标记。
4. 每次季度正式发布或补丁发布导致 `MAJOR.MINOR.PATCH` 变化时，必须同步更新根 `Cargo.toml` 中的 `workspace.package.version`。
5. 发布 Tag 中的 `MAJOR.MINOR.PATCH` 必须与当次发布对应提交上的 `workspace.package.version` 完全一致。

示例：

```text
workspace.package.version = "0.1.0"  <=>  Tag: v0.1.0-2606
workspace.package.version = "0.1.1"  <=>  Tag: v0.1.1-2606
workspace.package.version = "0.2.0"  <=>  Tag: v0.2.0-2609
```

主线开发示例：

```text
release/2606: workspace.package.version = "0.1.0"
release/2606: workspace.package.version = "0.1.1"
main:         workspace.package.version = "0.2.0-dev"
```

含义：

| 分支 | `workspace.package.version` | 说明 |
|---|---|---|
| `release/2606` | `0.1.0` | 2606 季度首个正式发布版本 |
| `release/2606` | `0.1.1` | 2606 季度补丁版本 |
| `main` | `0.2.0-dev` | 下一季度 `0.2.0` 版本开发中 |

---

## 5. 版本号递增规则

### 5.1 主版本号 MAJOR

主版本号用于表示项目进入新的重大稳定阶段。

当前 X-Kernel 处于能力建设和里程碑演进阶段，因此使用：

```text
v0.x.x
```

当 X-Kernel 达到第一个稳定可用版本后，可进入：

```text
v1.0.0
```

主版本号递增示例：

```text
v0.3.0-2612
v1.0.0-2703
```

---

### 5.2 次版本号 MINOR

次版本号对应季度发布版本。

每个季度发布一个新的里程碑版本时，递增 `MINOR`。

示例：

```text
v0.1.0-2606
v0.2.0-2609
v0.3.0-2612
v0.4.0-2703
```

---

### 5.3 补丁版本号 PATCH

补丁版本号用于表示同一个季度发布分支上的修复版本。

当发布分支上发生 bugfix、稳定性修复、安全修复或文档修正后，可以递增 `PATCH`。

示例：

```text
v0.1.0-2606
v0.1.1-2606
v0.1.2-2606
```

含义：

| 版本号 | 含义 |
|---|---|
| `v0.1.0-2606` | 2606 季度正式发布版本 |
| `v0.1.1-2606` | 2606 分支第一次补丁版本 |
| `v0.1.2-2606` | 2606 分支第二次补丁版本 |

---

## 6. Tag 命名规范

所有正式发布版本必须创建 Git Tag。

Tag 名称必须与版本号保持一致。

格式：

```text
vMAJOR.MINOR.PATCH-YYMM
```

示例：

```text
v0.1.0-2606
v0.1.1-2606
v0.2.0-2609
```

不建议使用以下不规范命名：

```text
2606
v2606
release-2606
xkernel-2606
v0.1
```

---

## 7. 发布分支与版本号对应关系

每个发布分支可以包含一个正式发布版本和多个补丁版本。

示例：

```text
main
  |
  |---- release/2606
  |        |---- v0.1.0-2606
  |        |---- v0.1.1-2606
  |        |---- v0.1.2-2606
  |
  |---- release/2609
  |        |---- v0.2.0-2609
  |        |---- v0.2.1-2609
  |
  |---- release/2612
           |---- v0.3.0-2612
```

对应关系：

| 发布分支 | 首个版本 | 后续补丁版本 |
|---|---|---|
| `release/2606` | `v0.1.0-2606` | `v0.1.1-2606`, `v0.1.2-2606` |
| `release/2609` | `v0.2.0-2609` | `v0.2.1-2609` |
| `release/2612` | `v0.3.0-2612` | `v0.3.1-2612` |

---

## 8. Release Notes 要求

每个正式发布版本都必须附带一份 release notes，用于说明版本定位、主要能力、
重要修复、已知限制和升级注意事项。

建议路径：

```text
docs/releases/vMAJOR.MINOR.PATCH-YYMM.md
```

示例：

```text
docs/releases/v0.1.0-2606.md
docs/releases/v0.1.1-2606.md
docs/releases/v0.2.0-2609.md
```

建议内容：

1. 版本摘要
2. 支持的架构与平台
3. 主要功能与子系统能力
4. 重要修复与稳定性改进
5. 配置、构建与运行说明
6. 已知限制
7. 后续版本计划

---

## 8. 发布版本名称

对外发布时，版本名称建议采用如下格式：

```text
X-Kernel YYMM Milestone Release
```

示例：

```text
X-Kernel 2606 Milestone Release
X-Kernel 2609 Milestone Release
X-Kernel 2612 Milestone Release
```


---

## 9. 发布分支维护规则

发布分支创建后，应进入稳定维护状态。

允许合入的内容：

- bugfix
- 安全修复
- 稳定性修复
- 测试用例补充
- 文档修正
- 构建脚本修正
- CI 配置修正
- 必要的兼容性修复

原则上不允许合入的内容：

- 大规模架构重构
- 新子系统引入
- 不必要的新功能
- 高风险性能优化
- 破坏兼容性的接口调整

如确需将新功能合入发布分支，需要经过版本负责人评审确认。

---

## 10. 发布流程

### 10.1 创建发布分支

从 `main` 创建季度发布分支：

```bash
git checkout main
git pull origin main
git checkout -b release/2606
git push origin release/2606
```

---

### 10.2 创建发布 Tag

在发布分支上创建 Tag：

```bash
git checkout release/2606
git pull origin release/2606
git tag -a v0.1.0-2606 -m "X-Kernel 2606 Milestone Release"
git push origin v0.1.0-2606
```

---

### 10.3 创建补丁版本

当 `release/2606` 分支上有修复后，创建补丁版本：

```bash
git checkout release/2606
git pull origin release/2606
git tag -a v0.1.1-2606 -m "X-Kernel 2606 Patch Release 1"
git push origin v0.1.1-2606
```

---

## 11. 版本发布节奏

X-Kernel 采用季度发布节奏。

建议发布窗口如下：

| 发布窗口 | 发布分支 | 版本号示例 |
|---|---|---|
| 3 月 | `release/YY03` | `v0.x.0-YY03` |
| 6 月 | `release/YY06` | `v0.x.0-YY06` |
| 9 月 | `release/YY09` | `v0.x.0-YY09` |
| 12 月 | `release/YY12` | `v0.x.0-YY12` |

示例：

| 时间 | 发布分支 | 版本号 | 发布名称 |
|---|---|---|---|
| 2026 年 6 月 | `release/2606` | `v0.1.0-2606` | X-Kernel 2606 Milestone Release |
| 2026 年 9 月 | `release/2609` | `v0.2.0-2609` | X-Kernel 2609 Milestone Release |
| 2026 年 12 月 | `release/2612` | `v0.3.0-2612` | X-Kernel 2612 Milestone Release |
| 2027 年 3 月 | `release/2703` | `v0.4.0-2703` | X-Kernel 2703 Milestone Release |

---

## 12. Release Note 命名规范

每个版本发布时，应同步维护 Release Note。

建议文件命名：

```text
docs/releases/x-kernel-2606.md
docs/releases/x-kernel-2609.md
docs/releases/x-kernel-2612.md
```

Release Note 推荐包含以下内容：

```markdown
# X-Kernel 2606 Milestone Release

## 1. 版本信息

- 版本号：v0.1.0-2606
- 发布分支：release/2606
- 发布时间：2026-06-xx
- 发布类型：Milestone Release

## 2. 版本目标

描述该版本的核心目标。

## 3. 主要能力

列出该版本完成的主要功能和子系统能力。

## 4. 重要变更

列出架构、接口、行为上的重要变化。

## 5. 已知问题

列出当前版本尚未解决的问题。

## 6. 测试情况

列出主要测试场景、测试结果和验证范围。

## 7. 后续计划

描述下一季度版本的主要方向。
```

---

## 13. 推荐版本路线

当前阶段建议采用如下路线：

| 版本 | 分支 | 定位 |
|---|---|---|
| `v0.1.0-2606` | `release/2606` | 基础能力里程碑版本 |
| `v0.2.0-2609` | `release/2609` | 场景验证里程碑版本 |
| `v0.3.0-2612` | `release/2612` | 双体系可信子系统演示版本 |
| `v1.0.0-2703` | `release/2703` | 首个稳定版本候选 |

---

## 14. 总结

X-Kernel 版本体系采用：

```text
分支：release/YYMM
版本：vMAJOR.MINOR.PATCH-YYMM
名称：X-Kernel YYMM Milestone Release
```

示例：

```text
release/2606
v0.1.0-2606
X-Kernel 2606 Milestone Release
```

其中：

- `release/2606` 表示 2026 年 6 月发布分支。
- `v0.1.0-2606` 表示 2606 季度的第一个里程碑版本。
- `v0.1.1-2606` 表示 2606 分支上的第一个补丁版本。
- `X-Kernel 2606 Milestone Release` 表示对外发布名称。

该规范既支持季度版本管理，也支持补丁维护和长期版本演进。
