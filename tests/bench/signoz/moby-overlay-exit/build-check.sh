#!/bin/sh
# Run on the recorded host toolchain: Go 1.27.1 and aarch64-linux-musl-gcc 14.2.0.
# Pass a patched Moby source directory and a new absolute output directory.
set -eu
task_source=${1:?Supply the patched Moby source directory}
task_output=${2:?Supply a new absolute output directory}
mkdir "$task_output"
cd "$task_source"
export GOOS=linux
export GOARCH=arm64
export GOARM64=v8.0
export CGO_ENABLED=1
export CC=/opt/homebrew/bin/aarch64-linux-musl-gcc
export GOTOOLCHAIN=local
export GOPROXY=off
export GOSUMDB=off
export GOMAXPROCS=6
/opt/homebrew/bin/go build -p 6 -mod=vendor -tags=netgo,osusergo,static_build,nri_no_wasm,journald -ldflags '-w -X "github.com/moby/moby/v2/dockerversion.Version=29.8.2" -X "github.com/moby/moby/v2/dockerversion.GitCommit=8af9fe3-overlay-exit-go1271" -X "github.com/moby/moby/v2/dockerversion.BuildTime=2026-09-30T19:32:49.000000000+00:00" -X "github.com/moby/moby/v2/dockerversion.PlatformName=Docker Engine - Community" -X "github.com/moby/moby/v2/dockerversion.ProductName=" -X "github.com/moby/moby/v2/dockerversion.DefaultProductLicense=Community Engine" -extldflags -static' -o "$task_output/dockerd" ./cmd/dockerd
/opt/homebrew/bin/go vet -p 6 -mod=vendor -tags=netgo,osusergo,static_build,nri_no_wasm,journald ./daemon ./daemon/internal/unshare
/opt/homebrew/bin/go test -c -p 6 -mod=vendor -tags=netgo,osusergo,static_build,nri_no_wasm,journald -ldflags '-w -X "github.com/moby/moby/v2/dockerversion.Version=29.8.2" -X "github.com/moby/moby/v2/dockerversion.GitCommit=8af9fe3-overlay-exit-r2-go1271" -X "github.com/moby/moby/v2/dockerversion.BuildTime=2026-09-30T19:32:49.000000000+00:00" -X "github.com/moby/moby/v2/dockerversion.PlatformName=Docker Engine - Community" -X "github.com/moby/moby/v2/dockerversion.ProductName=" -X "github.com/moby/moby/v2/dockerversion.DefaultProductLicense=Community Engine" -extldflags -static' -o "$task_output/daemon.test" ./daemon

# Run the selected daemon tests inside the Linux ARM64 guest.
# ./daemon.test -test.run "^(TestContainerFSViewCloseRetainsMountsOnJoinFailure|TestCreateIfNotExists)$" -test.v -test.timeout=60s
