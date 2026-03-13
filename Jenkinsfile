#!/usr/bin/env groovy

def ciResults = [:]

pipeline {
    agent {
        docker {
            image 'yeanwang/x-kernel-builder:v1.3'
            args '-v /var/run/docker.sock:/var/run/docker.sock -v /var/jenkins_home/cargo/registry:/usr/local/cargo/registry --privileged -u root:root'
        }
    }

    options {
        skipDefaultCheckout(true)
        disableConcurrentBuilds()
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
                    prepareSource()
                }
            }
        }

        stage('Rustfmt') {
            steps {
                catchError(buildResult: 'FAILURE', stageResult: 'FAILURE') {
                    script {
                        runRustfmt()
                        ciResults['Rustfmt'] = [status: 'passed']
                    }
                }
            }
            post {
                failure {
                    script { ciResults['Rustfmt'] = [status: 'failed', detail: 'cargo fmt --check 发现格式问题'] }
                }
            }
        }

        stage('Clippy: x86_64') {
            steps {
                catchError(buildResult: 'FAILURE', stageResult: 'FAILURE') {
                    script {
                        runClippy('x86_64')
                        ciResults['Clippy: x86_64'] = [status: 'passed']
                    }
                }
            }
            post {
                failure {
                    script { ciResults['Clippy: x86_64'] = [status: 'failed', detail: 'cargo clippy 发现 lint 警告/错误'] }
                }
            }
        }

        stage('Clippy: aarch64') {
            steps {
                catchError(buildResult: 'FAILURE', stageResult: 'FAILURE') {
                    script {
                        runClippy('aarch64')
                        ciResults['Clippy: aarch64'] = [status: 'passed']
                    }
                }
            }
            post {
                failure {
                    script { ciResults['Clippy: aarch64'] = [status: 'failed', detail: 'cargo clippy 发现 lint 警告/错误'] }
                }
            }
        }

        stage('Build Only: x86-csv') {
            steps {
                catchError(buildResult: 'FAILURE', stageResult: 'FAILURE') {
                    script {
                        runBuildOnly('x86-csv')
                        ciResults['Build: x86-csv'] = [status: 'passed']
                    }
                }
            }
            post {
                failure {
                    script { ciResults['Build: x86-csv'] = [status: 'failed', detail: 'make build 编译失败'] }
                }
            }
        }



        stage('Runtime Validation: x86_64') {
            steps {
                catchError(buildResult: 'FAILURE', stageResult: 'FAILURE') {
                    script {
                        executeBuildAndTest('x86_64')
                        ciResults['Runtime: x86_64'] = [status: 'passed']
                    }
                }
            }
            post {
                failure {
                    script { ciResults['Runtime: x86_64'] = [status: 'failed', detail: collectUnitTestSnippet('x86_64')] }
                }
            }
        }

        stage('Runtime Validation: aarch64') {
            steps {
                catchError(buildResult: 'FAILURE', stageResult: 'FAILURE') {
                    script {
                        executeBuildAndTest('aarch64')
                        ciResults['Runtime: aarch64'] = [status: 'passed']
                    }
                }
            }
            post {
                failure {
                    script { ciResults['Runtime: aarch64'] = [status: 'failed', detail: collectUnitTestSnippet('aarch64')] }
                }
            }
        }

    }

    post {
        always {
            archiveArtifacts artifacts: '**/artifacts/**/*', allowEmptyArchive: true
            archiveArtifacts artifacts: '**/logs/**/*', allowEmptyArchive: true
            archiveArtifacts artifacts: '**/unittest-output.log', allowEmptyArchive: true
            archiveArtifacts artifacts: '**/coverage-html/**/*', allowEmptyArchive: true
            archiveArtifacts artifacts: '**/coverage.info', allowEmptyArchive: true
            archiveArtifacts artifacts: '**/coverage.xml', allowEmptyArchive: true
            archiveArtifacts artifacts: '**/coverage.txt', allowEmptyArchive: true
            script {
                def coverageSummary = collectCoverageSummary()
                def comment = buildCiComment(ciResults, coverageSummary)
                notifyGiteePullRequest(comment)
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

def runClippy(String arch) {
    ws("${WORKSPACE}/clippy-${arch}") {
        def stageWorkspace = pwd()
        fixWorkspaceOwnership(stageWorkspace)
        try {
            deleteDir()
            restoreSource()

            sh """#!/bin/bash
set -euo pipefail
cp ${defconfigFor(arch)} .config
${clippyTargetSetupFor(arch)}
make clippy
"""
        } finally {
            fixWorkspaceOwnership(stageWorkspace)
        }
    }
}

def runBuildOnly(String platform) {
    ws("${WORKSPACE}/build-${platform}") {
        def stageWorkspace = pwd()
        fixWorkspaceOwnership(stageWorkspace)
        try {
            deleteDir()
            restoreSource()

            sh """#!/bin/bash
set -euo pipefail
${prepareBuildConfigFor(platform)}
${buildTargetSetupFor(platform)}
stdbuf -oL -eL make build
"""
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

TIMEOUT=120
if [ "${arch}" = "aarch64" ]; then
    TIMEOUT=360
fi

set +e
timeout \${TIMEOUT} stdbuf -oL -eL make UNITTEST=y VSOCK=n run | tee unittest-output.log
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

def prepareSource() {
    ws("${WORKSPACE}/source-cache") {
        def sourceWorkspace = pwd()
        fixWorkspaceOwnership(sourceWorkspace)
        try {
            deleteDir()
            checkoutProject()
            markSafeDirectory()
            stash name: sourceStashName(), includes: '**', useDefaultExcludes: false
        } finally {
            fixWorkspaceOwnership(sourceWorkspace)
        }
    }
}

def restoreSource() {
    unstash sourceStashName()
    markSafeDirectory()
}

def sourceStashName() {
    return 'x-kernel-source'
}

def checkoutProject() {
    if (env.giteePullRequestIid?.trim()) {
        checkout scm
        return
    }

    checkout([
        $class: 'GitSCM',
        branches: [[name: "*/${env.DEFAULT_BRANCH}"]],
        doGenerateSubmoduleConfigurations: false,
        extensions: [],
        userRemoteConfigs: [[url: env.PROJECT_REPO]]
    ])
}

def markSafeDirectory() {
    sh "git config --global --add safe.directory ${pwd()}"
}

def notifyGiteePullRequest(String message) {
    if (env.giteePullRequestIid?.trim()) {
        addGiteeMRComment comment: message
    } else {
        echo 'Skipping Gitee PR comment because this is not a PR build'
    }
}

def prepareBuildConfigFor(String platform) {
    switch (platform) {
        case 'x86-csv':
        case 'aarch64-crosvm-virt':
            return """
cp ${buildDefconfigFor(platform)} .config
"""
        default:
            error("Unsupported build-only platform: ${platform}")
    }
}

def buildDefconfigFor(String platform) {
    switch (platform) {
        case 'x86-csv':
            return 'platforms/x86-csv/defconfig'
        case 'aarch64-crosvm-virt':
            return 'platforms/aarch64-crosvm-virt/defconfig'
        default:
            error("Unsupported build-only platform: ${platform}")
    }
}

def defconfigFor(String arch) {
    switch (arch) {
        case 'aarch64':
            return 'platforms/aarch64-qemu-virt/defconfig'
        case 'x86_64':
            return 'platforms/x86_64-qemu-virt/defconfig'
        default:
            error("Unsupported architecture: ${arch}")
    }
}

def clippyTargetSetupFor(String arch) {
    switch (arch) {
        case 'aarch64':
            return '''
rustup target add aarch64-unknown-none-softfloat || true
'''
        case 'x86_64':
            return '''
rustup target add x86_64-unknown-none || true
'''
        default:
            error("Unsupported architecture: ${arch}")
    }
}

def buildTargetSetupFor(String platform) {
    switch (platform) {
        case 'x86-csv':
            return '''
rustup target add x86_64-unknown-none || true
'''
        case 'aarch64-crosvm-virt':
            return '''
rustup target add aarch64-unknown-none-softfloat || true
'''
        default:
            error("Unsupported build-only platform: ${platform}")
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

def buildCiComment(Map results, String coverageSummary = '') {
    def stagesUrl = "${env.BUILD_URL}stages/"
    def stageOrder = [
        'Rustfmt', 'Clippy: x86_64', 'Clippy: aarch64',
        'Build: x86-csv', 'Runtime: x86_64', 'Runtime: aarch64'
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

    def body = table + coverageBlock
    return errorBlocks ? "${body}\n${errorBlocks}" : body
}