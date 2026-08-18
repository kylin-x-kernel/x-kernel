# EEVDF 唤醒与交接

place / pick / buddy / WF_SYNC 语义，以及调度算法侧禁区。
测法与分类见 [wakeup-latency.md](wakeup-latency.md)。

## ns-slice 回归：已证明的 p99.9 根因

HEAD（周期 100Hz tick、50ms request）schbench p99.9 ~100µs。改成
`DEFAULT_SLICE_NS` = 2ms + oneshot hrtick 后，**p50 / RPS 一直健康，p99.9 曾钉在
~2ms**。`/proc/sched_stat` 证明常见路径（本核 futex handoff）是好的，IPI 送到了，
不是选核 fallback。busy home 上 `mark=0`、`mark_no_buddy` ≈ `remote_resched`、
`nom_no_curr` 高：远端 WF_SYNC 入队时 EEVDF `curr` 为空，`nominate_wake_buddy`
直接 return。Linux `set_next_buddy` 不看 `curr`。leave→pick 窗口里 ready 上已有
刚放回的 runner 时，没有 hint 就会把它再跑完一段 request。

**必须保留的修法：** `curr` 为空时仍然提名；`mark_sync_wake_preempt` 当时设
`prefer_sync_buddy`（leave→pick 往往不再走 WF_SYNC 探测）。修后 p99.9 回到
~70–90µs。

Linux 对齐、且不是这条长尾根因、仍保留的：

- `min_vruntime` 计入离树 `curr`；`leave` 清 `curr` 前更新水位（6.12
  `update_min_vruntime` / `put_prev`）
- IPI 只置 pending；探测失败才武装 backup hrtick
- PLACE_LAG 公式 `vruntime = V - lag`（lag 按 `(W+w)/W` 膨胀）

已收回、不要再加回来的本地补丁（不是 Linux `place_entity` / `update_deadline`）：

- 把 wake `vruntime` 钳到系统 V，或 `max(min_vruntime.min(V))` floor
- 探测里 `vruntime >= deadline` 的到期捷径（改由 `update_current` 滚新
  `vd = ve + r/w`，对齐 Linux `update_deadline`）

看 **busy home**（通常 CPU0）上哪一列先涨（修前基线）：

| 计数 | 含义 |
|------|------|
| `mark` 高、`wake_sync_preempt` 仍 ≈0 | 入队时标上了，IPI/timer 探测没走成 WF_SYNC |
| `mark_no_buddy` / `nom_no_curr` | 没提名到 buddy（`curr` 空，或 hint 已被吃掉） |
| `probe_no_buddy` | 探测时 flag 还在，buddy 已被 `pick` 消费 |
| `probe_ineligible` | 探测时 buddy 的 vruntime > 系统 V |
| `probe_false_buddy` | 探测判定 curr 继续跑，buddy 仍在树上（典型：半截 request，deadline 更早） |
| `buddy_drop` | `try_pick_wake_buddy` 拿了 hint 又丢掉 |

本地 messenger CPU 应看到 `mark` 与 `wake_sync_preempt` 同量级；若只有 busy home
对不上，根因就在 **远端 mark→probe 这一段**，而不是 PLACE 公式本身。

## 长尾：place + pick（多 ms）

### 假 1-tick wake deadline + 先 `put_prev(curr)` 再 `pick`

- 现象：p50 很好，**p99.9 爆炸（ms）**。
- 机制：短 deadline wakee 与重新入队的 `curr` 在 ready 树 leapfrog；pile-up
  时同一任务反复胜出。
- 修法：完整 request deadline（`vd = ve + r/w`）；非自愿路径用 **`curr` 离树**
  的 `peer_preempts_curr` 探测，只有同伴真胜再 `put_prev` + `pick`。

### Wake vruntime > 系统 V（不 eligible）

- 现象：wakee 空等一整段或多段 request。
- Linux：`place_entity` 是 `vruntime = V - lag`，负 lag **允许**暂时 ineligible。
  不要为延迟把 wakee 钳到 V。真正的 schbench 2ms 尾是 NEXT_BUDDY 在 `curr == None`
  时没提名，不是这条。

### 到期 request 卡在旧 deadline 上

- Linux `update_deadline`：`vruntime >= deadline` 时赋新 `vd = ve + r/w` 并
  resched（有同伴时）。不要在 `peer_preempts_curr` 里用「已到期则必抢」捷径；
  旧 deadline 会一直赢探测。`mark_sync_wake_preempt` 当时设 `prefer_sync_buddy`，
  因为远端可能在 IPI 探测前就 `leave`+`pick`。

### `min_vruntime` 不含离树 `curr`

- 对齐 Linux 6.12 `update_min_vruntime`：离树但仍 runnable 的 `curr` 参与水位；
  `leave` 清 `curr` 后再入队不要按 ready-only 树抬水位。这不是把 PLACE 钳到 V。

## 常见路径 p50：handoff（不是当初的 ms 尾）

满 slice + `peer_preempts_curr` 修好 p99.9 后，p50 常停在 ~60 µs：wakee 打不过
半截 `curr`，要等 waker 自己 block。

Linux 对齐、且不抬回 ms 尾的修法：

1. **NEXT_BUDDY** — busy rq 唤醒提名 wakee；保留更早 deadline 的既有 buddy；
   下次 pick 仅当 buddy **eligible 且不晚于** 当前最优 eligible（
   `!entity_before(best, buddy)`）时才优先，否则丢弃 one-shot hint，避免越过
   更早 deadline 的就绪任务 leapfrog。
2. **WF_SYNC** — futex `drain_inactive` 与 `Task::interrupt()` 走
   `ktask::with_wake_sync`；eligible buddy 可 sync-preempt 半截 `curr`（即使
   deadline 更晚）；随后 `prefer_sync_buddy` 保证 pick 仍交给该 buddy，不被刚
   放回的 curr 抢回。
   NEXT_BUDDY 提名与 `mark_sync_wake_preempt` 必须同一次持锁；拆成两次加锁时，
   远端目标 CPU 会在中间 pick 走 buddy，本地 wake（本核 IRQ 关）仍成功、远程
   失败，p99.9 停在一个 request。

### 动态 hrtick IPI 抢在探测前 refresh/disarm

- 现象：改 ns-slice / 取消周期 tick 之后 p50 仍好，**p99.9 ≈ `DEFAULT_SLICE_NS`**；
  busy home 上 `wake_sync_preempt≈0`、`timer_irq_sched` 远小于 unblock。
- 机制：改之前远端 IPI 只置 `need_resched`，IRQ 尾 `peer_preempts_curr` 做 WF_SYNC。
  动态 timer 路径在 IPI 里对目标 RQ `account` + `refresh_sched_deadline`，与 waker
  持有的远端 `&mut RunQueue` 别名，并把 schedule slot 重编程或 `Some(0)` disarm。
  探测失败后若不再武装 backup hrtick，wakee 空等当前 request。
- 修法：IPI 仍只置 pending；探测失败才在 `preempt_resched` 武装剩余 request。
  `leave_current` 清掉 `curr` 后再入队时不要按 ready-only 树更新 `min_vruntime`。

### until-eligible hrtick 到期却不探测

- 现象：提名 / WF_SYNC 常见路径已健康（p50/p99 好，`mark ≈ wake_sync_preempt`），
  **p99.9 仍钉在 `DEFAULT_SLICE_NS`**；busy home 上 `probe_ineligible` 与
  `probe_false_buddy` 同量级。探测失败时 `next_preemption_ns` 会按 until-eligible
  武装短 hrtick，但到期后仍空等剩余那一整段 request。
- 机制：IPI 探测时 buddy `vruntime > V`，抢不过，正确返回并武装 until-eligible。
  V 追上之后 hrtick 到期走 `on_timer_fire`：只 `account_current_runtime`。
  此时 request 未完，wakee 即使已 eligible 也往往 deadline 更晚，
  `update_current` 为 false，不置 `need_resched`。随后 `refresh_sched_deadline`
  把到期 slot 改写成剩余 slice（刚滚过则是新的 2ms），`rearm_local_timer`
  看不到 overdue slot，IRQ 尾从不 `peer_preempts_curr`。WF_SYNC 的第二次机会没了。
- 修法：对齐 Linux `entity_tick`。仅 schedule slot 到期时
  `check_preempt_tick`（不消费标记）：eligible NEXT_BUDDY 或更早 deadline 才
  置 `need_resched`。`update_current` 只在 request 用完时 resched。
  `next_preemption_ns` 只为 ineligible 的 WF_SYNC buddy 武装 until-eligible，
  不为所有 ineligible waiter 做 10µs 轮询，也不因「同伴 deadline 更早」返回
  `Some(0)`（会把刚换上的 later-deadline wakee 立刻抢回去）。IRQ 尾
  `peer_preempts_curr` 才消费标记；抢不过就返回。

仅 buddy、ready 里又只有 wakee 时，几乎省不下时间；**sync preempt** 才缩短
“等 waker block”。

## Sticky home

`select_wake_run_queue` **粘 home**（cpumask 不含 home 才 fallback）。
home 忙 → idle 溢出会把 schbench RPS 从 ~450 打到 ~325；不要为“均衡”加回来。

Sticky pile-up / 缺 LB 解释不了 **整段 p50 抬升**；那是 handoff 模型问题。

## 禁区（不要再引入）

1. 全局假 1-tick（或其它合成短）wake deadline 买延迟。
2. home 忙 → idle 唤醒溢出。
3. 非自愿抢占决策前把 `curr` 先放回 ready 树。
4. 把缺 LB 当成系统性 p50 变差的主因。
5. 一轮堆很多投机性调度改动——归因会废。

## 关键代码

- `task/ksched/src/eevdf.rs` — place、`peer_preempts_curr`、buddy、sync preempt
- `task/ktask/src/run_queue.rs` — sticky wake、`preempt_resched`、`mark_sync_wake_preempt`
- `process/kfutex/src/table.rs` — `with_wake_sync`
