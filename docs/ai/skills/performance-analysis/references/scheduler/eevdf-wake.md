# EEVDF 唤醒与交接

place / pick / buddy / WF_SYNC 语义，以及调度算法侧禁区。
测法与分类见 [wakeup-latency.md](wakeup-latency.md)。

## 长尾：place + pick（多 ms）

### 假 1-tick wake deadline + 先 `put_prev(curr)` 再 `pick`

- 现象：p50 很好，**p99.9 爆炸（ms）**。
- 机制：短 deadline wakee 与重新入队的 `curr` 在 ready 树 leapfrog；pile-up
  时同一任务反复胜出。
- 修法：完整 request deadline（`vd = ve + r/w`）；非自愿路径用 **`curr` 离树**
  的 `peer_preempts_curr` 探测，只有同伴真胜再 `put_prev` + `pick`。

### Wake vruntime > 系统 V（不 eligible）

- 现象：wakee 空等一整段或多段 request。
- 修法：`PLACE_LAG` 后把 wake `vruntime` **钳到系统 V**。

## 常见路径 p50：handoff（不是当初的 ms 尾）

满 slice + `peer_preempts_curr` 修好 p99.9 后，p50 常停在 ~60 µs：wakee 打不过
半截 `curr`，要等 waker 自己 block。

Linux 对齐、且不抬回 ms 尾的修法：

1. **NEXT_BUDDY** — busy rq 唤醒提名 wakee；保留更早 deadline 的既有 buddy；
   下次 pick 仅当 buddy **eligible 且不晚于** 当前最优 eligible（
   `!entity_before(best, buddy)`）时才优先，否则丢弃 one-shot hint，避免越过
   更早 deadline 的就绪任务 leapfrog。
2. **WF_SYNC** — futex `drain_inactive` 走 `ktask::with_wake_sync`；eligible
   buddy 可 sync-preempt 半截 `curr`（即使 deadline 更晚）；随后
   `prefer_sync_buddy` 保证 pick 仍交给该 buddy，不被刚放回的 curr 抢回。

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
