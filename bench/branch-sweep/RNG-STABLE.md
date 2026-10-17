# Wisp RNG 完整任务复测（2026-10-17）

**结论：目前无法确认 RNG 优化带来了完整任务提速。** 严格负载门控下完成了 before/after/after/before 四块测试；每版 18 轮。优化后总耗时中位数由 406.929 ms 降为 399.210 ms，观测差为 7.720 ms（1.897%），相当于 1.0193 倍速度。但差值小于两版 IQR，两个相邻块的变化方向相反。该数字仅为本次观测值，不能作为已证实的收益承诺。

## 方法与边界

Apple M2，8 核，8 GiB，macOS 26.6.2。保持原始 seed=7、PCG64 随机序列及运算：200 万 bars、40 组参数、8 路独立子沙箱；每块每个后端先丢弃一次完整 warmup，再测 9 轮。每轮按固定随机种子打乱 plain Native、Seatbelt Native 和 Wisp 的执行顺序，顺序及全部样本均保留。计时为预热服务上的一次完整任务（数据准备加参数扫描），不包含服务启动与负载等待。

四块依次启动同一 Rust daemon 二进制，每块仅一个 Wisp 服务驻留；before/after 只切换 reactor，不要求四块复用同一个进程。每个子沙箱仍独立，Mach COW 与 prepared imports 保持启用。Native 子进程安装 Seatbelt；file read/write、network、exec、fork 拒绝探针，以及 Wisp 的 file read/write、network、exec 拒绝探针，四块全部通过。

启动、warmup 和每轮测量前要求三次连续 CPU idle ≥80%、memory pressure=1、swap-out=0、swap-in ≤64 页/秒；阈值未放宽。每次逐轮等待上限 300 秒。未关闭用户应用或杀后台进程。所有完成轮次均纳入统计，没有剔除慢轮或选取最快轮；IQR 使用 inclusive 四分位数（Q3−Q1）。

## 完整任务耗时

| 同期版本 | 后端 | 样本数 | 中位数 ms | Q1–Q3 ms | IQR ms |
|---|---|---:|---:|---:|---:|
| before | Wisp | 18 | 406.929 | 392.487–421.379 | 28.891 |
| before | 隔离 Native（Seatbelt） | 18 | 326.322 | 322.274–340.355 | 18.081 |
| before | 普通 Native | 18 | 278.003 | 272.807–285.589 | 12.782 |
| after | Wisp | 18 | 399.210 | 388.974–414.115 | 25.141 |
| after | 隔离 Native（Seatbelt） | 18 | 330.268 | 321.872–344.059 | 22.188 |
| after | 普通 Native | 18 | 279.429 | 275.612–289.777 | 14.164 |

优化后相对优化前节约 **7.720 ms / 1.897%**。同期隔离 Native 中位数由 326.322 ms 变为 330.268 ms（增加 3.946 ms / 1.209%）；普通 Native 增加 0.513%。Native 没有发生此前高负载测试的倍数级漂移，但 Seatbelt 的 IQR 为 18.081 / 22.188 ms，仍存在可见噪声。

| Wisp 相对同期隔离 Native | 额外耗时 ms | 额外耗时百分比 | 中位数之比 | 逐轮 Wisp/Native 比值的中位数（Q1–Q3） |
|---|---:|---:|---:|---:|
| before | 80.607 | 24.702% | 1.247017 | 1.228167（1.132533–1.273454） |
| after | 68.942 | 20.874% | 1.208745 | 1.212200（1.140522–1.252147） |

优化后仍比隔离 Native 多约 **68.942 ms / 20.874%**。中位数之比与逐轮比值中位数是不同统计量，不互相替代；Native 对照使用自身同期样本。

## 顺序效应与分项

| 块 | 版本 | Wisp 中位数 ms | Wisp IQR ms | 隔离 Native 中位数 ms |
|---:|---|---:|---:|---:|
| 0 | before | 395.178 | 19.559 | 323.542 |
| 1 | after | 399.052 | 19.523 | 328.848 |
| 2 | after | 409.244 | 43.714 | 345.969 |
| 3 | before | 413.188 | 22.227 | 333.804 |

相邻 b0→b1：after 比 before **慢 0.980%**；Native 慢 1.640%，按同期 Native 比值归一化后为 0.993509（下降 0.649%）。相邻 b2→b3 逆序对照：after 比 before **快 0.954%**；Native 的 after/before 增加 3.644%，归一化为 0.955630（下降 4.437%）。两对原始 Wisp 变化方向相反，归一化幅度也不稳定，不能把合并后的 1.897% 解释为可靠因果收益。

Wisp setup 中位数 116.365→101.182 ms，而 branch 中位数 289.663→304.863 ms；各分项中位数不可直接相加。分项的反向变化进一步限制了对总任务收益的归因。

## 正确性、哈希与实际测量中的 swap

36 轮 Wisp 的 40 项结果逐项完全相同；两版结果完全一致。与 Native 的最大绝对误差各块均为 7.771561172376096e-15，低于脚本 1e-9 容差。Guest CPython 3.14.3 / NumPy 1.26.4 与 Native CPython 3.14.3 / NumPy 2.4.2 仍有版本差异，Native 比较包含这些实现差异。

- 固定 daemon SHA-256：`07073650d27b98b316291d0d60c659f5e80385e3edc3d191f7e7d64296a35465`。
- before reactor SHA-256：`559cb9c0f1dacba6ffec29a5ce14112d78710f3a4e59115ad8d4155c85a346d4`。
- after reactor SHA-256：`032f74a66dedbcbae9f5c0d227289590a2e10a56de22c8631505a0d0b7bef7ca`。
- 四块记录的哈希与本地实际文件重新计算一致；before 两块一致、after 两块一致、两版不同。

门控通过不代表计时期间完全没有背景换页。每轮 `vm_before` 在门控之后重采，`vm_after` 在该轮三个后端完成后采集，因此下表是全局计数变化，不能归属到某一个后端。

| 块 | swap-in 页总增量 | swap-out 页总增量 | 有 swap-in 的轮次（从 1 开始） |
|---:|---:|---:|---|
| 0 | 4 | 0 | 7: 4 页 |
| 1 | 4 | 0 | 7: 4 页 |
| 2 | 4 | 0 | 6: 4 页 |
| 3 | 31 | 0 | 4: 31 页 |

共 43 页 swap-in、0 页 swap-out；全部样本仍保留。没有记录每个后端计时过程的持续 CPU/pressure 轨迹，不能保证整个计时窗口背景完全静止。

## 原始数据与复现

完整运行前缀：`results/rng-stable-20261017T063646826276Z`。测量源码提交：`110a442685cfdf42110b409dd2a64b4515044245`。

```sh
python3 bench/branch-sweep/stable-rng.py \
  --reference-reactor runtime/cpython-wasi/vendor/cpython/cross-build/wasm32-wasip1/python-reactor-pre-rng-27966be.wasm \
  --output-prefix bench/branch-sweep/results/rng-stable-唯一时间戳 \
  --repeats 9 --idle-timeout 300
```

- 汇总：[rng-stable-20261017T063646826276Z-summary.json](results/rng-stable-20261017T063646826276Z-summary.json)。
- 控制台及块汇总：[rng-stable-20261017T063646826276Z.log](results/rng-stable-20261017T063646826276Z.log)。
- 本次启动前观测：[rng-load-check-20261017T063646826276Z.jsonl](results/rng-load-check-20261017T063646826276Z.jsonl)。
- 块 0 before：[完整数据](results/rng-stable-20261017T063646826276Z-b0-before.json)、[逐轮 checkpoint](results/rng-stable-20261017T063646826276Z-b0-before.rounds.jsonl)、[全部负载观测](results/rng-stable-20261017T063646826276Z-b0-before.load.jsonl)。
- 块 1 after：[完整数据](results/rng-stable-20261017T063646826276Z-b1-after.json)、[逐轮 checkpoint](results/rng-stable-20261017T063646826276Z-b1-after.rounds.jsonl)、[全部负载观测](results/rng-stable-20261017T063646826276Z-b1-after.load.jsonl)。
- 块 2 after：[完整数据](results/rng-stable-20261017T063646826276Z-b2-after.json)、[逐轮 checkpoint](results/rng-stable-20261017T063646826276Z-b2-after.rounds.jsonl)、[全部负载观测](results/rng-stable-20261017T063646826276Z-b2-after.load.jsonl)。
- 块 3 before：[完整数据](results/rng-stable-20261017T063646826276Z-b3-before.json)、[逐轮 checkpoint](results/rng-stable-20261017T063646826276Z-b3-before.rounds.jsonl)、[全部负载观测](results/rng-stable-20261017T063646826276Z-b3-before.load.jsonl)。

此前失败尝试（包括 20261017T032611645069Z、20261017T044642283637Z、20261017T061645121982Z）及其他轮询日志仍按各自唯一前缀保留在本地 results 目录；它们因负载等待超时而未完成，未拼接进本次四块统计。基线 reactor 位于忽略的本地 vendor 构建树，路径与哈希见上；本次提交保留完整测量原始数据，不提交构建二进制。
