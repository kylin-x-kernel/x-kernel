#!/usr/bin/env groovy

import groovy.transform.Field

@Field Map ciResults = [:]
@Field Map teeResults = [:]

pipeline {
    agent {
        docker {
            image 'yeanwang/x-kernel-builder:v1.5'
            args '-v /var/run/docker.sock:/var/run/docker.sock -v /var/jenkins_home/cargo/registry:/usr/local/cargo/registry -v /var/jenkins_home/cargo/config.toml:/usr/local/cargo/config.toml:ro -v /var/jenkins_home/.rustup/toolchains:/usr/local/rustup/toolchains -v /var/jenkins_home/xkernel-target:/xkernel-target --privileged -u root:root'
        }
    }

    options {
        skipDefaultCheckout(true)
        timestamps()
        parallelsAlwaysFailFast()
        timeout(time: 90, unit: 'MINUTES')
        buildDiscarder(logRotator(numToKeepStr: '30', artifactNumToKeepStr: '10'))
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
        CARGO_TERM_QUIET = 'true'
        CARGO_REGISTRIES_CRATES_IO_PROTOCOL = 'sparse'
        RUSTUP_PERMIT_COPY_RENAME = '1'
        PYTHONUNBUFFERED = '1'
        TARGET_DIR = '/xkernel-target'
        HARNESS_JOBS = '2'
    }

    stages {
        stage('Source: Checkout & PR Base') {
            steps {
                script {
                    env.ROOT_WS = env.WORKSPACE
                    currentBuild.description = "PR#${env.giteePullRequestIid ?: 'manual'}"
                    runCiStage(sourceStageName(), ciFailureDetail(sourceStageName()), false) {
                        prepareSource()
                        // 先创建 6 个并行检查占位（较早创建 -> Gitee 列表靠下）；顺序 3 项在后续 start/finish
                        giteeStartParallelCheckRuns()
                        giteeStartCheckRun(sourceStageName())
                    }
                }
            }
        }

        stage('Setup: Toolchains & Targets') {
            steps {
                script {
                    runCiStage(setupStageName(), ciFailureDetail(setupStageName())) {
                        checkBuildEnvironment()
                    }
                }
            }
        }

        stage('Check: Rustfmt') {
            steps {
                script {
                    runCiStage(rustfmtStageName(), ciFailureDetail(rustfmtStageName())) {
                        runRustfmt()
                    }
                }
            }
        }

        stage('Build & Test') {
            steps {
                script {
                    parallel ciParallelBranches()
                }
            }
        }

    }

    post {
        always {
            script {
                finalizeCiBuild()
            }
            cleanWs deleteDirs: true, disableDeferredWipeout: true, notFailBuild: true
        }
    }
}

def sourceStageName() { return 'Source: Checkout & PR Base' }
def setupStageName() { return 'Setup: Toolchains & Targets' }
def rustfmtStageName() { return 'Check: Rustfmt' }

def runtimeTestArchitectures() { return ['x86_64', 'aarch64', 'riscv64'] }
def teeTestArchitectures() { return ['x86_64', 'aarch64'] }
def teeTestBinaries() { return ['storage_test', 'cryp_test'] }

def ciSequentialStages() {
    return [
        [name: sourceStageName(), failure: '源码准备失败（可能是分支分叉需要 rebase）'],
        [name: setupStageName(), failure: 'Rust 工具链组件或 target 安装失败'],
        [name: rustfmtStageName(), failure: 'cargo fmt --check 发现格式问题'],
    ]
}

def ciParallelStages() {
    def stages = [[
        name: 'Build Check: aarch64-crosvm-virt',
        failure: 'clippy 或 build 失败',
        type: 'build',
        platform: 'aarch64-crosvm-virt',
    ], [
        name: 'Build Check: aarch64-qemu-virt-virtcca',
        failure: 'clippy 或 build 失败',
        type: 'build',
        platform: 'aarch64-qemu-virt-virtcca',
        defconfig: 'platforms/aarch64-qemu-virt/virtcca_defconfig',
    ], [
        name: 'Doc Check: aarch64',
        failure: 'Rust 文档生成失败',
        type: 'doc',
        arch: 'aarch64',
    ]]

    runtimeTestArchitectures().each { arch ->
        stages << [
            name: "Runtime Test: ${arch}-qemu-virt",
            failure: 'clippy、单元测试、覆盖率或 runtime 测试失败',
            type: 'runtime',
            arch: arch,
        ]
    }

    teeTestArchitectures().each { arch ->
        stages << [
            name: "TEE Tests: ${arch}",
            failure: 'TEE 测试失败',
            type: 'tee',
            arch: arch,
        ]
    }

    return stages
}

def ciStageNames(List stages) {
    return stages.collect { it.name }
}

def ciFailureDetail(String stageName) {
    def stage = (ciSequentialStages() + ciParallelStages()).find { it.name == stageName }
    return stage?.failure ?: "${stageName} 失败，请查看 Jenkins 日志"
}

def archiveArtifactPatterns() {
    return [
        'ci-summary.md',
        'stage-logs/**/*.log',
        '**/artifacts/**/*',
        '**/logs/**/*',
        '**/unittest-output.log',
        '**/tee-test-output.log',
        '**/coverage-html/**/*',
        '**/coverage.info',
        '**/coverage.xml',
        '**/coverage.txt',
        '**/doc-artifacts/**/*',
    ]
}

def finalizeCiBuild() {
    restoreReplayGiteeEnv()
    fixWorkspaceOwnership(env.WORKSPACE)

    def failedStageLogs = archiveFailedStageLogs(ciResults)
    archiveArtifacts artifacts: archiveArtifactPatterns().join(','), allowEmptyArchive: true

    deleteOldCiComments()
    def coverageSummary = collectCoverageSummary()
    def built = buildCombinedComment(ciResults, coverageSummary, failedStageLogs)
    writeFile file: 'ci-summary.md', text: built.comment.replaceFirst(/^<!-- x-kernel-ci -->\n/, '')
    currentBuild.description = buildShortBuildDescription(ciResults)
    notifyGiteePullRequest(built.comment)

    giteeFinalizeAllCheckRuns(ciResults, failedStageLogs)
    giteeRefreshFailedCheckOutputs(ciResults, failedStageLogs)
    giteeReorderSequentialCheckRuns(ciResults, failedStageLogs)

    if (currentBuild.currentResult == 'SUCCESS') {
        giteeTestPass()
    } else {
        giteeTestReset()
    }

    fixWorkspaceOwnership(env.WORKSPACE)
}

def buildShortBuildDescription(Map results) {
    def pr = env.giteePullRequestIid?.trim() ? "PR#${env.giteePullRequestIid}" : 'manual'
    def sha = resolveHeadSha()?.take(8) ?: env.GIT_COMMIT?.take(8) ?: ''
    def failedStages = ciStageOrder().findAll { results[it]?.status == 'failed' }
    if (currentBuild.currentResult == 'SUCCESS' && failedStages.isEmpty()) {
        return "${pr} ✅ ${sha}".trim()
    }
    if (!failedStages.isEmpty()) {
        def failed = failedStages.take(2).join(', ')
        def suffix = failedStages.size() > 2 ? " +${failedStages.size() - 2}" : ''
        return "${pr} ❌ ${failed}${suffix} ${sha}".trim()
    }
    return "${pr} ❌ ${currentBuild.currentResult} ${sha}".trim()
}

def runCiStage(String stageName, String failedDetail, Closure body) {
    runCiStage(stageName, failedDetail, true, body)
}

def runCiStage(String stageName, String failedDetail, boolean startCheckRun, Closure body) {
    if (startCheckRun) {
        giteeStartCheckRun(stageName)
    }

    try {
        body.call()
        ciResults[stageName] = [status: 'passed']
    } catch (e) {
        ciResults[stageName] = [status: 'failed', detail: buildFailureDetail(stageName, failedDetail, e)]
        throw e
    } finally {
        ciFinishGiteeStage(stageName, ciResults, failedDetail)
    }
}

def buildFailureDetail(String stageName, String defaultDetail, Throwable error) {
    def details = []
    if (defaultDetail?.trim()) {
        details << defaultDetail.trim()
    }

    def message = error?.message?.trim()
    if (message && !details.any { message.contains(it) }) {
        details << "Jenkins 异常: ${message}"
    }

    return details ? details.join('\n') : "${stageName} 失败，请查看 Jenkins 日志"
}

def ciParallelBranches() {
    def branches = [:]

    ciParallelStages().each { stageSpec ->
        def spec = stageSpec
        branches[spec.name] = {
            runParallelCiStage(spec.name, spec.failure) {
                runCiWorkload(spec)
            }
        }
    }

    branches.failFast = true
    return branches
}

def runCiWorkload(Map spec) {
    switch (spec.type) {
        case 'build':
            runClippyAndBuild(spec.platform, spec.defconfig ?: defconfigForPlatform(spec.platform))
            break
        case 'doc':
            runGendocStage(spec.arch)
            break
        case 'runtime':
            runClippyAndRuntime(spec.arch)
            break
        case 'tee':
            teeResults[spec.arch] = runTeeStorageTest(spec.arch)
            break
        default:
            error("Unsupported CI stage type: ${spec.type}")
    }
}

def runParallelCiStage(String stageName, String failedDetail, Closure body) {
    stage(stageName) {
        runCiStage(stageName, failedDetail, false) {
            giteeEnsureCheckRunStarted(stageName)
            body.call()
        }
    }
}

def withCleanSourceWorkspace(String relativePath, Closure body) {
    ws("${env.ROOT_WS}/${relativePath}") {
        def stageWorkspace = pwd()
        try {
            deleteDir()
            restoreSource()
            body.call(stageWorkspace)
        } finally {
            fixWorkspaceOwnership(stageWorkspace)
        }
    }
}

def checkBuildEnvironment() {
    initStageLog(setupStageName())
    withCleanSourceWorkspace('env-check') {
        sh label: 'Install Rust toolchains and targets', script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(setupStageName())}
scripts/ci/check_build_environment.sh
ln -sf /usr/local/bin/riscv64-linux-musl-gcc /usr/local/bin/riscv64gc-linux-musl-gcc
"""
    }
}

def runRustfmt() {
    initStageLog(rustfmtStageName())
    withCleanSourceWorkspace('rustfmt') {
        sh label: 'Check Rust formatting', script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(rustfmtStageName())}
cargo +"${AUX_RUST_TOOLCHAIN}" fmt --all --check
"""
    }
}

def runClippyAndBuild(String platform, String defconfigPath) {
    def stageName = "Build Check: ${platform}"
    initStageLog(stageName)
    def buildTargetDir = "/xkernel-target/build-${platform}"
    withCleanSourceWorkspace("clippy-build-${platform}") {
        withEnv(["TARGET_DIR=${buildTargetDir}"]) {
            sh label: "Clippy and build ${platform}", script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(stageName)}
cp ${defconfigPath} .config
make clippy
stdbuf -oL -eL make build
"""
        }
    }
}

def runClippyAndRuntime(String arch) {
    def platform = "${arch}-qemu-virt"
    def stageName = "Runtime Test: ${platform}"
    def stageLog = stageLogFile(stageName)
    initStageLog(stageName)
    def runtimeTargetDir = targetDirForArch(arch)

    withCleanSourceWorkspace(arch) {
        withEnv(["TARGET_DIR=${runtimeTargetDir}", "STAGE_LOG=${stageLog}"]) {
            sh label: "Clippy ${platform}", script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(stageName)}
cp platforms/${platform}/defconfig .config
make clippy
"""
            runUnitTests(arch)
            generateCoverageHtml(arch)
            copyCoverageToWorkspace(arch)

            sh label: "Build ${platform}", script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(stageName)}
cp platforms/${platform}/defconfig .config
stdbuf -oL -eL make build
"""

            dir('test-harness') {
                gitCheckoutWithToken(env.TEST_HARNESS_REPO, env.TEST_HARNESS_BRANCH)
                markSafeDirectory()

                withEnv(["XKERNEL_REMOTE=${pwd()}/..", "ARCH=${arch}",
                         "STARRY_SKIP_BUILD=1",
                         "ROOTFS_CACHE_DIR=/xkernel-target/rootfs-cache",
                         "GUEST_CASES_TARGET_DIR=/xkernel-target/guest-cases-${arch}",
                         "JOBS=${env.HARNESS_JOBS}"]) {
                    sh label: "Run starry-test-harness ${arch}", script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(stageName)}
stdbuf -oL -eL make ci-test run
"""
                }
            }
        }
    }
}

def runGendocStage(String arch) {
    def stageName = "Doc Check: ${arch}"
    initStageLog(stageName)
    def docTargetDir = targetDirForDoc(arch)
    def hostTarget = sh(script: "rustc -vV | sed -n 's|host: ||p'", returnStdout: true).trim()
    def targetTriple = targetTripleFor(arch)
    def platform = "${arch}-qemu-virt"
    def ldScript = "${docTargetDir}/${targetTriple}/release/linker_${platform}.lds"
    def kbuildConfigDir = "${docTargetDir}/kbuild/${platform}"

    withCleanSourceWorkspace("doc-${arch}") {
        withEnv(["TARGET_DIR=${docTargetDir}"]) {
            sh label: "Prepare config for gendoc ${arch}", script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(stageName)}
cp ${defconfigFor(arch)} .config
make defconfig
env CARGO_BUILD_TARGET='${hostTarget}' RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= \
  cargo run --target-dir '${docTargetDir}/tools/xconf' \
  --manifest-path xtask/xconfig/Cargo.toml --bin xconf -- \
  gen-const --output-dir='${kbuildConfigDir}'
env CARGO_BUILD_TARGET='${hostTarget}' RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= \
  cargo run --target-dir '${docTargetDir}/tools/xconf' \
  --manifest-path xtask/xconfig/Cargo.toml --bin xconf -- \
  gen-cargo --ld-script='${ldScript}'
"""
            runGendoc(stageName, docTargetDir, arch)
            copyDocArtifactsToWorkspace(docTargetDir, arch)
        }
    }
}

def runUnitTests(String arch) {
    sh label: "Unit tests ${arch}", script: """#!/bin/bash
set -euo pipefail
scripts/ci/run_unit_tests.sh '${arch}'
"""
}

def generateCoverageHtml(String arch) {
    def triple = targetTripleFor(arch)
    def baseDir = targetDirForArch(arch)
    def covInfo = "${baseDir}/${triple}/release/coverage.info"
    def htmlOut = "${baseDir}/${triple}/release/coverage-html"
    sh label: "Generate coverage HTML ${arch}", script: """#!/bin/bash
set -euo pipefail
if [ ! -f "${covInfo}" ]; then
    echo "No coverage.info found, skipping HTML report"
    exit 0
fi
if ! command -v genhtml &>/dev/null; then
    echo "genhtml not found. Please install lcov in the CI builder image."
    exit 1
fi
genhtml "${covInfo}" --output-directory "${htmlOut}" --title "x-kernel coverage (${arch})"
echo "HTML coverage report generated at ${htmlOut}/"
"""
}

def copyCoverageToWorkspace(String arch) {
    def triple = targetTripleFor(arch)
    def baseDir = targetDirForArch(arch)
    def srcDir = "${baseDir}/${triple}/release"
    sh label: "Collect coverage artifacts ${arch}", script: """#!/bin/bash
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

def runGendoc(String stageName, String targetDir, String arch) {
    sh label: 'Generate Rust docs', script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(stageName)}
# rustdoc runs on the host target with --cfg doc (see .cargo/config.toml).
# Do not pass --target: bare-metal triples have no std, but kio uses std under cfg(doc).
cargo run --manifest-path xtask/gendoc/Cargo.toml -- --target-dir '${targetDir}'
"""
}

def copyDocArtifactsToWorkspace(String targetDir, String arch) {
    sh label: 'Collect doc artifacts', script: """#!/bin/bash
set -euo pipefail
doc_src="${targetDir}/doc"
if [ ! -d "\${doc_src}" ]; then
    echo "No doc directory found at \${doc_src}"
    exit 1
fi
mkdir -p doc-artifacts
cp -r "\${doc_src}" doc-artifacts/
echo "Rust docs collected at doc-artifacts/doc/index.html"
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
         'giteePullRequestTargetProjectId', 'giteePullRequestLastCommit',
         'giteeAfterCommitSha', 'giteeSourceBranch',
         'giteeTargetBranch', 'giteeTargetNamespace', 'giteeTargetRepoName',
         'giteeSourceNamespace', 'giteeSourceRepoName', 'GIT_COMMIT'].each { key ->
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
    initStageLog(sourceStageName())
    ws("${env.ROOT_WS}/source-cache") {
        def sourceWorkspace = pwd()
        try {
            deleteDir()
            checkoutProject()
            markSafeDirectory()
            env.GIT_COMMIT = sh(label: 'Resolve checked-out commit', script: 'git rev-parse HEAD', returnStdout: true).trim()
            echo "Checked out HEAD: ${env.GIT_COMMIT}"
            if (env.giteePullRequestIid?.trim()) {
                checkNotDiverged(sourceStageName())
            }
        } finally {
            fixWorkspaceOwnership(sourceWorkspace)
        }
    }
}

def checkNotDiverged(String stageName = '') {
    def targetBranch = env.giteeTargetBranch ?: env.DEFAULT_BRANCH
    def teeLine = stageName ? stageLogTeeLine(stageName) : ''
    def result = sh(label: 'Check PR branch is rebased', script: """#!/bin/bash
set -euo pipefail
${teeLine}
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
    sh label: 'Restore source snapshot', script: "tar cf - -C '${env.ROOT_WS}/source-cache' . | tar xf -"
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
        gitCheckoutWithToken(sourceRepo, sourceBranch)
        return
    }

    gitCheckoutWithToken(env.PROJECT_REPO, env.DEFAULT_BRANCH)
}

def gitCheckoutWithToken(String repoUrl, String branch) {
    withCredentials([string(credentialsId: 'gitee-token-secret', variable: 'GIT_TOKEN')]) {
        def authUrl = repoUrl.replace('https://', "https://oauth2:${GIT_TOKEN}@")
        checkout([
            $class: 'GitSCM',
            branches: [[name: "*/${branch}"]],
            userRemoteConfigs: [[url: authUrl]]
        ])
    }
}

def markSafeDirectory() {
    sh label: 'Mark Git safe.directory', script: '''#!/bin/bash
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
            def ids = sh(label: 'Find old Gitee CI comments', script: """#!/bin/bash
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
                        sh(label: 'Delete old Gitee CI comment', script: """#!/bin/bash
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

def defconfigForPlatform(String platform) {
    return "platforms/${platform}/defconfig"
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

def targetDirForDoc(String arch) {
    return "/xkernel-target/doc-${arch}"
}

def teePortFor(String arch) {
    def base = arch == 'x86_64' ? 21000 : 21100
    def build = env.BUILD_NUMBER?.toInteger() ?: 1
    return firstFreeTcpPort(base + ((build % 50) * 2), 80)
}

def firstFreeTcpPort(int startPort, int attempts) {
    return sh(label: 'Allocate TEE hostfwd port', script: """#!/bin/bash
set -euo pipefail
python3 - '${startPort}' '${attempts}' <<'PY'
import socket
import sys

start = int(sys.argv[1])
attempts = int(sys.argv[2])
for port in range(start, start + attempts):
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            sock.bind(('127.0.0.1', port))
        except OSError:
            continue
        print(port)
        raise SystemExit(0)
raise SystemExit(f'no free TCP port in [{start}, {start + attempts})')
PY
""", returnStdout: true).trim().toInteger()
}

def teeVsockCidFor(String arch) {
    def archOffset = arch == 'x86_64' ? 1 : 2
    def build = env.BUILD_NUMBER?.toInteger() ?: 1
    return 100000000 + (build * 10) + archOffset
}

def giteeTestPass() { giteePrApi('POST', 'test', 'pass', '--data-urlencode \'force=true\'') }
def giteeTestReset() { giteePrApi('PATCH', 'testers', 'reset', '') }

def resolveHeadSha() {
    if (env.GIT_COMMIT?.trim()) return env.GIT_COMMIT.trim()
    if (env.giteePullRequestLastCommit?.trim()) return env.giteePullRequestLastCommit.trim()
    if (env.giteeAfterCommitSha?.trim()) return env.giteeAfterCommitSha.trim()
    if (env.sha?.trim()) return env.sha.trim()
    def sourceCache = "${env.ROOT_WS}/source-cache"
    if (env.ROOT_WS?.trim() && fileExists("${sourceCache}/.git")) {
        return sh(label: 'Resolve cached source commit', script: "git -C '${sourceCache}' rev-parse HEAD", returnStdout: true).trim()
    }
    return null
}

def resolveGiteeCheckRunsScript() {
    def candidates = [
        "${env.ROOT_WS}/source-cache/scripts/ci/gitee_check_runs.py",
        'scripts/ci/gitee_check_runs.py',
    ]
    for (path in candidates) {
        if (fileExists(path)) {
            return path
        }
    }
    return null
}

def giteeCheckIdsFile() {
    def ws = env.ROOT_WS?.trim() ?: env.WORKSPACE
    return "${ws}/gitee-check-ids.json"
}

def giteeManifestFile(String action, String stageName = null) {
    def ws = env.ROOT_WS?.trim() ?: env.WORKSPACE
    def label = stageName?.trim() ?: action
    def slug = sanitizeStageFileName("${action}-${label}")
    def unique = java.util.UUID.randomUUID().toString()
    return "${ws}/gitee-manifests/${slug}-${unique}.json"
}

def prepareGiteeManifestDirectory() {
    def ws = env.ROOT_WS?.trim() ?: env.WORKSPACE
    withEnv(["_GITEE_MANIFEST_DIR=${ws}/gitee-manifests"]) {
        sh label: 'Prepare Gitee manifest directory', script: '''#!/bin/bash
set -euo pipefail
manifest_dir="${_GITEE_MANIFEST_DIR}"
mkdir -p "${manifest_dir}"

parent_dir="$(dirname "${manifest_dir}")"
if [[ -e "${parent_dir}" ]]; then
    owner="$(stat -c '%u:%g' "${parent_dir}")"
    chown "${owner}" "${manifest_dir}" || true
fi

chmod 0777 "${manifest_dir}" || true
'''
    }
}

/** 并行阶段顺序（PR 评论表格 + gitee_check_runs.py manifest） */
def ciParallelStageOrder() {
    return ciStageNames(ciParallelStages())
}

/** 串行阶段顺序（PR 评论表格 + gitee_check_runs.py manifest） */
def ciSequentialStageOrder() {
    return ciStageNames(ciSequentialStages())
}

/**
 * 调用 scripts/ci/gitee_check_runs.py 处理门禁检查。
 * action: start | finish | start_parallel | ensure_start |
 *         post_finalize | refresh_failed | reorder_sequential
 * manifest 含 sequential_stages / parallel_stages（ciSequentialStageOrder / ciParallelStageOrder）
 */
def giteeCheck(String action, String stageName = null, Map ciResults = null, Map failedStageLogs = null) {
    if (!env.giteePullRequestIid?.trim()) {
        return
    }

    def scriptPath = resolveGiteeCheckRunsScript()
    if (!scriptPath) {
        echo "WARN: scripts/ci/gitee_check_runs.py not found; skip gitee ${action}"
        return
    }

    def headSha = resolveHeadSha()
    if (!headSha) {
        echo "WARN: gitee ${action}${stageName ? " ${stageName}" : ''}: head SHA not available"
        return
    }

    def namespace = env.giteeTargetNamespace ?: 'openkylin'
    def repo = env.giteeTargetRepoName ?: 'x-kernel'
    def prDbArg = env.giteePullRequestId?.trim() ? "--pr-db-id ${env.giteePullRequestId.trim()}" : "--pr ${env.giteePullRequestIid}"

    def manifest = [
        action: action,
        stage_name: stageName,
        owner: namespace,
        repo: repo,
        head_sha: headSha,
        pr_db_id: env.giteePullRequestId?.trim(),
        pr_iid: env.giteePullRequestIid?.trim(),
        details_url: env.BUILD_URL ?: '',
        ids_file: giteeCheckIdsFile(),
        root_ws: env.ROOT_WS?.trim() ?: env.WORKSPACE,
        sequential_stages: ciSequentialStageOrder(),
        parallel_stages: ciParallelStageOrder(),
        ci_results: ciResults ?: [:],
        failed_stage_logs: failedStageLogs ?: [:],
    ]

    def manifestFile = giteeManifestFile(action, stageName)
    prepareGiteeManifestDirectory()
    writeJSON file: manifestFile, json: manifest

    try {
        withCredentials([string(credentialsId: 'gitee-token-secret', variable: 'GITEE_TOKEN')]) {
            sh(label: 'Update Gitee check run', script: """#!/bin/bash
set -euo pipefail
python3 '${scriptPath}' \\
  --owner '${namespace}' \\
  --repo '${repo}' \\
  --jenkins-manifest '${manifestFile}' \\
  ${prDbArg} \\
  --details-url '${env.BUILD_URL ?: ''}' || {
  echo "WARNING: Gitee check ${action}${stageName ? " (${stageName})" : ''} failed"
}
""")
        }
    } catch (e) {
        echo "Gitee check ${action}: ${e.message}"
    }
}

def giteeStartCheckRun(String stageName) { giteeCheck('start', stageName) }
def giteeFinishCheckRun(String stageName, Map ciResults, Map failedStageLogs = [:]) {
    giteeCheck('finish', stageName, ciResults, failedStageLogs)
}

/** 保证 ciResults 有状态后再 finish，避免 Gitee 检查一直停在「进行中」。 */
def ciFinishGiteeStage(String stageName, Map results, String failedDetail = null) {
    if (!results[stageName]?.status) {
        results[stageName] = [status: 'failed', detail: failedDetail ?: ciFailureDetail(stageName)]
        echo "WARN: ${stageName} missing ciResults status before Gitee finish"
    }
    giteeFinishCheckRun(stageName, results, [:])
}
def giteeStartParallelCheckRuns() { giteeCheck('start_parallel') }
def giteeEnsureCheckRunStarted(String stageName) { giteeCheck('ensure_start', stageName) }
def giteeFinalizeAllCheckRuns(Map ciResults, Map failedStageLogs = [:]) {
    giteeCheck('post_finalize', null, ciResults, failedStageLogs)
}
def giteeRefreshFailedCheckOutputs(Map ciResults, Map failedStageLogs = [:]) {
    giteeCheck('refresh_failed', null, ciResults, failedStageLogs)
}
def giteeReorderSequentialCheckRuns(Map ciResults, Map failedStageLogs = [:]) {
    giteeCheck('reorder_sequential', null, ciResults, failedStageLogs)
}

def giteePrApi(String method, String endpoint, String label, String extraArgs) {
    if (!env.giteePullRequestIid?.trim()) return
    try {
        def prNumber = env.giteePullRequestIid
        def namespace = env.giteeTargetNamespace ?: 'openkylin'
        def repo = env.giteeTargetRepoName ?: 'x-kernel'
        withCredentials([string(credentialsId: 'gitee-token-secret', variable: 'GITEE_TOKEN')]) {
            sh(label: "Update Gitee PR test ${label}", script: """#!/bin/bash
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
        sh label: 'Fix workspace ownership', script: '''#!/bin/bash
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

def runTeeStorageTest(String arch) {
    def stageName = "TEE Tests: ${arch}"
    initStageLog(stageName)
    def result = [arch: arch, passed: 0, failed: 0, status: 'unknown', errorSnippet: '', missingApps: []]
    def teeTargetDir = "/xkernel-target/tee-${arch}"
    def teeHostfwdPort = teePortFor(arch)
    def teeVsockCid = teeVsockCidFor(arch)
    def testBins = teeTestBinaries()

    withCleanSourceWorkspace("tee-test-${arch}") { stageWorkspace ->
        withEnv(["TARGET_DIR=${teeTargetDir}",
                 "HOSTFWD_PORT=${teeHostfwdPort}",
                 "VSOCK_CID=${teeVsockCid}",
                 "TEE_TEST_BINS=${testBins.join(' ')}"]) {
            sh label: "Run TEE tests ${arch}", script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(stageName)}
scripts/ci/run_tee_storage_test.sh '${arch}'
"""
        }

        def logText = readFile("${stageWorkspace}/tee-test-output.log")
        result.passed = logText.split('<<< test success', -1).length - 1
        result.failed = logText.split('<<< test failed', -1).length - 1
        result.missingApps = testBins.findAll { app ->
            !logText.contains("tee_init: /tee/${app} exited with exit status: 0")
        }

        if (logText.contains('TEE_RESULT: TIMEOUT')) {
            result.status = 'timeout'
            result.errorSnippet = "QEMU 运行超时（1200s），测试未能完成\\n通过: ${result.passed}，失败: ${result.failed}"
        } else if (logText.contains('TEE_RESULT: QEMU_ERROR')) {
            result.status = 'failed'
            result.errorSnippet = extractSnippet(logText, 'TEE_RESULT: QEMU_ERROR', 5)
        } else if (logText.contains('panicked at')) {
            result.status = 'panic'
            result.errorSnippet = extractSnippet(logText, 'panicked at', 8)
        } else if (result.failed > 0) {
            result.status = 'failed'
            result.errorSnippet = extractSnippet(logText, '<<< test failed', 5)
        } else if (!result.missingApps.isEmpty()) {
            result.status = 'incomplete'
            result.errorSnippet = "以下 TEE 测试未成功退出: ${result.missingApps.join(', ')}"
        } else if (result.passed > 0) {
            result.status = 'passed'
        } else {
            result.status = 'no_output'
            result.errorSnippet = '未检测到任何测试输出，QEMU 可能未正常启动'
        }

        if (result.status != 'passed') {
            error("TEE Tests ${arch}: ${result.status} (passed=${result.passed}, failed=${result.failed})")
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
            def content = sh(label: "Read coverage summary ${arch}", script: "cat '${covFile}' 2>/dev/null || true", returnStdout: true).trim()
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

def ciStageOrder() {
    return ciSequentialStageOrder() + ciParallelStageOrder()
}

def sanitizeStageFileName(String stageName) {
    return stageName.replaceAll(/[^A-Za-z0-9._-]+/, '_').take(80)
}

def stageLogAnsiFilterPipe() {
    return "sed -u -e 's/\\x1[Bb]\\[[0-9;]*[a-zA-Z]//g' -e 's/\\x9[Bb]\\[[0-9;]*[a-zA-Z]//g' -e 's/\\[[0-9;]*[mK]//g'"
}

def sanitizeStageLogForDisplay(String text) {
    if (!text?.trim()) {
        return ''
    }
    def cleaned = text
        .replaceAll(/\u001B\[[0-9;?]*[ -\/]*[@-~]/, '')
        .replaceAll(/\u009B[0-9;?]*[ -\/]*[@-~]/, '')
        .replaceAll(/\[(?:\d{1,3};?)*[mK]/, '')
        .replaceAll(/\r/, '')
        .replaceAll(/\n{4,}/, '\n\n\n')
    return cleaned.trim()
}

def stageLogFile(String stageName) {
    return "${env.ROOT_WS}/stage-logs/${sanitizeStageFileName(stageName)}.log"
}

def initStageLog(String stageName) {
    if (!env.ROOT_WS?.trim()) {
        return
    }
    def logFile = stageLogFile(stageName)
    sh label: "Prepare stage log: ${stageName}", script: """#!/bin/bash
set -euo pipefail
mkdir -p '${env.ROOT_WS}/stage-logs'
: > '${logFile}'
"""
}

def stageLogTeeLine(String stageName) {
    def logFile = stageLogFile(stageName)
    def filter = stageLogAnsiFilterPipe()
    return "exec > >(${filter} | tee -a '${logFile}') 2>&1"
}

def readStageLogFile(String path) {
    if (!path?.trim() || !fileExists(path)) {
        return ''
    }
    try {
        return readFile(path).trim()
    } catch (e) {
        echo "read stage log ${path} failed: ${e.message}"
        return ''
    }
}

def resolveFailedStageLog(String stageName, Map ciResults) {
    def stageLog = readStageLogFile(stageLogFile(stageName))
    if (stageLog) {
        return sanitizeStageLogForDisplay(stageLog)
    }
    return sanitizeStageLogForDisplay(ciResults[stageName]?.detail?.trim() ?: '')
}

def archiveFailedStageLogs(Map ciResults) {
    try {
        def failedLogs = [:]
        def failedStages = ciStageOrder().findAll { ciResults[it]?.status == 'failed' }
        if (failedStages.isEmpty()) {
            return failedLogs
        }

        sh label: 'Prepare failed stage log archive', script: "mkdir -p '${env.ROOT_WS}/stage-logs' stage-logs || true"
        fixWorkspaceOwnership(env.WORKSPACE)

        failedStages.each { stageName ->
            def logContent = resolveFailedStageLog(stageName, ciResults)
            if (!logContent) {
                echo "No log captured for failed stage: ${stageName}"
                return
            }
            failedLogs[stageName] = logContent
        }
        return failedLogs
    } catch (e) {
        echo "archiveFailedStageLogs failed: ${e.message}"
        return [:]
    }
}

def buildCombinedComment(Map ciResults, String coverageSummary, Map failedStageLogs = [:]) {
    def result = buildCiComment(ciResults, coverageSummary, failedStageLogs)
    return [
        comment: "<!-- x-kernel-ci -->\n${result.body}",
        allPassed: result.allPassed,
    ]
}

def buildCiComment(Map results, String coverageSummary = '', Map failedStageLogs = [:]) {
    def stagesUrl = "${env.BUILD_URL}stages/"
    def stageOrder = ciStageOrder()
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

    def baseUrl = "${env.BUILD_URL}artifact"
    def coverageBlock = ''
    if (coverageSummary?.trim()) {
        def links = ['x86_64', 'aarch64'].collect { arch ->
            "[${arch} HTML 报告](${baseUrl}/${arch}/coverage-artifacts/coverage-html/index.html)"
        }.join(' | ')
        coverageBlock = "\n### 📊 代码覆盖率\n\n${coverageSummary}\n\n${links}\n"
    }

    def docBlock = ''
    if (allPassed) {
        docBlock = "\n### 📚 Rust 文档\n\n[aarch64 API 文档](${baseUrl}/doc-aarch64/doc-artifacts/doc/index.html)\n"
    }

    def errorBlocks = stageOrder.findAll { name ->
        normalizedResults[name].status != 'passed' &&
            (failedStageLogs[name]?.trim() || normalizedResults[name].detail?.trim())
    }.collect { name ->
        def detail = (failedStageLogs[name]?.trim() ?: normalizedResults[name].detail).take(4000)
        "\n### ❌ ${name}\n\n<details>\n<summary>查看错误详情</summary>\n\n" +
            '```' + "\n${detail}\n" + '```' + "\n</details>"
    }.join('\n')

    def details = coverageBlock + docBlock + (errorBlocks ? "${errorBlocks}\n" : '')
    def body = table + details
    if (!allPassed) {
        body = table + "\n\n<details>\n<summary>查看构建详情</summary>\n\n" + details + "\n</details>"
    }
    return [body: body, allPassed: allPassed]
}
