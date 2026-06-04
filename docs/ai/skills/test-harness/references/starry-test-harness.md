# Starry Test Harness Reference

This reference describes the shared Starry Test Harness repository:

- `git@gitee.com:openkylin/starry-test-harness.git`

In the current development environment, that repository is checked out at:

- `/home/laoyekang/code/starry-test-harness`

The examples below use `<starry-test-harness>` for the repository root. In the
current workspace, `<starry-test-harness>` resolves to
`/home/laoyekang/code/starry-test-harness`.

## Suite Entry Points

Common suite commands from the harness root:

```bash
make ci-test run
make ci-test run ARCH=riscv64
CASES=timerfd-semantics make ci-test run
make ci-test-iter run
CASES=my-test make ci-test-iter run
make ci-test inject
CASES=my-test make ci-test inject
make daily-test run
ARCH=aarch64 make longevity-test run
make longevity-matrix
```

Useful environment variables:

- `XKERNEL_REMOTE=/path/to/x-kernel`: use a local kernel tree
- `ARCH=aarch64|riscv64|x86_64|loongarch64`
- `CASES=case-a,case-b`
- `JOBS=N`
- `ROOTFS_VARIANT=...`
- `GUEST_CASES_TARGET_DIR=/persistent/target-dir`

## Directory Layout

Important harness directories:

```text
<starry-test-harness>/
├── README.md
├── tests/
│   ├── ci/suite.toml
│   ├── ci-test-iter/suite.toml
│   ├── daily/suite.toml
│   ├── longevity/suite.toml
│   ├── cases/tests/*.rs
│   ├── test-utils/
│   └── host-companions/
├── logs/<suite>/<timestamp>/
│   ├── suite.log
│   ├── cases/*.log
│   └── artifacts/<case>/
└── scripts/
```

## Adding a Rust Guest Test

The common path is:

1. Add a guest test source file under:

```text
<starry-test-harness>/tests/cases/tests/<case_name>.rs
```

2. Use ordinary Rust `#[test]` functions.

3. If useful, depend on helpers from:

```text
<starry-test-harness>/tests/test-utils/
```

4. Register the case in the target suite:

```toml
[[cases]]
name = "my-test"
runner = "guest-test"
binary = "my_test"
```

If the Rust libtest inside the guest should run serially:

```toml
[[cases]]
name = "my-test"
binary = "my_test"
test_threads = 1
```

The harness will typically run:

```text
/usr/tests/my_test --show-output
```

## Adding a Prebuilt or Scripted Guest Case

Use `inject` entries plus an explicit guest command:

```toml
[[cases]]
name = "my-prebuilt"
runner = "guest-test"
guest_command = "chmod +x /usr/tests/bin && /usr/tests/bin"

[[cases.inject]]
src = "tests/xxx/${ARCH}/bin"
dest = "/usr/tests/bin"
```

Directory sources are injected recursively.

## Powercut and Host Companion Modes

For powercut regression:

```toml
[[cases]]
name = "fio-powercut"
runner = "guest-powercut-test"
guest_command = "sh /root/fio/powercut-prepare.sh"
verify_guest_command = "sh /root/fio/powercut-verify.sh"
kill_trigger = "STARRY_POWERCUT_READY"
powercut_cycles = 3
```

For host/guest coordinated tests:

```toml
[[cases]]
name = "my-net-test"
runner = "guest-test"
binary = "my_guest"
host_companion = "my-companion"
companion_delay_secs = 3
companion_timeout_secs = 30
```

See also:

- `<starry-test-harness>/tests/host-companions/README.md`

## Choosing Where to Register a Case

- `tests/ci-test-iter/suite.toml`: first landing place for new regression cases
- `tests/ci/suite.toml`: stable cases with high signal and reasonable runtime
- `tests/daily/suite.toml`: broader periodic regression or performance checks
- `tests/longevity/suite.toml`: soak and stress scenarios

Prefer `ci-test-iter` first unless the case is already obviously mature.

## Reviewing Existing Coverage Before Adding a Case

Search these locations first:

```bash
rg -n "keyword|syscall_name|semantic_name" <starry-test-harness>/tests/cases/tests
rg -n "case-name|binary-name" <starry-test-harness>/tests/*/suite.toml
```

Good existing syscall-style examples include:

- `tests/cases/tests/fcntl_semantics.rs`
- `tests/cases/tests/timerfd_semantics.rs`
- `tests/cases/tests/clock_semantics.rs`
- `tests/cases/tests/itimer_semantics.rs`

Use existing `*_semantics.rs` files as the default landing place when the new
check extends the same contract family.

## Running and Verifying a Case

Typical narrow iteration loop:

```bash
cd <starry-test-harness>
CASES=my-test make ci-test-iter run XKERNEL_REMOTE=/home/laoyekang/code/x-kernel
```

Inspect results under the latest run directory:

```text
logs/ci-test-iter/<timestamp>/
├── suite.log
├── cases/my-test.log
└── artifacts/my-test/
```

What to inspect:

- `suite.log`: suite-level scheduling and overall pass/fail
- `cases/<name>.log`: guest-visible test output and command result
- `artifacts/<name>/results.json`: structured metrics/results when the case provides them

Fast lookup helpers:

```bash
cat logs/ci-test-iter/last_run.json
tail -n 200 logs/ci-test-iter/<timestamp>/suite.log
tail -n 200 logs/ci-test-iter/<timestamp>/cases/my-test.log
```

## Syscall Test Design Constraints

For syscall/user-space regression cases:

- start from Linux/POSIX-visible behavior;
- prefer one case per semantic contract cluster, not one case per internal code path;
- avoid asserting exact internal errno sources unless the public contract requires them;
- avoid white-box assumptions about scheduler timing, internal locks, or concrete helper calls;
- check whether an existing semantics test can absorb the new branch before creating a new file.

Examples of non-redundant additions:

- a previously uncovered errno branch;
- behavior under concurrent invocation;
- an ABI edge such as alignment, null pointer semantics, or restart rules;
- a lifecycle edge such as close-after-dup, peer exit, EOF, HUP, or signal delivery.

Examples of redundant additions:

- repeating the same success path with different incidental data;
- re-testing an errno already covered by the same semantics file without a new condition;
- adding a new file when one more `#[test]` in an existing semantics file would do.
