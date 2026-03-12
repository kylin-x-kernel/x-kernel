#!/usr/bin/env groovy

pipeline {
    agent {
        docker {
            image 'yeanwang/x-kernel-builder:v1.2'
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
                script {
                    runRustfmt()
                }
            }
        }

        stage('Clippy: x86_64') {
            steps {
                script {
                    runClippy('x86_64')
                }
            }
        }

        stage('Clippy: aarch64') {
            steps {
                script {
                    runClippy('aarch64')
                }
            }
        }

        stage('Build Only: x86-csv') {
            steps {
                catchError(buildResult: 'FAILURE', stageResult: 'FAILURE') {
                    script {
                        runBuildOnly('x86-csv')
                    }
                }
            }
        }



        stage('Runtime Validation: x86_64') {
            steps {
                catchError(buildResult: 'FAILURE', stageResult: 'FAILURE') {
                    script {
                        executeBuildAndTest('x86_64')
                    }
                }
            }
        }

        stage('Runtime Validation: aarch64') {
            steps {
                catchError(buildResult: 'FAILURE', stageResult: 'FAILURE') {
                    script {
                        executeBuildAndTest('aarch64')
                    }
                }
            }
        }
    }

    post {
        always {
            archiveArtifacts artifacts: '**/artifacts/**/*', allowEmptyArchive: true
            archiveArtifacts artifacts: '**/logs/**/*', allowEmptyArchive: true
            archiveArtifacts artifacts: '**/unittest-output.log', allowEmptyArchive: true
            script {
                fixWorkspaceOwnership(env.WORKSPACE)
            }
            cleanWs deleteDirs: true, disableDeferredWipeout: true, notFailBuild: true
        }
        success {
            script {
                currentBuild.description = 'Jenkins CI passed'
                echo 'Jenkins CI passed'
                notifyGiteePullRequest("✅ Jenkins CI 构建成功\n\n- Job: ${env.JOB_NAME}\n- Build: #${env.BUILD_NUMBER}\n- URL: ${env.BUILD_URL}")
            }
        }
        unsuccessful {
            script {
                currentBuild.description = 'Jenkins CI failed'
                echo 'Jenkins CI failed'
                notifyGiteePullRequest("❌ Jenkins CI 构建失败\n\n- Job: ${env.JOB_NAME}\n- Build: #${env.BUILD_NUMBER}\n- URL: ${env.BUILD_URL}")
            }
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
