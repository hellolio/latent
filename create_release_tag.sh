#!/usr/bin/env bash
set -euo pipefail

# 切换到 main 分支并更新
git switch main
git pull --ff-only origin main

# 获取远程 tag
git fetch --tags origin

# 确定版本号
if [[ $# -ge 1 ]]; then
    VERSION="$1"
else
    LATEST_TAG=$(git tag -l 'v[0-9]*' --sort=-v:refname | head -n 1)

    if [[ -z "$LATEST_TAG" ]]; then
        VERSION="v0.0.1"
    else
        if [[ ! "$LATEST_TAG" =~ ^v([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
            echo "错误：最新 tag 格式不正确：$LATEST_TAG" >&2
            exit 1
        fi

        VERSION="v${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.$((BASH_REMATCH[3] + 1))"
    fi
fi

# 检查 tag 是否已存在
if git rev-parse -q --verify "refs/tags/$VERSION" >/dev/null; then
    echo "错误：tag $VERSION 已存在" >&2
    exit 1
fi

echo "准备发布版本：$VERSION"

# 创建并推送 tag
git tag -a "$VERSION" -m "Release $VERSION"
git push origin "$VERSION"

echo "发布成功：$VERSION"
