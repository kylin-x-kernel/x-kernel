#!/usr/bin/env groovy

def ciResults = [:]

pipeline {
    agent {
        docker {
            image 'yeanwang/x-kernel-builder:v1.4'
            args '-v /var/run/docker.sock:/var/run/docker.sock -v /var/jenkins_home/cargo/registry:/usr/local/cargo/registry --privileged -u root:root'
        }
    }

    options {
        skipDefaultCheckout(true)
        timestamps()
    }

    environment {
        CI = 'true'
        PROJECT_REPO = 'https://gitee.com/openkylin/x-kernel'
        DEFAULT_BRANCH = 'main'
        TEST_HARNESS_REPO = 'https://gitee.com/openkylin/starry-test-harness'
        TEST_HARNESS_BRANCH = 'master'
        CARGO_TERM_COLOR = 'always'
        PYTHONUNBUFFERED = '1'
    }

    stages {
        stage('Prepare Source') {
            steps {
                script {
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

        stage('Build & Test') {
            parallel {
                stage('Clippy+Build: x86-csv') {
                    steps {
                        script {
                            runClippyAndBuild('x86-csv')
                            ciResults['Clippy+Build: x86-csv'] = [status: 'passed']
                        }
                    }
                    post { failure { script { ciResults['Clippy+Build: x86-csv'] = [status: 'failed', detail: 'clippy 或 build 失败'] } } }
                }
                stage('Clippy+Build: aarch64-crosvm-virt') {
                    steps {
                        script {
                            runClippyAndBuild('aarch64-crosvm-virt')
                            ciResults['Clippy+Build: aarch64-crosvm-virt'] = [status: 'passed']
                        }
                    }
                    post { failure { script { ciResults['Clippy+Build: aarch64-crosvm-virt'] = [status: 'failed', detail: 'clippy 或 build 失败'] } } }
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
            }
        }

    }

    post {
        always {
            archiveArtifacts artifacts: [
                '**/artifacts/**/*', '**/logs/**/*', '**/unittest-output.log',
                '**/coverage-html/**/*', '**/coverage.info', '**/coverage.xml', '**/coverage.txt'
            ].join(','), allowEmptyArchive: true
            script {
                restoreReplayGiteeEnv()
                deleteOldCiComments()
                def coverageSummary = collectCoverageSummary()
                def teeInfo = waitForTeeTest()
                def comment = buildCombinedComment(ciResults, coverageSummary, teeInfo)
                notifyGiteePullRequest(comment)
                if (currentBuild.currentResult == 'SUCCESS' && teeInfo.result == 'SUCCESS') {
                    echo "Both CI test and tee-test passed, marking test as passed"
                    giteeTestPass()
                } else if (currentBuild.currentResult != 'SUCCESS') {
                    giteeTestReset()
                }
                fixWorkspaceOwnership(env.WORKSPACE)
            }
            cleanWs deleteDirs: true, disableDeferredWipeout: true, notFailBuild: true
        }
    }
}

def runRustfmt() {
    ws("${WORKSPACE}/rustfmt") {
        def stageWorkspace = pwd()
        fixWorkspaceOwnership(stageWorkspace)
        try {
            deleteDir()
            restoreSource()
            sh '''#!/bin/bash
set -euo pipefail
cargo +nightly-2026-03-08 fmt --all --check
'''
        } finally {
            fixWorkspaceOwnership(stageWorkspace)
        }
    }
}

def runClippy(String platform) {
    ws("${WORKSPACE}/clippy-${platform}") {
        def stageWorkspace = pwd()
        fixWorkspaceOwnership(stageWorkspace)
        try {
            deleteDir()
            restoreSource()

            sh """#!/bin/bash
set -euo pipefail
cp platforms/${platform}/defconfig .config
rustup target add ${rustupTargetFor(platform)} || true
make clippy
"""
        } finally {
            fixWorkspaceOwnership(stageWorkspace)
        }
    }
}

def runClippyAndBuild(String platform) {
    ws("${WORKSPACE}/clippy-build-${platform}") {
        def stageWorkspace = pwd()
        fixWorkspaceOwnership(stageWorkspace)
        try {
            deleteDir()
            restoreSource()

            sh """#!/bin/bash
set -euo pipefail
cp platforms/${platform}/defconfig .config
rustup target add ${rustupTargetFor(platform)} || true
make clippy
stdbuf -oL -eL make build
"""
        } finally {
            fixWorkspaceOwnership(stageWorkspace)
        }
    }
}

def runClippyAndRuntime(String arch) {
    def platform = "${arch}-qemu-virt"
    ws("${WORKSPACE}/${arch}") {
        def stageWorkspace = pwd()
        fixWorkspaceOwnership(stageWorkspace)
        try {
            deleteDir()
            restoreSource()

            sh """#!/bin/bash
set -euo pipefail
cp platforms/${platform}/defconfig .config
rustup target add ${rustupTargetFor(platform)} || true
make clippy
"""
            runUnitTests(arch)
            generateCoverageHtml(arch)

            dir('test-harness') {
                git branch: "${env.TEST_HARNESS_BRANCH}",
                    url: "${env.TEST_HARNESS_REPO}"
                markSafeDirectory()

                def hostfwdPort = (arch == 'x86_64') ? '5556' : '5557'
                def vsockCid = (arch == 'x86_64') ? '101' : '102'
                withEnv(["XKERNEL_ROOT=${pwd()}/..", "ARCH=${arch}",
                         "HOSTFWD_PORT=${hostfwdPort}", "VSOCK_CID=${vsockCid}"]) {
                    sh '''#!/bin/bash
set -euo pipefail
stdbuf -oL -eL make ci-test run
'''
                }
            }
        } finally {
            fixWorkspaceOwnership(stageWorkspace)
        }
    }
}

def executeBuildAndTest(arch) {
    ws("${WORKSPACE}/${arch}") {
        def stageWorkspace = pwd()
        fixWorkspaceOwnership(stageWorkspace)
        try {
            deleteDir()
            restoreSource()
            echo "Verifying architecture: ${arch}"

            runUnitTests(arch)
            generateCoverageHtml(arch)

            dir('test-harness') {
                git branch: "${env.TEST_HARNESS_BRANCH}",
                    url: "${env.TEST_HARNESS_REPO}"
                markSafeDirectory()

                withEnv(["XKERNEL_ROOT=${pwd()}/..", "ARCH=${arch}"]) {
                    sh '''#!/bin/bash
set -euo pipefail
stdbuf -oL -eL make ci-test run
'''
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

cp ${defconfigFor(arch)} .config
${runtimeTargetSetupFor(arch)}

ROOTFS_VERSION=20260302
IMG_URL="https://gitee.com/openkylin/x-kernel-image/releases/download/\${ROOTFS_VERSION}"

curl -f -L "\${IMG_URL}/rootfs-${arch}.img.xz" -o rootfs-${arch}.img.xz
xz -df rootfs-${arch}.img.xz
cp rootfs-${arch}.img disk.img

TIMEOUT=480
if [ "${arch}" = "aarch64" ]; then
    TIMEOUT=481
fi

set +e
timeout \${TIMEOUT} stdbuf -oL -eL make UNITTEST=y VSOCK=n NET=n run | tee unittest-output.log
status=\${PIPESTATUS[0]}
set -e

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
fi

echo "Unable to determine test result from unit test output"
exit 1
"""
}

def generateCoverageHtml(String arch) {
    def triple = targetTripleFor(arch)
    def covInfo = "target/${triple}/release/coverage.info"
    def htmlOut = "target/${triple}/release/coverage-html"
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
    ws("${WORKSPACE}/source-cache") {
        def sourceWorkspace = pwd()
        fixWorkspaceOwnership(sourceWorkspace)
        try {
            deleteDir()
            checkoutProject()
            markSafeDirectory()
            if (env.giteePullRequestIid?.trim()) {
                checkNotDiverged()
            }
            stash name: "x-kernel-source-${env.BUILD_NUMBER}", includes: '**', useDefaultExcludes: false
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
    unstash "x-kernel-source-${env.BUILD_NUMBER}"
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
    sh "git config --global --add safe.directory ${pwd()}"
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

def rustupTargetFor(String platform) {
    if (platform.startsWith('aarch64')) return 'aarch64-unknown-none-softfloat'
    if (platform.startsWith('x86')) return 'x86_64-unknown-none'
    error("Unsupported platform: ${platform}")
}

def waitForTeeTest(int timeoutMinutes = 30) {
    def prId = env.giteePullRequestIid
    if (!prId?.trim()) {
        echo "No PR ID, skipping tee-test check"
        return [result: 'UNKNOWN', description: '']
    }
    def jenkinsUrl = env.JENKINS_URL ?: 'http://10.42.30.102:8088/'
    def prTag = "PR#${prId}"
    def maxAttempts = timeoutMinutes * 2
    echo "Waiting for tee-test build with ${prTag} (timeout: ${timeoutMinutes}min)..."

    for (int i = 0; i < maxAttempts; i++) {
        try {
            def output = sh(script: """#!/bin/bash
set +e
json=\$(curl -g -s --max-time 10 '${jenkinsUrl}job/tee-test/api/json?tree=builds[number,result,building,description]{0,20}')
echo "\${json}" | python3 -c "
import json, sys
data = json.load(sys.stdin)
for b in data.get('builds', []):
    desc = b.get('description') or ''
    if '${prTag}' in desc:
        if b.get('building'):
            print('RUNNING|')
        else:
            print(str(b.get('result', 'UNKNOWN')) + '|' + desc)
        sys.exit(0)
print('NOT_FOUND|')
"
""", returnStdout: true).trim()

            def parts = output.split('\\|', 2)
            def result = parts[0]
            def desc = parts.length > 1 ? parts[1] : ''

            if (result == 'SUCCESS' || result == 'FAILURE' || result == 'ABORTED' || result == 'UNSTABLE') {
                echo "tee-test for ${prTag}: ${result} (desc: ${desc})"
                return [result: result, description: desc]
            }
            echo "tee-test for ${prTag}: ${result}, waiting... (${i + 1}/${maxAttempts})"
        } catch (e) {
            echo "Error checking tee-test: ${e.message}"
        }
        sleep(30)
    }
    echo "Timed out waiting for tee-test"
    return [result: 'TIMEOUT', description: '']
}

def giteeTestPass() {
    if (!env.giteePullRequestIid?.trim()) return
    try {
        def prNumber = env.giteePullRequestIid
        def namespace = env.giteeTargetNamespace ?: 'openkylin'
        def repo = env.giteeTargetRepoName ?: 'x-kernel'
        withCredentials([string(credentialsId: 'gitee-token-secret', variable: 'GITEE_TOKEN')]) {
            sh(script: """#!/bin/bash
resp=\$(curl -sS -w '\\n%{http_code}' --max-time 15 \
  'https://gitee.com/api/v5/repos/${namespace}/${repo}/pulls/${prNumber}/test' \
  --data-urlencode "access_token=\${GITEE_TOKEN}" \
  --data-urlencode 'force=true' 2>&1) || true
code=\$(echo "\$resp" | tail -1)
echo "Gitee test pass: HTTP \$code"
""")
        }
    } catch (e) {
        echo "giteeTestPass skipped: ${e.message}"
    }
}

def giteeTestReset() {
    if (!env.giteePullRequestIid?.trim()) return
    try {
        def prNumber = env.giteePullRequestIid
        def namespace = env.giteeTargetNamespace ?: 'openkylin'
        def repo = env.giteeTargetRepoName ?: 'x-kernel'
        withCredentials([string(credentialsId: 'gitee-token-secret', variable: 'GITEE_TOKEN')]) {
            sh(script: """#!/bin/bash
resp=\$(curl -sS -w '\\n%{http_code}' --max-time 15 -X PATCH \
  'https://gitee.com/api/v5/repos/${namespace}/${repo}/pulls/${prNumber}/testers' \
  --data-urlencode "access_token=\${GITEE_TOKEN}" 2>&1) || true
code=\$(echo "\$resp" | tail -1)
echo "Gitee test reset: HTTP \$code"
""")
        }
    } catch (e) {
        echo "giteeTestReset skipped: ${e.message}"
    }
}

def fixWorkspaceOwnership(String workspacePath) {
    if (!workspacePath?.trim()) {
        return
    }

    sh """#!/bin/bash
set -euo pipefail
workspace_path='${workspacePath}'
reference_path="\$(dirname "\${workspace_path}")"

if [[ ! -e "\${reference_path}" ]]; then
    exit 0
fi

if [[ ! -e "\${workspace_path}" ]]; then
    exit 0
fi

owner="\$(stat -c '%u:%g' "\${reference_path}")"
chown -R "\${owner}" "\${workspace_path}" || true
chmod -R u+rwX "\${workspace_path}" || true

tmp_path="\${workspace_path}@tmp"
if [[ -e "\${tmp_path}" ]]; then
    chown -R "\${owner}" "\${tmp_path}" || true
    chmod -R u+rwX "\${tmp_path}" || true
fi
"""
}

def runtimeTargetSetupFor(String arch) {
    switch (arch) {
        case 'aarch64':
            return '''
rustup target add aarch64-unknown-none || true
rustup target add aarch64-unknown-none-softfloat || true
rustup target add aarch64-unknown-linux-musl || true
'''
        case 'x86_64':
            return '''
rustup target add x86_64-unknown-none || true
rustup target add x86_64-unknown-linux-musl || true
'''
        default:
            error("Unsupported architecture: ${arch}")
    }
}

def targetTripleFor(String arch) {
    switch (arch) {
        case 'aarch64':
            return 'aarch64-unknown-none-softfloat'
        case 'x86_64':
            return 'x86_64-unknown-none'
        default:
            error("Unsupported architecture: ${arch}")
    }
}

def collectUnitTestSnippet(String arch) {
    try {
        def logFile = "${WORKSPACE}/${arch}/unittest-output.log"
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

def collectCoverageSummary() {
    def rows = []
    ['x86_64', 'aarch64'].each { arch ->
        try {
            def triple = targetTripleFor(arch)
            def covFile = "${WORKSPACE}/${arch}/target/${triple}/release/coverage.txt"
            if (fileExists(covFile)) {
                def content = readFile(covFile)
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

def parseTeeDescription(String desc) {
    // Format: "PR#58|x86_64:228/0/passed|aarch64:228/0/passed"
    def teeResults = [:]
    if (!desc?.trim()) return teeResults
    desc.split('\\|').each { part ->
        def m = part =~ /^(x86_64|aarch64):(\d+)\/(\d+)\/(.+)$/
        if (m.matches()) {
            teeResults[m.group(1)] = [
                arch: m.group(1),
                passed: m.group(2) as int,
                failed: m.group(3) as int,
                status: m.group(4)
            ]
        }
    }
    return teeResults
}

def buildTeeSection(Map teeInfo) {
    def result = teeInfo.result ?: 'UNKNOWN'
    def teeResults = parseTeeDescription(teeInfo.description ?: '')

    if (result == 'TIMEOUT' || result == 'NOT_FOUND' || result == 'UNKNOWN') {
        return "\n### TEE 功能测试\n\n⏳ 等待超时或未找到对应构建\n"
    }

    ['x86_64', 'aarch64'].each { arch ->
        if (!teeResults.containsKey(arch)) {
            teeResults[arch] = [arch: arch, passed: 0, failed: 0, status: 'failed']
        }
    }

    def allPassed = result == 'SUCCESS'
    def header = allPassed ? '### ✅ TEE 功能测试通过' : '### ❌ TEE 功能测试失败'
    def rows = ['x86_64', 'aarch64'].collect { arch ->
        def r = teeResults[arch]
        def total = r.passed + r.failed
        def icon = r.status == 'passed' ? '✅' : '❌'
        "| ${arch} | ${r.passed} | ${r.failed} | ${total} | ${icon} |"
    }.join('\n')

    return """\

${header}

| 架构 | 通过 | 失败 | 合计 | 状态 |
|------|------|------|------|------|
${rows}"""
}

def buildCombinedComment(Map ciResults, String coverageSummary, Map teeInfo) {
    def ciComment = buildCiComment(ciResults, coverageSummary)
    def teeSection = buildTeeSection(teeInfo)
    def allGreen = currentBuild.currentResult == 'SUCCESS' && teeInfo.result == 'SUCCESS'
    def overallHeader = allGreen
        ? '## ✅ CI 全部通过'
        : '## ❌ CI 未全部通过'
    return "<!-- x-kernel-ci -->\n${overallHeader}\n\n${ciComment}\n${teeSection}"
}

def buildCiComment(Map results, String coverageSummary = '') {
    def stagesUrl = "${env.BUILD_URL}stages/"
    def stageOrder = [
        'Prepare Source',
        'Rustfmt',
        'Clippy+Build: x86-csv', 'Clippy+Build: aarch64-crosvm-virt',
        'Clippy+Runtime: x86_64-qemu-virt', 'Clippy+Runtime: aarch64-qemu-virt'
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
            def triple = (arch == 'aarch64') ? 'aarch64-unknown-none-softfloat' : 'x86_64-unknown-none'
            "[${arch} HTML 报告](${baseUrl}/${arch}/target/${triple}/release/coverage-html/index.html)"
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