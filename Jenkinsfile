#!/usr/bin/env groovy

def ciResults = [:]
def teeResults = [:]

pipeline {
    agent {
        docker {
            image 'yeanwang/x-kernel-builder:v1.5'
            args '-v /var/run/docker.sock:/var/run/docker.sock -v /var/jenkins_home/cargo/registry:/usr/local/cargo/registry -v /var/jenkins_home/.rustup/toolchains:/usr/local/rustup/toolchains -v /var/jenkins_home/xkernel-target:/xkernel-target --privileged -u root:root'
        }
    }

    options {
        skipDefaultCheckout(true)
        timestamps()
        parallelsAlwaysFailFast()
    }

    environment {
        CI = 'true'
        PROJECT_REPO = 'https://gitee.com/openkylin/x-kernel'
        DEFAULT_BRANCH = 'main'
        LIBUTEE_REPO = 'https://gitee.com/openkylin/rust-libutee'
        TEST_HARNESS_REPO = 'https://gitee.com/openkylin/starry-test-harness'
        TEST_HARNESS_BRANCH = 'master'
        AUX_RUST_TOOLCHAIN = 'nightly-2026-03-08'
        CARGO_TERM_COLOR = 'always'
        CARGO_REGISTRIES_CRATES_IO_PROTOCOL = 'sparse'
        RUSTUP_PERMIT_COPY_RENAME = '1'
        PYTHONUNBUFFERED = '1'
        TARGET_DIR = '/xkernel-target'
    }

    stages {
        stage('Prepare Source') {
            steps {
                script {
                    env.ROOT_WS = env.WORKSPACE
                    currentBuild.description = "PR#${env.giteePullRequestIid ?: 'manual'}"
                    prepareSource()
                    ciResults['Prepare Source'] = [status: 'passed']
                }
            }
            post {
                failure {
                    script { ciResults['Prepare Source'] = [status: 'failed', detail: '源码准备失败（可能是分支分叉需要 rebase）'] }
                }
            }
        }

        stage('Check Environment') {
            steps {
                script {
                    checkBuildEnvironment()
                    ciResults['Check Environment'] = [status: 'passed']
                }
            }
            post {
                failure {
                    script { ciResults['Check Environment'] = [status: 'failed', detail: 'Rust 工具链组件或 target 安装失败'] }
                }
            }
        }

        stage('Rustfmt') {
            steps {
                script {
                    runRustfmt()
                    ciResults['Rustfmt'] = [status: 'passed']
                }
            }
            post {
                failure {
                    script { ciResults['Rustfmt'] = [status: 'failed', detail: 'cargo fmt --check 发现格式问题'] }
                }
            }
        }

        stage('Prefetch Dependencies') {
            steps {
                script {
                    prefetchCargoDeps()
                    ciResults['Prefetch Dependencies'] = [status: 'passed']
                }
            }
            post {
                failure {
                    script { ciResults['Prefetch Dependencies'] = [status: 'failed', detail: 'cargo fetch 失败'] }
                }
            }
        }

        stage('Build & Test') {
            parallel {
                stage('Clippy+Build: aarch64-crosvm-virt') {
                    steps {
                        script {
                            runClippyAndBuild('aarch64-crosvm-virt')
                            ciResults['Clippy+Build: aarch64-crosvm-virt'] = [status: 'passed']
                        }
                    }
                    post { failure { script { ciResults['Clippy+Build: aarch64-crosvm-virt'] = [status: 'failed', detail: 'clippy 或 build 失败'] } } }
                }
                stage('Clippy+Runtime: riscv64-qemu-virt') {
                    steps {
                        script {
                            runClippyAndRuntime('riscv64')
                            ciResults['Clippy+Runtime: riscv64-qemu-virt'] = [status: 'passed']
                        }
                    }
                    post { failure { script { ciResults['Clippy+Runtime: riscv64-qemu-virt'] = [status: 'failed', detail: collectUnitTestSnippet('riscv64')] } } }
                }
                stage('Clippy+Runtime: x86_64-qemu-virt') {
                    steps {
                        script {
                            runClippyAndRuntime('x86_64')
                            ciResults['Clippy+Runtime: x86_64-qemu-virt'] = [status: 'passed']
                        }
                    }
                    post {
                        failure {
                            script { ciResults['Clippy+Runtime: x86_64-qemu-virt'] = [status: 'failed', detail: collectUnitTestSnippet('x86_64')] }
                        }
                    }
                }
                stage('Clippy+Runtime: aarch64-qemu-virt') {
                    steps {
                        script {
                            runClippyAndRuntime('aarch64')
                            ciResults['Clippy+Runtime: aarch64-qemu-virt'] = [status: 'passed']
                        }
                    }
                    post {
                        failure {
                            script { ciResults['Clippy+Runtime: aarch64-qemu-virt'] = [status: 'failed', detail: collectUnitTestSnippet('aarch64')] }
                        }
                    }
                }
                stage('TEE: x86_64') {
                    steps {
                        script {
                            teeResults['x86_64'] = runTeeStorageTest('x86_64')
                            ciResults['TEE: x86_64'] = [status: 'passed']
                        }
                    }
                    post { failure { script {
                        if (!teeResults.containsKey('x86_64')) {
                            teeResults['x86_64'] = [arch: 'x86_64', passed: 0, failed: 0, status: 'failed', errorSnippet: '构建或启动阶段失败，请查看 Jenkins 日志']
                        }
                        ciResults['TEE: x86_64'] = [status: 'failed', detail: teeResults['x86_64']?.errorSnippet ?: 'TEE 测试失败']
                    } } }
                }
                stage('TEE: aarch64') {
                    steps {
                        script {
                            teeResults['aarch64'] = runTeeStorageTest('aarch64')
                            ciResults['TEE: aarch64'] = [status: 'passed']
                        }
                    }
                    post { failure { script {
                        if (!teeResults.containsKey('aarch64')) {
                            teeResults['aarch64'] = [arch: 'aarch64', passed: 0, failed: 0, status: 'failed', errorSnippet: '构建或启动阶段失败，请查看 Jenkins 日志']
                        }
                        ciResults['TEE: aarch64'] = [status: 'failed', detail: teeResults['aarch64']?.errorSnippet ?: 'TEE 测试失败']
                    } } }
                }
            }
        }

    }

    post {
        always {
            archiveArtifacts artifacts: [
                '**/artifacts/**/*', '**/logs/**/*', '**/unittest-output.log',
                '**/tee-test-output.log',
                '**/coverage-html/**/*', '**/coverage.info', '**/coverage.xml', '**/coverage.txt'
            ].join(','), allowEmptyArchive: true
            script {
                restoreReplayGiteeEnv()
                deleteOldCiComments()
                def coverageSummary = collectCoverageSummary()
                def comment = buildCombinedComment(ciResults, coverageSummary)
                notifyGiteePullRequest(comment)
                if (currentBuild.currentResult == 'SUCCESS') {
                    giteeTestPass()
                } else {
                    giteeTestReset()
                }
                fixWorkspaceOwnership(env.WORKSPACE)
            }
            cleanWs deleteDirs: true, disableDeferredWipeout: true, notFailBuild: true
        }
    }
}

def prefetchCargoDeps() {
    ws("${env.ROOT_WS}/prefetch") {
        def stageWorkspace = pwd()
        try {
            deleteDir()
            restoreSource()
            sh '''#!/bin/bash
set -euo pipefail
echo "==> Prefetching cargo dependencies for all platforms..."

declare -A ARCH_TARGET=(
    [aarch64]=aarch64-unknown-none-softfloat
    [x86_64]=x86_64-unknown-none
    [riscv64]=riscv64gc-unknown-none-elf
)
for platform in x86_64-qemu-virt aarch64-qemu-virt aarch64-crosvm-virt; do
    arch="${platform%%-*}"
    target="${ARCH_TARGET[$arch]}"
    cp "platforms/${platform}/defconfig" .config
    cargo fetch --manifest-path entry/Cargo.toml --target "$target" || true
done

cargo fetch --manifest-path tee_apps/sh/Cargo.toml || true
cargo fetch --manifest-path xtask/crate_rootfs/Cargo.toml || true

echo "==> Dependency prefetch complete"
'''
        } finally {
            fixWorkspaceOwnership(stageWorkspace)
        }
    }
}

def checkBuildEnvironment() {
    ws("${env.ROOT_WS}/env-check") {
        def stageWorkspace = pwd()
        try {
            deleteDir()
            restoreSource()
            sh '''#!/bin/bash
set -euo pipefail

echo "==> Checking Rust build environment..."
NIGHTLY_TOOLCHAIN="${AUX_RUST_TOOLCHAIN}"

retry() {
    local attempts="$1"
    shift
    local i
    for i in $(seq 1 "${attempts}"); do
        "$@" && return 0
        if [ "${i}" = "${attempts}" ]; then
            return 1
        fi
        echo "Command failed, retrying (${i}/${attempts}): $*" >&2
        sleep 5
    done
}

eval "$(
python3 <<'PY'
import shlex
import tomllib

with open("rust-toolchain.toml", "rb") as f:
    toolchain = tomllib.load(f)["toolchain"]

def array(name, values):
    print(f"{name}=(" + " ".join(shlex.quote(v) for v in values) + ")")

print("XKERNEL_TOOLCHAIN=" + shlex.quote(toolchain["channel"]))
array("XKERNEL_COMPONENTS", toolchain.get("components", []))
array("XKERNEL_TARGETS", toolchain.get("targets", []))
PY
)"

DEFAULT_EXTRA_TARGETS=(
    x86_64-unknown-uefi
    x86_64-unknown-linux-musl
    aarch64-unknown-linux-musl
    riscv64gc-unknown-linux-musl
)
NIGHTLY_TARGETS=(
    x86_64-unknown-linux-musl
    aarch64-unknown-linux-musl
    riscv64gc-unknown-linux-musl
)

dedup_words() {
    printf '%s\n' "$@" | awk 'NF && !seen[$0]++'
}

mapfile -t DEFAULT_TARGETS < <(dedup_words "${XKERNEL_TARGETS[@]}" "${DEFAULT_EXTRA_TARGETS[@]}")

default_install_args=("${XKERNEL_TOOLCHAIN}" --profile minimal --no-self-update)
for component in "${XKERNEL_COMPONENTS[@]}"; do
    default_install_args+=(--component "${component}")
done
for target in "${DEFAULT_TARGETS[@]}"; do
    default_install_args+=(--target "${target}")
done

nightly_install_args=("${NIGHTLY_TOOLCHAIN}" --profile minimal --component rustfmt --no-self-update)
for target in "${NIGHTLY_TARGETS[@]}"; do
    nightly_install_args+=(--target "${target}")
done

echo "==> Installing x-kernel toolchain: ${XKERNEL_TOOLCHAIN}"
retry 3 rustup toolchain install "${default_install_args[@]}"

echo "==> Installing auxiliary nightly toolchain: ${NIGHTLY_TOOLCHAIN}"
retry 3 rustup toolchain install "${nightly_install_args[@]}"

echo "==> Active default toolchain"
cargo --version
rustc --version
rustup show active-toolchain

echo "==> Installed default targets"
rustup target list --installed

echo "==> Installed nightly targets"
rustup +"${NIGHTLY_TOOLCHAIN}" target list --installed
'''
        } finally {
            fixWorkspaceOwnership(stageWorkspace)
        }
    }
}

def runRustfmt() {
    ws("${env.ROOT_WS}/rustfmt") {
        def stageWorkspace = pwd()
        try {
            deleteDir()
            restoreSource()
            sh '''#!/bin/bash
set -euo pipefail
cargo +"${AUX_RUST_TOOLCHAIN}" fmt --all --check
'''
        } finally {
            fixWorkspaceOwnership(stageWorkspace)
        }
    }
}

def runClippyAndBuild(String platform) {
    ws("${env.ROOT_WS}/clippy-build-${platform}") {
        def stageWorkspace = pwd()
        def buildTargetDir = "/xkernel-target/build-${platform}"
        try {
            deleteDir()
            restoreSource()

            withEnv(["TARGET_DIR=${buildTargetDir}"]) {
            sh """#!/bin/bash
set -euo pipefail
cp platforms/${platform}/defconfig .config
make clippy
stdbuf -oL -eL make build
"""
            }
        } finally {
            fixWorkspaceOwnership(stageWorkspace)
        }
    }
}

def runClippyAndRuntime(String arch) {
    def platform = "${arch}-qemu-virt"
    def runtimeTargetDir = targetDirForArch(arch)
    ws("${env.ROOT_WS}/${arch}") {
        def stageWorkspace = pwd()
        try {
            deleteDir()
            restoreSource()

            withEnv(["TARGET_DIR=${runtimeTargetDir}"]) {
                sh """#!/bin/bash
set -euo pipefail
cp platforms/${platform}/defconfig .config
make clippy
"""
                runUnitTests(arch)
                generateCoverageHtml(arch)
                copyCoverageToWorkspace(arch)

                sh """#!/bin/bash
set -euo pipefail
cp platforms/${platform}/defconfig .config
stdbuf -oL -eL make build
"""

                dir('test-harness') {
                    git branch: "${env.TEST_HARNESS_BRANCH}",
                        url: "${env.TEST_HARNESS_REPO}"
                    markSafeDirectory()

                    def hostfwdPort
                    def vsockCid
                    switch (arch) {
                        case 'x86_64':
                            hostfwdPort = '5556'
                            vsockCid = '101'
                            break
                        case 'aarch64':
                            hostfwdPort = '5557'
                            vsockCid = '102'
                            break
                        case 'riscv64':
                            hostfwdPort = '5560'
                            vsockCid = '103'
                            break
                        default:
                            error("Unsupported runtime test architecture: ${arch}")
                    }
                    withEnv(["XKERNEL_REMOTE=${pwd()}/..", "ARCH=${arch}",
                             "STARRY_SKIP_BUILD=1",
                             "ROOTFS_CACHE_DIR=/xkernel-target/rootfs-cache",
                             "GUEST_CASES_TARGET_DIR=${runtimeTargetDir}/guest-cases-${arch}"]) {
                        sh '''#!/bin/bash
set -euo pipefail
stdbuf -oL -eL make ci-test run
'''
                    }
                }
            }
        } finally {
            fixWorkspaceOwnership(stageWorkspace)
        }
    }
}

def runUnitTests(String arch) {
    sh """#!/bin/bash
set -euo pipefail

ROOTFS_VERSION=20260302
ROOTFS_CACHE="/xkernel-target/rootfs-cache"
ROOTFS_CACHED="\${ROOTFS_CACHE}/rootfs-${arch}.img"
mkdir -p "\${ROOTFS_CACHE}"

if [ ! -f "\${ROOTFS_CACHED}" ]; then
    IMG_URL="https://gitee.com/openkylin/x-kernel-image/releases/download/\${ROOTFS_VERSION}"
    curl -f -L "\${IMG_URL}/rootfs-${arch}.img.xz" -o "\${ROOTFS_CACHED}.xz"
    xz -df "\${ROOTFS_CACHED}.xz"
fi
cp --reflink=auto "\${ROOTFS_CACHED}" disk.img

TIMEOUT=480
if [ "${arch}" = "aarch64" ]; then
    TIMEOUT=481
fi

set +e
timeout \${TIMEOUT} stdbuf -oL -eL make UNITTEST=y VSOCK=n NET=n run | tee unittest-output.log
status=\${PIPESTATUS[0]}
set -e

if [ "\${status}" -eq 124 ]; then
    echo "Unit test timed out after \${TIMEOUT}s"
    exit 1
fi

if grep -q "UNITTEST_STATUS: TESTS_FAILED" unittest-output.log; then
    echo "Unit tests failed"
    exit 1
fi

if grep -q "UNITTEST_STATUS: ALL_TESTS_PASSED" unittest-output.log; then
    exit 0
fi

if grep -q "panicked at" unittest-output.log; then
    echo "Kernel panic detected during unit tests"
    exit 1
fi

if grep -q "test result:.*FAILED" unittest-output.log; then
    echo "Legacy unit test failure detected"
    exit 1
fi

if grep -q "test result: ok" unittest-output.log; then
    exit 0
fi

if [ "\${status}" -ne 0 ]; then
    echo "Unit test command exited with status \${status}"
    exit 1
fi

echo "Unable to determine test result from unit test output"
exit 1
"""
}

def generateCoverageHtml(String arch) {
    def triple = targetTripleFor(arch)
    def baseDir = targetDirForArch(arch)
    def covInfo = "${baseDir}/${triple}/release/coverage.info"
    def htmlOut = "${baseDir}/${triple}/release/coverage-html"
    sh """#!/bin/bash
set -euo pipefail
if [ ! -f "${covInfo}" ]; then
    echo "No coverage.info found, skipping HTML report"
    exit 0
fi
if ! command -v genhtml &>/dev/null; then
    apt-get update -qq && apt-get install -y -qq lcov >/dev/null 2>&1
fi
genhtml "${covInfo}" --output-directory "${htmlOut}" --title "x-kernel coverage (${arch})"
echo "HTML coverage report generated at ${htmlOut}/"
"""
}

def copyCoverageToWorkspace(String arch) {
    def triple = targetTripleFor(arch)
    def baseDir = targetDirForArch(arch)
    def srcDir = "${baseDir}/${triple}/release"
    sh """#!/bin/bash
set -euo pipefail
mkdir -p coverage-artifacts
for f in coverage-html coverage.info coverage.xml coverage.txt; do
    src="${srcDir}/\${f}"
    if [ -e "\${src}" ]; then
        cp -r "\${src}" coverage-artifacts/
    fi
done
"""
}

def restoreReplayGiteeEnv() {
    if (env.giteePullRequestIid?.trim()) return
    try {
        def cause = currentBuild.rawBuild?.getCause(
            org.jenkinsci.plugins.workflow.cps.replay.ReplayCause)
        if (!cause) return
        def originalEnv = cause.getOriginal()
            .getEnvironment(hudson.model.TaskListener.NULL)
        ['giteePullRequestIid', 'giteePullRequestId',
         'giteePullRequestTargetProjectId', 'giteeSourceBranch',
         'giteeTargetBranch', 'giteeTargetNamespace', 'giteeTargetRepoName',
         'giteeSourceNamespace', 'giteeSourceRepoName'].each { key ->
            def val = originalEnv?.get(key)?.trim()
            if (val) env."${key}" = val
        }
        if (env.giteePullRequestIid?.trim()) {
            echo "Replay detected: restored PR context from build #${cause.getOriginalNumber()}"
        }
    } catch (e) {
        echo "Replay context restore skipped: ${e.message}"
    }
}

def prepareSource() {
    ws("${env.ROOT_WS}/source-cache") {
        def sourceWorkspace = pwd()
        try {
            deleteDir()
            checkoutProject()
            markSafeDirectory()
            if (env.giteePullRequestIid?.trim()) {
                checkNotDiverged()
            }
        } finally {
            fixWorkspaceOwnership(sourceWorkspace)
        }
    }
}

def checkNotDiverged() {
    def targetBranch = env.giteeTargetBranch ?: env.DEFAULT_BRANCH
    def result = sh(script: """#!/bin/bash
set -euo pipefail
git fetch origin ${targetBranch} --quiet
BASE=\$(git merge-base HEAD origin/${targetBranch})
TARGET=\$(git rev-parse origin/${targetBranch})
if [ "\$BASE" != "\$TARGET" ]; then
    echo "DIVERGED"
fi
""", returnStdout: true).trim()

    if (result == 'DIVERGED') {
        error("该 PR 与目标分支 `${targetBranch}` 存在冲突，请先执行 rebase 后再重新提交。")
    }
}

def restoreSource() {
    sh "tar cf - -C '${env.ROOT_WS}/source-cache' . | tar xf -"
    markSafeDirectory()
}

def checkoutProject() {
    if (env.giteePullRequestIid?.trim()) {
        def sourceRepo = env.giteeSourceRepoHttpUrl ?: env.PROJECT_REPO
        def sourceBranch = env.giteeSourceBranch
        if (!sourceBranch?.trim()) {
            echo "WARN: giteeSourceBranch not set, falling back to checkout scm"
            checkout scm
            return
        }
        checkout([
            $class: 'GitSCM',
            branches: [[name: "*/${sourceBranch}"]],
            userRemoteConfigs: [[url: sourceRepo]]
        ])
        return
    }

    checkout([
        $class: 'GitSCM',
        branches: [[name: "*/${env.DEFAULT_BRANCH}"]],
        userRemoteConfigs: [[url: env.PROJECT_REPO]]
    ])
}

def markSafeDirectory() {
    sh '''#!/bin/bash
set -euo pipefail

dir="$(pwd)"
errfile=$(mktemp /tmp/git-safe-dir.XXXXXX)
trap 'rm -f "$errfile"' EXIT

for i in $(seq 1 20); do
    if git config --global --add safe.directory "$dir" 2>"$errfile"; then
        exit 0
    fi

    if grep -q "could not lock config file" "$errfile"; then
        sleep 0.2
        continue
    fi

    cat "$errfile" >&2 || true
    exit 1
done

echo "WARN: failed to set git safe.directory due to persistent lock contention" >&2
cat "$errfile" >&2 || true
exit 1
'''
}

def deleteOldCiComments() {
    if (!env.giteePullRequestIid?.trim()) return
    try {
        def prNumber = env.giteePullRequestIid
        def namespace = env.giteeTargetNamespace ?: 'openkylin'
        def repo = env.giteeTargetRepoName ?: 'x-kernel'
        withCredentials([string(credentialsId: 'gitee-token-secret', variable: 'GITEE_TOKEN')]) {
            def ids = sh(script: """#!/bin/bash
curl -sS --max-time 15 \
  'https://gitee.com/api/v5/repos/${namespace}/${repo}/pulls/${prNumber}/comments?page=1&per_page=100' \
  --data-urlencode "access_token=\${GITEE_TOKEN}" | \
  python3 -c "
import json, sys
data = json.load(sys.stdin)
comments = data if isinstance(data, list) else []
for c in comments:
    body = c.get('body', '')
    user = c.get('user', {}).get('login', '')
    ctype = c.get('comment_type', '')
    if ctype == 'pr_comment' and '<!-- x-kernel-ci -->' in body:
        print(c['id'])
"
""", returnStdout: true).trim()

            if (ids) {
                ids.split('\n').each { commentId ->
                    if (commentId?.trim()) {
                        sh(script: """#!/bin/bash
curl -sS --max-time 10 -X DELETE \
  'https://gitee.com/api/v5/repos/${namespace}/${repo}/pulls/comments/${commentId.trim()}' \
  --data-urlencode "access_token=\${GITEE_TOKEN}" || true
""")
                        echo "Deleted old CI comment #${commentId.trim()}"
                    }
                }
            }
        }
    } catch (e) {
        echo "deleteOldCiComments skipped: ${e.message}"
    }
}

def notifyGiteePullRequest(String message) {
    if (env.giteePullRequestIid?.trim()) {
        addGiteeMRComment comment: message
    } else {
        echo 'Skipping Gitee PR comment because this is not a PR build'
    }
}

def defconfigFor(String arch) {
    return "platforms/${arch}-qemu-virt/defconfig"
}

def archForPlatform(String platform) {
    if (platform.startsWith('aarch64')) return 'aarch64'
    if (platform.startsWith('x86_64')) return 'x86_64'
    if (platform.startsWith('riscv64')) return 'riscv64'
    error("Unsupported platform: ${platform}")
}

def targetDirForArch(String arch) {
    return "/xkernel-target/runtime-${arch}"
}

def allocateFreePort() {
    return sh(script: "python3 -c \"import socket; s=socket.socket(); s.bind(('',0)); print(s.getsockname()[1]); s.close()\"",
              returnStdout: true).trim().toInteger()
}

def allocateFreeCid() {
    def build = env.BUILD_NUMBER?.toInteger() ?: 1
    def stage = env.STAGE_NAME?.hashCode()?.abs() ?: 0
    return 100 + ((build * 7 + stage) % 2000000)
}

def giteeTestPass() { giteePrApi('POST', 'test', 'pass', '--data-urlencode \'force=true\'') }
def giteeTestReset() { giteePrApi('PATCH', 'testers', 'reset', '') }

def giteePrApi(String method, String endpoint, String label, String extraArgs) {
    if (!env.giteePullRequestIid?.trim()) return
    try {
        def prNumber = env.giteePullRequestIid
        def namespace = env.giteeTargetNamespace ?: 'openkylin'
        def repo = env.giteeTargetRepoName ?: 'x-kernel'
        withCredentials([string(credentialsId: 'gitee-token-secret', variable: 'GITEE_TOKEN')]) {
            sh(script: """#!/bin/bash
resp=\$(curl -sS -w '\\n%{http_code}' --max-time 15 -X ${method} \
  'https://gitee.com/api/v5/repos/${namespace}/${repo}/pulls/${prNumber}/${endpoint}' \
  --data-urlencode "access_token=\${GITEE_TOKEN}" ${extraArgs} 2>&1) || true
code=\$(echo "\$resp" | tail -1)
echo "Gitee test ${label}: HTTP \$code"
""")
        }
    } catch (e) {
        echo "Gitee test ${label} skipped: ${e.message}"
    }
}

def fixWorkspaceOwnership(String workspacePath) {
    if (!workspacePath?.trim()) {
        return
    }

    withEnv(["_FIX_WS_PATH=${workspacePath}"]) {
        sh '''#!/bin/bash
set -euo pipefail
workspace_path="${_FIX_WS_PATH}"
reference_path="$(dirname "${workspace_path}")"

if [[ ! -e "${reference_path}" ]]; then
    exit 0
fi

if [[ ! -e "${workspace_path}" ]]; then
    exit 0
fi

owner="$(stat -c '%u:%g' "${reference_path}")"
chown -R "${owner}" "${workspace_path}" || true
chmod -R u+rwX "${workspace_path}" || true

tmp_path="${workspace_path}@tmp"
if [[ -e "${tmp_path}" ]]; then
    chown -R "${owner}" "${tmp_path}" || true
    chmod -R u+rwX "${tmp_path}" || true
fi
'''
    }
}

def targetTripleFor(String archOrPlatform) {
    if (archOrPlatform.startsWith('aarch64')) return 'aarch64-unknown-none-softfloat'
    if (archOrPlatform.startsWith('x86')) return 'x86_64-unknown-none'
    if (archOrPlatform.startsWith('riscv64')) return 'riscv64gc-unknown-none-elf'
    error("Unsupported arch/platform: ${archOrPlatform}")
}

def collectUnitTestSnippet(String arch) {
    try {
        def logFile = "${env.ROOT_WS}/${arch}/unittest-output.log"
        if (!fileExists(logFile)) {
            return '未找到 unittest-output.log，阶段可能在日志创建前失败，请查看 Jenkins Stages 详情。'
        }
        def log = readFile(logFile)
        def lines = log.split('\n')
        def keywords = [
            'panicked at',
            'TESTS_FAILED',
            'error[E',
            'error:',
            'could not compile',
            'make: ***',
            'Unit test command exited with status'
        ]
        for (keyword in keywords) {
            for (int i = 0; i < lines.size(); i++) {
                if (lines[i].contains(keyword)) {
                    def from = Math.max(0, i - 3)
                    def to = Math.min(lines.size() - 1, i + 8)
                    return lines[from..to].join('\n').trim()
                }
            }
        }
        return lines.size() > 0
            ? lines[Math.max(0, lines.size() - 20)..<lines.size()].join('\n').trim()
            : '运行验证阶段失败，但未捕获到关键错误关键词，请查看 Jenkins Stages 详情。'
    } catch (e) {
        return "提取错误摘要失败：${e.message}"
    }
}

def runTeeStorageTest(String arch) {
    def result = [arch: arch, passed: 0, failed: 0, status: 'unknown', errorSnippet: '']
    def muslTarget = "${arch}-unknown-linux-musl"
    def muslLinker = "${arch}-linux-musl-gcc"
    def targetUpper = muslTarget.toUpperCase().replaceAll('-', '_')
    def teeTargetDir = "/xkernel-target/tee-${arch}"

    ws("${env.ROOT_WS}/tee-test-${arch}") {
        def stageWorkspace = pwd()
        def teeHostfwdPort = allocateFreePort()
        def teeVsockCid = allocateFreeCid()
        try {
            deleteDir()
            restoreSource()

            withEnv(["TARGET_DIR=${teeTargetDir}"]) {
            sh """#!/bin/bash
set -euo pipefail

LIBUTEE_DIR="/xkernel-target/libutee-${arch}"
mkdir -p "\${LIBUTEE_DIR}"

echo "==> Syncing rust-libutee..."
if [ -d "\${LIBUTEE_DIR}/.git" ]; then
    git -C "\${LIBUTEE_DIR}" fetch --depth 1 origin HEAD && git -C "\${LIBUTEE_DIR}" reset --hard FETCH_HEAD
else
    git clone --depth 1 ${env.LIBUTEE_REPO} "\${LIBUTEE_DIR}"
fi

echo "==> Building storage_test for ${muslTarget}..."
( cd "\${LIBUTEE_DIR}" && CC=${muslLinker} cargo +"\${AUX_RUST_TOOLCHAIN}" build --bin storage_test --release --target ${muslTarget} )

echo "==> Building tee_apps/sh with TEE_INIT_APPS=/tee/storage_test..."
TEE_INIT_APPS="/tee/storage_test" RUSTFLAGS= CC=${muslLinker} \\
  CARGO_TARGET_${targetUpper}_LINKER=${muslLinker} \\
  cargo build --release --target ${muslTarget} --manifest-path tee_apps/sh/Cargo.toml \\
  --target-dir "\${TARGET_DIR}/tee-apps"

echo "==> Creating rootfs..."
env -u CARGO_BUILD_TARGET RUSTFLAGS= cargo run --release \\
  --manifest-path xtask/crate_rootfs/Cargo.toml \\
  --target-dir "\${TARGET_DIR}/crate-rootfs" -- \\
  --image disk.img --size-bytes 64M \\
  --copy "\${TARGET_DIR}/tee-apps/${muslTarget}/release/sh":/bin/sh \\
  --copy "\${LIBUTEE_DIR}/target/${muslTarget}/release/storage_test":/tee/storage_test

echo "==> Building kernel..."
cp ${defconfigFor(arch)} .config
make build

echo "==> Running TEE storage test..."
set +e
timeout 1200 stdbuf -oL -eL make HOSTFWD_PORT=${teeHostfwdPort} VSOCK_CID=${teeVsockCid} justrun 2>&1 | tee tee-test-output.log
QEMU_STATUS=\${PIPESTATUS[0]}
set -e

if [ "\${QEMU_STATUS}" -eq 124 ]; then
    echo "TEE_RESULT: TIMEOUT" | tee -a tee-test-output.log
elif [ "\${QEMU_STATUS}" -ne 0 ]; then
    echo "TEE_RESULT: QEMU_ERROR(\${QEMU_STATUS})" | tee -a tee-test-output.log
fi
"""
            }

            def logText = readFile("${stageWorkspace}/tee-test-output.log")
            result.passed = logText.split('<<< test success', -1).length - 1
            result.failed = logText.split('<<< test failed', -1).length - 1

            if (logText.contains('TEE_RESULT: TIMEOUT')) {
                result.status = 'timeout'
                result.errorSnippet = "QEMU 运行超时（360s），测试未能完成\n通过: ${result.passed}，失败: ${result.failed}"
            } else if (logText.contains('TEE_RESULT: QEMU_ERROR')) {
                result.status = 'failed'
                result.errorSnippet = extractSnippet(logText, 'TEE_RESULT: QEMU_ERROR', 5)
            } else if (logText.contains('panicked at')) {
                result.status = 'panic'
                result.errorSnippet = extractSnippet(logText, 'panicked at', 8)
            } else if (result.failed > 0) {
                result.status = 'failed'
                result.errorSnippet = extractSnippet(logText, '<<< test failed', 5)
            } else if (result.passed > 0) {
                result.status = 'passed'
            } else {
                result.status = 'no_output'
                result.errorSnippet = '未检测到任何测试输出，QEMU 可能未正常启动'
            }

            if (result.status != 'passed') {
                error("TEE Storage Test ${arch}: ${result.status} (passed=${result.passed}, failed=${result.failed})")
            }

        } finally {
            fixWorkspaceOwnership(stageWorkspace)
        }
    }

    return result
}

def extractSnippet(String log, String keyword, int contextLines) {
    def lines = log.split('\n')
    def snippetLines = []
    for (int i = 0; i < lines.size(); i++) {
        if (lines[i].contains(keyword)) {
            def from = Math.max(0, i - 2)
            def to = Math.min(lines.size() - 1, i + contextLines)
            for (int j = from; j <= to; j++) {
                snippetLines << lines[j]
            }
            if (snippetLines.size() >= 30) break
        }
    }
    return snippetLines.take(30).join('\n')
}

def collectCoverageSummary() {
    def rows = []
    ['x86_64', 'aarch64'].each { arch ->
        try {
            def triple = targetTripleFor(arch)
            def covFile = "${targetDirForArch(arch)}/${triple}/release/coverage.txt"
            def content = sh(script: "cat '${covFile}' 2>/dev/null || true", returnStdout: true).trim()
            if (content) {
                def lines = content.split('\n')
                def totalLine = lines.find { it.contains('TOTAL') }
                if (totalLine) {
                    def cols = totalLine.trim().split(/\s+/)
                    if (cols.size() >= 10) {
                        rows.add("| ${arch} | ${cols[9]} | ${cols[6]} | ${cols[3]} | ${cols[7]} | ${cols[8]} |")
                    }
                }
            }
        } catch (e) {
            // skip
        }
    }
    if (rows.isEmpty()) return ''
    def header = "| 架构 | 行覆盖率 | 函数覆盖率 | 区域覆盖率 | 总行数 | 未覆盖行 |\n|------|---------|-----------|-----------|--------|---------|"
    return header + '\n' + rows.join('\n')
}

def buildCombinedComment(Map ciResults, String coverageSummary) {
    def ciComment = buildCiComment(ciResults, coverageSummary)
    return "<!-- x-kernel-ci -->\n${ciComment}"
}

def buildCiComment(Map results, String coverageSummary = '') {
    def stagesUrl = "${env.BUILD_URL}stages/"
    def stageOrder = [
        'Prepare Source',
        'Check Environment',
        'Rustfmt',
        'Prefetch Dependencies',
        'Clippy+Build: aarch64-crosvm-virt',
        'Clippy+Runtime: x86_64-qemu-virt', 'Clippy+Runtime: aarch64-qemu-virt','Clippy+Runtime: riscv64-qemu-virt',
        'TEE: x86_64', 'TEE: aarch64'
    ]
    def normalizedResults = [:]
    stageOrder.each { name ->
        normalizedResults[name] = results[name] ?: [
            status: 'not_run',
            detail: '该阶段未执行，通常是前序阶段失败导致。请查看 Jenkins Stages 详情。'
        ]
    }
    def allPassed = currentBuild.currentResult == 'SUCCESS' &&
        stageOrder.every { normalizedResults[it].status == 'passed' }
    def header = allPassed
        ? "## ✅ Jenkins CI 构建成功"
        : "## ❌ Jenkins CI 构建失败"

    def rows = stageOrder.collect { name ->
        def r = normalizedResults[name]
        def icon = r.status == 'passed' ? '✅' : (r.status == 'not_run' ? '⏭' : '❌')
        "| ${name} | ${icon} |"
    }.join('\n')

    def table = """\
${header}

| 阶段 | 状态 |
|------|------|
${rows}

 [查看详细日志 (Jenkins Stages)](${stagesUrl})
- Job: `${env.JOB_NAME}`  Build: `#${env.BUILD_NUMBER}`"""

    def coverageBlock = ''
    if (coverageSummary?.trim()) {
        def baseUrl = "${env.BUILD_URL}artifact"
        def links = ['x86_64', 'aarch64'].collect { arch ->
            "[${arch} HTML 报告](${baseUrl}/${arch}/coverage-artifacts/coverage-html/index.html)"
        }.join(' | ')
        coverageBlock = "\n### 📊 代码覆盖率\n\n${coverageSummary}\n\n${links}\n"
    }

    def errorBlocks = stageOrder.findAll { name ->
        normalizedResults[name].status != 'passed' && normalizedResults[name].detail?.trim()
    }.collect { name ->
        def detail = normalizedResults[name].detail.take(1000)
        "\n### ❌ ${name}\n\n<details>\n<summary>查看错误详情</summary>\n\n" +
            '```' + "\n${detail}\n" + '```' + "\n</details>"
    }.join('\n')

    def details = coverageBlock + (errorBlocks ? "${errorBlocks}\n" : '')
    def body = table + details
    if (!allPassed) {
        return table + "\n\n<details>\n<summary>查看构建详情</summary>\n\n" + details + "\n</details>"
    }
    return body
}
