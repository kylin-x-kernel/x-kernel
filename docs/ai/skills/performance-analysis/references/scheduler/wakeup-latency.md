# 唤醒延迟测量（schbench）

测法、基线、如何区分 p50 / p99.9 / RPS，以及调查清单。
EEVDF 语义见 [eevdf-wake.md](eevdf-wake.md)；IRQ / affinity / `block_on` 见
[infra.md](infra.md)。

## 工作负载与基线

参考环境（量级，不是硬 SLA）：

- aarch64 QEMU，**4 CPUs**，定时器约 **100 Hz**，EEVDF；
- `./schbench`（1.0 默认：workers ≈ `get_nprocs()`，无 `-M/-W`）；
- 对比 **wakeup p50 / p99 / p99.9**、**request latency**、**average RPS**。

| 轴 | 健康量级（该环境） |
|----|-------------------|
| wakeup p50 | ~30 µs |
| wakeup p99.9 | ~100–120 µs（Linux guest 常见 ~130 µs） |
| RPS | ~450 |

跨 run 固定 platform、SMP、defconfig、schbench 参数。
`.config` 准备见 `docs/ai/skills/build-workflow/SKILL.md`。

可选：`KFEAT_SCHED_STAT=y` 后读 `/proc/sched_stat`。

## 先分开 p50 和 p99.9

二者机制不同；压一侧常伤另一侧。

| 现象 | 常见含义 |
|------|----------|
| p50/p90/p99 一起抬（~30 → ~60–100 µs），p99.9 仍低 | 常见路径在等 waker block / handoff；不是 LB |
| p50 好、p99.9 多 ms | leapfrog、wakee 不 eligible、pile-up、或抢占被推迟 |
| “优化选核”后 RPS 暴跌（~450 → ~325） | idle-seeking / home→idle 溢出 |

**顺序：** 先用 Linux place/pick 语义压多 ms 长尾，再用 NEXT_BUDDY + WF_SYNC
改善常见 handoff。禁止用全局假短 wake deadline 买 p50。

## 调查流程

```
Wakeup latency chase:
- [ ] 1. 记录 wakeup p50/p99/p99.9、request、RPS（同 flags/CPU）
- [ ] 2. 确认 affinity（get_nprocs / sched_getaffinity）
- [ ] 3. 分类：常见路径偏移 vs 多 ms 尾 vs RPS 崩
- [ ] 4. IRQ 尾抢占 / exception skip（见 infra.md）
- [ ] 5. EEVDF place（钳 V、满 request）与 preempt probe（见 eevdf-wake.md）
- [ ] 6. 再动 buddy / WF_SYNC 抠 p50
- [ ] 7. 勿用 idle-seeking“修”RPS（需单独 RPS 验证）
- [ ] 8. 每轮只改一个行为变量
```

## `/proc/sched_stat`（启用时）

| 信号 | 读法 |
|------|------|
| `wakeup_last_cpu` 高、fallback 低 | sticky home 在工作 |
| `wake_sync_preempt` 随 p50 变好而升 | WF_SYNC 在干活 |
| `wake_handoff` 高 | block 路径在 pick |
| `preempt_skip_exception` 非零（历史） | IRQ/exception 推迟抢占类问题 |
| `remote_resched_fail=0` 但仍慢 | 不是 IPI 丢；看本地 handoff / pile-up |

排查期临时直方图用完就删；需要可留薄计数。

## 变更完整性

行为变更同步 `task/ktask/docs/{design,security}.md` 与 EEVDF rustdoc。
用 Makefile/Kconfig 流程验证，不用裸 `cargo check -p ktask`。

## 本目录暂不覆盖

- 主动负载均衡 / `select_idle_sibling`（主要影响 pile-up 尾，非上述 p50/p99.9 主因）
- CFS / RR 细节
- 远端 running 任务的完整 Linux affinity migrate
