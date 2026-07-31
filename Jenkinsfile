#!/usr/bin/env groovy

import groovy.transform.Field

@Field Map ciResults = [:]
@Field Map teeResults = [:]

pipeline {
    agent {
        docker {
            label 'xkernel-agent && docker && vhost-vsock && container-vsock'
            image 'yeanwang/x-kernel-builder:v2.0.0-rc.4@sha256:31ea8c26f3a07ca83aa77602f19bd7c7114586ee837bc305537c91e73882ce22'
            args '--dns 223.5.5.5 --mount type=volume,dst=/xkernel-target --mount type=volume,src=xkernel-cargo-home-v2,dst=/xkernel-cache/cargo --mount type=volume,src=xkernel-rustup-toolchains-v2,dst=/usr/local/rustup/toolchains --mount type=volume,src=xkernel-rootfs-cache-v2,dst=/xkernel-cache/rootfs --device=/dev/kvm --device=/dev/vhost-vsock --group-add 36 --security-opt seccomp=unconfined --security-opt no-new-privileges=true'
        }
    }

    options {
        skipDefaultCheckout(true)
        timestamps()
        parallelsAlwaysFailFast()
        timeout(time: 90, unit: 'MINUTES')
        buildDiscarder(logRotator(numToKeepStr: '30', artifactNumToKeepStr: '10'))
    }

    parameters {
        string(name: 'PROJECT_REPO', defaultValue: 'https://gitee.com/openkylin/x-kernel')
        string(name: 'DEFAULT_BRANCH', defaultValue: 'main')
        string(name: 'LIBUTEE_REPO', defaultValue: 'https://gitee.com/openkylin/rust-libutee')
        string(name: 'TEST_HARNESS_REPO', defaultValue: 'https://gitee.com/openkylin/starry-test-harness')
        string(name: 'TEST_HARNESS_BRANCH', defaultValue: 'master')
        booleanParam(name: 'ENABLE_DOC_CHECK', defaultValue: false, description: 'Run Rust documentation generation check')
    }

    environment {
        CI = 'true'
        PROJECT_REPO = "${params.PROJECT_REPO}"
        DEFAULT_BRANCH = "${params.DEFAULT_BRANCH}"
        LIBUTEE_REPO = "${params.LIBUTEE_REPO}"
        TEST_HARNESS_REPO = "${params.TEST_HARNESS_REPO}"
        TEST_HARNESS_BRANCH = "${params.TEST_HARNESS_BRANCH}"
        AUX_RUST_TOOLCHAIN = 'nightly-2026-03-08'
        CARGO_TERM_COLOR = 'always'
        CARGO_TERM_QUIET = 'true'
        CARGO_HTTP_TIMEOUT = '120'
        CARGO_NET_RETRY = '3'
        CARGO_NET_GIT_FETCH_WITH_CLI = 'true'
        RUSTUP_DIST_SERVER = 'https://rsproxy.cn'
        RUSTUP_UPDATE_ROOT = 'https://rsproxy.cn/rustup'
        RUSTUP_PERMIT_COPY_RENAME = '1'
        PYTHONUNBUFFERED = '1'
        TARGET_DIR = '/xkernel-target'
        ROOTFS_CACHE_DIR = '/xkernel-cache/rootfs'
        ROOTFS_VERSION = '20260302'
        HARNESS_JOBS = '2'
        STARRY_ACCEL = 'n'
    }

    stages {
        stage('Source: Checkout & PR Base') {
            steps {
                script {
                    env.ROOT_WS = pwd()
                    currentBuild.description = env.giteePullRequestIid?.trim()
                        ? "PR#${env.giteePullRequestIid}"
                        : "${env.DEFAULT_BRANCH} (manual)"
                    runCiStage(sourceStageName(), ciFailureDetail(sourceStageName()), false) {
                        initializeCiWorkspace()
                        prepareSource()
                        // 先创建并行检查占位（较早创建 -> Gitee 列表靠下）；顺序 3 项在后续 start/finish
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

        stage('Build') {
            steps {
                script {
                    parallel ciBuildBranches()
                }
            }
        }

        stage('Run & Test') {
            steps {
                script {
                    parallel ciRunBranches()
                }
            }
        }

    }

    post {
        always {
            script {
                finalizeCiBuild()
            }
        }
        cleanup {
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
def docCheckEnabled() { return params.ENABLE_DOC_CHECK?.toString()?.toBoolean() ?: false }

def rootWorkspace() {
    return env.ROOT_WS?.trim() ?: env.WORKSPACE
}

def ciRootDir() {
    return "${rootWorkspace()}/.ci"
}

def ciSourceDir() {
    return "${ciRootDir()}/source"
}

def ciWorkRoot() {
    return "${ciRootDir()}/work"
}

def ciStageLogRoot() {
    return "${ciRootDir()}/stage-logs"
}

def ciGiteeRoot() {
    return "${ciRootDir()}/gitee"
}

def ciSequentialStages() {
    return [
        [name: sourceStageName(), failure: '源码准备失败（可能是分支分叉需要 rebase）'],
        [name: setupStageName(), failure: 'Rust 工具链组件或 target 安装失败'],
        [name: rustfmtStageName(), failure: 'cargo fmt --check 发现格式问题'],
    ]
}

def ciBuildStages() {
    def stages = [[
        name: 'Build Check: aarch64-crosvm-virt',
        failure: 'clippy 或 build 失败',
        type: 'build',
        platform: 'aarch64-crosvm-virt',
        defconfig: 'platforms/kplat-aarch64/qemu_crosvm_defconfig',
    ], [
        name: 'Build Check: kplat-aarch64-virtcca',
        failure: 'clippy 或 build 失败',
        type: 'build',
        platform: 'kplat-aarch64-virtcca',
        defconfig: 'platforms/kplat-aarch64/qemu_virtcca_defconfig',
    ]]

    if (docCheckEnabled()) {
        stages << [
            name: 'Doc Check: aarch64',
            failure: 'Rust 文档生成失败',
            type: 'doc',
            arch: 'aarch64',
        ]
    }

    runtimeTestArchitectures().each { arch ->
        stages << [
            name: "Build Artifact: kplat-${arch}",
            failure: 'clippy 或 normal build 失败',
            type: 'build_artifact',
            arch: arch,
        ]
        stages << [
            name: "Build Artifact: kplat-${arch} unittest",
            failure: 'unittest build 失败',
            type: 'unittest_build',
            arch: arch,
        ]
    }

    return stages
}

def ciRunStages() {
    def stages = []

    runtimeTestArchitectures().each { arch ->
        stages << [
            name: "Runtime Test: kplat-${arch}",
            failure: 'runtime 测试失败',
            type: 'runtime_run',
            arch: arch,
        ]
        stages << [
            name: "Unit Tests: kplat-${arch}",
            failure: '单元测试或覆盖率生成失败',
            type: 'unittest_run',
            arch: arch,
        ]
    }

    teeTestArchitectures().each { arch ->
        stages << [
            name: "TEE Tests: ${arch}",
            failure: 'TEE 测试失败',
            type: 'tee_run',
            arch: arch,
        ]
    }

    return stages
}

def ciParallelStages() {
    return ciBuildStages() + ciRunStages()
}

def ciStageNames(List stages) {
    return stages.collect { it.name }
}

def ciFailureDetail(String stageName) {
    def stage = (ciSequentialStages() + ciParallelStages()).find { it.name == stageName }
    return stage?.failure ?: "${stageName} 失败，请查看 Jenkins 日志"
}

def archiveArtifactPatterns() {
    def patterns = [
        'ci-summary.md',
        '.ci/stage-logs/**/*.log',
        'artifacts/**/*',
        '.ci/work/**/artifacts/**/*',
        '.ci/work/**/logs/**/*',
        '.ci/work/**/unittest-output.log',
        '.ci/work/**/tee-test-output.log',
    ]
    return patterns
}

def finalizeCiBuild() {
    restoreReplayGiteeEnv()

    def failedStageLogs = archiveFailedStageLogs(ciResults)
    def coverageSummary = collectCoverageSummary()
    def built = buildCombinedComment(ciResults, coverageSummary, failedStageLogs)
    writeFile file: 'ci-summary.md', text: built.comment.replaceFirst(/^<!-- x-kernel-ci -->\n/, '')
    currentBuild.description = buildShortBuildDescription(ciResults)
    archiveArtifacts artifacts: archiveArtifactPatterns().join(','), allowEmptyArchive: true

    deleteOldCiComments()
    notifyGiteePullRequest(built.comment)

    giteeFinalizeAllCheckRuns(ciResults, failedStageLogs)
    giteeRefreshFailedCheckOutputs(ciResults, failedStageLogs)
    giteeReorderSequentialCheckRuns(ciResults, failedStageLogs)

    if (currentBuild.currentResult == 'SUCCESS') {
        giteeTestPass()
    } else {
        giteeTestReset()
    }

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

def isCiAborted(Throwable error) {
    def ex = error
    while (ex != null) {
        if (ex instanceof org.jenkinsci.plugins.workflow.steps.FlowInterruptedException) {
            return true
        }
        ex = ex.getCause()
    }
    return false
}

def runCiStage(String stageName, String failedDetail, boolean startCheckRun, Closure body) {
    if (startCheckRun) {
        giteeStartCheckRun(stageName)
    }

    try {
        body.call()
        ciResults[stageName] = [status: 'passed']
    } catch (e) {
        if (isCiAborted(e)) {
            ciResults[stageName] = [
                status: 'skipped',
                detail: '因其他并行阶段失败而中止（fail-fast）',
            ]
        } else {
            ciResults[stageName] = [status: 'failed', detail: buildFailureDetail(stageName, failedDetail, e)]
        }
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

def ciBuildBranches() {
    def branches = [:]

    ciBuildStages().each { stageSpec ->
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

def ciRunBranches() {
    def branches = [:]

    ciRunStages().each { stageSpec ->
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
            runDocStage(spec.arch)
            break
        case 'build_artifact':
            runBuildArtifact(spec.arch)
            break
        case 'unittest_build':
            runUnittestBuildArtifact(spec.arch)
            break
        case 'runtime_run':
            runRuntimeTests(spec.arch)
            break
        case 'unittest_run':
            runUnitTestStage(spec.arch)
            break
        case 'tee_run':
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
    ws("${ciWorkRoot()}/${relativePath}") {
        def stageWorkspace = pwd()
        deleteDir()
        restoreSource()
        body.call(stageWorkspace)
    }
}

def checkBuildEnvironment() {
    initStageLog(setupStageName())
    def rootfsArches = runtimeTestArchitectures().join(' ')
    withCleanSourceWorkspace('env-check') {
        sh label: 'Install Rust toolchains and targets', script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(setupStageName())}
flock -x /usr/local/rustup/toolchains/.jenkins-install.lock \
  scripts/ci/check_build_environment.sh
scripts/ci/prepare_rootfs_cache.sh ${rootfsArches}
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
make defconfig
make clippy
stdbuf -oL -eL make build
"""
        }
    }
}

def runBuildArtifact(String arch) {
    def platform = "kplat-${arch}"
    def stageName = "Build Artifact: ${platform}"
    initStageLog(stageName)
    def runtimeTargetDir = targetDirForArch(arch)

    withCleanSourceWorkspace("build-artifact-${arch}") {
        withEnv(["TARGET_DIR=${runtimeTargetDir}"]) {
            sh label: "Clippy and build ${platform}", script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(stageName)}
cp platforms/${platform}/qemu_defconfig .config
make defconfig
make clippy
stdbuf -oL -eL make build
"""
            publishKernelArtifact(artifactNameForNormal(arch), platform)
        }
    }
}

def runUnittestBuildArtifact(String arch) {
    def platform = "kplat-${arch}"
    def stageName = "Build Artifact: ${platform} unittest"
    initStageLog(stageName)
    def unittestTargetDir = targetDirForUnittest(arch)

    withCleanSourceWorkspace("build-artifact-${arch}-unittest") {
        withEnv(["TARGET_DIR=${unittestTargetDir}"]) {
            sh label: "Build unittest artifact ${platform}", script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(stageName)}
cp platforms/${platform}/qemu_defconfig .config
scripts/ci/prepare_unittest_config.sh .config
make defconfig
stdbuf -oL -eL make UNITTEST=y VSOCK=n NET=n build
"""
            publishKernelArtifact(artifactNameForUnittest(arch), platform)
        }
    }
}

def runRuntimeTests(String arch) {
    def platform = "kplat-${arch}"
    def stageName = "Runtime Test: ${platform}"
    def stageLog = stageLogFile(stageName)
    initStageLog(stageName)
    def runtimeTargetDir = targetDirForArch(arch)

    withCleanSourceWorkspace("runtime-${arch}") {
        withEnv(["TARGET_DIR=${runtimeTargetDir}", "STAGE_LOG=${stageLog}"]) {
            restoreKernelArtifact(artifactNameForNormal(arch))
            dir('test-harness') {
                gitCheckoutPublic(env.TEST_HARNESS_REPO, env.TEST_HARNESS_BRANCH)

                withEnv(["XKERNEL_REMOTE=${pwd()}/..", "ARCH=${arch}",
                         "STARRY_SKIP_BUILD=1",
                         "ROOTFS_CACHE_DIR=${env.ROOTFS_CACHE_DIR}",
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

def runUnitTestStage(String arch) {
    def platform = "kplat-${arch}"
    def stageName = "Unit Tests: ${platform}"
    def stageLog = stageLogFile(stageName)
    initStageLog(stageName)
    def unittestTargetDir = targetDirForUnittest(arch)

    withCleanSourceWorkspace("unittest-${arch}") {
        withEnv(["TARGET_DIR=${unittestTargetDir}", "STAGE_LOG=${stageLog}", "SKIP_KERNEL_BUILD=1"]) {
            restoreKernelArtifact(artifactNameForUnittest(arch))
            runUnitTests(arch)
            generateCoverageHtml(arch, unittestTargetDir)
            copyCoverageToWorkspace(arch, unittestTargetDir)
        }
    }
}

def runDocStage(String arch) {
    def stageName = "Doc Check: ${arch}"
    initStageLog(stageName)
    def docTargetDir = targetDirForDoc(arch)

    withCleanSourceWorkspace("doc-${arch}") {
        withEnv(["TARGET_DIR=${docTargetDir}"]) {
            sh label: "Prepare config for docs ${arch}", script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(stageName)}
cp ${defconfigFor(arch)} .config
make defconfig
"""
            runDocs(stageName)
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

def generateCoverageHtml(String arch, String baseDir = targetDirForArch(arch)) {
    def triple = targetTripleFor(arch)
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

def copyCoverageToWorkspace(String arch, String baseDir = targetDirForArch(arch)) {
    def triple = targetTripleFor(arch)
    def srcDir = "${baseDir}/${triple}/release"
    def outputDir = "${rootWorkspace()}/artifacts/coverage/${arch}"
    withEnv(["_CI_COVERAGE_OUTPUT=${outputDir}"]) {
        sh label: "Collect coverage artifacts ${arch}", script: """#!/bin/bash
set -euo pipefail
mkdir -p "\${_CI_COVERAGE_OUTPUT}"
for f in coverage-html coverage.info coverage.xml coverage.txt; do
    src="${srcDir}/\${f}"
    if [ -e "\${src}" ]; then
        cp -r "\${src}" "\${_CI_COVERAGE_OUTPUT}/"
    fi
done
"""
    }
}

def runDocs(String stageName) {
    sh label: 'Generate Rust docs', script: """#!/bin/bash
set -euo pipefail
${stageLogTeeLine(stageName)}
make doc
"""
}

def copyDocArtifactsToWorkspace(String targetDir, String arch) {
    def targetTriple = targetTripleFor(arch)
    def outputDir = "${rootWorkspace()}/artifacts/docs/${arch}"
    withEnv(["_CI_DOC_OUTPUT=${outputDir}"]) {
        sh label: 'Collect doc artifacts', script: """#!/bin/bash
set -euo pipefail
doc_src="${targetDir}/${targetTriple}/doc"
if [ ! -d "\${doc_src}" ]; then
    echo "No doc directory found at \${doc_src}"
    exit 1
fi
mkdir -p "\${_CI_DOC_OUTPUT}"
cp -r "\${doc_src}" "\${_CI_DOC_OUTPUT}/"
echo "Rust docs collected at artifacts/docs/${arch}/doc/index.html"
"""
    }
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
    ws(ciSourceDir()) {
        deleteDir()
        checkoutProject()
        env.GIT_COMMIT = sh(label: 'Resolve checked-out commit', script: 'git rev-parse HEAD', returnStdout: true).trim()
        echo "Checked out HEAD: ${env.GIT_COMMIT}"
        if (env.giteePullRequestIid?.trim()) {
            checkNotDiverged(sourceStageName())
        }
    }
}

def checkNotDiverged(String stageName = '') {
    def targetBranch = env.giteeTargetBranch ?: env.DEFAULT_BRANCH
    def forkPr = isForkPullRequest()
    def remoteName = forkPr ? 'upstream' : 'origin'
    def teeLine = stageName ? stageLogTeeLine(stageName) : ''
    def result
    def targetRepo = targetRepoUrl()

    withEnv([
        "_CI_TARGET_BRANCH=${targetBranch}",
        "_CI_TARGET_REPO=${targetRepo}",
        "_CI_REMOTE_NAME=${remoteName}",
        "_CI_FORK_PR=${forkPr}",
    ]) {
        result = sh(label: 'Check PR branch is rebased', script: """#!/bin/bash
set -euo pipefail
${teeLine}
if [ "\${_CI_FORK_PR}" = "true" ]; then
    if git remote get-url upstream >/dev/null 2>&1; then
        git remote set-url upstream "\${_CI_TARGET_REPO}"
    else
        git remote add upstream "\${_CI_TARGET_REPO}"
    fi
fi
git fetch "\${_CI_REMOTE_NAME}" "\${_CI_TARGET_BRANCH}" \
    --quiet --no-recurse-submodules --no-tags
BASE=\$(git merge-base HEAD "\${_CI_REMOTE_NAME}/\${_CI_TARGET_BRANCH}")
TARGET=\$(git rev-parse "\${_CI_REMOTE_NAME}/\${_CI_TARGET_BRANCH}")
if [ "\$BASE" != "\$TARGET" ]; then
    echo "DIVERGED"
fi
""", returnStdout: true).trim()
    }

    if (result == 'DIVERGED') {
        error("该 PR 未包含目标分支 `${targetBranch}` 的最新提交，请先执行 rebase 后再重新提交。")
    }
}

def isForkPullRequest() {
    return normalizeRepoId(sourceRepoUrl()) != normalizeRepoId(targetRepoUrl())
}

def sourceRepoUrl() {
    def http = env.giteeSourceRepoHttpUrl?.trim()
    if (http) return http
    def ns = env.giteeSourceNamespace?.trim()
    def repo = env.giteeSourceRepoName?.trim()
    if (ns && repo) return "https://gitee.com/${ns}/${repo}"
    return env.PROJECT_REPO
}

def normalizeRepoId(String urlOrPath) {
    if (!urlOrPath?.trim()) return ''
    return urlOrPath.trim().toLowerCase()
        .replaceFirst(/^https?:\\/\\/(oauth2:[^@]+@)?/, '')
        .replaceFirst(/\\.git$/, '')
        .replaceAll('/+$', '')
}

def targetRepoUrl() {
    def ns = env.giteeTargetNamespace?.trim()
    def repo = env.giteeTargetRepoName?.trim()
    if (ns && repo) {
        return "https://gitee.com/${ns}/${repo}"
    }
    return env.PROJECT_REPO
}

def restoreSource() {
    withEnv(["_CI_SOURCE_DIR=${ciSourceDir()}"]) {
        sh label: 'Restore source snapshot', script: '''#!/bin/bash
set -euo pipefail
tar cf - -C "${_CI_SOURCE_DIR}" . | tar xf -
'''
    }
}

def checkoutProject() {
    if (env.giteePullRequestIid?.trim()) {
        def sourceRepo = env.giteeSourceRepoHttpUrl ?: env.PROJECT_REPO
        def sourceBranch = env.giteeSourceBranch
        if (!sourceBranch?.trim()) {
            error('giteeSourceBranch is required for a Gitee PR build')
        }
        gitCheckoutPublic(sourceRepo, sourceBranch)
        return
    }

    gitCheckoutPublic(env.PROJECT_REPO, env.DEFAULT_BRANCH)
}

def gitCheckoutPublic(String repoUrl, String branch) {
    if (!repoUrl?.trim()) {
        error('Repository URL is required')
    }
    if (!branch?.trim()) {
        error('Repository branch is required')
    }

    withEnv([
        "_CI_CHECKOUT_REPO=${repoUrl.trim()}",
        "_CI_CHECKOUT_BRANCH=${branch.trim()}",
    ]) {
        sh(label: 'Checkout source branch', script: '''#!/bin/bash
set -euo pipefail

git check-ref-format "refs/heads/${_CI_CHECKOUT_BRANCH}" >/dev/null
git init --quiet .
git remote add origin "${_CI_CHECKOUT_REPO}"

refspec="+refs/heads/${_CI_CHECKOUT_BRANCH}:refs/remotes/origin/${_CI_CHECKOUT_BRANCH}"
fetch_ok=false
for attempt in 1 2 3; do
    if git fetch \
        --force \
        --prune \
        --no-tags \
        --no-recurse-submodules \
        origin "${refspec}"; then
        fetch_ok=true
        break
    fi
    if [[ "${attempt}" -lt 3 ]]; then
        delay=$((attempt * 5))
        echo "Source fetch failed (attempt ${attempt}/3); retrying in ${delay}s" >&2
        sleep "${delay}"
    fi
done

if [[ "${fetch_ok}" != true ]]; then
    echo "Source fetch failed after 3 attempts" >&2
    exit 1
fi

remote_ref="refs/remotes/origin/${_CI_CHECKOUT_BRANCH}"
git checkout --quiet --force -B "${_CI_CHECKOUT_BRANCH}" "${remote_ref}"
git reset --quiet --hard "${remote_ref}"
'''
        )
    }
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
    return "platforms/kplat-${arch}/qemu_defconfig"
}

def defconfigForPlatform(String platform) {
    return "platforms/${platform}/qemu_defconfig"
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

def targetDirForUnittest(String arch) {
    return "/xkernel-target/unittest-${arch}"
}

def targetDirForDoc(String arch) {
    return "/xkernel-target/doc-${arch}"
}

def ciArtifactRoot() {
    return "/xkernel-target/ci-artifacts"
}

def artifactNameForNormal(String arch) {
    return "${arch}-normal"
}

def artifactNameForUnittest(String arch) {
    return "${arch}-unittest"
}

def publishKernelArtifact(String artifactName, String platform) {
    def artifactDir = "${ciArtifactRoot()}/${artifactName}"
    sh label: "Publish artifact ${artifactName}", script: """#!/bin/bash
set -euo pipefail
artifact_dir='${artifactDir}'
rm -rf "\${artifact_dir}"
mkdir -p "\${artifact_dir}"
cp .config auto.conf autoconf.h "\${artifact_dir}/"
shopt -s nullglob
for image in xkernel_*; do
    cp "\${image}" "\${artifact_dir}/"
done
# Archive the xkmake bundle (release images + manifest, and any runtime
# state such as OVMF vars) so later test stages can `xkmake run --no-build`
# without relying on a shared TARGET_DIR mount across agents.
bundle_src="\${TARGET_DIR}/xkmake/${platform}"
if [ -d "\${bundle_src}" ]; then
    tar -cf "\${artifact_dir}/bundle.tar" -C "\${TARGET_DIR}/xkmake" "${platform}"
else
    echo "warning: xkmake bundle dir not found at \${bundle_src}; skipping bundle archive" >&2
fi
"""
}

def restoreKernelArtifact(String artifactName) {
    def artifactDir = "${ciArtifactRoot()}/${artifactName}"
    // Restores workspace-level runtime inputs (.config, generated Kconfig
    // side files, final xkernel_* images) plus the xkmake bundle archived by
    // publishKernelArtifact (bundle.tar), so `xkmake run --no-build` works
    // even when build and test stages do not share a TARGET_DIR mount.
    sh label: "Restore artifact ${artifactName}", script: """#!/bin/bash
set -euo pipefail
artifact_dir='${artifactDir}'
if [ ! -d "\${artifact_dir}" ]; then
    echo "artifact not found: \${artifact_dir}" >&2
    exit 1
fi
cp "\${artifact_dir}/.config" .
cp "\${artifact_dir}/auto.conf" .
cp "\${artifact_dir}/autoconf.h" .
shopt -s nullglob
for image in "\${artifact_dir}"/xkernel_*; do
    cp "\${image}" .
done
# Restore the xkmake bundle (release/ + runtime/) under TARGET_DIR/xkmake
# so `xkmake run --no-build` finds a compatible bundle.
bundle_tar="\${artifact_dir}/bundle.tar"
if [ -f "\${bundle_tar}" ]; then
    mkdir -p "\${TARGET_DIR}/xkmake"
    tar -xf "\${bundle_tar}" -C "\${TARGET_DIR}/xkmake"
fi
"""
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
    def sourceCache = ciSourceDir()
    if (env.ROOT_WS?.trim() && fileExists("${sourceCache}/.git")) {
        return withEnv(["_CI_SOURCE_DIR=${sourceCache}"]) {
            sh(
                label: 'Resolve cached source commit',
                script: 'git -C "${_CI_SOURCE_DIR}" rev-parse HEAD',
                returnStdout: true
            ).trim()
        }
    }
    return null
}

def resolveGiteeCheckRunsScript() {
    def candidates = [
        "${ciSourceDir()}/scripts/ci/gitee_check_runs.py",
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
    return "${ciGiteeRoot()}/check-ids.json"
}

def giteeManifestFile(String action, String stageName = null) {
    def label = stageName?.trim() ?: action
    def slug = sanitizeStageFileName("${action}-${label}")
    def unique = java.util.UUID.randomUUID().toString()
    return "${ciGiteeRoot()}/manifests/${slug}-${unique}.json"
}

def prepareGiteeManifestDirectory() {
    withEnv([
        "_GITEE_ROOT=${ciGiteeRoot()}",
        "_GITEE_MANIFEST_DIR=${ciGiteeRoot()}/manifests",
    ]) {
        sh label: 'Prepare Gitee manifest directory', script: '''#!/bin/bash
set -euo pipefail
install -d -m 0700 "${_GITEE_ROOT}" "${_GITEE_MANIFEST_DIR}"
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
        root_ws: ciRootDir(),
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

def initializeCiWorkspace() {
    // The declarative Docker agent has already bind-mounted WORKSPACE.
    // Deleting the mount point replaces its inode and makes later docker exec
    // calls fail. Remove only its contents so the bind mount remains valid.
    sh(
        label: 'Clean CI workspace contents',
        script: '''#!/bin/bash
set -euo pipefail

if [[ -z "${WORKSPACE:-}" || "${PWD}" != "${WORKSPACE}" ]]; then
    echo "Refusing to clean unexpected workspace: PWD=${PWD}, WORKSPACE=${WORKSPACE:-unset}" >&2
    exit 2
fi

find "${WORKSPACE}" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
'''
    )
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
    def teeTargetDir = targetDirForArch(arch)
    def teeHostfwdPort = teePortFor(arch)
    def teeVsockCid = teeVsockCidFor(arch)
    def testBins = teeTestBinaries()

    withCleanSourceWorkspace("tee-test-${arch}") { stageWorkspace ->
        withEnv(["TARGET_DIR=${teeTargetDir}",
                 "HOSTFWD_PORT=${teeHostfwdPort}",
                 "VSOCK_CID=${teeVsockCid}",
                 "TEE_TEST_BINS=${testBins.join(' ')}",
                 "SKIP_KERNEL_BUILD=1"]) {
            restoreKernelArtifact(artifactNameForNormal(arch))
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
    return "${ciStageLogRoot()}/${sanitizeStageFileName(stageName)}.log"
}

def initStageLog(String stageName) {
    if (!env.ROOT_WS?.trim()) {
        return
    }
    def logFile = stageLogFile(stageName)
    withEnv([
        "_CI_STAGE_LOG_DIR=${ciStageLogRoot()}",
        "_CI_STAGE_LOG_FILE=${logFile}",
    ]) {
        sh label: "Prepare stage log: ${stageName}", script: '''#!/bin/bash
set -euo pipefail
install -d -m 0755 "${_CI_STAGE_LOG_DIR}"
: >"${_CI_STAGE_LOG_FILE}"
chmod 0644 "${_CI_STAGE_LOG_FILE}"
'''
    }
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

        withEnv(["_CI_STAGE_LOG_DIR=${ciStageLogRoot()}"]) {
            sh label: 'Prepare failed stage log archive', script: '''
mkdir -p "${_CI_STAGE_LOG_DIR}" || true
'''
        }

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
        def icon = r.status == 'passed' ? '✅' : (r.status in ['not_run', 'skipped'] ? '⏭' : '❌')
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
            "[${arch} HTML 报告](${baseUrl}/artifacts/coverage/${arch}/coverage-html/index.html)"
        }.join(' | ')
        coverageBlock = "\n### 📊 代码覆盖率\n\n${coverageSummary}\n\n${links}\n"
    }

    def docBlock = ''
    if (allPassed && docCheckEnabled()) {
        docBlock = "\n### 📚 Rust 文档\n\n[aarch64 API 文档](${baseUrl}/artifacts/docs/aarch64/doc/index.html)\n"
    }

    def errorBlocks = stageOrder.findAll { name ->
        normalizedResults[name].status == 'failed' &&
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
